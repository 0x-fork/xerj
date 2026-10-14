//! Corpus Hub v2 — the hosted, read-only endpoints (plan §6,
//! `docs/CORPUS_HUB_V2_IMPLEMENTATION.md`).
//!
//! Four `GET` routes under `/_hub/v1/` answer from the pre-indexed packs that
//! `xerj hub pull` restored on this node, so an agent with no clone and no
//! local index still gets an answer in one request:
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /_hub/v1/catalogue` | the bytes of the file at `hub.catalogue_path`, `ETag` = its sha256, `Cache-Control: public, max-age=3600`, `304` on a matching `If-None-Match` |
//! | `GET /_hub/v1/def/{slug}/{symbol}?k=` | an [`Answer`] from [`xerj_hub::def::lookup_in`] (the one `def` function the CLI uses) |
//! | `GET /_hub/v1/code/{slug}?q=&k=&max_tokens=&mode=` | an [`Answer`] built from [`xerj_common::xccode::run_code_query`] (the pipeline `xerj code` uses) |
//! | `GET /_hub/v1/search?q=&k=` | `{corpora:[{slug,best_score,best_hit}]}`, `multi_match` on `body,defs,symbol_text` across every pack index, grouped by corpus, best first, at most 10 |
//!
//! ## Limits (all four routes)
//!
//! `GET` only — any other method is `405` (behind authentication, unless
//! `hub.public` opens the route). Read-only: nothing here writes.
//! A corpus name must be in the catalogue (`404` otherwise, also for a name in
//! the catalogue with the `excluded` licence class — never served). `symbol`
//! and `q` are limited to [`MAX_INPUT_BYTES`] (`400`); `k` is clamped to
//! [`MAX_K`]. An unrecognised query parameter is a `400`, the way Elasticsearch
//! treats one, so a typo is never silently ignored.
//!
//! ## `hub.public` and the rate limit
//!
//! With `hub.public = true` exactly the four routes above — by exact path
//! shape, see [`is_public_path`] — skip authentication, authorization and the
//! audit log; `router.rs` wraps those three layers with a check that calls
//! [`is_public_request`]. Everything else on the node keeps all three. The
//! audit layer is skipped on purpose: it records every unauthenticated write-
//! shaped request, and a public route must not be a way to flush a bounded
//! evidence ring.
//!
//! `hub.public` makes the routes reachable by anyone, so they are rate-limited
//! per client address ([`RATE_LIMIT`] requests per [`RATE_WINDOW`], `429` with
//! `Retry-After`) — in both modes, so the limit never depends on how the node is
//! configured. `xerj-api` has no per-API-key limiter to reuse — the only
//! limiter in the crate is the share-claim one, which is private to
//! [`crate::share::ShareStore`] and shares a bounded table with claim
//! attempts, so hub traffic would throttle share claims. This module has its
//! own [`RateLimiter`]. The client address is [`crate::share::ClaimSource`]:
//! the TCP peer, believing `X-Forwarded-For` only from `server.trusted_proxies`.
//!
//! ## How a query reaches the engine — and why it is not a loopback call
//!
//! `lookup` and `run_code_query` are written against [`XcHttp`], a three-method
//! HTTP shell (`search`, `get_mapping`, `cat_indices_json`). The plan suggests
//! implementing it for the in-process case by calling the node's own loopback
//! address with the admin key. This module implements it **in-process
//! instead**: [`InProcessNode`] hands each call to a private router that holds
//! exactly the three read routes (`internal_router`) — the same handlers,
//! extractors and response encoding the real HTTP routes use, with no socket.
//! Reasons:
//!
//! * the node cannot reliably name its own address: `server.es_compat_port = 0`
//!   (ephemeral) is a supported configuration, `--port` rewrites ports, TLS may
//!   be on, and the bind address need not be loopback;
//! * no credential is needed, so no admin key is read from config or sent
//!   anywhere on a path a stranger can reach;
//! * the blocking client in `xerj-hub` (`HubNode`) has a 300 s timeout — far too
//!   long on a path a stranger can hold open — and would make the node an HTTP
//!   client of itself, a new kind of outbound connection for a server that
//!   publishes an inventory of every way data leaves it (`docs/RERANK.md`);
//! * the private router offers three read routes and nothing else, which is a
//!   smaller surface than "the admin key against the whole API".
//!
//! Three further guards bound what a hosted query can touch, even if a bug let
//! an unexpected name through: [`InProcessNode`] refuses any index expression
//! that is not made of `xc-` names in a conservative character set, every call
//! runs under an engine-level [`index_guard`] that hides every index outside
//! `xc-*`, and the corpus name is checked against the catalogue before any
//! filesystem path is built from it.
//!
//! The `XcHttp` methods are synchronous and the handlers async, so each query
//! runs on the blocking pool (`run_query`) under a global concurrency cap
//! ([`MAX_CONCURRENT_QUERIES`], `503` + `Retry-After` when full) and a wall
//! clock timeout ([`QUERY_TIMEOUT`], `504`).
//!
//! ## Known limits, stated so they are not rediscovered
//!
//! * Every hosted search appends an entry to the node's audit ring (the
//!   `_search` handler does so unconditionally). On a dedicated hub node that is
//!   harmless; on a node that also holds data worth auditing, a flood of
//!   rate-limited-but-distinct clients can age out older entries.
//! * `Answer.answered_from` is always `hosted` here: that is where the answer
//!   came from as far as the client is concerned, even though the server read a
//!   local pack.
//! * The shared query pipeline's JSON output does not report which arms ran for
//!   `mode=semantic|hybrid` (its "BM25 only" note is prose-only), so those
//!   modes carry a warning saying so instead of a silent fallback.
//! * `search` queries `xc-*`, not the plan's `xc-*-docs,xc-*-defs`. This engine
//!   resolves index-name wildcards only as `*`, `prefix*` or `*suffix`
//!   (`glob_match_simple`, `es_compat.rs`), so a `*` in the middle of a name
//!   matches nothing: measured on this build, `xc-*-docs`, `xc-*-defs` and
//!   `xc-mini-*-docs` return zero hits where `xc-*`, `xc-m*` and an explicit
//!   list return the documents. The plan's pattern would make `search` answer
//!   "nothing matches" for everything, with a `200`. `xc-*` also covers every
//!   dataset index of a pack (autoindex names them after the repo's
//!   directories, not `docs`), so the route sees more of a pack than the plan's
//!   two suffixes would have. Hits that belong to no listed corpus are skipped.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::NaiveDate;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use xerj_common::hub::{
    Answer, AnswerMode, AnsweredFrom, Catalogue, Hit, LicenceClass, CATALOGUE_FORMAT_VERSION,
};
use xerj_common::xccode::{self, passage, state as ledger, CodeParams, Mode, XcHttp};
use xerj_engine::index_guard::{self, IndexVisibility};

use crate::error::{EsErrorBody, EsErrorResponse, EsRootCause};
use crate::es_compat;
use crate::share::ClaimSource;
use crate::state::AppState;

// ─────────────────────────────────────────────────────────────────────────────
// Limits
// ─────────────────────────────────────────────────────────────────────────────

/// `symbol` and `q` may be at most this many bytes (after percent-decoding).
pub const MAX_INPUT_BYTES: usize = 512;
/// `k` is clamped to this.
pub const MAX_K: usize = 50;
/// `k` when the request names none (`def` and `code`).
pub const DEFAULT_K: usize = 5;
/// `k` when the request names none (`search`).
pub const SEARCH_DEFAULT_K: usize = 10;
/// `search` returns at most this many corpora, whatever `k` says.
pub const MAX_SEARCH_CORPORA: usize = 10;
/// Requests one client address may make per [`RATE_WINDOW`].
pub const RATE_LIMIT: usize = 60;
/// The sliding window [`RATE_LIMIT`] is counted over.
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Hard ceiling on tracked client addresses. Past it, addresses not yet seen
/// share one overflow bucket — throttled together rather than tracked without
/// bound, because this table is fed by unauthenticated callers.
pub const MAX_TRACKED_CLIENTS: usize = 10_000;
/// Engine-backed queries allowed in flight at once.
pub const MAX_CONCURRENT_QUERIES: usize = 16;
/// Wall-clock budget for one engine-backed query.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
/// `max_tokens` is refused above this.
pub const MAX_TOKENS_CAP: u64 = 8192;
/// How many characters one token is taken to be when `max_tokens` is applied.
/// An approximation, stated in the response warning when it binds.
pub const CHARS_PER_TOKEN: usize = 4;

/// Passage width when the request names no `max_tokens` (the CLI's default).
const DEFAULT_PASSAGE_CHARS: usize = 800;
/// Cap on any single internal response, so one hosted query cannot pull an
/// unbounded body into memory.
const MAX_INTERNAL_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Cap on the catalogue file the node will read and serve.
const MAX_CATALOGUE_BYTES: u64 = 16 * 1024 * 1024;
/// What `search` queries. Not the plan's `xc-*-docs,xc-*-defs`: this engine
/// resolves only `*`, `prefix*` and `*suffix`, so a `*` mid-name matches no
/// index (see the module docs, "Known limits").
const SEARCH_INDEX_EXPR: &str = "xc-*";
/// The fields `search` matches on, before the mapping gate drops any no
/// searched index maps (see [`HubShared::search_fields_blocking`]).
const SEARCH_FIELDS: [&str; 3] = ["body", "defs", "symbol_text"];
/// Hits fetched so that grouping by corpus still has ten corpora to pick from.
const SEARCH_POOL: usize = 100;
/// How long a resolved `search` field list is reused.
const SEARCH_FIELDS_TTL: Duration = Duration::from_secs(60);
/// Longest excerpt carried by a `search` best hit built from a docs record.
const EXCERPT_CHARS: usize = 240;
/// `_source` fields `search` asks the engine for — everything a best hit needs,
/// and none of the (large) `body`.
const SEARCH_SOURCE_FIELDS: [&str; 14] = [
    "symbol",
    "kind",
    "file",
    "line",
    "text",
    "value",
    "ax_path",
    "ax_file",
    "path",
    "lineno",
    "start_line",
    "title",
    "defs",
    "code",
];

// ─────────────────────────────────────────────────────────────────────────────
// Shared state
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the four routes share, built once per router.
///
/// Held per router rather than in a `static` so two routers in one process
/// (every test file) never share a rate-limit budget or a catalogue.
pub struct HubShared {
    app: AppState,
    limiter: RateLimiter,
    slots: Arc<tokio::sync::Semaphore>,
    catalogue: Mutex<Option<CachedCatalogue>>,
    search_fields: Mutex<Option<(Instant, Vec<&'static str>)>>,
}

impl HubShared {
    /// State for one router, over the node's [`AppState`].
    pub fn new(app: AppState) -> Arc<Self> {
        Arc::new(Self {
            app,
            limiter: RateLimiter::new(RATE_LIMIT, RATE_WINDOW, MAX_TRACKED_CLIENTS),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_QUERIES)),
            catalogue: Mutex::new(None),
            search_fields: Mutex::new(None),
        })
    }

    /// Test seam: take `n` query slots, as `n` slow queries would. Not
    /// reachable over HTTP.
    #[doc(hidden)]
    pub fn hold_slots_for_test(&self, n: usize) -> Vec<tokio::sync::OwnedSemaphorePermit> {
        (0..n)
            .filter_map(|_| self.slots.clone().try_acquire_owned().ok())
            .collect()
    }

    /// The catalogue, re-read only when the file's size or modification time
    /// changed. Blocking (a `stat`, and a read on change) — call from the
    /// blocking pool.
    fn catalogue_blocking(&self) -> Result<Arc<CatalogueData>, CatalogueError> {
        let configured = self.app.config.hub.catalogue_path.trim();
        if configured.is_empty() {
            return Err(CatalogueError::NotConfigured);
        }
        let path = PathBuf::from(configured);
        let meta = std::fs::metadata(&path)
            .map_err(|e| CatalogueError::Unavailable(format!("stat {}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(CatalogueError::Unavailable(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if meta.len() > MAX_CATALOGUE_BYTES {
            return Err(CatalogueError::Unavailable(format!(
                "{} is {} bytes, over the {MAX_CATALOGUE_BYTES}-byte cap",
                path.display(),
                meta.len()
            )));
        }
        let modified = meta.modified().ok();
        let mut cache = self.catalogue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cache.as_ref() {
            if c.path == path && c.len == meta.len() && c.modified == modified {
                return Ok(c.data.clone());
            }
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| CatalogueError::Unavailable(format!("read {}: {e}", path.display())))?;
        let data = Arc::new(CatalogueData::from_bytes(Bytes::from(bytes)));
        *cache = Some(CachedCatalogue {
            path,
            len: meta.len(),
            modified,
            data: data.clone(),
        });
        Ok(data)
    }

    async fn catalogue(self: &Arc<Self>) -> Result<Arc<CatalogueData>, CatalogueError> {
        let me = self.clone();
        match tokio::task::spawn_blocking(move || me.catalogue_blocking()).await {
            Ok(r) => r,
            Err(e) => Err(CatalogueError::Unavailable(format!(
                "catalogue task failed: {e}"
            ))),
        }
    }

    /// `Ok` when `slug` is a corpus the catalogue lists and permits serving,
    /// else the response to send instead.
    ///
    /// Everything that is not a servable corpus — a malformed name, a name the
    /// catalogue does not list, a listed name with the `excluded` licence class
    /// — is the same `404`: the caller learns nothing about which it was.
    async fn resolve_corpus(self: &Arc<Self>, slug: &str) -> Result<(), Response> {
        if !valid_slug(slug) {
            return Err(unknown_corpus());
        }
        let data = self
            .catalogue()
            .await
            .map_err(CatalogueError::into_response)?;
        let index = match &data.index {
            Ok(index) => index,
            Err(why) => {
                tracing::warn!(error = %why, "hub catalogue does not parse");
                return Err(error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "hub_unavailable",
                    "the hub catalogue cannot be read right now",
                ));
            }
        };
        match index.corpora.get(slug) {
            Some(rule) if rule.licence_class != LicenceClass::Excluded => Ok(()),
            _ => Err(unknown_corpus()),
        }
    }

    /// The `search` fields that at least one searched index maps.
    ///
    /// This engine does not ignore an unmapped `multi_match` field the way
    /// Elasticsearch does: a field that NO searched index maps collapses a
    /// multi-token query to zero hits (`xccode::fields::resolve_fields` has the
    /// measurement). So the fixed list is intersected with the mappings'
    /// union — the same gate the CLI applies — and the answer is reused for
    /// [`SEARCH_FIELDS_TTL`], since reading the mappings of every pack is far
    /// more expensive than a search. Blocking.
    fn search_fields_blocking(&self, node: &InProcessNode) -> Vec<&'static str> {
        {
            let cache = self.search_fields.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, fields)) = cache.as_ref() {
                if at.elapsed() < SEARCH_FIELDS_TTL {
                    return fields.clone();
                }
            }
        }
        let path = format!("/{SEARCH_INDEX_EXPR}/_mapping");
        let fields = match node.get_mapping(&path) {
            Ok(mapping) => mapped_search_fields(&mapping),
            // No `xc-*` index yet: nothing to search, and not an error. Not
            // remembered, so the first pack restored is searchable on the next
            // request rather than a minute later.
            Err(e) if e.contains("__404__") => return Vec::new(),
            // A failed read is not remembered either: one transient fault must
            // not blank `search` for the whole TTL.
            Err(e) => {
                tracing::warn!(error = ?truncate_chars(&e, 500), "hub search: cannot read mappings");
                return Vec::new();
            }
        };
        *self.search_fields.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), fields.clone()));
        fields
    }
}

struct CachedCatalogue {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    data: Arc<CatalogueData>,
}

/// The catalogue file as read: its bytes and etag (what `GET /catalogue`
/// serves), and the corpora it lists (what every other route validates against).
struct CatalogueData {
    bytes: Bytes,
    etag: String,
    index: Result<CatalogueIndex, String>,
}

struct CatalogueIndex {
    corpora: HashMap<String, CorpusRule>,
}

/// What the catalogue says about one corpus that a route needs.
#[derive(Clone)]
struct CorpusRule {
    licence_spdx: String,
    licence_class: LicenceClass,
}

impl CatalogueData {
    fn from_bytes(bytes: Bytes) -> Self {
        let etag = format!("\"{}\"", hex(&Sha256::digest(&bytes)));
        let index = serde_json::from_slice::<Catalogue>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|c| {
                if c.format_version != CATALOGUE_FORMAT_VERSION {
                    return Err(format!(
                        "catalogue format_version {} (this build reads {CATALOGUE_FORMAT_VERSION})",
                        c.format_version
                    ));
                }
                Ok(CatalogueIndex {
                    corpora: c
                        .corpora
                        .into_iter()
                        .map(|e| {
                            (
                                e.slug,
                                CorpusRule {
                                    licence_spdx: e.licence_spdx,
                                    licence_class: e.licence_class,
                                },
                            )
                        })
                        .collect(),
                })
            });
        Self { bytes, etag, index }
    }
}

enum CatalogueError {
    /// `hub.catalogue_path` is empty.
    NotConfigured,
    /// Configured, but the file cannot be used; the detail is for the log only.
    Unavailable(String),
}

impl CatalogueError {
    fn into_response(self) -> Response {
        match self {
            CatalogueError::NotConfigured => error(
                StatusCode::NOT_FOUND,
                "resource_not_found_exception",
                "this node has no hub catalogue (hub.catalogue_path is not set)",
            ),
            CatalogueError::Unavailable(detail) => {
                tracing::warn!(error = %detail, "hub catalogue unavailable");
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "hub_unavailable",
                    "the hub catalogue cannot be read right now",
                )
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Which requests are public
// ─────────────────────────────────────────────────────────────────────────────

/// Is `path` exactly one of the four hosted routes?
///
/// By shape, never by prefix: `/_hub/v1/catalogue`, `/_hub/v1/search`,
/// `/_hub/v1/code/{slug}` and `/_hub/v1/def/{slug}/{symbol}`, each segment
/// non-empty and nothing after the last one. A trailing slash, an extra
/// segment, a different case or an empty segment is NOT public — it falls
/// through to authentication like any other path, which is the safe direction
/// for a predicate that grants access.
pub fn is_public_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/_hub/v1/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        ["catalogue"] | ["search"] => true,
        ["code", slug] => !slug.is_empty(),
        ["def", slug, symbol] => !slug.is_empty() && !symbol.is_empty(),
        _ => false,
    }
}

/// Should this request skip authentication, authorization and the audit log?
///
/// True only when the node is configured `hub.public = true` AND the path is
/// one of the four hosted routes ([`is_public_path`]). Any method: a `POST` to
/// one of them reaches the router and gets its `405`, and it must not reach the
/// audit layer, which would record the unauthenticated attempt.
pub fn is_public_request(state: &AppState, req: &Request) -> bool {
    state.config.hub.public && is_public_path(req.uri().path())
}

// ─────────────────────────────────────────────────────────────────────────────
// Rate limiting
// ─────────────────────────────────────────────────────────────────────────────

/// A per-client sliding-window limiter: at most `limit` requests in any
/// `window`, and `Retry-After` is the time until the oldest of them ages out.
///
/// A sliding window rather than a refilling token bucket: the plan asks for
/// "60 requests/minute, the 61st in a minute is `429`", and a token bucket
/// that refills continuously lets a client make `limit` requests at once and
/// then more as tokens trickle back — up to twice the stated rate over the same
/// minute — so the 61st request could succeed when the machine is slow.
///
/// Memory is bounded: each client holds at most `limit` timestamps, and at most
/// `max_clients` clients are tracked; past that, new addresses share one
/// overflow bucket.
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    max_clients: usize,
    inner: Mutex<LimiterInner>,
}

struct LimiterInner {
    clients: HashMap<String, VecDeque<Instant>>,
    calls: u64,
}

const OVERFLOW_KEY: &str = "overflow";
/// Expired clients are swept every this-many calls.
const SWEEP_EVERY: u64 = 256;

impl RateLimiter {
    pub fn new(limit: usize, window: Duration, max_clients: usize) -> Self {
        Self {
            limit,
            window,
            max_clients,
            inner: Mutex::new(LimiterInner {
                clients: HashMap::new(),
                calls: 0,
            }),
        }
    }

    /// Admit one request from `client` at `now`, or say how many whole seconds
    /// (at least one) until a slot frees. A refused request is not recorded:
    /// hammering a `429` does not extend it.
    pub fn admit(&self, client: &str, now: Instant) -> Result<(), u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.calls = inner.calls.wrapping_add(1);
        let window = self.window;
        let live = |q: &VecDeque<Instant>| {
            q.back()
                .is_some_and(|t| now.saturating_duration_since(*t) < window)
        };
        if inner.calls.is_multiple_of(SWEEP_EVERY) {
            inner.clients.retain(|_, q| live(q));
        }
        let mut key = client;
        if !inner.clients.contains_key(key) && inner.clients.len() >= self.max_clients {
            // Table full: reclaim what has expired before conceding to the
            // shared overflow bucket.
            inner.clients.retain(|_, q| live(q));
            if inner.clients.len() >= self.max_clients {
                key = OVERFLOW_KEY;
            }
        }
        let q = inner.clients.entry(key.to_string()).or_default();
        while q
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= window)
        {
            q.pop_front();
        }
        if q.len() >= self.limit {
            let oldest = q.front().copied().unwrap_or(now);
            let wait = window.saturating_sub(now.saturating_duration_since(oldest));
            let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            return Err(secs.max(1));
        }
        q.push_back(now);
        Ok(())
    }
}

/// The rate-limit key for a client address: the address itself for IPv4, the
/// enclosing /64 for IPv6 (one host is routinely delegated a whole /64 and can
/// take a fresh address per request, so per-address buckets there are no
/// throttle at all), and anything that is not an address as its own bucket.
/// Mirrors `share::source_bucket`, which is private to that module.
pub fn client_bucket(source: &str) -> String {
    match source.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => {
            let mut seg = v6.segments();
            seg[4..].fill(0);
            format!("{}/64", std::net::Ipv6Addr::from(seg))
        }
        _ => source.to_string(),
    }
}

async fn rate_limit(State(shared): State<Arc<HubShared>>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let source = match ClaimSource::from_request_parts(&mut parts, &shared.app).await {
        Ok(ClaimSource(source)) => source,
        Err(never) => match never {},
    };
    let req = Request::from_parts(parts, body);
    match shared
        .limiter
        .admit(&client_bucket(&source), Instant::now())
    {
        Ok(()) => next.run(req).await,
        Err(retry_after) => {
            let mut resp = error(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_requests",
                format!(
                    "more than {RATE_LIMIT} requests in {} seconds from this address; \
                     retry in {retry_after}s",
                    RATE_WINDOW.as_secs()
                ),
            );
            if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            resp
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The in-process XcHttp
// ─────────────────────────────────────────────────────────────────────────────

/// Engine-level visibility for hosted queries: `xc-*` indices only.
///
/// Installed around every internal call. The engine consults it at the
/// funnel every name passes through (`Engine::get_index`, `list_indices`), so
/// an alias, a wildcard or a bug upstream of this module still cannot reach an
/// index outside the hub namespace.
struct HubVisibility;

impl IndexVisibility for HubVisibility {
    fn visible(&self, index: &str) -> bool {
        index.starts_with("xc-")
    }
}

/// The three read routes `lookup` and `run_code_query` need, and nothing else,
/// with no middleware: reachable only from [`InProcessNode`], never from a
/// socket.
fn internal_router(app: &AppState) -> Router {
    Router::new()
        .route(
            "/_cat/indices/:pattern",
            get(es_compat::cat_indices_pattern),
        )
        .route("/:index/_mapping", get(es_compat::get_mapping))
        .route("/:index/_search", post(es_compat::search))
        .with_state(app.clone())
}

/// Is `expr` a comma list of `xc-` index names in a character set that cannot
/// change the meaning of a URL path? Anything else is refused before a request
/// is built.
fn hub_index_expr_ok(expr: &str) -> bool {
    !expr.is_empty()
        && expr.len() <= 4096
        && expr.split(',').all(|part| {
            part.starts_with("xc-")
                && part.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'*' | b'-')
                })
        })
}

/// [`XcHttp`] over the node's own engine, in-process (see the module docs).
///
/// Methods block; call from the blocking pool, never from an async task.
pub struct InProcessNode {
    router: Router,
    rt: tokio::runtime::Handle,
    /// The caller's `Authorization` header, forwarded only so the audit entry a
    /// search writes names the right principal. The private router has no
    /// authentication layer; nothing here grants anything.
    auth: Option<HeaderValue>,
}

impl InProcessNode {
    fn new(app: &AppState, auth: Option<HeaderValue>) -> Self {
        Self {
            router: internal_router(app),
            rt: tokio::runtime::Handle::current(),
            auth,
        }
    }

    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Bytes), String> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(a) = &self.auth {
            builder = builder.header(header::AUTHORIZATION, a.clone());
        }
        let req = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(b).map_err(|e| e.to_string())?,
                )),
            None => builder.body(Body::empty()),
        }
        .map_err(|e| format!("cannot build the internal request: {e}"))?;
        let router = self.router.clone();
        let call = async move {
            let resp = match router.oneshot(req).await {
                Ok(resp) => resp,
                Err(never) => match never {},
            };
            let status = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), MAX_INTERNAL_RESPONSE_BYTES)
                .await
                .map_err(|_| {
                    format!("internal response over {MAX_INTERNAL_RESPONSE_BYTES} bytes")
                })?;
            Ok::<_, String>((status, bytes))
        };
        // `block_on` is legal here: this runs on a blocking-pool thread, never
        // on an async worker. The visibility guard is a task-local, so it must
        // be installed inside the future this thread drives.
        self.rt
            .block_on(index_guard::scoped(Arc::new(HubVisibility), call))
    }

    fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let (status, bytes) = self.send(method, path, body)?;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, value))
    }
}

impl XcHttp for InProcessNode {
    fn get_mapping(&self, path: &str) -> Result<Value, String> {
        let expr = path
            .strip_prefix('/')
            .and_then(|p| p.strip_suffix("/_mapping"))
            .filter(|e| hub_index_expr_ok(e))
            .ok_or_else(|| "refused: not a hub mapping path".to_string())?;
        match self.json(Method::GET, &format!("/{expr}/_mapping"), None)? {
            (s, v) if (200..300).contains(&s) => Ok(v),
            (404, _) => Err("__404__".to_string()),
            (s, v) => Err(format!("HTTP {s}: {v}")),
        }
    }

    fn cat_indices_json(&self, pattern: &str) -> Result<Vec<String>, String> {
        if !hub_index_expr_ok(pattern) {
            return Err("refused: not a hub index pattern".to_string());
        }
        let path = format!("/_cat/indices/{pattern}?format=json&h=index");
        match self.json(Method::GET, &path, None)? {
            // A wildcard matching no index is "none", not an error.
            (404, _) => Ok(Vec::new()),
            (s, v) if (200..300).contains(&s) => v
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r.get("index").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .ok_or_else(|| "non-array _cat reply".to_string()),
            (s, v) => Err(format!("HTTP {s}: {v}")),
        }
    }

    fn search(&self, index: &str, body: &Value) -> Result<Value, String> {
        if !hub_index_expr_ok(index) {
            return Err("refused: not a hub index expression".to_string());
        }
        match self.json(Method::POST, &format!("/{index}/_search"), Some(body))? {
            (s, v) if (200..300).contains(&s) => Ok(v),
            (404, _) => Err("__404__".to_string()),
            (s, v) => Err(format!("HTTP {s}: {v}")),
        }
    }
}

/// Run `work` on the blocking pool with an [`InProcessNode`], under the global
/// concurrency cap and the wall-clock timeout.
///
/// The permit is held by the blocking closure, not by this future, so a query
/// that outlives its timeout keeps counting against the cap until it really
/// finishes — the cap bounds blocking threads, not just waiting requests.
async fn run_query<T, F>(
    shared: &Arc<HubShared>,
    headers: &HeaderMap,
    work: F,
) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(&InProcessNode) -> T + Send + 'static,
{
    let Ok(permit) = shared.slots.clone().try_acquire_owned() else {
        let mut resp = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "hub_busy",
            "the hub is answering as many queries as it can; retry shortly",
        );
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return Err(resp);
    };
    let node = InProcessNode::new(&shared.app, headers.get(header::AUTHORIZATION).cloned());
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(&node)
    });
    match tokio::time::timeout(QUERY_TIMEOUT, job).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "hub query task failed");
            Err(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "hub_internal_error",
                "the hub could not answer this request",
            ))
        }
        Err(_) => Err(error(
            StatusCode::GATEWAY_TIMEOUT,
            "hub_timeout",
            format!("the hub did not answer within {}s", QUERY_TIMEOUT.as_secs()),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request validation
// ─────────────────────────────────────────────────────────────────────────────

/// A corpus name the catalogue could legitimately hold, and one that is safe to
/// put in a filesystem path and an index pattern: ASCII alphanumeric first,
/// then alphanumerics and `. _ + -`, at most 64 bytes. Checked before the
/// catalogue is consulted, so nothing else is ever looked up or built from it.
pub fn valid_slug(slug: &str) -> bool {
    let mut bytes = slug.bytes();
    slug.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// Collect the query string into a map, refusing a parameter this route does
/// not take and a parameter given twice.
fn collect_params(
    pairs: Vec<(String, String)>,
    allowed: &[&str],
) -> Result<HashMap<String, String>, Response> {
    let mut out = HashMap::new();
    for (key, value) in pairs {
        if !allowed.contains(&key.as_str()) {
            return Err(bad_request(format!(
                "request contains unrecognized parameter [{}]; this route takes {}",
                truncate_chars(&key, 32),
                allowed.join(", ")
            )));
        }
        if out.insert(key.clone(), value).is_some() {
            return Err(bad_request(format!(
                "parameter [{key}] given more than once"
            )));
        }
    }
    Ok(out)
}

/// A required text parameter: present, non-blank, at most [`MAX_INPUT_BYTES`].
fn text_param(raw: Option<&str>, name: &str) -> Result<String, Response> {
    let raw = raw.ok_or_else(|| bad_request(format!("`{name}` is required")))?;
    check_text(raw, name)
}

/// The same limit for a value that arrived as a path segment.
fn check_text(raw: &str, name: &str) -> Result<String, Response> {
    if raw.len() > MAX_INPUT_BYTES {
        return Err(bad_request(format!(
            "`{name}` is limited to {MAX_INPUT_BYTES} bytes, got {}",
            raw.len()
        )));
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(bad_request(format!("`{name}` must not be blank")));
    }
    Ok(trimmed.to_string())
}

/// `k`: absent is `default`; otherwise an integer of at least 1, clamped to
/// [`MAX_K`]. Zero and non-numbers are a `400`, not a silent default.
pub fn parse_k(raw: Option<&str>, default: usize) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) | Err(_) => Err(format!(
            "`k` must be an integer from 1 to {MAX_K}, got [{}]",
            truncate_chars(raw, 32)
        )),
        Ok(n) => Ok(usize::try_from(n).unwrap_or(MAX_K).min(MAX_K)),
    }
}

/// `max_tokens`: absent is no budget; otherwise an integer from 1 to
/// [`MAX_TOKENS_CAP`].
pub fn parse_max_tokens(raw: Option<&str>) -> Result<Option<u64>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.trim().parse::<u64>() {
        Ok(n) if (1..=MAX_TOKENS_CAP).contains(&n) => Ok(Some(n)),
        _ => Err(format!(
            "`max_tokens` must be an integer from 1 to {MAX_TOKENS_CAP}, got [{}]",
            truncate_chars(raw, 32)
        )),
    }
}

/// `mode`: the retrieval arm, as `xerj code --mode` spells it.
pub fn parse_mode(raw: Option<&str>) -> Result<Mode, String> {
    match raw {
        None => Ok(Mode::Bm25),
        Some(s) => Mode::parse(s.trim()).ok_or_else(|| {
            format!(
                "`mode` must be bm25, semantic or hybrid, got [{}]",
                truncate_chars(s, 32)
            )
        }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Responses
// ─────────────────────────────────────────────────────────────────────────────

fn error(status: StatusCode, error_type: &str, reason: impl Into<String>) -> Response {
    let reason = reason.into();
    let body = EsErrorResponse {
        error: EsErrorBody {
            root_cause: vec![EsRootCause {
                error_type: error_type.to_string(),
                reason: reason.clone(),
                resource_type: None,
                resource_id: None,
                index_uuid: None,
                index: None,
            }],
            error_type: error_type.to_string(),
            reason,
            resource_type: None,
            resource_id: None,
            index_uuid: None,
            index: None,
            request_id: None,
        },
        status: status.as_u16(),
        xerj_hint: None,
    };
    (status, Json(body)).into_response()
}

fn bad_request(reason: impl Into<String>) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "illegal_argument_exception",
        reason,
    )
}

fn unknown_corpus() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "resource_not_found_exception",
        "no such corpus in this hub's catalogue",
    )
}

fn not_loaded() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "corpus_not_loaded",
        "this corpus is in the catalogue but is not loaded on this node",
    )
}

/// A backend failure. The detail goes to the log; the client gets a fixed
/// sentence, because the detail can name filesystem paths and engine internals.
fn unavailable(detail: &str) -> Response {
    // Debug formatting escapes control characters: the detail can echo what the
    // caller sent, and a raw newline in it would forge log lines. Bounded, so
    // an engine error carrying a large body cannot flood the log either.
    tracing::warn!(error = ?truncate_chars(detail, 500), "hub query failed");
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "hub_unavailable",
        "the hub could not answer from this corpus right now",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Routes
// ─────────────────────────────────────────────────────────────────────────────

/// The four routes, with their own state and the rate limit in front of all of
/// them (a layer, not a per-handler call, so a route added here cannot forget
/// it). Generic over the outer router's state: this router carries its own.
pub fn routes<S>(shared: Arc<HubShared>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<Arc<HubShared>>::new()
        .route("/_hub/v1/catalogue", get(catalogue_route))
        .route("/_hub/v1/def/:slug/:symbol", get(def_route))
        .route("/_hub/v1/code/:slug", get(code_route))
        .route("/_hub/v1/search", get(search_route))
        .layer(middleware::from_fn_with_state(shared.clone(), rate_limit))
        .with_state(shared)
}

/// `GET /_hub/v1/catalogue`.
async fn catalogue_route(State(shared): State<Arc<HubShared>>, headers: HeaderMap) -> Response {
    let data = match shared.catalogue().await {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    let mut resp = if etag_matches(headers.get(header::IF_NONE_MATCH), &data.etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, "application/json")],
            data.bytes.clone(),
        )
            .into_response()
    };
    if let Ok(v) = HeaderValue::from_str(&data.etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    resp
}

/// Does an `If-None-Match` header match `etag`? Handles `*`, a comma list, and
/// weak (`W/`) validators, which compare equal for a conditional GET.
pub fn etag_matches(header: Option<&HeaderValue>, etag: &str) -> bool {
    let Some(value) = header.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// What a blocking query reports back.
enum Outcome {
    Done(Box<Answer>),
    NotLoaded,
    /// The corpus's own ledger says `excluded`, whatever the catalogue lists:
    /// both must permit serving, and either can refuse.
    NotServed,
    Failed(String),
}

fn respond(outcome: Result<Outcome, Response>) -> Response {
    match outcome {
        Ok(Outcome::Done(answer)) => Json(*answer).into_response(),
        Ok(Outcome::NotLoaded) => not_loaded(),
        Ok(Outcome::NotServed) => unknown_corpus(),
        Ok(Outcome::Failed(detail)) => unavailable(&detail),
        Err(resp) => resp,
    }
}

/// `GET /_hub/v1/def/{slug}/{symbol}?k=`.
async fn def_route(
    State(shared): State<Arc<HubShared>>,
    Path((slug, symbol)): Path<(String, String)>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let params = match collect_params(pairs, &["k"]) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let symbol = match check_text(&symbol, "symbol") {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let k = match parse_k(params.get("k").map(String::as_str), DEFAULT_K) {
        Ok(k) => k,
        Err(why) => return bad_request(why),
    };
    if let Err(resp) = shared.resolve_corpus(&slug).await {
        return resp;
    }
    let outcome = run_query(&shared, &headers, move |node| {
        let root = hub_root();
        let facts = match corpus_facts(&root, &slug) {
            Ok(f) => f,
            Err(_) => return Outcome::NotLoaded,
        };
        if facts.licence_class == LicenceClass::Excluded {
            return Outcome::NotServed;
        }
        // Mirror the CLI's refusals: a corpus the ledger knows but this node
        // holds no index for is "not loaded", not "no such definition".
        match node.cat_indices_json(&format!("{}*", facts.index_prefix)) {
            Ok(live) if live.is_empty() => return Outcome::NotLoaded,
            Ok(_) => {}
            Err(e) => return Outcome::Failed(format!("listing indices: {e}")),
        }
        let mut answer = xerj_hub::def::lookup_in(node, &root, &slug, &symbol, k);
        // `lookup` reports a failed query as a warning on an empty answer. The
        // CLI maps that to an exit code earlier; over HTTP a failure must not
        // read as "no such definition".
        if let Some(w) = answer
            .warnings
            .iter()
            .find(|w| w.starts_with("defs query failed"))
        {
            return Outcome::Failed(w.clone());
        }
        answer.answered_from = AnsweredFrom::Hosted;
        Outcome::Done(Box::new(answer))
    })
    .await;
    respond(outcome)
}

/// `GET /_hub/v1/code/{slug}?q=&k=&max_tokens=&mode=`.
async fn code_route(
    State(shared): State<Arc<HubShared>>,
    Path(slug): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let params = match collect_params(pairs, &["q", "k", "max_tokens", "mode"]) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let query = match text_param(params.get("q").map(String::as_str), "q") {
        Ok(q) => q,
        Err(resp) => return resp,
    };
    let k = match parse_k(params.get("k").map(String::as_str), DEFAULT_K) {
        Ok(k) => k,
        Err(why) => return bad_request(why),
    };
    let max_tokens = match parse_max_tokens(params.get("max_tokens").map(String::as_str)) {
        Ok(m) => m,
        Err(why) => return bad_request(why),
    };
    let mode = match parse_mode(params.get("mode").map(String::as_str)) {
        Ok(m) => m,
        Err(why) => return bad_request(why),
    };
    if let Err(resp) = shared.resolve_corpus(&slug).await {
        return resp;
    }
    let width = passage_width(max_tokens);
    let outcome = run_query(&shared, &headers, move |node| {
        let root = hub_root();
        let facts = match corpus_facts(&root, &slug) {
            Ok(f) => f,
            Err(_) => return Outcome::NotLoaded,
        };
        if facts.licence_class == LicenceClass::Excluded {
            return Outcome::NotServed;
        }
        let mut params = CodeParams::new(&slug, &query);
        params.k = k;
        params.mode = mode;
        params.full = width;
        // The raw server response, patched with the diversified page: the
        // same list the prose renderer prints, as data.
        params.as_json = true;
        let out = xccode::run_code_query(
            &root,
            node,
            "in-process",
            &params,
            "not available on the hosted route",
        );
        match out.exit {
            0 | 1 => {
                let resp = out.json.unwrap_or(Value::Null);
                let mut hits = record_hits(&resp, &query, width);
                let mut warnings = out.warnings;
                if let Some(note) = approach_only_note(&facts) {
                    for hit in &mut hits {
                        hit.licence_warning = Some(note.clone());
                    }
                }
                if mode != Mode::Bm25 {
                    warnings.push(
                        "mode semantic/hybrid falls back to BM25 when the corpus has no \
                         semantic_text mapping, and this response does not say which arms ran"
                            .to_string(),
                    );
                }
                let truncated = match max_tokens {
                    Some(budget) => apply_token_budget(&mut hits, budget, &mut warnings),
                    None => false,
                };
                Outcome::Done(Box::new(facts.into_answer(
                    AnswerMode::How,
                    hits,
                    truncated,
                    warnings,
                )))
            }
            3 => Outcome::NotLoaded,
            _ => Outcome::Failed(out.text),
        }
    })
    .await;
    respond(outcome)
}

/// `GET /_hub/v1/search?q=&k=`.
async fn search_route(
    State(shared): State<Arc<HubShared>>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    let params = match collect_params(pairs, &["q", "k"]) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let query = match text_param(params.get("q").map(String::as_str), "q") {
        Ok(q) => q,
        Err(resp) => return resp,
    };
    let k = match parse_k(params.get("k").map(String::as_str), SEARCH_DEFAULT_K) {
        Ok(k) => k.min(MAX_SEARCH_CORPORA),
        Err(why) => return bad_request(why),
    };
    let data = match shared.catalogue().await {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    if let Err(why) = &data.index {
        tracing::warn!(error = %why, "hub catalogue does not parse");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "hub_unavailable",
            "the hub catalogue cannot be read right now",
        );
    }
    let blocking_shared = shared.clone();
    let outcome = run_query(&shared, &headers, move |node| {
        let fields = blocking_shared.search_fields_blocking(node);
        if fields.is_empty() {
            return Ok(Vec::new());
        }
        // Only the top hits are read, never a total: with the fan-out over every
        // pack index, counting matches is work nobody asked for.
        let body = json!({
            "size": SEARCH_POOL,
            "track_total_hits": false,
            "_source": SEARCH_SOURCE_FIELDS,
            "query": { "multi_match": { "query": query, "fields": fields } },
        });
        let resp = match node.search(SEARCH_INDEX_EXPR, &body) {
            Ok(r) => r,
            Err(e) if e.contains("__404__") => return Ok(Vec::new()),
            Err(e) => return Err(format!("search failed: {e}")),
        };
        let corpora = match &data.index {
            Ok(index) => group_by_corpus(&resp, &index.corpora, k),
            Err(_) => Vec::new(),
        };
        Ok(corpora)
    })
    .await;
    match outcome {
        Ok(Ok(corpora)) => {
            let rows: Vec<Value> = corpora
                .into_iter()
                .map(|c| json!({ "slug": c.slug, "best_score": c.hit.score, "best_hit": c.hit }))
                .collect();
            Json(json!({ "corpora": rows })).into_response()
        }
        Ok(Err(detail)) => unavailable(&detail),
        Err(resp) => resp,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Facts about a loaded corpus
// ─────────────────────────────────────────────────────────────────────────────

/// Where the state ledger lives — the same root `xerj hub pull` writes and
/// `xerj_hub::def::lookup` reads.
fn hub_root() -> PathBuf {
    xerj_autoindex::xc::code_root()
}

/// What an [`Answer`] says about the corpus it came from, from the ledger the
/// pull wrote. Mirrors `xerj_hub::def`'s private `DefMeta` so a `def` answer
/// and a `code` answer for the same corpus carry the same header; a test pins
/// the two together.
struct CorpusFacts {
    corpus: String,
    index_prefix: String,
    pin_sha: String,
    pin_date: String,
    age_days: u32,
    licence_spdx: String,
    licence_class: LicenceClass,
}

fn corpus_facts(root: &FsPath, slug: &str) -> Result<CorpusFacts, String> {
    let st = ledger::load_state(root, slug)?;
    let pin_date = st.pin_date.clone().unwrap_or_default();
    Ok(CorpusFacts {
        corpus: st.name(slug),
        index_prefix: ledger::query_prefix(&st),
        age_days: age_days_from(&pin_date, st.indexed_at.as_deref()),
        pin_sha: st.pin_sha.clone().unwrap_or_default(),
        pin_date,
        licence_spdx: st.licence_spdx.clone().unwrap_or_default(),
        licence_class: parse_licence_class(st.licence_class.as_deref()),
    })
}

impl CorpusFacts {
    fn into_answer(
        self,
        mode: AnswerMode,
        hits: Vec<Hit>,
        truncated: bool,
        warnings: Vec<String>,
    ) -> Answer {
        Answer {
            corpus: self.corpus,
            pin_sha: self.pin_sha,
            pin_date: self.pin_date,
            age_days: self.age_days,
            licence_spdx: self.licence_spdx,
            licence_class: self.licence_class,
            answered_from: AnsweredFrom::Hosted,
            mode,
            hits,
            truncated,
            warnings,
        }
    }
}

/// An unknown or absent class never upgrades to a stricter one by surprise.
fn parse_licence_class(s: Option<&str>) -> LicenceClass {
    match s {
        Some("approach-only") => LicenceClass::ApproachOnly,
        Some("excluded") => LicenceClass::Excluded,
        _ => LicenceClass::AdaptWithAttribution,
    }
}

/// Days from the pin date (else the index stamp) to today, never negative.
fn age_days_from(pin_date: &str, indexed_at: Option<&str>) -> u32 {
    if let Ok(d) = NaiveDate::parse_from_str(pin_date, "%Y-%m-%d") {
        return (chrono::Utc::now().date_naive() - d).num_days().max(0) as u32;
    }
    if let Some(stamp) = indexed_at {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%dT%H:%M:%SZ") {
            let age = chrono::Utc::now().naive_utc().signed_duration_since(dt);
            return age.num_days().max(0) as u32;
        }
    }
    0
}

fn approach_only_note(facts: &CorpusFacts) -> Option<String> {
    (facts.licence_class == LicenceClass::ApproachOnly).then(|| licence_note(&facts.licence_spdx))
}

/// The wording `xerj_hub::def` puts on every hit of an approach-only corpus.
fn licence_note(spdx: &str) -> String {
    format!(
        "approach-only licence ({}): read, do not copy",
        if spdx.is_empty() { "unknown" } else { spdx }
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Building hits
// ─────────────────────────────────────────────────────────────────────────────

/// `s` cut to at most `n` characters (never splitting a code point).
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// `path:12` → (`path`, 12); anything else is all path and line 0 (a line is
/// never invented).
fn split_locator(loc: &str) -> (String, u32) {
    if let Some((path, tail)) = loc.rsplit_once(':') {
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = tail.parse::<u64>() {
                return (path.to_string(), u32::try_from(n).unwrap_or(u32::MAX));
            }
        }
    }
    (loc.to_string(), 0)
}

/// The text of a code record: `body`, else `text`, else its longest string
/// field (a raw-JSON record carries its content under schema-named keys).
/// Mirrors the renderer's choice in `xccode::render`, whose helper is private.
fn record_body(src: &Value) -> String {
    if let Some(s) = src.get("body").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = src.get("text").and_then(Value::as_str) {
        return s.to_string();
    }
    let mut best: Option<&str> = None;
    if let Some(obj) = src.as_object() {
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(s) = obj[key].as_str() {
                if !s.trim().is_empty()
                    && best.is_none_or(|b| s.chars().count() > b.chars().count())
                {
                    best = Some(s);
                }
            }
        }
    }
    best.unwrap_or_default().to_string()
}

/// One retrieved record as a [`Hit`], with the passage the CLI would print: the
/// best-matching definition when the record carries symbols, else the densest
/// window.
fn hit_from_record(row: &Value, query: &str, width: usize) -> Hit {
    let src = row.get("_source").unwrap_or(&Value::Null);
    let score = row.get("_score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    // The pack's `-defs` index sits under the same prefix, so a definition
    // record can come back here. Keep what it is — its kind, its value, its
    // exact line — instead of passing it off as a prose passage.
    if src.get("symbol").is_some_and(Value::is_string) {
        return def_hit(src, score, width);
    }
    let (file, record_line) = split_locator(&passage::provenance(src));
    let body = record_body(src);
    let (text, kind, line) = match passage::symbol_passage(&body, src, query, width) {
        Some((text, label)) => {
            let (kind, symbol_line) = parse_symbol_label(&label);
            // The matched definition's line, as the extractor recorded it against
            // the whole-file `body`, so it is a file line. A record with no
            // symbol line falls back to its own `line`, or 0: a line is reported
            // only when the index supplied one.
            let line = if symbol_line > 0 {
                symbol_line
            } else {
                record_line
            };
            (text, kind, line)
        }
        None => {
            let (window, _start, _total) = passage::best_window(&body, query, width);
            (window, "passage".to_string(), record_line)
        }
    };
    Hit {
        file,
        line,
        kind,
        text,
        value: None,
        score,
        licence_warning: None,
    }
}

/// `"function addReplyNull @ line 12"` → (`function`, 12).
fn parse_symbol_label(label: &str) -> (String, u32) {
    let Some((head, line)) = label.rsplit_once(" @ line ") else {
        return ("passage".to_string(), 0);
    };
    let kind = head.split_whitespace().next().unwrap_or("passage");
    let line = line.trim().parse::<u32>().unwrap_or(0);
    (kind.to_string(), line)
}

fn record_hits(resp: &Value, query: &str, width: usize) -> Vec<Hit> {
    resp.pointer("/hits/hits")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| hit_from_record(row, query, width))
                .collect()
        })
        .unwrap_or_default()
}

/// Characters per passage: the CLI's default, or less when `max_tokens` is
/// smaller than that.
pub fn passage_width(max_tokens: Option<u64>) -> usize {
    match max_tokens {
        Some(t) => usize::try_from(t)
            .unwrap_or(usize::MAX)
            .saturating_mul(CHARS_PER_TOKEN)
            .clamp(1, DEFAULT_PASSAGE_CHARS),
        None => DEFAULT_PASSAGE_CHARS,
    }
}

/// Hold the hits' passage text to `max_tokens`, counted as
/// [`CHARS_PER_TOKEN`] characters a token.
///
/// Hits keep their rank order while they fit. The first that does not is cut to
/// what is left and every later one is dropped. The budget covers passage text
/// only — file, line and the rest of the JSON are not counted — and the
/// four-characters-a-token figure is an approximation, not a model tokenizer;
/// the warning added whenever the budget binds says both. Returns whether
/// anything was cut or dropped.
pub fn apply_token_budget(
    hits: &mut Vec<Hit>,
    max_tokens: u64,
    warnings: &mut Vec<String>,
) -> bool {
    let budget = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(CHARS_PER_TOKEN);
    let mut used = 0usize;
    let mut cut = false;
    let mut keep = 0usize;
    for (i, hit) in hits.iter_mut().enumerate() {
        let len = hit.text.chars().count();
        if used.saturating_add(len) <= budget {
            used += len;
            keep = i + 1;
            continue;
        }
        let left = budget - used;
        if left > 0 {
            hit.text = truncate_chars(&hit.text, left);
            keep = i + 1;
        }
        cut = true;
        break;
    }
    let omitted = hits.len() - keep;
    hits.truncate(keep);
    if cut || omitted > 0 {
        warnings.push(format!(
            "answer held to max_tokens={max_tokens} (about {CHARS_PER_TOKEN} characters a token, \
             passage text only); {omitted} lower-ranked hit(s) omitted"
        ));
        return true;
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────────
// Federated search
// ─────────────────────────────────────────────────────────────────────────────

/// The `search` fields that some index in `mapping` (a `GET /{pattern}/_mapping`
/// reply) maps.
pub fn mapped_search_fields(mapping: &Value) -> Vec<&'static str> {
    let mut present = std::collections::BTreeSet::new();
    if let Some(obj) = mapping.as_object() {
        for m in obj.values() {
            if let Some(props) = m.pointer("/mappings/properties").and_then(Value::as_object) {
                present.extend(props.keys().map(String::as_str));
            }
        }
    }
    SEARCH_FIELDS
        .into_iter()
        .filter(|f| present.contains(f))
        .collect()
}

/// The corpus a hub index belongs to: `xc-<slug>-b<digits>` followed by `-…` or
/// the end, with `<slug>` one of `known`.
///
/// The rule `xerj corpus index` uses to tell a stamped build from a dataset
/// named like one (`stamped_prefix` in `xerj-autoindex`, which is private and
/// driven by a name already in hand): digits only between `-b` and the next
/// `-`. Driven by the catalogue here, so slugs containing `-` resolve, and the
/// shortest slug that fits wins, which is deterministic.
pub fn slug_of_index<'a, V>(index: &str, known: &'a HashMap<String, V>) -> Option<&'a str> {
    let rest = index.strip_prefix("xc-")?;
    for (at, _) in rest.match_indices("-b") {
        let after = &rest[at + 2..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let tail = &after[digits..];
        if !(tail.is_empty() || tail.starts_with('-')) {
            continue;
        }
        if let Some((slug, _)) = known.get_key_value(&rest[..at]) {
            return Some(slug.as_str());
        }
    }
    None
}

struct BestHit {
    slug: String,
    hit: Hit,
}

/// Group a `multi_match` reply by corpus: the best hit of each, best first
/// (ties by name), at most `want`. Hits from an index that belongs to no listed
/// corpus, and from an `excluded` one, are skipped.
fn group_by_corpus(resp: &Value, known: &HashMap<String, CorpusRule>, want: usize) -> Vec<BestHit> {
    let mut best: HashMap<&str, Hit> = HashMap::new();
    let rows = resp
        .pointer("/hits/hits")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for row in rows {
        let Some(index) = row.get("_index").and_then(Value::as_str) else {
            continue;
        };
        let Some(slug) = slug_of_index(index, known) else {
            continue;
        };
        let Some(rule) = known.get(slug) else {
            continue;
        };
        if rule.licence_class == LicenceClass::Excluded {
            continue;
        }
        let score = row.get("_score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        if best.get(slug).is_some_and(|h| h.score >= score) {
            continue;
        }
        let src = row.get("_source").unwrap_or(&Value::Null);
        // A definition record carries a `symbol`; anything else is a document
        // record. By shape, not by index name: a dataset directory may be
        // called `defs` too.
        let mut hit = if src.get("symbol").is_some_and(Value::is_string) {
            def_hit(src, score, EXCERPT_CHARS)
        } else {
            doc_hit(src, score)
        };
        if rule.licence_class == LicenceClass::ApproachOnly {
            hit.licence_warning = Some(licence_note(&rule.licence_spdx));
        }
        best.insert(slug, hit);
    }
    let mut out: Vec<BestHit> = best
        .into_iter()
        .map(|(slug, hit)| BestHit {
            slug: slug.to_string(),
            hit,
        })
        .collect();
    out.sort_by(|a, b| {
        b.hit
            .score
            .total_cmp(&a.hit.score)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    out.truncate(want);
    out
}

/// A best hit from a definition record: the definition itself.
fn def_hit(src: &Value, score: f32, max_chars: usize) -> Hit {
    let s = |k: &str| {
        src.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Hit {
        file: s("file"),
        line: src
            .get("line")
            .and_then(Value::as_u64)
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
        kind: s("kind"),
        text: truncate_chars(&s("text"), max_chars),
        value: src.get("value").and_then(Value::as_str).map(str::to_string),
        score,
        licence_warning: None,
    }
}

/// A best hit from a document record: where it is, and a short excerpt —
/// its definitions line list, else its text, else its title. Never the body.
fn doc_hit(src: &Value, score: f32) -> Hit {
    let (file, line) = split_locator(&passage::provenance(src));
    let excerpt = ["code", "defs", "text", "title"]
        .iter()
        .filter_map(|k| src.get(*k).and_then(Value::as_str))
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default();
    Hit {
        file,
        line,
        kind: "passage".to_string(),
        text: truncate_chars(excerpt, EXCERPT_CHARS),
        value: None,
        score,
        licence_warning: None,
    }
}

//! Corpus Hub v2 — node resolution and a blocking HTTP client to a xerj node
//! (plan §2 "Node for the client", §4 node resolution).
//!
//! `build-pack`, `pull` and `def` all talk to a running node: the builder
//! snapshots into one, `pull` restores into one, and `def` queries one. This
//! module resolves WHICH node ([`resolve_url`], the same `--url` / `XERJ_URL` /
//! `localhost:9200` order `xc.rs` uses) and gives a small blocking client
//! ([`HubNode`]) over it.
//!
//! The AUTOMATIC node spawn (start a node when none answers) is plan §5 and is
//! NOT done here — this module only resolves an explicit or defaulted URL and
//! connects to it. [`HubNode`] implements [`XcHttp`] so `def::lookup` runs over
//! it unchanged, and it adds the extra verbs (`PUT`/`POST` with an arbitrary
//! path, a readiness poll) the builder and the pull path need.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use xerj_common::localauth::{discover_local_admin_key, url_is_loopback};
use xerj_common::xccode::XcHttp;

/// `--url` flag > `XERJ_URL` > `http://localhost:9200`, exactly as `xc.rs`
/// resolves it for the reference-coding read commands (a hub `pull`/`def`
/// honours `XERJ_URL` the same way `corpus index` does).
pub fn resolve_url(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("XERJ_URL").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "http://localhost:9200".to_string())
}

/// A blocking HTTP client to one xerj node. Holds the resolved base URL and the
/// `Authorization: ApiKey …` value (if any), reusing one `reqwest` client.
pub struct HubNode {
    base: String,
    auth: Option<String>,
    client: reqwest::blocking::Client,
}

impl HubNode {
    /// Connect to `url`, resolving the API key the way `xc.rs` does:
    /// `XERJ_API_KEY` wins; otherwise, for a loopback URL, fall back to the
    /// node's on-disk admin key (announced, loopback-only). An `--insecure`
    /// node needs no key and none is sent.
    pub fn connect(url: &str) -> Result<Self> {
        Self::connect_with_key(url, None)
    }

    /// Connect with an explicit API key (overrides discovery). `api_key: None`
    /// uses the same `XERJ_API_KEY` / loopback-admin-key fallback as
    /// [`HubNode::connect`].
    pub fn connect_with_key(url: &str, api_key: Option<String>) -> Result<Self> {
        let base = url.trim_end_matches('/').to_string();
        let key = match api_key {
            Some(k) => Some(k),
            None => match std::env::var("XERJ_API_KEY").ok().filter(|k| !k.is_empty()) {
                Some(k) => Some(k),
                None if url_is_loopback(&base) => discover_local_admin_key().map(|(k, path)| {
                    eprintln!(
                        "xerj hub: no --api-key/XERJ_API_KEY given; using the admin key at {}",
                        path.display()
                    );
                    k
                }),
                None => None,
            },
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("build HTTP client")?;
        Ok(HubNode {
            base,
            auth: key.map(|k| format!("ApiKey {k}")),
            client,
        })
    }

    /// The resolved base URL (no trailing slash) — for diagnostics and the
    /// state ledger's `url` field.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// One request returning `(status, parsed-body)`. A non-JSON body comes
    /// back as `Value::String`. Transport failure (not an HTTP status) is an
    /// `Err`; the caller classifies HTTP statuses itself.
    fn send(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
        let url = format!("{}{}", self.base, path);
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("not an HTTP method: {method}"))?;
        let mut req = self.client.request(m, &url);
        if let Some(h) = &self.auth {
            req = req.header("Authorization", h);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .map_err(|e| format!("no response from {url}: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|e| format!("read {url}: {e}"))?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, v))
    }

    /// A request that must return 2xx, else an `anyhow` error carrying the
    /// status and body. Used by the builder/pull control-plane calls (create
    /// index, bulk, snapshot, restore).
    pub fn request_ok(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let (status, v) = self
            .send(method, path, body)
            .map_err(|e| anyhow!("{method} {path}: {e}"))?;
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            bail!("{method} {path} -> HTTP {status}: {v}")
        }
    }

    /// `POST` a raw NDJSON `_bulk` body (content-type `application/x-ndjson`)
    /// and require 2xx. The builder uses this to index the defs documents;
    /// `request_ok`'s `.json()` cannot send the newline-delimited bulk format.
    pub fn bulk_ndjson(&self, path: &str, ndjson: String) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-ndjson")
            .body(ndjson);
        if let Some(h) = &self.auth {
            req = req.header("Authorization", h);
        }
        let resp = req.send().with_context(|| format!("bulk {url}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if (200..300).contains(&status) {
            Ok(v)
        } else {
            bail!("bulk {path} -> HTTP {status}: {v}")
        }
    }

    /// Poll `GET /_cluster/health` until the node answers (any HTTP status that
    /// is not a transport error means it is up), up to `timeout`. Returns the
    /// last health body. The automatic node spawn (plan §5) will reuse this to
    /// wait for a node it started; here it lets `pull`/`build-pack` fail fast
    /// with a clear message when the node is not reachable.
    pub fn wait_until_reachable(&self, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let last_err = match self.send("GET", "/_cluster/health", None) {
                Ok((status, v)) if (200..500).contains(&status) => return Ok(v),
                Ok((status, v)) => format!("HTTP {status}: {v}"),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                bail!("node at {} not reachable: {last_err}", self.base);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl XcHttp for HubNode {
    fn get_mapping(&self, path: &str) -> Result<Value, String> {
        match self.send("GET", path, None) {
            Ok((status, v)) if (200..300).contains(&status) => Ok(v),
            Ok((404, _)) => Err("__404__".to_string()),
            Ok((status, v)) => Err(format!("HTTP {status}: {v}")),
            Err(e) => Err(e),
        }
    }
    fn cat_indices_json(&self, pattern: &str) -> Result<Vec<String>, String> {
        let path = format!("/_cat/indices/{pattern}?format=json&h=index");
        match self.send("GET", &path, None) {
            // A wildcard matching no index is "none", not an error.
            Ok((404, _)) => Ok(Vec::new()),
            Ok((status, v)) if (200..300).contains(&status) => v
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r.get("index").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .ok_or_else(|| "non-array _cat reply".to_string()),
            Ok((status, v)) => Err(format!("HTTP {status}: {v}")),
            Err(e) => Err(e),
        }
    }
    fn search(&self, index: &str, body: &Value) -> Result<Value, String> {
        match self.send("POST", &format!("/{index}/_search"), Some(body)) {
            Ok((status, v)) if (200..300).contains(&status) => Ok(v),
            Ok((404, _)) => Err("__404__".to_string()),
            Ok((status, v)) => Err(format!("HTTP {status}: {v}")),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `resolve_url` mutates process env, which is global; serialise the env
    // cases so they do not race other tests in the crate.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn url_resolution_prefers_flag_then_env_then_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Flag wins over everything.
        std::env::set_var("XERJ_URL", "http://env:1");
        assert_eq!(resolve_url(Some("http://flag:1")), "http://flag:1");
        // Env is used when no flag.
        assert_eq!(resolve_url(None), "http://env:1");
        // Default when neither.
        std::env::remove_var("XERJ_URL");
        assert_eq!(resolve_url(None), "http://localhost:9200");
        // An empty XERJ_URL is ignored (falls through to the default).
        std::env::set_var("XERJ_URL", "");
        assert_eq!(resolve_url(None), "http://localhost:9200");
        std::env::remove_var("XERJ_URL");
    }

    #[test]
    fn connect_trims_trailing_slash_and_sets_no_auth_without_a_key() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("XERJ_API_KEY");
        // A non-loopback URL never reaches for a local admin key, so no auth
        // header is set when none was given.
        let n = HubNode::connect("http://example.invalid:9200/").unwrap();
        assert_eq!(n.url(), "http://example.invalid:9200");
        assert!(n.auth.is_none());
    }

    #[test]
    fn explicit_key_becomes_an_apikey_header() {
        let n = HubNode::connect_with_key("http://example.invalid:9200", Some("sekret".into()))
            .unwrap();
        assert_eq!(n.auth.as_deref(), Some("ApiKey sekret"));
    }
}

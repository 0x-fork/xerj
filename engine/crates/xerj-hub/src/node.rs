//! Corpus Hub v2 — node resolution, the automatic local node, and a blocking
//! HTTP client to a xerj node (plan §2 "Node for the client", §5 "Node").
//!
//! `build-pack`, `pull`, `def` and `ask` all talk to a running node: the builder
//! snapshots into one, `pull` restores into one, and `def`/`ask` query one. This
//! module resolves WHICH node ([`resolve_url`], the same `--url` / `XERJ_URL` /
//! `localhost:9200` order `xc.rs` uses), gives a small blocking client
//! ([`HubNode`]) over it, and — plan §5 — starts a node of its own when none
//! answers ([`ensure_node`]) and stops exactly that node ([`stop_node`]).
//!
//! The automatic node follows four rules, each of them a line in the plan or in
//! its "do not" list:
//!
//! 1. **Never start a node on a port that answers.** The resolved URL is probed
//!    first; only a silent loopback URL leads to a spawn, and the port is chosen
//!    from ones that are bindable *now* (`xerj --port P` also claims `P+1` and
//!    `P+2`, so all three are checked).
//! 2. **Record what was started.** The PID goes to `<hub>/node.pid` the moment
//!    the process exists and the port to `<hub>/node.port` once it is healthy.
//! 3. **Stop only what was recorded.** [`stop_node`] signals the recorded PID and
//!    nothing else, and first checks that the process still looks like the node
//!    this tool started (its command line names `<hub>/node` as its data dir) —
//!    so a recycled PID, or a second process that merely shares the binary's
//!    name, is never signalled.
//! 4. **One starter at a time.** A lock directory serialises concurrent
//!    `xerj ask` runs so two of them cannot both launch a node on one data dir.
//!
//! [`HubNode`] implements [`XcHttp`] so `def::lookup` runs over it unchanged, and
//! it adds the extra verbs (`PUT`/`POST` with an arbitrary path, a readiness
//! poll) the builder and the pull path need.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

    /// Connect to a node this tool manages itself ([`ensure_node`] starts it
    /// with `--insecure`), sending no credential. Unlike [`HubNode::connect`] it
    /// never reaches for a locally discoverable admin key: that key belongs to
    /// some other node, and announcing it on every `xerj ask` would be noise.
    pub fn connect_insecure(url: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("build HTTP client")?;
        Ok(HubNode {
            base: url.trim_end_matches('/').to_string(),
            auth: None,
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
    /// last health body. It lets `build-pack` fail fast with a clear message
    /// when the node is not reachable. (The automatic node waits on its own
    /// stricter check, `wait_until_healthy`: HTTP 200 and a green/yellow
    /// status, and it stops early if the process it started has exited.)
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

// ── The automatic local node (plan §5 "Node") ────────────────────────────────

/// The first ES-compat port an automatic node may take. `xerj --port P` also
/// claims `P+1` (native REST) and `P+2` (gRPC), so a candidate needs all three.
pub const FIRST_AUTO_PORT: u16 = 9300;
/// Candidate base ports tried (stepping by 3) before giving up.
const AUTO_PORT_TRIES: u16 = 100;
/// How long a freshly started node gets to report a healthy cluster (plan §5:
/// "wait for green (max 60 s)").
pub const NODE_BOOT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a second `xerj ask` waits for the first one to finish starting the
/// node before it gives up (boot timeout plus slack).
const NODE_LOCK_WAIT: Duration = Duration::from_secs(90);
/// A start lock older than this was left by a crashed starter and is stolen.
const NODE_LOCK_STALE: Duration = Duration::from_secs(180);

const NODE_PID_FILE: &str = "node.pid";
const NODE_PORT_FILE: &str = "node.port";
const NODE_LOG_FILE: &str = "node.log";
const NODE_CONFIG_FILE: &str = "node.toml";
const NODE_DATA_DIR: &str = "node";
const NODE_LOCK_DIR: &str = "node.lock";

/// How to start "the xerj binary": the executable and any arguments that come
/// before the node's own flags. Production uses [`NodeLauncher::current`]
/// (`current_exe()`, no prefix); a test supplies its own pair so a cargo test
/// binary is never started with server flags it would reject.
#[derive(Debug, Clone)]
pub struct NodeLauncher {
    pub exe: PathBuf,
    pub pre_args: Vec<String>,
}

impl NodeLauncher {
    /// The running binary, i.e. the `xerj` the user invoked.
    pub fn current() -> Result<Self> {
        Ok(NodeLauncher {
            exe: std::env::current_exe().context("locate the xerj binary")?,
            pre_args: Vec::new(),
        })
    }

    /// A [`Command`] for the executable with the prefix arguments applied.
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.args(&self.pre_args);
        c
    }
}

/// The node `ask`/`pull` will talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNode {
    pub url: String,
    /// True when the node is the one this tool starts and records
    /// (`--insecure`, so no credential is sent); false for a user-supplied or
    /// default-URL node.
    pub managed: bool,
    /// True when THIS call started it (as opposed to finding it running).
    pub started_now: bool,
}

impl ResolvedNode {
    /// Connect with the right credential policy for this node.
    pub fn connect(&self) -> Result<HubNode> {
        if self.managed {
            HubNode::connect_insecure(&self.url)
        } else {
            HubNode::connect(&self.url)
        }
    }
}

/// A health probe's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub http_status: u16,
    /// `status` from the `/_cluster/health` body (`green`/`yellow`/`red`), when
    /// the body was JSON and carried one.
    pub status: Option<String>,
}

/// `GET {url}/_cluster/health` with a short timeout. `None` means nothing
/// answered (refused, timed out, not HTTP). ANY HTTP response counts as a live
/// node — a key-protected node answers 401 and must still not be started over.
pub fn probe(url: &str, timeout: Duration) -> Option<Health> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/_cluster/health", url.trim_end_matches('/')))
        .send()
        .ok()?;
    let http_status = resp.status().as_u16();
    let status = resp
        .text()
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_string));
    Some(Health {
        http_status,
        status,
    })
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The node data directory this tool starts a node on: `<hub>/node`.
pub fn node_data_dir(hub_home: &Path) -> PathBuf {
    hub_home.join(NODE_DATA_DIR)
}

/// Resolve the node to use, starting one if none answers (plan §5 "Node").
///
/// 1. `explicit` (`--url`) > `XERJ_URL` > `http://localhost:9200`. If that
///    answers, it is used — a node that answers is never started over.
/// 2. If it is silent and is NOT a loopback address, error: starting a local
///    node cannot satisfy a request for a remote one, and silently substituting
///    would hide the outage.
/// 3. Otherwise, a node this tool started earlier (`node.pid` + `node.port`) is
///    reused when it still answers.
/// 4. Otherwise start `exe --insecure --bind 127.0.0.1 --port <free ≥ 9300>
///    --data-dir <hub>/node --config <hub>/node.toml`, detached, record its PID,
///    and wait for a healthy cluster (≤ 60 s), then record the port.
///
/// `--bind 127.0.0.1` and `--config` are additions to the plan's command line:
/// the node runs with auth off, so the bind must not be steerable from the
/// environment (`XERJ_BIND_ADDRESS`); and the snapshot-repo allowlist in
/// `node.toml` is what lets the node accept `<hub>/packs/…` as a restore
/// location — without it the engine refuses any repository outside the node's
/// own data dir and `xerj hub pull` could never restore into this node.
pub fn ensure_node(
    hub_home: &Path,
    explicit: Option<&str>,
    launcher: &NodeLauncher,
) -> Result<ResolvedNode> {
    ensure_node_with(hub_home, explicit, launcher, NODE_BOOT_TIMEOUT)
}

/// [`ensure_node`] with the boot timeout spelled out (tests shorten it).
fn ensure_node_with(
    hub_home: &Path,
    explicit: Option<&str>,
    launcher: &NodeLauncher,
    boot_timeout: Duration,
) -> Result<ResolvedNode> {
    let url = resolve_url(explicit);
    if probe(&url, PROBE_TIMEOUT).is_some() {
        return Ok(ResolvedNode {
            url,
            managed: false,
            started_now: false,
        });
    }
    if !url_is_loopback(&url) {
        bail!(
            "no node answers at {url}; it is not a local address, so no local node was started \
             (pass --url for a running node, or unset XERJ_URL to use the automatic one)"
        );
    }
    if let Some(found) = recorded_node(hub_home) {
        return Ok(found);
    }

    // Start one — serialised, so concurrent callers cannot both launch a node
    // on the same data directory.
    let _lock = NodeLock::acquire(hub_home, NODE_LOCK_WAIT)?;
    if let Some(found) = recorded_node(hub_home) {
        // Another process finished starting it while we waited for the lock.
        return Ok(found);
    }
    start_node(hub_home, launcher, &url, boot_timeout)
}

/// The node this tool started earlier, if it is still running and answering.
/// Files left by a node that is gone (or whose PID now belongs to something
/// else) are removed here, so `xerj hub status` never reports a dead node.
fn recorded_node(hub_home: &Path) -> Option<ResolvedNode> {
    reap_stale(hub_home);
    let (_pid, port) = read_recorded(hub_home)?;
    let url = format!("http://127.0.0.1:{port}");
    probe(&url, PROBE_TIMEOUT)?;
    Some(ResolvedNode {
        url,
        managed: true,
        started_now: false,
    })
}

fn start_node(
    hub_home: &Path,
    launcher: &NodeLauncher,
    silent_url: &str,
    boot_timeout: Duration,
) -> Result<ResolvedNode> {
    let data_dir = node_data_dir(hub_home);
    fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let config = write_node_config(hub_home)?;
    let port = pick_free_port(FIRST_AUTO_PORT, AUTO_PORT_TRIES).ok_or_else(|| {
        anyhow!(
            "no free port for a local node: tried {} blocks of three from {FIRST_AUTO_PORT}",
            AUTO_PORT_TRIES
        )
    })?;
    let log = hub_home.join(NODE_LOG_FILE);

    eprintln!(
        "xerj hub: no node answers at {silent_url}; starting a local node on port {port} \
         (data: {}, log: {})",
        data_dir.display(),
        log.display()
    );
    let args = vec![
        "--insecure".to_string(),
        "--bind".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--data-dir".to_string(),
        data_dir.to_string_lossy().into_owned(),
        "--config".to_string(),
        config.to_string_lossy().into_owned(),
    ];
    let mut child = spawn_detached(launcher, &args, &log)
        .with_context(|| format!("start the local node ({})", launcher.exe.display()))?;
    let pid = child.id();
    // Recorded the moment the process exists: a crash during boot must still
    // leave a PID that `xerj hub stop` and `status` can see.
    write_file(&hub_home.join(NODE_PID_FILE), &format!("{pid}\n"))?;
    let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));

    let url = format!("http://127.0.0.1:{port}");
    match wait_until_healthy(&mut child, &url, boot_timeout, &log) {
        Ok(()) => {
            write_file(&hub_home.join(NODE_PORT_FILE), &format!("{port}\n"))?;
            eprintln!("xerj hub: node ready on port {port} (pid {pid})");
            Ok(ResolvedNode {
                url,
                managed: true,
                started_now: true,
            })
        }
        Err(e) => {
            // We started this process and recorded it, so cleaning it up is
            // stopping a PID we own — not a process we did not record.
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(hub_home.join(NODE_PID_FILE));
            let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
            Err(e)
        }
    }
}

/// Poll `/_cluster/health` until it is 200 with status green (yellow is
/// accepted too: a single node with an unassigned replica is usable), the child
/// exits, or `timeout` passes.
fn wait_until_healthy(child: &mut Child, url: &str, timeout: Duration, log: &Path) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("check the local node process")? {
            bail!(
                "the local node exited ({status}) before becoming healthy; its log ({}):\n{}",
                log.display(),
                log_tail(log, 20)
            );
        }
        if let Some(h) = probe(url, PROBE_TIMEOUT) {
            if h.http_status == 200 && matches!(h.status.as_deref(), Some("green" | "yellow")) {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "the local node did not report a healthy cluster within {}s; its log ({}):\n{}",
                timeout.as_secs(),
                log.display(),
                log_tail(log, 20)
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn log_tail(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            all[all.len().saturating_sub(lines)..].join("\n")
        }
        Err(e) => format!("(log unreadable: {e})"),
    }
}

/// Write `<hub>/node.toml`: the one setting the node needs beyond its flags — a
/// snapshot-repo allowlist naming `<hub>/packs`, where `xerj hub pull` extracts
/// the snapshot it asks the node to restore. Rewritten on every start.
fn write_node_config(hub_home: &Path) -> Result<PathBuf> {
    let packs = hub_home.join("packs");
    fs::create_dir_all(&packs).with_context(|| format!("create {}", packs.display()))?;
    let path = hub_home.join(NODE_CONFIG_FILE);
    // serde_json string escaping is valid TOML basic-string escaping for the
    // characters a path can contain (the same trick `xerj brain` uses).
    let packs_toml = serde_json::to_string(&packs.to_string_lossy())?;
    write_file(
        &path,
        &format!(
            "# written by `xerj ask` each time it starts the hub node\n\
             [limits]\n\
             snapshot_repo_allowlist = [{packs_toml}]\n"
        ),
    )?;
    Ok(path)
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

/// The first base port ≥ `start` (stepping by 3) whose three consecutive ports
/// are all bindable on loopback right now.
fn pick_free_port(start: u16, tries: u16) -> Option<u16> {
    let mut base = start;
    for _ in 0..tries {
        if base > u16::MAX - 2 {
            return None;
        }
        // Hold all three at once so the check is not three independent races.
        let held: Vec<_> = (0..3u16)
            .filter_map(|off| TcpListener::bind(("127.0.0.1", base + off)).ok())
            .collect();
        if held.len() == 3 {
            return Some(base);
        }
        base += 3;
    }
    None
}

/// Spawn `launcher` with `args`, detached: own process group (own session on
/// Windows), stdin closed, stdout and stderr appended to `log`. The child
/// outlives this process and never receives its Ctrl-C. Shared with the
/// background pull in `ask`.
pub(crate) fn spawn_detached(
    launcher: &NodeLauncher,
    args: &[String],
    log: &Path,
) -> Result<Child> {
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("open {}", log.display()))?;
    let err = out.try_clone()?;
    let mut cmd = launcher.command();
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    detach(&mut cmd);
    cmd.spawn()
        .with_context(|| format!("spawn {}", launcher.exe.display()))
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    cmd.creation_flags(0x0000_0008 | 0x0000_0200);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

/// A best-effort cross-process mutex for starting the node: an atomically
/// created `<hub>/node.lock` directory, removed on drop. A lock left by a
/// crashed starter is stolen once older than [`NODE_LOCK_STALE`].
struct NodeLock {
    path: PathBuf,
}

impl NodeLock {
    fn acquire(hub_home: &Path, wait: Duration) -> Result<NodeLock> {
        fs::create_dir_all(hub_home).with_context(|| format!("create {}", hub_home.display()))?;
        let path = hub_home.join(NODE_LOCK_DIR);
        let deadline = Instant::now() + wait;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(NodeLock { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age > NODE_LOCK_STALE);
                    if stale {
                        let _ = fs::remove_dir(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "another xerj process has been starting the hub node for over {}s \
                             (lock {}); retry, or remove the lock if no such process exists",
                            wait.as_secs(),
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e).with_context(|| format!("create lock {}", path.display())),
            }
        }
    }
}

impl Drop for NodeLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

// ── recorded PID / port ──────────────────────────────────────────────────────

/// `(pid, port)` from `<hub>/node.pid` and `<hub>/node.port`; `None` unless both
/// parse. (A pid file alone means the node is still booting or died booting.)
fn read_recorded(hub_home: &Path) -> Option<(u32, u16)> {
    let pid = read_pid(hub_home)?;
    let port = fs::read_to_string(hub_home.join(NODE_PORT_FILE))
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()?;
    Some((pid, port))
}

fn read_pid(hub_home: &Path) -> Option<u32> {
    fs::read_to_string(hub_home.join(NODE_PID_FILE))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// What a PID is, relative to the node this tool started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identity {
    /// Running, and its command line names this hub's node data directory.
    Ours,
    /// Running, but it is not the node we started (a recycled PID, a decoy).
    NotOurs,
    /// No such process (or only a zombie awaiting its parent's `wait`).
    Gone,
    /// Cannot tell on this platform / with these permissions. Never signalled.
    Unknown,
}

/// Identify `pid`: does its command line contain every needle? On Linux the
/// command line is `/proc/<pid>/cmdline`; elsewhere on unix it is `ps`; on
/// Windows only the image name is available, so the needle check is replaced by
/// "the image is a xerj binary".
pub(crate) fn identify(pid: u32, needles: &[&str]) -> Identity {
    match proc_state(pid) {
        ProcState::Gone | ProcState::Zombie => Identity::Gone,
        ProcState::Unknown => Identity::Unknown,
        ProcState::Running => {
            // A process that has only just exec'd is not reliably readable: for a
            // few milliseconds `/proc/<pid>/cmdline` is empty, shows the PARENT's
            // command line (the child still shares its address space), or is a
            // splice of both. Measured: reading it right after `spawn` returned
            // mis-identified a live, correct node as "not ours" in roughly one
            // run in ten under load. So a match is believed at once, but a
            // mismatch (or an empty read) must persist across several samples
            // before it counts.
            let mut saw_other = false;
            for attempt in 0..IDENTIFY_SAMPLES {
                match proc_cmdline(pid) {
                    Some(cmd) if identity_matches(&cmd, needles) => return Identity::Ours,
                    Some(_) => saw_other = true,
                    None => {}
                }
                if attempt + 1 < IDENTIFY_SAMPLES {
                    std::thread::sleep(IDENTIFY_GAP);
                }
            }
            if saw_other {
                Identity::NotOurs
            } else {
                Identity::Unknown
            }
        }
    }
}

/// How many times [`identify`] reads a running process's command line before
/// believing a mismatch, and the pause between reads (~175 ms in all, paid only
/// when the process is NOT the one being looked for).
const IDENTIFY_SAMPLES: u32 = 8;
const IDENTIFY_GAP: Duration = Duration::from_millis(25);

#[cfg(not(windows))]
fn identity_matches(cmdline: &str, needles: &[&str]) -> bool {
    needles.iter().all(|n| cmdline.contains(n))
}

#[cfg(windows)]
fn identity_matches(image: &str, _needles: &[&str]) -> bool {
    image.to_ascii_lowercase().contains("xerj")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // `tasklist` has no zombie state
enum ProcState {
    Running,
    Zombie,
    Gone,
    Unknown,
}

#[cfg(target_os = "linux")]
fn proc_state(pid: u32) -> ProcState {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `pid (comm) S …` — comm may contain spaces and parens, so the state
        // is the first character after the LAST `)`.
        Ok(s) => match s
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
        {
            Some('Z' | 'X') => ProcState::Zombie,
            Some(_) => ProcState::Running,
            None => ProcState::Unknown,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProcState::Gone,
        Err(_) => ProcState::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn proc_cmdline(pid: u32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(|b| *b == 0)
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn proc_state(pid: u32) -> ProcState {
    match Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(out) if out.status.success() => {
            let stat = String::from_utf8_lossy(&out.stdout);
            match stat.trim().chars().next() {
                Some('Z') => ProcState::Zombie,
                Some(_) => ProcState::Running,
                None => ProcState::Gone,
            }
        }
        // `ps -p` exits non-zero when the PID does not exist.
        Ok(_) => ProcState::Gone,
        Err(_) => ProcState::Unknown,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn proc_cmdline(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(windows)]
fn tasklist_row(pid: u32) -> Result<Option<String>, ()> {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .map_err(|_| ())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(text
        .lines()
        .find(|l| l.starts_with('"'))
        .map(str::to_string))
}

#[cfg(windows)]
fn proc_state(pid: u32) -> ProcState {
    match tasklist_row(pid) {
        Ok(Some(_)) => ProcState::Running,
        Ok(None) => ProcState::Gone,
        Err(()) => ProcState::Unknown,
    }
}

#[cfg(windows)]
fn proc_cmdline(pid: u32) -> Option<String> {
    tasklist_row(pid).ok().flatten()
}

/// The needle that makes a PID "the hub node we started": its command line
/// carries `--data-dir <hub>/node` (the two arguments adjacent), which only the
/// node this tool launched has (on Windows only the image name is checked — see
/// [`identify`]).
fn node_needles(hub_home: &Path) -> Vec<String> {
    vec![format!(
        "--data-dir {}",
        node_data_dir(hub_home).to_string_lossy()
    )]
}

fn identify_node(hub_home: &Path, pid: u32) -> Identity {
    let needles = node_needles(hub_home);
    let refs: Vec<&str> = needles.iter().map(String::as_str).collect();
    identify(pid, &refs)
}

/// Remove `node.pid` / `node.port` when the recorded process is gone or is no
/// longer the node we started. An `Unknown` identity leaves the files alone.
pub fn reap_stale(hub_home: &Path) {
    let Some(pid) = read_pid(hub_home) else {
        // No parseable PID: any port file is an orphan.
        if hub_home.join(NODE_PORT_FILE).exists() {
            let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
        }
        return;
    };
    if matches!(
        identify_node(hub_home, pid),
        Identity::Gone | Identity::NotOurs
    ) {
        let _ = fs::remove_file(hub_home.join(NODE_PID_FILE));
        let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
    }
}

// ── stopping ─────────────────────────────────────────────────────────────────

/// What [`stop_node`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// No `node.pid` — nothing was started by this tool.
    NothingRecorded,
    /// The recorded process was already gone (or its PID now belongs to another
    /// program); the stale record was removed and nothing was signalled.
    StaleRemoved { pid: u32 },
    /// The recorded node was stopped.
    Stopped { pid: u32 },
}

/// Stop the node this tool started: ONLY the PID in `<hub>/node.pid`, and only
/// after confirming the process is still that node. Never matches by name,
/// never signals a process group, never touches a node it did not record
/// (plan §11).
pub fn stop_node(hub_home: &Path) -> Result<StopOutcome> {
    let Some(pid) = read_pid(hub_home) else {
        let _ = fs::remove_file(hub_home.join(NODE_PID_FILE));
        let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
        return Ok(StopOutcome::NothingRecorded);
    };
    match identify_node(hub_home, pid) {
        Identity::Gone | Identity::NotOurs => {
            let _ = fs::remove_file(hub_home.join(NODE_PID_FILE));
            let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
            Ok(StopOutcome::StaleRemoved { pid })
        }
        Identity::Unknown => bail!(
            "cannot confirm that pid {pid} is the hub node (this platform or its permissions \
             do not expose the process command line); not signalling it. If it is the node, \
             stop it yourself and delete {}",
            hub_home.join(NODE_PID_FILE).display()
        ),
        Identity::Ours => {
            terminate(pid)?;
            // Not "gone" yet? Wait, then escalate once — to the same PID.
            let gone = wait_gone(hub_home, pid, Duration::from_secs(10));
            if !gone {
                kill_hard(pid)?;
                if !wait_gone(hub_home, pid, Duration::from_secs(3)) {
                    bail!("pid {pid} (the hub node) did not exit after SIGTERM and SIGKILL");
                }
            }
            let _ = fs::remove_file(hub_home.join(NODE_PID_FILE));
            let _ = fs::remove_file(hub_home.join(NODE_PORT_FILE));
            Ok(StopOutcome::Stopped { pid })
        }
    }
}

fn wait_gone(hub_home: &Path, pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(identify_node(hub_home, pid), Identity::Gone) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn terminate(pid: u32) -> Result<()> {
    send_signal(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn kill_hard(pid: u32) -> Result<()> {
    send_signal(pid, libc::SIGKILL)
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: libc::c_int) -> Result<()> {
    // A single PID — never `-pid` (a whole process group).
    // SAFETY: plain syscall with integer arguments; `pid` was read from the
    // hub's own pid file and identity-checked immediately before.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(()) // exited between the identity check and the signal
        } else {
            Err(err).with_context(|| format!("signal pid {pid}"))
        }
    }
}

#[cfg(windows)]
fn terminate(pid: u32) -> Result<()> {
    taskkill(pid)
}

#[cfg(windows)]
fn kill_hard(pid: u32) -> Result<()> {
    taskkill(pid)
}

#[cfg(windows)]
fn taskkill(pid: u32) -> Result<()> {
    // `/F` without `/T`: exactly this PID, not its children.
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run taskkill")?;
    if status.success() {
        Ok(())
    } else {
        bail!("taskkill /PID {pid} failed ({status})")
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate(_pid: u32) -> Result<()> {
    bail!("stopping a process is not supported on this platform")
}

#[cfg(not(any(unix, windows)))]
fn kill_hard(_pid: u32) -> Result<()> {
    bail!("stopping a process is not supported on this platform")
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

    // ── the automatic node ──────────────────────────────────────────────────

    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A stand-in node: answers every request on an ephemeral loopback port with
    /// `status` and the given JSON body. The thread lives until the test binary
    /// exits.
    fn fake_node(status: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut buf = [0u8; 2048];
                let _ = conn.read(&mut buf);
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            }
        });
        port
    }

    /// A launcher that must never be used.
    fn forbidden_launcher() -> NodeLauncher {
        NodeLauncher {
            exe: PathBuf::from("/nonexistent/xerj-must-not-be-spawned"),
            pre_args: vec![],
        }
    }

    #[test]
    fn a_node_that_answers_is_used_and_nothing_is_started() {
        let hub = tempfile::tempdir().unwrap();
        let port = fake_node("200 OK", r#"{"status":"green"}"#);
        let url = format!("http://127.0.0.1:{port}");
        let r = ensure_node(hub.path(), Some(&url), &forbidden_launcher()).unwrap();
        assert_eq!(r.url, url);
        assert!(!r.managed && !r.started_now);
        assert!(!hub.path().join("node.pid").exists());
    }

    #[test]
    fn a_key_protected_node_that_answers_401_still_counts_as_running() {
        let hub = tempfile::tempdir().unwrap();
        let port = fake_node("401 Unauthorized", r#"{"error":"auth"}"#);
        let url = format!("http://127.0.0.1:{port}");
        let r = ensure_node(hub.path(), Some(&url), &forbidden_launcher()).unwrap();
        assert_eq!(r.url, url, "never started over");
        let h = probe(&url, Duration::from_secs(2)).unwrap();
        assert_eq!(h.http_status, 401);
    }

    #[test]
    fn probe_reads_the_cluster_status_and_reports_silence_as_none() {
        let port = fake_node("200 OK", r#"{"status":"yellow","number_of_nodes":1}"#);
        let h = probe(&format!("http://127.0.0.1:{port}/"), Duration::from_secs(2)).unwrap();
        assert_eq!(h.http_status, 200);
        assert_eq!(h.status.as_deref(), Some("yellow"));
        // A port nothing listens on: bind then drop to find one.
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(probe(
            &format!("http://127.0.0.1:{closed}"),
            Duration::from_secs(1)
        )
        .is_none());
    }

    #[test]
    fn a_silent_remote_url_is_an_error_not_a_local_node() {
        let hub = tempfile::tempdir().unwrap();
        let err = ensure_node(
            hub.path(),
            Some("http://nonexistent.invalid:9200"),
            &forbidden_launcher(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a local address"), "{err}");
        assert!(!hub.path().join("node.pid").exists());
    }

    #[test]
    fn free_port_search_skips_a_busy_triple() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = held.local_addr().unwrap().port();
        let found = pick_free_port(busy, 50).expect("some free triple above");
        // Candidates step by three from the start, so a busy start is passed over.
        assert!(found > busy, "the busy base port is skipped");
        assert_eq!((found - busy) % 3, 0);
        // The search is bounded by the top of the port range and by `tries`.
        assert!(
            pick_free_port(u16::MAX - 1, 5).is_none(),
            "no room above the top"
        );
        assert!(pick_free_port(busy, 0).is_none(), "no tries, no port");
        // The default floor is the plan's: never 9200, never 9297-9299.
        assert_eq!(FIRST_AUTO_PORT, 9300);
    }

    #[test]
    fn the_node_config_is_accepted_by_the_real_config_loader_and_allows_the_packs_dir() {
        let hub = tempfile::tempdir().unwrap();
        let path = write_node_config(hub.path()).unwrap();
        let cfg = xerj_common::config::Config::load(&path).expect("node.toml loads");
        let packs = hub.path().join("packs");
        assert_eq!(
            cfg.limits.snapshot_repo_allowlist,
            [packs.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn the_start_lock_serialises_starters_and_steals_a_stale_one() {
        let hub = tempfile::tempdir().unwrap();
        let first = NodeLock::acquire(hub.path(), Duration::from_secs(1)).unwrap();
        let err = NodeLock::acquire(hub.path(), Duration::from_millis(300))
            .err()
            .expect("held lock times out")
            .to_string();
        assert!(err.contains("another xerj process"), "{err}");
        drop(first);
        let again = NodeLock::acquire(hub.path(), Duration::from_secs(1)).unwrap();
        drop(again);
        assert!(!hub.path().join(NODE_LOCK_DIR).exists());
    }

    #[test]
    fn connect_insecure_sends_no_credential() {
        let n = HubNode::connect_insecure("http://127.0.0.1:1/").unwrap();
        assert_eq!(n.url(), "http://127.0.0.1:1");
        assert!(n.auth.is_none());
    }

    // The process-level tests drive real child processes through `sh`, so they
    // are unix-only; the Windows paths are compiled but not exercised here.
    #[cfg(unix)]
    mod processes {
        use super::*;
        use std::process::Child;

        /// A long-lived `sh` whose command line carries `--data-dir <dir>` — what
        /// `identify_node` looks for. The trailing `; true` keeps `sh` itself
        /// alive (a lone command may be exec'd, replacing the command line).
        fn spawn_node_like(dir: &Path) -> Child {
            Command::new("sh")
                .args(["-c", "sleep 30; true", "node", "--data-dir"])
                .arg(dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }

        /// A process of the same program but a different command line — the
        /// "dummy process named like the binary" that must survive.
        fn spawn_decoy() -> Child {
            Command::new("sh")
                .args([
                    "-c",
                    "sleep 30; true",
                    "decoy",
                    "--data-dir",
                    "/elsewhere/node",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }

        fn alive(child: &mut Child) -> bool {
            child.try_wait().unwrap().is_none()
        }

        fn record(hub: &Path, pid: u32, port: Option<u16>) {
            fs::write(hub.join(NODE_PID_FILE), format!("{pid}\n")).unwrap();
            if let Some(p) = port {
                fs::write(hub.join(NODE_PORT_FILE), format!("{p}\n")).unwrap();
            }
        }

        #[test]
        fn stop_signals_exactly_the_recorded_pid_and_spares_a_look_alike() {
            let hub = tempfile::tempdir().unwrap();
            let dir = node_data_dir(hub.path());
            let mut node = spawn_node_like(&dir);
            let mut decoy = spawn_decoy();
            record(hub.path(), node.id(), Some(9301));
            assert_eq!(identify_node(hub.path(), node.id()), Identity::Ours);
            assert_eq!(identify_node(hub.path(), decoy.id()), Identity::NotOurs);

            let outcome = stop_node(hub.path()).unwrap();
            assert_eq!(outcome, StopOutcome::Stopped { pid: node.id() });
            // The recorded process is dead (reap it) ...
            let status = node.wait().unwrap();
            assert!(!status.success(), "terminated by a signal: {status:?}");
            // ... the look-alike is untouched, and the records are gone.
            assert!(alive(&mut decoy), "the decoy must survive `hub stop`");
            assert!(!hub.path().join(NODE_PID_FILE).exists());
            assert!(!hub.path().join(NODE_PORT_FILE).exists());
            decoy.kill().unwrap();
            decoy.wait().unwrap();
        }

        #[test]
        fn a_recycled_pid_is_never_signalled() {
            let hub = tempfile::tempdir().unwrap();
            // The record names a live process that is NOT this hub's node.
            let mut stranger = spawn_decoy();
            record(hub.path(), stranger.id(), Some(9301));
            let outcome = stop_node(hub.path()).unwrap();
            assert_eq!(outcome, StopOutcome::StaleRemoved { pid: stranger.id() });
            assert!(
                alive(&mut stranger),
                "an unrelated process must not be killed"
            );
            assert!(
                !hub.path().join(NODE_PID_FILE).exists(),
                "the stale record goes"
            );
            stranger.kill().unwrap();
            stranger.wait().unwrap();
        }

        #[test]
        fn stop_with_nothing_recorded_or_a_dead_pid_is_a_clean_no_op() {
            let hub = tempfile::tempdir().unwrap();
            assert_eq!(stop_node(hub.path()).unwrap(), StopOutcome::NothingRecorded);

            let mut gone = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
            let pid = gone.id();
            gone.wait().unwrap();
            record(hub.path(), pid, Some(9301));
            assert_eq!(identify_node(hub.path(), pid), Identity::Gone);
            assert_eq!(
                stop_node(hub.path()).unwrap(),
                StopOutcome::StaleRemoved { pid }
            );
            assert!(!hub.path().join(NODE_PID_FILE).exists());
        }

        #[test]
        fn a_zombie_counts_as_gone() {
            let hub = tempfile::tempdir().unwrap();
            // Exits at once; not `wait`ed, so it stays a zombie until reaped.
            let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(identify_node(hub.path(), child.id()), Identity::Gone);
            child.wait().unwrap();
        }

        #[test]
        fn identify_matches_every_needle_in_the_command_line() {
            let me = std::process::id();
            assert_eq!(identify(me, &["xerj_hub"]), Identity::Ours);
            assert_eq!(
                identify(me, &["xerj_hub", "no-such-argument"]),
                Identity::NotOurs
            );
        }

        #[test]
        fn status_forgets_a_dead_node_but_keeps_a_live_one() {
            let hub = tempfile::tempdir().unwrap();
            let mut dead = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
            let dead_pid = dead.id();
            dead.wait().unwrap();
            record(hub.path(), dead_pid, Some(9301));
            reap_stale(hub.path());
            assert!(!hub.path().join(NODE_PID_FILE).exists());
            assert!(!hub.path().join(NODE_PORT_FILE).exists());

            let mut live = spawn_node_like(&node_data_dir(hub.path()));
            record(hub.path(), live.id(), Some(9301));
            reap_stale(hub.path());
            assert!(hub.path().join(NODE_PID_FILE).exists());
            live.kill().unwrap();
            live.wait().unwrap();
        }

        #[test]
        fn a_recorded_live_node_that_answers_is_reused_not_restarted() {
            let hub = tempfile::tempdir().unwrap();
            let mut live = spawn_node_like(&node_data_dir(hub.path()));
            let port = fake_node("200 OK", r#"{"status":"green"}"#);
            record(hub.path(), live.id(), Some(port));
            // The default-ish URL is silent (a closed loopback port), so the
            // recorded node is the one used.
            let silent = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let r = ensure_node(
                hub.path(),
                Some(&format!("http://127.0.0.1:{silent}")),
                &forbidden_launcher(),
            )
            .unwrap();
            assert_eq!(r.url, format!("http://127.0.0.1:{port}"));
            assert!(r.managed && !r.started_now);
            live.kill().unwrap();
            live.wait().unwrap();
        }

        #[test]
        fn a_node_that_dies_while_booting_leaves_no_record_and_shows_its_log() {
            let hub = tempfile::tempdir().unwrap();
            let silent = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            // `sh -c 'echo boom; exit 3' sh <node flags…>` — exits straight away.
            let launcher = NodeLauncher {
                exe: PathBuf::from("sh"),
                pre_args: vec!["-c".into(), "echo boom-in-log; exit 3".into(), "sh".into()],
            };
            let err = ensure_node_with(
                hub.path(),
                Some(&format!("http://127.0.0.1:{silent}")),
                &launcher,
                Duration::from_secs(10),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("exited"), "{err}");
            assert!(
                err.contains("boom-in-log"),
                "the node's log is shown: {err}"
            );
            assert!(!hub.path().join(NODE_PID_FILE).exists());
            assert!(!hub.path().join(NODE_PORT_FILE).exists());
            assert!(
                !hub.path().join(NODE_LOCK_DIR).exists(),
                "the lock is released"
            );
        }

        #[test]
        fn a_node_that_never_gets_healthy_is_killed_and_forgotten_at_the_timeout() {
            let hub = tempfile::tempdir().unwrap();
            let silent = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let launcher = NodeLauncher {
                exe: PathBuf::from("sh"),
                pre_args: vec!["-c".into(), "sleep 30; true".into(), "sh".into()],
            };
            let t0 = Instant::now();
            let err = ensure_node_with(
                hub.path(),
                Some(&format!("http://127.0.0.1:{silent}")),
                &launcher,
                Duration::from_millis(800),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("did not report a healthy cluster"), "{err}");
            assert!(t0.elapsed() < Duration::from_secs(10));
            assert!(!hub.path().join(NODE_PID_FILE).exists());
            assert!(!hub.path().join(NODE_PORT_FILE).exists());
        }

        #[test]
        fn the_node_is_started_with_the_plans_flags_plus_loopback_bind_and_config() {
            let hub = tempfile::tempdir().unwrap();
            let silent = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            // `printf '%s\n' "$@"` writes the node's argv to its log, then fails
            // fast so the test does not wait on a healthy cluster.
            let launcher = NodeLauncher {
                exe: PathBuf::from("sh"),
                pre_args: vec![
                    "-c".into(),
                    "printf '%s\\n' \"$@\"; exit 4".into(),
                    "sh".into(),
                ],
            };
            let _ = ensure_node_with(
                hub.path(),
                Some(&format!("http://127.0.0.1:{silent}")),
                &launcher,
                Duration::from_secs(10),
            );
            let log = fs::read_to_string(hub.path().join(NODE_LOG_FILE)).unwrap();
            let argv: Vec<&str> = log.lines().collect();
            assert_eq!(argv[0], "--insecure");
            assert_eq!(&argv[1..3], ["--bind", "127.0.0.1"]);
            assert_eq!(argv[3], "--port");
            let port: u16 = argv[4].parse().unwrap();
            assert!(
                port >= FIRST_AUTO_PORT,
                "port {port} is below the floor (never 9200, never 9297-9299)"
            );
            assert_eq!(argv[5], "--data-dir");
            assert_eq!(argv[6], node_data_dir(hub.path()).to_string_lossy());
            assert_eq!(argv[7], "--config");
            assert_eq!(argv[8], hub.path().join("node.toml").to_string_lossy());
        }
    }
}

//! Corpus Hub v2 — the `xerj hub …` subcommands and the `--def` code path
//! (plan §2 builder + client, §3 `def`).
//!
//! Three entry points, all returning a process exit code:
//!
//! - [`run_hub_cli`] dispatches `xerj hub build-pack` (CI builds a signed,
//!   pre-indexed pack) and `xerj hub pull` (a client downloads, verifies and
//!   restores one). Future hub subcommands (status, sync, …) are plan §5/§8.
//! - [`run_code`] is the `xerj code` front door: `--def <SYMBOL>` answers from
//!   the corpus's `-defs` index via [`crate::def::lookup`] and the shared
//!   renderer; everything else delegates unchanged to the existing
//!   `xerj_autoindex::xc::run_code_cli`.
//!
//! The crate graph forbids `run_code_query` (in `xerj-common`) from calling
//! `def::lookup` (here, above `xerj-autoindex`), so the plan's "`run_code_query`
//! calls `lookup`" is realised one layer up, in `run_code`, which sees both the
//! code CLI and `def::lookup` without a dependency cycle. The user-facing
//! contract is identical: `xerj code <slug> --def <SYMBOL>` prints a def answer
//! through the §1 renderer.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use xerj_common::hub::{
    render, Answer, LicenceClass, PackIndex, PackManifest, PackSource, DEFAULT_HUB_URL,
    HUB_PUBKEY_HEX, PACK_FORMAT_VERSION,
};
use xerj_common::xccode::manifest::read_hub_manifest;
use xerj_common::xccode::state;
use xerj_common::xccode::XcHttp;

use crate::catalogue::{build_catalogue, load_catalogue, CorpusBuildInput};
use crate::node::{resolve_url, HubNode};
use crate::pack;

const HUB_USAGE: &str = "usage: xerj hub <command>

  build-pack <slug> --registry <dir> --out <dir> --url <node> --sign-key <seed-file>
        Build a signed, pre-indexed pack from a registry manifest (CI).
  pull <slug> [--quiet] [--url <node>]
        Download, verify and restore a pack, then `xerj code <slug>` works.

Packs are pulled on demand and never cloned or indexed on the user's machine
(docs/CORPUS_HUB_V2_IMPLEMENTATION.md).";

/// `xerj hub <command> …`.
pub fn run_hub_cli(args: &[String]) -> i32 {
    let Some(sub) = args.first() else {
        eprintln!("{HUB_USAGE}");
        return 2;
    };
    let rest = &args[1..];
    match sub.as_str() {
        "build-pack" => run_build_pack(rest),
        "pull" => run_pull(rest),
        "-h" | "--help" | "help" => {
            println!("{HUB_USAGE}");
            0
        }
        other => {
            eprintln!("xerj hub: unknown command '{other}'\n\n{HUB_USAGE}");
            2
        }
    }
}

// ── xerj hub build-pack ──────────────────────────────────────────────────────

fn run_build_pack(args: &[String]) -> i32 {
    let mut slug: Option<String> = None;
    let mut registry: Option<String> = None;
    let mut out: Option<String> = None;
    let mut url: Option<String> = None;
    let mut sign_key: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        macro_rules! val {
            ($n:expr) => {
                match args.get(i + 1) {
                    Some(v) => {
                        i += 1;
                        v.clone()
                    }
                    None => {
                        eprintln!("xerj hub build-pack: {} needs a value\n\n{HUB_USAGE}", $n);
                        return 2;
                    }
                }
            };
        }
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{HUB_USAGE}");
                return 0;
            }
            "--registry" => registry = Some(val!("--registry")),
            "--out" => out = Some(val!("--out")),
            "--url" => url = Some(val!("--url")),
            "--sign-key" => sign_key = Some(val!("--sign-key")),
            a if a.starts_with('-') => {
                eprintln!("xerj hub build-pack: unknown flag '{a}'\n\n{HUB_USAGE}");
                return 2;
            }
            a => {
                if slug.is_none() {
                    slug = Some(a.to_string());
                } else {
                    eprintln!("xerj hub build-pack: one <slug>, not two\n\n{HUB_USAGE}");
                    return 2;
                }
            }
        }
        i += 1;
    }
    let (Some(slug), Some(registry), Some(out), Some(sign_key)) = (slug, registry, out, sign_key)
    else {
        eprintln!(
            "xerj hub build-pack: <slug>, --registry, --out and --sign-key are required\n\n{HUB_USAGE}"
        );
        return 2;
    };
    let url = resolve_url(url.as_deref());
    match build_pack(&slug, &registry, &out, &url, &sign_key) {
        Ok(summary) => {
            println!("{summary}");
            0
        }
        Err(e) => {
            eprintln!("xerj hub build-pack: {e:#}");
            1
        }
    }
}

/// Build the pack and return the one-line summary `pack <slug> <pin8> <bytes>
/// <indices> <defs>` (plan §2 step 6).
fn build_pack(
    slug: &str,
    registry: &str,
    out: &str,
    url: &str,
    sign_key_file: &str,
) -> Result<String> {
    let seed = fs::read_to_string(sign_key_file)
        .with_context(|| format!("read sign key {sign_key_file}"))?
        .trim()
        .to_string();
    let manifest_path = Path::new(registry).join(format!("{slug}.json"));
    if !manifest_path.is_file() {
        bail!("no registry manifest at {}", manifest_path.display());
    }

    // 1. Licence, pin and sources — from the EXISTING parsers (no second parser
    //    for the registry manifest, plan §11): `read_hub_manifest` for the
    //    per-repo sources, and the catalogue builder for the restrictive-first
    //    licence class + pin date it already computes.
    let meta = resolve_pack_metadata(&manifest_path, &seed)
        .with_context(|| format!("read licence/pin from {}", manifest_path.display()))?;
    let pin8 = pin8_of(&meta.pin_sha)?;

    // 2. Clone + index into the builder node (the SAME lifecycle the CLI runs).
    let rc = xerj_autoindex::xc::run_corpus_cli(&[
        "add".into(),
        slug.into(),
        "--from".into(),
        manifest_path.to_string_lossy().into_owned(),
    ]);
    if rc != 0 {
        bail!("`corpus add` failed (exit {rc})");
    }
    let rc = xerj_autoindex::xc::run_corpus_cli(&[
        "index".into(),
        slug.into(),
        "--url".into(),
        url.into(),
    ]);
    if rc != 0 {
        bail!("`corpus index` failed (exit {rc})");
    }

    let root = xerj_autoindex::xc::code_root();
    let st = state::load_state(&root, slug).map_err(|e| anyhow!("{e}"))?;
    let index_prefix = state::query_prefix(&st);

    // 3. Defs + file inventory from the cloned tree (plan §3 extraction).
    let corpus_root = root.join("corpora").join(slug);
    let licence_map = xerj_common::xccode::manifest::licence_map(&root, slug);
    let overall_licence = meta.licence_spdx.clone();
    let licence_of = move |rel: &str| {
        // The file's repo is its top-level directory under the corpus root.
        rel.split('/')
            .next()
            .and_then(|top| licence_map.get(top))
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| overall_licence.clone())
    };
    let (defs, files) = pack::build_defs_and_files(&corpus_root, &licence_of)
        .with_context(|| format!("extract defs from {}", corpus_root.display()))?;

    // 4. Build + index the `-defs` index (plan §3 mapping) on the builder node.
    let node = HubNode::connect_with_key(url, None)?;
    node.wait_until_reachable(Duration::from_secs(30))
        .context("builder node not reachable")?;
    let defs_index = format!("{index_prefix}-defs");
    create_defs_index(&node, &defs_index)?;
    index_defs(&node, &defs_index, &defs)?;
    node.request_ok("POST", &format!("/{defs_index}/_refresh"), None)
        .ok();

    // 5. Snapshot `<index_prefix>-*` (docs + defs) into <out>/<slug>/snapshot.
    fs::create_dir_all(out).with_context(|| format!("create {out}"))?;
    let out_canon = fs::canonicalize(out).with_context(|| format!("canonicalize {out}"))?;
    let snapshot_location = out_canon.join(slug).join("snapshot");
    let snapshot_name = format!("{slug}-{pin8}");
    register_repo(&node, "hub-build", &snapshot_location)?;
    node.request_ok(
        "PUT",
        &format!("/_snapshot/hub-build/{snapshot_name}?wait_for_completion=true"),
        Some(&json!({ "indices": format!("{index_prefix}-*") })),
    )
    .context("create snapshot")?;

    // 6. Assemble pack.json, write + sign the archive.
    let indices = pack_indices(&node, &index_prefix)?;
    let manifest = PackManifest {
        format_version: PACK_FORMAT_VERSION,
        slug: slug.to_string(),
        pin_sha: meta.pin_sha.clone(),
        pin_date: meta.pin_date.clone(),
        built_at: now_rfc3339(),
        builder_version: format!("xerj {}", env!("CARGO_PKG_VERSION")),
        index_prefix: index_prefix.clone(),
        indices,
        snapshot_name: snapshot_name.clone(),
        licence_spdx: meta.licence_spdx.clone(),
        licence_class: meta.licence_class,
        sources: meta.sources.clone(),
        defs_count: defs.len() as u64,
        files_count: files.len() as u64,
    };
    let n_indices = manifest.indices.len();
    let out_archive = out_canon.join(format!("{slug}-{pin8}.hubpack.tar.zst"));
    let (_sha, bytes) = pack::write_pack(
        &out_archive,
        &manifest,
        &defs,
        &files,
        &snapshot_location,
        &seed,
    )
    .context("write pack archive")?;

    Ok(format!(
        "pack {slug} {pin8} {bytes} {n_indices} {}",
        defs.len()
    ))
}

/// Pack licence/pin/sources, resolved from the registry manifest via existing
/// parsers only.
struct PackMetadata {
    pin_sha: String,
    pin_date: String,
    licence_spdx: String,
    licence_class: LicenceClass,
    sources: Vec<PackSource>,
}

/// Read the pack's licence class + pin date through the catalogue builder (it
/// already applies the restrictive-first rule over the manifest's review
/// blocks) and the per-repo sources through `read_hub_manifest` — both existing
/// parsers, so the registry manifest is never parsed by a second reader
/// (plan §11). `seed` is only used because `build_catalogue` signs its output;
/// the signature is discarded.
fn resolve_pack_metadata(manifest_path: &Path, seed: &str) -> Result<PackMetadata> {
    let input = CorpusBuildInput {
        manifest_path: manifest_path.to_path_buf(),
        domain: String::new(),
        refresh_cadence_days: 0,
        pack_bytes: 0,
        pack_url: String::new(),
        pack_sha256: String::new(),
        sig_url: String::new(),
        quality_grade: None,
        terms: Vec::new(),
        pin_sha: String::new(),
        ask_for: String::new(),
    };
    let (cat, _sig) = build_catalogue(&[input], "", seed)?;
    let entry = cat
        .corpora
        .first()
        .ok_or_else(|| anyhow!("catalogue builder produced no entry"))?;

    let hub = read_hub_manifest(manifest_path).map_err(|e| anyhow!("{e}"))?;
    let sources = hub
        .rows
        .iter()
        .map(|r| PackSource {
            repo: r.repo.clone(),
            url: r.url.clone(),
            sha: r.sha.clone(),
            licence: r.declared_licence.clone(),
        })
        .collect();

    Ok(PackMetadata {
        pin_sha: entry.pin_sha.clone(),
        pin_date: entry.pin_date.clone(),
        licence_spdx: entry.licence_spdx.clone(),
        licence_class: entry.licence_class,
        sources,
    })
}

/// `PUT /<index>` with the plan's §3 defs mapping.
fn create_defs_index(node: &HubNode, index: &str) -> Result<()> {
    let body = json!({
        "mappings": { "properties": {
            "symbol":      { "type": "keyword" },
            "symbol_text": { "type": "text" },
            "kind":        { "type": "keyword" },
            "lang":        { "type": "keyword" },
            "file":        { "type": "keyword" },
            "line":        { "type": "integer" },
            "text":        { "type": "text" },
            "value":       { "type": "keyword" }
        }}
    });
    node.request_ok("PUT", &format!("/{index}"), Some(&body))
        .with_context(|| format!("create defs index {index}"))?;
    Ok(())
}

/// Bulk-index every def document (`symbol_text` mirrors `symbol`, analysed).
fn index_defs(node: &HubNode, index: &str, defs: &[xerj_common::hub::DefRecord]) -> Result<()> {
    if defs.is_empty() {
        return Ok(());
    }
    let mut ndjson = String::with_capacity(defs.len() * 160);
    for d in defs {
        ndjson.push_str("{\"index\":{}}\n");
        let doc = json!({
            "symbol": d.symbol,
            "symbol_text": d.symbol,
            "kind": d.kind,
            "lang": d.lang,
            "file": d.file,
            "line": d.line,
            "text": d.text,
            "value": d.value,
        });
        ndjson.push_str(&doc.to_string());
        ndjson.push('\n');
    }
    let resp = node.bulk_ndjson(&format!("/{index}/_bulk"), ndjson)?;
    if resp.get("errors").and_then(Value::as_bool) == Some(true) {
        bail!("defs bulk index reported errors: {resp}");
    }
    Ok(())
}

/// `PUT /_snapshot/<repo>` with the fs location.
fn register_repo(node: &HubNode, repo: &str, location: &Path) -> Result<()> {
    let body = json!({
        "type": "fs",
        "settings": { "location": location.to_string_lossy() }
    });
    node.request_ok("PUT", &format!("/_snapshot/{repo}"), Some(&body))
        .with_context(|| format!("register snapshot repo {repo}"))?;
    Ok(())
}

/// The `-*` indices as `PackIndex` rows (name/docs/bytes), from `_cat/indices`.
fn pack_indices(node: &HubNode, prefix: &str) -> Result<Vec<PackIndex>> {
    let path = format!("/_cat/indices/{prefix}-*?format=json&h=index,docs.count,store.size.bytes");
    let v = node
        .request_ok("GET", &path, None)
        .with_context(|| format!("list indices {prefix}-*"))?;
    let mut out = Vec::new();
    for row in v.as_array().cloned().unwrap_or_default() {
        let name = row
            .get("index")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        out.push(PackIndex {
            name,
            docs: num_field(&row, "docs.count"),
            bytes: num_field(&row, "store.size.bytes"),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Read a `_cat` numeric column that may arrive as a number or a string.
fn num_field(row: &Value, key: &str) -> u64 {
    match row.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

// ── xerj hub pull ────────────────────────────────────────────────────────────

fn run_pull(args: &[String]) -> i32 {
    let mut slug: Option<String> = None;
    let mut url: Option<String> = None;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{HUB_USAGE}");
                return 0;
            }
            "--quiet" => quiet = true,
            "--url" => match args.get(i + 1) {
                Some(v) => {
                    url = Some(v.clone());
                    i += 1;
                }
                None => {
                    eprintln!("xerj hub pull: --url needs a value\n\n{HUB_USAGE}");
                    return 2;
                }
            },
            a if a.starts_with('-') => {
                eprintln!("xerj hub pull: unknown flag '{a}'\n\n{HUB_USAGE}");
                return 2;
            }
            a => {
                if slug.is_none() {
                    slug = Some(a.to_string());
                } else {
                    eprintln!("xerj hub pull: one <slug>, not two\n\n{HUB_USAGE}");
                    return 2;
                }
            }
        }
        i += 1;
    }
    let Some(slug) = slug else {
        eprintln!("xerj hub pull: <slug> is required\n\n{HUB_USAGE}");
        return 2;
    };
    let url = resolve_url(url.as_deref());
    match pull(&slug, &url, quiet) {
        Ok(Pulled::Already) => {
            if !quiet {
                println!("{slug}: already present");
            }
            0
        }
        Ok(Pulled::Done(line)) => {
            if !quiet {
                println!("{line}");
            }
            0
        }
        Err(PullError { code, message }) => {
            eprintln!("xerj hub pull: {message}");
            code
        }
    }
}

enum Pulled {
    Already,
    Done(String),
}

/// A pull failure carrying the plan's exit code (2 unknown slug, 3 verify fail).
struct PullError {
    code: i32,
    message: String,
}
impl PullError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        PullError {
            code,
            message: message.into(),
        }
    }
}

fn pull(slug: &str, url: &str, _quiet: bool) -> Result<Pulled, PullError> {
    let hub_home = hub_home();
    let hub_url = std::env::var("XERJ_HUB_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_HUB_URL.to_string());

    // 1. Catalogue → the entry for this slug. Unknown slug is exit 2.
    let cat = load_catalogue(&hub_url, &hub_home, false)
        .map_err(|e| PullError::new(2, format!("cannot load catalogue: {e:#}")))?;
    let entry = cat.corpora.iter().find(|c| c.slug == slug).ok_or_else(|| {
        PullError::new(
            2,
            format!(
                "unknown corpus '{slug}' (catalogue generated {}); `xerj hub status` lists what is \
                 available",
                cat.generated_at
            ),
        )
    })?;
    let pin8 = pin8_of(&entry.pin_sha)
        .map_err(|e| PullError::new(2, format!("catalogue entry for '{slug}': {e:#}")))?;

    // 2. Already present? Same pin in the local ledger AND its indices live on
    //    this node → nothing to do (plan §2 step 4 "skip restore and say so").
    let root = xerj_autoindex::xc::code_root();
    let node = HubNode::connect_with_key(url, None)
        .map_err(|e| PullError::new(2, format!("connect {url}: {e:#}")))?;
    if let Ok(st) = state::load_state(&root, slug) {
        if st.pin_sha.as_deref() == Some(entry.pin_sha.as_str()) {
            let prefix = state::query_prefix(&st);
            if node
                .cat_indices_json(&format!("{prefix}*"))
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                return Ok(Pulled::Already);
            }
        }
    }

    // 3. Download pack + sig into a temp dir. NOTHING is extracted yet.
    let pack_dir = hub_home.join("packs").join(slug);
    let tmp = pack_dir.join(format!("{pin8}.tmp"));
    let extract = pack_dir.join(pin8.clone());
    // A stale temp from a crashed pull must not poison this one.
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)
        .map_err(|e| PullError::new(3, format!("create {}: {e}", tmp.display())))?;
    let archive = tmp.join(format!("{slug}-{pin8}.hubpack.tar.zst"));
    let sig = tmp.join(format!("{slug}-{pin8}.hubpack.tar.zst.sig"));
    let dl = (|| -> Result<u64> {
        let n = fetch_to(&entry.pack_url, &archive)
            .with_context(|| format!("download pack {}", entry.pack_url))?;
        fetch_to(&entry.sig_url, &sig)
            .with_context(|| format!("download sig {}", entry.sig_url))?;
        Ok(n)
    })();
    let bytes = match dl {
        Ok(n) => n,
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(PullError::new(3, format!("{e:#}")));
        }
    };

    // 4. Verify sha256 THEN signature THEN format_version, extracting nothing
    //    until all pass (plan §11). Any failure: delete the temp, exit 3.
    let pubkey = hub_pubkey().map_err(|e| PullError::new(3, format!("{e:#}")))?;
    let manifest =
        match pack::read_and_verify_pack(&archive, &sig, &entry.pack_sha256, &pubkey, &extract) {
            Ok(m) => m,
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                // On failure read_and_verify_pack never populates `extract`; make
                // sure a half-written dir from an interrupted prior run is gone too.
                let _ = fs::remove_dir_all(&extract);
                return Err(PullError::new(
                    3,
                    format!("pack verification failed: {e:#}"),
                ));
            }
        };
    let _ = fs::remove_dir_all(&tmp);

    // 5. Register + restore the snapshot (unless the same prefix is already
    //    restored on this node).
    let prefix = manifest.index_prefix.clone();
    let already_live = node
        .cat_indices_json(&format!("{prefix}*"))
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if !already_live {
        let location = extract.join("snapshot");
        register_repo(&node, &format!("hub-{slug}"), &location)
            .map_err(|e| PullError::new(3, format!("register restore repo: {e:#}")))?;
        node.request_ok(
            "POST",
            &format!(
                "/_snapshot/hub-{slug}/{}/_restore?wait_for_completion=true",
                manifest.snapshot_name
            ),
            Some(&json!({ "indices": format!("{prefix}-*") })),
        )
        .map_err(|e| PullError::new(3, format!("restore snapshot: {e:#}")))?;
    }

    // 6. Write the ledger with source: hubpack — `xerj code <slug>` now works.
    state::write_hubpack_state(
        &root,
        slug,
        url,
        &manifest.index_prefix,
        &manifest.pin_sha,
        &manifest.pin_date,
        &manifest.licence_spdx,
        licence_class_str(manifest.licence_class),
    )
    .map_err(|e| PullError::new(3, format!("write state: {e}")))?;

    let n_indices = manifest.indices.len();
    Ok(Pulled::Done(format!(
        "pulled {slug} {pin8} {bytes} → {n_indices} indices, {} definitions",
        manifest.defs_count
    )))
}

/// Download `url` to `dest`. Supports `file://` (the tests' local packs) and
/// `http(s)://`. Returns the number of bytes written.
fn fetch_to(url: &str, dest: &Path) -> Result<u64> {
    if let Some(path) = url.strip_prefix("file://") {
        let n = fs::copy(path, dest).with_context(|| format!("copy {path}"))?;
        return Ok(n);
    }
    let resp = reqwest::blocking::get(url).with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} -> HTTP {status}");
    }
    let bytes = resp.bytes().with_context(|| format!("read body {url}"))?;
    fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(bytes.len() as u64)
}

// ── xerj code (with --def) ───────────────────────────────────────────────────

/// `xerj code …`: `--def <SYMBOL>` answers from the `-defs` index; everything
/// else is the existing retrieval path, delegated unchanged.
pub fn run_code(args: &[String]) -> i32 {
    if !args.iter().any(|a| a == "--def") {
        return xerj_autoindex::xc::run_code_cli(args);
    }
    run_code_def(args)
}

fn run_code_def(args: &[String]) -> i32 {
    let mut slug: Option<String> = None;
    let mut symbol: Option<String> = None;
    let mut k: usize = crate::def::DEFAULT_K;
    let mut url: Option<String> = None;
    let mut api_key: Option<String> = None;
    let mut as_json = false;
    let mut i = 0;
    let bad = |m: String| -> i32 {
        eprintln!("xerj code: {m}");
        2
    };
    while i < args.len() {
        macro_rules! val {
            ($n:expr) => {
                match args.get(i + 1) {
                    Some(v) => {
                        i += 1;
                        v.clone()
                    }
                    None => return bad(format!("{} needs a value", $n)),
                }
            };
        }
        match args[i].as_str() {
            "--def" => symbol = Some(val!("--def")),
            "-k" => {
                let v = val!("-k");
                match v.parse::<usize>() {
                    Ok(n) if (1..=50).contains(&n) => k = n,
                    _ => return bad(format!("-k must be 1..=50, got '{v}'")),
                }
            }
            "--url" => url = Some(val!("--url")),
            "--api-key" => api_key = Some(val!("--api-key")),
            "--json" => as_json = true,
            "-h" | "--help" => {
                println!("usage: xerj code <corpus> --def <SYMBOL> [-k N] [--json] [--url URL]");
                return 0;
            }
            a if a.starts_with('-') => return bad(format!("unknown flag '{a}' in --def mode")),
            a => {
                if slug.is_none() {
                    slug = Some(a.to_string());
                } else {
                    return bad(format!("unexpected argument '{a}'"));
                }
            }
        }
        i += 1;
    }
    let (Some(slug), Some(symbol)) = (slug, symbol) else {
        return bad("both <corpus> and --def <SYMBOL> are required".into());
    };

    let url = resolve_url(url.as_deref());
    let node = match HubNode::connect_with_key(&url, api_key) {
        Ok(n) => n,
        Err(e) => return bad(format!("cannot reach {url}: {e:#}")),
    };
    let root = xerj_autoindex::xc::code_root();

    // Mirror the retrieval path's refusals so a def miss is distinguishable
    // from "not indexed" (exit 2) and "not loaded on this node" (exit 3).
    let st = match state::load_state(&root, &slug) {
        Ok(st) => st,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };
    if let Err(refusal) = state::check_fresh(&st, false, "`--stale-ok`") {
        eprintln!("{refusal}");
        return 2;
    }
    let prefix = state::query_prefix(&st);
    if let Ok(live) = node.cat_indices_json(&format!("{prefix}*")) {
        if live.is_empty() {
            eprintln!("{}", state::not_loaded_message(&st, &prefix, &url));
            return 3;
        }
    }

    let answer = crate::def::lookup(&node, &slug, &symbol, k);
    emit_answer(&answer, as_json);
    if answer.hits.is_empty() {
        1
    } else {
        0
    }
}

/// Print an [`Answer`]: warnings to stderr, the §1 rendering to stdout, and the
/// JSON form to stdout when `--json`.
fn emit_answer(answer: &Answer, as_json: bool) {
    for w in &answer.warnings {
        eprintln!("{w}");
    }
    if as_json {
        match serde_json::to_string(answer) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("xerj code: cannot serialize answer: {e}"),
        }
    } else {
        println!("{}", render(answer));
    }
}

// ── small shared helpers ─────────────────────────────────────────────────────

/// `~/.xerj/hub`, overridable by `XERJ_HUB_HOME` (tests). Never `/tmp`: the
/// pack cache and catalogue must survive a reboot (same rule as `code_root`).
fn hub_home() -> PathBuf {
    if let Ok(h) = std::env::var("XERJ_HUB_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".xerj").join("hub")
}

/// The hub's ed25519 public key: `XERJ_HUB_PUBKEY` (tests) else the compiled-in
/// [`HUB_PUBKEY_HEX`]. An empty key is a hard error — an unverifiable pack is
/// never extracted.
fn hub_pubkey() -> Result<String> {
    if let Ok(k) = std::env::var("XERJ_HUB_PUBKEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    if !HUB_PUBKEY_HEX.is_empty() {
        return Ok(HUB_PUBKEY_HEX.to_string());
    }
    bail!(
        "no hub public key configured (HUB_PUBKEY_HEX is empty and XERJ_HUB_PUBKEY is unset); \
           cannot verify a pack signature"
    )
}

/// First 8 hex chars of a 40-hex commit — the `pin8` in pack names.
fn pin8_of(pin_sha: &str) -> Result<String> {
    let s = pin_sha.trim();
    if s.len() < 8 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("pin sha '{s}' is not a hex commit of at least 8 chars");
    }
    Ok(s[..8].to_ascii_lowercase())
}

/// The kebab-case string for a licence class — the state ledger's spelling.
fn licence_class_str(c: LicenceClass) -> &'static str {
    match c {
        LicenceClass::AdaptWithAttribution => "adapt-with-attribution",
        LicenceClass::ApproachOnly => "approach-only",
        LicenceClass::Excluded => "excluded",
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin8_takes_eight_lowercase_hex() {
        assert_eq!(
            pin8_of("0774D05537f9762f838f7ab541b7765f1a729cb5").unwrap(),
            "0774d055"
        );
        assert!(pin8_of("short").is_err());
        assert!(pin8_of("nothexxx").is_err());
    }

    #[test]
    fn licence_class_strings_round_trip_through_the_state_spelling() {
        for (c, s) in [
            (LicenceClass::AdaptWithAttribution, "adapt-with-attribution"),
            (LicenceClass::ApproachOnly, "approach-only"),
            (LicenceClass::Excluded, "excluded"),
        ] {
            assert_eq!(licence_class_str(c), s);
        }
    }

    #[test]
    fn run_code_without_def_does_not_enter_the_def_path() {
        // No node is contacted and no panic: a bare `code` with a bad flag
        // routes to the existing CLI, which returns its own usage exit code.
        let rc = run_code(&["--definitely-not-a-flag".into()]);
        assert_eq!(rc, 2, "delegates to run_code_cli, which rejects the flag");
    }

    #[test]
    fn hub_usage_names_both_subcommands() {
        assert!(HUB_USAGE.contains("build-pack"));
        assert!(HUB_USAGE.contains("pull"));
        assert_eq!(run_hub_cli(&["help".into()]), 0);
        assert_eq!(run_hub_cli(&["bogus".into()]), 2);
    }

    #[test]
    fn fetch_to_copies_a_file_url() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        fs::write(&src, b"hub pack bytes").unwrap();
        let dst = tmp.path().join("dst.bin");
        let url = format!("file://{}", src.display());
        let n = fetch_to(&url, &dst).unwrap();
        assert_eq!(n, 14);
        assert_eq!(fs::read(&dst).unwrap(), b"hub pack bytes");
    }

    #[test]
    fn hub_pubkey_prefers_env_and_errors_when_empty() {
        // Serialise env mutation with other env-touching tests in this crate is
        // unnecessary here: XERJ_HUB_PUBKEY is unique to this path.
        std::env::set_var("XERJ_HUB_PUBKEY", "abcd");
        assert_eq!(hub_pubkey().unwrap(), "abcd");
        std::env::remove_var("XERJ_HUB_PUBKEY");
        // HUB_PUBKEY_HEX is empty in this build, so no key is an error.
        if HUB_PUBKEY_HEX.is_empty() {
            assert!(hub_pubkey().is_err());
        }
    }
}

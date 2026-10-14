//! Corpus Hub v2 — the pack build → pull → `def` roundtrip (plan §2/§3 "Tests").
//!
//! This boots REAL nodes in-process (an ephemeral TCP port each, the way
//! `xerj-api`'s endpoint tests build a router) so the snapshot/restore HTTP
//! calls run end to end:
//!
//! - build a mini-c pack on node A (create a `-defs` index, bulk the defs,
//!   snapshot it, [`pack::write_pack`] + a test keypair);
//! - pull it into node B's empty data dir through the real `xerj hub pull` CLI,
//!   verifying sha256 THEN signature before extracting;
//! - assert `def::lookup` answers `MINI_LIMIT = 128` and the state ledger says
//!   `source: hubpack`;
//! - a flipped archive byte makes `pull` exit 3 and extract nothing;
//! - a second pull of the same pin says "already present";
//! - a `format_version: 2` pack is refused (exit 3).
//!
//! The archive-level guarantees (verify-before-extract, SHA256SUMS coverage,
//! format-version refusal) are also covered by `pack.rs`'s unit tests; this file
//! adds the parts that need a live node: restore and the `def` query over it.
//!
//! One `#[tokio::test]` holds the whole flow because it mutates process-global
//! env (`XERJ_CODE_HOME`, `XERJ_HUB_HOME`, `XERJ_HUB_PUBKEY`) between scenarios.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};

use xerj_autoindex::harvest::sign::generate_keypair;
use xerj_hub::node::HubNode;
use xerj_hub::pack::{self, PackFileEntry};
use xerj_hub::{
    Catalogue, CatalogueEntry, DefRecord, LicenceClass, PackIndex, PackManifest, PackSource,
    Sketch, CATALOGUE_FORMAT_VERSION, PACK_FORMAT_VERSION,
};

const PIN_SHA: &str = "0774d05537f9762f838f7ab541b7765f1a729cb5";
const PIN8: &str = "0774d055";
const PIN_DATE: &str = "2026-10-02";
const PREFIX: &str = "xc-mini-c-b1";

/// Boot an es-compat node on an ephemeral port with `data_dir` and the given
/// snapshot-repo allowlist, and wait until it answers.
async fn spawn_node(data_dir: &Path, allowlist: Vec<String>) -> String {
    std::fs::create_dir_all(data_dir).unwrap();
    let mut config = xerj_common::config::Config::default();
    config.server.data_dir = data_dir.to_string_lossy().into_owned();
    config.storage.wal_sync = xerj_common::config::WalSync::Async;
    config.limits.snapshot_repo_allowlist = allowlist;
    let metrics = xerj_common::metrics::Metrics::new().unwrap();
    let engine = xerj_engine::Engine::new(config.clone()).unwrap();
    let state = xerj_api::state::AppState::new(config, engine, metrics);
    let app = xerj_api::router::build_es_compat_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app.into_make_service()).await;
    });
    let url = format!("http://127.0.0.1:{port}");
    let probe = url.clone();
    tokio::task::spawn_blocking(move || {
        HubNode::connect(&probe)
            .unwrap()
            .wait_until_reachable(Duration::from_secs(30))
            .unwrap();
    })
    .await
    .unwrap();
    url
}

/// The defs the test indexes and packs. `MINI_LIMIT` is the §3 lookup target.
fn sample_defs() -> Vec<DefRecord> {
    vec![
        DefRecord {
            symbol: "MINI_LIMIT".into(),
            kind: "define".into(),
            lang: "c".into(),
            file: "limits.h".into(),
            line: 5,
            text: "#define MINI_LIMIT 128".into(),
            value: Some("128".into()),
        },
        DefRecord {
            symbol: "MINI_HASH_SEED".into(),
            kind: "define".into(),
            lang: "c".into(),
            file: "hash.c".into(),
            line: 3,
            text: "#define MINI_HASH_SEED 5381".into(),
            value: Some("5381".into()),
        },
        DefRecord {
            symbol: "mini_clamp".into(),
            kind: "function".into(),
            lang: "c".into(),
            file: "clamp.c".into(),
            line: 4,
            text: "int mini_clamp(int value, int lo, int hi)".into(),
            value: None,
        },
    ]
}

fn sha256_hex(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A `PackManifest` for the mini-c pack at a given `format_version`.
fn manifest(format_version: u32) -> PackManifest {
    PackManifest {
        format_version,
        slug: "mini-c".into(),
        pin_sha: PIN_SHA.into(),
        pin_date: PIN_DATE.into(),
        built_at: "2026-10-14T00:00:00Z".into(),
        builder_version: "xerj test".into(),
        index_prefix: PREFIX.into(),
        indices: vec![
            PackIndex {
                name: format!("{PREFIX}-defs"),
                docs: 3,
                bytes: 0,
            },
            PackIndex {
                name: format!("{PREFIX}-docs"),
                docs: 1,
                bytes: 0,
            },
        ],
        snapshot_name: format!("mini-c-{PIN8}"),
        licence_spdx: "MIT".into(),
        licence_class: LicenceClass::AdaptWithAttribution,
        sources: vec![PackSource {
            repo: "mini-c".into(),
            url: "file:///fixtures/mini-c".into(),
            sha: PIN_SHA.into(),
            licence: "MIT".into(),
        }],
        defs_count: 3,
        files_count: 2,
    }
}

/// Create the docs + defs indices on `url`, bulk the defs, and snapshot
/// `<prefix>-*` into `location`. Leaves the snapshot files on disk for packing.
fn seed_and_snapshot(url: &str, location: &Path) {
    let node = HubNode::connect(url).unwrap();
    // A docs index so the snapshot carries more than one index (realistic).
    node.request_ok(
        "PUT",
        &format!("/{PREFIX}-docs"),
        Some(&json!({"mappings":{"properties":{"body":{"type":"text"}}}})),
    )
    .unwrap();
    node.bulk_ndjson(
        &format!("/{PREFIX}-docs/_bulk"),
        "{\"index\":{}}\n{\"body\":\"mini c reference\"}\n".to_string(),
    )
    .unwrap();

    // The defs index with the §3 mapping.
    node.request_ok(
        "PUT",
        &format!("/{PREFIX}-defs"),
        Some(&json!({"mappings":{"properties":{
            "symbol":{"type":"keyword"},
            "symbol_text":{"type":"text"},
            "kind":{"type":"keyword"},
            "lang":{"type":"keyword"},
            "file":{"type":"keyword"},
            "line":{"type":"integer"},
            "text":{"type":"text"},
            "value":{"type":"keyword"}
        }}})),
    )
    .unwrap();
    let mut ndjson = String::new();
    for d in sample_defs() {
        ndjson.push_str("{\"index\":{}}\n");
        ndjson.push_str(
            &json!({
                "symbol": d.symbol, "symbol_text": d.symbol, "kind": d.kind,
                "lang": d.lang, "file": d.file, "line": d.line, "text": d.text, "value": d.value,
            })
            .to_string(),
        );
        ndjson.push('\n');
    }
    node.bulk_ndjson(&format!("/{PREFIX}-defs/_bulk"), ndjson)
        .unwrap();
    node.request_ok("POST", &format!("/{PREFIX}-defs/_refresh"), None)
        .ok();

    node.request_ok(
        "PUT",
        "/_snapshot/hub-build",
        Some(&json!({"type":"fs","settings":{"location": location.to_string_lossy()}})),
    )
    .unwrap();
    node.request_ok(
        "PUT",
        &format!("/_snapshot/hub-build/mini-c-{PIN8}?wait_for_completion=true"),
        Some(&json!({"indices": format!("{PREFIX}-*")})),
    )
    .unwrap();
}

/// Write a catalogue with one mini-c entry pointing at a `file://` pack.
fn write_catalogue(hub_home: &Path, archive: &Path, pack_sha256: &str) {
    let sig = {
        let mut s = archive.as_os_str().to_os_string();
        s.push(".sig");
        PathBuf::from(s)
    };
    let entry = CatalogueEntry {
        slug: "mini-c".into(),
        domain: "DEV".into(),
        topics: vec!["c".into()],
        ask_for: "a tiny C fixture corpus".into(),
        pin_sha: PIN_SHA.into(),
        pin_date: PIN_DATE.into(),
        refresh_cadence_days: 7,
        licence_spdx: "MIT".into(),
        licence_class: LicenceClass::AdaptWithAttribution,
        pack_bytes: std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0),
        pack_url: format!("file://{}", archive.display()),
        pack_sha256: pack_sha256.to_string(),
        sig_url: format!("file://{}", sig.display()),
        dependency_names: vec![],
        quality_grade: None,
        term_sketch: Sketch {
            m_bits: 64,
            k_hashes: 7,
            bits_base64: "AAAAAAAAAAA=".into(),
        },
    };
    let cat = Catalogue {
        format_version: CATALOGUE_FORMAT_VERSION,
        generated_at: "2026-10-14T00:00:00Z".into(),
        corpora: vec![entry],
    };
    std::fs::create_dir_all(hub_home).unwrap();
    std::fs::write(
        hub_home.join("catalogue.json"),
        serde_json::to_vec(&cat).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pack_build_pull_def_roundtrip() {
    // One canonical work root so every snapshot location is inside the nodes'
    // allowlist (the engine refuses a repo location outside data_dir or the
    // allowlist — plan §2; we configure the root, never weaken the check).
    let work_tmp = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(work_tmp.path()).unwrap();
    let out = work.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let allowlist = vec![work.to_string_lossy().into_owned()];

    let (seed, public) = generate_keypair().unwrap();

    // ── Build the pack on node A ──────────────────────────────────────────
    let url_a = spawn_node(&work.join("data-a"), allowlist.clone()).await;
    let location = out.join("mini-c").join("snapshot");
    {
        let url_a = url_a.clone();
        let location = location.clone();
        tokio::task::spawn_blocking(move || seed_and_snapshot(&url_a, &location))
            .await
            .unwrap();
    }

    // write_pack from the on-disk snapshot + the matching defs/files.
    let defs = sample_defs();
    let files = vec![
        PackFileEntry {
            path: "LICENSE".into(),
            lang: "".into(),
            bytes: 1069,
            licence: "MIT".into(),
        },
        PackFileEntry {
            path: "limits.h".into(),
            lang: "c".into(),
            bytes: 300,
            licence: "MIT".into(),
        },
    ];
    let archive = out.join(format!("mini-c-{PIN8}.hubpack.tar.zst"));
    {
        let (m, d, f, loc, seed) = (
            manifest(PACK_FORMAT_VERSION),
            defs.clone(),
            files.clone(),
            location.clone(),
            seed.clone(),
        );
        let archive = archive.clone();
        tokio::task::spawn_blocking(move || {
            pack::write_pack(&archive, &m, &d, &f, &loc, &seed).unwrap();
        })
        .await
        .unwrap();
    }
    assert!(archive.exists(), "archive written");
    let good_sha = sha256_hex(&archive);

    // ── Scenario 1: a flipped byte is refused (exit 3), nothing extracted ──
    {
        let code_home = work.join("code-flip");
        let hub_home = work.join("hub-flip");
        let flipped = out.join("flipped.hubpack.tar.zst");
        std::fs::copy(&archive, &flipped).unwrap();
        // keep the sibling .sig next to the flipped copy
        std::fs::copy(
            archive.with_file_name(format!("mini-c-{PIN8}.hubpack.tar.zst.sig")),
            flipped.with_file_name("flipped.hubpack.tar.zst.sig"),
        )
        .unwrap();
        let mut bytes = std::fs::read(&flipped).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&flipped, &bytes).unwrap();
        // catalogue records the ORIGINAL sha, so the flip is caught.
        write_catalogue(&hub_home, &flipped, &good_sha);

        let url_a2 = url_a.clone();
        let (ch, hh, pk) = (code_home.clone(), hub_home.clone(), public.clone());
        let rc = tokio::task::spawn_blocking(move || {
            std::env::set_var("XERJ_CODE_HOME", &ch);
            std::env::set_var("XERJ_HUB_HOME", &hh);
            std::env::set_var("XERJ_HUB_PUBKEY", &pk);
            xerj_hub::cli::run_hub_cli(&[
                "pull".into(),
                "mini-c".into(),
                "--url".into(),
                url_a2,
                "--quiet".into(),
            ])
        })
        .await
        .unwrap();
        assert_eq!(rc, 3, "a flipped archive fails verification with exit 3");
        let extract = hub_home.join("packs").join("mini-c").join(PIN8);
        assert!(
            !extract.exists(),
            "nothing is extracted when verification fails"
        );
    }

    // ── Scenario 2: a format_version: 2 pack is refused (exit 3) ───────────
    {
        let code_home = work.join("code-v2");
        let hub_home = work.join("hub-v2");
        let v2 = out.join("v2.hubpack.tar.zst");
        {
            let (m, d, f, loc, seed) = (
                manifest(2),
                defs.clone(),
                files.clone(),
                location.clone(),
                seed.clone(),
            );
            let v2 = v2.clone();
            tokio::task::spawn_blocking(move || {
                pack::write_pack(&v2, &m, &d, &f, &loc, &seed).unwrap();
            })
            .await
            .unwrap();
        }
        let v2_sha = sha256_hex(&v2);
        write_catalogue(&hub_home, &v2, &v2_sha);

        let url_a2 = url_a.clone();
        let (ch, hh, pk) = (code_home.clone(), hub_home.clone(), public.clone());
        let rc = tokio::task::spawn_blocking(move || {
            std::env::set_var("XERJ_CODE_HOME", &ch);
            std::env::set_var("XERJ_HUB_HOME", &hh);
            std::env::set_var("XERJ_HUB_PUBKEY", &pk);
            xerj_hub::cli::run_hub_cli(&[
                "pull".into(),
                "mini-c".into(),
                "--url".into(),
                url_a2,
                "--quiet".into(),
            ])
        })
        .await
        .unwrap();
        assert_eq!(rc, 3, "a future pack format_version is refused");
    }

    // ── Scenario 3: a valid pull into node B, then def, then already-present ─
    let url_b = spawn_node(&work.join("data-b"), allowlist.clone()).await;
    let code_home = work.join("code-pull");
    let hub_home = work.join("hub-pull");
    write_catalogue(&hub_home, &archive, &good_sha);

    // First pull — downloads, verifies, restores, writes the ledger.
    let rc = {
        let (url_b, ch, hh, pk) = (
            url_b.clone(),
            code_home.clone(),
            hub_home.clone(),
            public.clone(),
        );
        tokio::task::spawn_blocking(move || {
            std::env::set_var("XERJ_CODE_HOME", &ch);
            std::env::set_var("XERJ_HUB_HOME", &hh);
            std::env::set_var("XERJ_HUB_PUBKEY", &pk);
            xerj_hub::cli::run_hub_cli(&[
                "pull".into(),
                "mini-c".into(),
                "--url".into(),
                url_b,
                "--quiet".into(),
            ])
        })
        .await
        .unwrap()
    };
    assert_eq!(rc, 0, "a valid pack pulls cleanly");

    // The ledger records source: hubpack.
    let state_json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(code_home.join("state").join("mini-c.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state_json["source"], json!("hubpack"));
    assert_eq!(state_json["index_prefix"], json!(PREFIX));
    assert_eq!(state_json["pin_sha"], json!(PIN_SHA));

    // def answers MINI_LIMIT = 128 from the restored defs index.
    let answer = {
        let (url_b, ch) = (url_b.clone(), code_home.clone());
        tokio::task::spawn_blocking(move || {
            std::env::set_var("XERJ_CODE_HOME", &ch);
            let node = HubNode::connect(&url_b).unwrap();
            xerj_hub::def::lookup(&node, "mini-c", "MINI_LIMIT", 5)
        })
        .await
        .unwrap()
    };
    assert_eq!(answer.corpus, "mini-c");
    assert_eq!(answer.pin_date, PIN_DATE);
    assert_eq!(answer.hits.len(), 1, "one exact MINI_LIMIT hit: {answer:?}");
    assert_eq!(answer.hits[0].value.as_deref(), Some("128"));
    assert_eq!(answer.hits[0].file, "limits.h");
    assert_eq!(answer.hits[0].line, 5);

    // Second pull of the same pin → already present, restores nothing.
    let rc2 = {
        let (url_b, ch, hh, pk) = (
            url_b.clone(),
            code_home.clone(),
            hub_home.clone(),
            public.clone(),
        );
        tokio::task::spawn_blocking(move || {
            std::env::set_var("XERJ_CODE_HOME", &ch);
            std::env::set_var("XERJ_HUB_HOME", &hh);
            std::env::set_var("XERJ_HUB_PUBKEY", &pk);
            xerj_hub::cli::run_hub_cli(&[
                "pull".into(),
                "mini-c".into(),
                "--url".into(),
                url_b,
                "--quiet".into(),
            ])
        })
        .await
        .unwrap()
    };
    assert_eq!(rc2, 0, "a second pull of the same pin is a no-op success");

    // Clean up the env so it does not leak to other tests in the binary.
    std::env::remove_var("XERJ_CODE_HOME");
    std::env::remove_var("XERJ_HUB_HOME");
    std::env::remove_var("XERJ_HUB_PUBKEY");
}

//! Index create, mapping, settings, and delete on one member of a 3-node
//! ring show up on the others. A document written on one member is readable
//! from the others because it is stored on the shard owner. A ring that is
//! not running refuses the create instead of applying it locally.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::watch;
use tower::ServiceExt;
use xerj_api::cluster_shard::ApiShardHandler;
use xerj_cluster::node::in_memory::{InMemoryBus, InMemoryTransport};
use xerj_cluster::runner::ClusterRunner;
use xerj_cluster::{ClusterHandle, ClusterNode};
use xerj_engine::EngineMetadataApplier;

struct Member {
    app: Router,
    engine: std::sync::Arc<xerj_engine::Engine>,
    shutdown: watch::Sender<bool>,
    handle: ClusterHandle,
    /// Held when the runner is not started, so the proposal channel stays open.
    _node: Option<ClusterNode>,
    _dir: tempfile::TempDir,
}

async fn ring(start_runners: bool) -> Vec<Member> {
    let bus = InMemoryBus::new();
    let ids = ["n1", "n2", "n3"];
    let mut members = Vec::new();
    for id in ids {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = xerj_common::config::Config::default();
        config.server.data_dir = dir.path().to_string_lossy().into_owned();
        let metrics = xerj_common::metrics::Metrics::new().expect("metrics");
        let engine = xerj_engine::Engine::new(config.clone()).expect("engine");
        let state = xerj_api::state::AppState::new(config, engine, metrics);

        let peers: Vec<String> = ids
            .iter()
            .filter(|peer| **peer != id)
            .map(|peer| (*peer).to_string())
            .collect();
        let transport = InMemoryTransport::new(id.to_string(), bus.clone()).await;
        let mut node = ClusterNode::new(id.to_string(), peers, Box::new(transport));
        node.set_applier(std::sync::Arc::new(EngineMetadataApplier {
            engine: std::sync::Arc::clone(&state.engine),
        }));
        let handle = node.handle();
        state.engine.attach_cluster(handle.clone());
        handle.set_shard_handler(std::sync::Arc::new(ApiShardHandler::new(state.clone())));
        let engine = std::sync::Arc::clone(&state.engine);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let kept = if start_runners {
            let mut runner = ClusterRunner::new(node, Duration::from_millis(5), shutdown_rx);
            tokio::spawn(async move {
                runner.run().await;
            });
            None
        } else {
            handle.set_timeout(Duration::from_millis(400));
            let _ = shutdown_rx;
            Some(node)
        };
        let app = xerj_api::router::build_es_compat_router(state);
        members.push(Member {
            app,
            engine,
            shutdown,
            handle,
            _node: kept,
            _dir: dir,
        });
    }
    members
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(req).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn put_json(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        Request::put(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
    )
    .await
}

async fn post_json(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
    )
    .await
}

async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::get(path).body(Body::empty()).expect("request"),
    )
    .await
}

async fn delete(app: &Router, path: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::delete(path).body(Body::empty()).expect("request"),
    )
    .await
}

async fn wait_leader(members: &[Member], dead: &[bool]) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let leaders: Vec<usize> = members
            .iter()
            .enumerate()
            .filter(|(i, member)| !dead[*i] && member.handle.is_leader())
            .map(|(i, _)| i)
            .collect();
        if leaders.len() == 1 {
            return leaders[0];
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for one raft leader");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_status(app: &Router, path: &str, want: StatusCode) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, body) = get(app, path).await;
        if status == want {
            return body;
        }
        if Instant::now() > deadline {
            panic!("GET {path} stayed {status} body={body} (wanted {want})");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_body_contains(app: &Router, path: &str, needle: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (status, body) = get(app, path).await;
        if status.is_success() && body.to_string().contains(needle) {
            return body;
        }
        if Instant::now() > deadline {
            panic!("GET {path} never contained {needle}: {status} {body}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_metadata_propagates_and_a_new_leader_still_commits() {
    let members = ring(true).await;
    let mut dead = [false, false, false];
    let leader = wait_leader(&members, &dead).await;
    let follower = (0..3).find(|i| *i != leader).expect("a follower");

    let (status, body) = put_json(
        &members[follower].app,
        "/orders",
        json!({
            "mappings": { "properties": { "title": { "type": "text" } } }
        }),
    )
    .await;
    assert!(status.is_success(), "create from follower: {status} {body}");

    for member in &members {
        wait_status(&member.app, "/orders", StatusCode::OK).await;
        let mapping = wait_status(&member.app, "/orders/_mapping", StatusCode::OK).await;
        assert!(
            mapping.to_string().contains("title"),
            "mapping did not propagate: {mapping}"
        );
    }

    let (status, body) = put_json(
        &members[follower].app,
        "/orders/_settings",
        json!({ "index": { "number_of_replicas": "2" } }),
    )
    .await;
    assert!(status.is_success(), "settings: {status} {body}");
    for member in &members {
        wait_body_contains(&member.app, "/orders/_settings", "number_of_replicas").await;
    }

    let (status, body) = put_json(
        &members[follower].app,
        "/orders/_doc/1",
        json!({ "title": "routed" }),
    )
    .await;
    assert!(status.is_success(), "document write: {status} {body}");
    for member in &members {
        let (status, body) = get(&member.app, "/orders/_doc/1").await;
        assert!(
            status.is_success(),
            "document is readable on every member: {status} {body}"
        );
    }
    let owners = members
        .iter()
        .filter(|member| member.engine.owns_shard("orders", "1"))
        .count();
    assert_eq!(owners, 1, "the document has one primary owner");

    let (status, body) = delete(&members[follower].app, "/orders").await;
    assert!(status.is_success(), "delete: {status} {body}");
    for member in &members {
        wait_status(&member.app, "/orders", StatusCode::NOT_FOUND).await;
    }

    let _ = members[leader].shutdown.send(true);
    dead[leader] = true;
    let next = wait_leader(&members, &dead).await;
    assert_ne!(next, leader);
    let proposer = (0..3).find(|i| !dead[*i] && *i != next).unwrap_or(next);
    let (status, body) = put_json(&members[proposer].app, "/tickets", json!({})).await;
    assert!(
        status.is_success(),
        "create after leader change: {status} {body}"
    );
    for (i, member) in members.iter().enumerate() {
        if dead[i] {
            continue;
        }
        wait_status(&member.app, "/tickets", StatusCode::OK).await;
    }

    for member in &members {
        let _ = member.shutdown.send(true);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ring_with_no_leader_does_not_create_the_index() {
    let members = ring(false).await;
    let (status, body) = put_json(&members[0].app, "/orders", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, _) = get(&members[0].app, "/orders").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = put_json(
        &members[0].app,
        "/orders/_doc/1",
        json!({ "title": "no leader" }),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, _) = get(&members[0].app, "/orders/_doc/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&members[1].app, "/orders").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_disabled_create_stays_on_the_node_that_received_it() {
    async fn lone() -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = xerj_common::config::Config::default();
        config.server.data_dir = dir.path().to_string_lossy().into_owned();
        let metrics = xerj_common::metrics::Metrics::new().expect("metrics");
        let engine = xerj_engine::Engine::new(config.clone()).expect("engine");
        assert!(!engine.has_cluster());
        let state = xerj_api::state::AppState::new(config, engine, metrics);
        (xerj_api::router::build_es_compat_router(state), dir)
    }

    let (a, _dir_a) = lone().await;
    let (b, _dir_b) = lone().await;
    let (status, body) = put_json(&a, "/local", json!({})).await;
    assert!(status.is_success(), "{status} {body}");
    let (status, _) = get(&a, "/local").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&b, "/local").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = put_json(&a, "/local/_doc/1", json!({ "title": "here" })).await;
    assert!(
        status.is_success(),
        "single-node document write: {status} {body}"
    );
    let (status, _) = get(&a, "/local/_doc/1").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(&b, "/local/_doc/1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = post_json(
        &a,
        "/local/_search",
        json!({ "query": { "match_all": {} }, "size": 10 }),
    )
    .await;
    assert!(status.is_success(), "single-node search: {status} {body}");
    assert_eq!(
        body.pointer("/hits/total/value").and_then(|v| v.as_u64()),
        Some(1),
        "single-node search total: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_is_stored_on_its_shard_owner_and_search_sees_it_from_any_member() {
    let members = ring(true).await;
    let dead = [false, false, false];
    let _leader = wait_leader(&members, &dead).await;

    let (status, body) = put_json(&members[0].app, "/orders", json!({})).await;
    assert!(status.is_success(), "create: {status} {body}");
    for member in &members {
        wait_status(&member.app, "/orders", StatusCode::OK).await;
        assert_eq!(
            member.engine.shard_search_targets("orders").len(),
            3,
            "create installs one primary on each member"
        );
    }

    let mut owner_of: Vec<(String, usize)> = Vec::new();
    for i in 0..48 {
        let id = format!("d{i}");
        if let Some(owner) = members
            .iter()
            .position(|member| member.engine.owns_shard("orders", &id))
        {
            if !owner_of.iter().any(|(_, seen)| *seen == owner) {
                owner_of.push((id, owner));
            }
        }
        if owner_of.len() == 3 {
            break;
        }
    }
    assert_eq!(
        owner_of.len(),
        3,
        "expected the plan to place some id on each member"
    );

    for (id, _) in &owner_of {
        let writer = (owner_of[0].1 + 1) % 3;
        let (status, body) = put_json(
            &members[writer].app,
            &format!("/orders/_doc/{id}"),
            json!({ "title": id }),
        )
        .await;
        assert!(status.is_success(), "write {id}: {status} {body}");
    }

    for (id, owner) in &owner_of {
        for (i, member) in members.iter().enumerate() {
            let (status, body) = get(&member.app, &format!("/orders/_doc/{id}")).await;
            assert!(
                status.is_success(),
                "GET {id} on member {i}: {status} {body}"
            );
            let local = member
                .engine
                .get_index("orders")
                .expect("index")
                .get_document(id)
                .await
                .expect("local get");
            if i == *owner {
                assert!(local.is_some(), "owner stores {id}");
            } else {
                assert!(local.is_none(), "non-owner does not store {id}");
            }
        }
    }

    let reader = (owner_of[0].1 + 2) % 3;
    let (status, body) = post_json(
        &members[reader].app,
        "/orders/_search",
        json!({ "query": { "match_all": {} }, "size": 20 }),
    )
    .await;
    assert!(status.is_success(), "search: {status} {body}");
    let hits = body
        .pointer("/hits/hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let ids: Vec<&str> = hits
        .iter()
        .filter_map(|hit| hit.get("_id").and_then(|v| v.as_str()))
        .collect();
    for (id, _) in &owner_of {
        assert!(ids.contains(&id.as_str()), "search missed {id} in {ids:?}");
    }
    let total = body.pointer("/hits/total/value").and_then(|v| v.as_u64());
    assert_eq!(total, Some(owner_of.len() as u64), "merged total: {body}");

    let (status, body) = post_json(
        &members[reader].app,
        "/orders/_search",
        json!({ "size": 0, "aggs": { "titles": { "terms": { "field": "title" } } } }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "aggregations are not merged: {body}"
    );

    let (id, owner) = &owner_of[0];
    let deleter = (*owner + 1) % 3;
    let (status, body) = delete(&members[deleter].app, &format!("/orders/_doc/{id}")).await;
    assert!(status.is_success(), "delete: {status} {body}");
    for member in &members {
        let (status, _) = get(&member.app, &format!("/orders/_doc/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let still = members[*owner]
        .engine
        .get_index("orders")
        .expect("index")
        .get_document(id)
        .await
        .expect("local get");
    assert!(still.is_none(), "delete reached the owner");

    for member in &members {
        let _ = member.shutdown.send(true);
    }
}

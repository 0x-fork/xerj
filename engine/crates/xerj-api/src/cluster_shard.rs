//! Shard routing for the ES-compat document and search handlers.
//!
//! With a cluster handle attached, a replicated index carries a primary plan
//! in the same Raft entry as the create. [`dispatch_index`] forwards a
//! document to the owner [`Engine::route_write`] names. The primary then
//! copies an index or delete onto the in-sync replicas and acknowledges the
//! caller when a quorum of the copies has confirmed. A replica that does not
//! confirm is dropped from the in-sync set when the rest already make a
//! quorum. That replica copies the primary's live documents when it can
//! answer again, and the primary then publishes it back into the in-sync
//! set. [`fanout_search`] asks every
//! primary and merges hits. If a primary does not answer, a GET or a search
//! of that shard reads a replica copy. Single-node mode (`has_cluster`
//! false) returns before any of this.
//!
//! Replica bytes live in a hidden per-shard index, not in the user index, so
//! a primary search does not count them twice. The owner's handler does not
//! route a `replica` apply again.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use xerj_cluster::{ApplyEffect, ClusterCommand, ShardOp, ShardResult};

use crate::es_compat::{
    apply_get_doc_source_filter, search_shard_local, EsSearchBody, EsSearchJson,
    EsSearchQueryParams, GetDocParams,
};
use crate::responses::{EsDeleteDocResponse, EsDocResponse, EsGetResponse};
use crate::state::AppState;
use crate::ApiError;

/// Per-shard `from + size` cap when a search is merged across owners.
const MAX_FANOUT_WINDOW: usize = 10_000;

/// How long a forward waits for the owner to apply the index create.
const OWNER_READY_WAIT: Duration = Duration::from_secs(2);

pub(crate) enum Dispatch {
    /// This node holds the shard (or the index has no plan). The caller
    /// uses its existing local path.
    Local,
    /// The response is complete: forwarded, or refused.
    Done(Response),
}

pub(crate) fn shard_not_ready_body(index: &str) -> String {
    json!({
        "error": {
            "type": "shard_not_ready",
            "reason": format!("index [{index}] is not on this shard yet")
        },
        "status": 503
    })
    .to_string()
}

pub(crate) fn shard_not_ready_response(index: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "type": "shard_not_ready",
                "reason": format!("index [{index}] is not on this shard yet")
            },
            "status": 503
        })),
    )
        .into_response()
}

fn http_result(result: ShardResult) -> Response {
    let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let value: Value = serde_json::from_str(&result.body).unwrap_or(Value::Null);
    (status, Json(value)).into_response()
}

fn is_shard_not_ready(result: &ShardResult) -> bool {
    result.status == 503 && result.body.contains("shard_not_ready")
}

fn engine_error_response(err: xerj_engine::EngineError) -> Response {
    ApiError::new(xerj_common::XerjError::from(err)).into_response()
}

async fn forward_retrying(state: &AppState, owner: &str, op: ShardOp) -> Response {
    let deadline = Instant::now() + OWNER_READY_WAIT;
    loop {
        match state.engine.forward_shard(owner, op.clone()).await {
            Ok(result) if is_shard_not_ready(&result) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(result) => return http_result(result),
            Err(err) => return engine_error_response(err),
        }
    }
}

/// Route a prepared document. `Ok`-shaped [`Dispatch::Local`] means index here.
///
/// `op` is `"index"` or `"create"`. Sequence numbers are the engine's
/// internal (1-based) values.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_index(
    state: &AppState,
    index: &str,
    id: &str,
    doc: &Value,
    op: &str,
    if_seq_no: Option<u64>,
    if_primary_term: Option<u64>,
    version: Option<u64>,
    version_type: Option<String>,
) -> Dispatch {
    if !state.engine.has_cluster() {
        return Dispatch::Local;
    }
    if state.engine.get_index(index).is_err() {
        if let Err(err) = state.engine.ensure_replicated_index(index).await {
            return Dispatch::Done(engine_error_response(err));
        }
    }
    let source_json = doc.to_string();
    if state.engine.owns_shard(index, id) {
        let result = local_index(
            state,
            index,
            id,
            &source_json,
            op,
            if_seq_no,
            if_primary_term,
            version,
            version_type.as_deref(),
            false,
            0,
        )
        .await;
        return Dispatch::Done(http_result(result));
    }
    let Some(owner) = state.engine.route_write(index, id) else {
        return Dispatch::Local;
    };
    let shard_op = ShardOp::IndexDoc {
        index: index.to_string(),
        id: id.to_string(),
        source_json,
        op: op.to_string(),
        if_seq_no,
        if_primary_term,
        version,
        version_type,
        replica: false,
        shard: 0,
    };
    Dispatch::Done(forward_retrying(state, &owner, shard_op).await)
}

pub(crate) async fn dispatch_get(
    state: &AppState,
    index: &str,
    id: &str,
    params: &GetDocParams,
) -> Dispatch {
    if !state.engine.has_cluster() {
        return Dispatch::Local;
    }
    let Some((shard, owner)) = state.engine.primary_of(index, id) else {
        return Dispatch::Local;
    };
    if owner == state.engine.routing_node_id() {
        return Dispatch::Local;
    }
    let op = ShardOp::GetDoc {
        index: index.to_string(),
        id: id.to_string(),
        source: params.source.clone(),
        source_includes: params.source_includes.clone(),
        source_excludes: params.source_excludes.clone(),
        replica: false,
        shard: 0,
    };
    if let Some(response) = forward_primary(state, &owner, op).await {
        return Dispatch::Done(response);
    }
    for replica in state.engine.replica_nodes(index, shard) {
        let op = ShardOp::GetDoc {
            index: index.to_string(),
            id: id.to_string(),
            source: params.source.clone(),
            source_includes: params.source_includes.clone(),
            source_excludes: params.source_excludes.clone(),
            replica: true,
            shard,
        };
        match state.engine.forward_shard(&replica, op).await {
            Ok(result) if !is_shard_not_ready(&result) => {
                return Dispatch::Done(http_result(result));
            }
            _ => continue,
        }
    }
    Dispatch::Done(shard_not_ready_response(index))
}

/// `Some` when the primary answered (including a 404). `None` when the
/// primary could not be reached, so the caller may read a replica.
async fn forward_primary(state: &AppState, owner: &str, op: ShardOp) -> Option<Response> {
    let deadline = Instant::now() + OWNER_READY_WAIT;
    loop {
        match state.engine.forward_shard(owner, op.clone()).await {
            Ok(result) if is_shard_not_ready(&result) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(result) => return Some(http_result(result)),
            Err(_) => return None,
        }
    }
}

pub(crate) async fn dispatch_delete(
    state: &AppState,
    index: &str,
    id: &str,
    if_seq_no: Option<u64>,
    if_primary_term: Option<u64>,
) -> Dispatch {
    if !state.engine.has_cluster() {
        return Dispatch::Local;
    }
    if state.engine.owns_shard(index, id) {
        let result = local_delete(state, index, id, if_seq_no, if_primary_term, false, 0).await;
        return Dispatch::Done(http_result(result));
    }
    let Some(owner) = state.engine.route_write(index, id) else {
        return Dispatch::Local;
    };
    let op = ShardOp::DeleteDoc {
        index: index.to_string(),
        id: id.to_string(),
        if_seq_no,
        if_primary_term,
        replica: false,
        shard: 0,
    };
    Dispatch::Done(forward_retrying(state, &owner, op).await)
}

fn cluster_search_unsupported(feature: &str) -> Response {
    let reason = format!(
        "cluster search does not merge {feature} across shards yet; the request was not executed"
    );
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "type": "illegal_argument_exception",
                "reason": reason
            },
            "status": 400
        })),
    )
        .into_response()
}

/// `Some` when this search was answered by fan-out (including a refusal).
/// `None` when the caller should run the ordinary local search.
pub(crate) async fn fanout_search(
    state: &AppState,
    index: &str,
    params: &EsSearchQueryParams,
    principal: &crate::auth::Principal,
    body: &EsSearchJson,
) -> Option<Response> {
    if !state.engine.has_cluster() {
        return None;
    }
    if index.contains(',') || index.contains('*') || index == "_all" {
        return None;
    }
    let targets = state.engine.shard_search_targets(index);
    let me = state.engine.routing_node_id();
    if targets.is_empty() || (targets.len() == 1 && targets[0] == me) {
        return None;
    }
    let parsed = body.0.clone().unwrap_or_default();
    if parsed.aggs.is_some() || parsed.aggregations.is_some() {
        return Some(cluster_search_unsupported("aggregations"));
    }
    if parsed.suggest.is_some() {
        return Some(cluster_search_unsupported("suggest"));
    }
    if parsed.collapse.is_some() {
        return Some(cluster_search_unsupported("collapse"));
    }
    if parsed.rescore.is_some() {
        return Some(cluster_search_unsupported("rescore"));
    }
    if parsed.profile {
        return Some(cluster_search_unsupported("profile"));
    }
    if parsed.search_after.is_some() {
        return Some(cluster_search_unsupported("search_after"));
    }
    if params.scroll.is_some() {
        return Some(cluster_search_unsupported("scroll"));
    }

    let from = parsed.from;
    let size = parsed.size;
    let window = from.saturating_add(size);
    if window > MAX_FANOUT_WINDOW {
        return Some(cluster_search_unsupported("a from+size window above 10000"));
    }
    let mut shard_body = parsed;
    shard_body.from = 0;
    shard_body.size = window;
    let mut shard_params = params.clone();
    shard_params.from = Some("0".to_string());
    shard_params.size = Some(window.to_string());

    let started = Instant::now();
    let mut parts: Vec<Value> = Vec::with_capacity(targets.len());
    for target in &targets {
        let (status, value) = if target == &me {
            let response = Box::pin(search_shard_local(
                state.clone(),
                index.to_string(),
                shard_params.clone(),
                principal.clone(),
                shard_body.clone(),
            ))
            .await;
            match response_value(response).await {
                Ok(pair) => pair,
                Err(err) => {
                    return Some(shard_failure(target, &err));
                }
            }
        } else {
            let op = match (
                serde_json::to_string(&shard_body),
                serde_json::to_string(&shard_params),
            ) {
                (Ok(body_json), Ok(params_json)) => ShardOp::Search {
                    index: index.to_string(),
                    body_json,
                    params_json,
                },
                (Err(err), _) | (_, Err(err)) => {
                    return Some(shard_failure(target, &err.to_string()));
                }
            };
            match state.engine.forward_shard(target, op).await {
                Ok(result) => {
                    let value = serde_json::from_str(&result.body).unwrap_or(Value::Null);
                    (result.status, value)
                }
                Err(err) => match replica_search_for_primary(
                    state,
                    index,
                    target,
                    &shard_body,
                    &shard_params,
                    principal,
                )
                .await
                {
                    Some(value) => (200, value),
                    None => return Some(engine_error_response(err)),
                },
            }
        };
        if !(200..300).contains(&status) {
            return Some(
                (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    Json(value),
                )
                    .into_response(),
            );
        }
        parts.push(value);
    }

    let took_ms = started.elapsed().as_millis() as u64;
    let as_int = matches!(
        params.rest_total_hits_as_int.as_deref(),
        Some("true") | Some("1")
    );
    let merged = merge_hits(&parts, from, size, as_int, took_ms);
    Some(Json(merged).into_response())
}

fn shard_failure(target: &str, err: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "type": "shard_not_ready",
                "reason": format!("search on shard owner [{target}] failed: {err}")
            },
            "status": 503
        })),
    )
        .into_response()
}

async fn response_value(response: Response) -> Result<(u16, Value), String> {
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|err| err.to_string())?;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok((status, value))
}

fn merge_hits(
    parts: &[Value],
    from: usize,
    size: usize,
    total_as_int: bool,
    took_ms: u64,
) -> Value {
    let mut hits: Vec<Value> = Vec::new();
    let mut total: u64 = 0;
    let mut relation = "eq";
    let mut max_score: Option<f64> = None;
    let mut timed_out = false;
    for part in parts {
        if part.get("timed_out").and_then(Value::as_bool) == Some(true) {
            timed_out = true;
        }
        match part.pointer("/hits/total") {
            Some(Value::Number(n)) => total = total.saturating_add(n.as_u64().unwrap_or(0)),
            Some(Value::Object(_)) => {
                total = total.saturating_add(
                    part.pointer("/hits/total/value")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                if part.pointer("/hits/total/relation").and_then(Value::as_str) == Some("gte") {
                    relation = "gte";
                }
            }
            _ => {}
        }
        if let Some(score) = part.pointer("/hits/max_score").and_then(Value::as_f64) {
            max_score = Some(max_score.map(|current| current.max(score)).unwrap_or(score));
        }
        if let Some(arr) = part.pointer("/hits/hits").and_then(Value::as_array) {
            hits.extend(arr.iter().cloned());
        }
    }
    hits.sort_by(|left, right| {
        let left_score = left.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        let right_score = right.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left.get("_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(right.get("_id").and_then(Value::as_str).unwrap_or(""))
            })
    });
    if size == 0 {
        hits.clear();
    } else {
        hits = hits.into_iter().skip(from).take(size).collect();
    }
    let total_value = if total_as_int {
        json!(total)
    } else {
        json!({ "value": total, "relation": relation })
    };
    json!({
        "took": took_ms,
        "timed_out": timed_out,
        "_shards": {
            "total": parts.len(),
            "successful": parts.len(),
            "skipped": 0,
            "failed": 0
        },
        "hits": {
            "total": total_value,
            "max_score": max_score,
            "hits": hits
        }
    })
}

/// Handler installed on the cluster handle. Runs the operation locally.
pub struct ApiShardHandler {
    state: AppState,
}

impl ApiShardHandler {
    pub fn new(state: AppState) -> Self {
        ApiShardHandler { state }
    }
}

#[async_trait::async_trait]
impl xerj_cluster::ShardHandler for ApiShardHandler {
    async fn handle(&self, op: ShardOp) -> ShardResult {
        match op {
            ShardOp::IndexDoc {
                index,
                id,
                source_json,
                op,
                if_seq_no,
                if_primary_term,
                version,
                version_type,
                replica,
                shard,
            } => {
                local_index(
                    &self.state,
                    &index,
                    &id,
                    &source_json,
                    &op,
                    if_seq_no,
                    if_primary_term,
                    version,
                    version_type.as_deref(),
                    replica,
                    shard,
                )
                .await
            }
            ShardOp::GetDoc {
                index,
                id,
                source,
                source_includes,
                source_excludes,
                replica,
                shard,
            } => {
                local_get(
                    &self.state,
                    &index,
                    &id,
                    source,
                    source_includes,
                    source_excludes,
                    replica,
                    shard,
                )
                .await
            }
            ShardOp::DeleteDoc {
                index,
                id,
                if_seq_no,
                if_primary_term,
                replica,
                shard,
            } => {
                local_delete(
                    &self.state,
                    &index,
                    &id,
                    if_seq_no,
                    if_primary_term,
                    replica,
                    shard,
                )
                .await
            }
            ShardOp::Search {
                index,
                body_json,
                params_json,
            } => local_search(&self.state, &index, &body_json, &params_json).await,
            ShardOp::BackfillReplica {
                index,
                shard,
                replica_id,
            } => backfill_replica(&self.state, &index, shard, &replica_id).await,
            ShardOp::RetainReplicaDocs { index, shard, ids } => {
                retain_replica_docs(&self.state, &index, shard, &ids).await
            }
        }
    }
}

async fn response_to_result(response: Response) -> ShardResult {
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body = String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| "{}".to_string());
    ShardResult::http(status, body)
}

#[allow(clippy::too_many_arguments)]
async fn local_index(
    state: &AppState,
    index: &str,
    id: &str,
    source_json: &str,
    op: &str,
    if_seq_no: Option<u64>,
    if_primary_term: Option<u64>,
    version: Option<u64>,
    version_type: Option<&str>,
    replica: bool,
    shard: u32,
) -> ShardResult {
    let store = match doc_store(index, replica, shard) {
        Ok(store) => store,
        Err(result) => return result,
    };
    if open_store(state, &store).await.is_err() {
        return ShardResult::http(503, shard_not_ready_body(index));
    }
    let doc: Value = match serde_json::from_str(source_json) {
        Ok(doc) => doc,
        Err(err) => {
            return ShardResult::http(
                400,
                json!({
                    "error": {
                        "type": "document_parsing_exception",
                        "reason": format!("forwarded document is not JSON: {err}")
                    },
                    "status": 400
                })
                .to_string(),
            );
        }
    };
    let idx = match open_store(state, &store).await {
        Ok(idx) => idx,
        Err(_) => return ShardResult::http(503, shard_not_ready_body(index)),
    };
    let result = if op == "create" {
        idx.create_document(id.to_string(), doc).await
    } else if let (Some(version), Some(version_type)) = (version, version_type) {
        if version_type == "external" || version_type == "external_gte" {
            idx.index_document_external(Some(id.to_string()), doc, version, version_type)
                .await
        } else {
            idx.index_document_with_version(Some(id.to_string()), doc, if_seq_no, if_primary_term)
                .await
        }
    } else {
        idx.index_document_with_version(Some(id.to_string()), doc, if_seq_no, if_primary_term)
            .await
    };
    match result {
        Ok(resp) => {
            state.metrics.record_doc_indexed(index);
            let wire_seq = resp.seq_no.saturating_sub(1);
            let (body, status) = if resp.result == "updated" {
                (
                    EsDocResponse::updated(index, &resp.id, resp.version, wire_seq),
                    200,
                )
            } else {
                (
                    EsDocResponse::created(index, &resp.id, resp.version, wire_seq),
                    201,
                )
            };
            let result = ShardResult::http(
                status,
                serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()),
            );
            if replica {
                return result;
            }
            if let Err(nack) = replicate_write(
                state,
                index,
                id,
                ShardOp::IndexDoc {
                    index: index.to_string(),
                    id: id.to_string(),
                    source_json: source_json.to_string(),
                    op: op.to_string(),
                    if_seq_no,
                    if_primary_term,
                    version,
                    version_type: version_type.map(str::to_string),
                    replica: false,
                    shard: 0,
                },
            )
            .await
            {
                return nack;
            }
            result
        }
        Err(err) => response_to_result(engine_error_response(err)).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn local_get(
    state: &AppState,
    index: &str,
    id: &str,
    source: Option<String>,
    source_includes: Option<String>,
    source_excludes: Option<String>,
    replica: bool,
    shard: u32,
) -> ShardResult {
    if !replica && state.engine.closed_indices.contains_key(index) {
        return response_to_result(crate::es_compat::closed_index_error(index)).await;
    }
    let store = match doc_store(index, replica, shard) {
        Ok(store) => store,
        Err(result) => return result,
    };
    let idx = match open_store(state, &store).await {
        Ok(idx) => idx,
        Err(_) => return ShardResult::http(503, shard_not_ready_body(index)),
    };
    match idx.get_document(id).await {
        Ok(Some(mut doc)) => {
            xerj_query::executor::strip_internal_passage_metadata(&mut doc);
            let params = GetDocParams {
                source,
                source_includes,
                source_excludes,
            };
            let filtered = apply_get_doc_source_filter(doc, &params);
            let version = idx.lookup_version(id).unwrap_or(1);
            let seq_no = idx.lookup_seq_no(id).unwrap_or(0);
            let resp = EsGetResponse::found(index, id, version, seq_no, filtered);
            ShardResult::http(
                200,
                serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
            )
        }
        Ok(None) => {
            let resp = EsGetResponse::not_found(index, id);
            ShardResult::http(
                404,
                serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()),
            )
        }
        Err(err) => response_to_result(engine_error_response(err)).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn local_delete(
    state: &AppState,
    index: &str,
    id: &str,
    if_seq_no: Option<u64>,
    if_primary_term: Option<u64>,
    replica: bool,
    shard: u32,
) -> ShardResult {
    let store = match doc_store(index, replica, shard) {
        Ok(store) => store,
        Err(result) => return result,
    };
    let idx = match open_store(state, &store).await {
        Ok(idx) => idx,
        Err(_) => return ShardResult::http(503, shard_not_ready_body(index)),
    };
    match idx
        .delete_document_versioned(id, if_seq_no, if_primary_term)
        .await
    {
        Ok(outcome) if outcome.found => {
            let resp = EsDeleteDocResponse::deleted(
                index,
                id,
                outcome.version,
                outcome.seq_no.saturating_sub(1),
            );
            finish_delete(state, index, id, replica, 200, &resp).await
        }
        Ok(outcome) => {
            let resp = EsDeleteDocResponse::not_found(
                index,
                id,
                outcome.version,
                outcome.seq_no.saturating_sub(1),
            );
            finish_delete(state, index, id, replica, 404, &resp).await
        }
        Err(err) => response_to_result(engine_error_response(err)).await,
    }
}

async fn finish_delete(
    state: &AppState,
    index: &str,
    id: &str,
    replica: bool,
    status: u16,
    resp: &EsDeleteDocResponse,
) -> ShardResult {
    if !replica {
        if let Err(nack) = replicate_write(
            state,
            index,
            id,
            ShardOp::DeleteDoc {
                index: index.to_string(),
                id: id.to_string(),
                if_seq_no: None,
                if_primary_term: None,
                replica: false,
                shard: 0,
            },
        )
        .await
        {
            return nack;
        }
    }
    ShardResult::http(
        status,
        serde_json::to_string(resp).unwrap_or_else(|_| "{}".into()),
    )
}

/// Replica stores are cluster-internal. A caller's index ACL must not hide
/// them. A failover search of a replica that lives on this node runs on the
/// caller's task, and that task's visibility guard would otherwise 404 the
/// copy. The in-process shard thread has no request guard (it stands in for
/// a TCP handler); this still covers the local path.
struct OpenReplicaStore(String);

impl xerj_engine::index_guard::IndexVisibility for OpenReplicaStore {
    fn visible(&self, index: &str) -> bool {
        index == self.0
    }
}

async fn open_store(
    state: &AppState,
    name: &str,
) -> Result<Arc<xerj_engine::Index>, xerj_engine::EngineError> {
    if name.starts_with(".xerj_replica_") {
        let engine = Arc::clone(&state.engine);
        let owned = name.to_string();
        let guard = Arc::new(OpenReplicaStore(owned.clone()));
        return xerj_engine::index_guard::scoped(guard, async move { engine.get_index(&owned) })
            .await;
    }
    state.engine.get_index(name)
}

fn doc_store(index: &str, replica: bool, shard: u32) -> Result<String, ShardResult> {
    if !replica {
        return Ok(index.to_string());
    }
    xerj_cluster::replica_index_name(index, shard).ok_or_else(|| {
        ShardResult::http(
            400,
            json!({
                "error": {
                    "type": "invalid_index_name_exception",
                    "reason": format!("replica store name for [{index}] shard {shard} is not valid")
                },
                "status": 400
            })
            .to_string(),
        )
    })
}

fn quorum_nack(index: &str, needed: usize, got: usize, detail: &str) -> ShardResult {
    ShardResult::http(
        503,
        json!({
            "error": {
                "type": "replica_not_acknowledged",
                "reason": format!(
                    "primary applied the write on [{index}] but only {got} of {needed} replica acknowledgements are in sync: {detail}. The primary has the write; retry the index so an in-sync replica can take the copy"
                )
            },
            "status": 503
        })
        .to_string(),
    )
}

fn replica_nack(index: &str, replica: &str, detail: &str) -> ShardResult {
    ShardResult::http(
        503,
        json!({
            "error": {
                "type": "replica_not_acknowledged",
                "reason": format!(
                    "primary applied the write on [{index}] but replica [{replica}] did not acknowledge it: {detail}. The primary has the write; retry the index so the copy can catch up"
                )
            },
            "status": 503
        })
        .to_string(),
    )
}

/// Copy `op` onto the in-sync replicas and wait for a quorum.
///
/// The quorum is a majority of the copies placed at create (the primary,
/// which already has the write, plus [`xerj_cluster::replica_quorum_acks`]
/// replica confirmations). An empty placement returns immediately. The copy
/// is an upsert or a delete without the caller's sequence check: the
/// replica's own sequence numbers are not the primary's.
///
/// Every in-sync replica is asked. When enough confirm and some do not, the
/// ones that confirmed are published through the Raft log before the caller
/// is acknowledged, and later reads skip the rest until
/// [`backfill_replica`] copies the primary's live documents and publishes
/// the replica again. Two writes of the same shard do not publish different
/// sets; the primary holds `replica_sync_lock` across the copy and the
/// publish. Backfill takes that same lock.
async fn replicate_write(
    state: &AppState,
    index: &str,
    id: &str,
    op: ShardOp,
) -> Result<(), ShardResult> {
    let Some((shard, _)) = state.engine.primary_of(index, id) else {
        return Ok(());
    };
    let sync = state.engine.replica_sync_lock(index, shard);
    let _sync = sync.lock().await;
    let placed = state.engine.placed_replica_nodes(index, shard);
    if placed.is_empty() {
        return Ok(());
    }
    let targets = state.engine.replica_nodes(index, shard);
    let needed = xerj_cluster::replica_quorum_acks(placed.len());
    if targets.len() < needed {
        return Err(quorum_nack(
            index,
            needed,
            targets.len(),
            "fewer replicas are in sync than the quorum requires",
        ));
    }
    let op = replica_copy(op, shard);
    let mut acked: Vec<String> = Vec::new();
    let mut failed: Option<(String, String)> = None;
    for replica in &targets {
        match confirm_replica(state, replica, &op).await {
            Ok(()) => acked.push(replica.clone()),
            Err(detail) => {
                if failed.is_none() {
                    failed = Some((replica.clone(), detail));
                }
            }
        }
    }
    if acked.len() < needed {
        let (replica, detail) = failed.unwrap_or_else(|| {
            (
                "none".to_string(),
                "no in-sync replica acknowledged the write".to_string(),
            )
        });
        return Err(replica_nack(index, &replica, &detail));
    }
    if acked.len() == targets.len() {
        return Ok(());
    }
    acked.sort();
    acked.dedup();
    let Some(handle) = state.engine.cluster_handle() else {
        return Err(quorum_nack(
            index,
            needed,
            acked.len(),
            "no cluster handle to publish the in-sync set",
        ));
    };
    match handle
        .propose(ClusterCommand::SetShardInSync {
            index: index.to_string(),
            shard,
            nodes: acked.clone(),
        })
        .await
    {
        Ok(ApplyEffect::Applied) => {
            let published = state.engine.replica_nodes(index, shard);
            if acked.iter().all(|id| published.iter().any(|got| got == id)) {
                Ok(())
            } else {
                Err(quorum_nack(
                    index,
                    needed,
                    acked.len(),
                    "the in-sync set committed without the replicas that confirmed this write",
                ))
            }
        }
        Ok(other) => Err(quorum_nack(
            index,
            needed,
            acked.len(),
            &format!("in-sync update applied as {other:?}"),
        )),
        Err(err) => Err(quorum_nack(index, needed, acked.len(), &err.to_string())),
    }
}

async fn confirm_replica(state: &AppState, replica: &str, op: &ShardOp) -> Result<(), String> {
    let deadline = Instant::now() + OWNER_READY_WAIT;
    loop {
        match state.engine.forward_shard(replica, op.clone()).await {
            Ok(result) if is_shard_not_ready(&result) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(result) if replica_confirmed(op, &result) => return Ok(()),
            Ok(result) => return Err(result.body),
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn replica_copy(op: ShardOp, shard: u32) -> ShardOp {
    match op {
        ShardOp::IndexDoc {
            index,
            id,
            source_json,
            ..
        } => ShardOp::IndexDoc {
            index,
            id,
            source_json,
            op: "index".to_string(),
            if_seq_no: None,
            if_primary_term: None,
            version: None,
            version_type: None,
            replica: true,
            shard,
        },
        ShardOp::DeleteDoc { index, id, .. } => ShardOp::DeleteDoc {
            index,
            id,
            if_seq_no: None,
            if_primary_term: None,
            replica: true,
            shard,
        },
        other => other,
    }
}

fn replica_confirmed(op: &ShardOp, result: &ShardResult) -> bool {
    match op {
        // A delete of a copy that is already gone is still in sync.
        ShardOp::DeleteDoc { .. } => result.status == 200 || result.status == 404,
        _ => (200..300).contains(&result.status),
    }
}

fn backfill_body(status: u16, type_name: &str, reason: impl Into<String>) -> ShardResult {
    let reason = reason.into();
    ShardResult::http(
        status,
        json!({
            "error": { "type": type_name, "reason": reason },
            "status": status
        })
        .to_string(),
    )
}

fn already_in_sync() -> ShardResult {
    ShardResult::http(
        200,
        json!({ "already_in_sync": true, "copied": 0 }).to_string(),
    )
}

/// Live documents on this primary for `shard`.
///
/// The user index on the primary holds the documents it owns. Ids that
/// route elsewhere are skipped. A tombstone is not in the list. `Err` is
/// the reason the copy must not publish an in-sync set.
async fn primary_live_docs(
    state: &AppState,
    index: &str,
    shard: u32,
) -> Result<Vec<(String, Value)>, String> {
    let idx = state
        .engine
        .get_index(index)
        .map_err(|err| format!("primary index [{index}] is not open: {err}"))?;
    let mut docs = Vec::new();
    for id in idx.live_document_ids() {
        let Some((doc_shard, _)) = state.engine.primary_of(index, &id) else {
            continue;
        };
        if doc_shard != shard {
            continue;
        }
        match idx.get_document(&id).await {
            Ok(Some(source)) => docs.push((id, source)),
            Ok(None) => {}
            Err(err) => return Err(format!("reading [{index}] document [{id}]: {err}")),
        }
    }
    Ok(docs)
}

/// Copy this primary's live documents onto `replica_id` and publish it.
///
/// The replica lock is held from the snapshot through the in-sync publish.
/// A document write applies on the primary first and then waits for this
/// lock before it copies, so a write that missed the snapshot is replicated
/// to the set that includes `replica_id`. A failed copy leaves the in-sync
/// set unchanged. The replica retries.
async fn backfill_replica(
    state: &AppState,
    index: &str,
    shard: u32,
    replica_id: &str,
) -> ShardResult {
    if replica_id.is_empty() {
        return backfill_body(400, "illegal_argument_exception", "replica id is empty");
    }
    let me = state.engine.routing_node_id();
    if replica_id == me {
        return backfill_body(
            400,
            "illegal_argument_exception",
            "a primary does not backfill itself",
        );
    }
    let Some(owner) = state.engine.shard_primary(index, shard) else {
        return backfill_body(
            409,
            "shard_not_ready",
            format!("[{index}] shard {shard} has no primary on this node"),
        );
    };
    if owner != me {
        return backfill_body(
            409,
            "shard_not_primary",
            format!("[{index}] shard {shard} is owned by [{owner}], not [{me}]"),
        );
    }
    let placed = state.engine.placed_replica_nodes(index, shard);
    if !placed.iter().any(|id| id == replica_id) {
        return backfill_body(
            400,
            "illegal_argument_exception",
            format!("[{replica_id}] is not a replica of [{index}] shard {shard}"),
        );
    }
    if state
        .engine
        .replica_nodes(index, shard)
        .iter()
        .any(|id| id == replica_id)
    {
        return already_in_sync();
    }

    let sync = state.engine.replica_sync_lock(index, shard);
    let _guard = sync.lock().await;
    if state
        .engine
        .replica_nodes(index, shard)
        .iter()
        .any(|id| id == replica_id)
    {
        return already_in_sync();
    }

    let docs = match primary_live_docs(state, index, shard).await {
        Ok(docs) => docs,
        Err(reason) => return backfill_body(503, "replica_backfill_failed", reason),
    };
    for (id, source) in &docs {
        let source_json = match serde_json::to_string(source) {
            Ok(json) => json,
            Err(err) => {
                return backfill_body(
                    503,
                    "replica_backfill_failed",
                    format!("document [{id}] is not JSON: {err}"),
                );
            }
        };
        let op = ShardOp::IndexDoc {
            index: index.to_string(),
            id: id.clone(),
            source_json,
            op: "index".to_string(),
            if_seq_no: None,
            if_primary_term: None,
            version: None,
            version_type: None,
            replica: true,
            shard,
        };
        if let Err(detail) = confirm_replica(state, replica_id, &op).await {
            return backfill_body(
                503,
                "replica_backfill_failed",
                format!("replica [{replica_id}] did not take document [{id}]: {detail}"),
            );
        }
    }
    let ids: Vec<String> = docs.iter().map(|(id, _)| id.clone()).collect();
    let retain = ShardOp::RetainReplicaDocs {
        index: index.to_string(),
        shard,
        ids,
    };
    if let Err(detail) = confirm_replica(state, replica_id, &retain).await {
        return backfill_body(
            503,
            "replica_backfill_failed",
            format!(
                "replica [{replica_id}] did not drop documents the primary no longer has: {detail}"
            ),
        );
    }

    let mut nodes = state.engine.replica_nodes(index, shard);
    if !nodes.iter().any(|id| id == replica_id) {
        nodes.push(replica_id.to_string());
    }
    nodes.sort();
    nodes.dedup();
    let Some(handle) = state.engine.cluster_handle() else {
        return backfill_body(
            503,
            "replica_backfill_failed",
            "no cluster handle to publish the in-sync set",
        );
    };
    let copied = docs.len();
    match handle
        .propose(ClusterCommand::SetShardInSync {
            index: index.to_string(),
            shard,
            nodes,
        })
        .await
    {
        Ok(ApplyEffect::Applied) => {
            let published = state.engine.replica_nodes(index, shard);
            if !published.iter().any(|id| id == replica_id) {
                return backfill_body(
                    503,
                    "replica_backfill_failed",
                    "the in-sync set committed without the replica that was filled",
                );
            }
            tracing::info!(
                index = %index,
                shard,
                replica = %replica_id,
                copied,
                "copied primary documents onto a replica and published the in-sync set"
            );
            ShardResult::http(
                200,
                json!({ "already_in_sync": false, "copied": copied }).to_string(),
            )
        }
        Ok(other) => backfill_body(
            503,
            "replica_backfill_failed",
            format!("in-sync update applied as {other:?}"),
        ),
        Err(err) => backfill_body(503, "replica_backfill_failed", err.to_string()),
    }
}

/// Drop replica-store documents that are not in `ids`.
async fn retain_replica_docs(
    state: &AppState,
    index: &str,
    shard: u32,
    ids: &[String],
) -> ShardResult {
    let store = match doc_store(index, true, shard) {
        Ok(store) => store,
        Err(result) => return result,
    };
    let idx = match open_store(state, &store).await {
        Ok(idx) => idx,
        Err(_) => return ShardResult::http(503, shard_not_ready_body(index)),
    };
    let keep: HashSet<&str> = ids.iter().map(String::as_str).collect();
    for id in idx.live_document_ids() {
        if keep.contains(id.as_str()) {
            continue;
        }
        if let Err(err) = idx.delete_document(&id).await {
            return backfill_body(
                503,
                "replica_backfill_failed",
                format!("dropping [{id}] from [{store}]: {err}"),
            );
        }
    }
    ShardResult::http(200, json!({ "retained": ids.len() }).to_string())
}

/// Search the shards `primary` owns by reading one replica copy of each.
///
/// `None` when this index has no replica for that primary, or none of them
/// answered. The caller then fails the search the way it did before replicas
/// existed.
async fn replica_search_for_primary(
    state: &AppState,
    index: &str,
    primary: &str,
    body: &EsSearchBody,
    params: &EsSearchQueryParams,
    principal: &crate::auth::Principal,
) -> Option<Value> {
    let shards = state.engine.shards_owned_by(index, primary);
    if shards.is_empty() {
        return None;
    }
    let window = body.size;
    let mut parts = Vec::with_capacity(shards.len());
    for shard in shards {
        let store = xerj_cluster::replica_index_name(index, shard)?;
        let mut found = None;
        for replica in state.engine.replica_nodes(index, shard) {
            if let Some(mut value) =
                search_replica(state, &replica, &store, body, params, principal).await
            {
                rewrite_hit_index(&mut value, &store, index);
                found = Some(value);
                break;
            }
        }
        parts.push(found?);
    }
    Some(merge_hits(&parts, 0, window, false, 0))
}

async fn search_replica(
    state: &AppState,
    replica: &str,
    store: &str,
    body: &EsSearchBody,
    params: &EsSearchQueryParams,
    principal: &crate::auth::Principal,
) -> Option<Value> {
    if replica == state.engine.routing_node_id() {
        let engine_store = store.to_string();
        let guard = Arc::new(OpenReplicaStore(engine_store.clone()));
        let response = xerj_engine::index_guard::scoped(guard, async move {
            search_shard_local(
                state.clone(),
                engine_store,
                params.clone(),
                principal.clone(),
                body.clone(),
            )
            .await
        })
        .await;
        let (status, value) = response_value(response).await.ok()?;
        return (200..300).contains(&status).then_some(value);
    }
    let op = match (serde_json::to_string(body), serde_json::to_string(params)) {
        (Ok(body_json), Ok(params_json)) => ShardOp::Search {
            index: store.to_string(),
            body_json,
            params_json,
        },
        _ => return None,
    };
    match state.engine.forward_shard(replica, op).await {
        Ok(result) if (200..300).contains(&result.status) => {
            serde_json::from_str(&result.body).ok()
        }
        _ => None,
    }
}

fn rewrite_hit_index(value: &mut Value, from: &str, to: &str) {
    let Some(hits) = value
        .pointer_mut("/hits/hits")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for hit in hits {
        if hit.get("_index").and_then(Value::as_str) == Some(from) {
            hit["_index"] = json!(to);
        }
    }
}

async fn local_search(
    state: &AppState,
    index: &str,
    body_json: &str,
    params_json: &str,
) -> ShardResult {
    let params: EsSearchQueryParams = serde_json::from_str(params_json).unwrap_or_default();
    let body: EsSearchBody = match serde_json::from_str(body_json) {
        Ok(body) => body,
        Err(err) => {
            return ShardResult::http(
                400,
                json!({
                    "error": {
                        "type": "parsing_exception",
                        "reason": format!("forwarded search body is not JSON: {err}")
                    },
                    "status": 400
                })
                .to_string(),
            );
        }
    };
    // The accepting node already passed its auth middleware. This RPC is the
    // cluster transport, not a second client. The local shard of the
    // coordinator still runs under the caller's principal.
    let response = if index.starts_with(".xerj_replica_") {
        let guard = Arc::new(OpenReplicaStore(index.to_string()));
        xerj_engine::index_guard::scoped(guard, async move {
            search_shard_local(
                state.clone(),
                index.to_string(),
                params,
                crate::auth::Principal::Superuser,
                body,
            )
            .await
        })
        .await
    } else {
        Box::pin(search_shard_local(
            state.clone(),
            index.to_string(),
            params,
            crate::auth::Principal::Superuser,
            body,
        ))
        .await
    };
    response_to_result(response).await
}

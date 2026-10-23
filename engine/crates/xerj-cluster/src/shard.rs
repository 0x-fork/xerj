//! Data-plane calls between shard owners.
//!
//! Metadata (index create, the shard plan) goes through the Raft log.
//! A document does not. The node that accepts the HTTP call asks
//! [`crate::router::ShardRouter`] which member owns the document and, when
//! that member is someone else, invokes [`ShardRpc::call`]. The owner's
//! [`ShardHandler`] applies the operation to local storage and returns the
//! JSON the caller's HTTP response forwards.
//!
//! In-process tests share a [`MemoryShardDir`]. The TCP transport opens a
//! fresh authenticated connection, writes one [`crate::raft::RaftMessage::ShardRequest`],
//! and reads one [`crate::raft::RaftMessage::ShardResponse`] before closing.
//! That exchange is not the Raft outbound queue: a dropped heartbeat is
//! retransmitted, a dropped document write must not be.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One primary-shard assignment carried in a committed index-create entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardAssignment {
    pub shard: u32,
    pub node_id: String,
}

/// One primary per member, shard `i` on the `i`th member id in sorted order.
///
/// The plan is computed by the node that proposes the create and stored in
/// the log entry. Every member applies that list; nobody recomputes it.
pub fn primary_shard_plan(members: &[String]) -> Vec<ShardAssignment> {
    let mut members: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|id| !id.is_empty())
        .collect();
    members.sort_unstable();
    members.dedup();
    if members.is_empty() {
        return Vec::new();
    }
    (0..members.len() as u32)
        .map(|shard| ShardAssignment {
            shard,
            node_id: members[shard as usize].to_string(),
        })
        .collect()
}

/// Assignments embedded in a `CreateIndex` schema envelope, if any.
///
/// A schema that is not the replicated envelope — including the `"{}"`
/// fixtures the Raft tests propose — yields an empty plan.
pub fn shard_plan_from_schema_json(schema_json: &str) -> Vec<ShardAssignment> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(schema_json) else {
        return Vec::new();
    };
    let Some(items) = value.get("shard_plan").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let shard = item.get("shard")?.as_u64()?;
            let node_id = item.get("node_id")?.as_str()?.to_string();
            if node_id.is_empty() || shard > u32::MAX as u64 {
                return None;
            }
            Some(ShardAssignment {
                shard: shard as u32,
                node_id,
            })
        })
        .collect()
}

/// A document or search operation forwarded to the shard owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardOp {
    /// Index or create one document. `source_json` is the body after the
    /// accepting node has run pipelines and field checks.
    IndexDoc {
        index: String,
        id: String,
        source_json: String,
        /// `"index"` or `"create"`.
        op: String,
        /// Internal (1-based) sequence, already translated from the ES wire.
        if_seq_no: Option<u64>,
        if_primary_term: Option<u64>,
        version: Option<u64>,
        version_type: Option<String>,
    },
    GetDoc {
        index: String,
        id: String,
        source: Option<String>,
        source_includes: Option<String>,
        source_excludes: Option<String>,
    },
    DeleteDoc {
        index: String,
        id: String,
        if_seq_no: Option<u64>,
        if_primary_term: Option<u64>,
    },
    /// Search this member's local shard only. The caller merges hits.
    Search {
        index: String,
        /// JSON object, the search body, with `from`/`size` already widened
        /// to the window the merger needs.
        body_json: String,
        /// JSON [`crate`] search query params. `from` and `size` match the body.
        params_json: String,
    },
}

/// What the owner sends back. `status` is an HTTP status; `body` is JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardResult {
    pub status: u16,
    pub body: String,
}

impl ShardResult {
    pub fn http(status: u16, body: impl Into<String>) -> Self {
        ShardResult {
            status,
            body: body.into(),
        }
    }
}

/// Why a shard call did not produce a [`ShardResult`].
///
/// The document was not acknowledged. Callers must not write it locally
/// instead.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    #[error("shard call to {to} failed: {reason}")]
    Unavailable { to: String, reason: String },
    #[error("shard call to {to} timed out")]
    Timeout { to: String },
}

/// Executes a [`ShardOp`] against this node's engine.
#[async_trait::async_trait]
pub trait ShardHandler: Send + Sync {
    async fn handle(&self, op: ShardOp) -> ShardResult;
}

/// Sends a [`ShardOp`] to the named member and waits for its result.
#[async_trait::async_trait]
pub trait ShardRpc: Send + Sync {
    async fn call(&self, to: &str, op: ShardOp) -> Result<ShardResult, ShardError>;
    fn set_handler(&self, handler: Arc<dyn ShardHandler>);
}

/// Transport that has no data plane. Calls fail; nothing is written locally.
pub struct NullShardRpc;

#[async_trait::async_trait]
impl ShardRpc for NullShardRpc {
    async fn call(&self, to: &str, _op: ShardOp) -> Result<ShardResult, ShardError> {
        Err(ShardError::Unavailable {
            to: to.to_string(),
            reason: "this transport has no shard path".to_string(),
        })
    }

    fn set_handler(&self, _handler: Arc<dyn ShardHandler>) {}
}

/// Handler map shared by every in-process member of one ring.
#[derive(Default)]
pub struct MemoryShardDir {
    handlers: Mutex<HashMap<String, Arc<dyn ShardHandler>>>,
}

impl MemoryShardDir {
    pub fn new() -> Arc<Self> {
        Arc::new(MemoryShardDir {
            handlers: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<dyn ShardHandler>>> {
        self.handlers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// In-process [`ShardRpc`]. A call invokes the target's handler directly.
pub struct MemoryShardRpc {
    node_id: String,
    dir: Arc<MemoryShardDir>,
}

impl MemoryShardRpc {
    pub fn new(node_id: impl Into<String>, dir: Arc<MemoryShardDir>) -> Arc<Self> {
        Arc::new(MemoryShardRpc {
            node_id: node_id.into(),
            dir,
        })
    }
}

#[async_trait::async_trait]
impl ShardRpc for MemoryShardRpc {
    async fn call(&self, to: &str, op: ShardOp) -> Result<ShardResult, ShardError> {
        let handler = self.dir.lock().get(to).cloned();
        match handler {
            Some(handler) => Ok(handler.handle(op).await),
            None => Err(ShardError::Unavailable {
                to: to.to_string(),
                reason: "no shard handler registered on that member".to_string(),
            }),
        }
    }

    fn set_handler(&self, handler: Arc<dyn ShardHandler>) {
        self.dir.lock().insert(self.node_id.clone(), handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_one_primary_per_sorted_member() {
        let plan = primary_shard_plan(&["n3".into(), "n1".into(), "n2".into(), "n1".into()]);
        assert_eq!(
            plan,
            vec![
                ShardAssignment {
                    shard: 0,
                    node_id: "n1".into()
                },
                ShardAssignment {
                    shard: 1,
                    node_id: "n2".into()
                },
                ShardAssignment {
                    shard: 2,
                    node_id: "n3".into()
                },
            ]
        );
    }

    #[test]
    fn schema_without_a_plan_assigns_nothing() {
        assert!(shard_plan_from_schema_json("{}").is_empty());
        assert!(shard_plan_from_schema_json("not-json").is_empty());
        let parsed = shard_plan_from_schema_json(
            r#"{"v":1,"shard_plan":[{"shard":1,"node_id":"n2"},{"shard":0,"node_id":""}]}"#,
        );
        assert_eq!(
            parsed,
            vec![ShardAssignment {
                shard: 1,
                node_id: "n2".into()
            }]
        );
    }
}

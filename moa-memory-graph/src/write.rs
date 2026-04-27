//! Deferred graph write hooks.

use uuid::Uuid;

use crate::{GraphError, edge::EdgeWriteIntent, node::NodeWriteIntent};

/// Placeholder for M08's atomic node creation protocol.
pub async fn create_node(_intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
    Err(GraphError::NotImplemented("create_node lands in M08"))
}

/// Placeholder for M08's bitemporal supersession protocol.
pub async fn supersede_node(_old_uid: Uuid, _intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
    Err(GraphError::NotImplemented("supersede_node lands in M08"))
}

/// Placeholder for M08's soft-invalidation protocol.
pub async fn invalidate_node(_uid: Uuid, _reason: &str) -> Result<(), GraphError> {
    Err(GraphError::NotImplemented("invalidate_node lands in M08"))
}

/// Placeholder for M24's hard-purge protocol.
pub async fn hard_purge(_uid: Uuid, _redaction_marker: &str) -> Result<(), GraphError> {
    Err(GraphError::NotImplemented("hard_purge lands in M24"))
}

/// Placeholder for M08's atomic edge creation protocol.
pub async fn create_edge(_intent: EdgeWriteIntent) -> Result<Uuid, GraphError> {
    Err(GraphError::NotImplemented("create_edge lands in M08"))
}

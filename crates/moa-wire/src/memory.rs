//! Graph-memory service wire DTOs.

use chrono::{DateTime, Utc};
use moa_core::{
    types::contact::ContactId,
    types::identifiers::{SessionId, TenantId},
    types::memory::InformationBarrierId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for graph-memory search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchRequest {
    /// Persisted session whose pinned scope and agent policy authorize retrieval.
    pub session_id: SessionId,
    /// Search query text.
    pub query: String,
    /// Maximum number of hits to return.
    pub limit: u32,
}

/// Response payload containing graph-memory search hits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    /// Query text that produced these hits.
    pub query: String,
    /// Memory hits ordered by rank.
    #[serde(default)]
    pub hits: Vec<MemoryHit>,
}

/// One graph-memory hit returned to API renderers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Retrieval score assigned to the hit.
    pub score: f64,
    /// Short text snippet for table display.
    pub snippet: String,
    /// Retrieval legs that contributed to this hit.
    #[serde(default)]
    pub legs: Vec<String>,
    /// Optional server-side node summary or properties used for richer renderers.
    #[serde(default)]
    pub properties: Option<Value>,
    /// Tenant knowledge chunk uid when the hit is a knowledge chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_uid: Option<Uuid>,
    /// Knowledge document version containing the cited chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_version_uid: Option<Uuid>,
    /// Source URI for user-facing citations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Renderer-safe source title when the hit is a knowledge chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_title: Option<String>,
}

/// Request payload for showing one graph-memory node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryShowRequest {
    /// Persisted session whose pinned scope and agent policy authorize retrieval.
    pub session_id: SessionId,
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Neighbor traversal depth requested by the caller.
    #[serde(default)]
    pub neighbor_depth: u32,
}

/// Response payload containing one graph-memory node and immediate context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryShowResponse {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Persisted memory scope label.
    pub scope: String,
    /// Timestamp when this node version became valid.
    pub valid_from: DateTime<Utc>,
    /// Timestamp when this node version was superseded, if any.
    pub valid_to: Option<DateTime<Utc>>,
    /// Node properties prepared for display.
    pub properties: Value,
    /// Neighboring nodes returned with the node.
    #[serde(default)]
    pub neighbors: Vec<MemoryNeighbor>,
}

/// One neighboring graph-memory node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryNeighbor {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Optional relationship label connecting the neighbor.
    pub relationship: Option<String>,
}

/// One document supplied to graph-memory ingestion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestDocument {
    /// Human-readable source name for the document.
    pub source_name: String,
    /// Source document content to ingest.
    pub content: String,
    /// Optional logical source path or URI for audit trails.
    pub source_uri: Option<String>,
    /// Additional caller-supplied ingestion metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request payload for graph-memory ingestion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestRequest {
    /// Tenant receiving the ingested documents.
    pub tenant_id: TenantId,
    /// Contact owner for contact memory; absent means tenant-owned ingestion.
    pub contact_id: Option<ContactId>,
    /// Operator-authorized information barrier assigned to every document in the request.
    pub information_barrier: Option<InformationBarrierId>,
    /// Documents to ingest.
    #[serde(default)]
    pub documents: Vec<MemoryIngestDocument>,
}

/// Response payload containing graph-memory ingestion results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestResponse {
    /// Tenant that received the ingested documents.
    pub tenant_id: TenantId,
    /// Per-document ingestion results.
    #[serde(default)]
    pub results: Vec<MemoryIngestResult>,
}

/// Per-document graph-memory ingestion result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestResult {
    /// Human-readable source name for the document.
    pub source_name: String,
    /// Number of graph nodes inserted.
    pub inserted: u64,
    /// Number of graph nodes superseded.
    pub superseded: u64,
    /// Number of graph nodes skipped.
    pub skipped: u64,
    /// Number of re-observed facts that reinforced an existing node.
    #[serde(default)]
    pub reinforced: u64,
    /// Number of graph nodes that failed ingestion.
    pub failed: u64,
    /// Number of graph edges inserted.
    pub edges: u64,
    /// Number of contradictions detected.
    pub contradictions: u64,
    /// Whether this document produced dead-letter work.
    pub dead_lettered: bool,
}

/// Request payload for detailed memory retrieval debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRetrieveDebugRequest {
    /// Persisted session whose pinned scope and agent policy authorize retrieval.
    pub session_id: SessionId,
    /// Search query text.
    pub query: String,
    /// Maximum number of hits to return.
    pub limit: u32,
    /// Whether the server should skip durable lineage flushing.
    #[serde(default)]
    pub no_flush_wait: bool,
}

/// Response payload for detailed memory retrieval debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrieveDebugResponse {
    /// Query text that produced these hits.
    pub query: String,
    /// Whether lineage capture was enabled during retrieval.
    pub lineage_enabled: bool,
    /// Whether durable lineage flushing was skipped.
    pub no_flush_wait: bool,
    /// Turn identifier for the debug lineage record, when one was emitted.
    pub lineage_turn: Option<Uuid>,
    /// Seed node UIDs used by the hybrid retrieval request.
    #[serde(default)]
    pub seed_uids: Vec<Uuid>,
    /// Memory hits ordered by rank.
    #[serde(default)]
    pub hits: Vec<MemoryHit>,
    /// Additional backend-specific retrieval diagnostics.
    #[serde(default)]
    pub diagnostics: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieval_requests_reject_legacy_self_asserted_authorization_fields() {
        // Pins: public callers can identify only a persisted session; tenant,
        // scope, clearance policy, and audit operation identity are server-owned.
        let error = serde_json::from_value::<MemorySearchRequest>(serde_json::json!({
            "session_id": Uuid::now_v7(),
            "query": "restricted deal status",
            "limit": 5,
            "tenant_id": Uuid::now_v7(),
            "contact_id": Uuid::now_v7(),
            "retrieval_operation_id": Uuid::now_v7(),
            "information_barrier_policy": {
                "revision": "forged",
                "clearances": ["deal-alpha"]
            }
        }))
        .expect_err("legacy caller-owned authorization fields must be rejected");

        assert!(
            error.to_string().contains("unknown field"),
            "strict wire decoding should reject self-asserted policy: {error}"
        );
    }
}

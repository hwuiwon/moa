//! Graph delta types emitted by tenant knowledge ingestion.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::{DocumentVersion, KnowledgeChunk, KnowledgeObject};

/// Tenant knowledge graph write set independent of a concrete graph store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct KnowledgeGraphDelta {
    /// Nodes to upsert.
    #[serde(default)]
    pub nodes: Vec<GraphNodeUpsert>,
    /// Edges to upsert.
    #[serde(default)]
    pub edges: Vec<GraphEdgeUpsert>,
}

/// Graph node upsert request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNodeUpsert {
    /// Stable node label.
    pub label: String,
    /// Stable external key.
    pub key: String,
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Compact graph properties.
    #[serde(default)]
    pub properties: Value,
    /// Text to embed for retrieval, when this node should have a vector row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_text: Option<String>,
    /// Optional extraction confidence for graph storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// Graph edge upsert request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdgeUpsert {
    /// Source node key.
    pub from_key: String,
    /// Target node key.
    pub to_key: String,
    /// Stable graph edge UID.
    pub uid: Uuid,
    /// Relationship type.
    pub relationship: String,
    /// Compact graph properties.
    #[serde(default)]
    pub properties: Value,
}

/// Builds a compact document and chunk graph delta.
#[must_use]
pub fn document_chunk_delta(
    object: &KnowledgeObject,
    version: &DocumentVersion,
    chunks: &[KnowledgeChunk],
) -> KnowledgeGraphDelta {
    let source_key = format!("source:{}:{}", object.connection_uid, object.source_id);
    let document_key = format!("document:{}", version.version_uid);
    let mut delta = KnowledgeGraphDelta {
        nodes: vec![
            GraphNodeUpsert {
                label: "Source".to_string(),
                key: source_key.clone(),
                uid: stable_uid(&source_key),
                properties: serde_json::json!({
                    "connection_uid": object.connection_uid,
                    "object_type": object.object_type,
                    "title": object.title,
                }),
                embedding_text: None,
                confidence: None,
            },
            GraphNodeUpsert {
                label: "Document".to_string(),
                key: document_key.clone(),
                uid: stable_uid(&document_key),
                properties: serde_json::json!({
                    "object_uid": object.object_uid,
                    "version_uid": version.version_uid,
                    "source_id": object.source_id,
                    "title": object.title,
                    "content_hash": version.content_hash,
                }),
                embedding_text: None,
                confidence: None,
            },
        ],
        edges: vec![GraphEdgeUpsert {
            from_key: source_key,
            to_key: document_key.clone(),
            uid: stable_uid(&format!("edge:contains:{document_key}")),
            relationship: "HAS_DOCUMENT".to_string(),
            properties: serde_json::json!({}),
        }],
    };
    for chunk in chunks {
        let chunk_key = chunk_occurrence_key(chunk);
        delta.nodes.push(GraphNodeUpsert {
            label: "Chunk".to_string(),
            key: chunk_key.clone(),
            uid: chunk.chunk_uid,
            properties: serde_json::json!({
                "chunk_hash": chunk.chunk_hash,
                "version_uid": chunk.version_uid,
                "ordinal": chunk.ordinal,
                "token_count": chunk.token_count,
                "heading_path": chunk.heading_path,
            }),
            embedding_text: Some(chunk.text.clone()),
            confidence: None,
        });
        delta.edges.push(GraphEdgeUpsert {
            from_key: document_key.clone(),
            to_key: chunk_key.clone(),
            uid: stable_uid(&format!("edge:contains:{document_key}:{chunk_key}")),
            relationship: "HAS_CHUNK".to_string(),
            properties: serde_json::json!({}),
        });
        for entity in deterministic_entities(object, chunk) {
            let entity_key = format!("entity:{}:{}", object.tenant_id, stable_slug(&entity));
            delta.nodes.push(GraphNodeUpsert {
                label: "Entity".to_string(),
                key: entity_key.clone(),
                uid: stable_uid(&entity_key),
                properties: serde_json::json!({
                    "name": entity,
                    "source": "knowledge_deterministic",
                }),
                embedding_text: None,
                confidence: None,
            });
            delta.edges.push(GraphEdgeUpsert {
                from_key: chunk_key.clone(),
                to_key: entity_key.clone(),
                uid: stable_uid(&format!("edge:mentions:{chunk_key}:{entity_key}")),
                relationship: "MENTIONS".to_string(),
                properties: serde_json::json!({}),
            });
        }
        if let Some(fact) = deterministic_fact(chunk) {
            let fact_key = format!(
                "fact:{}:{}:{}",
                object.tenant_id,
                chunk.chunk_hash,
                stable_slug(&fact)
            );
            delta.nodes.push(GraphNodeUpsert {
                label: "Fact".to_string(),
                key: fact_key.clone(),
                uid: stable_uid(&fact_key),
                properties: serde_json::json!({
                    "statement": fact,
                    "source": "knowledge_deterministic",
                    "chunk_hash": chunk.chunk_hash,
                }),
                embedding_text: None,
                confidence: None,
            });
            delta.edges.push(GraphEdgeUpsert {
                from_key: chunk_key.clone(),
                to_key: fact_key.clone(),
                uid: stable_uid(&format!("edge:evidences:{chunk_key}:{fact_key}")),
                relationship: "EVIDENCES".to_string(),
                properties: serde_json::json!({}),
            });
        }
    }
    delta
}

/// Returns the graph delta key for one chunk occurrence.
///
/// The key is derived from `chunk_uid` — the occurrence identity — so the graph
/// node written for a chunk belongs to exactly one document version. Content
/// hashes stay in node properties for dedupe and diffing; they never form
/// identity.
fn chunk_occurrence_key(chunk: &KnowledgeChunk) -> String {
    format!("chunk:{}", chunk.chunk_uid)
}

/// Returns a stable UID for graph nodes, edges, and document versions.
#[must_use]
pub fn stable_uid(seed: &str) -> Uuid {
    let hash = blake3::hash(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[0..16]);
    Uuid::from_bytes(bytes)
}

fn deterministic_entities(object: &KnowledgeObject, chunk: &KnowledgeChunk) -> Vec<String> {
    let mut entities = Vec::new();
    if let Some(title) = object
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        entities.push(title.trim().to_string());
    }
    for heading in &chunk.heading_path {
        let heading = heading.trim();
        if !heading.is_empty() && !entities.iter().any(|entity| entity == heading) {
            entities.push(heading.to_string());
        }
    }
    entities
}

fn deterministic_fact(chunk: &KnowledgeChunk) -> Option<String> {
    chunk
        .text
        .split(['.', '\n'])
        .map(str::trim)
        .find(|sentence| {
            let lower = sentence.to_ascii_lowercase();
            sentence.len() >= 12 && (sentence.contains(':') || lower.contains(" is "))
        })
        .map(ToString::to_string)
}

fn stable_slug(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = normalized.trim_matches('-');
    if trimmed.is_empty() {
        stable_uid(value).to_string()
    } else {
        trimmed.to_string()
    }
}

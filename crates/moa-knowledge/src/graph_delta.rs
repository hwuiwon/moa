//! Graph delta types emitted by tenant knowledge ingestion.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    domain::{DocumentVersion, KnowledgeChunk, KnowledgeObject},
    semantic_graph::{SemanticGraphExtraction, SemanticRelationKind},
};

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

/// Builds a compact document and chunk graph delta with semantic graph facts.
#[must_use]
pub fn document_chunk_delta_with_semantics(
    object: &KnowledgeObject,
    version: &DocumentVersion,
    chunks: &[KnowledgeChunk],
    semantic_extractions: &[SemanticGraphExtraction],
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
        let chunk_key = format!("chunk:{}:{}", object.tenant_id, chunk.chunk_hash);
        delta.nodes.push(GraphNodeUpsert {
            label: "Chunk".to_string(),
            key: chunk_key.clone(),
            uid: stable_uid(&chunk_key),
            properties: serde_json::json!({
                "chunk_hash": chunk.chunk_hash,
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
    append_semantic_graph(object, chunks, semantic_extractions, &mut delta);
    delta
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

fn append_semantic_graph(
    object: &KnowledgeObject,
    chunks: &[KnowledgeChunk],
    semantic_extractions: &[SemanticGraphExtraction],
    delta: &mut KnowledgeGraphDelta,
) {
    let extraction_by_chunk = semantic_extractions
        .iter()
        .map(|extraction| (extraction.chunk_hash.as_str(), extraction))
        .collect::<BTreeMap<_, _>>();
    let mut entity_key_by_slug = BTreeMap::<String, String>::new();
    let mut semantic_chunks_by_entity = BTreeMap::<String, Vec<(&KnowledgeChunk, f64)>>::new();

    for chunk in chunks {
        let Some(extraction) = extraction_by_chunk.get(chunk.chunk_hash.as_str()).copied() else {
            continue;
        };
        let chunk_key = format!("chunk:{}:{}", object.tenant_id, chunk.chunk_hash);
        for entity in &extraction.entities {
            let entity_key = format!(
                "semantic-entity:{}:{}",
                object.tenant_id, entity.canonical_slug
            );
            entity_key_by_slug
                .entry(entity.canonical_slug.clone())
                .or_insert_with(|| entity_key.clone());
            delta.nodes.push(GraphNodeUpsert {
                label: "Entity".to_string(),
                key: entity_key.clone(),
                uid: stable_uid(&entity_key),
                properties: serde_json::json!({
                    "name": entity.canonical_name,
                    "entity_kind": entity.kind.as_str(),
                    "aliases": entity.aliases,
                    "source": "semantic_graph_extraction",
                    "semantic_graph": true,
                    "schema_version": extraction.schema_version,
                    "model": extraction.model,
                    "prompt_version": extraction.prompt_version,
                    "confidence": entity.confidence,
                    "evidence": entity.evidence,
                }),
                embedding_text: None,
                confidence: Some(entity.confidence),
            });
            delta.edges.push(GraphEdgeUpsert {
                from_key: chunk_key.clone(),
                to_key: entity_key.clone(),
                uid: stable_uid(&format!(
                    "edge:semantic-mentions:{chunk_key}:{entity_key}:{}",
                    extraction.schema_version
                )),
                relationship: "MENTIONS".to_string(),
                properties: serde_json::json!({
                    "semantic_graph": true,
                    "semantic_relation": "mentions",
                    "entity_kind": entity.kind.as_str(),
                    "schema_version": extraction.schema_version,
                    "model": extraction.model,
                    "prompt_version": extraction.prompt_version,
                    "confidence": entity.confidence,
                }),
            });
            if entity.confidence >= 0.72 {
                semantic_chunks_by_entity
                    .entry(entity.canonical_slug.clone())
                    .or_default()
                    .push((chunk, entity.confidence));
            }
        }

        for relation in &extraction.relations {
            let Some(from_key) = entity_key_by_slug.get(&relation.from_slug).cloned() else {
                continue;
            };
            let Some(to_key) = entity_key_by_slug.get(&relation.to_slug).cloned() else {
                continue;
            };
            delta.edges.push(GraphEdgeUpsert {
                from_key: from_key.clone(),
                to_key: to_key.clone(),
                uid: stable_uid(&format!(
                    "edge:semantic-relation:{from_key}:{to_key}:{}:{}",
                    relation.kind.as_str(),
                    extraction.schema_version
                )),
                relationship: relation.kind.graph_relationship().to_string(),
                properties: serde_json::json!({
                    "semantic_graph": true,
                    "semantic_relation": relation.kind.as_str(),
                    "schema_version": extraction.schema_version,
                    "model": extraction.model,
                    "prompt_version": extraction.prompt_version,
                    "confidence": relation.confidence,
                    "evidence": relation.evidence,
                }),
            });
        }
    }

    append_same_document_semantic_chunk_links(object, &semantic_chunks_by_entity, delta);
}

fn append_same_document_semantic_chunk_links(
    object: &KnowledgeObject,
    semantic_chunks_by_entity: &BTreeMap<String, Vec<(&KnowledgeChunk, f64)>>,
    delta: &mut KnowledgeGraphDelta,
) {
    for (entity_slug, linked_chunks) in semantic_chunks_by_entity {
        let mut linked_chunks = linked_chunks.clone();
        linked_chunks.sort_by_key(|(chunk, _)| (chunk.ordinal, chunk.chunk_hash.clone()));
        linked_chunks.dedup_by(|left, right| left.0.chunk_hash == right.0.chunk_hash);
        if linked_chunks.len() < 2 {
            continue;
        }
        for window in linked_chunks.windows(2) {
            let [(from, from_confidence), (to, to_confidence)] = window else {
                continue;
            };
            append_semantic_chunk_link(
                object,
                from,
                to,
                entity_slug,
                (*from_confidence).min(*to_confidence),
                delta,
            );
            append_semantic_chunk_link(
                object,
                to,
                from,
                entity_slug,
                (*from_confidence).min(*to_confidence),
                delta,
            );
        }
    }
}

fn append_semantic_chunk_link(
    object: &KnowledgeObject,
    from: &KnowledgeChunk,
    to: &KnowledgeChunk,
    entity_slug: &str,
    confidence: f64,
    delta: &mut KnowledgeGraphDelta,
) {
    let from_key = format!("chunk:{}:{}", object.tenant_id, from.chunk_hash);
    let to_key = format!("chunk:{}:{}", object.tenant_id, to.chunk_hash);
    delta.edges.push(GraphEdgeUpsert {
        from_key: from_key.clone(),
        to_key: to_key.clone(),
        uid: stable_uid(&format!(
            "edge:semantic-shared-entity:{entity_slug}:{from_key}:{to_key}"
        )),
        relationship: SemanticRelationKind::Mentions
            .graph_relationship()
            .to_string(),
        properties: serde_json::json!({
            "semantic_graph": true,
            "semantic_relation": "shared_entity",
            "entity_slug": entity_slug,
            "confidence": confidence,
            "scope": "same_document",
        }),
    });
}

/// Counts same-document semantic chunk links that would be emitted for reports.
#[must_use]
pub fn semantic_chunk_link_count(
    chunks: &[KnowledgeChunk],
    semantic_extractions: &[SemanticGraphExtraction],
) -> usize {
    let extraction_by_chunk = semantic_extractions
        .iter()
        .map(|extraction| (extraction.chunk_hash.as_str(), extraction))
        .collect::<BTreeMap<_, _>>();
    let mut semantic_chunks_by_entity = BTreeMap::<String, Vec<&KnowledgeChunk>>::new();
    for chunk in chunks {
        let Some(extraction) = extraction_by_chunk.get(chunk.chunk_hash.as_str()).copied() else {
            continue;
        };
        for entity in &extraction.entities {
            if entity.confidence >= 0.72 {
                semantic_chunks_by_entity
                    .entry(entity.canonical_slug.clone())
                    .or_default()
                    .push(chunk);
            }
        }
    }
    semantic_chunks_by_entity
        .into_values()
        .map(|mut linked_chunks| {
            linked_chunks.sort_by_key(|chunk| (chunk.ordinal, chunk.chunk_hash.clone()));
            linked_chunks.dedup_by(|left, right| left.chunk_hash == right.chunk_hash);
            linked_chunks.len().saturating_sub(1) * 2
        })
        .sum()
}

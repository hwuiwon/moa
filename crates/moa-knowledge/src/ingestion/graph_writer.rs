//! Graph and vector persistence for tenant knowledge ingestion.

use super::*;
use moa_core::types::memory::InformationBarrierId;

/// Graph write report returned by the tenant-knowledge graph sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphWriteReport {
    /// Number of graph nodes created or updated.
    pub nodes_upserted: u64,
    /// Number of graph edges created or updated.
    pub edges_upserted: u64,
    /// Number of vector rows deleted while invalidating old chunks.
    pub vector_rows_deleted: u64,
}

/// Graph and vector write seam used by tenant knowledge ingestion.
#[async_trait]
pub trait KnowledgeGraphWriter: Send + Sync {
    /// Applies a graph delta and embeds the supplied node UIDs.
    async fn upsert_delta(
        &self,
        delta: &KnowledgeGraphDelta,
        embeddings: &HashMap<Uuid, Vec<f32>>,
        embedding_model: &str,
        embedding_model_version: i32,
    ) -> Result<GraphWriteReport>;

    /// Invalidates active chunk graph nodes and removes their vector rows.
    async fn invalidate_chunks(&self, graph_node_uids: &[Uuid]) -> Result<GraphWriteReport>;
}

/// `moa-memory-graph` backed tenant knowledge graph writer.
pub struct MemoryKnowledgeGraphWriter<G> {
    graph: Arc<G>,
    scope: MemoryScope,
    actor_id: String,
    information_barrier: Option<InformationBarrierId>,
}

impl<G> MemoryKnowledgeGraphWriter<G> {
    /// Creates a graph writer using an existing scoped graph store.
    #[must_use]
    pub fn new(
        graph: Arc<G>,
        scope: MemoryScope,
        actor_id: impl Into<String>,
        information_barrier: Option<InformationBarrierId>,
    ) -> Self {
        Self {
            graph,
            scope,
            actor_id: actor_id.into(),
            information_barrier,
        }
    }
}

#[async_trait]
impl<G> KnowledgeGraphWriter for MemoryKnowledgeGraphWriter<G>
where
    G: GraphStore,
{
    async fn upsert_delta(
        &self,
        delta: &KnowledgeGraphDelta,
        embeddings: &HashMap<Uuid, Vec<f32>>,
        embedding_model: &str,
        embedding_model_version: i32,
    ) -> Result<GraphWriteReport> {
        let mut report = GraphWriteReport::default();
        let mut key_to_uid = HashMap::new();
        let mut seen_node_uids = HashSet::new();
        let mut unique_nodes = Vec::new();
        for node in &delta.nodes {
            key_to_uid.insert(node.key.clone(), node.uid);
            if seen_node_uids.insert(node.uid) {
                unique_nodes.push(node);
            }
        }

        // Resolve which nodes already exist in one lookup instead of an
        // N+1 `get_node` loop, then reactivate (hard-purge) any that were
        // invalidated and create the rest with a single batched write. Active
        // existing nodes are left untouched, matching the previous per-node
        // create-or-skip behavior.
        let unique_uids = unique_nodes.iter().map(|node| node.uid).collect::<Vec<_>>();
        let existing_by_uid = self
            .graph
            .bulk_get_nodes(&unique_uids)
            .await
            .map_err(map_graph_error)?
            .into_iter()
            .map(|row| (row.uid, row))
            .collect::<HashMap<_, _>>();
        let mut create_intents = Vec::new();
        for node in unique_nodes {
            if let Some(existing) = existing_by_uid.get(&node.uid) {
                if existing.valid_to.is_none() {
                    continue;
                }
                self.graph
                    .hard_purge(node.uid, "knowledge_node_reactivated")
                    .await
                    .map_err(map_graph_error)?;
            }
            let properties = compact_properties(node.properties.clone());
            let embedding = embeddings.get(&node.uid).cloned();
            let embedding_text = embedding.as_ref().and_then(|_| node.embedding_text.clone());
            create_intents.push(NodeWriteIntent {
                barrier: self.information_barrier.clone(),
                uid: node.uid,
                data_subject_id: self.scope.tenant_id().0,
                label: node_label(&node.label)?,
                storage_partition_id: Some(self.scope.tenant_id().0.to_string()),
                contact_id: None,
                scope: "tenant".to_string(),
                name: node_name(&node.label, &properties),
                properties,
                pii_class: SensitivityClass::None,
                confidence: Some(node.confidence.unwrap_or(0.95)),
                valid_from: Utc::now(),
                embedding,
                embedding_model: embeddings
                    .contains_key(&node.uid)
                    .then(|| embedding_model.to_string()),
                embedding_model_version: embeddings
                    .contains_key(&node.uid)
                    .then_some(embedding_model_version),
                embedding_text,
                actor_id: self.actor_id.clone(),
                actor_kind: "system".to_string(),
            });
        }
        let created = create_intents.len() as u64;
        self.graph
            .bulk_create_nodes(create_intents)
            .await
            .map_err(map_graph_error)?;
        report.nodes_upserted = report.nodes_upserted.saturating_add(created);

        let mut seen_edge_uids = HashSet::new();
        for edge in &delta.edges {
            if !seen_edge_uids.insert(edge.uid) {
                continue;
            }
            let Some(start_uid) = key_to_uid.get(&edge.from_key).copied() else {
                continue;
            };
            let Some(end_uid) = key_to_uid.get(&edge.to_key).copied() else {
                continue;
            };
            self.graph
                .create_edge(edge_intent(
                    edge,
                    start_uid,
                    end_uid,
                    self.scope.tenant_id().0.to_string(),
                    &self.actor_id,
                )?)
                .await
                .map_err(map_graph_error)?;
            report.edges_upserted = report.edges_upserted.saturating_add(1);
        }
        Ok(report)
    }

    async fn invalidate_chunks(&self, graph_node_uids: &[Uuid]) -> Result<GraphWriteReport> {
        let mut report = GraphWriteReport::default();
        if graph_node_uids.is_empty() {
            return Ok(report);
        }
        // Resolve existence in one lookup rather than an N+1 `get_node` loop, then
        // invalidate each existing node individually so the per-node changelog and
        // already-invalidated error semantics are preserved.
        let existing_uids = self
            .graph
            .bulk_get_nodes(graph_node_uids)
            .await
            .map_err(map_graph_error)?
            .into_iter()
            .map(|row| row.uid)
            .collect::<HashSet<_>>();
        for uid in graph_node_uids {
            if existing_uids.contains(uid) {
                self.graph
                    .invalidate_node(*uid, "knowledge_chunk_orphaned")
                    .await
                    .map_err(map_graph_error)?;
                report.vector_rows_deleted = report.vector_rows_deleted.saturating_add(1);
            }
        }
        Ok(report)
    }
}

fn node_label(label: &str) -> Result<NodeLabel> {
    match label {
        "Source" => Ok(NodeLabel::Source),
        "Document" => Ok(NodeLabel::Document),
        "Chunk" => Ok(NodeLabel::Chunk),
        "Fact" => Ok(NodeLabel::Fact),
        "Entity" => Ok(NodeLabel::Entity),
        other => Err(Error::Repository(format!(
            "unsupported knowledge graph node label `{other}`"
        ))),
    }
}

fn edge_label(relationship: &str) -> Result<EdgeLabel> {
    match relationship {
        "HAS_DOCUMENT" | "HAS_CHUNK" => Ok(EdgeLabel::Contains),
        "EVIDENCES" | "DERIVED_FROM" => Ok(EdgeLabel::DerivedFrom),
        "MENTIONS" => Ok(EdgeLabel::MentionedIn),
        "RELATES_TO" => Ok(EdgeLabel::RelatesTo),
        "DEPENDS_ON" => Ok(EdgeLabel::DependsOn),
        "CAUSED" => Ok(EdgeLabel::Caused),
        "APPLIES_TO" => Ok(EdgeLabel::AppliesTo),
        other => Err(Error::Repository(format!(
            "unsupported knowledge graph edge relationship `{other}`"
        ))),
    }
}

fn edge_intent(
    edge: &GraphEdgeUpsert,
    start_uid: Uuid,
    end_uid: Uuid,
    storage_partition_id: String,
    actor_id: &str,
) -> Result<EdgeWriteIntent> {
    Ok(EdgeWriteIntent {
        uid: edge.uid,
        label: edge_label(&edge.relationship)?,
        start_uid,
        end_uid,
        valid_from: chrono::Utc::now(),
        properties: compact_properties(edge.properties.clone()),
        storage_partition_id: Some(storage_partition_id),
        contact_id: None,
        scope: "tenant".to_string(),
        actor_id: actor_id.to_string(),
        actor_kind: "system".to_string(),
    })
}

fn node_name(label: &str, properties: &Value) -> String {
    properties
        .get("title")
        .or_else(|| properties.get("name"))
        .or_else(|| properties.get("statement"))
        .or_else(|| properties.get("chunk_hash"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| label.to_string())
}

fn compact_properties(properties: Value) -> Value {
    match properties {
        Value::Object(_) => redact_provider_metadata(properties),
        _ => json!({}),
    }
}

fn map_graph_error(error: moa_memory_graph::Error) -> Error {
    Error::Repository(error.to_string())
}

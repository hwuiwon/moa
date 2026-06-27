//! Tenant knowledge ingestion pipeline from provider records to graph/vector writes.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::traits::EmbeddingProvider;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_types::MemoryScope;
use serde_json::{Value, json};
use tracing::{Instrument, Span};
use uuid::Uuid;

use crate::{
    chunking::{ChunkingConfig, blocks_to_chunks, content_hash, elements_to_blocks},
    contact_groups::derive_contact_groups_from_object,
    domain::{
        DocumentVersion, IngestionStepStatus, KnowledgeChunk, KnowledgeObject,
        KnowledgeSyncCounters, ParsedDocument, ProviderRecord, RecordPage, SyncRunStatus,
    },
    error::{Error, Result},
    graph_delta::{GraphEdgeUpsert, KnowledgeGraphDelta, document_chunk_delta, stable_uid},
    normalize::{normalize_text, redact_provider_metadata},
    observability::{
        FailureClassification, IngestionObserver, StepLabels, StepOutcome, build_step_row,
        classify_failure, failed_outcome,
    },
    parser::DocumentParser,
    repository::KnowledgeRepository,
};

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
}

impl<G> MemoryKnowledgeGraphWriter<G> {
    /// Creates a graph writer using an existing scoped graph store.
    #[must_use]
    pub fn new(graph: Arc<G>, scope: MemoryScope, actor_id: impl Into<String>) -> Self {
        Self {
            graph,
            scope,
            actor_id: actor_id.into(),
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
        for node in &delta.nodes {
            key_to_uid.insert(node.key.clone(), node.uid);
            if !seen_node_uids.insert(node.uid) {
                continue;
            }
            if let Some(existing) = self
                .graph
                .get_node(node.uid)
                .await
                .map_err(map_graph_error)?
            {
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
            self.graph
                .create_node(NodeWriteIntent {
                    uid: node.uid,
                    label: node_label(&node.label)?,
                    storage_partition_id: Some(self.scope.tenant_id().0.to_string()),
                    contact_id: None,
                    scope: "tenant".to_string(),
                    name: node_name(&node.label, &properties),
                    properties,
                    pii_class: PiiClass::None,
                    confidence: Some(0.95),
                    valid_from: Utc::now(),
                    embedding,
                    embedding_model: embeddings
                        .contains_key(&node.uid)
                        .then(|| embedding_model.to_string()),
                    embedding_model_version: embeddings
                        .contains_key(&node.uid)
                        .then_some(embedding_model_version),
                    actor_id: self.actor_id.clone(),
                    actor_kind: "system".to_string(),
                })
                .await
                .map_err(map_graph_error)?;
            report.nodes_upserted = report.nodes_upserted.saturating_add(1);
        }

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
        for uid in graph_node_uids {
            if self
                .graph
                .get_node(*uid)
                .await
                .map_err(map_graph_error)?
                .is_some()
            {
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

/// Summary returned after ingesting one provider page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageIngestionReport {
    /// Number of records listed by the provider page.
    pub records_listed: u64,
    /// Number of changed records ingested.
    pub records_ingested: u64,
    /// Number of records skipped as unchanged.
    pub records_skipped: u64,
    /// Number of provider-deleted records handled.
    pub records_deleted: u64,
    /// Number of new embeddings created.
    pub embeddings_created: u64,
}

/// Dependency-injected ingestion service free of Restate service types.
pub struct KnowledgeIngestionPipeline<R, P, E, G, O> {
    repository: Arc<R>,
    parser: Arc<P>,
    embedder: Arc<E>,
    graph: Arc<G>,
    observer: Arc<O>,
    chunking: ChunkingConfig,
    provider: String,
    parser_label: String,
}

/// Static pipeline settings used for chunking and observability labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeIngestionPipelineConfig {
    /// Chunking thresholds for normalized knowledge blocks.
    pub chunking: ChunkingConfig,
    /// Low-cardinality provider label for steps, spans, and metrics.
    pub provider: String,
    /// Low-cardinality parser label for steps, spans, and metrics.
    pub parser_label: String,
}

impl<R, P, E, G, O> KnowledgeIngestionPipeline<R, P, E, G, O>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
    O: IngestionObserver,
{
    /// Creates a knowledge ingestion pipeline from injected dependencies.
    #[must_use]
    pub fn new(
        repository: Arc<R>,
        parser: Arc<P>,
        embedder: Arc<E>,
        graph: Arc<G>,
        observer: Arc<O>,
        config: KnowledgeIngestionPipelineConfig,
    ) -> Self {
        Self {
            repository,
            parser,
            embedder,
            graph,
            observer,
            chunking: config.chunking,
            provider: config.provider,
            parser_label: config.parser_label,
        }
    }

    /// Ingests one provider record page, including change-token checks and deletions.
    #[tracing::instrument(
        name = "knowledge_sync_run",
        skip(self, page),
        fields(
            tenant_id = %tenant_id,
            connection_id = %connection_uid,
            sync_run_id = %sync_run_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            records_listed = page.records.len()
        )
    )]
    pub async fn ingest_record_page(
        &self,
        sync_run_uid: Uuid,
        connection_uid: Uuid,
        tenant_id: moa_core::TenantId,
        page: RecordPage,
    ) -> Result<PageIngestionReport> {
        let records_listed = page.records.len() as u64;
        self.record_step(
            sync_run_uid,
            None,
            "provider_records_listed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "records_listed": records_listed }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;

        let mut report = PageIngestionReport {
            records_listed,
            ..PageIngestionReport::default()
        };
        for record in page.records {
            let object = self.materialize_object(connection_uid, tenant_id, &record);
            if record.deleted {
                let deleted = self
                    .handle_deleted_record(sync_run_uid, object, record)
                    .await?;
                report.records_deleted = report.records_deleted.saturating_add(deleted);
                continue;
            }
            match self.ingest_record(sync_run_uid, object, record).await? {
                RecordIngestionOutcome::Ingested { embeddings_created } => {
                    report.records_ingested = report.records_ingested.saturating_add(1);
                    report.embeddings_created =
                        report.embeddings_created.saturating_add(embeddings_created);
                }
                RecordIngestionOutcome::Skipped => {
                    report.records_skipped = report.records_skipped.saturating_add(1);
                }
            }
        }
        Ok(report)
    }

    /// Tombstones active local objects that were absent from an exhaustive selected-source sync.
    #[tracing::instrument(
        name = "knowledge_source_selection_prune",
        skip(self, seen_source_ids),
        fields(
            tenant_id = %tenant_id,
            connection_id = %connection_uid,
            sync_run_id = %sync_run_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            seen_sources = seen_source_ids.len()
        )
    )]
    pub async fn prune_unseen_objects(
        &self,
        sync_run_uid: Uuid,
        connection_uid: Uuid,
        tenant_id: moa_core::TenantId,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport> {
        let active_objects = self
            .repository
            .active_objects_for_connection(connection_uid)
            .await?;
        let mut report = PageIngestionReport::default();
        for object in active_objects {
            if object.tenant_id != tenant_id || seen_source_ids.contains(&object.source_id) {
                continue;
            }
            let deleted = self.handle_pruned_object(sync_run_uid, object).await?;
            report.records_deleted = report.records_deleted.saturating_add(deleted);
        }
        self.record_counter_step(
            sync_run_uid,
            None,
            "source_selection_pruned",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "records_pruned": report.records_deleted }),
                summary: Some("removed objects absent from selected provider sources".to_string()),
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters::default(),
        )
        .await?;
        Ok(report)
    }

    /// Parses, normalizes, chunks, persists, and writes graph/vector state for one object.
    #[tracing::instrument(
        name = "knowledge_object_ingest",
        skip(self, input),
        fields(
            sync_run_id = %sync_run_uid,
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        )
    )]
    pub async fn ingest_parsed_object(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        input: crate::domain::ParseInput,
    ) -> Result<KnowledgeGraphDelta> {
        self.repository.upsert_object(object.clone()).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_submitted",
            StepOutcome::completed(),
        )
        .await?;
        let parse_span = tracing::info_span!(
            "knowledge_parse_job",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        );
        let parsed = match self
            .parser
            .parse(input)
            .instrument(parse_span.clone())
            .await
        {
            Ok(parsed) => {
                record_span_outcome(&parse_span, "completed", None);
                parsed
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "parse_completed",
                        &error,
                    )
                    .await?;
                record_span_outcome(&parse_span, "failed", Some(classification.error_code));
                return Err(error);
            }
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_completed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "objects_parsed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                objects_parsed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        self.persist_parsed(sync_run_uid, object, parsed, Vec::new())
            .await
            .map(|outcome| outcome.delta)
    }

    async fn ingest_record(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        record: ProviderRecord,
    ) -> Result<RecordIngestionOutcome> {
        if let Some(existing) = self
            .repository
            .get_object_by_source(object.connection_uid, &object.source_id)
            .await?
            && existing.status == crate::domain::ObjectStatus::Active
            && existing.change_token.is_some()
            && existing.change_token == object.change_token
            && self
                .record_has_completed_ingestion(&existing, object.clone(), &record)
                .await?
        {
            self.record_counter_step(
                sync_run_uid,
                Some(existing.object_uid),
                "object_change_checked",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({ "records_seen": 1, "records_changed": 0 }),
                    summary: Some("change token unchanged".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
                KnowledgeSyncCounters {
                    records_seen: 1,
                    ..KnowledgeSyncCounters::default()
                },
            )
            .await?;
            return Ok(RecordIngestionOutcome::Skipped);
        }

        self.repository.upsert_object(object.clone()).await?;
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "object_change_checked",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "records_seen": 1, "records_changed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let input = match self.parse_input_from_record(object.clone(), &record) {
            Ok(input) => input,
            Err(error) => {
                self.record_failure_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "content_fetched",
                    &error,
                )
                .await?;
                return Err(error);
            }
        };
        let bytes_fetched = input.bytes.as_ref().map_or_else(
            || input.text.as_ref().map_or(0, |text| text.len()),
            Vec::len,
        );
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "content_fetched",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "bytes_fetched": bytes_fetched }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_submitted",
            StepOutcome::completed(),
        )
        .await?;
        let parse_span = tracing::info_span!(
            "knowledge_parse_job",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        );
        let parsed = match self
            .parser
            .parse(input)
            .instrument(parse_span.clone())
            .await
        {
            Ok(parsed) => {
                record_span_outcome(&parse_span, "completed", None);
                parsed
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "parse_completed",
                        &error,
                    )
                    .await?;
                record_span_outcome(&parse_span, "failed", Some(classification.error_code));
                return Err(error);
            }
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "parse_completed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "parser_items": parsed.elements.len(), "objects_parsed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                objects_parsed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        let outcome = self
            .persist_parsed(sync_run_uid, object, parsed, Vec::new())
            .await?;
        Ok(RecordIngestionOutcome::Ingested {
            embeddings_created: outcome.embeddings_created,
        })
    }

    async fn persist_parsed(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        parsed: ParsedDocument,
        deleted_chunk_uids: Vec<Uuid>,
    ) -> Result<PersistedIngestion> {
        let content_hash = content_hash(&normalize_text(&parsed.text));
        let latest_version = self
            .repository
            .latest_document_version(object.object_uid)
            .await?;
        let latest_chunks = if let Some(latest) = &latest_version {
            self.repository
                .chunks_for_version(latest.version_uid)
                .await?
        } else {
            Vec::new()
        };
        let latest_version_completed = if let Some(latest) = &latest_version {
            latest.content_hash == content_hash
                && self
                    .repository
                    .object_ingestion_completed_since(object.object_uid, latest.created_at)
                    .await?
        } else {
            false
        };
        if let Some(latest) = &latest_version
            && latest.content_hash == content_hash
            && !latest_chunks.is_empty()
            && latest_version_completed
        {
            self.record_step(
                sync_run_uid,
                Some(object.object_uid),
                "normalized",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({ "blocks_total": 0, "chunks_total": latest_chunks.len() }),
                    summary: Some("content hash unchanged".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
            )
            .await?;
            return Ok(PersistedIngestion {
                delta: KnowledgeGraphDelta::default(),
                embeddings_created: 0,
            });
        }

        let previous_chunks = if latest_version_completed {
            latest_chunks
        } else if latest_version
            .as_ref()
            .is_some_and(|latest| latest.content_hash == content_hash)
        {
            Vec::new()
        } else {
            latest_chunks
        };
        let version = if let Some(latest) = latest_version
            && latest.content_hash == content_hash
        {
            latest
        } else {
            let version = DocumentVersion {
                version_uid: stable_uid(&format!("version:{}:{content_hash}", object.object_uid)),
                object_uid: object.object_uid,
                parser: parsed.parser,
                parser_job_id: parsed.parser_job_id,
                content_hash,
                metadata: parsed.metadata,
                created_at: Utc::now(),
            };
            self.repository
                .insert_document_version(version.clone())
                .await?;
            version
        };
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "normalized",
            StepOutcome::completed(),
        )
        .await?;

        let blocks = elements_to_blocks(version.version_uid, &parsed.elements);
        self.repository
            .replace_blocks(version.version_uid, blocks.clone())
            .await?;
        let old_block_hashes = previous_chunks
            .iter()
            .flat_map(|chunk| chunk.block_hashes.iter())
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let blocks_new = blocks
            .iter()
            .filter(|block| !old_block_hashes.contains(&block.block_hash))
            .count();
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "blocks_diffed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "blocks_total": blocks.len(),
                    "blocks_new": blocks_new,
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;

        let chunks = blocks_to_chunks(version.version_uid, &blocks, self.chunking);
        self.repository
            .replace_chunks(version.version_uid, chunks.clone())
            .await?;
        let old_by_hash = previous_chunks
            .iter()
            .map(|chunk| (chunk.chunk_hash.clone(), chunk.clone()))
            .collect::<HashMap<_, _>>();
        let chunks_new = chunks
            .iter()
            .filter(|chunk| !old_by_hash.contains_key(&chunk.chunk_hash))
            .count();
        let new_hashes = chunks
            .iter()
            .map(|chunk| chunk.chunk_hash.as_str())
            .collect::<std::collections::HashSet<_>>();
        let orphan_chunks = previous_chunks
            .iter()
            .filter(|chunk| !new_hashes.contains(chunk.chunk_hash.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "chunks_diffed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "chunks_total": chunks.len(),
                    "chunks_new": chunks_new,
                    "chunks_deleted": orphan_chunks.len() + deleted_chunk_uids.len(),
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;

        let delta = document_chunk_delta(&object, &version, &chunks);
        let mut embedding_inputs = Vec::new();
        let mut embedding_uids = Vec::new();
        for chunk in &chunks {
            let graph_uid = chunk_graph_uid(&delta, object.tenant_id, chunk)?;
            self.repository
                .set_chunk_graph_uid(chunk.chunk_uid, graph_uid)
                .await?;
            if !old_by_hash.contains_key(&chunk.chunk_hash) {
                embedding_uids.push(graph_uid);
                embedding_inputs.push(chunk.text.clone());
            }
        }
        let embeddings = if embedding_inputs.is_empty() {
            HashMap::new()
        } else {
            let vectors = match self.embedder.embed(&embedding_inputs).await {
                Ok(vectors) => vectors,
                Err(error) => {
                    let error = Error::Repository(format!("embedding failed: {error}"));
                    self.record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "embedded",
                        &error,
                    )
                    .await?;
                    return Err(error);
                }
            };
            embedding_uids
                .into_iter()
                .zip(vectors)
                .collect::<HashMap<_, _>>()
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "embedded",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "embeddings_created": embeddings.len(),
                    "embeddings_reused": chunks.len().saturating_sub(embeddings.len()),
                    "chunks_embedded": embeddings.len(),
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                chunks_embedded: embeddings.len() as u64,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let graph_span = tracing::info_span!(
            "knowledge_graph_write",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            kind = "upsert",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            graph_node_count = delta.nodes.len(),
            graph_edge_count = delta.edges.len()
        );
        let graph_report = match self
            .graph
            .upsert_delta(
                &delta,
                &embeddings,
                self.embedder.model_name(),
                self.embedder.model_version(),
            )
            .instrument(graph_span.clone())
            .await
        {
            Ok(report) => {
                record_span_outcome(&graph_span, "completed", None);
                report
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "graph_upserted",
                        &error,
                    )
                    .await?;
                record_span_outcome(&graph_span, "failed", Some(classification.error_code));
                return Err(error);
            }
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "graph_upserted",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "graph_nodes_upserted": graph_report.nodes_upserted,
                    "graph_edges_upserted": graph_report.edges_upserted,
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                graph_nodes_upserted: graph_report.nodes_upserted,
                graph_edges_upserted: graph_report.edges_upserted,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let orphan_uids = orphan_chunks
            .iter()
            .filter_map(|chunk| old_by_hash.get(&chunk.chunk_hash))
            .map(|chunk| stable_uid(&format!("chunk:{}:{}", object.tenant_id, chunk.chunk_hash)))
            .collect::<Vec<_>>();
        let invalidation_span = tracing::info_span!(
            "knowledge_graph_write",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            kind = "invalidate",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            graph_node_count = orphan_uids.len(),
            graph_edge_count = 0
        );
        let invalidation_report = match self
            .graph
            .invalidate_chunks(&orphan_uids)
            .instrument(invalidation_span.clone())
            .await
        {
            Ok(report) => {
                record_span_outcome(&invalidation_span, "completed", None);
                report
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "vector_indexed",
                        &error,
                    )
                    .await?;
                record_span_outcome(
                    &invalidation_span,
                    "failed",
                    Some(classification.error_code),
                );
                return Err(error);
            }
        };
        let tombstones = orphan_chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
            .chain(deleted_chunk_uids)
            .collect::<Vec<_>>();
        self.repository.tombstone_chunks(&tombstones).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "vector_rows_upserted": embeddings.len(),
                    "vector_rows_deleted": invalidation_report.vector_rows_deleted,
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;
        let contact_delta = derive_contact_groups_from_object(&object);
        let contact_group_count = contact_delta.groups.len();
        let mut contact_memberships = 0_u64;
        for group in contact_delta.groups {
            let group_uid = group.group_uid;
            self.repository.upsert_contact_group(group).await?;
            let memberships = contact_delta
                .memberships
                .iter()
                .filter(|membership| membership.group_uid == group_uid)
                .cloned()
                .collect::<Vec<_>>();
            contact_memberships = contact_memberships.saturating_add(memberships.len() as u64);
            self.repository
                .replace_contact_group_memberships(group_uid, memberships)
                .await?;
        }
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "contact_groups_derived",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({
                    "contact_groups": contact_group_count,
                    "contact_group_memberships_changed": contact_memberships,
                    "records_ingested": 1,
                }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            KnowledgeSyncCounters {
                records_ingested: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        Ok(PersistedIngestion {
            delta,
            embeddings_created: embeddings.len() as u64,
        })
    }

    async fn handle_deleted_record(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        _record: ProviderRecord,
    ) -> Result<u64> {
        self.repository.upsert_object(object.clone()).await?;
        self.delete_object(
            sync_run_uid,
            object,
            json!({ "records_seen": 1, "records_deleted": 1 }),
            Some("provider record deleted".to_string()),
            KnowledgeSyncCounters {
                records_seen: 1,
                records_deleted: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
    }

    async fn handle_pruned_object(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
    ) -> Result<u64> {
        self.delete_object(
            sync_run_uid,
            object,
            json!({ "records_deleted": 1, "records_pruned": 1 }),
            Some("provider record absent from selected sources".to_string()),
            KnowledgeSyncCounters {
                records_deleted: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
    }

    async fn delete_object(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        counters: Value,
        summary: Option<String>,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<u64> {
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "object_change_checked",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters,
                summary,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
            counter_delta,
        )
        .await?;
        self.repository
            .mark_object_deleted(object.object_uid, Utc::now())
            .await?;
        let latest = self
            .repository
            .latest_document_version(object.object_uid)
            .await?;
        let chunks = if let Some(version) = latest {
            self.repository
                .chunks_for_version(version.version_uid)
                .await?
        } else {
            Vec::new()
        };
        let graph_uids = chunks
            .iter()
            .map(|chunk| stable_uid(&format!("chunk:{}:{}", object.tenant_id, chunk.chunk_hash)))
            .collect::<Vec<_>>();
        let invalidation_span = tracing::info_span!(
            "knowledge_graph_write",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            kind = "invalidate",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            graph_node_count = graph_uids.len(),
            graph_edge_count = 0
        );
        let invalidation_report = match self
            .graph
            .invalidate_chunks(&graph_uids)
            .instrument(invalidation_span.clone())
            .await
        {
            Ok(report) => {
                record_span_outcome(&invalidation_span, "completed", None);
                report
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "vector_indexed",
                        &error,
                    )
                    .await?;
                record_span_outcome(
                    &invalidation_span,
                    "failed",
                    Some(classification.error_code),
                );
                return Err(error);
            }
        };
        let chunk_uids = chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
            .collect::<Vec<_>>();
        self.repository.tombstone_chunks(&chunk_uids).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "chunks_diffed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "chunks_deleted": chunk_uids.len() }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "vector_rows_deleted": invalidation_report.vector_rows_deleted }),
                summary: None,
                retry_count: 0,
                error_code: None,
                duration_ms: None,
            },
        )
        .await?;
        Ok(1)
    }

    fn materialize_object(
        &self,
        connection_uid: Uuid,
        tenant_id: moa_core::TenantId,
        record: &ProviderRecord,
    ) -> KnowledgeObject {
        KnowledgeObject {
            object_uid: stable_uid(&format!(
                "knowledge-object:{connection_uid}:{}",
                record.source_id
            )),
            tenant_id,
            connection_uid,
            object_type: record.object_type.clone(),
            source_id: record.source_id.clone(),
            parent_source_id: None,
            source_uri: record.source_uri.clone(),
            title: record.title.clone(),
            change_token: record.change_token.clone(),
            metadata: redact_provider_metadata(record.metadata.clone()),
            status: if record.deleted {
                crate::domain::ObjectStatus::Deleted
            } else {
                crate::domain::ObjectStatus::Active
            },
            source_updated_at: record.source_updated_at,
            deleted_at: record.deleted.then(Utc::now),
        }
    }

    async fn record_has_completed_ingestion(
        &self,
        existing: &KnowledgeObject,
        incoming: KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<bool> {
        // The object row is advanced before parse and graph writes, so the token alone is not completion proof.
        let input = match self.parse_input_from_record(incoming, record) {
            Ok(input) => input,
            Err(_) => return Ok(false),
        };
        let Some(text) = input.text.as_deref() else {
            return Ok(false);
        };
        let incoming_hash = content_hash(&normalize_text(text));
        let Some(version) = self
            .repository
            .latest_document_version(existing.object_uid)
            .await?
        else {
            return Ok(false);
        };
        if version.content_hash != incoming_hash {
            return Ok(false);
        }
        if !self
            .repository
            .object_ingestion_completed_since(existing.object_uid, version.created_at)
            .await?
        {
            return Ok(false);
        }
        let chunks = self
            .repository
            .chunks_for_version(version.version_uid)
            .await?;
        Ok(!chunks.is_empty() && chunks.iter().all(|chunk| chunk.graph_node_uid.is_some()))
    }

    fn parse_input_from_record(
        &self,
        object: KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<crate::domain::ParseInput> {
        let text = record
            .payload
            .get("text")
            .or_else(|| record.payload.get("content"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| record.title.clone())
            .ok_or_else(|| Error::Provider {
                provider: self.provider.clone(),
                message: "provider record did not include materializable text".to_string(),
            })?;
        Ok(crate::domain::ParseInput {
            object,
            file_name: record.title.clone(),
            mime_type: record
                .metadata
                .get("mime_type")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            source_url: None,
            bytes: None,
            text: Some(text),
            options: json!({}),
        })
    }

    async fn record_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        outcome: StepOutcome,
    ) -> Result<()> {
        self.record_step_with_counters(sync_run_uid, object_uid, stage, outcome, None)
            .await
            .map(|_| ())
    }

    async fn record_counter_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        outcome: StepOutcome,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool> {
        self.record_step_with_counters(
            sync_run_uid,
            object_uid,
            stage,
            outcome,
            Some(counter_delta),
        )
        .await
    }

    async fn record_step_with_counters(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        mut outcome: StepOutcome,
        counter_delta: Option<KnowledgeSyncCounters>,
    ) -> Result<bool> {
        let started = std::time::Instant::now();
        let labels = StepLabels {
            provider: &self.provider,
            parser: &self.parser_label,
            stage,
            retryable: outcome
                .summary
                .as_deref()
                .is_some_and(|summary| summary.starts_with("retryable")),
            error_code: outcome.error_code.as_deref().unwrap_or("none"),
        };
        let status = outcome.status.as_str();
        tracing::Span::current().record("status", status);
        tracing::Span::current().record(
            "error_code",
            outcome.error_code.as_deref().unwrap_or("none"),
        );
        outcome.duration_ms = outcome.duration_ms.or_else(|| {
            u64::try_from(started.elapsed().as_millis())
                .ok()
                .map(|duration| duration.max(1))
        });
        self.observer
            .record_step(sync_run_uid, object_uid, labels, outcome.clone())
            .await?;
        let step = build_step_row(sync_run_uid, object_uid, stage, outcome);
        if let Some(counter_delta) = counter_delta {
            self.repository
                .record_ingestion_step_once(step, counter_delta)
                .await
        } else {
            self.repository
                .record_ingestion_step(step)
                .await
                .map(|()| true)
        }
    }

    async fn record_failure_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        error: &Error,
    ) -> Result<FailureClassification> {
        let classification = classify_failure(stage, error);
        self.record_counter_step(
            sync_run_uid,
            object_uid,
            stage,
            failed_outcome(classification),
            KnowledgeSyncCounters {
                records_failed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        if let Some(mut run) = self.repository.get_sync_run(sync_run_uid).await? {
            run.status = if classification.retryable {
                SyncRunStatus::FailedRetryable
            } else {
                SyncRunStatus::FailedTerminal
            };
            run.error_code = Some(classification.error_code.to_string());
            run.finished_at = Some(Utc::now());
            self.repository.update_sync_run(run).await?;
        }
        let object_id = object_uid
            .map(|uid| uid.to_string())
            .unwrap_or_else(|| "none".to_string());
        tracing::warn!(
            sync_run_id = %sync_run_uid,
            object_id = %object_id,
            provider = %self.provider,
            parser = %self.parser_label,
            stage,
            retryable = classification.retryable,
            error_code = classification.error_code,
            "knowledge ingestion failure recorded"
        );
        Ok(classification)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordIngestionOutcome {
    Ingested { embeddings_created: u64 },
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
struct PersistedIngestion {
    delta: KnowledgeGraphDelta,
    embeddings_created: u64,
}

fn chunk_graph_uid(
    delta: &KnowledgeGraphDelta,
    tenant_id: moa_core::TenantId,
    chunk: &KnowledgeChunk,
) -> Result<Uuid> {
    let key = format!("chunk:{}:{}", tenant_id, chunk.chunk_hash);
    delta
        .nodes
        .iter()
        .find(|node| node.key == key)
        .map(|node| node.uid)
        .ok_or_else(|| Error::Repository(format!("missing graph node for chunk {key}")))
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

fn map_graph_error(error: moa_memory_graph::GraphError) -> Error {
    Error::Repository(error.to_string())
}

fn record_span_outcome(span: &Span, status: &'static str, error_code: Option<&str>) {
    span.record("status", status);
    span.record("error_code", error_code.unwrap_or("none"));
}

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
        DocumentVersion, FetchedRecordContent, IngestionStepStatus, KnowledgeChunk,
        KnowledgeObject, KnowledgeSyncCounters, ParseInput, ParsedDocument, ProviderRecord,
        RecordPage, SyncRunStatus,
    },
    error::{Error, Result},
    graph_delta::{
        GraphEdgeUpsert, KnowledgeGraphDelta, document_chunk_delta_with_semantics,
        semantic_chunk_link_count, stable_uid,
    },
    normalize::{normalize_text, redact_provider_metadata},
    observability::{
        FailureClassification, StepLabels, StepOutcome, build_step_row, classify_failure,
        failed_outcome, record_step_observability,
    },
    parser::DocumentParser,
    providers::RecordContentFetcher,
    repository::{DocumentVersionIngestionClaim, KnowledgeRepository},
    semantic_graph::{
        SEMANTIC_GRAPH_MODEL, SEMANTIC_GRAPH_PROMPT_VERSION, SEMANTIC_GRAPH_SCHEMA_VERSION,
        SemanticGraphExtraction, extract_chunk_semantics,
    },
};

/// Maximum objects fetched and tombstoned per source-selection prune page.
const PRUNE_BATCH_SIZE: i64 = 500;

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
                uid: node.uid,
                label: node_label(&node.label)?,
                storage_partition_id: Some(self.scope.tenant_id().0.to_string()),
                contact_id: None,
                scope: "tenant".to_string(),
                name: node_name(&node.label, &properties),
                properties,
                pii_class: PiiClass::None,
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
pub struct KnowledgeIngestionPipeline<R, P, E, G> {
    repository: Arc<R>,
    parser: Arc<P>,
    embedder: Arc<E>,
    graph: Arc<G>,
    chunking: ChunkingConfig,
    provider: String,
    parser_label: String,
    content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
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

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
    /// Creates a knowledge ingestion pipeline from injected dependencies.
    #[must_use]
    pub fn new(
        repository: Arc<R>,
        parser: Arc<P>,
        embedder: Arc<E>,
        graph: Arc<G>,
        config: KnowledgeIngestionPipelineConfig,
    ) -> Self {
        Self {
            repository,
            parser,
            embedder,
            graph,
            chunking: config.chunking,
            provider: config.provider,
            parser_label: config.parser_label,
            content_fetcher: None,
        }
    }

    /// Attaches a per-run content fetcher used to download byte content for
    /// records that carry neither inline text nor a directly fetchable URL.
    ///
    /// Passing `None` leaves the pipeline with its title-only fallback for such
    /// records.
    #[must_use]
    pub fn with_content_fetcher(
        mut self,
        content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
    ) -> Self {
        self.content_fetcher = content_fetcher;
        self
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
            StepOutcome::completed_with_counters(json!({ "records_listed": records_listed })),
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
        // Compute the prune set in SQL (active objects not in the seen set) and
        // page through it with a keyset cursor so a large already-seen set is
        // never materialized on the client. Each pruned object flips to
        // `deleted`, but the cursor advances independently, guaranteeing
        // termination.
        let seen = seen_source_ids.iter().cloned().collect::<Vec<_>>();
        let mut report = PageIngestionReport::default();
        let mut cursor: Option<(String, Uuid)> = None;
        loop {
            let batch = self
                .repository
                .unseen_active_objects_for_connection(
                    connection_uid,
                    tenant_id,
                    &seen,
                    cursor.take(),
                    PRUNE_BATCH_SIZE,
                )
                .await?;
            let Some(last) = batch.last() else {
                break;
            };
            cursor = Some((last.source_id.clone(), last.object_uid));
            let batch_len = batch.len();
            for object in batch {
                let deleted = self.handle_pruned_object(sync_run_uid, object).await?;
                report.records_deleted = report.records_deleted.saturating_add(deleted);
            }
            if batch_len < PRUNE_BATCH_SIZE as usize {
                break;
            }
        }
        self.record_counter_step(
            sync_run_uid,
            None,
            "source_selection_pruned",
            StepOutcome::completed_with_counters_and_summary(
                json!({ "records_pruned": report.records_deleted }),
                "removed objects absent from selected provider sources",
            ),
            KnowledgeSyncCounters::default(),
        )
        .await?;
        Ok(report)
    }

    /// Parses `input`, routing text-only records to the native parser.
    ///
    /// When the input carries neither bytes nor a `source_url`, an external
    /// document parser has nothing to fetch or upload, so the native parser is
    /// used even if an external parser is configured. Inputs with bytes or a
    /// source URL always go to the configured parser. See
    /// [`crate::parser::is_external_document_parser`].
    async fn parse_document(&self, input: ParseInput, parse_span: &Span) -> Result<ParsedDocument> {
        if use_native_document_fallback(&input, &self.parser_label) {
            tracing::debug!(
                configured_parser = %self.parser_label,
                "record has only inline text; using the native parser instead of the configured external parser"
            );
            crate::parser::native::NativeDocumentParser::new()
                .parse(input)
                .instrument(parse_span.clone())
                .await
        } else {
            self.parser
                .parse(input)
                .instrument(parse_span.clone())
                .await
        }
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
            StepOutcome::completed_with_counters(
                json!({ "records_seen": 1, "records_changed": 1 }),
            ),
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let input = self
            .resolve_record_parse_input(sync_run_uid, &object, &record)
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
        let parsed = match self.parse_document(input, &parse_span).await {
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
            StepOutcome::completed_with_counters(
                json!({ "parser_items": parsed.elements.len(), "objects_parsed": 1 }),
            ),
            KnowledgeSyncCounters {
                objects_parsed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        let outcome = self.persist_parsed(sync_run_uid, object, parsed).await?;
        if outcome.ingested {
            Ok(RecordIngestionOutcome::Ingested {
                embeddings_created: outcome.embeddings_created,
            })
        } else {
            Ok(RecordIngestionOutcome::Skipped)
        }
    }

    async fn persist_parsed(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        parsed: ParsedDocument,
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
                ingested: false,
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
            DocumentVersion {
                version_uid: stable_uid(&format!("version:{}:{content_hash}", object.object_uid)),
                object_uid: object.object_uid,
                parser: parsed.parser.clone(),
                parser_job_id: parsed.parser_job_id.clone(),
                content_hash,
                metadata: parsed.metadata.clone(),
                created_at: Utc::now(),
            }
        };
        let (version, claim_token) = match self
            .repository
            .claim_document_version_ingestion(sync_run_uid, version)
            .await?
        {
            DocumentVersionIngestionClaim::Claimed {
                version,
                claim_token,
            } => (version, claim_token),
            DocumentVersionIngestionClaim::AlreadyInProgress(_version)
            | DocumentVersionIngestionClaim::AlreadyCompleted(_version) => {
                self.record_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "normalized",
                    StepOutcome {
                        status: IngestionStepStatus::Skipped,
                        counters: json!({ "blocks_total": 0, "chunks_total": 0 }),
                        summary: Some("content version already claimed".to_string()),
                        retry_count: 0,
                        error_code: None,
                        duration_ms: None,
                    },
                )
                .await?;
                return Ok(PersistedIngestion {
                    delta: KnowledgeGraphDelta::default(),
                    embeddings_created: 0,
                    ingested: false,
                });
            }
        };
        let version_uid = version.version_uid;
        let persisted = self
            .persist_claimed_version(sync_run_uid, object, version, parsed, previous_chunks)
            .await;
        if persisted.is_ok() {
            self.repository
                .complete_document_version_ingestion(sync_run_uid, version_uid, claim_token)
                .await?;
        } else {
            self.repository
                .fail_document_version_ingestion(sync_run_uid, version_uid, claim_token)
                .await?;
        }
        persisted
    }

    async fn persist_claimed_version(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        version: DocumentVersion,
        parsed: ParsedDocument,
        previous_chunks: Vec<KnowledgeChunk>,
    ) -> Result<PersistedIngestion> {
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
            StepOutcome::completed_with_counters(json!({
                    "blocks_total": blocks.len(),
                    "blocks_new": blocks_new,
            })),
        )
        .await?;

        let mut chunks = blocks_to_chunks(version.version_uid, &blocks, self.chunking);
        let semantic_report = match self
            .semantic_graph_extractions(object.tenant_id, &object, &chunks)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                self.record_failure_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "semantic_graph_extracted",
                    &error,
                )
                .await?;
                return Err(error);
            }
        };
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "semantic_graph_extracted",
            StepOutcome::completed_with_counters(json!({
                "chunks_total": chunks.len(),
                "cache_hits": semantic_report.cache_hits,
                "cache_misses": semantic_report.cache_misses,
                "entities_extracted": semantic_report.entities_extracted,
                "relations_extracted": semantic_report.relations_extracted,
                "semantic_chunk_links": semantic_report.semantic_chunk_links,
                "failures": 0,
            })),
        )
        .await?;
        let delta = document_chunk_delta_with_semantics(
            &object,
            &version,
            &chunks,
            &semantic_report.extractions,
        );
        // Fold each chunk's graph node UID and the `active` retrieval marker into
        // the rows before the batch insert. Persisting them up front makes chunk
        // storage a single multi-row write.
        for chunk in &mut chunks {
            let graph_uid = chunk_graph_uid(&delta, object.tenant_id, chunk)?;
            chunk.graph_node_uid = Some(graph_uid);
            chunk.metadata = mark_metadata_active(std::mem::take(&mut chunk.metadata));
        }
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
            StepOutcome::completed_with_counters(json!({
                    "chunks_total": chunks.len(),
                    "chunks_new": chunks_new,
                    "chunks_deleted": orphan_chunks.len(),
            })),
        )
        .await?;

        let mut embedding_inputs = Vec::new();
        let mut embedding_uids = Vec::new();
        for chunk in &chunks {
            if old_by_hash.contains_key(&chunk.chunk_hash) {
                continue;
            }
            if let Some(graph_uid) = chunk.graph_node_uid {
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
            StepOutcome::completed_with_counters(json!({
                    "embeddings_created": embeddings.len(),
                    "embeddings_reused": chunks.len().saturating_sub(embeddings.len()),
                    "chunks_embedded": embeddings.len(),
            })),
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
                self.embedder.model_id(),
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
            StepOutcome::completed_with_counters(json!({
                    "graph_nodes_upserted": graph_report.nodes_upserted,
                    "graph_edges_upserted": graph_report.edges_upserted,
            })),
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
            .collect::<Vec<_>>();
        self.repository.tombstone_chunks(&tombstones).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome::completed_with_counters(json!({
                    "vector_rows_upserted": embeddings.len(),
                    "vector_rows_deleted": invalidation_report.vector_rows_deleted,
            })),
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
            StepOutcome::completed_with_counters(json!({
                    "contact_groups": contact_group_count,
                    "contact_group_memberships_changed": contact_memberships,
                    "records_ingested": 1,
            })),
            KnowledgeSyncCounters {
                records_ingested: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        Ok(PersistedIngestion {
            delta,
            embeddings_created: embeddings.len() as u64,
            ingested: true,
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
            match summary {
                Some(summary) => {
                    StepOutcome::completed_with_counters_and_summary(counters, summary)
                }
                None => StepOutcome::completed_with_counters(counters),
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
            StepOutcome::completed_with_counters(json!({ "chunks_deleted": chunk_uids.len() })),
        )
        .await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome::completed_with_counters(json!({
                "vector_rows_deleted": invalidation_report.vector_rows_deleted
            })),
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
        // The object row is advanced before parse and graph writes, so an
        // unchanged change token alone is not completion proof: there must be a
        // completed document version with graph-linked chunks.
        let Some(version) = self
            .repository
            .latest_document_version(existing.object_uid)
            .await?
        else {
            return Ok(false);
        };
        // Records carrying real inline text must also match the stored version
        // hash, so an inline edit delivered under an unchanged change token is
        // not skipped. Records that rely on the content-fetch hook or the
        // title-only fallback have no inline text to hash against the
        // fetched/parsed content — hashing their title would never match and
        // would force a re-fetch every sync — so the unchanged change token plus
        // a completed version is the authority for them.
        if record_materializes_inline(record) {
            let input = match parse_input_from_record(&self.provider, incoming, record) {
                Ok(input) => input,
                Err(_) => return Ok(false),
            };
            let Some(text) = input.text.as_deref() else {
                return Ok(false);
            };
            let incoming_hash = content_hash(&normalize_text(text));
            if version.content_hash != incoming_hash {
                return Ok(false);
            }
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

    /// Resolves the parse input for one record, downloading provider content
    /// when the record carries neither inline text nor a fetchable URL.
    ///
    /// Records that already materialize (inline text or a directly fetchable
    /// URL) skip the fetch entirely. Otherwise, when a content fetcher is wired,
    /// a successful fetch yields a bytes-backed [`ParseInput`]; a fetch that
    /// returns nothing or errors records a distinct soft signal and falls back
    /// to the title-only behavior. The `content_fetched` step is recorded here
    /// exactly once, and a record with no title still fails with the pinned
    /// `materializable text` classification.
    async fn resolve_record_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<ParseInput> {
        if record_has_materializable_content(record) {
            let input = parse_input_from_record(&self.provider, object.clone(), record)?;
            return self
                .record_resolved_parse_input(sync_run_uid, object, input, None)
                .await;
        }

        let mut fetch_note: Option<&'static str> = None;
        if let Some(fetcher) = &self.content_fetcher {
            match fetcher.fetch_record_content(record).await {
                Ok(Some(content)) if !content.bytes.is_empty() => {
                    let input = parse_input_from_fetched_content(object.clone(), record, content);
                    return self
                        .record_resolved_parse_input(sync_run_uid, object, input, None)
                        .await;
                }
                Ok(_) => {
                    fetch_note = Some("provider_content_fetch_empty");
                }
                Err(error) => {
                    fetch_note = Some("provider_content_fetch_failed");
                    tracing::warn!(
                        sync_run_id = %sync_run_uid,
                        object_id = %object.object_uid,
                        provider = %self.provider,
                        error = %error,
                        "provider content fetch failed; falling back to record title"
                    );
                }
            }
        }

        let input = match parse_input_from_record(&self.provider, object.clone(), record) {
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
        self.record_resolved_parse_input(sync_run_uid, object, input, fetch_note)
            .await
    }

    /// Records the `content_fetched` step for a resolved parse input.
    ///
    /// A `fetch_note` marks a soft content-fetch fallback: the step still
    /// completes (the title-only input is usable) but carries a distinct
    /// `error_code` so operators can tell "content fetch failed" apart from a
    /// plain metadata-only record.
    async fn record_resolved_parse_input(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        input: ParseInput,
        fetch_note: Option<&'static str>,
    ) -> Result<ParseInput> {
        let bytes_fetched = input.bytes.as_ref().map_or_else(
            || input.text.as_ref().map_or(0, |text| text.len()),
            Vec::len,
        );
        let outcome = match fetch_note {
            Some(note) => StepOutcome {
                status: IngestionStepStatus::Completed,
                counters: json!({ "bytes_fetched": bytes_fetched }),
                summary: Some(
                    "provider content fetch unavailable; indexed record title".to_string(),
                ),
                retry_count: 0,
                error_code: Some(note.to_string()),
                duration_ms: None,
            },
            None => StepOutcome::completed_with_counters(json!({ "bytes_fetched": bytes_fetched })),
        };
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "content_fetched",
            outcome,
        )
        .await?;
        Ok(input)
    }

    async fn semantic_graph_extractions(
        &self,
        tenant_id: moa_core::TenantId,
        object: &KnowledgeObject,
        chunks: &[KnowledgeChunk],
    ) -> Result<SemanticGraphExtractionReport> {
        let chunk_hashes = chunks
            .iter()
            .map(|chunk| chunk.chunk_hash.clone())
            .collect::<Vec<_>>();
        let cached = self
            .repository
            .cached_semantic_graph_extractions(
                tenant_id,
                &chunk_hashes,
                SEMANTIC_GRAPH_SCHEMA_VERSION,
                SEMANTIC_GRAPH_MODEL,
                SEMANTIC_GRAPH_PROMPT_VERSION,
            )
            .await?;
        let mut cached_by_hash = cached
            .into_iter()
            .map(|extraction| (extraction.chunk_hash.clone(), extraction))
            .collect::<HashMap<_, _>>();
        let mut cache_hits = 0_u64;
        let mut cache_misses = 0_u64;
        let mut extracted = Vec::with_capacity(chunks.len());
        let mut new_extractions = Vec::new();

        for chunk in chunks {
            if let Some(extraction) = cached_by_hash.remove(&chunk.chunk_hash) {
                cache_hits = cache_hits.saturating_add(1);
                extracted.push(extraction);
            } else {
                cache_misses = cache_misses.saturating_add(1);
                let extraction = extract_chunk_semantics(object, chunk);
                new_extractions.push(extraction.clone());
                extracted.push(extraction);
            }
        }
        self.repository
            .upsert_semantic_graph_extractions(tenant_id, new_extractions)
            .await?;

        Ok(SemanticGraphExtractionReport {
            cache_hits,
            cache_misses,
            entities_extracted: extracted
                .iter()
                .map(|extraction| extraction.entities.len() as u64)
                .sum(),
            relations_extracted: extracted
                .iter()
                .map(|extraction| extraction.relations.len() as u64)
                .sum(),
            semantic_chunk_links: semantic_chunk_link_count(chunks, &extracted) as u64,
            extractions: extracted,
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
        record_step_observability(labels, &outcome);
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
    ingested: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct SemanticGraphExtractionReport {
    extractions: Vec<SemanticGraphExtraction>,
    cache_hits: u64,
    cache_misses: u64,
    entities_extracted: u64,
    relations_extracted: u64,
    semantic_chunk_links: u64,
}

/// Returns `metadata` as a JSON object with the `active` retrieval flag set.
///
/// Non-object metadata is replaced with an empty object before the flag is
/// inserted, so a freshly persisted chunk is always visible to active
/// retrieval.
fn mark_metadata_active(metadata: Value) -> Value {
    let mut object = match metadata {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    object.insert("active".to_string(), Value::Bool(true));
    Value::Object(object)
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

/// Payload fields, in priority order, that carry already-materialized record
/// text. The first present string is used as inline `text` for the parser.
const RECORD_INLINE_TEXT_FIELDS: &[&str] = &[
    "text",
    "content",
    "body",
    "plain_text",
    "plaintext",
    "markdown",
    "html",
];

/// Payload fields, in priority order, that carry a directly fetchable document
/// URL. The first present string becomes `source_url` so an external parser can
/// download the file.
///
/// These are download/content links only. Auth-walled browser viewers such as
/// Google Drive's `webViewLink`/`web_view_link`, and the ambiguous generic
/// `url` (which providers map to the human-facing `source_uri`), are
/// deliberately excluded: they are not fetchable by an unauthenticated parser,
/// so a record carrying only such a link routes to the provider content-fetch
/// hook or the title-only fallback instead of a doomed download.
const RECORD_SOURCE_URL_FIELDS: &[&str] = &[
    "download_url",
    "file_url",
    "content_url",
    "web_content_link",
    "webContentLink",
];

/// Metadata and payload fields, in priority order, that carry a MIME type.
const RECORD_MIME_TYPE_FIELDS: &[&str] = &["mime_type", "mimeType", "content_type", "contentType"];

/// Builds a [`ParseInput`] from a normalized provider record using a
/// provider-agnostic payload convention shared by every
/// [`LinkedIntegrationProvider`](crate::providers::LinkedIntegrationProvider)
/// adapter.
///
/// Resolution order:
///
/// 1. Inline text from the first present of [`RECORD_INLINE_TEXT_FIELDS`] is
///    used directly as `text` (no fetch or upload needed).
/// 2. Otherwise `source_url` is populated from the first present of
///    [`RECORD_SOURCE_URL_FIELDS`] so an external document parser can fetch the
///    file, and any [`RECORD_MIME_TYPE_FIELDS`] value is passed through.
/// 3. Otherwise the record `title` is indexed as `text`, preserving the prior
///    title-only fallback behavior.
///
/// # Errors
///
/// Returns [`Error::Provider`] when a record carries no inline text, no
/// fetchable URL, and no title. The message retains the `materializable text`
/// marker used by failure classification.
pub fn parse_input_from_record(
    provider: &str,
    object: KnowledgeObject,
    record: &ProviderRecord,
) -> Result<ParseInput> {
    let inline_text = first_record_string(record, RECORD_INLINE_TEXT_FIELDS);
    let source_url = if inline_text.is_none() {
        first_record_string(record, RECORD_SOURCE_URL_FIELDS)
    } else {
        None
    };
    let text = match (&inline_text, &source_url) {
        (Some(_), _) => inline_text,
        (None, Some(_)) => None,
        (None, None) => Some(record.title.clone().ok_or_else(|| {
            Error::Provider {
                provider: provider.to_string(),
                message:
                    "provider record did not include materializable text or a fetchable source URL"
                        .to_string(),
            }
        })?),
    };
    Ok(ParseInput {
        object,
        file_name: record.title.clone(),
        mime_type: first_record_string(record, RECORD_MIME_TYPE_FIELDS),
        source_url,
        bytes: None,
        text,
        options: json!({}),
    })
}

/// Returns whether a record carries inline text materialized directly from its
/// own payload fields.
///
/// This is the signal distinguishing records whose stored content is the record
/// text (so the version hash is meaningful for change detection) from records
/// whose content comes from the fetch hook or the title-only fallback (where the
/// change token, not a title hash, is the completion authority).
fn record_materializes_inline(record: &ProviderRecord) -> bool {
    first_record_string(record, RECORD_INLINE_TEXT_FIELDS).is_some()
}

/// Returns whether a record already materializes without a provider content
/// fetch, i.e. it carries inline text or a directly fetchable source URL.
fn record_has_materializable_content(record: &ProviderRecord) -> bool {
    record_materializes_inline(record)
        || first_record_string(record, RECORD_SOURCE_URL_FIELDS).is_some()
}

/// Builds a [`ParseInput`] from provider-fetched byte content.
///
/// The fetched bytes route through the parser-selection heuristic exactly like a
/// downloaded document: text bytes fall to the native parser while binary bytes
/// go to the configured external parser. The MIME type prefers the value
/// reported by the fetch, falling back to any MIME field on the record.
fn parse_input_from_fetched_content(
    object: KnowledgeObject,
    record: &ProviderRecord,
    content: FetchedRecordContent,
) -> ParseInput {
    ParseInput {
        object,
        file_name: record.title.clone(),
        mime_type: content
            .mime_type
            .or_else(|| first_record_string(record, RECORD_MIME_TYPE_FIELDS)),
        source_url: None,
        bytes: Some(content.bytes),
        text: None,
        options: json!({}),
    }
}

/// Returns whether a parse input should fall back to the native parser.
///
/// True only when the input carries neither bytes nor a `source_url` (nothing
/// for an external parser to fetch or upload) and the configured parser is an
/// external document parser. Inputs with bytes or a URL, and non-external
/// configured parsers (including `native` and test parsers), keep the
/// configured parser.
fn use_native_document_fallback(input: &ParseInput, parser_label: &str) -> bool {
    input.bytes.is_none()
        && input.source_url.is_none()
        && crate::parser::is_external_document_parser(parser_label)
}

/// Returns the first of `keys` present as a non-empty string in the record
/// payload, then metadata. Payload wins because it holds the raw source fields.
fn first_record_string(record: &ProviderRecord, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        record
            .payload
            .get(*key)
            .or_else(|| record.metadata.get(*key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
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

fn map_graph_error(error: moa_memory_graph::GraphError) -> Error {
    Error::Repository(error.to_string())
}

fn record_span_outcome(span: &Span, status: &'static str, error_code: Option<&str>) {
    span.record("status", status);
    span.record("error_code", error_code.unwrap_or("none"));
}

#[cfg(test)]
mod tests {
    use moa_core::TenantId;

    use super::{
        ParseInput, parse_input_from_fetched_content, parse_input_from_record,
        record_has_materializable_content, use_native_document_fallback,
    };
    use crate::domain::{FetchedRecordContent, KnowledgeObject, ObjectStatus, ProviderRecord};
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn object() -> KnowledgeObject {
        KnowledgeObject {
            object_uid: Uuid::from_u128(1),
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            connection_uid: Uuid::from_u128(3),
            object_type: "document".to_string(),
            source_id: "src-1".to_string(),
            parent_source_id: None,
            source_uri: None,
            title: None,
            change_token: None,
            metadata: json!({}),
            status: ObjectStatus::Pending,
            source_updated_at: None,
            deleted_at: None,
        }
    }

    fn record(title: Option<&str>, source_uri: Option<&str>, payload: Value) -> ProviderRecord {
        ProviderRecord {
            source_id: "src-1".to_string(),
            object_type: "document".to_string(),
            title: title.map(ToString::to_string),
            source_uri: source_uri.map(ToString::to_string),
            change_token: None,
            deleted: false,
            source_updated_at: None,
            metadata: json!({}),
            payload,
        }
    }

    fn parse_input(
        bytes: Option<Vec<u8>>,
        source_url: Option<&str>,
        text: Option<&str>,
    ) -> ParseInput {
        ParseInput {
            object: object(),
            file_name: None,
            mime_type: None,
            source_url: source_url.map(ToString::to_string),
            bytes,
            text: text.map(ToString::to_string),
            options: json!({}),
        }
    }

    #[test]
    fn parse_input_uses_inline_content_as_text() {
        // Pins: inline body text materializes as ParseInput.text with no
        // source_url, so no external fetch is needed even when a web link exists.
        let record = record(
            Some("Doc"),
            Some("https://web.example/doc"),
            json!({ "content": "hello world", "mime_type": "text/plain" }),
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(input.text.as_deref(), Some("hello world"));
        assert_eq!(input.source_url, None);
        assert_eq!(input.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(input.file_name.as_deref(), Some("Doc"));
    }

    #[test]
    fn parse_input_sets_source_url_for_url_only_record() {
        // Pins: a record with a download URL but no inline text yields a
        // source_url for an external parser and leaves text unset.
        let record = record(
            Some("Report.pdf"),
            None,
            json!({
                "download_url": "https://files.example/report.pdf",
                "mime_type": "application/pdf"
            }),
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(
            input.source_url.as_deref(),
            Some("https://files.example/report.pdf")
        );
        assert_eq!(input.text, None);
        assert_eq!(input.mime_type.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn parse_input_falls_back_to_title_when_no_body_or_url() {
        // Pins: prior title-only behavior. A record with neither inline text nor
        // a fetchable download URL indexes its title as text; the human-facing
        // source_uri web link is not treated as a fetchable source_url.
        let record = record(
            Some("Just A Title"),
            Some("https://web.example/x"),
            json!({ "irrelevant": "field" }),
        );
        let input = parse_input_from_record("nango", object(), &record).expect("materializes");
        assert_eq!(input.text.as_deref(), Some("Just A Title"));
        assert_eq!(input.source_url, None);
    }

    #[test]
    fn parse_input_errors_without_text_url_or_title() {
        // Pins: a record with no inline text, no fetchable URL, and no title
        // fails with the `materializable text` classification marker.
        let record = record(
            None,
            Some("https://web.example/x"),
            json!({ "safe": "meta" }),
        );
        let error = parse_input_from_record("nango", object(), &record).expect_err("no content");
        let message = error.to_string();
        assert!(
            message.contains("materializable text"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn parse_input_ignores_auth_walled_web_view_link() {
        // Pins the web_view_link hazard fix: a record whose only link is an
        // auth-walled Google Drive browser viewer (webViewLink/web_view_link) or
        // a generic `url` is not treated as a fetchable source_url. With a title
        // it falls back to title-only text; the viewer link never routes to a
        // doomed unauthenticated download.
        for field in ["web_view_link", "webViewLink", "url"] {
            let record = record(
                Some("Drive Doc"),
                None,
                json!({ field: "https://drive.google.com/file/d/abc/view" }),
            );
            let input = parse_input_from_record("nango", object(), &record).expect("materializes");
            assert_eq!(
                input.source_url, None,
                "field `{field}` must not be fetchable"
            );
            assert_eq!(input.text.as_deref(), Some("Drive Doc"));
            assert!(
                !record_has_materializable_content(&record),
                "field `{field}` must not count as materializable content"
            );
        }
    }

    #[test]
    fn parse_input_still_accepts_genuine_download_links() {
        // Pins: real download/content links remain fetchable after the
        // web_view_link fix removed the auth-walled viewer candidates.
        for field in [
            "download_url",
            "file_url",
            "content_url",
            "web_content_link",
            "webContentLink",
        ] {
            let record = record(
                Some("File"),
                None,
                json!({ field: "https://files.example/x" }),
            );
            let input = parse_input_from_record("nango", object(), &record).expect("materializes");
            assert_eq!(
                input.source_url.as_deref(),
                Some("https://files.example/x"),
                "field `{field}` should remain fetchable"
            );
            assert!(
                record_has_materializable_content(&record),
                "field `{field}` should count as materializable content"
            );
        }
    }

    #[test]
    fn fetched_content_builds_bytes_backed_parse_input() {
        // Pins: fetched byte content becomes ParseInput.bytes (never text or
        // source_url), and the fetch-reported MIME is preferred over the record's
        // own MIME field.
        let record = record(
            Some("Report"),
            None,
            json!({ "mime_type": "application/pdf" }),
        );
        let content = FetchedRecordContent {
            bytes: b"fetched-bytes".to_vec(),
            mime_type: Some("text/plain".to_string()),
        };
        let input = parse_input_from_fetched_content(object(), &record, content);
        assert_eq!(input.bytes.as_deref(), Some(b"fetched-bytes".as_slice()));
        assert_eq!(input.text, None);
        assert_eq!(input.source_url, None);
        assert_eq!(input.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(input.file_name.as_deref(), Some("Report"));
    }

    #[test]
    fn native_fallback_only_for_text_only_external_parser() {
        // Pins: text-only inputs override an external parser to native, while
        // inputs with bytes or a URL, and non-external parsers, keep the
        // configured parser.
        let text_only = parse_input(None, None, Some("inline"));
        assert!(use_native_document_fallback(&text_only, "llamaparse"));
        assert!(use_native_document_fallback(&text_only, "unstructured"));
        assert!(use_native_document_fallback(&text_only, "reducto"));
        assert!(!use_native_document_fallback(&text_only, "native"));
        assert!(!use_native_document_fallback(&text_only, "test_parser"));

        let with_url = parse_input(None, Some("https://files.example/x.pdf"), None);
        assert!(!use_native_document_fallback(&with_url, "llamaparse"));

        let with_bytes = parse_input(Some(vec![1, 2, 3]), None, None);
        assert!(!use_native_document_fallback(&with_bytes, "llamaparse"));
    }
}

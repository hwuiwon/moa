//! DB integration coverage for the tenant knowledge ingestion pipeline.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::RlsContext;
use moa_core::{TenantId, traits::EmbeddingProvider};
use moa_knowledge::{
    chunking::{ChunkingConfig, content_hash},
    domain::{
        ConnectionStatus, DocumentElement, DocumentElementKind, DocumentVersion,
        FetchedRecordContent, IngestionStepStatus, KnowledgeChunk, KnowledgeConnection,
        KnowledgeIngestionStep, KnowledgeObject, KnowledgeSyncCounters, KnowledgeSyncRun,
        ObjectStatus, ParsedDocument, ProviderRecord, RecordPage, SyncRunStatus,
    },
    graph_delta::KnowledgeGraphDelta,
    ingestion::{
        GraphWriteReport, KnowledgeGraphWriter, KnowledgeIngestionPipeline,
        KnowledgeIngestionPipelineConfig,
    },
    normalize::normalize_text,
    parser::DocumentParser,
    providers::RecordContentFetcher,
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_test_support::postgres;
use serde_json::{Value, json};
use tokio::sync::Barrier;
use uuid::Uuid;

#[derive(Debug, Default)]
struct ParagraphParser;

#[async_trait]
impl DocumentParser for ParagraphParser {
    async fn parse(
        &self,
        input: moa_knowledge::domain::ParseInput,
    ) -> moa_knowledge::Result<ParsedDocument> {
        let text = input
            .text
            .ok_or_else(|| moa_knowledge::Error::parser("test", "missing text"))?;
        let elements = text
            .split("\n\n")
            .enumerate()
            .filter_map(|(index, part)| {
                let text = part.trim();
                (!text.is_empty()).then(|| DocumentElement {
                    element_id: format!("p{index}"),
                    kind: DocumentElementKind::Paragraph,
                    text: text.to_string(),
                    heading_path: input.object.title.clone().into_iter().collect(),
                    ordinal: index as u32,
                    page_number: None,
                    layout: None,
                    metadata: json!({ "source": "test_parser" }),
                })
            })
            .collect::<Vec<_>>();
        Ok(ParsedDocument {
            parser: "test_parser".to_string(),
            parser_job_id: None,
            text,
            elements,
            metadata: json!({ "parser": "test_parser" }),
        })
    }
}

#[derive(Debug)]
struct BarrierParser {
    barrier: Arc<Barrier>,
}

#[async_trait]
impl DocumentParser for BarrierParser {
    async fn parse(
        &self,
        input: moa_knowledge::domain::ParseInput,
    ) -> moa_knowledge::Result<ParsedDocument> {
        self.barrier.wait().await;
        ParagraphParser.parse(input).await
    }
}

/// Paragraph parser that accepts either inline text or byte content, decoding
/// bytes as UTF-8 so provider-fetched content flows through the same splitter.
#[derive(Debug, Default)]
struct BytesOrTextParagraphParser;

#[async_trait]
impl DocumentParser for BytesOrTextParagraphParser {
    async fn parse(
        &self,
        input: moa_knowledge::domain::ParseInput,
    ) -> moa_knowledge::Result<ParsedDocument> {
        let text = match (input.text.clone(), input.bytes.clone()) {
            (Some(text), _) => text,
            (None, Some(bytes)) => String::from_utf8(bytes)
                .map_err(|_| moa_knowledge::Error::parser("test", "non-utf8 fetched bytes"))?,
            (None, None) => {
                return Err(moa_knowledge::Error::parser(
                    "test",
                    "missing text and bytes",
                ));
            }
        };
        ParagraphParser
            .parse(moa_knowledge::domain::ParseInput {
                text: Some(text),
                bytes: None,
                ..input
            })
            .await
    }
}

/// Outcome a [`FakeContentFetcher`] returns for every record.
enum FetchOutcome {
    Bytes(Vec<u8>, Option<String>),
    Error,
}

/// Test content fetcher standing in for a provider proxy download, counting how
/// many times it was called so tests can pin that unchanged records do not
/// re-fetch.
struct FakeContentFetcher {
    outcome: FetchOutcome,
    calls: AtomicUsize,
}

impl FakeContentFetcher {
    fn new(outcome: FetchOutcome) -> Self {
        Self {
            outcome,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RecordContentFetcher for FakeContentFetcher {
    async fn fetch_record_content(
        &self,
        _record: &ProviderRecord,
    ) -> moa_knowledge::Result<Option<FetchedRecordContent>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.outcome {
            FetchOutcome::Bytes(bytes, mime_type) => Ok(Some(FetchedRecordContent {
                bytes: bytes.clone(),
                mime_type: mime_type.clone(),
            })),
            FetchOutcome::Error => {
                Err(moa_knowledge::Error::provider("test", "content fetch boom"))
            }
        }
    }
}

#[derive(Debug, Default)]
struct CountingEmbedder {
    embedded_texts: Mutex<Vec<String>>,
}

impl CountingEmbedder {
    fn embedded_count(&self) -> usize {
        self.embedded_texts
            .lock()
            .expect("embedded text mutex should not be poisoned")
            .len()
    }
}

#[async_trait]
impl EmbeddingProvider for CountingEmbedder {
    fn model_id(&self) -> &str {
        "test-model"
    }

    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        self.embedded_texts
            .lock()
            .expect("embedded text mutex should not be poisoned")
            .extend(inputs.iter().cloned());
        Ok(inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let mut vector = vec![0.0; 1024];
                vector[index % 1024] = 1.0;
                vector
            })
            .collect())
    }
}

#[derive(Debug, Default)]
struct FakeGraphWriter {
    nodes: Mutex<HashMap<Uuid, Value>>,
    edges: Mutex<HashMap<Uuid, (String, Value)>>,
    vectors: Mutex<HashSet<Uuid>>,
    invalidated: Mutex<Vec<Uuid>>,
    /// When set, `invalidate_chunks` fails for any non-empty request, simulating
    /// a graph-invalidation backend error mid-transition or mid-deletion.
    fail_nonempty_invalidate: AtomicBool,
}

impl FakeGraphWriter {
    fn set_fail_invalidate(&self, fail: bool) {
        self.fail_nonempty_invalidate.store(fail, Ordering::SeqCst);
    }

    fn vector_count(&self) -> usize {
        self.vectors
            .lock()
            .expect("vector mutex should not be poisoned")
            .len()
    }

    fn invalidated_count(&self) -> usize {
        self.invalidated
            .lock()
            .expect("invalidated mutex should not be poisoned")
            .len()
    }

    fn properties_json(&self) -> String {
        serde_json::to_string(
            &*self
                .nodes
                .lock()
                .expect("node mutex should not be poisoned"),
        )
        .expect("serialize graph node properties")
    }

    fn edge_properties_json(&self) -> String {
        serde_json::to_string(
            &*self
                .edges
                .lock()
                .expect("edge mutex should not be poisoned"),
        )
        .expect("serialize graph edge properties")
    }
}

#[async_trait]
impl KnowledgeGraphWriter for FakeGraphWriter {
    async fn upsert_delta(
        &self,
        delta: &KnowledgeGraphDelta,
        embeddings: &HashMap<Uuid, Vec<f32>>,
        _embedding_model: &str,
        _embedding_model_version: i32,
    ) -> moa_knowledge::Result<GraphWriteReport> {
        let mut nodes = self
            .nodes
            .lock()
            .expect("node mutex should not be poisoned");
        let mut node_count = 0_u64;
        for node in &delta.nodes {
            if nodes.insert(node.uid, node.properties.clone()).is_none() {
                node_count += 1;
            }
        }
        drop(nodes);

        let mut edges = self
            .edges
            .lock()
            .expect("edge mutex should not be poisoned");
        let mut edge_count = 0_u64;
        for edge in &delta.edges {
            if edges
                .insert(
                    edge.uid,
                    (edge.relationship.clone(), edge.properties.clone()),
                )
                .is_none()
            {
                edge_count += 1;
            }
        }
        drop(edges);

        self.vectors
            .lock()
            .expect("vector mutex should not be poisoned")
            .extend(embeddings.keys().copied());
        Ok(GraphWriteReport {
            nodes_upserted: node_count,
            edges_upserted: edge_count,
            vector_rows_deleted: 0,
        })
    }

    async fn invalidate_chunks(
        &self,
        graph_node_uids: &[Uuid],
    ) -> moa_knowledge::Result<GraphWriteReport> {
        if !graph_node_uids.is_empty() && self.fail_nonempty_invalidate.load(Ordering::SeqCst) {
            return Err(moa_knowledge::Error::Repository(
                "injected invalidate_chunks failure".to_string(),
            ));
        }
        let mut deleted = 0_u64;
        let mut vectors = self
            .vectors
            .lock()
            .expect("vector mutex should not be poisoned");
        for uid in graph_node_uids {
            if vectors.remove(uid) {
                deleted += 1;
            }
        }
        drop(vectors);
        self.invalidated
            .lock()
            .expect("invalidated mutex should not be poisoned")
            .extend_from_slice(graph_node_uids);
        Ok(GraphWriteReport {
            nodes_upserted: 0,
            edges_upserted: 0,
            vector_rows_deleted: deleted,
        })
    }
}

#[tokio::test]
async fn ingestion_pipeline_skips_unchanged_reembeds_edits_and_tombstones_deletes() {
    // Pins: provider-page ingestion is idempotent, re-embeds only changed chunks, and keeps DB audit rows.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_1".to_string(),
            credential_ref: "vault://knowledge/test".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let connection_metadata = connection_metadata(&pool, connection_uid).await;
    assert_eq!(connection_metadata["safe"], true);
    assert_no_secret_material(&connection_metadata);

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    let first = pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Alpha is ready.\n\nBudget is 10.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first record");
    assert_eq!(first.records_ingested, 1);
    assert_eq!(first.embeddings_created, 2);
    let first_counters = sync_counters(&pool, first_run).await;
    assert_eq!(first_counters.records_seen, 1);
    assert_eq!(first_counters.records_changed, 1);
    assert_eq!(first_counters.records_ingested, 1);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    let object_uid = object_uid(connection_uid);
    let first_object_metadata = object_metadata(&pool, object_uid).await;
    assert_eq!(first_object_metadata["mime_type"], "text/plain");
    assert_eq!(first_object_metadata["safe"], true);
    assert_eq!(
        first_object_metadata["nested"]["session_header"],
        "[redacted]"
    );
    assert_no_secret_material(&first_object_metadata);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);

    let unchanged_run = create_run(&repository, tenant_id, connection_uid).await;
    let unchanged = pipeline
        .ingest_record_page(
            unchanged_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v1", false, "Alpha is ready.\n\nBudget is 10.")],
                next_cursor: None,
            },
        )
        .await
        .expect("skip unchanged record");
    assert_eq!(unchanged.records_skipped, 1);
    let unchanged_counters = sync_counters(&pool, unchanged_run).await;
    assert_eq!(unchanged_counters.records_seen, 1);
    assert_eq!(unchanged_counters.records_changed, 0);
    assert_eq!(unchanged_counters.records_ingested, 0);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);

    let edit_run = create_run(&repository, tenant_id, connection_uid).await;
    let edited = pipeline
        .ingest_record_page(
            edit_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v2", false, "Alpha is ready.\n\nBudget is 20.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest one-block edit");
    assert_eq!(edited.records_ingested, 1);
    assert_eq!(edited.embeddings_created, 1);
    assert_eq!(embedder.embedded_count(), 3);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(graph.invalidated_count(), 1);
    assert_eq!(version_count(&pool, object_uid).await, 2);
    assert_eq!(chunk_count(&pool, object_uid).await, 4);
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 1);

    let delete_run = create_run(&repository, tenant_id, connection_uid).await;
    let deleted = pipeline
        .ingest_record_page(
            delete_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("v3", true, "")],
                next_cursor: None,
            },
        )
        .await
        .expect("handle provider deletion");
    assert_eq!(deleted.records_deleted, 1);
    let delete_counters = sync_counters(&pool, delete_run).await;
    assert_eq!(delete_counters.records_seen, 1);
    assert_eq!(delete_counters.records_deleted, 1);
    assert_eq!(graph.vector_count(), 0);
    assert_eq!(object_status(&pool, object_uid).await, "deleted");
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 3);

    let steps = repository
        .object_timeline(object_uid)
        .await
        .expect("read object ingestion timeline");
    let first_run_steps = steps
        .iter()
        .filter(|step| step.sync_run_uid == first_run)
        .map(|step| step.step.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        first_run_steps,
        vec![
            "object_change_checked",
            "content_fetched",
            "parse_submitted",
            "parse_completed",
            "normalized",
            "blocks_diffed",
            "semantic_graph_extracted",
            "chunks_diffed",
            "embedded",
            "graph_upserted",
            "vector_indexed",
            "contact_groups_derived",
        ]
    );
    let unchanged_steps = steps
        .iter()
        .filter(|step| step.sync_run_uid == unchanged_run)
        .collect::<Vec<_>>();
    assert_eq!(unchanged_steps.len(), 1);
    assert_eq!(
        unchanged_steps[0].status,
        moa_knowledge::domain::IngestionStepStatus::Skipped
    );

    let counters = sync_counters(&pool, edit_run).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.chunks_embedded, 1);

    let graph_json = graph.properties_json();
    assert_no_secret_text(&graph_json);
}

#[tokio::test]
async fn ingestion_reconciles_stale_predecessor_when_retrying_incomplete_same_hash_version_db_memory()
 {
    // Pins: F07 — a version transition that persists new chunks but fails before
    // invalidating its predecessor leaves the newest version same-hash-incomplete.
    // The retry must reconcile against every active chunk across all versions and
    // orphan the real predecessor, instead of forgetting it (an empty
    // `previous_chunks`) and leaving BOTH versions' chunks active and retrievable.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_1".to_string(),
            credential_ref: "vault://knowledge/test".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // Attempt A: first content completes version V1 with two active chunks.
    let run_a = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run_a,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-a", false, "Alpha one.\n\nBeta one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest first content");
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(active_chunk_count(&pool, object_uid).await, 2);

    // Attempt B: new content creates V2 and persists its chunks, then fails at
    // predecessor invalidation. Both V1 and V2 chunks are now active.
    graph.set_fail_invalidate(true);
    let run_b = create_run(&repository, tenant_id, connection_uid).await;
    let attempt_b = pipeline
        .ingest_record_page(
            run_b,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-b", false, "Gamma two.\n\nDelta two.")],
                next_cursor: None,
            },
        )
        .await;
    assert!(
        attempt_b.is_err(),
        "failed predecessor invalidation must surface as an error"
    );
    assert_eq!(version_count(&pool, object_uid).await, 2);
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        4,
        "both versions' chunks are stranded active before the retry"
    );

    // Attempt C: retry the same content (same hash, new change token). Invalidation
    // now succeeds; reconciliation must orphan V1's chunks rather than forget them.
    graph.set_fail_invalidate(false);
    let run_c = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run_c,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-c", false, "Gamma two.\n\nDelta two.")],
                next_cursor: None,
            },
        )
        .await
        .expect("retry same content");
    assert_eq!(
        version_count(&pool, object_uid).await,
        2,
        "the same-hash retry reuses V2 rather than creating a third version"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        2,
        "only the newest version's chunks remain active after reconciliation"
    );
    assert_eq!(
        tombstoned_chunk_count(&pool, object_uid).await,
        2,
        "the stale predecessor's chunks are invalidated, not left active"
    );
}

#[tokio::test]
async fn deletion_writes_terminal_status_last_and_stays_retryable_on_invalidation_failure_db_memory()
 {
    // Pins: F06 — a provider deletion whose graph invalidation fails must NOT leave
    // the object in terminal `deleted` with active chunks stranded. The object stays
    // non-terminal (so it is still selectable), and a later prune completes the
    // deletion. This covers both the `handle_deleted_record` entry point (no direct
    // upsert into terminal `deleted`) and the terminal-state-last ordering.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_1".to_string(),
            credential_ref: "vault://knowledge/test".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // Ingest content so the object has one active version with two active chunks.
    let run_a = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            run_a,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-a", false, "Alpha one.\n\nBeta one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest content");
    assert_eq!(active_chunk_count(&pool, object_uid).await, 2);
    assert_eq!(object_status(&pool, object_uid).await, "active");

    // Provider deletion with failing invalidation: the object must remain
    // non-terminal (never upserted straight into `deleted`) and keep its chunks.
    graph.set_fail_invalidate(true);
    let run_del = create_run(&repository, tenant_id, connection_uid).await;
    let failed = pipeline
        .ingest_record_page(
            run_del,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("tok-del", true, "")],
                next_cursor: None,
            },
        )
        .await;
    assert!(
        failed.is_err(),
        "failed deletion invalidation must surface as an error"
    );
    assert_eq!(
        object_status(&pool, object_uid).await,
        "active",
        "terminal `deleted` must not be written before cleanup succeeds"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        2,
        "chunks stay active and selectable for a cleanup retry"
    );

    // Retry via prune: the still-active object is selected and deletion completes.
    graph.set_fail_invalidate(false);
    let run_prune = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .prune_unseen_objects(
            run_prune,
            connection_uid,
            tenant_id,
            &std::collections::HashSet::new(),
        )
        .await
        .expect("prune completes the stranded deletion");
    assert_eq!(
        object_status(&pool, object_uid).await,
        "deleted",
        "deletion completes once invalidation succeeds"
    );
    assert_eq!(
        active_chunk_count(&pool, object_uid).await,
        0,
        "all chunks are invalidated once the deletion completes"
    );
    assert_eq!(tombstoned_chunk_count(&pool, object_uid).await, 2);
}

#[tokio::test]
async fn semantic_graph_extraction_is_cached_reported_and_written_db_memory() {
    // Pins: semantic graph extraction is a persisted ingestion-time cache with
    // reported hit/miss counters and graph-visible typed edges.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder,
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_semantic".to_string(),
            credential_ref: "vault://knowledge/semantic".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let text = "Connecting a custom domain requires a premium plan and DNS records.\n\nTroubleshoot domain not working by checking DNS.";
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;

    let result = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![
                    record_with_source("semantic-a", "v1", false, text),
                    record_with_source("semantic-b", "v1", false, text),
                ],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest semantic records");

    assert_eq!(result.records_ingested, 2);
    let first_object_uid = object_uid_for_source(connection_uid, "semantic-a");
    let second_object_uid = object_uid_for_source(connection_uid, "semantic-b");
    let first_counters = semantic_graph_step_counters(&pool, sync_run_uid, first_object_uid).await;
    let second_counters =
        semantic_graph_step_counters(&pool, sync_run_uid, second_object_uid).await;
    assert_eq!(first_counters["chunks_total"], 2);
    assert_eq!(first_counters["cache_hits"], 0);
    assert_eq!(first_counters["cache_misses"], 2);
    assert_eq!(second_counters["chunks_total"], 2);
    assert_eq!(second_counters["cache_hits"], 2);
    assert_eq!(second_counters["cache_misses"], 0);
    assert!(first_counters["entities_extracted"].as_u64().unwrap_or(0) > 0);
    assert!(first_counters["relations_extracted"].as_u64().unwrap_or(0) > 0);
    assert!(first_counters["semantic_chunk_links"].as_u64().unwrap_or(0) > 0);
    assert_eq!(semantic_graph_cache_row_count(&pool, tenant_id).await, 2);

    let edge_json = graph.edge_properties_json();
    assert!(
        edge_json.contains("semantic_graph"),
        "semantic graph edge metadata should be written: {edge_json}"
    );
    assert!(
        edge_json.contains("RELATES_TO"),
        "same-document semantic chunk links should be graph-visible: {edge_json}"
    );
}

#[tokio::test]
async fn ingestion_preserves_chunk_structure_for_bounded_neighbor_context_db_memory() {
    // Pins: ingested chunks preserve document version, ordinal, heading path, and active status for bounded neighbor lookup.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder,
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_structure_audit".to_string(),
            credential_ref: "vault://knowledge/structure-audit".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "structure-token",
                    false,
                    "Eligibility alpha.\n\nApproval bravo.\n\nCarryover charlie.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest structure audit record");

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load latest version")
        .expect("ingestion should create a document version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks for version");
    assert_eq!(chunks.len(), 3);
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.version_uid)
            .collect::<Vec<_>>(),
        vec![
            version.version_uid,
            version.version_uid,
            version.version_uid
        ]
    );
    assert_eq!(
        chunks.iter().map(|chunk| chunk.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        chunks.iter().all(|chunk| chunk.graph_node_uid.is_some()),
        "{chunks:?}"
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.metadata["active"] == json!(true)),
        "{chunks:?}"
    );
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.heading_path.clone())
            .collect::<Vec<_>>(),
        vec![
            vec!["Alpha Plan".to_string()],
            vec!["Alpha Plan".to_string()],
            vec!["Alpha Plan".to_string()],
        ]
    );

    let adjacent = active_adjacent_chunk_rows(&pool, chunks[1].chunk_uid).await;
    assert_eq!(
        adjacent.iter().map(|row| row.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        adjacent
            .iter()
            .all(|row| row.version_uid == version.version_uid),
        "{adjacent:?}"
    );
    assert!(
        adjacent
            .iter()
            .all(|row| row.heading_path == vec!["Alpha Plan".to_string()]),
        "{adjacent:?}"
    );
    assert!(
        adjacent.iter().all(|row| row.active == "true"),
        "{adjacent:?}"
    );
    assert_eq!(
        adjacent
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Eligibility alpha.",
            "Approval bravo.",
            "Carryover charlie."
        ]
    );

    repository
        .tombstone_chunks(&[chunks[0].chunk_uid])
        .await
        .expect("tombstone previous chunk");
    let active_after_tombstone = active_adjacent_chunk_rows(&pool, chunks[1].chunk_uid).await;
    assert_eq!(
        active_after_tombstone
            .iter()
            .map(|row| row.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        active_after_tombstone
            .iter()
            .all(|row| row.active == "true"),
        "{active_after_tombstone:?}"
    );
}

#[tokio::test]
async fn ingestion_pipeline_replaying_same_page_keeps_counters_and_identities_once() {
    // Pins: replaying one provider page for the same sync run does not duplicate step counters or graph identities.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_replay".to_string(),
            credential_ref: "vault://knowledge/replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;

    let first = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "replay-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("first page apply should ingest");
    assert_eq!(first.records_listed, 1);
    assert_eq!(first.records_ingested, 1);
    assert_eq!(first.records_skipped, 0);
    assert_eq!(first.embeddings_created, 2);

    let object_uid = object_uid(connection_uid);
    let first_counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(first_counters.records_seen, 1);
    assert_eq!(first_counters.records_changed, 1);
    assert_eq!(first_counters.records_deleted, 0);
    assert_eq!(first_counters.records_ingested, 1);
    assert_eq!(first_counters.records_failed, 0);
    assert_eq!(first_counters.objects_parsed, 1);
    assert_eq!(first_counters.chunks_embedded, 2);
    assert!(first_counters.graph_nodes_upserted > 0);
    assert!(first_counters.graph_edges_upserted > 0);
    let first_identities = stored_identities(&pool, object_uid).await;
    assert_eq!(first_identities.object_uid, object_uid);

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "replay-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("replayed page apply should not fail on duplicate steps");
    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 0);
    assert_eq!(replay.records_skipped, 1);
    assert_eq!(replay.embeddings_created, 0);

    assert_eq!(sync_counters(&pool, sync_run_uid).await, first_counters);
    assert_eq!(stored_identities(&pool, object_uid).await, first_identities);
    assert_eq!(embedder.embedded_count(), 2);

    let steps = repository
        .sync_run_steps(sync_run_uid, Some(object_uid))
        .await
        .expect("read object steps");
    assert_eq!(
        steps
            .iter()
            .filter(|step| step.step == "object_change_checked")
            .count(),
        1
    );
    let object_change = steps
        .iter()
        .find(|step| step.step == "object_change_checked")
        .expect("object change step should exist");
    assert_eq!(object_change.counters["records_seen"], json!(1));
    assert_eq!(object_change.counters["records_changed"], json!(1));
}

#[tokio::test]
async fn ingestion_pipeline_duplicate_workers_coalesce_object_version_before_graph_writes_db_knowledge()
 {
    // Pins: duplicate object delivery from two workers stores one document version and performs one graph/vector write.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository_a = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope.clone(),
    ));
    let repository_b = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(BarrierParser {
        barrier: Arc::new(Barrier::new(2)),
    });
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline_a = KnowledgeIngestionPipeline::new(
        repository_a.clone(),
        parser.clone(),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );
    let pipeline_b = KnowledgeIngestionPipeline::new(
        repository_b.clone(),
        parser,
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository_a
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_duplicate_workers".to_string(),
            credential_ref: "vault://knowledge/duplicate-workers".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository_a, tenant_id, connection_uid).await;
    let page = RecordPage {
        records: vec![record(
            "duplicate-token",
            false,
            "Alpha is ready.\n\nBudget is 10.",
        )],
        next_cursor: None,
    };

    let (result_a, result_b) = tokio::join!(
        pipeline_a.ingest_record_page(sync_run_uid, connection_uid, tenant_id, page.clone()),
        pipeline_b.ingest_record_page(sync_run_uid, connection_uid, tenant_id, page)
    );
    let result_a = result_a.expect("first worker should finish");
    let result_b = result_b.expect("second worker should finish");
    assert_eq!(result_a.records_ingested + result_b.records_ingested, 1);
    assert_eq!(result_a.records_skipped + result_b.records_skipped, 1);

    let object_uid = object_uid(connection_uid);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        completed_ingestion_step_count(&pool, sync_run_uid, object_uid).await,
        1
    );
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.chunks_embedded, 2);
}

#[tokio::test]
async fn ingestion_pipeline_prunes_unseen_objects_after_full_selection_refresh() {
    // Pins: full selected-source refreshes tombstone previously active objects absent from the provider listing.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder,
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 4,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_prune".to_string(),
            credential_ref: "vault://knowledge/prune".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let initial_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            initial_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![
                    record_with_source("doc-keep", "v1", false, "Keep this source."),
                    record_with_source("doc-drop", "v1", false, "Drop this source."),
                ],
                next_cursor: None,
            },
        )
        .await
        .expect("ingest initial selected sources");
    assert_eq!(graph.vector_count(), 2);

    let prune_run = create_run(&repository, tenant_id, connection_uid).await;
    let seen_source_ids = HashSet::from(["doc-keep".to_string()]);
    let report = pipeline
        .prune_unseen_objects(prune_run, connection_uid, tenant_id, &seen_source_ids)
        .await
        .expect("prune sources absent from full refresh");

    let keep_uid = object_uid_for_source(connection_uid, "doc-keep");
    let drop_uid = object_uid_for_source(connection_uid, "doc-drop");
    assert_eq!(report.records_deleted, 1);
    assert_eq!(object_status(&pool, keep_uid).await, "active");
    assert_eq!(object_status(&pool, drop_uid).await, "deleted");
    assert_eq!(graph.vector_count(), 1);
    assert_eq!(graph.invalidated_count(), 1);
    assert_eq!(tombstoned_chunk_count(&pool, drop_uid).await, 1);
    assert_eq!(tombstoned_chunk_count(&pool, keep_uid).await, 0);

    let counters = sync_counters(&pool, prune_run).await;
    assert_eq!(counters.records_seen, 0);
    assert_eq!(counters.records_deleted, 1);
    let run_steps = repository
        .sync_run_steps(prune_run, None)
        .await
        .expect("read prune run timeline");
    let rendered_steps = run_steps
        .iter()
        .map(|step| format!("{} {}", step.step, step.counters))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        run_steps
            .iter()
            .any(|step| step.step == "source_selection_pruned"
                && step.counters["records_pruned"] == json!(1)),
        "prune run should record a run-level selected-source prune step; got {rendered_steps}"
    );
}

#[tokio::test]
async fn ingestion_pipeline_replay_after_change_token_only_progress_finishes_ingestion() {
    // Pins: replay after object/change-token advancement must resume missing parse, graph, vector, and final success work.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_partial_replay".to_string(),
            credential_ref: "vault://knowledge/partial-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("partial-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed partially advanced object row");
    repository
        .record_ingestion_step_once(
            KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid,
                object_uid: Some(object_uid),
                step: "object_change_checked".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(1),
                counters: json!({ "records_seen": 1, "records_changed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
            },
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
        .expect("seed object change step")
        .then_some(())
        .expect("seeded object change step should insert");
    assert_eq!(
        sync_counters(&pool, sync_run_uid).await,
        Counters {
            records_seen: 1,
            records_changed: 1,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
        }
    );

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "partial-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("partial-progress replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.records_failed, 0);
    assert_eq!(counters.objects_parsed, 1);
    assert_eq!(counters.chunks_embedded, 2);
    assert!(counters.graph_nodes_upserted > 0);
    assert!(counters.graph_edges_upserted > 0);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
}

#[tokio::test]
async fn ingestion_pipeline_reclaims_stale_started_claim_after_crash_db_knowledge() {
    // Pins: retrying unchanged content after a crash reclaims the stale claim and finishes graph/vector writes.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_stale_claim_replay".to_string(),
            credential_ref: "vault://knowledge/stale-claim-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    let text = "Alpha is ready.\n\nBudget is 10.";
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("stale-claim-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed object row advanced before crash");
    let normalized = normalize_text(text);
    let hash = content_hash(&normalized);
    let version = DocumentVersion {
        version_uid: moa_knowledge::graph_delta::stable_uid(&format!(
            "version:{object_uid}:{hash}"
        )),
        object_uid,
        parser: "test_parser".to_string(),
        parser_job_id: None,
        content_hash: hash,
        metadata: json!({ "crash": "after_claim" }),
        created_at: Utc::now(),
    };
    assert!(matches!(
        repository
            .claim_document_version_ingestion(sync_run_uid, version.clone())
            .await
            .expect("seed stale started claim"),
        moa_knowledge::repository::DocumentVersionIngestionClaim::Claimed { .. }
    ));
    expire_claim_lease(&pool, version.version_uid).await;

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("stale-claim-token", false, text)],
                next_cursor: None,
            },
        )
        .await
        .expect("stale-claim replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    assert_eq!(version_count(&pool, object_uid).await, 1);
    assert_eq!(chunk_count(&pool, object_uid).await, 2);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
    assert_eq!(graph.vector_count(), 2);
    assert_eq!(
        document_version_claim_status(&pool, version.version_uid).await,
        "completed"
    );
}

#[tokio::test]
async fn ingestion_pipeline_replay_after_graph_uid_midpoint_finishes_ingestion() {
    // Pins: graph_node_uid on chunks is not enough replay proof before final records_ingested step.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let parser = Arc::new(ParagraphParser);
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "acct_graph_uid_replay".to_string(),
            credential_ref: "vault://knowledge/graph-uid-replay".to_string(),
            status: ConnectionStatus::Active,
            metadata: credentialish_metadata(),
            source_selection: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        })
        .await
        .expect("upsert connection");
    let sync_run_uid = create_run(&repository, tenant_id, connection_uid).await;
    let object_uid = object_uid(connection_uid);
    repository
        .upsert_object(KnowledgeObject {
            object_uid,
            tenant_id,
            connection_uid,
            object_type: "page".to_string(),
            source_id: "doc-1".to_string(),
            parent_source_id: None,
            source_uri: Some("https://example.test/doc-1".to_string()),
            title: Some("Alpha Plan".to_string()),
            change_token: Some("graph-midpoint-token".to_string()),
            metadata: credentialish_metadata(),
            status: ObjectStatus::Active,
            source_updated_at: Some(Utc::now()),
            deleted_at: None,
        })
        .await
        .expect("seed partially advanced object row");
    repository
        .record_ingestion_step_once(
            KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid,
                object_uid: Some(object_uid),
                step: "object_change_checked".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                duration_ms: Some(1),
                counters: json!({ "records_seen": 1, "records_changed": 1 }),
                summary: None,
                retry_count: 0,
                error_code: None,
            },
            KnowledgeSyncCounters {
                records_seen: 1,
                records_changed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
        .expect("seed object change step")
        .then_some(())
        .expect("seeded object change step should insert");
    seed_graph_linked_partial_version(&repository, object_uid, "Alpha is ready.\n\nBudget is 10.")
        .await;

    let replay = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record(
                    "graph-midpoint-token",
                    false,
                    "Alpha is ready.\n\nBudget is 10.",
                )],
                next_cursor: None,
            },
        )
        .await
        .expect("graph-uid midpoint replay should finish ingestion");

    assert_eq!(replay.records_listed, 1);
    assert_eq!(replay.records_ingested, 1);
    assert_eq!(replay.records_skipped, 0);
    assert_eq!(replay.embeddings_created, 2);
    let counters = sync_counters(&pool, sync_run_uid).await;
    assert_eq!(counters.records_seen, 1);
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.records_failed, 0);
    assert_eq!(counters.objects_parsed, 1);
    assert_eq!(counters.chunks_embedded, 2);
    assert!(counters.graph_nodes_upserted > 0);
    assert!(counters.graph_edges_upserted > 0);
    assert_eq!(chunks_with_graph_uid(&pool, object_uid).await, 2);
    assert_eq!(embedder.embedded_count(), 2);
}

#[tokio::test]
async fn ingestion_pipeline_fetches_content_for_metadata_only_records_db_memory() {
    // Pins: a metadata-only provider record (no inline text, no fetchable URL)
    // has its content downloaded through the content fetcher, and the fetched
    // bytes are chunked and stored as real content instead of a title stub.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let embedder = Arc::new(CountingEmbedder::default());
    let graph = Arc::new(FakeGraphWriter::default());
    let fetched = b"Fetched alpha.\n\nFetched beta.".to_vec();
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(BytesOrTextParagraphParser),
        embedder.clone(),
        graph.clone(),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    )
    .with_content_fetcher(Some(Arc::new(FakeContentFetcher::new(
        FetchOutcome::Bytes(fetched.clone(), Some("text/plain".to_string())),
    ))));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let run = create_run(&repository, tenant_id, connection_uid).await;
    let report = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("metadata-only record should ingest via fetched content");
    assert_eq!(report.records_ingested, 1);

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load version")
        .expect("fetched content should create a version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>(),
        vec!["Fetched alpha.".to_string(), "Fetched beta.".to_string()],
        "chunks should carry fetched content, not the title"
    );

    let steps = repository
        .sync_run_steps(run, Some(object_uid))
        .await
        .expect("read object steps");
    let content_step = steps
        .iter()
        .find(|step| step.step == "content_fetched")
        .expect("content_fetched step should be recorded");
    assert_eq!(content_step.status, IngestionStepStatus::Completed);
    assert_eq!(content_step.error_code, None);
    assert_eq!(
        content_step.counters["bytes_fetched"],
        json!(fetched.len()),
        "content_fetched should report the fetched byte count"
    );
}

#[tokio::test]
async fn ingestion_pipeline_falls_back_to_title_when_content_fetch_fails_db_memory() {
    // Pins: a failed content fetch keeps the title-only fallback (record still
    // ingests, run not failed) but records a distinct content_fetch failure code
    // so it is not confused with a plain metadata-only record.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(BytesOrTextParagraphParser),
        Arc::new(CountingEmbedder::default()),
        Arc::new(FakeGraphWriter::default()),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    )
    .with_content_fetcher(Some(Arc::new(FakeContentFetcher::new(FetchOutcome::Error))));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let run = create_run(&repository, tenant_id, connection_uid).await;
    let report = pipeline
        .ingest_record_page(
            run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Fallback Title")],
                next_cursor: None,
            },
        )
        .await
        .expect("failed fetch should not fail the page");
    assert_eq!(report.records_ingested, 1);

    let object_uid = object_uid(connection_uid);
    let version = repository
        .latest_document_version(object_uid)
        .await
        .expect("load version")
        .expect("title fallback should create a version");
    let chunks = repository
        .chunks_for_version(version.version_uid)
        .await
        .expect("load chunks");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>(),
        vec!["Fallback Title".to_string()],
        "failed fetch should fall back to indexing the record title"
    );

    let steps = repository
        .sync_run_steps(run, Some(object_uid))
        .await
        .expect("read object steps");
    let content_step = steps
        .iter()
        .find(|step| step.step == "content_fetched")
        .expect("content_fetched step should be recorded");
    assert_eq!(content_step.status, IngestionStepStatus::Completed);
    assert_eq!(
        content_step.error_code.as_deref(),
        Some("provider_content_fetch_failed"),
        "failed fetch must be distinguishable from a plain metadata-only record"
    );

    let run_row = repository
        .get_sync_run(run)
        .await
        .expect("read run")
        .expect("run should exist");
    assert_eq!(run_row.records_failed, 0);
    assert!(
        !matches!(
            run_row.status,
            SyncRunStatus::FailedRetryable | SyncRunStatus::FailedTerminal
        ),
        "content fetch failure must not fail the run: {:?}",
        run_row.status
    );
}

#[tokio::test]
async fn ingestion_pipeline_skips_unchanged_fetched_content_without_refetching_db_memory() {
    // Pins: a metadata-only record whose content came from the fetch hook is not
    // re-fetched on an unchanged-change-token sync (no second fetch, no new
    // version), while a changed change token does trigger a re-fetch.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let fetcher = Arc::new(FakeContentFetcher::new(FetchOutcome::Bytes(
        b"Fetched alpha.\n\nFetched beta.".to_vec(),
        Some("text/plain".to_string()),
    )));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(BytesOrTextParagraphParser),
        Arc::new(CountingEmbedder::default()),
        Arc::new(FakeGraphWriter::default()),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    )
    .with_content_fetcher(Some(fetcher.clone()));

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    // First sync (token v1): fetches and ingests.
    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("first sync should ingest fetched content");
    assert_eq!(fetcher.calls(), 1);
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // Re-sync with the same change token: must skip without re-fetching.
    let unchanged_run = create_run(&repository, tenant_id, connection_uid).await;
    let unchanged = pipeline
        .ingest_record_page(
            unchanged_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v1", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("unchanged re-sync should succeed");
    assert_eq!(unchanged.records_skipped, 1);
    assert_eq!(unchanged.records_ingested, 0);
    assert_eq!(
        fetcher.calls(),
        1,
        "unchanged change token must not re-fetch content"
    );
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // A changed change token re-fetches (the provider signaled a change).
    let changed_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            changed_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![metadata_only_record("doc-1", "v2", "Roadmap")],
                next_cursor: None,
            },
        )
        .await
        .expect("changed-token re-sync should succeed");
    assert_eq!(
        fetcher.calls(),
        2,
        "a changed change token must re-fetch content"
    );
}

#[tokio::test]
async fn ingestion_pipeline_reingests_inline_edit_under_unchanged_token_db_memory() {
    // Pins: the version-hash guard still forces re-ingestion when an inline-text
    // record's content changes under an unchanged change token — the case the
    // hash comparison protects.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated Postgres");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let scope = RlsContext::tenant(tenant_id);
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        scope,
    ));
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        Arc::new(ParagraphParser),
        Arc::new(CountingEmbedder::default()),
        Arc::new(FakeGraphWriter::default()),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 1,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: "test_provider".to_string(),
            parser_label: "test_parser".to_string(),
        },
    );

    repository
        .upsert_connection(drive_connection(connection_uid, tenant_id))
        .await
        .expect("upsert connection");
    let object_uid = object_uid(connection_uid);

    let first_run = create_run(&repository, tenant_id, connection_uid).await;
    pipeline
        .ingest_record_page(
            first_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("same-token", false, "Alpha one.")],
                next_cursor: None,
            },
        )
        .await
        .expect("first inline sync should ingest");
    assert_eq!(version_count(&pool, object_uid).await, 1);

    // Same change token, edited inline text: the hash guard forces re-ingestion.
    let edit_run = create_run(&repository, tenant_id, connection_uid).await;
    let edited = pipeline
        .ingest_record_page(
            edit_run,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record("same-token", false, "Beta two.")],
                next_cursor: None,
            },
        )
        .await
        .expect("inline edit under unchanged token should re-ingest");
    assert_eq!(edited.records_ingested, 1);
    assert_eq!(edited.records_skipped, 0);
    assert_eq!(version_count(&pool, object_uid).await, 2);
}

fn drive_connection(connection_uid: Uuid, tenant_id: TenantId) -> KnowledgeConnection {
    KnowledgeConnection {
        connection_uid,
        tenant_id,
        provider: "test_provider".to_string(),
        connector: "google-drive".to_string(),
        provider_account_id: "acct_fetch".to_string(),
        credential_ref: "vault://knowledge/fetch".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({}),
        source_selection: json!({}),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_synced_at: None,
    }
}

fn metadata_only_record(source_id: &str, change_token: &str, title: &str) -> ProviderRecord {
    ProviderRecord {
        source_id: source_id.to_string(),
        object_type: "drive_file".to_string(),
        title: Some(title.to_string()),
        // Auth-walled browser viewer only; not a fetchable content URL.
        source_uri: Some(format!("https://drive.google.com/file/d/{source_id}/view")),
        change_token: Some(change_token.to_string()),
        deleted: false,
        source_updated_at: Some(Utc::now()),
        metadata: json!({ "safe": true }),
        payload: json!({ "mimeType": "text/plain", "name": format!("{title}.txt") }),
    }
}

async fn create_run(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> Uuid {
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("test_parser".to_string()),
            max_records: None,
            status: SyncRunStatus::Completed,
            records_seen: 0,
            records_changed: 0,
            records_deleted: 0,
            records_ingested: 0,
            records_failed: 0,
            objects_parsed: 0,
            chunks_embedded: 0,
            graph_nodes_upserted: 0,
            graph_edges_upserted: 0,
            error_code: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
        })
        .await
        .expect("create sync run");
    sync_run_uid
}

fn record(change_token: &str, deleted: bool, text: &str) -> ProviderRecord {
    record_with_source("doc-1", change_token, deleted, text)
}

fn record_with_source(
    source_id: &str,
    change_token: &str,
    deleted: bool,
    text: &str,
) -> ProviderRecord {
    ProviderRecord {
        source_id: source_id.to_string(),
        object_type: "page".to_string(),
        title: Some(if source_id == "doc-1" {
            "Alpha Plan".to_string()
        } else {
            format!("Source {source_id}")
        }),
        source_uri: Some(format!("https://example.test/{source_id}")),
        change_token: Some(change_token.to_string()),
        deleted,
        source_updated_at: Some(Utc::now()),
        metadata: credentialish_metadata(),
        payload: json!({
            "text": text,
            "access_token": "secret-token"
        }),
    }
}

fn credentialish_metadata() -> Value {
    json!({
        "mime_type": "text/plain",
        "safe": true,
        "access_token": "secret-token",
        "refresh_token": "secret-refresh",
        "api_key": "secret-api-key",
        "authorization": "Bearer secret-authorization",
        "client_secret": "secret-client",
        "password": "secret-password",
        "credential_ref": "raw-provider-credential",
        "nested": {
            "safe_nested": "ok",
            "session_header": "Bearer nested-token",
            "credentials": "raw-nested-credentials"
        },
        "items": [
            { "safe_item": "ok", "refresh_token": "secret-array-refresh" },
            "authorization: Bearer array-token"
        ]
    })
}

fn assert_no_secret_material(value: &Value) {
    assert_no_secret_text(&serde_json::to_string(value).expect("serialize metadata"));
}

fn assert_no_secret_text(text: &str) {
    for secret in [
        "access_token",
        "refresh_token",
        "api_key",
        "authorization",
        "client_secret",
        "password",
        "credential_ref",
        "credentials",
        "secret-token",
        "secret-refresh",
        "secret-api-key",
        "secret-authorization",
        "secret-client",
        "secret-password",
        "raw-provider-credential",
        "raw-nested-credentials",
        "array-token",
    ] {
        assert!(
            !text.contains(secret),
            "sanitized metadata should not contain `{secret}` in {text}"
        );
    }
}

fn object_uid(connection_uid: Uuid) -> Uuid {
    object_uid_for_source(connection_uid, "doc-1")
}

fn object_uid_for_source(connection_uid: Uuid, source_id: &str) -> Uuid {
    moa_knowledge::graph_delta::stable_uid(&format!(
        "knowledge-object:{connection_uid}:{source_id}"
    ))
}

async fn version_count(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.knowledge_document_versions WHERE object_id = $1",
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count versions")
}

async fn chunk_count(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
        "#,
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count chunks")
}

async fn completed_ingestion_step_count(
    pool: &sqlx::PgPool,
    sync_run_uid: Uuid,
    object_uid: Uuid,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_ingestion_steps
        WHERE sync_run_id = $1
          AND object_id = $2
          AND stage = 'contact_groups_derived'
          AND status = 'completed'
        "#,
    )
    .bind(sync_run_uid)
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count completed object ingestion steps")
}

async fn chunks_with_graph_uid(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
          AND c.graph_node_uid IS NOT NULL
        "#,
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count graph uid chunks")
}

async fn tombstoned_chunk_count(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
          AND c.metadata->>'active' = 'false'
        "#,
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count tombstoned chunks")
}

async fn active_chunk_count(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
          AND COALESCE(c.metadata->>'active', 'true') <> 'false'
        "#,
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count active chunks")
}

async fn object_status(pool: &sqlx::PgPool, object_uid: Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT status FROM moa.knowledge_objects WHERE object_uid = $1",
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("read object status")
}

async fn object_metadata(pool: &sqlx::PgPool, object_uid: Uuid) -> Value {
    sqlx::query_scalar::<_, Value>(
        "SELECT metadata FROM moa.knowledge_objects WHERE object_uid = $1",
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("read object metadata")
}

async fn connection_metadata(pool: &sqlx::PgPool, connection_uid: Uuid) -> Value {
    sqlx::query_scalar::<_, Value>(
        "SELECT metadata FROM moa.knowledge_connections WHERE connection_uid = $1",
    )
    .bind(connection_uid)
    .fetch_one(pool)
    .await
    .expect("read connection metadata")
}

async fn seed_graph_linked_partial_version(
    repository: &PostgresKnowledgeRepository,
    object_uid: Uuid,
    text: &str,
) {
    let normalized = normalize_text(text);
    let hash = content_hash(&normalized);
    let version = DocumentVersion {
        version_uid: moa_knowledge::graph_delta::stable_uid(&format!(
            "version:{object_uid}:{hash}"
        )),
        object_uid,
        parser: "test_parser".to_string(),
        parser_job_id: None,
        content_hash: hash.clone(),
        metadata: json!({ "partial": true }),
        created_at: Utc::now(),
    };
    repository
        .insert_document_version(version.clone())
        .await
        .expect("seed partial document version");
    repository
        .replace_chunks(
            version.version_uid,
            vec![KnowledgeChunk {
                chunk_uid: moa_knowledge::graph_delta::stable_uid(&format!(
                    "partial-chunk:{}:{hash}",
                    version.version_uid
                )),
                version_uid: version.version_uid,
                graph_node_uid: Some(moa_knowledge::graph_delta::stable_uid(&format!(
                    "partial-graph-node:{object_uid}:{hash}"
                ))),
                chunk_hash: format!("partial-{hash}"),
                block_hashes: vec![hash],
                text: text.to_string(),
                heading_path: Vec::new(),
                ordinal: 0,
                token_count: 1,
                metadata: json!({ "active": true, "partial": true }),
            }],
        )
        .await
        .expect("seed graph-linked partial chunk");
}

async fn expire_claim_lease(pool: &sqlx::PgPool, version_uid: Uuid) {
    let result = sqlx::query(
        r#"
        UPDATE moa.knowledge_object_ingestion_claims
        SET lease_expires_at = now() - INTERVAL '1 second',
            updated_at = now() - INTERVAL '1 second'
        WHERE document_version_id = $1
        "#,
    )
    .bind(version_uid)
    .execute(pool)
    .await
    .expect("expire ingestion claim lease");
    assert_eq!(result.rows_affected(), 1);
}

async fn document_version_claim_status(pool: &sqlx::PgPool, version_uid: Uuid) -> String {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT status
        FROM moa.knowledge_object_ingestion_claims
        WHERE document_version_id = $1
        "#,
    )
    .bind(version_uid)
    .fetch_one(pool)
    .await
    .expect("read ingestion claim status")
}

#[derive(Debug, PartialEq, Eq)]
struct AdjacentChunkAuditRow {
    version_uid: Uuid,
    ordinal: i32,
    heading_path: Vec<String>,
    active: String,
    text: String,
}

async fn active_adjacent_chunk_rows(
    pool: &sqlx::PgPool,
    anchor_chunk_uid: Uuid,
) -> Vec<AdjacentChunkAuditRow> {
    sqlx::query_as::<_, (Uuid, i32, Vec<String>, String, String)>(
        r#"
        WITH anchor AS (
            SELECT document_version_id, ordinal
            FROM moa.knowledge_chunks
            WHERE chunk_uid = $1
        )
        SELECT c.document_version_id, c.ordinal, c.heading_path,
               COALESCE(c.metadata->>'active', 'unset') AS active,
               c.text
        FROM moa.knowledge_chunks c
        JOIN anchor a
          ON a.document_version_id = c.document_version_id
        WHERE c.ordinal BETWEEN a.ordinal - 1 AND a.ordinal + 1
          AND c.metadata->>'active' IS DISTINCT FROM 'false'
        ORDER BY c.ordinal ASC
        "#,
    )
    .bind(anchor_chunk_uid)
    .fetch_all(pool)
    .await
    .expect("load active adjacent chunks")
    .into_iter()
    .map(
        |(version_uid, ordinal, heading_path, active, text)| AdjacentChunkAuditRow {
            version_uid,
            ordinal,
            heading_path,
            active,
            text,
        },
    )
    .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Counters {
    records_seen: i64,
    records_changed: i64,
    records_deleted: i64,
    records_ingested: i64,
    records_failed: i64,
    objects_parsed: i64,
    chunks_embedded: i64,
    graph_nodes_upserted: i64,
    graph_edges_upserted: i64,
}

async fn sync_counters(pool: &sqlx::PgPool, sync_run_uid: Uuid) -> Counters {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64, i64)>(
        r#"
        SELECT records_seen, records_changed, records_deleted, records_ingested, records_failed,
               objects_parsed, chunks_embedded, graph_nodes_upserted, graph_edges_upserted
        FROM moa.knowledge_sync_runs
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(sync_run_uid)
    .fetch_one(pool)
    .await
    .expect("read sync counters");
    Counters {
        records_seen: row.0,
        records_changed: row.1,
        records_deleted: row.2,
        records_ingested: row.3,
        records_failed: row.4,
        objects_parsed: row.5,
        chunks_embedded: row.6,
        graph_nodes_upserted: row.7,
        graph_edges_upserted: row.8,
    }
}

async fn semantic_graph_step_counters(
    pool: &sqlx::PgPool,
    sync_run_uid: Uuid,
    object_uid: Uuid,
) -> Value {
    sqlx::query_scalar::<_, Value>(
        r#"
        SELECT counters
        FROM moa.knowledge_ingestion_steps
        WHERE sync_run_id = $1
          AND object_id = $2
          AND stage = 'semantic_graph_extracted'
        "#,
    )
    .bind(sync_run_uid)
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("read semantic graph extraction counters")
}

async fn semantic_graph_cache_row_count(pool: &sqlx::PgPool, tenant_id: TenantId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_semantic_graph_extractions
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id.0)
    .fetch_one(pool)
    .await
    .expect("count semantic graph extraction cache rows")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredIdentities {
    object_uid: Uuid,
    version_uids: Vec<Uuid>,
    chunks: Vec<(Uuid, Option<Uuid>, String)>,
}

async fn stored_identities(pool: &sqlx::PgPool, object_uid: Uuid) -> StoredIdentities {
    let version_uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT document_version_uid
        FROM moa.knowledge_document_versions
        WHERE object_id = $1
        ORDER BY created_at ASC, document_version_uid ASC
        "#,
    )
    .bind(object_uid)
    .fetch_all(pool)
    .await
    .expect("read document version identities");
    let chunks = sqlx::query_as::<_, (Uuid, Option<Uuid>, String)>(
        r#"
        SELECT chunk_uid, graph_node_uid, chunk_hash
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
        ORDER BY c.ordinal ASC, c.chunk_uid ASC
        "#,
    )
    .bind(object_uid)
    .fetch_all(pool)
    .await
    .expect("read chunk identities");
    StoredIdentities {
        object_uid,
        version_uids,
        chunks,
    }
}

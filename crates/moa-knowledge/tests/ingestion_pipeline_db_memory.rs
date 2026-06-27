//! DB integration coverage for the tenant knowledge ingestion pipeline.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::RlsContext;
use moa_core::{TenantId, traits::EmbeddingProvider};
use moa_knowledge::{
    chunking::{ChunkingConfig, content_hash},
    domain::{
        ConnectionStatus, DocumentElement, DocumentElementKind, DocumentVersion,
        IngestionStepStatus, KnowledgeChunk, KnowledgeConnection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeSyncCounters, KnowledgeSyncRun, ObjectStatus, ParsedDocument,
        ProviderRecord, RecordPage, SyncRunStatus,
    },
    graph_delta::KnowledgeGraphDelta,
    ingestion::{
        GraphWriteReport, KnowledgeGraphWriter, KnowledgeIngestionPipeline,
        KnowledgeIngestionPipelineConfig,
    },
    normalize::normalize_text,
    observability::MetricsIngestionObserver,
    parser::DocumentParser,
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_test_support::postgres;
use serde_json::{Value, json};
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
    edges: Mutex<HashSet<Uuid>>,
    vectors: Mutex<HashSet<Uuid>>,
    invalidated: Mutex<Vec<Uuid>>,
}

impl FakeGraphWriter {
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
            if edges.insert(edge.uid) {
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
    let observer = Arc::new(MetricsIngestionObserver);
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph.clone(),
        observer,
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
    let observer = Arc::new(MetricsIngestionObserver);
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        observer,
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
    let observer = Arc::new(MetricsIngestionObserver);
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        observer,
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
    let observer = Arc::new(MetricsIngestionObserver);
    let pipeline = KnowledgeIngestionPipeline::new(
        repository.clone(),
        parser,
        embedder.clone(),
        graph,
        observer,
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
            status: SyncRunStatus::Ingesting,
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
            finished_at: None,
        })
        .await
        .expect("create sync run");
    sync_run_uid
}

fn record(change_token: &str, deleted: bool, text: &str) -> ProviderRecord {
    ProviderRecord {
        source_id: "doc-1".to_string(),
        object_type: "page".to_string(),
        title: Some("Alpha Plan".to_string()),
        source_uri: Some("https://example.test/doc-1".to_string()),
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
    moa_knowledge::graph_delta::stable_uid(&format!("knowledge-object:{connection_uid}:doc-1"))
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

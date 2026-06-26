//! DB integration coverage for the tenant knowledge ingestion pipeline.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{TenantId, traits::EmbeddingProvider};
use moa_knowledge::{
    chunking::ChunkingConfig,
    domain::{
        ConnectionStatus, DocumentElement, DocumentElementKind, KnowledgeConnection,
        KnowledgeSyncRun, ParsedDocument, ProviderRecord, RecordPage, SyncRunStatus,
    },
    graph_delta::KnowledgeGraphDelta,
    ingestion::{
        GraphWriteReport, KnowledgeGraphWriter, KnowledgeIngestionPipeline,
        KnowledgeIngestionPipelineConfig,
    },
    observability::MetricsIngestionObserver,
    parser::DocumentParser,
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_memory_types::ScopeContext;
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
    let scope = ScopeContext::tenant(tenant_id);
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
    assert_eq!(counters.records_changed, 1);
    assert_eq!(counters.records_ingested, 1);
    assert_eq!(counters.chunks_embedded, 1);

    let graph_json = graph.properties_json();
    assert_no_secret_text(&graph_json);
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

#[derive(Debug, PartialEq, Eq)]
struct Counters {
    records_changed: i64,
    records_ingested: i64,
    chunks_embedded: i64,
}

async fn sync_counters(pool: &sqlx::PgPool, sync_run_uid: Uuid) -> Counters {
    let row = sqlx::query_as::<_, (i64, i64, i64)>(
        r#"
        SELECT records_changed, records_ingested, chunks_embedded
        FROM moa.knowledge_sync_runs
        WHERE sync_run_uid = $1
        "#,
    )
    .bind(sync_run_uid)
    .fetch_one(pool)
    .await
    .expect("read sync counters");
    Counters {
        records_changed: row.0,
        records_ingested: row.1,
        chunks_embedded: row.2,
    }
}

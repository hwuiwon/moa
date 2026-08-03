//! DB coverage for tenant knowledge observability and failure semantics.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use moa_core::types::memory::RlsContext;
use moa_core::{error::MoaError, traits::EmbeddingProvider, types::identifiers::TenantId};
use moa_knowledge::{
    chunking::ChunkingConfig,
    domain::{
        DocumentElement, DocumentElementKind, IngestionStepStatus, KnowledgeConnection,
        KnowledgeIngestionStep, KnowledgeSyncCounters, KnowledgeSyncRun, ParsedDocument,
        ProviderRecord, ProviderRecordAcl, RecordPage, SyncRunStatus,
    },
    error::Error,
    graph_delta::KnowledgeGraphDelta,
    ingestion::{
        GraphWriteReport, KnowledgeGraphWriter, KnowledgeIngestionPipeline,
        KnowledgeIngestionPipelineConfig,
    },
    parser::DocumentParser,
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_test_support::postgres;
use serde_json::json;
use uuid::Uuid;

const SECRET_TOKEN: &str = "raw-provider-secret-token";
const RAW_DOCUMENT: &str = "RAW_DOCUMENT_TEXT_SHOULD_NOT_BE_IN_STEPS";

fn provider_record_acl() -> ProviderRecordAcl {
    ProviderRecordAcl {
        provider_revision: "fixture-acl-rev".to_string(),
        complete: true,
        entries: Vec::new(),
    }
}

#[derive(Debug, Clone, Copy)]
enum ParserMode {
    Ok,
    UnsupportedFormat,
}

#[derive(Debug)]
struct TestParser {
    mode: ParserMode,
}

#[async_trait]
impl DocumentParser for TestParser {
    async fn parse(
        &self,
        input: moa_knowledge::domain::ParseInput,
    ) -> moa_knowledge::Result<ParsedDocument> {
        if matches!(self.mode, ParserMode::UnsupportedFormat) {
            return Err(Error::UnsupportedFormat(
                "fixture unsupported format".to_string(),
            ));
        }
        let text = input
            .text
            .ok_or_else(|| Error::parser("test_parser", "missing fixture text"))?;
        Ok(ParsedDocument {
            parser: "test_parser".to_string(),
            parser_job_id: Some("safe-parser-job".to_string()),
            text: text.clone(),
            elements: vec![DocumentElement {
                element_id: "p0".to_string(),
                kind: DocumentElementKind::Paragraph,
                text,
                heading_path: vec!["Fixture".to_string()],
                ordinal: 0,
                page_number: None,
                layout: None,
                metadata: json!({ "safe": "parser" }),
            }],
            metadata: json!({ "safe": "parser" }),
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum EmbedderMode {
    Ok,
    Fail,
}

#[derive(Debug)]
struct TestEmbedder {
    mode: EmbedderMode,
}

#[async_trait]
impl EmbeddingProvider for TestEmbedder {
    fn model_id(&self) -> &str {
        "test-embedder"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        if matches!(self.mode, EmbedderMode::Fail) {
            return Err(MoaError::ProviderError(
                "fixture embedding backend unavailable".to_string(),
            ));
        }
        Ok(inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let mut vector = vec![0.0; 4];
                vector[index % 4] = 1.0;
                vector
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy)]
enum GraphMode {
    Ok,
    FailUpsert,
}

#[derive(Debug)]
struct TestGraphWriter {
    mode: GraphMode,
    upserts: Mutex<Vec<usize>>,
}

impl TestGraphWriter {
    fn new(mode: GraphMode) -> Self {
        Self {
            mode,
            upserts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl KnowledgeGraphWriter for TestGraphWriter {
    async fn upsert_delta(
        &self,
        delta: &KnowledgeGraphDelta,
        _embeddings: &HashMap<Uuid, Vec<f32>>,
        _embedding_model: &str,
        _embedding_model_version: i32,
    ) -> moa_knowledge::Result<GraphWriteReport> {
        if matches!(self.mode, GraphMode::FailUpsert) {
            return Err(Error::Repository(
                "fixture graph write unavailable".to_string(),
            ));
        }
        self.upserts
            .lock()
            .expect("graph upsert log should not be poisoned")
            .push(delta.nodes.len());
        Ok(GraphWriteReport {
            nodes_upserted: delta.nodes.len() as u64,
            edges_upserted: delta.edges.len() as u64,
            vector_rows_deleted: 0,
        })
    }

    async fn invalidate_chunks(
        &self,
        _graph_node_uids: &[Uuid],
    ) -> moa_knowledge::Result<GraphWriteReport> {
        Ok(GraphWriteReport::default())
    }
}

#[tokio::test]
async fn sync_failure_rows_status_error_codes_redaction_and_counter_order_db_knowledge() {
    // Pins: failed tenant knowledge steps persist safe status/error rows and sync counters are inserted, updated, and read in the right columns.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge observability DB");
    let pool = db.store().pool().clone();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = Arc::new(PostgresKnowledgeRepository::scoped_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    ));
    let connection_uid = Uuid::now_v7();
    repository
        .upsert_connection(KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: "test_provider".to_string(),
            connector: "docs".to_string(),
            provider_account_id: "account-observability".to_string(),
            metadata: json!({ "safe": "connection", "access_token": SECRET_TOKEN }),
            source_selection: json!({}),
            information_barrier: None,
            created_at: moa_test_support::fixtures::pg_now(),
            updated_at: moa_test_support::fixtures::pg_now(),
            last_synced_at: None,
        })
        .await
        .expect("insert fixture connection");

    let counter_run_uid = create_counter_seed_run(&repository, tenant_id, connection_uid).await;
    assert_counter_projection(
        &repository,
        counter_run_uid,
        CounterProjection {
            records_seen: 11,
            records_changed: 22,
            records_deleted: 33,
            records_ingested: 44,
            records_failed: 55,
            objects_parsed: 66,
            chunks_embedded: 77,
            graph_nodes_upserted: 88,
            graph_edges_upserted: 99,
        },
    )
    .await;
    repository
        .add_sync_counters(
            counter_run_uid,
            KnowledgeSyncCounters {
                records_seen: 100,
                records_changed: 200,
                records_deleted: 300,
                records_ingested: 400,
                records_failed: 500,
                objects_parsed: 600,
                chunks_embedded: 700,
                graph_nodes_upserted: 800,
                graph_edges_upserted: 900,
            },
        )
        .await
        .expect("add distinct counter values");
    assert_counter_projection(
        &repository,
        counter_run_uid,
        CounterProjection {
            records_seen: 111,
            records_changed: 222,
            records_deleted: 333,
            records_ingested: 444,
            records_failed: 555,
            objects_parsed: 666,
            chunks_embedded: 777,
            graph_nodes_upserted: 888,
            graph_edges_upserted: 999,
        },
    )
    .await;

    let provider_failure = run_failure_case(FailureCase {
        repository: repository.clone(),
        tenant_id,
        connection_uid,
        label: "provider",
        parser_mode: ParserMode::Ok,
        embedder_mode: EmbedderMode::Ok,
        graph_mode: GraphMode::Ok,
        record: provider_failure_record(),
    })
    .await;
    assert_failed_steps(
        &repository,
        provider_failure,
        SyncRunStatus::FailedTerminal,
        "provider_record_missing_text",
        &[
            (
                "provider_records_listed",
                IngestionStepStatus::Completed,
                None,
            ),
            ("source_acl_captured", IngestionStepStatus::Completed, None),
            (
                "object_change_checked",
                IngestionStepStatus::Completed,
                None,
            ),
            (
                "content_fetched",
                IngestionStepStatus::Failed,
                Some("provider_record_missing_text"),
            ),
        ],
    )
    .await;

    let parser_failure = run_failure_case(FailureCase {
        repository: repository.clone(),
        tenant_id,
        connection_uid,
        label: "parser",
        parser_mode: ParserMode::UnsupportedFormat,
        embedder_mode: EmbedderMode::Ok,
        graph_mode: GraphMode::Ok,
        record: content_record("parser"),
    })
    .await;
    assert_failed_steps(
        &repository,
        parser_failure,
        SyncRunStatus::FailedTerminal,
        "parser_unsupported_format",
        &[
            (
                "provider_records_listed",
                IngestionStepStatus::Completed,
                None,
            ),
            ("source_acl_captured", IngestionStepStatus::Completed, None),
            (
                "object_change_checked",
                IngestionStepStatus::Completed,
                None,
            ),
            ("content_fetched", IngestionStepStatus::Completed, None),
            ("parse_submitted", IngestionStepStatus::Completed, None),
            (
                "parse_completed",
                IngestionStepStatus::Failed,
                Some("parser_unsupported_format"),
            ),
        ],
    )
    .await;

    let embedder_failure = run_failure_case(FailureCase {
        repository: repository.clone(),
        tenant_id,
        connection_uid,
        label: "embedder",
        parser_mode: ParserMode::Ok,
        embedder_mode: EmbedderMode::Fail,
        graph_mode: GraphMode::Ok,
        record: content_record("embedder"),
    })
    .await;
    assert_failed_steps(
        &repository,
        embedder_failure,
        SyncRunStatus::FailedRetryable,
        "embedder_failed_retryable",
        &[
            (
                "provider_records_listed",
                IngestionStepStatus::Completed,
                None,
            ),
            ("source_acl_captured", IngestionStepStatus::Completed, None),
            (
                "object_change_checked",
                IngestionStepStatus::Completed,
                None,
            ),
            ("content_fetched", IngestionStepStatus::Completed, None),
            ("parse_submitted", IngestionStepStatus::Completed, None),
            ("parse_completed", IngestionStepStatus::Completed, None),
            ("normalized", IngestionStepStatus::Completed, None),
            ("blocks_diffed", IngestionStepStatus::Completed, None),
            ("chunks_diffed", IngestionStepStatus::Completed, None),
            (
                "embedded",
                IngestionStepStatus::Failed,
                Some("embedder_failed_retryable"),
            ),
        ],
    )
    .await;

    let graph_failure = run_failure_case(FailureCase {
        repository: repository.clone(),
        tenant_id,
        connection_uid,
        label: "graph",
        parser_mode: ParserMode::Ok,
        embedder_mode: EmbedderMode::Ok,
        graph_mode: GraphMode::FailUpsert,
        record: content_record("graph"),
    })
    .await;
    assert_failed_steps(
        &repository,
        graph_failure,
        SyncRunStatus::FailedRetryable,
        "graph_write_failed_retryable",
        &[
            (
                "provider_records_listed",
                IngestionStepStatus::Completed,
                None,
            ),
            ("source_acl_captured", IngestionStepStatus::Completed, None),
            (
                "object_change_checked",
                IngestionStepStatus::Completed,
                None,
            ),
            ("content_fetched", IngestionStepStatus::Completed, None),
            ("parse_submitted", IngestionStepStatus::Completed, None),
            ("parse_completed", IngestionStepStatus::Completed, None),
            ("normalized", IngestionStepStatus::Completed, None),
            ("blocks_diffed", IngestionStepStatus::Completed, None),
            ("chunks_diffed", IngestionStepStatus::Completed, None),
            ("embedded", IngestionStepStatus::Completed, None),
            (
                "graph_upserted",
                IngestionStepStatus::Failed,
                Some("graph_write_failed_retryable"),
            ),
        ],
    )
    .await;
}

async fn create_counter_seed_run(
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
            information_barrier: None,
            status: SyncRunStatus::Completed,
            records_seen: 11,
            records_changed: 22,
            records_deleted: 33,
            records_ingested: 44,
            records_failed: 55,
            objects_parsed: 66,
            chunks_embedded: 77,
            graph_nodes_upserted: 88,
            graph_edges_upserted: 99,
            error_code: Some("seed_error_code".to_string()),
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: Some(moa_test_support::fixtures::pg_now()),
            provider_trigger_completed_at: None,
        })
        .await
        .expect("create counter seed run");
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read counter seed run")
        .expect("counter seed run should exist");
    assert_eq!(run.error_code.as_deref(), Some("seed_error_code"));
    sync_run_uid
}

struct FailureCase {
    repository: Arc<PostgresKnowledgeRepository>,
    tenant_id: TenantId,
    connection_uid: Uuid,
    label: &'static str,
    parser_mode: ParserMode,
    embedder_mode: EmbedderMode,
    graph_mode: GraphMode,
    record: ProviderRecord,
}

async fn run_failure_case(case: FailureCase) -> Uuid {
    let FailureCase {
        repository,
        tenant_id,
        connection_uid,
        label,
        parser_mode,
        embedder_mode,
        graph_mode,
        record,
    } = case;
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("test_parser".to_string()),
            max_records: None,
            information_barrier: None,
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
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: None,
            provider_trigger_completed_at: None,
        })
        .await
        .expect("create failure run");
    let pipeline = KnowledgeIngestionPipeline::new(
        repository,
        Arc::new(TestParser { mode: parser_mode }),
        Arc::new(TestEmbedder {
            mode: embedder_mode,
        }),
        Arc::new(TestGraphWriter::new(graph_mode)),
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 4,
                max_tokens: 16,
                min_tokens: 1,
            },
            provider: format!("test_provider_{label}"),
            parser_label: "test_parser".to_string(),
        },
    );
    let error = pipeline
        .ingest_record_page(
            sync_run_uid,
            connection_uid,
            tenant_id,
            RecordPage {
                records: vec![record],
                next_cursor: None,
            },
        )
        .await
        .expect_err("failure fixture should fail ingestion");
    let message = error.to_string();
    assert!(
        !message.contains(SECRET_TOKEN),
        "error should not include fixture secret material: {message}"
    );
    sync_run_uid
}

async fn assert_failed_steps(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    expected_status: SyncRunStatus,
    expected_error_code: &str,
    expected_steps: &[(&str, IngestionStepStatus, Option<&str>)],
) {
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read failed sync run")
        .expect("failed sync run should exist");
    assert_eq!(run.status, expected_status);
    assert_eq!(run.error_code.as_deref(), Some(expected_error_code));
    assert_eq!(run.records_failed, 1);
    assert!(run.finished_at.is_some());

    let steps = repository
        .sync_run_steps(sync_run_uid, None)
        .await
        .expect("read failure steps");
    let actual = steps
        .iter()
        .map(|step| (step.step.as_str(), step.status, step.error_code.as_deref()))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected_steps);

    let failed = steps
        .iter()
        .find(|step| step.status == IngestionStepStatus::Failed)
        .expect("one failed step should be present");
    assert_eq!(failed.retry_count, 0);
    assert_eq!(failed.error_code.as_deref(), Some(expected_error_code));
    assert!(
        matches!(
            failed.summary.as_deref(),
            Some("retryable failure") | Some("terminal failure")
        ),
        "failed step should use a safe retry summary: {failed:?}"
    );
    assert_eq!(failed.counters, json!({}));
    assert_redacted_steps(&steps);
}

fn assert_redacted_steps(steps: &[KnowledgeIngestionStep]) {
    let rendered = serde_json::to_string(steps).expect("steps should serialize");
    for forbidden in [
        SECRET_TOKEN,
        RAW_DOCUMENT,
        "access_token",
        "provider_event_id",
        "parser_job_id",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "step rows should not contain `{forbidden}` in {rendered}"
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct CounterProjection {
    records_seen: u64,
    records_changed: u64,
    records_deleted: u64,
    records_ingested: u64,
    records_failed: u64,
    objects_parsed: u64,
    chunks_embedded: u64,
    graph_nodes_upserted: u64,
    graph_edges_upserted: u64,
}

async fn assert_counter_projection(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
    expected: CounterProjection,
) {
    let run = repository
        .get_sync_run(sync_run_uid)
        .await
        .expect("read counter projection run")
        .expect("counter projection run should exist");
    assert_eq!(run.records_seen, expected.records_seen);
    assert_eq!(run.records_changed, expected.records_changed);
    assert_eq!(run.records_deleted, expected.records_deleted);
    assert_eq!(run.records_ingested, expected.records_ingested);
    assert_eq!(run.records_failed, expected.records_failed);
    assert_eq!(run.objects_parsed, expected.objects_parsed);
    assert_eq!(run.chunks_embedded, expected.chunks_embedded);
    assert_eq!(run.graph_nodes_upserted, expected.graph_nodes_upserted);
    assert_eq!(run.graph_edges_upserted, expected.graph_edges_upserted);
}

fn provider_failure_record() -> ProviderRecord {
    ProviderRecord {
        acl: provider_record_acl(),
        source_id: format!("provider-missing-text-{}", Uuid::now_v7()),
        object_type: "page".to_string(),
        title: None,
        source_uri: Some("https://example.test/provider-missing-text".to_string()),
        change_token: Some("provider-v1".to_string()),
        deleted: false,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        metadata: json!({ "safe": "metadata", "access_token": SECRET_TOKEN }),
        payload: json!({ "safe": "payload", "access_token": SECRET_TOKEN }),
    }
}

fn content_record(label: &str) -> ProviderRecord {
    ProviderRecord {
        acl: provider_record_acl(),
        source_id: format!("{label}-{}", Uuid::now_v7()),
        object_type: "page".to_string(),
        title: Some(format!("Fixture {label}")),
        source_uri: Some(format!("https://example.test/{label}")),
        change_token: Some(format!("{label}-v1")),
        deleted: false,
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
        metadata: json!({ "safe": "metadata", "access_token": SECRET_TOKEN }),
        payload: json!({
            "text": format!("Safe fixture text for {label}. {RAW_DOCUMENT}"),
            "access_token": SECRET_TOKEN
        }),
    }
}

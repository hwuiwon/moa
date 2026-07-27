//! DB integration coverage for the tenant knowledge ingestion pipeline.

mod concurrency;
mod content_fetch;
mod deletion;
mod idempotency;
mod occurrence_identity;
mod semantic_graph;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use moa_core::types::memory::RlsContext;
use moa_core::{traits::EmbeddingProvider, types::identifiers::TenantId};
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

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
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

/// Embedder that returns one fewer vector than it was given, simulating a
/// provider that violates the embedding cardinality contract.
#[derive(Debug, Default)]
struct MiscountingEmbedder;

#[async_trait]
impl EmbeddingProvider for MiscountingEmbedder {
    fn model_id(&self) -> &str {
        "test-model"
    }

    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        let short = inputs.len().saturating_sub(1);
        Ok((0..short).map(|_| vec![0.0; 1024]).collect())
    }
}

#[derive(Debug, Default)]
struct FakeGraphWriter {
    nodes: Mutex<HashMap<Uuid, Value>>,
    edges: Mutex<HashMap<Uuid, (String, Value)>>,
    /// Every applied edge as `(from_key, to_key, relationship)`, so occurrence
    /// tests can assert which chunk an edge actually attaches to instead of only
    /// counting edges.
    edge_keys: Mutex<Vec<(String, String, String)>>,
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

    fn has_vector(&self, uid: Uuid) -> bool {
        self.vectors
            .lock()
            .expect("vector mutex should not be poisoned")
            .contains(&uid)
    }

    fn has_node(&self, uid: Uuid) -> bool {
        self.nodes
            .lock()
            .expect("node mutex should not be poisoned")
            .contains_key(&uid)
    }

    fn node_properties(&self, uid: Uuid) -> Option<Value> {
        self.nodes
            .lock()
            .expect("node mutex should not be poisoned")
            .get(&uid)
            .cloned()
    }

    fn invalidated_uids(&self) -> Vec<Uuid> {
        self.invalidated
            .lock()
            .expect("invalidated mutex should not be poisoned")
            .clone()
    }

    /// Returns the `from_key`s of applied edges with `relationship` pointing at
    /// `to_key`.
    fn edge_sources_into(&self, to_key: &str, relationship: &str) -> Vec<String> {
        self.edge_keys
            .lock()
            .expect("edge key mutex should not be poisoned")
            .iter()
            .filter(|(_, edge_to, edge_relationship)| {
                edge_to == to_key && edge_relationship == relationship
            })
            .map(|(from, _, _)| from.clone())
            .collect()
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
        let mut edge_keys = self
            .edge_keys
            .lock()
            .expect("edge key mutex should not be poisoned");
        let mut edge_count = 0_u64;
        for edge in &delta.edges {
            if edges
                .insert(
                    edge.uid,
                    (edge.relationship.clone(), edge.properties.clone()),
                )
                .is_none()
            {
                edge_keys.push((
                    edge.from_key.clone(),
                    edge.to_key.clone(),
                    edge.relationship.clone(),
                ));
                edge_count += 1;
            }
        }
        drop(edge_keys);
        drop(edges);

        // Mirror the production writer: a vector row is attached to the graph node
        // whose uid the embedding is keyed by, so an embedding whose uid matches no
        // node in the delta writes nothing.
        let mut vectors = self
            .vectors
            .lock()
            .expect("vector mutex should not be poisoned");
        for node in &delta.nodes {
            if embeddings.contains_key(&node.uid) {
                vectors.insert(node.uid);
            }
        }
        drop(vectors);
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

fn drive_connection(connection_uid: Uuid, tenant_id: TenantId) -> KnowledgeConnection {
    KnowledgeConnection {
        connection_uid,
        tenant_id,
        provider: "test_provider".to_string(),
        connector: "google-drive".to_string(),
        provider_account_id: "acct_fetch".to_string(),
        credential_ref: "cb57cc63-b5cf-a112-f438-761700b5648c".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({}),
        source_selection: json!({}),
        information_barrier: None,
        created_at: moa_test_support::fixtures::pg_now(),
        updated_at: moa_test_support::fixtures::pg_now(),
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
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
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
            information_barrier: None,
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
            started_at: moa_test_support::fixtures::pg_now(),
            finished_at: Some(moa_test_support::fixtures::pg_now()),
            provider_trigger_completed_at: None,
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
        source_updated_at: Some(moa_test_support::fixtures::pg_now()),
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

/// Counts this object's chunk rows whose persisted graph identity is their own
/// occurrence identity.
///
/// A caller asserting this equals the chunk count is asserting the occurrence
/// invariant end to end: identity was written by ingestion (not recomputed from
/// a tenant-plus-content-hash seed) and survived storage.
async fn chunks_with_occurrence_identity(pool: &sqlx::PgPool, object_uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
          AND c.graph_node_uid = c.chunk_uid
        "#,
    )
    .bind(object_uid)
    .fetch_one(pool)
    .await
    .expect("count occurrence-identity chunks")
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
        created_at: moa_test_support::fixtures::pg_now(),
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

/// One persisted chunk occurrence, as storage sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OccurrenceRow {
    version_uid: Uuid,
    chunk_uid: Uuid,
    graph_node_uid: Uuid,
    chunk_hash: String,
    ordinal: i32,
    active: bool,
}

/// Loads every chunk occurrence of one object in (version, ordinal) order,
/// across all document versions.
///
/// Occurrence tests read this instead of a per-version query because the
/// behaviour under test is precisely that older versions keep their own
/// occurrences until they are invalidated.
async fn occurrence_rows(pool: &sqlx::PgPool, object_uid: Uuid) -> Vec<OccurrenceRow> {
    sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, i32, bool)>(
        r#"
        SELECT c.document_version_id,
               c.chunk_uid,
               c.graph_node_uid,
               c.chunk_hash,
               c.ordinal,
               COALESCE(c.metadata->>'active', 'true') <> 'false' AS active
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        WHERE v.object_id = $1
        ORDER BY v.created_at ASC, v.document_version_uid ASC, c.ordinal ASC
        "#,
    )
    .bind(object_uid)
    .fetch_all(pool)
    .await
    .expect("load chunk occurrences")
    .into_iter()
    .map(
        |(version_uid, chunk_uid, graph_node_uid, chunk_hash, ordinal, active)| OccurrenceRow {
            version_uid,
            chunk_uid,
            graph_node_uid,
            chunk_hash,
            ordinal,
            active,
        },
    )
    .collect()
}

/// Reads one ingestion step's counters for an object.
async fn ingestion_step_counters(
    pool: &sqlx::PgPool,
    sync_run_uid: Uuid,
    object_uid: Uuid,
    stage: &str,
) -> Value {
    sqlx::query_scalar::<_, Value>(
        r#"
        SELECT counters
        FROM moa.knowledge_ingestion_steps
        WHERE sync_run_id = $1
          AND object_id = $2
          AND stage = $3
        "#,
    )
    .bind(sync_run_uid)
    .bind(object_uid)
    .bind(stage)
    .fetch_one(pool)
    .await
    .expect("read ingestion step counters")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredIdentities {
    object_uid: Uuid,
    version_uids: Vec<Uuid>,
    chunks: Vec<(Uuid, Uuid, String)>,
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
    let chunks = sqlx::query_as::<_, (Uuid, Uuid, String)>(
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

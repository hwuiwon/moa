//! Shared deterministic fixtures for slow-path ingestion integration tests.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ScopeContext, ScopedConn, SessionId, UserId, WorkspaceId, traits::EmbeddingProvider,
};
use moa_memory_graph::{
    AgeGraphStore, GraphStore, NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_ingest::{IngestCtx, RrfPlusJudgeDetector, SessionTurn};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

pub(crate) static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone)]
pub(crate) struct MockEmbedder;

#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    fn model_id(&self) -> &str {
        "mock-slow-embedder"
    }

    fn model_version(&self) -> i32 {
        11
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FixedPiiClassifier {
    pub(crate) class: PiiClass,
}

#[async_trait]
impl PiiClassifier for FixedPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult {
            class: self.class,
            spans: Vec::<PiiSpan>::new(),
            model_version: "mock-pii".to_string(),
            abstained: false,
        })
    }
}

#[derive(Debug)]
pub(crate) struct FailOnNthPiiClassifier {
    nth: usize,
    calls: AtomicUsize,
}

impl FailOnNthPiiClassifier {
    pub(crate) fn new(nth: usize) -> Self {
        Self {
            nth,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl PiiClassifier for FailOnNthPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.nth {
            return Err(PiiError::Inference(format!(
                "intentional pre-write failure at fact {call}"
            )));
        }
        Ok(PiiResult {
            class: PiiClass::None,
            spans: Vec::<PiiSpan>::new(),
            model_version: "mock-pii".to_string(),
            abstained: false,
        })
    }
}

pub(crate) async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_TEST_POSTGRES_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

pub(crate) fn deterministic_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

pub(crate) fn ingest_ctx(pool: &PgPool, workspace_id: Uuid) -> IngestCtx {
    ingest_ctx_with_pii(
        pool,
        workspace_id,
        Arc::new(FixedPiiClassifier {
            class: PiiClass::None,
        }),
    )
}

pub(crate) fn ingest_ctx_with_pii(
    pool: &PgPool,
    workspace_id: Uuid,
    pii: Arc<dyn PiiClassifier>,
) -> IngestCtx {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph = Arc::new(
        AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(vector.clone()),
    );
    IngestCtx::new(
        pool.clone(),
        graph,
        vector,
        Arc::new(MockEmbedder),
        pii,
        Arc::new(RrfPlusJudgeDetector::default()),
    )
}

pub(crate) fn turn(
    workspace_id: Uuid,
    transcript: impl Into<String>,
    turn_seq: u64,
) -> SessionTurn {
    SessionTurn {
        workspace_id: WorkspaceId::new(workspace_id.to_string()),
        user_id: UserId::new("slow-path-user"),
        session_id: SessionId::new(),
        turn_seq,
        transcript: transcript.into(),
        dominant_pii_class: "none".to_string(),
        finalized_at: fixed_time(),
    }
}

pub(crate) fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-05-07T12:00:00Z")
        .expect("fixed test time parses")
        .with_timezone(&Utc)
}

pub(crate) fn fact_intent(
    workspace_id: Uuid,
    name: &str,
    valid_from: DateTime<Utc>,
) -> NodeWriteIntent {
    let mut words = name.split_whitespace();
    let subject = words.next().unwrap_or("fact");
    let predicate = words.next().unwrap_or("states");
    let object = words.collect::<Vec<_>>().join(" ");
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: name.to_string(),
        properties: json!({
            "summary": name,
            "subject": subject,
            "predicate": predicate,
            "object": object,
            "source": "slow_path_test_seed",
        }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from,
        embedding: Some(deterministic_vector(name)),
        embedding_model: Some("mock-slow-embedder".to_string()),
        embedding_model_version: Some(11),
        actor_id: "slow-path-test".to_string(),
        actor_kind: "system".to_string(),
    }
}

pub(crate) async fn create_fact(
    pool: &PgPool,
    workspace_id: Uuid,
    name: &str,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), ctx.clone());
    let graph =
        AgeGraphStore::scoped_for_app_role(pool.clone(), ctx).with_vector_store(Arc::new(vector));
    graph
        .create_node(fact_intent(workspace_id, name, valid_from))
        .await
        .expect("seed fact node")
}

pub(crate) async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: Uuid) -> ScopedConn<'a> {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

pub(crate) async fn fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, workspace_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at \
         FROM moa.node_index WHERE workspace_id = $1 AND label = 'Fact' ORDER BY name",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read fact rows");
    conn.commit().await.expect("commit fact row read");
    rows
}

pub(crate) async fn active_fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    fact_rows(pool, workspace_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn node_valid_to(
    pool: &PgPool,
    workspace_id: Uuid,
    uid: Uuid,
) -> Option<DateTime<Utc>> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let valid_to = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT valid_to FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read valid_to");
    conn.commit().await.expect("commit valid_to read");
    valid_to
}

pub(crate) async fn node_confidence(pool: &PgPool, workspace_id: Uuid, uid: Uuid) -> f64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let confidence = sqlx::query_scalar::<_, Option<f64>>(
        "SELECT confidence FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node confidence")
    .expect("confidence should be set");
    conn.commit().await.expect("commit confidence read");
    confidence
}

pub(crate) async fn contradiction_edge_count(pool: &PgPool, workspace_id: Uuid) -> i64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND target_label = 'CONTRADICTS'",
    )
    .bind(workspace_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count contradicts edges");
    conn.commit().await.expect("commit contradicts edge count");
    count
}

pub(crate) async fn create_changelog_payloads(pool: &PgPool, workspace_id: Uuid) -> Vec<Value> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query(
        "SELECT payload FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'create' AND target_kind = 'node' \
         ORDER BY change_id",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read create changelog payloads");
    conn.commit().await.expect("commit changelog read");
    rows.into_iter()
        .map(|row| row.try_get::<Value, _>("payload").expect("payload json"))
        .collect()
}

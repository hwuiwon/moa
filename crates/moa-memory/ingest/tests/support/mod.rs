//! Shared deterministic fixtures for slow-path ingestion integration tests.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ContactId, ScopeContext, ScopedConn, SessionId, TenantId, traits::EmbeddingProvider,
};
use moa_memory_graph::{
    AgeGraphStore, GraphStore, NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass, cypher,
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
pub(crate) const SLOW_PATH_CONTACT_ID: &str = "00000000-0000-0000-0000-0000000510a7";

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
    std::env::var_os("MOA_DATABASE_URL")?;
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

pub(crate) async fn ingest_ctx(pool: &PgPool, workspace_id: Uuid) -> IngestCtx {
    ingest_ctx_with_pii(
        pool,
        workspace_id,
        Arc::new(FixedPiiClassifier {
            class: PiiClass::None,
        }),
    )
    .await
}

pub(crate) async fn ingest_ctx_with_pii(
    pool: &PgPool,
    workspace_id: Uuid,
    pii: Arc<dyn PiiClassifier>,
) -> IngestCtx {
    seed_workspace_embedder_state(pool, workspace_id).await;
    let scope = ScopeContext::tenant(TenantId::from(workspace_id));
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
        tenant_id: TenantId::from(workspace_id),
        contact_id: slow_path_contact_id(),
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
        scope: "tenant".to_string(),
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
    seed_workspace_embedder_state(pool, workspace_id).await;
    let ctx = ScopeContext::tenant(TenantId::from(workspace_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), ctx.clone());
    let graph =
        AgeGraphStore::scoped_for_app_role(pool.clone(), ctx).with_vector_store(Arc::new(vector));
    graph
        .create_node(fact_intent(workspace_id, name, valid_from))
        .await
        .expect("seed fact node")
}

async fn seed_workspace_embedder_state(pool: &PgPool, workspace_id: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state
            (workspace_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'mock-slow-embedder', 11, $2)
        ON CONFLICT (workspace_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed workspace embedder state");
    conn.commit()
        .await
        .expect("commit workspace embedder state");
}

pub(crate) async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: Uuid) -> ScopedConn<'a> {
    let scope = ScopeContext::tenant(TenantId::from(workspace_id));
    scoped_conn_for_scope(pool, scope).await
}

pub(crate) async fn user_scoped_conn<'a>(pool: &'a PgPool, workspace_id: Uuid) -> ScopedConn<'a> {
    let scope = ScopeContext::contact(TenantId::from(workspace_id), slow_path_contact_id());
    scoped_conn_for_scope(pool, scope).await
}

fn slow_path_contact_id() -> ContactId {
    ContactId(Uuid::from_u128(0x5_10a7))
}

async fn scoped_conn_for_scope<'a>(pool: &'a PgPool, scope: ScopeContext) -> ScopedConn<'a> {
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
    workspace_fact_rows(pool, workspace_id).await
}

pub(crate) async fn workspace_fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, workspace_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE workspace_id = $1 AND label = 'Fact' AND scope = 'tenant' \
         ORDER BY name",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read fact rows");
    conn.commit().await.expect("commit fact row read");
    rows
}

pub(crate) async fn user_fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, workspace_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE workspace_id = $1 AND user_id = $2 AND label = 'Fact' AND scope = 'contact' \
         ORDER BY name",
    )
    .bind(workspace_id.to_string())
    .bind(SLOW_PATH_CONTACT_ID)
    .fetch_all(conn.as_mut())
    .await
    .expect("read user fact rows");
    conn.commit().await.expect("commit user fact row read");
    rows
}

pub(crate) async fn entity_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    workspace_entity_rows(pool, workspace_id).await
}

pub(crate) async fn workspace_entity_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, workspace_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE workspace_id = $1 AND label = 'Entity' AND scope = 'tenant' \
         ORDER BY name",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read entity rows");
    conn.commit().await.expect("commit entity row read");
    rows
}

pub(crate) async fn user_entity_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, workspace_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE workspace_id = $1 AND user_id = $2 AND label = 'Entity' AND scope = 'contact' \
         ORDER BY name",
    )
    .bind(workspace_id.to_string())
    .bind(SLOW_PATH_CONTACT_ID)
    .fetch_all(conn.as_mut())
    .await
    .expect("read user entity rows");
    conn.commit().await.expect("commit user entity row read");
    rows
}

pub(crate) async fn active_fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    fact_rows(pool, workspace_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_user_fact_rows(pool: &PgPool, workspace_id: Uuid) -> Vec<NodeIndexRow> {
    user_fact_rows(pool, workspace_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_workspace_fact_rows(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Vec<NodeIndexRow> {
    workspace_fact_rows(pool, workspace_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_user_entity_rows(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Vec<NodeIndexRow> {
    user_entity_rows(pool, workspace_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_workspace_entity_rows(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Vec<NodeIndexRow> {
    workspace_entity_rows(pool, workspace_id)
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
    let mut conn = user_scoped_conn(pool, workspace_id).await;
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
    let mut conn = user_scoped_conn(pool, workspace_id).await;
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

pub(crate) async fn supersede_protocol_count(pool: &PgPool, workspace_id: Uuid) -> i64 {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'supersede' AND target_kind = 'node' \
           AND target_label = 'Fact'",
    )
    .bind(workspace_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count supersede protocol rows");
    conn.commit()
        .await
        .expect("commit supersede protocol count");
    count
}

pub(crate) async fn supersedes_edge_exists(
    pool: &PgPool,
    workspace_id: Uuid,
    old_uid: Uuid,
    new_uid: Uuid,
) -> bool {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let row = cypher::edge::SUPERSEDES_EXISTS
        .execute(&json!({
            "old_uid": old_uid.to_string(),
            "new_uid": new_uid.to_string(),
        }))
        .fetch_optional(conn.as_mut())
        .await
        .expect("query supersedes edge");
    conn.commit().await.expect("commit supersedes edge read");
    row.is_some()
}

pub(crate) async fn relates_to_edges(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Vec<(String, String, String)> {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query(
        "SELECT payload->>'start_uid' AS start_uid, \
                payload->>'end_uid' AS end_uid, \
                payload->'after'->>'role' AS role \
         FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND target_label = 'RELATES_TO' \
         ORDER BY change_id",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read relates_to edges");
    conn.commit().await.expect("commit relates_to edge read");
    rows.into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("start_uid")
                    .expect("start uid in edge payload"),
                row.try_get::<String, _>("end_uid")
                    .expect("end uid in edge payload"),
                row.try_get::<String, _>("role")
                    .expect("role in edge payload"),
            )
        })
        .collect()
}

pub(crate) async fn entity_resolution_edges(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Vec<(String, String, String, String)> {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query(
        "SELECT target_label, \
                payload->>'start_uid' AS start_uid, \
                payload->>'end_uid' AS end_uid, \
                payload->'after'->>'role' AS role \
         FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND payload->'after'->>'source' = 'slow_path_entity_resolution' \
         ORDER BY change_id",
    )
    .bind(workspace_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read entity-resolution edges");
    conn.commit()
        .await
        .expect("commit entity-resolution edge read");
    rows.into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>("target_label")
                    .expect("edge label in changelog"),
                row.try_get::<String, _>("start_uid")
                    .expect("start uid in edge payload"),
                row.try_get::<String, _>("end_uid")
                    .expect("end uid in edge payload"),
                row.try_get::<String, _>("role")
                    .expect("role in edge payload"),
            )
        })
        .collect()
}

pub(crate) async fn create_changelog_payloads(pool: &PgPool, workspace_id: Uuid) -> Vec<Value> {
    let mut conn = user_scoped_conn(pool, workspace_id).await;
    let rows = sqlx::query(
        "SELECT payload FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'create' AND target_kind = 'node' \
           AND target_label = 'Fact' \
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

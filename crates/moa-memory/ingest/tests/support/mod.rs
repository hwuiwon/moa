//! Shared deterministic fixtures for slow-path ingestion integration tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::EmbeddingProvider, types::contact::ContactId, types::identifiers::SessionId,
    types::identifiers::TenantId,
};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, NodeIndexRow, NodeLabel, NodeWriteIntent, PostgresGraphStore};
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

    async fn embed(&self, texts: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FixedPiiClassifier {
    pub(crate) class: SensitivityClass,
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

#[derive(Debug, Clone)]
pub(crate) struct FailClosedPiiClassifier {
    model_version: String,
}

impl FailClosedPiiClassifier {
    pub(crate) fn new(model_version: impl Into<String>) -> Self {
        Self {
            model_version: model_version.into(),
        }
    }
}

#[async_trait]
impl PiiClassifier for FailClosedPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult::fail_closed(self.model_version.clone()))
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
            class: SensitivityClass::None,
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

pub(crate) async fn ingest_ctx(pool: &PgPool, storage_partition_id: Uuid) -> IngestCtx {
    ingest_ctx_with_pii(
        pool,
        storage_partition_id,
        Arc::new(FixedPiiClassifier {
            class: SensitivityClass::None,
        }),
    )
    .await
}

pub(crate) async fn ingest_ctx_with_pii(
    pool: &PgPool,
    storage_partition_id: Uuid,
    pii: Arc<dyn PiiClassifier>,
) -> IngestCtx {
    seed_workspace_embedder_state(pool, storage_partition_id).await;
    let scope = RlsContext::tenant(TenantId::from(storage_partition_id));
    let kms = test_kms();
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, kms.clone())
            .with_vector_store(vector.clone()),
    );
    IngestCtx::new(
        pool.clone(),
        kms,
        graph,
        vector,
        Arc::new(MockEmbedder),
        pii,
        Arc::new(RrfPlusJudgeDetector::default()),
    )
}

pub(crate) fn turn(
    storage_partition_id: Uuid,
    transcript: impl Into<String>,
    turn_seq: u64,
) -> SessionTurn {
    SessionTurn {
        tenant_id: TenantId::from(storage_partition_id),
        contact_id: Some(slow_path_contact_id()),
        session_id: SessionId::new(),
        turn_seq,
        transcript: transcript.into(),
        dominant_pii_class: "none".to_string(),
        finalized_at: fixed_time(),
        barrier: None,
    }
}

/// Builds a contact-scoped turn tagged with an information barrier so ingestion
/// writes need-to-know-restricted graph nodes.
pub(crate) fn barriered_turn(
    storage_partition_id: Uuid,
    transcript: impl Into<String>,
    turn_seq: u64,
    barrier: &str,
) -> SessionTurn {
    SessionTurn {
        barrier: Some(
            moa_core::types::memory::InformationBarrierId::parse(barrier).expect("valid barrier"),
        ),
        ..turn(storage_partition_id, transcript, turn_seq)
    }
}

pub(crate) fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-05-07T12:00:00Z")
        .expect("fixed test time parses")
        .with_timezone(&Utc)
}

pub(crate) fn fact_intent(
    storage_partition_id: Uuid,
    name: &str,
    valid_from: DateTime<Utc>,
) -> NodeWriteIntent {
    let mut words = name.split_whitespace();
    let subject = words.next().unwrap_or("fact");
    let predicate = words.next().unwrap_or("states");
    let object = words.collect::<Vec<_>>().join(" ");
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: Uuid::parse_str(SLOW_PATH_CONTACT_ID)
            .expect("slow-path contact fixture should be a UUID"),
        label: NodeLabel::Fact,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: Some(SLOW_PATH_CONTACT_ID.to_string()),
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({
            "summary": name,
            "subject": subject,
            "predicate": predicate,
            "object": object,
            "source": "slow_path_test_seed",
        }),
        pii_class: SensitivityClass::None,
        confidence: Some(0.9),
        valid_from,
        embedding: Some(deterministic_vector(name)),
        embedding_model: Some("mock-slow-embedder".to_string()),
        embedding_model_version: Some(11),
        embedding_text: None,
        actor_id: "slow-path-test".to_string(),
        actor_kind: "system".to_string(),
    }
}

pub(crate) async fn create_fact(
    pool: &PgPool,
    storage_partition_id: Uuid,
    name: &str,
    valid_from: DateTime<Utc>,
) -> Uuid {
    seed_workspace_embedder_state(pool, storage_partition_id).await;
    let ctx = RlsContext::contact(TenantId::from(storage_partition_id), slow_path_contact_id());
    let vector = PgvectorStore::new_for_app_role(pool.clone(), ctx.clone());
    let graph = PostgresGraphStore::scoped_for_app_role(pool.clone(), ctx, test_kms())
        .with_vector_store(Arc::new(vector));
    graph
        .create_node(fact_intent(storage_partition_id, name, valid_from))
        .await
        .expect("seed fact node")
}

async fn seed_workspace_embedder_state(pool: &PgPool, storage_partition_id: Uuid) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'mock-slow-embedder', 11, $2)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed workspace embedder state");
    conn.commit()
        .await
        .expect("commit workspace embedder state");
}

pub(crate) async fn scoped_conn<'a>(
    pool: &'a PgPool,
    storage_partition_id: Uuid,
) -> ScopedConn<'a> {
    let scope = RlsContext::tenant(TenantId::from(storage_partition_id));
    scoped_conn_for_scope(pool, scope).await
}

pub(crate) async fn user_scoped_conn<'a>(
    pool: &'a PgPool,
    storage_partition_id: Uuid,
) -> ScopedConn<'a> {
    let scope = RlsContext::contact(TenantId::from(storage_partition_id), slow_path_contact_id());
    scoped_conn_for_scope(pool, scope).await
}

fn slow_path_contact_id() -> ContactId {
    ContactId(Uuid::from_u128(0x5_10a7))
}

async fn scoped_conn_for_scope<'a>(pool: &'a PgPool, scope: RlsContext) -> ScopedConn<'a> {
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

pub(crate) async fn fact_rows(pool: &PgPool, storage_partition_id: Uuid) -> Vec<NodeIndexRow> {
    tenant_fact_rows(pool, storage_partition_id).await
}

pub(crate) async fn tenant_fact_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE storage_partition_id = $1 AND label = 'Fact' AND scope = 'tenant' \
         ORDER BY name",
    )
    .bind(storage_partition_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read fact rows");
    conn.commit().await.expect("commit fact row read");
    rows
}

pub(crate) async fn user_fact_rows(pool: &PgPool, storage_partition_id: Uuid) -> Vec<NodeIndexRow> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE storage_partition_id = $1 AND user_id = $2 AND label = 'Fact' AND scope = 'contact' \
         ORDER BY name",
    )
    .bind(storage_partition_id.to_string())
    .bind(SLOW_PATH_CONTACT_ID)
    .fetch_all(conn.as_mut())
    .await
    .expect("read user fact rows");
    conn.commit().await.expect("commit user fact row read");
    rows
}

pub(crate) async fn entity_rows(pool: &PgPool, storage_partition_id: Uuid) -> Vec<NodeIndexRow> {
    tenant_entity_rows(pool, storage_partition_id).await
}

pub(crate) async fn tenant_entity_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE storage_partition_id = $1 AND label = 'Entity' AND scope = 'tenant' \
         ORDER BY name",
    )
    .bind(storage_partition_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read entity rows");
    conn.commit().await.expect("commit entity row read");
    rows
}

pub(crate) async fn user_entity_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        "SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class, valid_to, valid_from, \
         properties_summary, last_accessed_at, COALESCE(quality_score, 0.5) AS quality_score \
         FROM moa.node_index \
         WHERE storage_partition_id = $1 AND user_id = $2 AND label = 'Entity' AND scope = 'contact' \
         ORDER BY name",
    )
    .bind(storage_partition_id.to_string())
    .bind(SLOW_PATH_CONTACT_ID)
    .fetch_all(conn.as_mut())
    .await
    .expect("read user entity rows");
    conn.commit().await.expect("commit user entity row read");
    rows
}

pub(crate) async fn active_user_fact_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    user_fact_rows(pool, storage_partition_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_tenant_fact_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    tenant_fact_rows(pool, storage_partition_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn active_user_entity_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    user_entity_rows(pool, storage_partition_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

/// Returns active contact-owned node UIDs visible under the exact barrier clearances.
pub(crate) async fn active_user_node_uids_with_clearances(
    pool: &PgPool,
    storage_partition_id: Uuid,
    label: NodeLabel,
    cleared: &[&str],
) -> Vec<Uuid> {
    let scope = RlsContext::contact(TenantId::from(storage_partition_id), slow_path_contact_id())
        .with_cleared_barriers(
            cleared
                .iter()
                .map(|tag| {
                    moa_core::types::memory::InformationBarrierId::parse(*tag)
                        .expect("valid barrier")
                })
                .collect(),
        );
    let mut conn = scoped_conn_for_scope(pool, scope).await;
    let rows = sqlx::query_scalar::<_, Uuid>(
        "SELECT uid FROM moa.node_index \
         WHERE storage_partition_id = $1 AND user_id = $2 AND label = $3 \
           AND scope = 'contact' AND valid_to IS NULL \
         ORDER BY uid",
    )
    .bind(storage_partition_id.to_string())
    .bind(SLOW_PATH_CONTACT_ID)
    .bind(label.as_str())
    .fetch_all(conn.as_mut())
    .await
    .expect("read barrier-cleared contact node uids");
    conn.commit()
        .await
        .expect("commit barrier-cleared contact node read");
    rows
}

pub(crate) async fn active_tenant_entity_rows(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<NodeIndexRow> {
    tenant_entity_rows(pool, storage_partition_id)
        .await
        .into_iter()
        .filter(|row| row.valid_to.is_none())
        .collect()
}

pub(crate) async fn node_valid_to(
    pool: &PgPool,
    storage_partition_id: Uuid,
    uid: Uuid,
) -> Option<DateTime<Utc>> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
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

/// Reads the persisted `moa.node_index.barrier` tag for one contact-scoped node
/// as a caller cleared for `cleared`.
///
/// The read runs as `moa_app` under the `rd_barrier_need_to_know` policy, so a
/// barriered row is only returned when its tag is in `cleared` (pass the empty
/// slice to read an unbarriered row, whose NULL barrier is always visible).
pub(crate) async fn node_barrier(
    pool: &PgPool,
    storage_partition_id: Uuid,
    uid: Uuid,
    cleared: &[&str],
) -> Option<String> {
    let scope = RlsContext::contact(TenantId::from(storage_partition_id), slow_path_contact_id())
        .with_cleared_barriers(
            cleared
                .iter()
                .map(|tag| {
                    moa_core::types::memory::InformationBarrierId::parse(*tag)
                        .expect("valid barrier")
                })
                .collect(),
        );
    let mut conn = scoped_conn_for_scope(pool, scope).await;
    let barrier = sqlx::query_scalar::<_, Option<String>>(
        "SELECT barrier FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node barrier");
    conn.commit().await.expect("commit barrier read");
    barrier
}

pub(crate) async fn node_confidence(pool: &PgPool, storage_partition_id: Uuid, uid: Uuid) -> f64 {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
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

/// Overwrites one node's derived ranking state to simulate a decayed fact.
pub(crate) async fn set_node_ranking_state(
    pool: &PgPool,
    storage_partition_id: Uuid,
    uid: Uuid,
    confidence: f64,
    base_confidence: Option<f64>,
    last_accessed_at: DateTime<Utc>,
) {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        UPDATE moa.node_index
        SET confidence = $2,
            base_confidence = $3,
            last_accessed_at = $4
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .bind(confidence)
    .bind(base_confidence)
    .bind(last_accessed_at)
    .execute(conn.as_mut())
    .await
    .expect("set node ranking state");
    conn.commit().await.expect("commit node ranking state");
}

/// Reads one node's derived ranking state: confidence, decay anchor, last access.
pub(crate) async fn node_ranking_state(
    pool: &PgPool,
    storage_partition_id: Uuid,
    uid: Uuid,
) -> (f64, Option<f64>, DateTime<Utc>) {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let row = sqlx::query(
        r#"
        SELECT confidence,
               base_confidence,
               last_accessed_at
        FROM moa.node_index
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node ranking state");
    conn.commit().await.expect("commit node ranking state read");
    (
        row.try_get::<Option<f64>, _>("confidence")
            .expect("confidence column")
            .expect("confidence should be set"),
        row.try_get::<Option<f64>, _>("base_confidence")
            .expect("base_confidence column"),
        row.try_get::<DateTime<Utc>, _>("last_accessed_at")
            .expect("last_accessed_at column"),
    )
}

pub(crate) async fn contradiction_edge_count(pool: &PgPool, storage_partition_id: Uuid) -> i64 {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND target_label = 'CONTRADICTS'",
    )
    .bind(storage_partition_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count contradicts edges");
    conn.commit().await.expect("commit contradicts edge count");
    count
}

pub(crate) async fn supersede_protocol_count(pool: &PgPool, storage_partition_id: Uuid) -> i64 {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'supersede' AND target_kind = 'node' \
           AND target_label = 'Fact'",
    )
    .bind(storage_partition_id.to_string())
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
    storage_partition_id: Uuid,
    old_uid: Uuid,
    new_uid: Uuid,
) -> bool {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 \
             FROM moa.edge_index \
             WHERE label = 'SUPERSEDES' \
               AND start_uid = $1 \
               AND end_uid = $2 \
               AND storage_partition_id = $3 \
         )",
    )
    .bind(new_uid)
    .bind(old_uid)
    .bind(storage_partition_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("query supersedes edge");
    conn.commit().await.expect("commit supersedes edge read");
    exists
}

pub(crate) async fn relates_to_edges(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<(String, String, String)> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query(
        "SELECT payload->>'start_uid' AS start_uid, \
                payload->>'end_uid' AS end_uid, \
                payload->'after'->>'role' AS role \
         FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND target_label = 'RELATES_TO' \
         ORDER BY change_id",
    )
    .bind(storage_partition_id.to_string())
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
    storage_partition_id: Uuid,
) -> Vec<(String, String, String, String)> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query(
        "SELECT target_label, \
                payload->>'start_uid' AS start_uid, \
                payload->>'end_uid' AS end_uid, \
                payload->'after'->>'role' AS role \
         FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create' AND target_kind = 'edge' \
           AND payload->'after'->>'source' = 'slow_path_entity_resolution' \
         ORDER BY change_id",
    )
    .bind(storage_partition_id.to_string())
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

pub(crate) async fn create_changelog_payloads(
    pool: &PgPool,
    storage_partition_id: Uuid,
) -> Vec<Value> {
    let mut conn = user_scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query(
        "SELECT payload FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create' AND target_kind = 'node' \
           AND target_label = 'Fact' \
         ORDER BY change_id",
    )
    .bind(storage_partition_id.to_string())
    .fetch_all(conn.as_mut())
    .await
    .expect("read create changelog payloads");
    conn.commit().await.expect("commit changelog read");
    rows.into_iter()
        .map(|row| row.try_get::<Value, _>("payload").expect("payload json"))
        .collect()
}
fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

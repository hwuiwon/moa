//! Integration tests for graph-backed fast memory ingestion.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{ContactId, TenantId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, NodeLabel, PiiClass, cypher};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, FastError, FastPathCtx,
    FastRememberRequest, ForgetPattern, IngestError, fast_forget, fast_remember,
};
use moa_memory_pii::{PiiCategory, PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_types::ScopeContext;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone)]
struct RecordingEmbedder {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl RecordingEmbedder {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl EmbeddingProvider for RecordingEmbedder {
    fn model_id(&self) -> &str {
        "mock-fast-embedder"
    }

    fn model_version(&self) -> i32 {
        7
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        self.calls.lock().await.push(texts.to_vec());
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
struct FixedPiiClassifier {
    result: PiiResult,
}

#[async_trait]
impl PiiClassifier for FixedPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(self.result.clone())
    }
}

#[derive(Debug, Clone)]
struct ScriptedConflictDetector {
    conflict: Conflict,
    delay: Duration,
}

#[async_trait]
impl ContradictionDetector for ScriptedConflictDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: NodeLabel,
        _pii_class: PiiClass,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.conflict)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        Ok(self.conflict)
    }
}

fn deterministic_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn pii_result(class: PiiClass) -> PiiResult {
    PiiResult {
        class,
        spans: Vec::<PiiSpan>::new(),
        model_version: "mock-pii".to_string(),
        abstained: false,
    }
}

fn test_ctx(
    pool: &PgPool,
    tenant_id: Uuid,
    conflict: Conflict,
    delay: Duration,
    pii_class: PiiClass,
) -> FastPathCtx {
    let scope = ScopeContext::tenant(TenantId::from(tenant_id));
    test_ctx_for_scope(pool, scope, conflict, delay, pii_result(pii_class))
}

fn contact_test_ctx(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    conflict: Conflict,
    delay: Duration,
    pii_class: PiiClass,
) -> FastPathCtx {
    let scope = ScopeContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    test_ctx_for_scope(pool, scope, conflict, delay, pii_result(pii_class))
}

fn test_ctx_for_scope(
    pool: &PgPool,
    scope: ScopeContext,
    conflict: Conflict,
    delay: Duration,
    pii_result: PiiResult,
) -> FastPathCtx {
    test_ctx_for_scope_with_embedder(
        pool,
        scope,
        conflict,
        delay,
        Arc::new(RecordingEmbedder::new()),
        Arc::new(FixedPiiClassifier { result: pii_result }),
    )
}

fn test_ctx_for_scope_with_embedder(
    pool: &PgPool,
    scope: ScopeContext,
    conflict: Conflict,
    delay: Duration,
    embedder: Arc<dyn EmbeddingProvider>,
    pii: Arc<dyn PiiClassifier>,
) -> FastPathCtx {
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph = Arc::new(
        AgeGraphStore::scoped_for_app_role(pool.clone(), scope.clone())
            .with_vector_store(vector.clone()),
    );
    FastPathCtx::new(
        pool.clone(),
        scope,
        graph,
        vector,
        embedder,
        pii,
        Arc::new(ScriptedConflictDetector { conflict, delay }),
    )
    .with_assume_app_role(true)
}

fn tenant_remember_request(tenant_id: Uuid, text: &str) -> FastRememberRequest {
    FastRememberRequest {
        tenant_id,
        contact_id: None,
        scope: "tenant".to_string(),
        text: text.to_string(),
        label: NodeLabel::Fact,
        supersedes_specific: None,
        actor_id: Uuid::now_v7(),
        actor_kind: "user".to_string(),
    }
}

fn contact_remember_request(tenant_id: Uuid, contact_id: Uuid, text: &str) -> FastRememberRequest {
    FastRememberRequest {
        contact_id: Some(contact_id),
        scope: "contact".to_string(),
        ..tenant_remember_request(tenant_id, text)
    }
}

async fn tenant_scoped_conn<'a>(pool: &'a PgPool, tenant_id: Uuid) -> ScopedConn<'a> {
    let scope = ScopeContext::tenant(TenantId::from(tenant_id));
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn node_name(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> String {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let name = sqlx::query_scalar::<_, String>("SELECT name FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("read node name");
    conn.commit().await.expect("commit name read");
    name
}

async fn node_summary(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> String {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let summary = sqlx::query_scalar::<_, String>(
        "SELECT properties_summary->>'summary' FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node summary");
    conn.commit().await.expect("commit summary read");
    summary
}

async fn node_confidence(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> f64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let confidence = sqlx::query_scalar::<_, Option<f64>>(
        "SELECT confidence FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read confidence")
    .expect("confidence should be set");
    conn.commit().await.expect("commit confidence read");
    confidence
}

async fn node_pii_class(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> String {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let pii_class =
        sqlx::query_scalar::<_, String>("SELECT pii_class FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(conn.as_mut())
            .await
            .expect("read pii_class");
    conn.commit().await.expect("commit pii read");
    pii_class
}

async fn node_valid_to(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> Option<DateTime<Utc>> {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
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

async fn node_valid_to_for_contact(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    uid: Uuid,
) -> Option<DateTime<Utc>> {
    let scope = ScopeContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin contact scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    let valid_to = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT valid_to FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read contact valid_to");
    conn.commit().await.expect("commit contact valid_to read");
    valid_to
}

async fn supersedes_edge_exists(pool: &PgPool, tenant_id: Uuid, old_uid: Uuid, new_uid: Uuid) {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let row = cypher::edge::SUPERSEDES_EXISTS
        .execute(&json!({
            "old_uid": old_uid.to_string(),
            "new_uid": new_uid.to_string(),
        }))
        .fetch_optional(conn.as_mut())
        .await
        .expect("query supersedes edge");
    assert!(row.is_some(), "SUPERSEDES edge should exist");
    conn.commit().await.expect("commit edge check");
}

async fn tenant_state_version(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(tenant_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("read tenant state version");
    conn.commit().await.expect("commit version read");
    version
}

async fn node_count_for_tenant(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.node_index WHERE storage_partition_id = $1",
    )
    .bind(tenant_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count tenant nodes");
    conn.commit().await.expect("commit node count read");
    count
}

async fn embedding_count_for_tenant(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.embeddings WHERE storage_partition_id = $1",
    )
    .bind(tenant_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count tenant embeddings");
    conn.commit().await.expect("commit embedding count read");
    count
}

async fn raw_text_property_count(pool: &PgPool, tenant_id: Uuid, raw_text: &str) -> i64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.node_index \
         WHERE storage_partition_id = $1 AND strpos(properties_summary::text, $2) > 0",
    )
    .bind(tenant_id.to_string())
    .bind(raw_text)
    .fetch_one(conn.as_mut())
    .await
    .expect("count raw text properties");
    conn.commit().await.expect("commit raw text property read");
    count
}

async fn seed_tenant_embedder_state(pool: &PgPool, tenant_id: Uuid) {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'mock-fast-embedder', 7, $2)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed tenant embedder state");
    conn.commit().await.expect("commit tenant embedder state");
}

#[tokio::test]
async fn fast_remember_db_memory() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let started = Instant::now();
    let uid = fast_remember(
        tenant_remember_request(tenant_id, "we deploy to fly.io"),
        &ctx,
    )
    .await
    .expect("remember fact");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(
        node_name(session_store.pool(), tenant_id, uid).await,
        "we deploy to fly.io"
    );
    assert_eq!(
        node_pii_class(session_store.pool(), tenant_id, uid).await,
        "none"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_fail_closed_pii_without_spans_errors_before_embedding_or_graph_write() {
    // Pins: missing PII classification fails before embedding or graph persistence.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let embedder = RecordingEmbedder::new();
    let scope = ScopeContext::tenant(TenantId::from(tenant_id));
    let ctx = test_ctx_for_scope_with_embedder(
        session_store.pool(),
        scope,
        Conflict::Insert,
        Duration::ZERO,
        Arc::new(embedder.clone()),
        Arc::new(FixedPiiClassifier {
            result: PiiResult::fail_closed("mock-pii-unavailable"),
        }),
    );

    let raw_text = "Dana phone 555-123-0000 deploys auth";
    let err = fast_remember(tenant_remember_request(tenant_id, raw_text), &ctx)
        .await
        .expect_err("fail-closed PII with no spans must reject the write");
    assert!(
        matches!(
            err,
            FastError::PiiClassificationUnavailable { ref model_version }
                if model_version == "mock-pii-unavailable"
        ),
        "unexpected fast-path PII error: {err:?}"
    );
    assert_eq!(
        embedder.calls().await,
        Vec::<Vec<String>>::new(),
        "fail-closed PII must not reach the embedder"
    );
    assert_eq!(
        node_count_for_tenant(session_store.pool(), tenant_id).await,
        0
    );
    assert_eq!(
        embedding_count_for_tenant(session_store.pool(), tenant_id).await,
        0
    );
    assert_eq!(
        raw_text_property_count(session_store.pool(), tenant_id, raw_text).await,
        0
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_successful_pii_classification_embeds_and_stores_redacted_text() {
    // Pins: successful PII classification preserves fast-path writes using redacted text only.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let embedder = RecordingEmbedder::new();
    let raw_text = "contact email dana@example.com deploys auth";
    let email_start = raw_text
        .find("dana@example.com")
        .expect("test fixture contains email");
    let email_end = email_start + "dana@example.com".len();
    let redacted_text = "contact email [EMAIL_REDACTED] deploys auth";
    let scope = ScopeContext::tenant(TenantId::from(tenant_id));
    let ctx = test_ctx_for_scope_with_embedder(
        session_store.pool(),
        scope,
        Conflict::Insert,
        Duration::ZERO,
        Arc::new(embedder.clone()),
        Arc::new(FixedPiiClassifier {
            result: PiiResult {
                class: PiiClass::Pii,
                spans: vec![PiiSpan::new(
                    email_start,
                    email_end,
                    PiiCategory::Email,
                    0.99,
                )],
                model_version: "mock-pii".to_string(),
                abstained: false,
            },
        }),
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let uid = fast_remember(tenant_remember_request(tenant_id, raw_text), &ctx)
        .await
        .expect("redacted fast remember should commit");

    assert_eq!(
        embedder.calls().await,
        vec![vec![redacted_text.to_string()]],
        "embedder must receive the redacted text"
    );
    assert_eq!(
        node_name(session_store.pool(), tenant_id, uid).await,
        redacted_text
    );
    assert_eq!(
        node_summary(session_store.pool(), tenant_id, uid).await,
        redacted_text
    );
    assert_eq!(
        node_pii_class(session_store.pool(), tenant_id, uid).await,
        "pii"
    );
    assert_eq!(
        raw_text_property_count(session_store.pool(), tenant_id, raw_text).await,
        0
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_explicit_supersede_invalidates_old_node_and_links_edge() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::Pii,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let old_uid = fast_remember(
        tenant_remember_request(tenant_id, "deployments use heroku"),
        &ctx,
    )
    .await
    .expect("create old fact");

    let mut req = tenant_remember_request(tenant_id, "deployments use fly.io");
    req.supersedes_specific = Some(old_uid);
    let new_uid = fast_remember(req, &ctx).await.expect("supersede old fact");

    assert!(
        node_valid_to(session_store.pool(), tenant_id, old_uid)
            .await
            .is_some()
    );
    assert!(
        node_valid_to(session_store.pool(), tenant_id, new_uid)
            .await
            .is_none()
    );
    assert_eq!(
        node_pii_class(session_store.pool(), tenant_id, new_uid).await,
        "pii"
    );
    supersedes_edge_exists(session_store.pool(), tenant_id, old_uid, new_uid).await;
    assert_eq!(
        tenant_state_version(session_store.pool(), tenant_id).await,
        3
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_judge_timeout_commits_indeterminate_with_low_confidence() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Supersede(Uuid::now_v7()),
        Duration::from_millis(350),
        PiiClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let uid = fast_remember(
        tenant_remember_request(tenant_id, "auth service uses passkeys"),
        &ctx,
    )
    .await
    .expect("indeterminate insert should commit");
    assert_eq!(
        node_confidence(session_store.pool(), tenant_id, uid).await,
        0.5
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_forget_idempotent_by_name() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let uid = fast_remember(tenant_remember_request(tenant_id, "auth"), &ctx)
        .await
        .expect("create forget target");

    let first = fast_forget(ForgetPattern::NameMatch("auth".to_string()), &ctx)
        .await
        .expect("first forget");
    let second = fast_forget(ForgetPattern::NameMatch("auth".to_string()), &ctx)
        .await
        .expect("second forget");
    assert_eq!(first, 1);
    assert_eq!(second, 0);
    assert!(
        node_valid_to(session_store.pool(), tenant_id, uid)
            .await
            .is_some()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_forget_soft_all_respects_contact_scope() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let contact_a = Uuid::now_v7();
    let contact_b = Uuid::now_v7();
    let ctx_a = contact_test_ctx(
        session_store.pool(),
        tenant_id,
        contact_a,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::None,
    );
    let ctx_b = contact_test_ctx(
        session_store.pool(),
        tenant_id,
        contact_b,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let a_one = fast_remember(
        contact_remember_request(tenant_id, contact_a, "contact a preference one"),
        &ctx_a,
    )
    .await
    .expect("create first contact a node");
    let a_two = fast_remember(
        contact_remember_request(tenant_id, contact_a, "contact a preference two"),
        &ctx_a,
    )
    .await
    .expect("create second contact a node");
    let b_one = fast_remember(
        contact_remember_request(tenant_id, contact_b, "contact b preference"),
        &ctx_b,
    )
    .await
    .expect("create contact b node");

    let count = fast_forget(ForgetPattern::SoftAll(contact_a), &ctx_a)
        .await
        .expect("forget contact a nodes");
    assert_eq!(count, 2);
    assert!(
        node_valid_to_for_contact(session_store.pool(), tenant_id, contact_a, a_one)
            .await
            .is_some()
    );
    assert!(
        node_valid_to_for_contact(session_store.pool(), tenant_id, contact_a, a_two)
            .await
            .is_some()
    );
    assert!(
        node_valid_to_for_contact(session_store.pool(), tenant_id, contact_b, b_one)
            .await
            .is_none()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_batch_writes_all_facts_with_local_dependencies() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Insert,
        Duration::ZERO,
        PiiClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let mut uids = Vec::new();

    for index in 0..10 {
        let uid = fast_remember(
            tenant_remember_request(tenant_id, &format!("latency budget fact {index}")),
            &ctx,
        )
        .await
        .expect("remember latency fact");
        uids.push(uid);
    }
    assert_eq!(uids.len(), 10);
    for (index, uid) in uids.into_iter().enumerate() {
        assert_eq!(
            node_name(session_store.pool(), tenant_id, uid).await,
            format!("latency budget fact {index}")
        );
    }

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

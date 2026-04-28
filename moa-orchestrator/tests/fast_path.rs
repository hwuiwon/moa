//! Integration tests for graph-backed fast memory ingestion.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{ScopeContext, ScopedConn, UserId, WorkspaceId};
use moa_hands::ToolRegistry;
use moa_memory_graph::{
    AgeGraphStore, NodeLabel, NodeWriteIntent, PiiClass as GraphPiiClass, cypher,
};
use moa_memory_ingest::{Conflict, ContradictionContext, ContradictionDetector, IngestError};
use moa_memory_pii::{PiiClass as ClassifierPiiClass, PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_vector::{Embedder, Error as VectorError, PgvectorStore, VECTOR_DIMENSION};
use moa_orchestrator::fast_path::{
    FastPathCtx, FastRememberRequest, ForgetPattern, fast_forget, fast_remember, fast_supersede,
};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone)]
struct MockEmbedder;

#[async_trait]
impl Embedder for MockEmbedder {
    fn model_name(&self) -> &'static str {
        "mock-fast-embedder"
    }

    fn model_version(&self) -> i32 {
        7
    }

    fn dimension(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, VectorError> {
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
struct FixedPiiClassifier {
    class: ClassifierPiiClass,
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
struct FixedConflictChecker {
    conflict: Conflict,
    delay: Duration,
}

#[async_trait]
impl ContradictionDetector for FixedConflictChecker {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: NodeLabel,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.conflict)
    }

    async fn check_one_slow(
        &self,
        _fact: &moa_memory_ingest::EmbeddedFact,
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

fn test_ctx(
    pool: &PgPool,
    workspace_id: Uuid,
    conflict: Conflict,
    delay: Duration,
    pii_class: ClassifierPiiClass,
) -> FastPathCtx {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    test_ctx_for_scope(pool, scope, conflict, delay, pii_class)
}

fn user_test_ctx(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    conflict: Conflict,
    delay: Duration,
    pii_class: ClassifierPiiClass,
) -> FastPathCtx {
    let scope = ScopeContext::user(
        WorkspaceId::new(workspace_id.to_string()),
        UserId::new(user_id.to_string()),
    );
    test_ctx_for_scope(pool, scope, conflict, delay, pii_class)
}

fn test_ctx_for_scope(
    pool: &PgPool,
    scope: ScopeContext,
    conflict: Conflict,
    delay: Duration,
    pii_class: ClassifierPiiClass,
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
        Arc::new(MockEmbedder),
        Arc::new(FixedPiiClassifier { class: pii_class }),
        Arc::new(FixedConflictChecker { conflict, delay }),
    )
    .with_assume_app_role(true)
}

fn remember_request(workspace_id: Uuid, text: &str) -> FastRememberRequest {
    FastRememberRequest {
        workspace_id,
        user_id: None,
        scope: "workspace".to_string(),
        text: text.to_string(),
        label: NodeLabel::Fact,
        supersedes_specific: None,
        actor_id: Uuid::now_v7(),
        actor_kind: "user".to_string(),
    }
}

fn user_remember_request(workspace_id: Uuid, user_id: Uuid, text: &str) -> FastRememberRequest {
    FastRememberRequest {
        user_id: Some(user_id),
        scope: "user".to_string(),
        ..remember_request(workspace_id, text)
    }
}

fn supersede_intent(workspace_id: Uuid, text: &str) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: text.to_string(),
        properties: json!({ "summary": text, "source": "fast_supersede_test" }),
        pii_class: GraphPiiClass::None,
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding: Some(deterministic_vector(text)),
        embedding_model: Some("mock-fast-embedder".to_string()),
        embedding_model_version: Some(7),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "user".to_string(),
    }
}

async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: Uuid) -> ScopedConn<'a> {
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

async fn node_name(pool: &PgPool, workspace_id: Uuid, uid: Uuid) -> String {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let name = sqlx::query_scalar::<_, String>("SELECT name FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("read node name");
    conn.commit().await.expect("commit name read");
    name
}

async fn node_confidence(pool: &PgPool, workspace_id: Uuid, uid: Uuid) -> f64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
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

async fn node_pii_class(pool: &PgPool, workspace_id: Uuid, uid: Uuid) -> String {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let pii_class =
        sqlx::query_scalar::<_, String>("SELECT pii_class FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(conn.as_mut())
            .await
            .expect("read pii_class");
    conn.commit().await.expect("commit pii read");
    pii_class
}

async fn node_valid_to(pool: &PgPool, workspace_id: Uuid, uid: Uuid) -> Option<DateTime<Utc>> {
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

async fn node_valid_to_for_user(
    pool: &PgPool,
    workspace_id: Uuid,
    user_id: Uuid,
    uid: Uuid,
) -> Option<DateTime<Utc>> {
    let scope = ScopeContext::user(
        WorkspaceId::new(workspace_id.to_string()),
        UserId::new(user_id.to_string()),
    );
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin user scoped test transaction");
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
    .expect("read user valid_to");
    conn.commit().await.expect("commit user valid_to read");
    valid_to
}

async fn supersedes_edge_exists(pool: &PgPool, workspace_id: Uuid, old_uid: Uuid, new_uid: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
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

async fn workspace_version(pool: &PgPool, workspace_id: Uuid) -> i64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(workspace_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("read workspace version");
    conn.commit().await.expect("commit version read");
    version
}

#[tokio::test]
async fn fast_remember_e2e() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );

    let started = Instant::now();
    let uid = fast_remember(remember_request(workspace_id, "we deploy to fly.io"), &ctx)
        .await
        .expect("remember fact");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(
        node_name(session_store.pool(), workspace_id, uid).await,
        "we deploy to fly.io"
    );
    assert_eq!(
        node_pii_class(session_store.pool(), workspace_id, uid).await,
        "none"
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
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::Pii,
    );
    let old_uid = fast_remember(
        remember_request(workspace_id, "deployments use heroku"),
        &ctx,
    )
    .await
    .expect("create old fact");

    let mut req = remember_request(workspace_id, "deployments use fly.io");
    req.supersedes_specific = Some(old_uid);
    let new_uid = fast_remember(req, &ctx).await.expect("supersede old fact");

    assert!(
        node_valid_to(session_store.pool(), workspace_id, old_uid)
            .await
            .is_some()
    );
    assert!(
        node_valid_to(session_store.pool(), workspace_id, new_uid)
            .await
            .is_none()
    );
    assert_eq!(
        node_pii_class(session_store.pool(), workspace_id, new_uid).await,
        "pii"
    );
    supersedes_edge_exists(session_store.pool(), workspace_id, old_uid, new_uid).await;
    assert_eq!(
        workspace_version(session_store.pool(), workspace_id).await,
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
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Supersede(Uuid::now_v7()),
        Duration::from_millis(350),
        ClassifierPiiClass::None,
    );

    let uid = fast_remember(
        remember_request(workspace_id, "auth service uses passkeys"),
        &ctx,
    )
    .await
    .expect("indeterminate insert should commit");
    assert_eq!(
        node_confidence(session_store.pool(), workspace_id, uid).await,
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
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );
    let uid = fast_remember(remember_request(workspace_id, "auth"), &ctx)
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
        node_valid_to(session_store.pool(), workspace_id, uid)
            .await
            .is_some()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_forget_soft_all_respects_user_scope() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = Uuid::now_v7();
    let user_a = Uuid::now_v7();
    let user_b = Uuid::now_v7();
    let ctx_a = user_test_ctx(
        session_store.pool(),
        workspace_id,
        user_a,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );
    let ctx_b = user_test_ctx(
        session_store.pool(),
        workspace_id,
        user_b,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );

    let a_one = fast_remember(
        user_remember_request(workspace_id, user_a, "user a preference one"),
        &ctx_a,
    )
    .await
    .expect("create first user a node");
    let a_two = fast_remember(
        user_remember_request(workspace_id, user_a, "user a preference two"),
        &ctx_a,
    )
    .await
    .expect("create second user a node");
    let b_one = fast_remember(
        user_remember_request(workspace_id, user_b, "user b preference"),
        &ctx_b,
    )
    .await
    .expect("create user b node");

    let count = fast_forget(ForgetPattern::SoftAll(user_a), &ctx_a)
        .await
        .expect("forget user a nodes");
    assert_eq!(count, 2);
    assert!(
        node_valid_to_for_user(session_store.pool(), workspace_id, user_a, a_one)
            .await
            .is_some()
    );
    assert!(
        node_valid_to_for_user(session_store.pool(), workspace_id, user_a, a_two)
            .await
            .is_some()
    );
    assert!(
        node_valid_to_for_user(session_store.pool(), workspace_id, user_b, b_one)
            .await
            .is_none()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_supersede_wrapper_replaces_existing_node() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );
    let old_uid = fast_remember(
        remember_request(workspace_id, "the API gateway is nginx"),
        &ctx,
    )
    .await
    .expect("create old node");
    let new_uid = fast_supersede(
        old_uid,
        supersede_intent(workspace_id, "the API gateway is envoy"),
        &ctx,
    )
    .await
    .expect("fast supersede");

    assert!(
        node_valid_to(session_store.pool(), workspace_id, old_uid)
            .await
            .is_some()
    );
    assert!(
        node_valid_to(session_store.pool(), workspace_id, new_uid)
            .await
            .is_none()
    );
    supersedes_edge_exists(session_store.pool(), workspace_id, old_uid, new_uid).await;

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_p95_stays_under_latency_budget_with_local_dependencies() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = Uuid::now_v7();
    let ctx = test_ctx(
        session_store.pool(),
        workspace_id,
        Conflict::Insert,
        Duration::ZERO,
        ClassifierPiiClass::None,
    );
    let mut durations = Vec::new();

    for index in 0..10 {
        let started = Instant::now();
        fast_remember(
            remember_request(workspace_id, &format!("latency budget fact {index}")),
            &ctx,
        )
        .await
        .expect("remember latency fact");
        durations.push(started.elapsed());
    }
    durations.sort();
    let p95 = durations[durations.len() - 1];
    assert!(
        p95 < Duration::from_millis(500),
        "p95 fast_remember latency was {p95:?}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[test]
fn fast_memory_tools_appear_in_default_registry() {
    let registry = ToolRegistry::default_local();
    assert!(registry.get("memory.remember").is_some());
    assert!(registry.get("memory.forget").is_some());
    assert!(registry.get("memory.supersede").is_some());
}

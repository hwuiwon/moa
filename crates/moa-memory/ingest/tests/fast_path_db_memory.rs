//! Integration tests for graph-backed fast memory ingestion.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_config::MoaConfig;
use moa_core::types::agent::{AgentContext, AgentKnowledgePolicy, AgentPolicySnapshot};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::EmbeddingProvider, types::contact::ContactId, types::identifiers::TenantId,
    types::session::SessionMeta, types::tools::ToolOutput,
};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{NodeLabel, PostgresGraphStore};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, Error as IngestError,
    FastError, FastPathCtx, FastRememberRequest, ForgetPattern, IncidentRecord, IngestRuntime,
    RrfPlusJudgeDetector, execute_memory_tool, fast_forget, fast_remember, install_runtime,
    record_incident, record_incident_with_ctx,
};
use moa_memory_pii::{Error as PiiError, PiiCategory, PiiClassifier, PiiResult, PiiSpan};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION, VectorStoreFactory};
use moa_session::testing;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

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

    async fn embed(&self, texts: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
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
        _query_embedding: Option<moa_memory_vector::QueryEmbedding>,
        _label: NodeLabel,
        _pii_class: SensitivityClass,
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

fn pii_result(class: SensitivityClass) -> PiiResult {
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
    pii_class: SensitivityClass,
) -> FastPathCtx {
    let scope = RlsContext::tenant(TenantId::from(tenant_id));
    test_ctx_for_scope(pool, scope, conflict, delay, pii_result(pii_class))
}

fn contact_test_ctx(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    conflict: Conflict,
    delay: Duration,
    pii_class: SensitivityClass,
) -> FastPathCtx {
    let scope = RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    test_ctx_for_scope(pool, scope, conflict, delay, pii_result(pii_class))
}

fn test_ctx_for_scope(
    pool: &PgPool,
    scope: RlsContext,
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
    scope: RlsContext,
    conflict: Conflict,
    delay: Duration,
    embedder: Arc<dyn EmbeddingProvider>,
    pii: Arc<dyn PiiClassifier>,
) -> FastPathCtx {
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), scope.clone(), test_kms())
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
        barrier: None,
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
    let scope = RlsContext::tenant(TenantId::from(tenant_id));
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

async fn node_label(pool: &PgPool, tenant_id: Uuid, uid: Uuid) -> String {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let label = sqlx::query_scalar::<_, String>("SELECT label FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("read node label");
    conn.commit().await.expect("commit label read");
    label
}

async fn node_property(pool: &PgPool, tenant_id: Uuid, uid: Uuid, key: &str) -> String {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let value = sqlx::query_scalar::<_, Option<String>>(&format!(
        "SELECT properties_summary->>'{key}' FROM moa.node_index WHERE uid = $1"
    ))
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node property")
    .unwrap_or_default();
    conn.commit().await.expect("commit property read");
    value
}

async fn incident_count_for_tenant(pool: &PgPool, tenant_id: Uuid) -> i64 {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.node_index \
         WHERE storage_partition_id = $1 AND label = 'Incident' AND valid_to IS NULL",
    )
    .bind(tenant_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("count tenant incidents");
    conn.commit().await.expect("commit incident count read");
    count
}

fn tenant_incident_record(tenant_id: Uuid, session_id: Uuid) -> IncidentRecord {
    IncidentRecord {
        tenant_id,
        contact_id: None,
        scope: "tenant".to_string(),
        session_id,
        turn_seq: 3,
        attempted: "search_web".to_string(),
        failure: "provider_error".to_string(),
        barrier: None,
        actor_id: Uuid::now_v7(),
        actor_kind: "system".to_string(),
    }
}

/// Reads the persisted `moa.node_index.barrier` tag for one node as a caller
/// cleared for `cleared`.
///
/// Runs as `moa_app` under the `rd_barrier_need_to_know` policy: a barriered row
/// is returned only when its tag is in `cleared`; pass the empty slice for an
/// unbarriered row, whose NULL barrier is always visible.
async fn node_barrier(
    pool: &PgPool,
    tenant_id: Uuid,
    uid: Uuid,
    cleared: &[&str],
) -> Option<String> {
    let scope = RlsContext::tenant(TenantId::from(tenant_id)).with_cleared_barriers(
        cleared
            .iter()
            .map(|tag| {
                moa_core::types::memory::InformationBarrierId::parse(*tag).expect("valid barrier")
            })
            .collect(),
    );
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
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

async fn active_named_node_count_with_barrier(
    pool: &PgPool,
    tenant_id: Uuid,
    label: &str,
    name: &str,
    barrier: &str,
) -> i64 {
    let scope = RlsContext::tenant(TenantId::from(tenant_id)).with_cleared_barriers(
        [moa_core::types::memory::InformationBarrierId::parse(barrier).expect("valid barrier")]
            .into_iter()
            .collect(),
    );
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin barrier-cleared count transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.node_index \
         WHERE storage_partition_id = $1 \
           AND label = $2 \
           AND name = $3 \
           AND valid_to IS NULL",
    )
    .bind(tenant_id.to_string())
    .bind(label)
    .bind(name)
    .fetch_one(conn.as_mut())
    .await
    .expect("count active barriered nodes");
    conn.commit().await.expect("commit barriered count read");
    count
}

async fn node_valid_to_with_barrier(
    pool: &PgPool,
    tenant_id: Uuid,
    uid: Uuid,
    barrier: &str,
) -> Option<DateTime<Utc>> {
    let scope = RlsContext::tenant(TenantId::from(tenant_id)).with_cleared_barriers(
        [moa_core::types::memory::InformationBarrierId::parse(barrier).expect("valid barrier")]
            .into_iter()
            .collect(),
    );
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .expect("begin barrier-cleared validity transaction");
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
    .expect("read barriered node validity");
    conn.commit().await.expect("commit barriered validity read");
    valid_to
}

fn session_with_write_barrier(
    tenant_id: Uuid,
    barrier: &str,
    include_clearance: bool,
) -> SessionMeta {
    let barrier =
        moa_core::types::memory::InformationBarrierId::parse(barrier).expect("valid barrier");
    let mut agent_context = AgentContext::system_default();
    let cleared_barriers = if include_clearance {
        [barrier.clone()].into_iter().collect()
    } else {
        Default::default()
    };
    agent_context.policy_snapshot = serde_json::json!(AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            cleared_barriers,
            write_barrier: Some(barrier),
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    });
    SessionMeta {
        tenant_id: TenantId::from(tenant_id),
        agent_context: Some(agent_context),
        ..SessionMeta::default()
    }
}

fn single_stored_uid(output: &ToolOutput, operation: &str) -> Uuid {
    assert!(!output.is_error, "{operation} output: {}", output.to_text());
    let data = output
        .structured
        .as_ref()
        .expect("successful memory write should have structured output");
    assert_eq!(data["operation"], "remember");
    assert_eq!(data["stored"], 1);
    assert_eq!(data["rejected"], 0);
    let results = data["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "stored");
    Uuid::parse_str(results[0]["uid"].as_str().expect("stored uid string"))
        .expect("stored uid should parse")
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
    let scope = RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
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
    .bind(tenant_id.to_string())
    .fetch_one(conn.as_mut())
    .await
    .expect("query supersedes edge");
    assert!(exists, "SUPERSEDES edge should exist");
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
                embedding_dimension = EXCLUDED.embedding_dimension
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed tenant embedder state");
    conn.commit().await.expect("commit tenant embedder state");
}

async fn set_tenant_vector_backend(pool: &PgPool, tenant_id: Uuid, vector_backend: &str) {
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    sqlx::query(
        r#"
        UPDATE moa.storage_partition_state
           SET vector_backend = $2,
               vector_backend_state = 'steady'
         WHERE storage_partition_id = $1
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(vector_backend)
    .execute(conn.as_mut())
    .await
    .expect("set tenant vector backend");
    conn.commit().await.expect("commit tenant vector backend");
}

async fn seed_active_tenant_node(pool: &PgPool, tenant_id: Uuid, name: &str) -> Uuid {
    let uid = Uuid::now_v7();
    let mut conn = tenant_scoped_conn(pool, tenant_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, data_subject_id, name, pii_class, confidence, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, 'none', 0.9, $5)
        "#,
    )
    .bind(uid)
    .bind(tenant_id.to_string())
    .bind(tenant_id)
    .bind(name)
    .bind(serde_json::json!({ "summary": name }))
    .execute(conn.as_mut())
    .await
    .expect("seed active tenant node");
    conn.commit().await.expect("commit active tenant node");
    uid
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
        SensitivityClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let started = Instant::now();
    let uid = fast_remember(
        tenant_remember_request(tenant_id, "we deploy to railway"),
        &ctx,
    )
    .await
    .expect("remember fact");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(
        node_name(session_store.pool(), tenant_id, uid).await,
        "we deploy to railway"
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
async fn session_fast_paths_keep_pinned_barrier_visible_db_memory() {
    // Pins: exported session-facing remember, incident, supersede, and forget
    // calls tag new rows with the pinned write barrier and install that same
    // barrier as transaction-local RLS clearance, so a session can deduplicate,
    // supersede, and delete its own need-to-know rows; malformed policy fails closed.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let barrier = "deal-session-fast-path";
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let app_options: PgConnectOptions = database_url
        .parse()
        .expect("isolated database URL should parse");
    let app_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(app_options.options([("role", "moa_app")]))
        .await
        .expect("connect to isolated database as moa_app");
    let mut config = MoaConfig::default();
    config.memory.vector.embedder.name = "disabled".to_string();
    config.memory.extraction.enabled = true;
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(RecordingEmbedder::new());
    let pii: Arc<dyn PiiClassifier> = Arc::new(FixedPiiClassifier {
        result: pii_result(SensitivityClass::None),
    });
    let runtime = IngestRuntime::from_config(app_pool, test_kms(), &config)
        .expect("build hermetic runtime")
        .with_fast_path_dependencies(embedder.clone(), pii.clone());
    assert!(Arc::ptr_eq(
        &runtime.embedder().expect("injected embedder"),
        &embedder
    ));
    assert!(Arc::ptr_eq(&runtime.pii_classifier(), &pii));
    install_runtime(runtime).expect("install hermetic fast-path runtime");

    let session = session_with_write_barrier(tenant_id, barrier, true);
    let remember_input = serde_json::json!({
        "items": [{ "text": "we deploy to railway" }]
    });
    let first_uid = single_stored_uid(
        &execute_memory_tool(&session, "memory_remember", &remember_input)
            .await
            .expect("first remember dispatch"),
        "first remember",
    );
    let repeated_uid = single_stored_uid(
        &execute_memory_tool(&session, "memory_remember", &remember_input)
            .await
            .expect("repeated remember dispatch"),
        "repeated remember",
    );
    assert_eq!(repeated_uid, first_uid, "repeat must return the survivor");
    assert_eq!(
        active_named_node_count_with_barrier(
            session_store.pool(),
            tenant_id,
            "Fact",
            "we deploy to railway",
            barrier,
        )
        .await,
        1,
        "repeat must not create a second barriered fact",
    );
    assert_eq!(
        node_barrier(session_store.pool(), tenant_id, first_uid, &[barrier])
            .await
            .as_deref(),
        Some(barrier),
    );

    let incident_uid = record_incident(&session, 7, "search_web", "provider_error")
        .await
        .expect("first incident")
        .expect("first incident should write");
    assert_eq!(
        record_incident(&session, 7, "search_web", "provider_error")
            .await
            .expect("repeated incident"),
        None,
        "repeat must deduplicate the barriered incident",
    );
    assert_eq!(
        active_named_node_count_with_barrier(
            session_store.pool(),
            tenant_id,
            "Incident",
            "search_web: provider_error",
            barrier,
        )
        .await,
        1,
    );
    assert_eq!(
        node_barrier(session_store.pool(), tenant_id, incident_uid, &[barrier])
            .await
            .as_deref(),
        Some(barrier),
    );

    let old_uid = single_stored_uid(
        &execute_memory_tool(
            &session,
            "memory_remember",
            &serde_json::json!({ "items": [{ "text": "cache backend is redis" }] }),
        )
        .await
        .expect("remember supersession target"),
        "remember supersession target",
    );
    let replacement_uid = single_stored_uid(
        &execute_memory_tool(
            &session,
            "memory_supersede",
            &serde_json::json!({
                "old_uid": old_uid,
                "new_text": "cache backend is postgres"
            }),
        )
        .await
        .expect("supersede dispatch"),
        "supersede",
    );
    assert_ne!(replacement_uid, old_uid);
    assert!(
        node_valid_to_with_barrier(session_store.pool(), tenant_id, old_uid, barrier)
            .await
            .is_some(),
        "supersede must invalidate its barriered target",
    );
    assert_eq!(
        node_barrier(session_store.pool(), tenant_id, replacement_uid, &[barrier])
            .await
            .as_deref(),
        Some(barrier),
    );

    let invalid_session = session_with_write_barrier(tenant_id, barrier, false);
    let invalid_forget = execute_memory_tool(
        &invalid_session,
        "memory_forget",
        &serde_json::json!({ "uid": first_uid }),
    )
    .await
    .expect("invalid policy is returned as a tool output");
    assert!(invalid_forget.is_error, "invalid policy must fail closed");
    assert_eq!(
        node_valid_to_with_barrier(session_store.pool(), tenant_id, first_uid, barrier).await,
        None,
        "invalid policy must not mutate memory",
    );

    let forget = execute_memory_tool(
        &session,
        "memory_forget",
        &serde_json::json!({ "uid": first_uid }),
    )
    .await
    .expect("forget dispatch");
    assert!(!forget.is_error, "forget output: {}", forget.to_text());
    let forget_data = forget.structured.expect("forget structured output");
    assert_eq!(forget_data["operation"], "forget");
    assert_eq!(forget_data["invalidated"], 1);
    assert!(
        node_valid_to_with_barrier(session_store.pool(), tenant_id, first_uid, barrier)
            .await
            .is_some(),
        "forget must invalidate its own barriered row",
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_tags_node_with_barrier_db_memory() {
    // Pins: an explicit fast-path remember running under an information barrier
    // persists that barrier on the written node, while an unbarriered remember
    // leaves `moa.node_index.barrier` NULL (unchanged behavior).
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
        SensitivityClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let barriered_uid = fast_remember(
        FastRememberRequest {
            barrier: Some(
                moa_core::types::memory::InformationBarrierId::parse("deal-beta")
                    .expect("valid barrier"),
            ),
            ..tenant_remember_request(tenant_id, "we deploy to railway")
        },
        &ctx,
    )
    .await
    .expect("remember barriered fact");
    assert_eq!(
        node_barrier(
            session_store.pool(),
            tenant_id,
            barriered_uid,
            &["deal-beta"]
        )
        .await
        .as_deref(),
        Some("deal-beta")
    );

    let open_uid = fast_remember(tenant_remember_request(tenant_id, "we use grafana"), &ctx)
        .await
        .expect("remember unbarriered fact");
    assert_eq!(
        node_barrier(session_store.pool(), tenant_id, open_uid, &[]).await,
        None
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn record_incident_tags_node_with_barrier_db_memory() {
    // Pins: a durable failure recorded inside a barriered session writes an
    // Incident node carrying that barrier, so negative-results memory is
    // need-to-know restricted like the rest of the session's memory.
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
        SensitivityClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let session_id = Uuid::now_v7();

    let uid = record_incident_with_ctx(
        IncidentRecord {
            barrier: Some(
                moa_core::types::memory::InformationBarrierId::parse("deal-beta")
                    .expect("valid barrier"),
            ),
            ..tenant_incident_record(tenant_id, session_id)
        },
        &ctx,
    )
    .await
    .expect("record incident")
    .expect("barriered incident should write a node");
    assert_eq!(
        node_barrier(session_store.pool(), tenant_id, uid, &["deal-beta"])
            .await
            .as_deref(),
        Some("deal-beta")
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn fast_remember_duplicate_reinforces_survivor_instead_of_dropping_db_memory() {
    // Pins: an agent restating a known fact returns the surviving node's uid and
    // confirms it — confidence steps by exactly the reinforcement step and the
    // base_confidence decay anchor clears — instead of silently dropping the
    // observation or writing a second node.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    let seeded_uid =
        seed_active_tenant_node(session_store.pool(), tenant_id, "we deploy to railway").await;
    // Simulate a decayed fact: lowered confidence with an anchored base.
    let mut conn = tenant_scoped_conn(session_store.pool(), tenant_id).await;
    sqlx::query(
        r#"
        UPDATE moa.node_index
        SET confidence = 0.6,
            base_confidence = 0.9
        WHERE uid = $1
        "#,
    )
    .bind(seeded_uid)
    .execute(conn.as_mut())
    .await
    .expect("seed decayed ranking state");
    conn.commit().await.expect("commit decayed ranking state");
    let ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Duplicate(seeded_uid),
        Duration::ZERO,
        SensitivityClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let uid = fast_remember(
        tenant_remember_request(tenant_id, "we deploy to railway"),
        &ctx,
    )
    .await
    .expect("duplicate remember succeeds");

    assert_eq!(uid, seeded_uid, "duplicate must return the surviving node");
    assert_eq!(
        node_count_for_tenant(session_store.pool(), tenant_id).await,
        1,
        "duplicate remember must not write a second node"
    );
    let confidence = node_confidence(session_store.pool(), tenant_id, seeded_uid).await;
    assert!(
        (confidence - 0.7).abs() < 1e-9,
        "confidence must step from 0.6 by exactly 0.1, got {confidence}"
    );
    let mut conn = tenant_scoped_conn(session_store.pool(), tenant_id).await;
    let anchored = sqlx::query_scalar::<_, bool>(
        "SELECT base_confidence IS NOT NULL FROM moa.node_index WHERE uid = $1",
    )
    .bind(seeded_uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read decay anchor presence");
    conn.commit().await.expect("commit decay anchor read");
    assert!(
        !anchored,
        "decay anchor must clear so the next decay re-anchors from the boosted value"
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
    let scope = RlsContext::tenant(TenantId::from(tenant_id));
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
    let scope = RlsContext::tenant(TenantId::from(tenant_id));
    let ctx = test_ctx_for_scope_with_embedder(
        session_store.pool(),
        scope,
        Conflict::Insert,
        Duration::ZERO,
        Arc::new(embedder.clone()),
        Arc::new(FixedPiiClassifier {
            result: PiiResult {
                class: SensitivityClass::Pii,
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
        SensitivityClass::Pii,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let old_uid = fast_remember(
        tenant_remember_request(tenant_id, "deployments use heroku"),
        &ctx,
    )
    .await
    .expect("create old fact");

    let mut req = tenant_remember_request(tenant_id, "deployments use railway");
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
        SensitivityClass::None,
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
        SensitivityClass::None,
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
async fn fast_forget_does_not_select_read_vector_backend_db_memory() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    set_tenant_vector_backend(session_store.pool(), tenant_id, "turbopuffer").await;
    let uid = seed_active_tenant_node(session_store.pool(), tenant_id, "lazy-forget-target").await;

    let scope = RlsContext::tenant(TenantId::from(tenant_id));
    let vector_factory = VectorStoreFactory::default();
    let graph_vector = vector_factory.transactional_graph_backend(
        session_store.pool().clone(),
        scope.clone(),
        true,
    );
    let graph = Arc::new(
        PostgresGraphStore::scoped_for_app_role(
            session_store.pool().clone(),
            scope.clone(),
            test_kms(),
        )
        .with_vector_store(graph_vector.vector_store()),
    );
    let ctx = FastPathCtx::new_with_vector_factory(
        session_store.pool().clone(),
        scope,
        graph,
        vector_factory,
        Arc::new(RecordingEmbedder::new()),
        Arc::new(FixedPiiClassifier {
            result: pii_result(SensitivityClass::None),
        }),
        Arc::new(ScriptedConflictDetector {
            conflict: Conflict::Insert,
            delay: Duration::ZERO,
        }),
    )
    .with_assume_app_role(true);

    let forgotten = fast_forget(
        ForgetPattern::NameMatch("lazy-forget-target".to_string()),
        &ctx,
    )
    .await
    .expect("forget should not need read-side configured vector selection");

    assert_eq!(forgotten, 1);
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
        SensitivityClass::None,
    );
    let ctx_b = contact_test_ctx(
        session_store.pool(),
        tenant_id,
        contact_b,
        Conflict::Insert,
        Duration::ZERO,
        SensitivityClass::None,
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
async fn fast_remember_real_detector_flags_restated_fact_as_duplicate_db_memory() {
    // Pins: the production RrfPlusJudgeDetector (NoopReranker + HeuristicJudge, no
    // network/credentials) drives the real fast-path candidate pipeline — vector
    // KNN + lexical FTS + RRF + hydrate against seeded Postgres rows — and the
    // heuristic judge flags a re-stated fact as a duplicate of the existing node.
    // Every other fast-path test injects a scripted detector, so this is the only
    // coverage exercising the real detector's `check_one_fast` against real rows.
    // The detector is given generous budgets so first-call (cold-schema) query
    // compilation does not trip the latency guard; the production 250ms budget is
    // already pinned by `fast_remember_judge_timeout_commits_indeterminate`.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = Uuid::now_v7();
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;

    let fact = "the primary datastore is postgres";

    // Seed an existing node through the real fast write path (scripted Insert) so
    // the detector has a node_index row + embedding to retrieve.
    let seed_ctx = test_ctx(
        session_store.pool(),
        tenant_id,
        Conflict::Insert,
        Duration::ZERO,
        SensitivityClass::None,
    );
    let seeded_uid = fast_remember(tenant_remember_request(tenant_id, fact), &seed_ctx)
        .await
        .expect("seed first fact");

    // Drive the real detector's fast-path detection method directly against the
    // seeded rows. The embedding matches the seeded node's stored vector (same
    // deterministic embedder over identical, unredacted text), so vector KNN and
    // lexical FTS both surface the candidate and the heuristic judge restates it.
    let detector = RrfPlusJudgeDetector::default().with_budgets(
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    );
    let scope = RlsContext::tenant(TenantId::from(tenant_id));
    let vector = Arc::new(PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        scope.clone(),
    ));
    let contradiction_ctx =
        ContradictionContext::for_app_role(session_store.pool().clone(), scope, vector);

    let verdict = detector
        .check_one_fast(
            fact,
            Some(
                moa_memory_vector::QueryEmbedding::new(
                    deterministic_vector(fact),
                    "mock-fast-embedder",
                )
                .expect("valid query embedding"),
            ),
            NodeLabel::Fact,
            SensitivityClass::None,
            &contradiction_ctx,
        )
        .await
        .expect("real detector fast check");

    assert_eq!(
        verdict,
        Conflict::Duplicate(seeded_uid),
        "real detector must flag the restated fact as a duplicate of the seeded node"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn record_incident_writes_scoped_node_and_dedups_within_session_db_memory() {
    // Pins: a durable failure writes one PII-classified, session-scoped Incident
    // node whose properties preserve the attempt/failure and session partition;
    // re-recording the same failure in the same session is a no-op, while the same
    // failure in a different session writes a distinct incident.
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
        SensitivityClass::None,
    );
    seed_tenant_embedder_state(session_store.pool(), tenant_id).await;
    let session_id = Uuid::now_v7();

    let uid = record_incident_with_ctx(tenant_incident_record(tenant_id, session_id), &ctx)
        .await
        .expect("record incident")
        .expect("first incident should write a node");

    assert_eq!(
        node_label(session_store.pool(), tenant_id, uid).await,
        "Incident"
    );
    assert_eq!(
        node_pii_class(session_store.pool(), tenant_id, uid).await,
        "none"
    );
    assert_eq!(
        node_property(session_store.pool(), tenant_id, uid, "attempted").await,
        "search_web"
    );
    assert_eq!(
        node_property(session_store.pool(), tenant_id, uid, "failure").await,
        "provider_error"
    );
    assert_eq!(
        node_property(session_store.pool(), tenant_id, uid, "session_id").await,
        session_id.to_string()
    );
    assert_eq!(
        node_name(session_store.pool(), tenant_id, uid).await,
        "search_web: provider_error"
    );

    // Same failure, same session: deduplicated by name within the session.
    let duplicate = record_incident_with_ctx(tenant_incident_record(tenant_id, session_id), &ctx)
        .await
        .expect("record duplicate incident");
    assert!(
        duplicate.is_none(),
        "identical failure in the same session must not write a second node"
    );
    assert_eq!(
        incident_count_for_tenant(session_store.pool(), tenant_id).await,
        1
    );

    // Same failure, different session: a distinct negative-results node.
    let other_session = Uuid::now_v7();
    record_incident_with_ctx(tenant_incident_record(tenant_id, other_session), &ctx)
        .await
        .expect("record incident for other session")
        .expect("distinct session should write its own incident");
    assert_eq!(
        incident_count_for_tenant(session_store.pool(), tenant_id).await,
        2
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
        SensitivityClass::None,
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

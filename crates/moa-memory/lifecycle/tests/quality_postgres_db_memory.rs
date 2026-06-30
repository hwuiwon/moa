//! Postgres-backed checks for memory quality scoring and cache freshness.

use chrono::Utc;
use moa_core::{
    Channel, ModelId, SessionActorRef, SessionId, SessionMeta, SessionStore as _,
    StoragePartitionId, TenantId, UserId,
};
use moa_memory_lifecycle::compute_quality_scores;
use moa_test_support::fixtures::quote_identifier;
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static QUALITY_TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
async fn quality_scores_count_only_resolved_outcomes_and_are_idempotent() {
    // Pins: quality scoring counts outcome-backed uses, treats only resolved segments as successes, and leaves pending/no-outcome lineage neutral.
    let _guard = QUALITY_TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let user_id = UserId::new("quality-user");
    let session_id = SessionId::new();
    let mixed_uid = Uuid::now_v7();
    let unresolved_uid = Uuid::now_v7();
    let pending_uid = Uuid::now_v7();
    let outside_segment_uid = Uuid::now_v7();
    let now = Utc::now();

    test_db
        .store()
        .create_session(SessionMeta {
            id: session_id,
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session row");
    seed_node_index_row(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        mixed_uid,
        "mixed outcomes",
        now,
    )
    .await;
    seed_node_index_row(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        unresolved_uid,
        "unresolved only",
        now,
    )
    .await;
    seed_node_index_row(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        pending_uid,
        "pending outcome",
        now,
    )
    .await;
    seed_node_index_row(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        outside_segment_uid,
        "outside segment",
        now,
    )
    .await;
    seed_task_segment(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        tenant_id,
        session_id,
        0,
        Some("resolved"),
        now,
    )
    .await;
    seed_task_segment(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        tenant_id,
        session_id,
        1,
        Some("failed"),
        now,
    )
    .await;
    seed_task_segment(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        tenant_id,
        session_id,
        2,
        Some("resolved"),
        now,
    )
    .await;
    seed_task_segment(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        tenant_id,
        session_id,
        3,
        None,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        mixed_uid,
        1,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        mixed_uid,
        2,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        mixed_uid,
        3,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        unresolved_uid,
        2,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        pending_uid,
        4,
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        outside_segment_uid,
        99,
        now,
    )
    .await;

    let first = compute_quality_scores(test_db.store().pool(), &tenant_id, 30)
        .await
        .expect("compute quality scores");

    assert_eq!(first.scored, 2);
    assert_eq!(first.skipped_no_outcome_source, 0);
    assert_quality_score(test_db.store().pool(), mixed_uid, 3.0 / 5.0).await;
    assert_quality_score(test_db.store().pool(), unresolved_uid, 1.0 / 3.0).await;
    assert_quality_score(test_db.store().pool(), pending_uid, 0.5).await;
    assert_quality_score(test_db.store().pool(), outside_segment_uid, 0.5).await;
    let first_version = workspace_version(test_db.store().pool(), &storage_partition_id).await;
    assert_eq!(first_version, 1);

    let second = compute_quality_scores(test_db.store().pool(), &tenant_id, 30)
        .await
        .expect("recompute quality scores");

    assert_eq!(second.scored, 0);
    assert_eq!(
        workspace_version(test_db.store().pool(), &storage_partition_id).await,
        first_version
    );

    cleanup_quality_rows(test_db.store().pool(), &storage_partition_id).await;
}

#[tokio::test]
async fn quality_scores_skip_when_task_segment_outcome_source_is_unavailable() {
    // Pins: manual quality scoring reports a no-source skip instead of mutating neutral priors.
    let _guard = QUALITY_TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let user_id = UserId::new("quality-no-source-user");
    let session_id = SessionId::new();
    let node_uid = Uuid::now_v7();
    let now = Utc::now();

    test_db
        .store()
        .create_session(SessionMeta {
            id: session_id,
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session row");
    seed_node_index_row(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        node_uid,
        "no source",
        now,
    )
    .await;
    seed_retrieval_lineage(
        test_db.store().pool(),
        &storage_partition_id,
        &user_id,
        session_id,
        node_uid,
        1,
        now,
    )
    .await;
    hide_task_segments_source(&test_db).await;

    let stats = compute_quality_scores(test_db.store().pool(), &tenant_id, 30)
        .await
        .expect("compute quality scores without an outcome source");

    assert_eq!(stats.scored, 0);
    assert_eq!(stats.skipped_no_outcome_source, 1);
    assert_quality_score(test_db.store().pool(), node_uid, 0.5).await;

    cleanup_quality_rows(test_db.store().pool(), &storage_partition_id).await;
}

async fn seed_node_index_row(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    node_uid: Uuid,
    name: &str,
    retrieved_at: chrono::DateTime<Utc>,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, user_id, name, pii_class, confidence, valid_from, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, 'none', 0.9, $5, $6)
        "#,
    )
    .bind(node_uid)
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_str())
    .bind(name)
    .bind(retrieved_at)
    .bind(json!({ "source": "quality_test" }))
    .execute(pool)
    .await
    .expect("seed node index row");
}

// Seeds one `task_segments` row; the column set maps directly to function
// parameters, so the arity matches the table rather than a missing abstraction.
#[allow(clippy::too_many_arguments)]
async fn seed_task_segment(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    tenant_id: TenantId,
    session_id: SessionId,
    segment_index: i32,
    outcome: Option<&str>,
    started_at: chrono::DateTime<Utc>,
) {
    sqlx::query(
        r#"
        INSERT INTO task_segments
            (id, session_id, storage_partition_id, user_id, tenant_id, segment_index, started_at, ended_at,
             outcome, tools_used, skills_activated, turn_count, token_cost)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $7, $8, '{}', '{}', 1, 0)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(session_id.0)
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_str())
    .bind(tenant_id.to_string())
    .bind(segment_index)
    .bind(started_at)
    .bind(outcome)
    .execute(pool)
    .await
    .expect("seed task segment");
}

async fn seed_retrieval_lineage(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: &UserId,
    session_id: SessionId,
    node_uid: Uuid,
    turn_seq: i64,
    retrieved_at: chrono::DateTime<Utc>,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.retrieval_lineage
            (storage_partition_id, user_id, session_id, turn_seq, uid, rank, retrieved_at)
        VALUES ($1, $2, $3, $4, $5, 1, $6)
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_str())
    .bind(session_id.0)
    .bind(turn_seq)
    .bind(node_uid)
    .bind(retrieved_at)
    .execute(pool)
    .await
    .expect("seed retrieval lineage row");
}

async fn hide_task_segments_source(test_db: &TestDb) {
    let schema_name = quote_identifier(test_db.schema_name());
    sqlx::query(&format!("DROP TABLE {schema_name}.task_segments CASCADE"))
        .execute(test_db.store().pool())
        .await
        .expect("drop isolated task_segments source");
    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
        .bind(schema_name)
        .execute(test_db.store().pool())
        .await
        .expect("hide public task_segments from test connection");
}

async fn assert_quality_score(pool: &PgPool, uid: Uuid, expected: f64) {
    let score =
        sqlx::query_scalar::<_, f64>("SELECT quality_score FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("read quality score");
    assert!(
        (score - expected).abs() < f64::EPSILON,
        "expected quality score {expected}, got {score}"
    );
}

async fn workspace_version(pool: &PgPool, storage_partition_id: &StoragePartitionId) -> i64 {
    let row = sqlx::query(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id.as_str())
    .fetch_one(pool)
    .await
    .expect("read workspace changelog version");
    row.try_get("changelog_version")
        .expect("decode changelog version")
}

async fn cleanup_quality_rows(pool: &PgPool, storage_partition_id: &StoragePartitionId) {
    sqlx::query("DELETE FROM moa.retrieval_lineage WHERE storage_partition_id = $1")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .expect("delete quality lineage rows");
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = $1")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .expect("delete quality node rows");
    sqlx::query("DELETE FROM moa.storage_partition_state WHERE storage_partition_id = $1")
        .bind(storage_partition_id.as_str())
        .execute(pool)
        .await
        .expect("delete quality partition state");
}

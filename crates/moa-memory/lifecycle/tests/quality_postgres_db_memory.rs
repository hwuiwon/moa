//! Postgres-backed checks for memory quality scoring and cache freshness.

use chrono::Utc;
use moa_core::{ModelId, Platform, SessionId, SessionMeta, SessionStore as _, UserId, WorkspaceId};
use moa_memory_lifecycle::compute_quality_scores;
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn quality_scores_use_task_segment_outcomes_and_bump_workspace_version_once() {
    // Pins: lifecycle quality scoring reads task_segments.outcome and invalidates retrieval cache exactly when scores change.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = WorkspaceId::new(format!("quality-{}", Uuid::now_v7().simple()));
    let user_id = UserId::new("quality-user");
    let session_id = SessionId::new();
    let node_uid = Uuid::now_v7();
    let now = Utc::now();

    test_db
        .store()
        .create_session(SessionMeta {
            id: session_id,
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            platform: Platform::Api,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session row");
    seed_quality_inputs(
        test_db.store().pool(),
        &workspace_id,
        &user_id,
        session_id,
        node_uid,
        now,
    )
    .await;

    let first = compute_quality_scores(test_db.store().pool(), &workspace_id, 30)
        .await
        .expect("compute quality scores");

    assert_eq!(first.scored, 1);
    assert_eq!(first.skipped_no_outcome_source, 0);
    assert_quality_score(test_db.store().pool(), node_uid, 2.0 / 3.0).await;
    let first_version = workspace_version(test_db.store().pool(), &workspace_id).await;
    assert_eq!(first_version, 1);

    let second = compute_quality_scores(test_db.store().pool(), &workspace_id, 30)
        .await
        .expect("recompute quality scores");

    assert_eq!(second.scored, 0);
    assert_eq!(
        workspace_version(test_db.store().pool(), &workspace_id).await,
        first_version
    );
}

async fn seed_quality_inputs(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    user_id: &UserId,
    session_id: SessionId,
    node_uid: Uuid,
    retrieved_at: chrono::DateTime<Utc>,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, workspace_id, user_id, name, pii_class, confidence, valid_from, properties_summary)
        VALUES ($1, 'Fact', $2, $3, 'quality scored fact', 'none', 0.9, $4, $5)
        "#,
    )
    .bind(node_uid)
    .bind(workspace_id.as_str())
    .bind(user_id.as_str())
    .bind(retrieved_at)
    .bind(json!({ "source": "quality_test" }))
    .execute(pool)
    .await
    .expect("seed node index row");

    sqlx::query(
        r#"
        INSERT INTO task_segments
            (id, session_id, workspace_id, user_id, tenant_id, segment_index, started_at, ended_at,
             outcome, tools_used, skills_activated, turn_count, token_cost)
        VALUES ($1, $2, $3, $4, $3, 0, $5, $5, 'resolved', '{}', '{}', 1, 0)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(session_id.0)
    .bind(workspace_id.as_str())
    .bind(user_id.as_str())
    .bind(retrieved_at)
    .execute(pool)
    .await
    .expect("seed resolved task segment");

    sqlx::query(
        r#"
        INSERT INTO moa.retrieval_lineage
            (workspace_id, user_id, session_id, turn_seq, uid, rank, retrieved_at)
        VALUES ($1, $2, $3, 1, $4, 1, $5)
        "#,
    )
    .bind(workspace_id.as_str())
    .bind(user_id.as_str())
    .bind(session_id.0)
    .bind(node_uid)
    .bind(retrieved_at)
    .execute(pool)
    .await
    .expect("seed retrieval lineage row");
}

async fn assert_quality_score(pool: &PgPool, uid: Uuid, expected: f64) {
    let score =
        sqlx::query_scalar::<_, f64>("SELECT quality_score FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("read quality score");
    assert!((score - expected).abs() < f64::EPSILON);
}

async fn workspace_version(pool: &PgPool, workspace_id: &WorkspaceId) -> i64 {
    let row =
        sqlx::query("SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1")
            .bind(workspace_id.as_str())
            .fetch_one(pool)
            .await
            .expect("read workspace changelog version");
    row.try_get("changelog_version")
        .expect("decode changelog version")
}

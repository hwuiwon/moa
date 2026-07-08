//! Postgres-backed checks for ACE-style skill-lesson curation: exact-normalized
//! summary merge into the oldest canonical, age-based retirement, and the
//! renewal exemption that spares an old canonical still gathering restatements.

use chrono::{DateTime, Duration, TimeZone, Utc};
use moa_core::{StoragePartitionId, TenantId};
use moa_memory_lifecycle::{LessonCurationOptions, curate_skill_lessons};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::PgPool;
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
async fn duplicate_lessons_merge_into_oldest_canonical_db_memory() {
    // Pins: lessons whose summaries normalize equal merge into the oldest
    // canonical by supersession; the canonical stays active, the rest close.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let skill_uid = Uuid::now_v7();
    let now = fixed_instant();
    let fresh = now - Duration::days(10);

    let canonical = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Rotate keys before deploy",
        fresh,
    )
    .await;
    let dup_whitespace = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "rotate  keys before deploy.",
        fresh + Duration::seconds(1),
    )
    .await;
    let dup_case = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Rotate Keys Before Deploy",
        fresh + Duration::seconds(2),
    )
    .await;

    let stats = curate_skill_lessons(pool, tenant_id, now, &LessonCurationOptions::default())
        .await
        .expect("curate skill lessons");

    assert_eq!(stats.merged, 2, "both duplicates superseded into canonical");
    assert_eq!(stats.retired, 0, "fresh lessons are not retired");
    assert_active(pool, canonical, true).await;
    assert_active(pool, dup_whitespace, false).await;
    assert_active(pool, dup_case, false).await;
    assert!(
        supersedes_edge_exists(pool, canonical, dup_whitespace).await,
        "canonical supersedes the whitespace duplicate"
    );
    assert!(
        supersedes_edge_exists(pool, canonical, dup_case).await,
        "canonical supersedes the case duplicate"
    );
}

#[tokio::test]
async fn stale_unrenewed_lesson_retires_db_memory() {
    // Pins: a distinct lesson older than retire_after is bitemporally closed,
    // while a fresh distinct lesson in the same skill is left untouched.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let skill_uid = Uuid::now_v7();
    let now = fixed_instant();

    let stale = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Old advice nobody renewed",
        now - Duration::days(200),
    )
    .await;
    let fresh = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Recent distinct advice",
        now - Duration::days(10),
    )
    .await;

    let stats = curate_skill_lessons(pool, tenant_id, now, &LessonCurationOptions::default())
        .await
        .expect("curate skill lessons");

    assert_eq!(stats.merged, 0, "distinct summaries do not merge");
    assert_eq!(stats.retired, 1, "only the stale lesson retires");
    assert_active(pool, stale, false).await;
    assert_active(pool, fresh, true).await;
}

#[tokio::test]
async fn fresh_unique_lesson_untouched_db_memory() {
    // Pins: a single fresh distinct lesson is neither merged nor retired.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let skill_uid = Uuid::now_v7();
    let now = fixed_instant();

    let lesson = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Keep the only lesson",
        now - Duration::days(5),
    )
    .await;

    let stats = curate_skill_lessons(pool, tenant_id, now, &LessonCurationOptions::default())
        .await
        .expect("curate skill lessons");

    assert_eq!(stats, Default::default(), "no work on a lone fresh lesson");
    assert_active(pool, lesson, true).await;
}

#[tokio::test]
async fn old_canonical_renewed_by_fresh_duplicate_survives_retirement_db_memory() {
    // Pins: an old canonical that absorbs a duplicate created within the
    // retention window is renewed, so retirement leaves it active.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let skill_uid = Uuid::now_v7();
    let now = fixed_instant();

    let old_canonical = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "Prefer idempotent retries",
        now - Duration::days(200),
    )
    .await;
    let fresh_restatement = seed_lesson(
        pool,
        &storage_partition_id,
        tenant_id,
        skill_uid,
        "prefer idempotent retries.",
        now - Duration::days(10),
    )
    .await;

    let stats = curate_skill_lessons(pool, tenant_id, now, &LessonCurationOptions::default())
        .await
        .expect("curate skill lessons");

    assert_eq!(stats.merged, 1, "the fresh restatement merges in");
    assert_eq!(stats.retired, 0, "the renewed canonical is not retired");
    assert_active(pool, old_canonical, true).await;
    assert_active(pool, fresh_restatement, false).await;
}

fn fixed_instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0)
        .single()
        .expect("fixed timestamp")
}

async fn seed_lesson(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    skill_uid: Uuid,
    summary: &str,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let uid = Uuid::now_v7();
    let name: String = summary.chars().take(80).collect();
    let properties = json!({
        "skill_uid": skill_uid.to_string(),
        "summary": summary,
        "text": summary,
    });
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, user_id, contact_id, name,
             pii_class, confidence, valid_from, last_accessed_at, properties_summary)
        VALUES ($1, 'Lesson', $2, $3, NULL, NULL, $4, 'none', 1.0, $5, $5, $6::jsonb)
        "#,
    )
    .bind(uid)
    .bind(storage_partition_id.as_str())
    .bind(tenant_id.0)
    .bind(name)
    .bind(valid_from)
    .bind(properties)
    .execute(pool)
    .await
    .expect("seed lesson node");
    uid
}

async fn assert_active(pool: &PgPool, uid: Uuid, expected: bool) {
    let active =
        sqlx::query_scalar::<_, bool>("SELECT valid_to IS NULL FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("read node active state");
    assert_eq!(active, expected, "unexpected active state for {uid}");
}

async fn supersedes_edge_exists(pool: &PgPool, replacement: Uuid, old: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM moa.edge_index
            WHERE label = 'SUPERSEDES'
              AND start_uid = $1
              AND end_uid = $2
        )
        "#,
    )
    .bind(replacement)
    .bind(old)
    .fetch_one(pool)
    .await
    .expect("read supersedes edge")
}

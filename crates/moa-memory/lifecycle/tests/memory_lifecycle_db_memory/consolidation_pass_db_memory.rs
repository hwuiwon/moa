//! Postgres-backed checks for the set-based decay pass, the shared single-load
//! consolidation pass, and the incremental consolidation cursor.

use chrono::{DateTime, Duration, TimeZone, Utc};
use moa_core::{StoragePartitionId, TenantId};
use moa_memory_lifecycle::{
    ConsolidationOptions, TenantConsolidationCursor, advance_consolidation_watermark,
    consolidate_tenant, decay_confidence, decay_target_confidence, expire_idle_facts,
    tenants_needing_consolidation,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::{Value, json};
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
async fn set_based_decay_matches_reference_confidence_db_memory() {
    // Pins: the single set-based decay UPDATE reproduces the per-node anchored-decay
    // confidences and the decayed/at-floor counts, and is idempotent at a fixed instant.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let opts = ConsolidationOptions::default();
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    // Fresh fact: too recent to decay, must stay untouched.
    let fresh = seed_fact(pool, &storage_partition_id, tenant_id, 0.9, now, None).await;
    // Idle fact without a stored anchor: decays from its live confidence.
    let idle = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        0.8,
        now - Duration::days(240),
        None,
    )
    .await;
    // Stale low-confidence fact: decays down to the configured floor.
    let at_floor = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        0.5,
        now - Duration::days(720),
        None,
    )
    .await;
    // Partially decayed fact: decays anchored to its stored base_confidence.
    let anchored = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        0.4,
        now - Duration::days(240),
        Some(0.9),
    )
    .await;

    let stats = decay_confidence(pool, &tenant_id, now, &opts)
        .await
        .expect("run set-based decay");

    // Three idle facts move, the fresh one does not; only the floor fact sits at the floor.
    assert_eq!(stats.decayed, 3, "unexpected decayed count");
    assert_eq!(stats.at_floor, 1, "unexpected at-floor count");

    assert_confidence(pool, fresh, 0.9).await;
    assert_confidence(
        pool,
        idle,
        decay_target_confidence(0.8, now - Duration::days(240), now, &opts).expect("idle target"),
    )
    .await;
    assert_confidence(pool, at_floor, opts.decay_floor).await;
    assert_confidence(
        pool,
        anchored,
        decay_target_confidence(0.9, now - Duration::days(240), now, &opts)
            .expect("anchored target"),
    )
    .await;

    // The decay anchor is recorded for facts that had none, and preserved otherwise.
    assert_eq!(base_confidence(pool, idle).await, Some(0.8));
    assert_eq!(base_confidence(pool, anchored).await, Some(0.9));

    // The pass bumps the partition changelog version exactly once when it writes.
    assert_eq!(changelog_version(pool, &storage_partition_id).await, 1);

    // A second pass at the same instant is a no-op and does not bump again.
    let second = decay_confidence(pool, &tenant_id, now, &opts)
        .await
        .expect("rerun set-based decay");
    assert_eq!(second.decayed, 0);
    assert_eq!(changelog_version(pool, &storage_partition_id).await, 1);
}

#[tokio::test]
async fn consolidate_tenant_shares_one_snapshot_and_is_idempotent_db_memory() {
    // Pins: one consolidation pass merges duplicates, decays idle facts, sweeps
    // contradictions and rebuilds digests from a single shared fact load, and a
    // second pass at the same instant performs no mutating work.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    // Two exact duplicates collapse into one canonical fact.
    seed_spo_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        "dup-hash",
        "deploy pipeline",
        "prefers",
        "green builds",
        now - Duration::days(1),
    )
    .await;
    seed_spo_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        "dup-hash",
        "deploy pipeline",
        "prefers",
        "green builds",
        now,
    )
    .await;
    // Two contradictory deploy-target facts: the newest supersedes the older one.
    seed_spo_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        "target-old",
        "checkout-service",
        "deploy_target",
        "staging",
        now - Duration::days(2),
    )
    .await;
    seed_spo_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        "target-new",
        "checkout-service",
        "deploy_target",
        "production",
        now - Duration::days(1),
    )
    .await;
    // An idle fact that will decay.
    seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        0.8,
        now - Duration::days(240),
        None,
    )
    .await;

    let outcome = consolidate_tenant(pool, tenant_id, ConsolidationOptions::default(), now, None)
        .await
        .expect("first consolidation pass");

    assert_eq!(outcome.merged, 1, "exactly one duplicate merged");
    assert_eq!(
        outcome.contradiction_supersessions, 1,
        "exactly one contradiction superseded"
    );
    assert!(outcome.decayed >= 1, "the idle fact decayed");
    assert!(
        outcome.digests_rebuilt >= 1,
        "a standing digest was rebuilt"
    );
    assert_eq!(outcome.duplicates_remaining, 0);

    let second = consolidate_tenant(pool, tenant_id, ConsolidationOptions::default(), now, None)
        .await
        .expect("second consolidation pass");
    assert!(
        second.has_no_work(),
        "second consolidation pass must be idempotent, got {second:?}"
    );
}

#[tokio::test]
async fn consolidation_cursor_short_circuits_unchanged_tenant_db_memory() {
    // Pins: the incremental cursor returns only tenants whose changelog advanced
    // past their recorded consolidation watermark, and advancing the watermark
    // makes an unchanged tenant short-circuit until a later write bumps it.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let changed = TenantId::new();
    let idle = TenantId::new();

    seed_partition_state(pool, changed, 5, 0).await;
    seed_partition_state(pool, idle, 3, 3).await;

    let pending = tenants_needing_consolidation(pool)
        .await
        .expect("enumerate pending tenants");
    let changed_cursor = TenantConsolidationCursor {
        tenant_id: changed,
        changelog_version: 5,
    };
    assert!(
        pending.contains(&changed_cursor),
        "changed tenant must be pending at the observed version"
    );
    assert!(
        !pending.iter().any(|cursor| cursor.tenant_id == idle),
        "caught-up tenant must short-circuit"
    );

    // A graph write that arrives while consolidation is running must remain
    // visible after the workflow advances only the version it actually covered.
    bump_changelog_version(pool, changed).await;
    advance_consolidation_watermark(pool, &[changed_cursor])
        .await
        .expect("advance watermark to observed version");

    let after_advance = tenants_needing_consolidation(pool)
        .await
        .expect("re-enumerate after advance");
    assert!(
        after_advance.contains(&TenantConsolidationCursor {
            tenant_id: changed,
            changelog_version: 6,
        }),
        "late write must remain pending after advancing to the observed version"
    );
    assert!(!after_advance.iter().any(|cursor| cursor.tenant_id == idle));

    advance_consolidation_watermark(
        pool,
        &[TenantConsolidationCursor {
            tenant_id: changed,
            changelog_version: 6,
        }],
    )
    .await
    .expect("advance watermark to caught-up version");

    let after_caught_up = tenants_needing_consolidation(pool)
        .await
        .expect("re-enumerate after caught-up advance");
    assert!(
        !after_caught_up
            .iter()
            .any(|cursor| cursor.tenant_id == changed),
        "caught-up tenant must short-circuit"
    );
}

async fn seed_fact(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    confidence: f64,
    last_accessed_at: DateTime<Utc>,
    base_confidence: Option<f64>,
) -> Uuid {
    let uid = Uuid::now_v7();
    let mut properties = serde_json::Map::new();
    properties.insert("subject".to_string(), json!("service"));
    properties.insert("predicate".to_string(), json!("prefers"));
    properties.insert("object".to_string(), json!("value"));
    if let Some(base) = base_confidence {
        properties.insert("base_confidence".to_string(), json!(base));
        properties.insert("confidence".to_string(), json!(confidence));
    }
    insert_fact_row(
        pool,
        storage_partition_id,
        tenant_id,
        uid,
        "service prefers value",
        confidence,
        last_accessed_at,
        Value::Object(properties),
    )
    .await;
    uid
}

#[allow(clippy::too_many_arguments)]
async fn seed_spo_fact(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    fact_hash: &str,
    subject: &str,
    predicate: &str,
    object: &str,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let uid = Uuid::now_v7();
    let properties = json!({
        "subject": subject,
        "predicate": predicate,
        "object": object,
        "fact_hash": fact_hash,
    });
    insert_fact_row(
        pool,
        storage_partition_id,
        tenant_id,
        uid,
        &format!("{subject} {predicate} {object}"),
        0.9,
        valid_from,
        properties,
    )
    .await;
    // Align valid_from with the seeded timestamp so duplicate/contradiction
    // ordering is deterministic.
    sqlx::query("UPDATE moa.node_index SET valid_from = $2 WHERE uid = $1")
        .bind(uid)
        .bind(valid_from)
        .execute(pool)
        .await
        .expect("pin valid_from");
    uid
}

#[tokio::test]
async fn idle_floor_facts_expire_bitemporally_and_rerun_is_noop_db_memory() {
    // Pins: expiry closes only floor-bound facts idle past the window, with a
    // bitemporal close (`valid_to`/`expired_idle` reason, row preserved); floor
    // facts inside the window and above-floor idle facts survive, and rerunning
    // at the same instant is a no-op.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let opts = ConsolidationOptions::default();
    let now = Utc
        .with_ymd_and_hms(2026, 6, 11, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    // Floor-bound and idle past the 180-day window: must expire.
    let expired = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        opts.decay_floor,
        now - Duration::days(200),
        Some(0.9),
    )
    .await;
    // Floor-bound but inside the window: must survive.
    let floor_recent = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        opts.decay_floor,
        now - Duration::days(100),
        Some(0.9),
    )
    .await;
    // Above the floor, however idle: must survive.
    let idle_confident = seed_fact(
        pool,
        &storage_partition_id,
        tenant_id,
        0.8,
        now - Duration::days(400),
        None,
    )
    .await;

    let stats = expire_idle_facts(pool, &tenant_id, now, &opts)
        .await
        .expect("run idle expiry");

    assert_eq!(stats.expired_idle, 1, "unexpected expiry count");
    let (valid_to, reason) = node_validity(pool, expired).await;
    assert_eq!(valid_to, Some(now), "expired fact must close at `now`");
    assert_eq!(
        reason.as_deref(),
        Some(moa_memory_lifecycle::EXPIRED_IDLE_REASON)
    );
    assert_eq!(node_validity(pool, floor_recent).await, (None, None));
    assert_eq!(node_validity(pool, idle_confident).await, (None, None));

    // Rerun at the same instant: closed rows leave the candidate set.
    let second = expire_idle_facts(pool, &tenant_id, now, &opts)
        .await
        .expect("rerun idle expiry");
    assert_eq!(second.expired_idle, 0, "expiry rerun must be a no-op");

    // Disabled window expires nothing.
    let disabled = ConsolidationOptions {
        expire_idle_days: 0,
        ..ConsolidationOptions::default()
    };
    let none = expire_idle_facts(pool, &tenant_id, now, &disabled)
        .await
        .expect("run disabled expiry");
    assert_eq!(none.expired_idle, 0, "disabled expiry must not close facts");
}

async fn node_validity(pool: &PgPool, uid: Uuid) -> (Option<DateTime<Utc>>, Option<String>) {
    let row = sqlx::query("SELECT valid_to, invalidated_reason FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
        .expect("read node validity");
    (
        row.try_get("valid_to").expect("valid_to column"),
        row.try_get("invalidated_reason")
            .expect("invalidated_reason column"),
    )
}

#[allow(clippy::too_many_arguments)]
async fn insert_fact_row(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    uid: Uuid,
    name: &str,
    confidence: f64,
    last_accessed_at: DateTime<Utc>,
    properties: Value,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, name, pii_class, confidence,
             valid_from, last_accessed_at, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, 'none', $5, $6, $6, $7)
        "#,
    )
    .bind(uid)
    .bind(storage_partition_id.as_str())
    .bind(tenant_id.0)
    .bind(name)
    .bind(confidence)
    .bind(last_accessed_at)
    .bind(properties)
    .execute(pool)
    .await
    .expect("seed fact row");
}

async fn seed_partition_state(
    pool: &PgPool,
    tenant_id: TenantId,
    changelog_version: i64,
    consolidated_changelog_version: i64,
) {
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, changelog_version, consolidated_changelog_version)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(changelog_version)
    .bind(consolidated_changelog_version)
    .execute(pool)
    .await
    .expect("seed partition state");
}

async fn bump_changelog_version(pool: &PgPool, tenant_id: TenantId) {
    sqlx::query(
        "UPDATE moa.storage_partition_state SET changelog_version = changelog_version + 1 WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .execute(pool)
    .await
    .expect("bump changelog version");
}

async fn assert_confidence(pool: &PgPool, uid: Uuid, expected: f64) {
    let confidence = sqlx::query_scalar::<_, Option<f64>>(
        "SELECT confidence FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .expect("read confidence")
    .expect("confidence present");
    assert!(
        (confidence - expected).abs() < 1e-9,
        "expected confidence {expected}, got {confidence} for {uid}"
    );
}

async fn base_confidence(pool: &PgPool, uid: Uuid) -> Option<f64> {
    let properties = sqlx::query_scalar::<_, Option<Value>>(
        "SELECT properties_summary FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .expect("read properties");
    properties
        .as_ref()
        .and_then(|value| value.get("base_confidence"))
        .and_then(Value::as_f64)
}

async fn changelog_version(pool: &PgPool, storage_partition_id: &StoragePartitionId) -> i64 {
    sqlx::query(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id.as_str())
    .fetch_one(pool)
    .await
    .expect("read changelog version")
    .try_get("changelog_version")
    .expect("decode changelog version")
}

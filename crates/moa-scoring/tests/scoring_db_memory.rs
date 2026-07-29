//! Execution-lane tests for the score-query runtime path.
//!
//! moa-scoring previously had no `tests/` directory: every async query function
//! and score-parent helpers were unexecuted, pinned only by SQL string-matches.
//! These tests seed real `analytics.scores` / `analytics.score_run` rows in an
//! isolated test schema and drive the production functions end to end,
//! asserting aggregation, cross-tenant filtering, NULL-delta handling, and
//! `ensure_score_run_parent` idempotency plus source-mismatch behavior.
//!
//! Rows for the shared `analytics`/`moa` schemas are isolated per test by a
//! unique tenant UUID (storage partition) and unique run UUIDs, matching the
//! sibling `moa-experiments` `_db` tests. The local Docker Postgres connects as
//! the table owner, which carries the `owner_dev_access` RLS policy, so seeds
//! and reads use the plain pool.

use moa_core::{
    types::action_policy::ActionRuleScope, types::contact::ContactId,
    types::experiments::ScorecardValueType, types::identifiers::TenantId,
};
use moa_scoring::{
    Error, ScoreCompareRef, ScoreRunRef, compare_score_runs_for_tenant, ensure_score_run_parent,
    score_summaries_for_tenant,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const SCORE_RUN_SOURCE_EXPERIMENT_RUN: &str = "experiment_run";
const SCORE_RUN_SOURCE_OTHER: &str = "other";

fn approx(actual: Option<f64>, expected: f64) {
    let value = actual.unwrap_or_else(|| panic!("expected a numeric aggregate, got NULL"));
    assert!(
        (value - expected).abs() < 1e-9,
        "expected ~{expected}, got {value}"
    );
}

async fn insert_score(
    pool: &PgPool,
    storage_partition_id: &str,
    run_id: Uuid,
    name: &str,
    value_type: &str,
    value_numeric: Option<f64>,
    value_boolean: Option<bool>,
) {
    sqlx::query(
        r#"
        INSERT INTO analytics.scores (
            score_id, ts, storage_partition_id, target_kind, run_id, name,
            value_type, value_numeric, value_boolean, source, model_or_evaluator
        )
        VALUES ($1, now(), $2, 'agent_loop', $3, $4, $5, $6, $7, 'test', 'offline')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(storage_partition_id)
    .bind(run_id)
    .bind(name)
    .bind(value_type)
    .bind(value_numeric)
    .bind(value_boolean)
    .execute(pool)
    .await
    .expect("insert analytics.scores row");
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn score_summaries_for_tenant_aggregates_and_excludes_other_tenant_rows_db_memory() {
    // Pins: per-(name, value_type) aggregation coalesces numeric mean / boolean
    // rate, and the storage-partition filter excludes another tenant's rows that
    // share the same run_id (the real cross-tenant guard behind the SQL tripwire).
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap test db");
    let pool = test_db.store().pool();

    let tenant = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let sp = tenant.to_string();
    let other_sp = other_tenant.to_string();
    let run_id = Uuid::now_v7();

    insert_score(pool, &sp, run_id, "quality", "numeric", Some(0.4), None).await;
    insert_score(pool, &sp, run_id, "quality", "numeric", Some(0.8), None).await;
    insert_score(pool, &sp, run_id, "grounded", "boolean", None, Some(true)).await;
    // Same run_id but a different tenant partition: must be filtered out.
    insert_score(
        pool,
        &other_sp,
        run_id,
        "quality",
        "numeric",
        Some(0.0),
        None,
    )
    .await;

    let summary = score_summaries_for_tenant(
        pool,
        ScoreRunRef {
            tenant_id: tenant,
            run_id,
        },
    )
    .await
    .expect("score summaries");

    assert_eq!(summary.run_id, run_id);
    assert_eq!(summary.rows.len(), 2, "two score groups for this tenant");

    let grounded = &summary.rows[0];
    assert_eq!(grounded.name, "grounded");
    assert_eq!(grounded.value_type, ScorecardValueType::Boolean);
    assert_eq!(grounded.n, 1);
    approx(grounded.mean_or_rate, 1.0);

    let quality = &summary.rows[1];
    assert_eq!(quality.name, "quality");
    assert_eq!(quality.value_type, ScorecardValueType::Numeric);
    assert_eq!(
        quality.n, 2,
        "other tenant's row is excluded from the count"
    );
    approx(quality.mean_or_rate, 0.6); // (0.4 + 0.8) / 2, not polluted by the 0.0 leak.
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn compare_score_runs_returns_delta_and_null_for_one_sided_runs_db_memory() {
    // Pins: numeric comparison subtracts means, and a score present in only one
    // run yields a NULL delta instead of a fabricated zero magnitude.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap test db");
    let pool = test_db.store().pool();

    let tenant = TenantId::from(Uuid::now_v7());
    let sp = tenant.to_string();
    let base_run = Uuid::now_v7();
    let new_run = Uuid::now_v7();

    insert_score(pool, &sp, base_run, "quality", "numeric", Some(0.5), None).await;
    insert_score(pool, &sp, base_run, "only_base", "numeric", Some(0.2), None).await;
    insert_score(pool, &sp, new_run, "quality", "numeric", Some(0.9), None).await;

    let compare = compare_score_runs_for_tenant(
        pool,
        ScoreCompareRef {
            tenant_id: tenant,
            base_run,
            new_run,
        },
    )
    .await
    .expect("compare score runs");

    assert_eq!(compare.rows.len(), 2);

    let only_base = &compare.rows[0];
    assert_eq!(only_base.name, "only_base");
    approx(only_base.base_mean, 0.2);
    assert_eq!(only_base.new_mean, None, "absent new run side stays NULL");
    assert_eq!(
        only_base.delta, None,
        "one-sided score must not coalesce to zero"
    );

    let quality = &compare.rows[1];
    assert_eq!(quality.name, "quality");
    approx(quality.base_mean, 0.5);
    approx(quality.new_mean, 0.9);
    approx(quality.delta, 0.4); // 0.9 - 0.5
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn ensure_score_run_parent_is_idempotent_and_rejects_source_mismatch_db_memory() {
    // Pins: re-inserting the same score-run parent within one scope is a no-op
    // (Ok, single row), while a conflicting source for the same run_id returns
    // ScoreRunMismatch.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap test db");
    let pool = test_db.store().pool();

    let tenant = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id: tenant };
    let run_id = Uuid::now_v7();

    let mut conn = pool.acquire().await.expect("acquire connection");

    ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_EXPERIMENT_RUN)
        .await
        .expect("first ensure inserts parent");
    ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_EXPERIMENT_RUN)
        .await
        .expect("second ensure is idempotent");

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.score_run WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("count parents");
    assert_eq!(count, 1, "idempotent ensure must not duplicate the parent");

    let error = ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_OTHER)
        .await
        .expect_err("conflicting source must be rejected");
    assert!(
        matches!(
            error,
            Error::ScoreRunMismatch { score_run_id, expected_source }
                if score_run_id == run_id && expected_source == SCORE_RUN_SOURCE_OTHER
        ),
        "expected ScoreRunMismatch, got {error:?}"
    );
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn ensure_score_run_parent_preserves_contact_scope_db_memory() {
    // Pins: score-run parents can be contact-scoped and the same tenant's other contacts cannot reuse them.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap test db");
    let pool = test_db.store().pool();

    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId(Uuid::now_v7());
    let other_contact_id = ContactId(Uuid::now_v7());
    let contact_scope = ActionRuleScope::Contact {
        tenant_id,
        contact_id,
    };
    let other_contact_scope = ActionRuleScope::Contact {
        tenant_id,
        contact_id: other_contact_id,
    };
    let run_id = Uuid::now_v7();

    let mut conn = pool.acquire().await.expect("acquire connection");
    ensure_score_run_parent(
        &mut conn,
        &contact_scope,
        run_id,
        SCORE_RUN_SOURCE_EXPERIMENT_RUN,
    )
    .await
    .expect("contact scope inserts parent");
    ensure_score_run_parent(
        &mut conn,
        &contact_scope,
        run_id,
        SCORE_RUN_SOURCE_EXPERIMENT_RUN,
    )
    .await
    .expect("same contact scope is idempotent");

    let row = sqlx::query(
        "SELECT scope, storage_partition_id, user_id FROM analytics.score_run WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .expect("load score-run parent");
    assert_eq!(row.get::<String, _>("scope"), "contact");
    assert_eq!(
        row.get::<Option<String>, _>("storage_partition_id")
            .as_deref(),
        Some(tenant_id.to_string().as_str())
    );
    assert_eq!(
        row.get::<Option<String>, _>("user_id").as_deref(),
        Some(contact_id.to_string().as_str())
    );

    let error = ensure_score_run_parent(
        &mut conn,
        &other_contact_scope,
        run_id,
        SCORE_RUN_SOURCE_EXPERIMENT_RUN,
    )
    .await
    .expect_err("other contact must not reuse the score-run parent");
    assert!(
        matches!(
            error,
            Error::ScoreRunMismatch {
                score_run_id,
                expected_source
            } if score_run_id == run_id && expected_source == SCORE_RUN_SOURCE_EXPERIMENT_RUN
        ),
        "expected contact scope mismatch, got {error:?}"
    );
}

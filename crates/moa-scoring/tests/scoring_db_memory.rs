//! Execution-lane tests for the score-query runtime path.
//!
//! moa-scoring previously had no `tests/` directory: every async query function
//! and the consecutive-row grouping helpers were unexecuted, pinned only by SQL
//! string-matches. These tests seed real `analytics.scores` /
//! `analytics.score_run` / `moa.experiment_trial` rows in an isolated test
//! schema and drive the production functions end to end, asserting aggregation,
//! cross-tenant filtering, NULL-delta handling, consecutive-row grouping (incl.
//! the documented non-contiguous split), and `ensure_score_run_parent`
//! idempotency + source-mismatch behavior.
//!
//! Rows for the shared `analytics`/`moa` schemas are isolated per test by a
//! unique tenant UUID (storage partition) and unique run UUIDs, matching the
//! sibling `moa-experiments` `_db` tests. The local Docker Postgres connects as
//! the table owner, which carries the `owner_dev_access` RLS policy, so seeds
//! and reads use the plain pool.

use moa_core::{ActionRuleScope, ContactId, TenantId};
use moa_scoring::{
    ExperimentRunScoreRef, SCORE_RUN_SOURCE_EVAL_REPLAY, SCORE_RUN_SOURCE_EXPERIMENT_RUN,
    ScoreCompareRef, ScoreRunRef, ScoringError, compare_score_runs_for_tenant,
    ensure_score_run_parent, experiment_score_breakdown_for_tenant,
    scenario_score_summaries_from_rows, score_summaries_for_tenant,
    trial_score_summaries_from_rows,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

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

async fn insert_score_run(pool: &PgPool, storage_partition_id: &str, run_id: Uuid, source: &str) {
    sqlx::query(
        r#"
        INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
        VALUES ($1, $2, NULL, $3)
        "#,
    )
    .bind(run_id)
    .bind(storage_partition_id)
    .bind(source)
    .execute(pool)
    .await
    .expect("insert analytics.score_run row");
}

async fn insert_experiment_run(
    pool: &PgPool,
    storage_partition_id: &str,
    run_uid: Uuid,
    score_run_id: Uuid,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.experiment_run (
            run_uid, storage_partition_id, user_id, name, target_kind, status,
            target, variant, score_run_id, created_by_identity
        )
        VALUES ($1, $2, NULL, 'breakdown-fixture', 'agent_loop', 'completed',
                '{}'::jsonb, '{}'::jsonb, $3, '{}'::jsonb)
        "#,
    )
    .bind(run_uid)
    .bind(storage_partition_id)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("insert moa.experiment_run row");
}

#[allow(clippy::too_many_arguments)]
async fn insert_experiment_trial(
    pool: &PgPool,
    storage_partition_id: &str,
    trial_uid: Uuid,
    run_uid: Uuid,
    trial_key: &str,
    variant_key: &str,
    scenario_id: &str,
    score_run_id: Uuid,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.experiment_trial (
            trial_uid, run_uid, storage_partition_id, user_id, trial_key, status,
            target_kind, variant_key, plan_revision_uid, scenario_id, simulator,
            simulator_model, score_run_id
        )
        VALUES ($1, $2, $3, NULL, $4, 'completed', 'agent_loop', $5, $6, $7,
                '{}'::jsonb, 'gpt-test', $8)
        "#,
    )
    .bind(trial_uid)
    .bind(run_uid)
    .bind(storage_partition_id)
    .bind(trial_key)
    .bind(variant_key)
    .bind(Uuid::now_v7())
    .bind(scenario_id)
    .bind(score_run_id)
    .execute(pool)
    .await
    .expect("insert moa.experiment_trial row");
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
    assert_eq!(grounded.value_type, "boolean");
    assert_eq!(grounded.n, 1);
    approx(grounded.mean_or_rate, 1.0);

    let quality = &summary.rows[1];
    assert_eq!(quality.name, "quality");
    assert_eq!(quality.value_type, "numeric");
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
async fn experiment_breakdown_groups_trials_and_scenarios_and_splits_noncontiguous_rows_db_memory()
{
    // Pins: the trial-aware breakdown groups one summary per trial and per
    // scenario over contiguous rows, and the grouping helpers split a single
    // trial/scenario into multiple groups when fed non-contiguous rows (the
    // documented ORDER-BY-contiguity dependency).
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap test db");
    let pool = test_db.store().pool();

    let tenant = TenantId::from(Uuid::now_v7());
    let sp = tenant.to_string();
    let run_uid = Uuid::now_v7();
    let run_score_run = Uuid::now_v7();
    insert_score_run(pool, &sp, run_score_run, "experiment_run").await;
    insert_experiment_run(pool, &sp, run_uid, run_score_run).await;

    let trial_a = Uuid::now_v7();
    let sr_a = Uuid::now_v7();
    insert_score_run(pool, &sp, sr_a, "experiment_trial").await;
    insert_experiment_trial(
        pool, &sp, trial_a, run_uid, "trial-a", "baseline", "scn-1", sr_a,
    )
    .await;
    insert_score(pool, &sp, sr_a, "accuracy", "numeric", Some(0.8), None).await;
    insert_score(pool, &sp, sr_a, "quality", "numeric", Some(0.6), None).await;

    let trial_b = Uuid::now_v7();
    let sr_b = Uuid::now_v7();
    insert_score_run(pool, &sp, sr_b, "experiment_trial").await;
    insert_experiment_trial(
        pool,
        &sp,
        trial_b,
        run_uid,
        "trial-b",
        "candidate",
        "scn-2",
        sr_b,
    )
    .await;
    insert_score(pool, &sp, sr_b, "accuracy", "numeric", Some(0.4), None).await;
    insert_score(pool, &sp, sr_b, "quality", "numeric", Some(0.9), None).await;

    let breakdown = experiment_score_breakdown_for_tenant(
        pool,
        ExperimentRunScoreRef {
            tenant_id: tenant,
            run_uid,
        },
    )
    .await
    .expect("experiment breakdown");

    // Per-trial grouping (ordered by variant_key: baseline before candidate).
    assert_eq!(breakdown.trials.len(), 2, "one summary group per trial");
    assert_eq!(breakdown.trials[0].trial_uid, trial_a);
    assert_eq!(breakdown.trials[0].variant_key, "baseline");
    assert_eq!(breakdown.trials[0].scenario_id.as_deref(), Some("scn-1"));
    assert_eq!(breakdown.trials[0].rows.len(), 2);
    approx(breakdown.trials[0].rows[0].mean_or_rate, 0.8); // accuracy
    approx(breakdown.trials[0].rows[1].mean_or_rate, 0.6); // quality
    assert_eq!(breakdown.trials[1].trial_uid, trial_b);

    // Per-scenario grouping.
    assert_eq!(
        breakdown.scenarios.len(),
        2,
        "one summary group per scenario"
    );
    assert_eq!(breakdown.scenarios[0].scenario_id.as_deref(), Some("scn-1"));
    assert_eq!(breakdown.scenarios[1].scenario_id.as_deref(), Some("scn-2"));

    // Rollup across both trial score runs.
    assert_eq!(breakdown.trial_rollup_rows.len(), 2);
    approx(breakdown.trial_rollup_rows[0].mean_or_rate, 0.6); // accuracy: (0.8 + 0.4)/2
    approx(breakdown.trial_rollup_rows[1].mean_or_rate, 0.75); // quality: (0.6 + 0.9)/2

    // Non-contiguous case: order rows by score name so each trial's rows are
    // interleaved. The helper relies on contiguity, so it splits each trial into
    // two single-row groups (4 groups for 2 trials).
    let scrambled_trial_rows = sqlx::query(
        r#"
        SELECT trial.trial_uid, trial.trial_key, trial.score_run_id, trial.variant_key,
               trial.scenario_id, score.name, score.value_type, COUNT(*)::BIGINT AS n,
               AVG(score.value_numeric) AS numeric_mean,
               AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION
                   AS boolean_rate
        FROM moa.experiment_trial AS trial
        JOIN analytics.scores AS score
          ON score.run_id = trial.score_run_id
         AND score.storage_partition_id = $2
        WHERE trial.run_uid = $1
          AND trial.storage_partition_id = $2
          AND trial.user_id IS NULL
        GROUP BY trial.trial_uid, trial.trial_key, trial.score_run_id, trial.variant_key,
                 trial.scenario_id, score.name, score.value_type
        ORDER BY score.name, trial.trial_key
        "#,
    )
    .bind(run_uid)
    .bind(&sp)
    .fetch_all(pool)
    .await
    .expect("scrambled trial rows");
    let split_trials =
        trial_score_summaries_from_rows(&scrambled_trial_rows).expect("group scrambled trials");
    assert_eq!(
        split_trials.len(),
        4,
        "non-contiguous trial rows split each trial into separate groups"
    );

    let scrambled_scenario_rows = sqlx::query(
        r#"
        SELECT trial.scenario_id, score.name, score.value_type, COUNT(*)::BIGINT AS n,
               AVG(score.value_numeric) AS numeric_mean,
               AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION
                   AS boolean_rate
        FROM moa.experiment_trial AS trial
        JOIN analytics.scores AS score
          ON score.run_id = trial.score_run_id
         AND score.storage_partition_id = $2
        WHERE trial.run_uid = $1
          AND trial.storage_partition_id = $2
          AND trial.user_id IS NULL
        GROUP BY trial.scenario_id, score.name, score.value_type
        ORDER BY score.name, trial.scenario_id
        "#,
    )
    .bind(run_uid)
    .bind(&sp)
    .fetch_all(pool)
    .await
    .expect("scrambled scenario rows");
    let split_scenarios = scenario_score_summaries_from_rows(&scrambled_scenario_rows)
        .expect("group scrambled scenarios");
    assert_eq!(
        split_scenarios.len(),
        4,
        "non-contiguous scenario rows split each scenario into separate groups"
    );
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

    ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_EVAL_REPLAY)
        .await
        .expect("first ensure inserts parent");
    ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_EVAL_REPLAY)
        .await
        .expect("second ensure is idempotent");

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.score_run WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("count parents");
    assert_eq!(count, 1, "idempotent ensure must not duplicate the parent");

    let error = ensure_score_run_parent(&mut conn, &scope, run_id, SCORE_RUN_SOURCE_EXPERIMENT_RUN)
        .await
        .expect_err("conflicting source must be rejected");
    assert!(
        matches!(
            error,
            ScoringError::ScoreRunMismatch { score_run_id, expected_source }
                if score_run_id == run_id && expected_source == SCORE_RUN_SOURCE_EXPERIMENT_RUN
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
            ScoringError::ScoreRunMismatch {
                score_run_id,
                expected_source
            } if score_run_id == run_id && expected_source == SCORE_RUN_SOURCE_EXPERIMENT_RUN
        ),
        "expected contact scope mismatch, got {error:?}"
    );
}

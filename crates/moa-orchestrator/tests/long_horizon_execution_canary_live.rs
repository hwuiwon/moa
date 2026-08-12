//! Opt-in, unbilled 24-hour and seven-day long-horizon invariant canaries.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_test_support::OrchestratorTestFixture;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

const SAMPLE_PERIOD: Duration = Duration::from_secs(60);
const RESTATE_KEY_BATCH_SIZE: usize = 250;

#[tokio::test]
#[ignore = "requires MOA_RUN_LONG_HORIZON_CANARY=1 and MOA_LONG_HORIZON_CANARY_WINDOW=24h"]
async fn deployed_long_horizon_invariants_hold_for_24_hours_live() -> Result<()> {
    // Pins: an explicitly selected external deployment is sampled for a full
    // 24 hours; an instantaneous healthy sample cannot satisfy this canary.
    if !canary_selected("24h")? {
        eprintln!(
            "SKIPPED long-horizon canary 24h: MOA_LONG_HORIZON_CANARY_WINDOW selects the other window"
        );
        return Ok(());
    }
    run_canary(Duration::from_secs(24 * 60 * 60)).await
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LONG_HORIZON_CANARY=1 and MOA_LONG_HORIZON_CANARY_WINDOW=7d"]
async fn deployed_long_horizon_invariants_hold_for_seven_days_live() -> Result<()> {
    // Pins: the seven-day deployment soak continuously rejects overdue runs,
    // parked compute ownership, and still-live attempt invocations.
    if !canary_selected("7d")? {
        eprintln!(
            "SKIPPED long-horizon canary 7d: MOA_LONG_HORIZON_CANARY_WINDOW selects the other window"
        );
        return Ok(());
    }
    run_canary(Duration::from_secs(7 * 24 * 60 * 60)).await
}

fn canary_selected(expected: &str) -> Result<bool> {
    if std::env::var("MOA_RUN_LONG_HORIZON_CANARY").as_deref() != Ok("1") {
        // Both cases are `#[ignore]`d, so reaching this point means the binary was
        // explicitly selected with `--run-ignored`. Returning `Ok(false)` here used to
        // report a green 24h/7d soak that sampled nothing, making an unauthorized sweep
        // indistinguishable from a real deployment canary in CI logs.
        bail!(
            "long-horizon canary was explicitly selected without MOA_RUN_LONG_HORIZON_CANARY=1; \
             refusing to report a passing soak that sampled nothing"
        );
    }
    let selected = std::env::var("MOA_LONG_HORIZON_CANARY_WINDOW").context(
        "MOA_RUN_LONG_HORIZON_CANARY=1 requires MOA_LONG_HORIZON_CANARY_WINDOW=24h or 7d",
    )?;
    if selected != "24h" && selected != "7d" {
        bail!("MOA_LONG_HORIZON_CANARY_WINDOW must be exactly 24h or 7d");
    }
    Ok(selected == expected)
}

async fn run_canary(window: Duration) -> Result<()> {
    if let Some((provider_flag, _)) = std::env::vars()
        .find(|(name, value)| name.starts_with("MOA_RUN_LIVE_") && value.trim() == "1")
    {
        bail!("long-horizon canary refuses live integration flag {provider_flag}=1");
    }
    for required in [
        "MOA_DATABASE_URL",
        "MOA_RESTATE_INGRESS_URL",
        "RESTATE_ADMIN_URL",
    ] {
        if std::env::var(required)
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            bail!("long-horizon canary requires external deployment variable {required}");
        }
    }
    // Resolve the fixture only after proving external discovery is configured;
    // otherwise `shared` could create an empty disposable stack and false-pass.
    let fixture = OrchestratorTestFixture::shared().await?;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let deadline = Instant::now() + window;
    loop {
        assert_deployment_invariants(&fixture, &pool).await?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        tokio::time::sleep(SAMPLE_PERIOD.min(deadline - now)).await;
    }
}

async fn assert_deployment_invariants(
    fixture: &OrchestratorTestFixture,
    pool: &PgPool,
) -> Result<()> {
    let has_overdue: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM moa.execution_run \
         WHERE status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled') \
           AND budget_deadline_at <= now())",
    )
    .fetch_one(pool)
    .await?;
    let has_parked_with_invalid_receipt_count: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ( \
           SELECT run.run_uid \
           FROM moa.execution_run AS run \
           LEFT JOIN moa.execution_capacity_reservation AS reservation \
             ON reservation.run_uid = run.run_uid \
            AND reservation.resource_dimension = 'parked_runs' \
            AND reservation.state <> 'released' \
           WHERE run.status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                                'waiting_timer', 'waiting_external', 'paused') \
           GROUP BY run.run_uid HAVING COUNT(reservation.reservation_uid) <> 1 \
         ) AS invalid_parked_receipts)",
    )
    .fetch_one(pool)
    .await?;
    let has_parked_with_attempt_capacity: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM moa.execution_run AS run \
         JOIN moa.execution_capacity_reservation AS reservation \
           ON reservation.run_uid = run.run_uid \
         WHERE run.status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                              'waiting_timer', 'waiting_external', 'paused') \
           AND reservation.resource_dimension = 'active_tasks' \
           AND reservation.state <> 'released')",
    )
    .fetch_one(pool)
    .await?;
    let has_parked_with_active_hands: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM moa.execution_run AS run \
         JOIN moa.sandbox_workspaces AS workspace \
           ON workspace.tenant_id = run.tenant_id \
          AND workspace.scope_kind = 'execution_task' \
          AND workspace.scope_run_id = run.run_uid \
         JOIN moa.sandbox_capacity_reservations AS reservation \
           ON reservation.tenant_id = workspace.tenant_id \
          AND reservation.workspace_id = workspace.workspace_id \
         WHERE run.status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                              'waiting_timer', 'waiting_external', 'paused') \
           AND reservation.resource_dimension = 'active_hands' \
           AND reservation.reservation_state <> 'released')",
    )
    .fetch_one(pool)
    .await?;
    let has_parked_with_active_dispatch: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM moa.execution_run AS run \
         JOIN moa.execution_task AS task ON task.run_uid = run.run_uid \
         WHERE run.status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                              'waiting_timer', 'waiting_external', 'paused') \
           AND (task.active_dispatch_uid IS NOT NULL \
                OR task.attempt_state IN ('dispatching', 'running', 'cancelling')))",
    )
    .fetch_one(pool)
    .await?;
    assert_no_live_parked_attempts(fixture, pool).await?;
    assert_no_live_parked_controllers(fixture, pool).await?;
    assert!(!has_overdue, "nonterminal execution runs exceeded deadline");
    assert!(
        !has_parked_with_invalid_receipt_count,
        "parked execution runs did not own exactly one ParkedRuns receipt"
    );
    assert!(
        !has_parked_with_attempt_capacity,
        "parked execution runs retained active task capacity"
    );
    assert!(
        !has_parked_with_active_hands,
        "parked execution runs retained active sandbox hands"
    );
    assert!(
        !has_parked_with_active_dispatch,
        "parked execution runs retained active task dispatch state"
    );
    Ok(())
}

async fn assert_no_live_parked_attempts(
    fixture: &OrchestratorTestFixture,
    pool: &PgPool,
) -> Result<()> {
    let mut cursor: Option<Uuid> = None;
    loop {
        let keys: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT dispatch.dispatch_uid \
             FROM moa.execution_dispatch_outbox AS dispatch \
             JOIN moa.execution_run AS run ON run.run_uid = dispatch.run_uid \
             WHERE run.status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                                  'waiting_timer', 'waiting_external', 'paused') \
               AND dispatch.dispatch_kind IN ('task_attempt', 'compensation_attempt') \
               AND ($1::UUID IS NULL OR dispatch.dispatch_uid > $1) \
             ORDER BY dispatch.dispatch_uid LIMIT $2",
        )
        .bind(cursor)
        .bind(i64::try_from(RESTATE_KEY_BATCH_SIZE)?)
        .fetch_all(pool)
        .await?;
        if keys.is_empty() {
            return Ok(());
        }
        assert_no_live_restate_invocations(
            fixture,
            &keys,
            "target_service_name IN ('ExecutionTaskAttempt', 'ExecutionCompensationAttempt')",
            "parked attempt",
        )
        .await?;
        cursor = keys.last().copied();
    }
}

async fn assert_no_live_parked_controllers(
    fixture: &OrchestratorTestFixture,
    pool: &PgPool,
) -> Result<()> {
    let mut cursor: Option<Uuid> = None;
    loop {
        let keys: Vec<Uuid> = sqlx::query_scalar(
            "SELECT run_uid FROM moa.execution_run \
             WHERE status IN ('waiting_input', 'waiting_review', 'waiting_signal', \
                              'waiting_timer', 'waiting_external', 'paused') \
               AND ($1::UUID IS NULL OR run_uid > $1) \
             ORDER BY run_uid LIMIT $2",
        )
        .bind(cursor)
        .bind(i64::try_from(RESTATE_KEY_BATCH_SIZE)?)
        .fetch_all(pool)
        .await?;
        if keys.is_empty() {
            return Ok(());
        }
        assert_no_live_restate_invocations(
            fixture,
            &keys,
            "target_service_name = 'ExecutionRunController'",
            "parked run controller",
        )
        .await?;
        cursor = keys.last().copied();
    }
}

async fn assert_no_live_restate_invocations(
    fixture: &OrchestratorTestFixture,
    keys: &[Uuid],
    service_predicate: &str,
    owner_kind: &str,
) -> Result<()> {
    let quoted_keys = keys
        .iter()
        .map(|key| format!("'{key}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!(
        "SELECT id, target_service_name, target_service_key, status \
         FROM sys_invocation WHERE {service_predicate} \
           AND target_service_key IN ({quoted_keys}) \
           AND status NOT IN ('completed', 'killed') LIMIT 1"
    );
    let response = reqwest::Client::new()
        .post(format!("{}/query", fixture.admin_url.trim_end_matches('/')))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({"query": query}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let rows = response
        .get("rows")
        .and_then(Value::as_array)
        .context("Restate canary query omitted rows")?;
    if !rows.is_empty() {
        bail!("{owner_kind} retained a nonterminal Restate invocation: {rows:?}");
    }
    Ok(())
}

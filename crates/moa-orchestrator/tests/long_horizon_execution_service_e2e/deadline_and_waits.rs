//! Deadline, absolute target, and wait-expiry coverage.

use super::*;
use moa_artifacts::execution_plan::ExecutionTaskResult;
use moa_execution::state::ExecutionTerminalReason;
use moa_execution::wire::{
    ExecutionConflictReason, ExecutionMutationResponse, ExecutionSignalRequest,
    ExecutionTaskAttemptRequest,
};

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn run_deadline_terminalizes_slow_active_attempt_and_releases_capacity_service_e2e()
-> Result<()> {
    // Pins: the absolute admitted run deadline wins over a slow provider slice,
    // terminalizes once with DeadlineExceeded, and releases the active-attempt reservation.
    let completed = serde_json::to_string(&json!({"result": "too late"}))?;
    let fixture = execution_fixture_with_script(
        json!({
            "default": {
                "content": completed,
                "tool_calls": [],
                "latency_ms": 10_000,
                "ttft_ms": 10_000
            }
        }),
        Vec::new(),
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "run-deadline",
        vec![
            node(
                "slow-agent",
                &[],
                ExecutionOperation::Agent {
                    instructions: "Return after the provider delay.".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 1,
                },
                json!({"type": "object"}),
            ),
            output_node(&["slow-agent"], json!({"unexpected": true})),
        ],
        Duration::from_secs(3),
    )
    .await?;
    await_task_status(&test, &run, "slow-agent", ExecutionTaskStatus::Running).await?;
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Failed).await?;
    assert!(terminal.output.is_none());
    assert_eq!(terminal.run.failed_tasks, 0);
    assert_eq!(
        terminal.run.terminal_reason,
        Some(ExecutionTerminalReason::DeadlineExceeded)
    );
    let slow_agent = tasks(&test, &run)
        .await?
        .into_iter()
        .find(|task| task.node_id == "slow-agent")
        .context("deadline run omitted its slow agent task")?;
    // The deadline fence makes the run result authoritative before cancellation delivery. A
    // provider result already in flight may still win the task-level settlement race and is
    // retained as terminal evidence; neither disposition may revive or complete the run.
    assert!(matches!(
        (
            slow_agent.status,
            slow_agent.outcome.as_ref().map(|outcome| &outcome.result)
        ),
        (
            ExecutionTaskStatus::Completed,
            Some(ExecutionTaskResult::Completed { .. })
        ) | (
            ExecutionTaskStatus::Cancelled,
            Some(ExecutionTaskResult::Cancelled { .. })
        )
    ));
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active, 0,
        "deadline terminalization leaked attempt capacity"
    );
    let deadline_boundary: (String, String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.run_uid = $1 AND trigger.trigger_kind = 'run_deadline'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        deadline_boundary,
        (
            "superseded".to_string(),
            "cancelled".to_string(),
            "released".to_string(),
        )
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn exact_wait_expiry_fails_once_and_late_delivery_cannot_revive_run_service_e2e() -> Result<()>
{
    // Pins: one storage-only signal expiry uses its persisted absolute deadline,
    // fails the run once, and leaves no active attempt capacity while parked.
    let fixture = execution_fixture(Vec::new()).await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "wait-expiry",
        vec![
            node(
                "expiring-signal",
                &[],
                ExecutionOperation::WaitSignal {
                    signal_name: "never-arrives".to_string(),
                    wait_policy: ExecutionWaitPolicy {
                        expiry: after_logical_days(2),
                        on_expiry: ExecutionWaitExpiryAction::FailTask,
                    },
                },
                json!({"type": "object"}),
            ),
            output_node(&["expiring-signal"], json!({"unexpected": true})),
        ],
        Duration::from_secs(10),
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingSignal).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "expiring-signal",
        ExecutionTaskStatus::WaitingSignal,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    let rows = persisted_trigger_rows(&pool, run.run_uid).await?;
    let expiry = rows
        .iter()
        .find(|(kind, _, _)| kind == "wait_expiry")
        .context("signal wait did not persist its expiry trigger")?;
    let waiting_since: DateTime<Utc> = sqlx::query_scalar(
        "SELECT waiting_since FROM moa.execution_task WHERE run_uid = $1 AND task_id = $2",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        expiry.1.signed_duration_since(waiting_since),
        TimeDelta::from_std(LOGICAL_DAY * 2)?,
        "persisted expiry did not preserve the exact compressed two-day wait"
    );

    let failed = await_run_status(&test, &run, ExecutionRunStatus::Failed).await?;
    assert!(failed.output.is_none());
    let failed_at = failed
        .run
        .completed_at
        .context("failed run omitted terminal time")?;
    tokio::time::sleep(LOGICAL_DAY).await;
    let replay = status(&test, &run).await?;
    assert_eq!(replay.run.status, ExecutionRunStatus::Failed);
    assert_eq!(replay.run.completed_at, Some(failed_at));
    assert_eq!(replay.run.failed_tasks, 1);
    let late: ExecutionMutationResponse = test
        .client()
        .post_call(
            "/Execution/deliver_signal",
            &ExecutionSignalRequest {
                tenant_id: run.tenant_id,
                contact_id: None,
                run_uid: run.run_uid,
                task_id: task_id(&waiting),
                expected_generation: waiting.generation,
                signal_name: "never-arrives".to_string(),
                payload: json!({"late": true}),
            },
        )
        .await?;
    assert_eq!(
        late,
        ExecutionMutationResponse::Conflict {
            reason: ExecutionConflictReason::AlreadyTerminal,
        }
    );
    let immutable = status(&test, &run).await?;
    assert_eq!(immutable.run.status, ExecutionRunStatus::Failed);
    assert_eq!(immutable.run.completed_at, Some(failed_at));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for real watchdog delivery and held MCP effects"]
async fn watchdog_retries_idempotent_but_never_resends_ambiguous_effect_service_e2e() -> Result<()>
{
    // Pins: the same durable watchdog boundary retries a catalog-idempotent
    // effect under attempt generation two, but terminalizes a possibly-applied
    // non-idempotent effect as UnknownOutcome without a second logical send.
    let idempotent_tool = "long_horizon_watchdog_idempotent";
    let ambiguous_tool = "long_horizon_watchdog_ambiguous";
    let fixture = execution_fixture_with_tools(
        vec![
            FixtureCapabilityTool {
                name: idempotent_tool.to_string(),
                description: "Task 12 idempotent watchdog barrier".to_string(),
                input_schema: watchdog_input_schema(),
                item_key_pointer: None,
                idempotent: true,
                outcomes: vec![FixtureCapabilityOutcome::Success {
                    output: json!({"watchdog": "retried"}),
                }],
            },
            FixtureCapabilityTool {
                name: ambiguous_tool.to_string(),
                description: "Task 12 ambiguous watchdog effect".to_string(),
                input_schema: watchdog_input_schema(),
                item_key_pointer: None,
                idempotent: false,
                outcomes: vec![FixtureCapabilityOutcome::ApplyThenDisconnect],
            },
        ],
        vec![
            (
                "MOA_EXECUTION_ACTIVE_ATTEMPT_TIMEOUT_SECONDS".to_string(),
                "2".to_string(),
            ),
            // Staleness must stay strictly below the attempt timeout, so a scenario that
            // shortens the timeout has to shorten this with it or the orchestrator refuses
            // to boot.
            (
                "MOA_EXECUTION_ATTEMPT_HEARTBEAT_STALENESS_SECONDS".to_string(),
                "1".to_string(),
            ),
            (
                "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
                "1".to_string(),
            ),
        ],
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let controller = fixture
        .fixture_capability()
        .context("watchdog fixture omitted capability controller")?;

    let idempotent = start_plan(
        &test,
        "watchdog-idempotent",
        vec![
            fixture_capability_node(
                "watchdog-idempotent",
                idempotent_tool,
                json!({"case": "retry-safe"}),
            ),
            output_node(&["watchdog-idempotent"], json!({"watchdog": "safe"})),
        ],
        Duration::from_secs(20),
    )
    .await?;
    controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    let first_request = await_attempt_request(&pool, idempotent.run_uid, 1).await?;
    let second = controller.wait_for_calls(2, SCENARIO_TIMEOUT).await?;
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].capability, idempotent_tool);
    assert_eq!(second[1].capability, idempotent_tool);
    assert_ne!(second[0].invocation_id, second[1].invocation_id);
    let retrying = await_task_attempt(
        &test,
        &idempotent,
        "watchdog-idempotent",
        2,
        ExecutionTaskStatus::Running,
    )
    .await?;
    assert_eq!(retrying.attempt, 2);
    controller.release(2);
    let safe_terminal = await_run_status(&test, &idempotent, ExecutionRunStatus::Completed).await?;
    assert_eq!(safe_terminal.output, Some(json!({"watchdog": "safe"})));
    let safe_task = await_task_status(
        &test,
        &idempotent,
        "watchdog-idempotent",
        ExecutionTaskStatus::Completed,
    )
    .await?;
    assert_eq!(safe_task.attempt, 2);
    assert_eq!(controller.effect_count(), 2);
    assert_watchdog_settled_once(&pool, &first_request).await?;

    let ambiguous = start_plan(
        &test,
        "watchdog-ambiguous",
        vec![
            fixture_capability_node(
                "watchdog-ambiguous",
                ambiguous_tool,
                json!({"case": "possible-commit"}),
            ),
            output_node(
                &["watchdog-ambiguous"],
                json!({"unexpected": "ambiguous resend"}),
            ),
        ],
        Duration::from_secs(20),
    )
    .await?;
    controller.wait_for_calls(3, SCENARIO_TIMEOUT).await?;
    let ambiguous_request = await_attempt_request(&pool, ambiguous.run_uid, 1).await?;
    controller.release(1);
    let ambiguous_task = await_task_status(
        &test,
        &ambiguous,
        "watchdog-ambiguous",
        ExecutionTaskStatus::UnknownOutcome,
    )
    .await?;
    assert_eq!(ambiguous_task.attempt, 1);
    assert!(matches!(
        ambiguous_task.outcome.as_ref().map(|outcome| &outcome.result),
        Some(ExecutionTaskResult::UnknownOutcome { message })
            if message.contains("possible commit")
    ));
    let ambiguous_terminal =
        await_run_status(&test, &ambiguous, ExecutionRunStatus::Failed).await?;
    assert!(ambiguous_terminal.output.is_none());
    assert_eq!(ambiguous_terminal.run.failed_tasks, 0);
    let effect_count = controller.effect_count();
    assert_eq!(effect_count, 3, "ambiguous effect was logically sent twice");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        controller.effect_count(),
        effect_count,
        "watchdog replay resent an ambiguous effect"
    );
    assert_watchdog_settled_once(&pool, &ambiguous_request).await?;
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND task_id = $2 AND resource_dimension = 'active_tasks' \
           AND state <> 'released'",
    )
    .bind(ambiguous.run_uid)
    .bind(ambiguous_request.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(active, 0, "ambiguous watchdog retained active capacity");
    Ok(())
}

fn watchdog_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["case"],
        "properties": {"case": {"type": "string"}}
    })
}

async fn await_attempt_request(
    pool: &PgPool,
    run_uid: Uuid,
    attempt_generation: u64,
) -> Result<ExecutionTaskAttemptRequest> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let payload: Option<Value> = sqlx::query_scalar(
            "SELECT payload FROM moa.execution_dispatch_outbox \
             WHERE run_uid = $1 AND dispatch_kind = 'task_attempt' \
               AND attempt_generation = $2",
        )
        .bind(run_uid)
        .bind(i64::try_from(attempt_generation)?)
        .fetch_optional(pool)
        .await?;
        if let Some(payload) = payload {
            return serde_json::from_value(payload)
                .context("decode immutable task-attempt dispatch");
        }
        if Instant::now() >= deadline {
            bail!("run {run_uid} did not persist attempt generation {attempt_generation}")
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_task_attempt(
    test: &IsolatedTest<'_>,
    run: &StartedRun,
    node_id: &str,
    expected_attempt: u32,
    expected_status: ExecutionTaskStatus,
) -> Result<ExecutionTaskProjection> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let current = tasks(test, run).await?;
        if let Some(task) = current.iter().find(|task| {
            task.node_id == node_id
                && task.attempt == expected_attempt
                && task.status == expected_status
        }) {
            return Ok(task.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "node `{node_id}` did not reach attempt {expected_attempt} {expected_status:?}; tasks={current:?}"
            )
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn assert_watchdog_settled_once(
    pool: &PgPool,
    request: &ExecutionTaskAttemptRequest,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT trigger.state, dispatch.state AS dispatch_state, \
                (SELECT COUNT(*) FROM moa.execution_trigger AS duplicate \
                 WHERE duplicate.run_uid = trigger.run_uid \
                   AND duplicate.task_id = trigger.task_id \
                   AND duplicate.trigger_kind = 'task_watchdog' \
                   AND duplicate.attempt_generation = trigger.attempt_generation) AS exact_count \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid = trigger.trigger_uid \
          AND dispatch.dispatch_kind = 'trigger_delivery' \
         WHERE trigger.trigger_uid = $1",
    )
    .bind(request.watchdog_trigger_uid)
    .fetch_one(pool)
    .await?;
    assert_eq!(row.try_get::<String, _>("state")?, "superseded");
    assert_eq!(row.try_get::<String, _>("dispatch_state")?, "cancelled");
    assert_eq!(row.try_get::<i64, _>("exact_count")?, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn retry_backoff_releases_attempt_capacity_and_redispatches_new_generation_service_e2e()
-> Result<()> {
    // Pins: a retryable provider failure persists a future retry, releases the
    // first attempt's capacity during backoff, and dispatches generation two once.
    let tool_name = "long_horizon_retry_probe";
    let fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 retry barrier".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["case"],
                "properties": {"case": {"type": "string"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![
                FixtureCapabilityOutcome::HttpFailure {
                    status: 429,
                    retry_after_ms: Some(100),
                    message: "fixture rate limit".to_string(),
                },
                FixtureCapabilityOutcome::Success {
                    output: json!({"result": "retried"}),
                },
            ],
        }],
        Vec::new(),
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let mut retry_node = fixture_capability_node(
        "retry-capability",
        tool_name,
        json!({"case": "long-horizon-retry"}),
    );
    retry_node.retry = RetryPolicy {
        max_attempts: 2,
        initial_backoff_ms: 1_000,
        max_backoff_ms: 1_000,
    };
    let run = start_plan(
        &test,
        "retry-backoff",
        vec![
            retry_node,
            output_node(&["retry-capability"], json!({"retry": "complete"})),
        ],
        Duration::from_secs(12),
    )
    .await?;

    let controller = fixture
        .fixture_capability()
        .context("retry fixture omitted capability controller")?;
    let first = controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].capability, tool_name);
    controller.release(1);

    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let row = sqlx::query(
            "SELECT attempt, attempt_generation, ready_at, active_dispatch_uid, attempt_state \
             FROM moa.execution_task WHERE run_uid = $1 AND node_id = 'retry-capability'",
        )
        .bind(run.run_uid)
        .fetch_optional(&pool)
        .await?;
        if let Some(row) = row
            && let Some(ready_at) = row.try_get::<Option<DateTime<Utc>>, _>("ready_at")?
        {
            assert_eq!(row.try_get::<i32, _>("attempt")?, 2);
            assert_eq!(row.try_get::<i64, _>("attempt_generation")?, 2);
            assert_eq!(row.try_get::<Option<Uuid>, _>("active_dispatch_uid")?, None);
            assert_eq!(row.try_get::<String, _>("attempt_state")?, "idle");
            let active: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
                 WHERE run_uid = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
            )
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
            assert_eq!(active, 0, "retry backoff retained active attempt capacity");
            let database_now: DateTime<Utc> =
                sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
            assert!(
                ready_at > database_now,
                "retry backoff was not persisted in the future: ready={ready_at}, now={database_now}"
            );
            assert_eq!(
                controller.calls().len(),
                1,
                "retry generation two dispatched before its persisted ready_at"
            );
            break;
        }
        if Instant::now() >= deadline {
            bail!("retry-agent never persisted its storage-only backoff");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let second = controller.wait_for_calls(2, SCENARIO_TIMEOUT).await?;
    assert_eq!(second[0].input, second[1].input);
    assert_ne!(second[0].invocation_id, second[1].invocation_id);
    controller.release(1);

    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"retry": "complete"})));
    let task = await_task_status(
        &test,
        &run,
        "retry-capability",
        ExecutionTaskStatus::Completed,
    )
    .await?;
    assert_eq!(task.attempt, 2);
    assert_eq!(task.generation, 2);
    Ok(())
}

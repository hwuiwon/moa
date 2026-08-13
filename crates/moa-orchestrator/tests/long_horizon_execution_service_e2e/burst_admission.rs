//! Burst admission, parked-capacity, and deterministic ordering coverage.

use super::*;
use futures_util::{StreamExt, TryStreamExt, stream};
use moa_core::traits::{Identity, IdentityType};

#[tokio::test]
#[ignore = "requires Docker and a bounded three-minute common-wake admission window"]
async fn one_thousand_common_wakes_bound_capacity_invocations_and_oldest_ready_age_service_e2e()
-> Result<()> {
    // Pins: 1,000 independently admitted runs wake on one absolute instant; the production
    // dispatcher drains timer delivery and run activation as one bounded chain rather than an
    // unkeyed kick storm, never exceeds 32 DB receipts or Restate attempts, and exports its
    // bounded batch plus checked oldest-ready age through OTLP.
    const RUN_COUNT: usize = 1_000;
    const FLEET_CAP: usize = 32;
    const ADMISSION_CONCURRENCY: usize = 8;
    // Seconds between admission and the shared absolute wake, and the margin by
    // which every run must already be parked before that wake arrives.
    const PRE_WAKE_SECONDS: i64 = 180;
    const PRE_WAKE_MARGIN_SECONDS: i64 = 30;
    // Absolute execution deadline handed to each admitted run. It has to outlast
    // the pre-wake window plus the whole fleet-capped drain, so it is
    // deliberately far larger than any phase bound below: a run that expires
    // here is a product deadline defect, not a slow observation.
    const RUN_DEADLINE: Duration = Duration::from_secs(900);
    // Bound on any single observation phase. Each phase reports its own
    // diagnostic well inside the lane's per-case ceiling rather than being
    // terminated by nextest with no attribution.
    const PHASE_TIMEOUT: Duration = Duration::from_secs(90);
    // Total bound on the ~32-wave drain. Bounding the whole drain instead of
    // each wave keeps the worst case additive rather than multiplicative.
    const DRAIN_BUDGET: Duration = Duration::from_secs(240);
    let tool_name = "long_horizon_thousand_wake_probe";
    let fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 thousand-wake barrier".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["index"],
                "properties": {"index": {"type": "integer"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::Success {
                output: json!({"completed": true}),
            }],
        }],
        vec![
            (
                "MOA_EXECUTION_MAX_TENANT_ACTIVE_RUNS".to_string(),
                RUN_COUNT.to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_FLEET_ACTIVE_RUNS".to_string(),
                RUN_COUNT.to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_TENANT_PARKED_RUNS".to_string(),
                RUN_COUNT.to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_FLEET_PARKED_RUNS".to_string(),
                RUN_COUNT.to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_TENANT_ACTIVE_TASKS".to_string(),
                FLEET_CAP.to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_FLEET_ACTIVE_TASKS".to_string(),
                FLEET_CAP.to_string(),
            ),
        ],
    )
    .await?;
    let tenant_id = fixture
        .client
        .identity()
        .context("thousand-wake fixture omitted identity")?
        .tenant_id;
    fixture.grant_default_tenant_admin(tenant_id).await?;
    let capability_name = moa_hands::mcp_tool_reference("fixture-capability", tool_name);
    allow_fixture_capability(&fixture, tenant_id, &capability_name, "thousand-wake").await?;
    let test = fixture.isolated().await;
    let common_wake = Utc::now() + TimeDelta::seconds(PRE_WAKE_SECONDS);
    let runs = stream::iter(0..RUN_COUNT)
        .map(|index| {
            let test = &test;
            async move {
                let mut capability =
                    fixture_capability_node("burst-capability", tool_name, json!({"index": index}));
                capability.depends_on = vec!["burst-timer".to_string()];
                start_plan_with_policy(
                    test,
                    &format!("thousand-wake-{index}"),
                    vec![
                        node(
                            "burst-timer",
                            &[],
                            ExecutionOperation::WaitUntil {
                                wake: ExecutionTemporalTarget::At { at: common_wake },
                                result: json!({"ready": true}),
                            },
                            json!({"type": "object"}),
                        ),
                        capability,
                        output_node(&["burst-capability"], json!({"completed": true})),
                    ],
                    RUN_DEADLINE,
                    false,
                )
                .await
            }
        })
        // Admission locks the shared tenant capacity buckets. Keep that setup traffic below
        // Restate's suspension threshold; this scenario stresses the common wake, not admission.
        .buffer_unordered(ADMISSION_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(runs.len(), RUN_COUNT);
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    await_tenant_run_count(&pool, tenant_id, "waiting_timer", RUN_COUNT, PHASE_TIMEOUT).await?;
    await_capacity_quantity_before(
        &pool,
        tenant_id,
        "parked_runs",
        RUN_COUNT,
        common_wake - TimeDelta::seconds(PRE_WAKE_MARGIN_SECONDS),
    )
    .await?;
    let admission_margin = common_wake.signed_duration_since(Utc::now());
    assert!(
        admission_margin > TimeDelta::seconds(PRE_WAKE_MARGIN_SECONDS),
        "1,000 runs were not fully parked before the shared wake: {admission_margin:?}"
    );
    fixture.otlp_capture()?.clear().await;

    let controller = fixture
        .fixture_capability()
        .context("thousand-wake fixture omitted capability controller")?;
    // `PHASE_TIMEOUT` bounds the first drain wave, not the wait for the timer that
    // releases it. The runs are parked on an absolute wake that is still in the future
    // here — by the assertion above, at least `PRE_WAKE_MARGIN_SECONDS` of it remains,
    // and after fast admission far more than that. Starting a 90s observation now
    // measures the pre-wake window instead of the drain and fails before the wake can
    // fire, and it fails *sooner* the faster admission was. Wait out the remaining
    // pre-wake window first, then bound the wave itself.
    let until_wake = common_wake
        .signed_duration_since(Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO);
    tokio::time::sleep(until_wake).await;
    controller.wait_for_calls(FLEET_CAP, PHASE_TIMEOUT).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(controller.calls().len(), FLEET_CAP);
    let active: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(quantity), 0)::BIGINT FROM moa.execution_capacity_reservation \
         WHERE tenant_id = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await?;
    assert_eq!(active, FLEET_CAP as i64);
    let invocations = restate_rows(
        &fixture,
        "SELECT id FROM sys_invocation WHERE target_service_name = 'ExecutionTaskAttempt' \
         AND status NOT IN ('completed', 'killed')",
    )
    .await?;
    assert!(invocations.len() <= FLEET_CAP);
    let dispatch_metric = fixture
        .otlp_capture()?
        .wait_for_metric(PHASE_TIMEOUT, |metric| {
            metric.name() == "moa_execution_dispatch_batch_size"
                && metric.data_points().iter().any(|point| {
                    point.count() > 0
                        && point.value() >= FLEET_CAP as f64
                        && point.value() / point.count() as f64 <= FLEET_CAP as f64
                })
        })
        .await
        .context("observe bounded production execution-dispatch batch metric")?;
    assert!(dispatch_metric.data_points().iter().any(|point| {
        point.count() > 0
            && point.value() >= FLEET_CAP as f64
            && point.value() / point.count() as f64 <= FLEET_CAP as f64
    }));
    let oldest_ready_metric = fixture
        .otlp_capture()?
        .wait_for_metric(PHASE_TIMEOUT, |metric| {
            metric.name() == "moa_execution_oldest_ready_age_seconds"
                && metric
                    .data_points()
                    .iter()
                    .any(|point| point.value() > 0.0 && point.value() <= 60.0)
        })
        .await
        .context("observe checked production oldest-ready-age metric")?;
    assert!(
        oldest_ready_metric
            .data_points()
            .iter()
            .any(|point| point.value() > 0.0 && point.value() <= 60.0)
    );

    let mut released = 0;
    let mut maximum_oldest_ready_seconds = 0.0_f64;
    let drain_deadline = Instant::now() + DRAIN_BUDGET;
    while released < RUN_COUNT {
        let next = (released + FLEET_CAP).min(RUN_COUNT);
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "fleet-capped drain released {released}/{RUN_COUNT} runs within {DRAIN_BUDGET:?}"
            );
        }
        controller.wait_for_calls(next, remaining).await?;
        let wave_active: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(quantity), 0)::BIGINT FROM moa.execution_capacity_reservation \
             WHERE tenant_id = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        assert!(wave_active <= FLEET_CAP as i64);
        let oldest: f64 = sqlx::query_scalar(
            "SELECT COALESCE(EXTRACT(EPOCH FROM (now() - MIN(ready_at))), 0)::DOUBLE PRECISION \
             FROM moa.execution_task WHERE tenant_id = $1 AND status = 'ready'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await?;
        maximum_oldest_ready_seconds = maximum_oldest_ready_seconds.max(oldest);
        controller.release(next - released);
        released = next;
    }
    assert!(
        maximum_oldest_ready_seconds <= 60.0,
        "oldest ready task exceeded bounded age: {maximum_oldest_ready_seconds}s"
    );
    // The terminal settle is the tail of the same fleet-capped drain the loop above
    // budgets `DRAIN_BUDGET` for: every one of `RUN_COUNT` runs still has to finish
    // through a `FLEET_CAP` slot. `PHASE_TIMEOUT` bounds a single observation, so
    // applying it here measures one phase against work that is `RUN_COUNT / FLEET_CAP`
    // waves deep and fails partway through steady progress rather than on a stall.
    await_tenant_run_count(&pool, tenant_id, "completed", RUN_COUNT, DRAIN_BUDGET).await?;
    let first = status(&test, &runs[0]).await?;
    let last = status(&test, &runs[RUN_COUNT - 1]).await?;
    assert_eq!(first.output, Some(json!({"completed": true})));
    assert_eq!(last.output, Some(json!({"completed": true})));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn timer_burst_obeys_parked_cap_and_preserves_fifo_due_order_service_e2e() -> Result<()> {
    // Pins: a same-tenant burst converts active attempts into bounded parked
    // ownership without exceeding the configured resident-run entitlement;
    // cap+1 admission is rejected until one entitlement is released, and due
    // work retains FIFO order after the rejected admission is retried.
    let fixture = execution_fixture(vec![
        (
            "MOA_EXECUTION_MAX_TENANT_ACTIVE_RUNS".to_string(),
            "4".to_string(),
        ),
        (
            "MOA_EXECUTION_MAX_FLEET_ACTIVE_RUNS".to_string(),
            "4".to_string(),
        ),
        (
            "MOA_EXECUTION_MAX_TENANT_PARKED_RUNS".to_string(),
            "4".to_string(),
        ),
        (
            "MOA_EXECUTION_MAX_FLEET_PARKED_RUNS".to_string(),
            "4".to_string(),
        ),
        (
            "MOA_EXECUTION_MAX_TENANT_ACTIVE_TASKS".to_string(),
            "2".to_string(),
        ),
        (
            "MOA_EXECUTION_MAX_FLEET_ACTIVE_TASKS".to_string(),
            "2".to_string(),
        ),
    ])
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let mut runs = Vec::new();
    for index in 0..4_u64 {
        runs.push(
            start_plan(
                &test,
                &format!("burst-{index}"),
                vec![
                    node(
                        "burst-timer",
                        &[],
                        ExecutionOperation::WaitUntil {
                            wake: after_logical_days(3 + index),
                            result: json!({"index": index}),
                        },
                        json!({"type": "object"}),
                    ),
                    output_node(&["burst-timer"], json!({"burst": index})),
                ],
                Duration::from_secs(15),
            )
            .await?,
        );
    }
    let overflow_admission = start_plan(
        &test,
        "burst-overflow-rejected",
        vec![
            node(
                "overflow-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: after_logical_days(7),
                    result: json!({"overflow": true}),
                },
                json!({"type": "object"}),
            ),
            output_node(&["overflow-timer"], json!({"overflow": "completed"})),
        ],
        Duration::from_secs(30),
    )
    .await;
    let overflow_error = match overflow_admission {
        Ok(_) => bail!("cap+1 run was admitted without resident capacity"),
        Err(error) => error,
    };
    let overflow_error = format!("{overflow_error:#}");
    assert!(
        overflow_error
            .contains("execution parked_runs capacity is exhausted; retry admission later"),
        "cap+1 admission returned the wrong error: {overflow_error}"
    );

    for run in &runs {
        await_run_status(&test, run, ExecutionRunStatus::WaitingTimer).await?;
        assert_parked_has_no_active_compute(&fixture, &pool, run).await?;
    }
    let active_task_capacity: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
         WHERE tenant_id = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
    )
    .bind(runs[0].tenant_id.0)
    .fetch_one(&pool)
    .await?;
    let parked_capacity: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(quantity), 0)::BIGINT FROM moa.execution_capacity_reservation \
         WHERE tenant_id = $1 AND resource_dimension = 'parked_runs' AND state <> 'released'",
    )
    .bind(runs[0].tenant_id.0)
    .fetch_one(&pool)
    .await?;
    let admitted_runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_run WHERE tenant_id = $1")
            .bind(runs[0].tenant_id.0)
            .fetch_one(&pool)
            .await?;
    assert_eq!(active_task_capacity, 0);
    assert_eq!(parked_capacity, 4);
    assert_eq!(admitted_runs, 4);

    let first_terminal = await_run_status(&test, &runs[0], ExecutionRunStatus::Completed).await?;
    assert_eq!(first_terminal.output, Some(json!({"burst": 0})));
    let overflow = start_plan(
        &test,
        "burst-overflow-retry",
        vec![
            node(
                "overflow-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: after_logical_days(7),
                    result: json!({"overflow": true}),
                },
                json!({"type": "object"}),
            ),
            output_node(&["overflow-timer"], json!({"overflow": "completed"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    await_run_status(&test, &overflow, ExecutionRunStatus::WaitingTimer).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &overflow).await?;

    for (index, run) in runs.iter().enumerate().skip(1) {
        let terminal = await_run_status(&test, run, ExecutionRunStatus::Completed).await?;
        assert_eq!(terminal.output, Some(json!({"burst": index as u64})));
    }
    let overflow_terminal =
        await_run_status(&test, &overflow, ExecutionRunStatus::Completed).await?;
    assert_eq!(
        overflow_terminal.output,
        Some(json!({"overflow": "completed"}))
    );
    let mut delivered_at = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        let row = sqlx::query(
            "SELECT due_at, created_at, delivered_at FROM moa.execution_trigger \
             WHERE run_uid = $1 AND trigger_kind = 'task_timer'",
        )
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
        let due_at: DateTime<Utc> = row.try_get("due_at")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        delivered_at.push(
            row.try_get::<Option<DateTime<Utc>>, _>("delivered_at")?
                .with_context(|| format!("burst-{index} timer omitted delivered_at"))?,
        );
        let expected = TimeDelta::from_std(LOGICAL_DAY * (3 + index as u32))?;
        let error = due_at.signed_duration_since(created_at) - expected;
        assert!(
            error.num_milliseconds().unsigned_abs() <= 250,
            "burst-{index} persisted the wrong relative due time: {error:?}"
        );
    }
    assert!(
        delivered_at.windows(2).all(|pair| pair[0] < pair[1]),
        "timer delivery did not preserve increasing due order: {delivered_at:?}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn two_tenant_burst_admits_one_attempt_each_before_second_wave_service_e2e() -> Result<()> {
    // Pins: fleet capacity two and tenant capacity one produce one concurrent
    // attempt per tenant; one tenant cannot consume both slots ahead of its peer.
    let tool_name = "long_horizon_fairness_probe";
    let mut fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 fairness barrier".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["case"],
                "properties": {"case": {"type": "string"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::Success {
                output: json!({"result": "fair"}),
            }],
        }],
        vec![
            (
                "MOA_EXECUTION_MAX_TENANT_ACTIVE_TASKS".to_string(),
                "1".to_string(),
            ),
            (
                "MOA_EXECUTION_MAX_FLEET_ACTIVE_TASKS".to_string(),
                "2".to_string(),
            ),
        ],
    )
    .await?;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let identity_a = fixture
        .client
        .identity()
        .cloned()
        .context("fixture client omitted tenant A identity")?;
    let tenant_a = identity_a.tenant_id;
    let common_wake = Utc::now() + TimeDelta::seconds(30);
    let test_a = fixture.isolated().await;
    let runs_a = vec![
        start_slow_fair_run(&test_a, tool_name, "tenant-a-1", common_wake).await?,
        start_slow_fair_run(&test_a, tool_name, "tenant-a-2", common_wake).await?,
    ];
    drop(test_a);

    let identity_b = Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id: TenantId::from(Uuid::now_v7()),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let tenant_b = identity_b.tenant_id;
    fixture.client = fixture.client.clone().with_identity(identity_b.clone());
    let test_b = fixture.isolated().await;
    let runs_b = vec![
        start_slow_fair_run(&test_b, tool_name, "tenant-b-1", common_wake).await?,
        start_slow_fair_run(&test_b, tool_name, "tenant-b-2", common_wake).await?,
    ];

    let controller = fixture
        .fixture_capability()
        .context("fairness fixture omitted capability controller")?;
    let first_wave = controller.wait_for_calls(2, SCENARIO_TIMEOUT).await?;
    assert_eq!(first_wave.len(), 2);
    let first_cases = first_wave
        .iter()
        .filter_map(|call| call.input.get("case").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(first_cases.iter().any(|case| case.starts_with("tenant-a")));
    assert!(first_cases.iter().any(|case| case.starts_with("tenant-b")));

    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let rows = sqlx::query(
            "SELECT tenant_id, SUM(quantity)::BIGINT AS quantity \
             FROM moa.execution_capacity_reservation \
             WHERE resource_dimension = 'active_tasks' AND state <> 'released' \
             GROUP BY tenant_id ORDER BY tenant_id",
        )
        .fetch_all(&pool)
        .await?;
        let observed = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<Uuid, _>("tenant_id")?,
                    row.try_get::<i64, _>("quantity")?,
                ))
            })
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?;
        if observed.len() == 2 {
            assert_eq!(
                observed.iter().map(|(_, quantity)| *quantity).sum::<i64>(),
                2
            );
            assert!(observed.iter().all(|(_, quantity)| *quantity == 1));
            assert!(observed.iter().any(|(tenant, _)| *tenant == tenant_a.0));
            assert!(observed.iter().any(|(tenant, _)| *tenant == tenant_b.0));
            break;
        }
        if Instant::now() >= deadline {
            bail!("two-tenant burst never admitted one active attempt per tenant: {observed:?}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        controller.calls().len(),
        2,
        "fleet cap admitted a third attempt while the first wave was held"
    );
    let active_invocations = restate_rows(
        &fixture,
        "SELECT id FROM sys_invocation \
         WHERE target_service_name = 'ExecutionTaskAttempt' \
           AND status NOT IN ('completed', 'killed')",
    )
    .await?;
    assert_eq!(active_invocations.len(), 2);
    controller.release(2);
    let second_wave = controller.wait_for_calls(4, SCENARIO_TIMEOUT).await?;
    assert_eq!(second_wave.len(), 4);
    let second_cases = second_wave[2..]
        .iter()
        .filter_map(|call| call.input.get("case").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(second_cases.iter().any(|case| case.starts_with("tenant-a")));
    assert!(second_cases.iter().any(|case| case.starts_with("tenant-b")));
    controller.release(2);

    for run in &runs_b {
        let terminal = await_run_status(&test_b, run, ExecutionRunStatus::Completed).await?;
        assert_eq!(terminal.output, Some(json!({"fair": true})));
    }
    drop(test_b);
    fixture.client = fixture.client.clone().with_identity(identity_a);
    let test_a = fixture.isolated().await;
    for run in &runs_a {
        let terminal = await_run_status(&test_a, run, ExecutionRunStatus::Completed).await?;
        assert_eq!(terminal.output, Some(json!({"fair": true})));
    }
    Ok(())
}

async fn start_slow_fair_run(
    test: &IsolatedTest<'_>,
    tool_name: &str,
    label: &str,
    common_wake: DateTime<Utc>,
) -> Result<StartedRun> {
    let mut capability =
        fixture_capability_node("fair-capability", tool_name, json!({"case": label}));
    capability.depends_on = vec!["fair-timer".to_string()];
    start_plan(
        test,
        label,
        vec![
            node(
                "fair-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: ExecutionTemporalTarget::At { at: common_wake },
                    result: json!({"ready": true}),
                },
                json!({"type": "object"}),
            ),
            capability,
            output_node(&["fair-capability"], json!({"fair": true})),
        ],
        Duration::from_secs(60),
    )
    .await
}

async fn await_tenant_run_count(
    pool: &PgPool,
    tenant_id: TenantId,
    status: &str,
    expected: usize,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_run WHERE tenant_id = $1 AND status = $2",
        )
        .bind(tenant_id.0)
        .bind(status)
        .fetch_one(pool)
        .await?;
        if count == expected as i64 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "tenant {} reached {count}/{expected} runs in status {status} within {timeout:?}",
                tenant_id.0
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_capacity_quantity_before(
    pool: &PgPool,
    tenant_id: TenantId,
    dimension: &str,
    expected: usize,
    deadline: DateTime<Utc>,
) -> Result<()> {
    loop {
        let quantity: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(quantity), 0)::BIGINT \
             FROM moa.execution_capacity_reservation \
             WHERE tenant_id = $1 AND resource_dimension = $2 AND state <> 'released'",
        )
        .bind(tenant_id.0)
        .bind(dimension)
        .fetch_one(pool)
        .await?;
        if quantity == expected as i64 {
            return Ok(());
        }
        if Utc::now() >= deadline {
            bail!(
                "tenant {} reached {quantity}/{expected} active {dimension} capacity before {deadline}",
                tenant_id.0
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

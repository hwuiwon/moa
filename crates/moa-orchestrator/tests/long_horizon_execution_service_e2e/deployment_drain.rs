//! Real Restate handler revision registration, latest routing, and drain coverage.

use super::*;

#[tokio::test]
#[ignore = "requires Docker for three real orchestrator handler deployments"]
async fn three_handler_revisions_route_latest_then_drain_from_one_to_zero_service_e2e() -> Result<()>
{
    // Pins: each newest real deployment owns one held bounded attempt, then the
    // old deployment drains from one pinned invocation to zero and is stopped.
    let tool_name = "long_horizon_revision_probe";
    let fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 deployment barrier".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["revision"],
                "properties": {"revision": {"type": "integer"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::Success {
                output: json!({"drained": true}),
            }],
        }],
        Vec::new(),
    )
    .await?;
    let first = fixture.current_handler_revision().await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let controller = fixture
        .fixture_capability()
        .context("deployment fixture omitted capability controller")?;

    let first_run = start_revision_run(&test, tool_name, 1).await?;
    controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    let first_dispatch = active_dispatch_uid(&pool, first_run.run_uid).await?;
    assert_dispatch_pinned_to(&fixture, first_dispatch, &first).await?;
    assert!(fixture.handler_revision_pinned_invocations(&first).await? >= 1);

    let second = fixture
        .start_handler_revision("long-horizon-revision-2")
        .await?;
    assert_ne!(first.deployment_id, second.deployment_id);
    assert_ne!(first.deployment_uri, second.deployment_uri);
    controller.release(1);
    await_run_status(&test, &first_run, ExecutionRunStatus::Completed).await?;
    fixture
        .wait_for_handler_revision_drained(&first, Duration::from_secs(10))
        .await?;
    fixture.stop_drained_handler_revision(&first).await?;

    let second_run = start_revision_run(&test, tool_name, 2).await?;
    controller.wait_for_calls(2, SCENARIO_TIMEOUT).await?;
    let second_dispatch = active_dispatch_uid(&pool, second_run.run_uid).await?;
    assert_dispatch_pinned_to(&fixture, second_dispatch, &second).await?;
    assert!(fixture.handler_revision_pinned_invocations(&second).await? >= 1);

    let third = fixture
        .start_handler_revision("long-horizon-revision-3")
        .await?;
    assert_ne!(second.deployment_id, third.deployment_id);
    assert_ne!(second.deployment_uri, third.deployment_uri);
    controller.release(1);
    await_run_status(&test, &second_run, ExecutionRunStatus::Completed).await?;
    fixture
        .wait_for_handler_revision_drained(&second, Duration::from_secs(10))
        .await?;
    fixture.stop_drained_handler_revision(&second).await?;

    let third_run = start_revision_run(&test, tool_name, 3).await?;
    controller.wait_for_calls(3, SCENARIO_TIMEOUT).await?;
    let third_dispatch = active_dispatch_uid(&pool, third_run.run_uid).await?;
    assert_dispatch_pinned_to(&fixture, third_dispatch, &third).await?;
    assert!(fixture.handler_revision_pinned_invocations(&third).await? >= 1);
    controller.release(1);
    let terminal = await_run_status(&test, &third_run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"revision": 3})));
    fixture
        .wait_for_handler_revision_drained(&third, Duration::from_secs(10))
        .await?;
    fixture.stop_drained_handler_revision(&third).await?;
    Ok(())
}

async fn start_revision_run(
    test: &IsolatedTest<'_>,
    tool_name: &str,
    revision: u64,
) -> Result<StartedRun> {
    start_plan(
        test,
        &format!("deployment-revision-{revision}"),
        vec![
            fixture_capability_node(
                "revision-capability",
                tool_name,
                json!({"revision": revision}),
            ),
            output_node(&["revision-capability"], json!({"revision": revision})),
        ],
        Duration::from_secs(30),
    )
    .await
}

async fn active_dispatch_uid(pool: &PgPool, run_uid: Uuid) -> Result<Uuid> {
    sqlx::query_scalar(
        "SELECT active_dispatch_uid FROM moa.execution_task \
         WHERE run_uid = $1 AND node_id = 'revision-capability'",
    )
    .bind(run_uid)
    .fetch_one(pool)
    .await
    .context("revision capability omitted active dispatch UID")
}

async fn assert_dispatch_pinned_to(
    fixture: &OrchestratorTestFixture,
    dispatch_uid: Uuid,
    revision: &moa_test_support::FixtureHandlerRevision,
) -> Result<()> {
    let rows = restate_rows(
        fixture,
        &format!(
            "SELECT pinned_deployment_id, last_attempt_deployment_id, status \
             FROM sys_invocation WHERE target_service_name = 'ExecutionTaskAttempt' \
             AND target_service_key = '{dispatch_uid}'"
        ),
    )
    .await?;
    let row = rows
        .first()
        .with_context(|| format!("Restate omitted task-attempt invocation {dispatch_uid}"))?;
    let pinned = row
        .get("pinned_deployment_id")
        .and_then(Value::as_str)
        .or_else(|| {
            row.get("last_attempt_deployment_id")
                .and_then(Value::as_str)
        })
        .context("Restate invocation omitted deployment identity")?;
    assert_eq!(pinned, revision.deployment_id);
    Ok(())
}

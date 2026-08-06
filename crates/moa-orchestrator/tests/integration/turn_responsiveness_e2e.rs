//! End-to-end turn responsiveness coverage through the scripted orchestrator fixture.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::{
    events::Event, events::EventType, types::action_policy::ActionPolicyEffect,
    types::contact::SessionActorRef, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SessionId, types::identifiers::TenantId,
    types::provider::ModelTier, types::session::SessionMeta, types::session::SessionStatus,
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::fixtures::fresh_client_message_id;
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    OrchestratorTestFixture, TestApiClient,
};
use moa_wire::turn::{
    SessionProgress, SessionProgressRequest, StartTurnRequest, TurnOutcome, TurnOutcomeKind,
    TurnPhase,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const CLARIFICATION_RESPONSE: &str =
    "I need the following information before I can continue:\n\n- target\n- specific change";
const RECOVERY_GATE_TOOL_NAME: &str = "recovery_matrix_gate";
const RECOVERY_GATE_SERVER_NAME: &str = "fixture-capability";
const ROUTE_CLASSIFIER_MATCH: &str =
    "You classify one user turn into MOA's public execution decision.";
const PROGRESS_MESSAGE: &str = "please answer with the scripted progress response";
const RECOVERY_MATRIX_TIMEOUT: Duration = Duration::from_secs(90);
const RECOVERY_MATRIX_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn execution_routing_writes_normalized_audits_outside_session_progress_service_e2e()
-> Result<()> {
    // Pins: the real Session -> TurnExecution -> Execution service path distinguishes Respond,
    // Execute/Inline, and Execute/Durable, durably audits normalized routing, and returns Accepted
    // with one run UID without placing planner internals in Session/progress events.
    let run_objective = "Start an execution run for a durable report";
    let fixture = OrchestratorTestFixture::with_script(json!({
        "responses": [
            classifier_response("respond", None, "The request only needs a direct response."),
            { "completion": { "content": "4" } },
            classifier_response(
                "execute",
                Some("inline"),
                "The work fits a bounded interactive loop.",
            ),
            { "completion": { "content": "inspection complete" } },
            classifier_response(
                "execute",
                Some("durable"),
                "The requested report should persist as a durable execution.",
            ),
            { "completion": { "content": execution_candidate(run_objective) } }
        ],
        "keyed": [{
            "match": "Synthesize the final user response for execution run",
            "completion": { "content": "durable report complete" }
        }],
        "default": { "completion": { "content": "unexpected scripted fallback" } }
    }))
    .await?;
    let test = fixture.isolated().await;

    let respond_session = test.create_session("execution-route-respond").await?;
    let (_, respond_outcome, respond_events) =
        run_scripted_turn(&fixture.client, respond_session, "What is 2+2?").await?;
    assert_eq!(
        respond_outcome.kind,
        TurnOutcomeKind::Completed,
        "respond outcome: {}; events: {}",
        respond_outcome.message,
        event_summary(&respond_events)
    );
    assert_eq!(
        route_decisions_and_strategies(&fixture.postgres_url, respond_session).await?,
        vec![("respond".to_string(), None)]
    );

    let inline_session = test.create_session("execution-route-inline").await?;
    let (_, inline_outcome, inline_events) = run_scripted_turn(
        &fixture.client,
        inline_session,
        "Inspect the repository and explain the result.",
    )
    .await?;
    assert_eq!(
        inline_outcome.kind,
        TurnOutcomeKind::Completed,
        "Inline outcome: {}; events: {}",
        inline_outcome.message,
        event_summary(&inline_events)
    );
    assert_eq!(
        route_decisions_and_strategies(&fixture.postgres_url, inline_session).await?,
        vec![("execute".to_string(), Some("inline".to_string()))]
    );

    let durable_session = test.create_session("execution-route-durable").await?;
    let (_, durable_outcome, durable_events) =
        run_scripted_turn(&fixture.client, durable_session, run_objective).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = durable_outcome.kind else {
        panic!(
            "explicit Durable execution must return Accepted, got {:?}: {}; events: {}",
            durable_outcome.kind,
            durable_outcome.message,
            event_summary(&durable_events)
        );
    };
    assert_eq!(
        route_decisions_and_strategies(&fixture.postgres_url, durable_session).await?,
        vec![("execute".to_string(), Some("durable".to_string()))]
    );
    assert!(durable_events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ExecutionRunStarted(started) if started.run_uid == execution_run_uid
        )
    }));
    assert_eq!(
        planner_outcomes(&fixture.postgres_url, durable_session).await?,
        vec!["accepted"]
    );
    assert_eq!(
        compile_outcomes(&fixture.postgres_url, durable_session).await?,
        vec!["accepted"]
    );
    assert_eq!(
        fixture
            .client
            .session(durable_session.to_string())
            .status()
            .await?,
        SessionStatus::Running
    );

    let snapshot_end = durable_events
        .last()
        .expect("durable turn event snapshot should contain at least one event")
        .sequence_num;
    let progress: SessionProgress = fixture
        .client
        .post_call(
            &format!("/Session/{durable_session}/progress"),
            &SessionProgressRequest {
                event_range: EventRange {
                    to_seq: Some(snapshot_end),
                    ..EventRange::all()
                },
            },
        )
        .await?;
    assert_eq!(progress.events, durable_events);
    Ok(())
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn turn_responsiveness_vague_fix_this_clarifies_before_main_loop() -> Result<()> {
    // Pins: an underspecified "fix this" turn persists UserMessage, emits clarification, and skips expensive work.
    let fixture = OrchestratorTestFixture::with_script(main_loop_should_not_run_script()).await?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("turn-responsiveness-clarification")
        .await?;

    let (turn_id, outcome, events) = run_scripted_turn(&fixture.client, session_id, "fix this")
        .await
        .context("run vague clarification turn")?;

    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.turn_id, turn_id);
    assert_eq!(outcome.message, CLARIFICATION_RESPONSE);
    assert_eq!(
        user_messages(&events),
        vec!["fix this".to_string()],
        "clarification path must still persist the admitted user message: {}",
        event_summary(&events)
    );
    assert_eq!(
        brain_responses(&events),
        vec![CLARIFICATION_RESPONSE.to_string()],
        "clarification path should append exactly the deterministic response: {}",
        event_summary(&events)
    );
    assert_eq!(
        tool_call_count(&events),
        0,
        "clarification path must not dispatch scripted tools: {}",
        event_summary(&events)
    );
    assert!(
        !event_summary(&events).contains("MAIN LOOP SHOULD NOT RUN"),
        "clarification path must not consume scripted model output: {}",
        event_summary(&events)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn public_session_progress_projects_transient_updates_without_persisting_them() -> Result<()>
{
    // Pins: Session/progress reads the ingress-private TurnExecution projection service-to-service,
    // while ProgressUpdate remains transient workflow state rather than durable event-log history.
    let fixture = OrchestratorTestFixture::with_script_and_env(
        progress_script(),
        vec![
            (
                "MOA_SESSION_LIMITS_PROGRESS_FIRST_DELAY_MS".to_string(),
                "0".to_string(),
            ),
            (
                "MOA_SESSION_LIMITS_PROGRESS_INTERVAL_MS".to_string(),
                "0".to_string(),
            ),
        ],
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("turn-progress-projection").await?;

    let started = start_recovery_matrix_turn(&fixture.client, session_id, PROGRESS_MESSAGE)
        .await
        .context("start progress-emitting turn")?;
    let turn_id = started
        .turn_id
        .context("progress-emitting turn should start immediately")?;
    let live_progress =
        await_recovery_matrix_session_progress(&fixture.client, session_id, |progress| {
            progress.snapshot.active_turn_id.as_deref() == Some(turn_id.as_str())
                && progress.active_turn_progress.as_ref().is_some_and(|turn| {
                    turn.turn_id == turn_id
                        && turn.last_progress_summary.as_deref() == Some("Calling the model")
                })
        })
        .await
        .context("public Session/progress should expose the active workflow projection")?;
    let projected = live_progress
        .active_turn_progress
        .context("active turn projection should be present")?;
    assert_eq!(projected.turn_id, turn_id);
    assert_eq!(projected.phase, TurnPhase::Streaming);
    assert_eq!(
        progress_updates(&live_progress.events, &turn_id),
        Vec::<(String, String)>::new(),
        "public progress should not expose transient updates as durable rows: {}",
        event_summary(&live_progress.events)
    );

    let outcome = fixture
        .client
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            RECOVERY_MATRIX_TIMEOUT,
            RECOVERY_MATRIX_POLL_INTERVAL,
        )
        .await
        .context("await progress-emitting turn outcome")?;
    let events = fixture
        .client
        .get_events(session_id, EventRange::all())
        .await
        .context("read completed progress-emitting turn events")?;

    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(
        progress_updates(&events, &turn_id),
        Vec::<(String, String)>::new(),
        "initial event read should not include durable ProgressUpdate rows: {}",
        event_summary(&events)
    );

    let fresh_client =
        TestApiClient::new(&fixture.ingress_url)?.with_identity(default_fixture_identity());
    let replayed = fresh_client
        .get_events(
            session_id,
            EventRange {
                event_types: Some(vec![EventType::ProgressUpdate]),
                ..EventRange::all()
            },
        )
        .await
        .context("fresh event-log read should query progress update rows")?;
    assert_eq!(
        progress_updates(&replayed, &turn_id),
        Vec::<(String, String)>::new(),
        "fresh event read should not replay transient progress as durable rows: {}",
        event_summary(&replayed)
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Valkey service fixture"]
async fn recovery_matrix_same_session_messages_stay_fifo_while_another_session_progresses_service_e2e()
-> Result<()> {
    // Pins: an active Session VO records later messages in FIFO order without holding an
    // unrelated Session key behind the first turn's blocked upstream effect.
    const FIRST_MESSAGE: &str = "recovery FIFO first message";
    const SECOND_MESSAGE: &str = "recovery FIFO second message";
    const DISTINCT_MESSAGE: &str = "recovery FIFO distinct-session message";
    const FIRST_RESULT: &str = "recovery FIFO first result";
    const SECOND_RESULT: &str = "recovery FIFO second result";
    const DISTINCT_RESULT: &str = "recovery FIFO distinct result";

    let fixture = OrchestratorTestFixture::with_execution_fixture(
        recovery_matrix_script(
            FIRST_MESSAGE,
            SECOND_MESSAGE,
            DISTINCT_MESSAGE,
            FIRST_RESULT,
            SECOND_RESULT,
            DISTINCT_RESULT,
        ),
        FixtureCapabilityOptions {
            tools: vec![recovery_gate_tool()],
            orchestrator_env: vec![
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_FLEET_LIMIT".to_string(),
                    "2".to_string(),
                ),
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_TENANT_LIMIT".to_string(),
                    "2".to_string(),
                ),
            ],
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let fifo_session = test.create_session("recovery-fifo-primary").await?;
    let distinct_session = test.create_session("recovery-fifo-distinct").await?;
    let tenant_id = test.client().get_session(fifo_session).await?.tenant_id;
    seed_recovery_gate_allow_policy(&fixture, test.client(), tenant_id).await?;

    let first = start_recovery_matrix_turn(test.client(), fifo_session, FIRST_MESSAGE).await?;
    let first_turn_id = first
        .turn_id
        .context("first FIFO message should start immediately")?;
    let controller = fixture
        .fixture_capability()
        .context("recovery fixture omitted capability controller")?;
    let blocked_calls = controller
        .wait_for_calls(1, RECOVERY_MATRIX_TIMEOUT)
        .await
        .context("first FIFO turn should reach the exact capability barrier")?;
    assert_eq!(blocked_calls.len(), 1);
    assert_eq!(blocked_calls[0].capability, RECOVERY_GATE_TOOL_NAME);
    assert_eq!(blocked_calls[0].input, json!({"label": FIRST_MESSAGE}));

    let queued = start_recovery_matrix_turn(test.client(), fifo_session, SECOND_MESSAGE).await?;
    assert!(
        queued.queued,
        "the second same-session message must be queued"
    );
    assert_eq!(
        queued.turn_id, None,
        "queued messages do not mint a turn yet"
    );

    let blocked_events = test
        .client()
        .get_events(fifo_session, EventRange::all())
        .await?;
    assert_eq!(user_messages(&blocked_events), vec![FIRST_MESSAGE]);
    assert_eq!(queued_messages(&blocked_events), vec![SECOND_MESSAGE]);
    assert_eq!(brain_responses(&blocked_events), Vec::<String>::new());

    let (distinct_turn_id, distinct_outcome, distinct_events) =
        run_scripted_turn(test.client(), distinct_session, DISTINCT_MESSAGE)
            .await
            .context("distinct Session key should complete while the FIFO head is blocked")?;
    assert_eq!(distinct_outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(distinct_outcome.turn_id, distinct_turn_id);
    assert_eq!(distinct_outcome.message, DISTINCT_RESULT);
    assert_eq!(user_messages(&distinct_events), vec![DISTINCT_MESSAGE]);
    assert_eq!(brain_responses(&distinct_events), vec![DISTINCT_RESULT]);

    let still_blocked = test
        .client()
        .session(fifo_session.to_string())
        .snapshot()
        .await?;
    assert_eq!(
        still_blocked.active_turn_id.as_deref(),
        Some(first_turn_id.as_str())
    );
    assert_eq!(still_blocked.pending_message_count, 1);
    controller.release(1);

    let settled = await_recovery_matrix_session_progress(test.client(), fifo_session, |progress| {
        progress.snapshot.active_turn_id.is_none()
            && progress.snapshot.pending_message_count == 0
            && progress
                .snapshot
                .last_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.message == SECOND_RESULT)
    })
    .await?;
    assert_eq!(
        user_messages(&settled.events),
        vec![FIRST_MESSAGE, SECOND_MESSAGE]
    );
    assert_eq!(queued_messages(&settled.events), vec![SECOND_MESSAGE]);
    assert_eq!(
        brain_responses(&settled.events),
        vec![FIRST_RESULT, SECOND_RESULT]
    );
    assert_eq!(controller.effect_count(), 1);
    assert_eq!(controller.request_count(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Valkey service fixture"]
async fn recovery_matrix_tenant_admission_isolates_tenants_and_keeps_progress_responsive_service_e2e()
-> Result<()> {
    // Pins: one tenant's saturated Valkey admission lease delays only that tenant's new
    // Session turn; another tenant still runs and SharedObjectContext progress stays callable.
    const TENANT_A_BLOCKING: &str = "recovery tenant A blocking message";
    const TENANT_A_WAITING: &str = "recovery tenant A waiting message";
    const TENANT_B_MESSAGE: &str = "recovery tenant B independent message";
    const TENANT_A_BLOCKING_RESULT: &str = "recovery tenant A blocking result";
    const TENANT_A_WAITING_RESULT: &str = "recovery tenant A waiting result";
    const TENANT_B_RESULT: &str = "recovery tenant B independent result";

    let fixture = OrchestratorTestFixture::with_execution_fixture(
        recovery_matrix_script(
            TENANT_A_BLOCKING,
            TENANT_A_WAITING,
            TENANT_B_MESSAGE,
            TENANT_A_BLOCKING_RESULT,
            TENANT_A_WAITING_RESULT,
            TENANT_B_RESULT,
        ),
        FixtureCapabilityOptions {
            tools: vec![recovery_gate_tool()],
            orchestrator_env: vec![
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_FLEET_LIMIT".to_string(),
                    "2".to_string(),
                ),
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_TENANT_LIMIT".to_string(),
                    "1".to_string(),
                ),
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_LEASE_TTL_MS".to_string(),
                    "60000".to_string(),
                ),
                (
                    "MOA_SESSION_LIMITS_TURN_ADMISSION_RETRY_AFTER_MS".to_string(),
                    "50".to_string(),
                ),
            ],
        },
    )
    .await?;
    let template_test = fixture.isolated().await;
    let template_session = template_test.create_session("tenant-template").await?;
    let template = template_test.client().get_session(template_session).await?;

    let tenant_a_identity = recovery_matrix_identity(TenantId::new());
    let tenant_b_identity = recovery_matrix_identity(TenantId::new());
    let tenant_a_client =
        TestApiClient::new(&fixture.ingress_url)?.with_identity(tenant_a_identity.clone());
    let tenant_b_client =
        TestApiClient::new(&fixture.ingress_url)?.with_identity(tenant_b_identity.clone());
    let tenant_a_blocking_session = create_recovery_matrix_tenant_session(
        &fixture,
        &tenant_a_client,
        &tenant_a_identity,
        &template,
        "tenant-a-blocking",
    )
    .await?;
    let tenant_a_waiting_session = create_recovery_matrix_tenant_session(
        &fixture,
        &tenant_a_client,
        &tenant_a_identity,
        &template,
        "tenant-a-waiting",
    )
    .await?;
    let tenant_b_session = create_recovery_matrix_tenant_session(
        &fixture,
        &tenant_b_client,
        &tenant_b_identity,
        &template,
        "tenant-b-independent",
    )
    .await?;
    seed_recovery_gate_allow_policy(&fixture, &tenant_a_client, tenant_a_identity.tenant_id)
        .await?;
    seed_recovery_gate_allow_policy(&fixture, &tenant_b_client, tenant_b_identity.tenant_id)
        .await?;

    let blocking = start_recovery_matrix_turn(
        &tenant_a_client,
        tenant_a_blocking_session,
        TENANT_A_BLOCKING,
    )
    .await?;
    let blocking_turn_id = blocking
        .turn_id
        .context("tenant A's first session should acquire its tenant lease")?;
    let controller = fixture
        .fixture_capability()
        .context("recovery fixture omitted capability controller")?;
    controller
        .wait_for_calls(1, RECOVERY_MATRIX_TIMEOUT)
        .await
        .context("tenant A's first session should reach the capability barrier")?;

    let active_tool_invocations = await_restate_rows(
        &fixture,
        "SELECT id, status FROM sys_invocation \
         WHERE target_service_name = 'ToolExecutor' AND status != 'completed'",
        RECOVERY_MATRIX_TIMEOUT,
        |rows| !rows.is_empty(),
    )
    .await
    .context("observe the held turn's active ToolExecutor child")?;
    assert_eq!(
        active_tool_invocations.len(),
        1,
        "the exact capability barrier should hold one ToolExecutor child: {active_tool_invocations:?}"
    );

    let user_limits = restate_rows(&fixture, "SELECT * FROM sys_user_limits").await?;
    let virtual_queues = restate_rows(&fixture, "SELECT * FROM sys_vqueues").await?;
    assert_eq!(
        user_limits,
        Vec::<Value>::new(),
        "Restate user limits must stay inactive while Valkey owns the held turn: {user_limits:?}"
    );
    assert_eq!(
        virtual_queues,
        Vec::<Value>::new(),
        "Restate virtual queues must stay inactive while Valkey owns the held turn: {virtual_queues:?}"
    );
    fixture.otlp_capture()?.clear().await;

    let waiting_client = tenant_a_client.clone();
    let waiting_start = tokio::spawn(async move {
        start_recovery_matrix_turn(&waiting_client, tenant_a_waiting_session, TENANT_A_WAITING)
            .await
    });
    fixture
        .otlp_capture()?
        .wait_for_metric(RECOVERY_MATRIX_TIMEOUT, |metric| {
            metric.name() == "moa_turn_admission_decisions_total"
                && metric.data_points().iter().any(|point| {
                    point.attribute("scope") == Some("tenant")
                        && point.attribute("outcome") == Some("waiting")
                        && point.value() >= 1.0
                })
        })
        .await
        .context("observe tenant-scoped admission wait after exact Valkey rejection")?;
    assert!(
        !waiting_start.is_finished(),
        "tenant A's second Session start must remain blocked at admission"
    );

    let waiting_progress: SessionProgress = tokio::time::timeout(
        Duration::from_secs(5),
        tenant_a_client.post_call(
            &format!("/Session/{tenant_a_waiting_session}/progress"),
            &SessionProgressRequest::default(),
        ),
    )
    .await
    .context("Session/progress should not queue behind the blocked exclusive handler")??;
    assert_eq!(
        waiting_progress.snapshot.session_id,
        tenant_a_waiting_session.to_string()
    );
    assert_eq!(waiting_progress.snapshot.active_turn_id, None);
    assert_eq!(waiting_progress.snapshot.pending_message_count, 0);
    assert_eq!(
        user_messages(&waiting_progress.events),
        Vec::<String>::new()
    );

    let (tenant_b_turn_id, tenant_b_outcome, tenant_b_events) =
        run_scripted_turn(&tenant_b_client, tenant_b_session, TENANT_B_MESSAGE)
            .await
            .context("tenant B should use the remaining fleet slot independently")?;
    assert_eq!(tenant_b_outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(tenant_b_outcome.turn_id, tenant_b_turn_id);
    assert_eq!(tenant_b_outcome.message, TENANT_B_RESULT);
    assert_eq!(user_messages(&tenant_b_events), vec![TENANT_B_MESSAGE]);
    assert_eq!(brain_responses(&tenant_b_events), vec![TENANT_B_RESULT]);
    assert!(
        !waiting_start.is_finished(),
        "tenant B completing must not bypass tenant A's saturated lease"
    );

    let blocking_snapshot = tenant_a_client
        .session(tenant_a_blocking_session.to_string())
        .snapshot()
        .await?;
    assert_eq!(
        blocking_snapshot.active_turn_id.as_deref(),
        Some(blocking_turn_id.as_str())
    );
    controller.release(1);

    let waiting_started = tokio::time::timeout(RECOVERY_MATRIX_TIMEOUT, waiting_start)
        .await
        .context("tenant A's waiting start should resume after lease release")?
        .context("join tenant A waiting start task")??;
    let waiting_turn_id = waiting_started
        .turn_id
        .context("tenant A's waiting Session should start after capacity returns")?;
    assert!(!waiting_started.queued);
    let waiting_outcome = tenant_a_client
        .session(tenant_a_waiting_session.to_string())
        .await_turn_outcome(
            &waiting_turn_id,
            RECOVERY_MATRIX_TIMEOUT,
            RECOVERY_MATRIX_POLL_INTERVAL,
        )
        .await?;
    assert_eq!(waiting_outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(waiting_outcome.message, TENANT_A_WAITING_RESULT);
    let blocking_progress = await_recovery_matrix_session_progress(
        &tenant_a_client,
        tenant_a_blocking_session,
        |progress| {
            progress.snapshot.active_turn_id.is_none()
                && progress
                    .snapshot
                    .last_outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.message == TENANT_A_BLOCKING_RESULT)
        },
    )
    .await?;
    assert_eq!(
        brain_responses(&blocking_progress.events),
        vec![TENANT_A_BLOCKING_RESULT]
    );
    assert_eq!(controller.effect_count(), 1);
    Ok(())
}

fn recovery_matrix_script(
    blocking_message: &str,
    waiting_message: &str,
    distinct_message: &str,
    blocking_result: &str,
    waiting_result: &str,
    distinct_result: &str,
) -> serde_json::Value {
    json!({
        "default": {
            "completion": {"content": "unexpected recovery-matrix scripted fallback"}
        },
        "keyed": [
            {
                "match": ROUTE_CLASSIFIER_MATCH,
                "completion": {
                    "content": json!({
                        "label": "execute",
                        "strategy": "inline",
                        "rationale": "The request fits a bounded deterministic recovery scenario.",
                        "confidence_bps": 10_000,
                        "missing_inputs": []
                    }).to_string()
                }
            },
            // This match precedes the FIFO head because the second turn's context also
            // contains both the first user message and its tool result after the queued
            // message is dispatched.
            {
                "match": waiting_message,
                "completion": {"content": waiting_result}
            },
            {
                "match": "capability released",
                "completion": {"content": blocking_result}
            },
            {
                "match": blocking_message,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": recovery_gate_tool_reference(),
                        "id": "recovery-matrix-gate-call",
                        "input": {"label": blocking_message}
                    }]
                }
            },
            {
                "match": distinct_message,
                "completion": {"content": distinct_result}
            }
        ]
    })
}

fn recovery_gate_tool() -> FixtureCapabilityTool {
    FixtureCapabilityTool {
        name: RECOVERY_GATE_TOOL_NAME.to_string(),
        description: "Hold one deterministic recovery-matrix turn at an exact barrier".to_string(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["label"],
            "properties": {"label": {"type": "string"}}
        }),
        item_key_pointer: None,
        idempotent: true,
        outcomes: vec![FixtureCapabilityOutcome::SuccessWithInput {
            output: json!({"result": "capability released"}),
        }],
    }
}

fn recovery_gate_tool_reference() -> String {
    moa_hands::mcp_tool_reference(RECOVERY_GATE_SERVER_NAME, RECOVERY_GATE_TOOL_NAME)
}

async fn seed_recovery_gate_allow_policy(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    tenant_id: TenantId,
) -> Result<()> {
    let identity = client
        .identity()
        .context("recovery-matrix client must carry identity headers")?;
    grant_recovery_matrix_relation(fixture, identity, "admin", &format!("tenant:{tenant_id}"))
        .await?;
    client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id,
                contact_id: None,
                tool_name: recovery_gate_tool_reference(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("deterministic recovery matrix barrier".to_string()),
            },
        )
        .await
        .context("allow recovery-matrix capability through production action policy")
}

async fn start_recovery_matrix_turn(
    client: &TestApiClient,
    session_id: SessionId,
    message: &str,
) -> Result<moa_wire::turn::StartTurnResponse> {
    client
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: message.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                resource_budget: Default::default(),
                execution_template: None,
            },
            None,
        )
        .await
}

async fn await_recovery_matrix_session_progress<F>(
    client: &TestApiClient,
    session_id: SessionId,
    predicate: F,
) -> Result<SessionProgress>
where
    F: Fn(&SessionProgress) -> bool,
{
    let deadline = tokio::time::Instant::now() + RECOVERY_MATRIX_TIMEOUT;
    loop {
        let progress: SessionProgress = client
            .post_call(
                &format!("/Session/{session_id}/progress"),
                &SessionProgressRequest::default(),
            )
            .await
            .context("read recovery-matrix Session progress")?;
        if predicate(&progress) {
            return Ok(progress);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "session {session_id} did not reach the recovery-matrix state; last progress: {progress:?}"
            );
        }
        tokio::time::sleep(RECOVERY_MATRIX_POLL_INTERVAL).await;
    }
}

fn recovery_matrix_identity(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

async fn create_recovery_matrix_tenant_session(
    fixture: &OrchestratorTestFixture,
    client: &TestApiClient,
    identity: &Identity,
    template: &SessionMeta,
    label: &str,
) -> Result<SessionId> {
    let session_id = SessionId::new();
    fixture
        .grant_tenant_operator_identity(identity, identity.tenant_id)
        .await?;
    grant_recovery_matrix_relation(
        fixture,
        identity,
        "participant",
        &format!("session:{session_id}"),
    )
    .await?;

    let now = chrono::Utc::now();
    let mut meta = template.clone();
    meta.id = session_id;
    meta.tenant_id = identity.tenant_id;
    meta.title = Some(label.to_string());
    meta.status = SessionStatus::Created;
    meta.active_channel_binding_id = None;
    meta.created_at = now;
    meta.updated_at = now;
    meta.completed_at = None;
    meta.parent_session_id = None;
    meta.contact = None;
    meta.created_by = Some(SessionActorRef::Identity { id: identity.id });
    meta.contact_promoted_from_id = None;
    meta.total_input_tokens = 0;
    meta.total_input_tokens_uncached = 0;
    meta.total_input_tokens_cache_write = 0;
    meta.total_input_tokens_cache_read = 0;
    meta.total_output_tokens = 0;
    meta.total_cost_cents = 0;
    meta.event_count = 0;
    meta.last_checkpoint_seq = None;

    client
        .create_session(meta.clone())
        .await
        .context("create recovery-matrix tenant session")?;
    client
        .append_event(
            session_id,
            Event::SessionCreated {
                tenant_id: identity.tenant_id,
                contact_id: None,
                created_by: Some(SessionActorRef::Identity { id: identity.id }),
                model: meta.model.clone(),
                channel: meta.channel,
            },
        )
        .await
        .context("append recovery-matrix SessionCreated event")?;
    client
        .init_session_vo(session_id, meta)
        .await
        .context("initialize recovery-matrix Session VO")?;
    Ok(session_id)
}

async fn grant_recovery_matrix_relation(
    fixture: &OrchestratorTestFixture,
    identity: &Identity,
    relation: &str,
    object: &str,
) -> Result<()> {
    let fga = fixture
        .fga_client
        .as_ref()
        .context("recovery-matrix fixture requires OpenFGA")?;
    fga.apply_raw(json!({
        "authorization_model_id": fga.model_id(),
        "writes": {"tuple_keys": [{
            "user": format!("{}:{}", identity.identity_type.as_str(), identity.id),
            "relation": relation,
            "object": object
        }]}
    }))
    .await
    .context("grant recovery-matrix OpenFGA relation")
}

async fn run_scripted_turn(
    client: &TestApiClient,
    session_id: SessionId,
    message: &str,
) -> Result<(String, TurnOutcome, Vec<EventRecord>)> {
    let started = client
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: message.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                resource_budget: Default::default(),
                execution_template: None,
            },
            None,
        )
        .await?;
    let turn_id = started
        .turn_id
        .context("start_turn should start immediately in isolated E2E")?;
    let outcome = client
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(90),
            Duration::from_millis(250),
        )
        .await?;
    let events = client.get_events(session_id, EventRange::all()).await?;
    Ok((turn_id, outcome, events))
}

async fn restate_rows(
    fixture: &OrchestratorTestFixture,
    query: impl AsRef<str>,
) -> Result<Vec<Value>> {
    let response = reqwest::Client::new()
        .post(format!("{}/query", fixture.admin_url.trim_end_matches('/')))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({ "query": query.as_ref() }))
        .send()
        .await
        .context("query Restate flow-control system tables")?
        .error_for_status()
        .context("Restate flow-control system-table query should succeed")?
        .json::<Value>()
        .await
        .context("decode Restate flow-control system-table query")?;
    response
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .context("Restate flow-control system-table query omitted rows")
}

async fn await_restate_rows<F>(
    fixture: &OrchestratorTestFixture,
    query: impl AsRef<str>,
    timeout: Duration,
    predicate: F,
) -> Result<Vec<Value>>
where
    F: Fn(&[Value]) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let rows = restate_rows(fixture, query.as_ref()).await?;
        if predicate(&rows) {
            return Ok(rows);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Restate system rows did not reach the held-turn state within {timeout:?}; rows={rows:?}"
            );
        }
        tokio::time::sleep(RECOVERY_MATRIX_POLL_INTERVAL).await;
    }
}

fn main_loop_should_not_run_script() -> serde_json::Value {
    json!({
        "keyed": [{
            "match": "You classify one user turn into MOA's public execution decision.",
            "completion": {
                "content": json!({
                    "label": "needs_input",
                    "strategy": null,
                    "rationale": "The target and requested change are required before work can begin.",
                    "confidence_bps": 10_000,
                    "missing_inputs": ["target", "specific change"]
                }).to_string(),
                "tool_calls": []
            }
        }],
        "default": {
            "completion": {
                "content": "MAIN LOOP SHOULD NOT RUN",
                "tool_calls": [{
                    "id": "clarification-should-not-run",
                    "name": "bash",
                    "input": { "cmd": "printf should-not-run" }
                }]
            }
        }
    })
}

fn progress_script() -> serde_json::Value {
    json!({
        "keyed": [
            {
                "match": ROUTE_CLASSIFIER_MATCH,
                "completion": {
                    "content": json!({
                        "label": "respond",
                        "strategy": null,
                        "rationale": "The request only needs a direct response.",
                        "confidence_bps": 10_000,
                        "missing_inputs": []
                    }).to_string()
                }
            },
            {
                "match": PROGRESS_MESSAGE,
                "completion": {
                    "content": "progress response complete",
                    "latency_ms": 2_000,
                    "ttft_ms": 2_000,
                    "duration_ms": 1,
                    "input_tokens": 8,
                    "output_tokens": 4,
                    "tool_calls": []
                }
            }
        ],
        "default": {
            "completion": {
                "content": "unexpected progress fixture fallback"
            }
        }
    })
}

fn execution_candidate(objective: &str) -> String {
    json!({
        "goal": {
            "objective": objective,
            "requirements": [{
                "id": "req_report",
                "description": "Produce the requested report."
            }],
            "deliverables": [],
            "coverage": [],
            "constraints": [],
            "completion_checks": [{
                "id": "check_output",
                "description": "Validate the final output.",
                "requirement_ids": ["req_report"],
                "constraint_ids": [],
                "kind": { "kind": "output_schema" }
            }]
        },
        "plan": {
            "schema_version": 2,
            "cancel_policy": "retain_effects",
            "input_schema": { "type": "object" },
            "output_schema": { "type": "object" },
            "nodes": [{
                "id": "output",
                "requirement_ids": ["req_report"],
                "depends_on": [],
                "when": null,
                "input": {},
                "output_schema": { "type": "object" },
                "operation": {
                    "kind": "output",
                    "value": { "status": "complete" }
                },
                "compensation": null,
                "retry": {
                    "max_attempts": 1,
                    "initial_backoff_ms": 0,
                    "max_backoff_ms": 0
                },
                "budget": null
            }]
        },
        "run_input": {}
    })
    .to_string()
}

fn classifier_response(label: &str, strategy: Option<&str>, rationale: &str) -> serde_json::Value {
    json!({
        "completion": {
            "content": json!({
                "label": label,
                "strategy": strategy,
                "rationale": rationale,
                "confidence_bps": 10_000,
                "missing_inputs": []
            }).to_string(),
            "tool_calls": []
        }
    })
}

async fn route_decisions_and_strategies(
    postgres_url: &str,
    session_id: SessionId,
) -> Result<Vec<(String, Option<String>)>> {
    let pool = PgPool::connect(postgres_url).await?;
    let values = sqlx::query_as(
        "SELECT decision, strategy FROM moa.execution_route_audit \
         WHERE session_id = $1 ORDER BY accepted_at",
    )
    .bind(session_id.0)
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    Ok(values)
}

async fn planner_outcomes(postgres_url: &str, session_id: SessionId) -> Result<Vec<String>> {
    audit_column(
        postgres_url,
        "SELECT outcome FROM moa.execution_planner_call_audit \
         WHERE session_id = $1 ORDER BY created_at",
        session_id,
    )
    .await
}

async fn compile_outcomes(postgres_url: &str, session_id: SessionId) -> Result<Vec<String>> {
    audit_column(
        postgres_url,
        "SELECT outcome FROM moa.execution_compile_audit \
         WHERE session_id = $1 ORDER BY created_at",
        session_id,
    )
    .await
}

async fn audit_column(
    postgres_url: &str,
    query: &str,
    session_id: SessionId,
) -> Result<Vec<String>> {
    let pool = PgPool::connect(postgres_url).await?;
    let values = sqlx::query_scalar(query)
        .bind(session_id.0)
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(values)
}

fn user_messages(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn queued_messages(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::QueuedMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn brain_responses(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse {
                text,
                model_tier,
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                cost_cents,
                duration_ms,
                ..
            } => {
                if text == CLARIFICATION_RESPONSE {
                    assert_eq!(*model_tier, ModelTier::Auxiliary);
                    assert_eq!(*input_tokens_uncached, 0);
                    assert_eq!(*input_tokens_cache_write, 0);
                    assert_eq!(*input_tokens_cache_read, 0);
                    assert_eq!(*output_tokens, 0);
                    assert_eq!(*cost_cents, 0);
                    assert_eq!(*duration_ms, 0);
                }
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn progress_updates(events: &[EventRecord], turn_id: &str) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ProgressUpdate {
                turn_id: event_turn_id,
                phase,
                summary,
                ..
            } if event_turn_id == turn_id => Some((phase.clone(), summary.clone())),
            _ => None,
        })
        .collect()
}

fn tool_call_count(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::ToolCall { .. }))
        .count()
}

fn event_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .map(|record| match &record.event {
            Event::SessionCreated { .. } => format!("#{} SessionCreated", record.sequence_num),
            Event::UserMessage { text, .. } => {
                format!("#{} UserMessage {text}", record.sequence_num)
            }
            Event::BrainResponse { text, .. } => {
                format!("#{} BrainResponse {text}", record.sequence_num)
            }
            Event::ProgressUpdate {
                turn_id,
                phase,
                summary,
                elapsed_ms,
            } => format!(
                "#{} ProgressUpdate {turn_id} {phase} {elapsed_ms}ms {summary}",
                record.sequence_num
            ),
            Event::ToolCall {
                provider_tool_use_id,
                tool_name,
                ..
            } => format!(
                "#{} ToolCall {tool_name} {provider_tool_use_id:?}",
                record.sequence_num
            ),
            other => format!("#{} {}", record.sequence_num, other.type_name()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn default_fixture_identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0001),
        tenant_id: TenantId::from(Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

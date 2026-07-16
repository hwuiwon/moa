//! End-to-end turn responsiveness coverage through the scripted orchestrator fixture.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::turn::{
    SessionProgress, SessionProgressRequest, StartTurnRequest, TurnOutcome, TurnOutcomeKind,
    TurnPhase, TurnProgress,
};
use moa_core::{
    events::Event, events::EventType, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SessionId, types::identifiers::TenantId,
    types::provider::ModelTier, types::session::SessionStatus,
};
use moa_test_support::{OrchestratorTestFixture, TestApiClient};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const CLARIFICATION_RESPONSE: &str = "What should I change? Point me at the file, message, object, or output and the specific fix you want.";

#[tokio::test]
#[ignore = "requires a local restate-server, Postgres, OpenFGA, and provider-overrides feature"]
async fn execution_routing_writes_normalized_audits_outside_session_progress_service_e2e()
-> Result<()> {
    // Pins: the real Session -> TurnExecution -> Execution service path distinguishes all three
    // routes, durably audits planning in normalized storage, and returns Accepted with one run UID
    // without placing planner internals in Session/progress events.
    let run_objective = "Start an execution run for a durable report";
    let fixture = OrchestratorTestFixture::with_script(json!({
        "keyed": [
            {
                "match": "Inspect the repository",
                "completion": { "content": "inspection complete" }
            },
            {
                "match": run_objective,
                "completion": { "content": execution_candidate(run_objective) }
            }
        ],
        "default": { "completion": { "content": "4" } }
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
        route_modes(&fixture.postgres_url, respond_session).await?,
        vec!["respond"]
    );

    let act_session = test.create_session("execution-route-act").await?;
    let (_, act_outcome, act_events) = run_scripted_turn(
        &fixture.client,
        act_session,
        "Inspect the repository and explain the result.",
    )
    .await?;
    assert_eq!(
        act_outcome.kind,
        TurnOutcomeKind::Completed,
        "act outcome: {}; events: {}",
        act_outcome.message,
        event_summary(&act_events)
    );
    assert_eq!(
        route_modes(&fixture.postgres_url, act_session).await?,
        vec!["act"]
    );

    let run_session = test.create_session("execution-route-run").await?;
    let (_, run_outcome, run_events) =
        run_scripted_turn(&fixture.client, run_session, run_objective).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = run_outcome.kind else {
        panic!(
            "explicit run must return Accepted, got {:?}: {}; events: {}",
            run_outcome.kind,
            run_outcome.message,
            event_summary(&run_events)
        );
    };
    assert_eq!(
        route_modes(&fixture.postgres_url, run_session).await?,
        vec!["run"]
    );
    assert!(run_events.iter().any(|record| {
        matches!(
            &record.event,
            Event::ExecutionRunStarted(started) if started.run_uid == execution_run_uid
        )
    }));
    assert_eq!(
        planner_outcomes(&fixture.postgres_url, run_session).await?,
        vec!["accepted"]
    );
    assert_eq!(
        compile_outcomes(&fixture.postgres_url, run_session).await?,
        vec!["accepted"]
    );
    assert_eq!(
        fixture
            .client
            .session(run_session.to_string())
            .status()
            .await?,
        SessionStatus::Running
    );

    let progress: SessionProgress = fixture
        .client
        .post_call(
            &format!("/Session/{run_session}/progress"),
            &SessionProgressRequest {
                event_range: EventRange::all(),
            },
        )
        .await?;
    assert_eq!(progress.events, run_events);
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
async fn turn_progress_projection_survives_without_persisted_updates() -> Result<()> {
    // Pins: ProgressUpdate is transient workflow projection state, not durable event-log history.
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

    let (turn_id, outcome, events) = run_scripted_turn(
        &fixture.client,
        session_id,
        "please answer with the scripted progress response",
    )
    .await
    .context("run progress-emitting turn")?;

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

    let progress = read_turn_progress(&fixture.ingress_url, &turn_id)
        .await
        .context("fresh TurnExecution/progress read should recover projection")?;
    assert_eq!(progress.turn_id, turn_id);
    assert_eq!(progress.phase, TurnPhase::Completed);
    assert_eq!(
        progress.last_progress_summary.as_deref(),
        Some("Calling the model")
    );
    Ok(())
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
                user_message: message.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
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

async fn read_turn_progress(ingress_url: &str, turn_id: &str) -> Result<TurnProgress> {
    reqwest::Client::new()
        .post(format!(
            "{}/restate/call/TurnExecution/{turn_id}/progress",
            ingress_url.trim_end_matches('/')
        ))
        .send()
        .await
        .context("send TurnExecution/progress request")?
        .error_for_status()
        .context("TurnExecution/progress should succeed")?
        .json::<TurnProgress>()
        .await
        .context("deserialize TurnExecution/progress response")
}

fn main_loop_should_not_run_script() -> serde_json::Value {
    json!({
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
        "default": {
            "completion": {
                "content": "progress response complete",
                "duration_ms": 1,
                "input_tokens": 8,
                "output_tokens": 4,
                "tool_calls": []
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
            "schema_version": 1,
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

async fn route_modes(postgres_url: &str, session_id: SessionId) -> Result<Vec<String>> {
    audit_column(
        postgres_url,
        "SELECT mode FROM moa.execution_route_audit WHERE session_id = $1 ORDER BY accepted_at",
        session_id,
    )
    .await
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

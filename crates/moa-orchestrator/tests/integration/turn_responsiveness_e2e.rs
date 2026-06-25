//! End-to-end turn responsiveness coverage through the scripted orchestrator fixture.

use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::turn::{
    StartTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{Event, EventRange, EventRecord, EventType, ModelTier, SessionId, TenantId};
use moa_test_support::{OrchestratorTestFixture, TestApiClient};
use serde_json::json;
use uuid::Uuid;

const CLARIFICATION_RESPONSE: &str = "What should I change? Point me at the file, message, object, or output and the specific fix you want.";

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
async fn turn_progress_persisted_update_replays_after_fresh_read() -> Result<()> {
    // Pins: durable ProgressUpdate events and TurnExecution/progress survive a reconnect-style fresh read.
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
    let session_id = test.create_session("turn-progress-replay").await?;

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
        vec![
            ("Compiling".to_string(), "Working on it".to_string()),
            ("Streaming".to_string(), "Calling the model".to_string()),
        ],
        "initial read should include deterministic progress updates: {}",
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
        .context("fresh event-log read should recover progress updates")?;
    assert_eq!(
        progress_updates(&replayed, &turn_id),
        vec![
            ("Compiling".to_string(), "Working on it".to_string()),
            ("Streaming".to_string(), "Calling the model".to_string()),
        ],
        "fresh event read should replay persisted progress updates: {}",
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
            "{}/TurnExecution/{turn_id}/progress",
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
        identity_type: IdentityType::User,
        id: Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0001),
        tenant_id: TenantId::from(Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

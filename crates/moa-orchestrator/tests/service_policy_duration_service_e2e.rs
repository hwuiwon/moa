//! Opt-in duration coverage for Restate high-cost service policy.

#![cfg(feature = "integration")]

use std::time::Duration;

use anyhow::{Context, Result};
use moa_test_support::OrchestratorTestFixture;
use moa_wire::turn::{StartTurnRequest, TurnOutcomeKind};
use serde_json::json;

#[tokio::test]
#[ignore = "requires Docker for the local Restate/Postgres/OpenFGA/Valkey fixture and takes about 61 seconds"]
async fn sixty_one_second_scripted_session_call_completes_under_restate_policy() -> Result<()> {
    // Pins: the public Session path can keep its private LLM child active beyond
    // Restate's one-minute default while exercising the production six-minute
    // inactivity policy. The opt-in lane pays one real 61-second wait.
    const OBJECTIVE: &str = "Answer the deterministic long-call probe.";
    let fixture = OrchestratorTestFixture::with_script_and_env(
        json!({
            "default": {"completion": {"content": "unexpected scripted request"}},
            "keyed": [
                {
                    "match": "You classify one user turn into MOA's public execution decision.",
                    "completion": {
                        "content": r#"{"label":"respond","strategy":null,"rationale":"The probe asks for one response.","confidence_bps":10000,"missing_inputs":[]}"#
                    }
                },
                {
                    "match": OBJECTIVE,
                    "completion": {
                        "content": "long-call-complete",
                        "latency_ms": 61_000,
                        "ttft_ms": 61_000,
                        "stop_reason": "end_turn"
                    }
                }
            ]
        }),
        vec![
            (
                "MOA_PROVIDERS_STREAM_TIMEOUTS_FIRST_BYTE_MS".to_string(),
                "70000".to_string(),
            ),
            (
                "MOA_PROVIDERS_STREAM_TIMEOUTS_IDLE_MS".to_string(),
                "70000".to_string(),
            ),
            (
                "MOA_PROVIDERS_STREAM_TIMEOUTS_TOTAL_MS".to_string(),
                "300000".to_string(),
            ),
        ],
    )
    .await
    .context("boot scripted long-call fixture")?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("sixty-one-second-llm").await?;
    let started_at = tokio::time::Instant::now();
    let started = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                client_message_id: moa_test_support::fixtures::fresh_client_message_id(),
                reply_to: None,
                stream_cursor: None,
                user_message: OBJECTIVE.to_string(),
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
        .context("start public long-call Session turn")?;
    let turn_id = started
        .turn_id
        .context("idle long-call session should start immediately")?;
    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(95),
            Duration::from_millis(250),
        )
        .await
        .context("await long-call Session outcome")?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, "long-call-complete");
    let elapsed = started_at.elapsed();
    assert!(
        elapsed >= Duration::from_secs(61) && elapsed < Duration::from_secs(95),
        "scripted call should exercise a 60-300 second duration, got {elapsed:?}"
    );
    Ok(())
}

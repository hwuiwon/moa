//! Smoke tests for the Restate-backed orchestrator integration fixture.

#![cfg(feature = "integration")]

use std::time::Duration;

use moa_core::events::Event;
use moa_core::types::events_stream::EventRange;
use moa_core::types::session::SessionStatus;
use moa_test_support::OrchestratorTestFixture;
use moa_wire::turn::{StartTurnRequest, TurnOutcomeKind};
use serde_json::json;

#[tokio::test]
async fn fixture_round_trips_session_turn_through_restate() -> anyhow::Result<()> {
    // Pins: the shared fixture can start a real Session VO turn through Restate and observe its terminal outcome.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("round-trip").await?;

    let start = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: "ping".to_string(),
                attachments: Vec::new(),
                model: Some("scripted-loadtest".to_string()),
                contact: None,
                max_turns: None,
                execution_template: None,
            },
            Some("fixture-round-trip"),
        )
        .await?;
    assert!(!start.queued, "first turn should start immediately");
    let turn_id = start.turn_id.expect("turn id for immediately started turn");

    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(30),
            Duration::from_millis(250),
        )
        .await?;
    assert_eq!(outcome.turn_id, turn_id);
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let snapshot = test
        .client()
        .session(session_id.to_string())
        .snapshot()
        .await?;
    assert_eq!(snapshot.active_turn_id, None);
    assert_eq!(snapshot.pending_message_count, 0);
    assert_eq!(
        snapshot.last_outcome.as_ref().map(|last| &last.turn_id),
        Some(&outcome.turn_id)
    );
    Ok(())
}

#[tokio::test]
async fn fixture_round_trips_accepted_execution_run_through_restate() -> anyhow::Result<()> {
    // Pins: a real Restate Session turn admits one detached execution run, returns its committed
    // UID as Accepted, and keeps the owning session Running after the root turn detaches.
    let objective = "Start an execution run for a durable fixture report";
    let fixture = OrchestratorTestFixture::with_script(json!({
        "keyed": [{
            "match": objective,
            "completion": { "content": execution_candidate(objective) }
        }],
        "default": { "completion": { "content": "unexpected scripted request" } }
    }))
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("accepted-execution-run").await?;

    let start = test
        .client()
        .session(session_id.to_string())
        .start_turn(
            StartTurnRequest {
                user_message: objective.to_string(),
                attachments: Vec::new(),
                model: None,
                contact: None,
                max_turns: None,
                execution_template: None,
            },
            Some("fixture-accepted-execution-run"),
        )
        .await?;
    assert!(!start.queued, "execution turn should start immediately");
    let turn_id = start.turn_id.expect("turn id for admitted execution turn");
    let outcome = test
        .client()
        .session(session_id.to_string())
        .await_turn_outcome(
            &turn_id,
            Duration::from_secs(60),
            Duration::from_millis(250),
        )
        .await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        anyhow::bail!(
            "execution turn should be Accepted, got {:?}: {}",
            outcome.kind,
            outcome.message
        );
    };

    let snapshot = test
        .client()
        .session(session_id.to_string())
        .snapshot()
        .await?;
    assert_eq!(snapshot.active_turn_id, None);
    assert_eq!(snapshot.active_execution_run_uids, vec![execution_run_uid]);
    assert_eq!(
        test.client()
            .session(session_id.to_string())
            .status()
            .await?,
        SessionStatus::Running
    );
    let events = test
        .client()
        .get_events(session_id, EventRange::all())
        .await?;
    assert_eq!(
        events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionRunStarted(started) => Some(started.run_uid),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![execution_run_uid]
    );
    Ok(())
}

fn execution_candidate(objective: &str) -> String {
    json!({
        "goal": {
            "objective": objective,
            "requirements": [{
                "id": "req_report",
                "description": "Produce the requested fixture report."
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

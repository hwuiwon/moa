//! Smoke tests for the Restate-backed orchestrator integration fixture.

#![cfg(feature = "integration")]

use std::time::Duration;

use moa_core::wire::{StartTurnRequest, TurnOutcomeKind};
use moa_test_support::OrchestratorTestFixture;

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

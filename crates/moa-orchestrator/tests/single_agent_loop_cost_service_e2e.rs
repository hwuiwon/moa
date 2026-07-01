//! Deterministic single-agent (non-delegating) loop-cost measurement over the REAL Restate path.
//!
//! The coordination path is already minimal on model turns/tool calls (see
//! `coordination_cost_service_e2e`). The common case — a plain single-agent turn (context → model →
//! tools → repeat) — is where per-user-message turn/tool-call waste is most likely to hide. This
//! test drives a plain tool-using conversation with a keyed scripted provider and reconstructs a
//! [`moa_core::ConversationCost`] so the exact model turns, tool calls, and internal round-trips per
//! user message are pinned and any regression (or optimization) is provable with zero LLM noise.
//!
//! Run: the fixture boots Postgres + Restate + OpenFGA testcontainers and builds
//! `moa-orchestrator-bin` with `provider-overrides`. Requires Docker.
//! `cargo nextest run -p moa-orchestrator --features integration single_agent_loop_cost`.

#![cfg(feature = "integration")]

use moa_core::ConversationCost;
use moa_test_support::{ConversationOptions, OrchestratorTestFixture, drive_conversation};
use serde_json::json;

/// Keyed script for a plain single-agent answer: the user question maps to a direct end-turn
/// reply with no tool calls. Anything else the loop asks the model for internally (query rewrite,
/// segment assessment, …) falls through to `default`, so the measured `model_turns` reveals how
/// many model round-trips the loop really spends to answer one simple message.
fn single_answer_script() -> serde_json::Value {
    json!({
        "default": { "content": "OK" },
        "keyed": [
            {
                "match": "capital of France",
                "completion": {
                    "content": "The capital of France is Paris.",
                    "stop_reason": "end_turn"
                }
            }
        ]
    })
}

#[tokio::test]
async fn single_agent_plain_answer_loop_cost_service_e2e() {
    let fixture = OrchestratorTestFixture::with_script_and_env(
        single_answer_script(),
        vec![("MOA_PERSIST_TURN_METRICS".to_string(), "1".to_string())],
    )
    .await
    .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("single-agent-cost")
        .await
        .expect("create + init session");

    let events = drive_conversation(
        test.client(),
        session_id,
        &["What is the capital of France?"],
        ConversationOptions::default(),
    )
    .await
    .expect("drive the single-agent conversation to completion");

    let cost = ConversationCost::from_events(&events);
    eprintln!("BASELINE single_agent_cost = {cost:#?}");

    // A plain question is answered in EXACTLY ONE model turn — the loop adds no hidden query-rewrite,
    // segment-assessment, or wrap-up model round-trips. This is the proven per-message floor; any
    // regression that inserts an extra model call for a trivial message fails here.
    assert_eq!(
        cost.model_turns, 1,
        "one plain question costs exactly one model turn"
    );
    assert_eq!(cost.total_tool_calls, 0, "a plain answer uses no tools");
    assert_eq!(cost.worker_spawns, 0, "a plain question spawns no workers");
    assert_eq!(cost.error_events, 0, "no durable errors on the happy path");
    assert_eq!(
        cost.total_input_tokens, 64,
        "one scripted turn x 64 uncached input tokens"
    );
    assert!(
        cost.total_output_tokens > 0,
        "the scripted answer carries output tokens"
    );

    // The single-agent happy path makes ZERO Session/Worker VO round-trips and no fire-and-forget
    // sends — it only appends its own turn events. This is the coordination-overhead floor.
    assert!(
        cost.coordination.present,
        "TurnMetrics were persisted (MOA_PERSIST_TURN_METRICS=1)"
    );
    assert_eq!(
        cost.coordination.total_vo_calls(),
        0,
        "a non-delegating turn makes no VO round-trips"
    );
    assert_eq!(cost.coordination.vo_sends, 0, "no worker dispatch sends");
}

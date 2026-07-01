//! Deterministic long-conversation loop-cost measurement over the REAL Restate path.
//!
//! Simple single-turn paths are already model-turn/tool-call optimal (see the other `*_cost`
//! gates). Long conversations are where per-message overhead could compound: extra model turns as
//! history grows, compaction/checkpoint behavior, and VO round-trip growth. This test drives a
//! ~16-message conversation with a keyed scripted provider and reconstructs a
//! [`moa_core::ConversationCost`] so the structural cost profile is pinned deterministically.
//!
//! Scope note: the scripted provider reports a FIXED synthetic `input_tokens`, so this test
//! measures STRUCTURAL waste (model turns per message, compaction triggers, round-trip growth) —
//! not raw token growth, which needs a content-sizing provider or the live sweep.
//!
//! Run: boots Postgres + Restate + OpenFGA testcontainers and builds `moa-orchestrator-bin` with
//! `provider-overrides`. Requires Docker.
//! `cargo nextest run -p moa-orchestrator --features integration long_conversation_cost`.

#![cfg(feature = "integration")]

use std::collections::BTreeMap;

use moa_core::ConversationCost;
use moa_test_support::{ConversationOptions, OrchestratorTestFixture, drive_conversation};
use serde_json::json;

/// Number of user messages driven through the single growing session.
const MESSAGE_COUNT: usize = 16;

/// Every plain user message resolves to a short end-turn acknowledgement (no tools, no delegation),
/// so each message should cost exactly one model turn — any extra model turns as the history grows
/// are loop overhead (e.g. compaction-inserted model calls) worth surfacing.
fn long_conversation_script() -> serde_json::Value {
    json!({
        "default": { "content": "Acknowledged.", "stop_reason": "end_turn" }
    })
}

#[tokio::test]
async fn long_conversation_loop_cost_service_e2e() {
    let fixture = OrchestratorTestFixture::with_script_and_env(
        long_conversation_script(),
        vec![("MOA_PERSIST_TURN_METRICS".to_string(), "1".to_string())],
    )
    .await
    .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("long-convo-cost")
        .await
        .expect("create + init session");

    // A single session that accumulates history across many user messages.
    let turns: Vec<String> = (0..MESSAGE_COUNT)
        .map(|i| {
            format!(
                "Message {i}: please note fact number {i} about our ongoing multi-topic \
                 discussion, and keep the earlier facts in mind as we continue."
            )
        })
        .collect();
    let turn_refs: Vec<&str> = turns.iter().map(String::as_str).collect();

    let events = drive_conversation(
        test.client(),
        session_id,
        &turn_refs,
        ConversationOptions::default(),
    )
    .await
    .expect("drive the long conversation to completion");

    // Durable-log event histogram, surfaced so any structural drift (extra turns, surprise
    // compaction, segment churn) is visible in the run output.
    let mut histogram: BTreeMap<String, usize> = BTreeMap::new();
    for rec in &events {
        let name = format!("{:?}", rec.event)
            .split_whitespace()
            .next()
            .unwrap_or("?")
            .to_string();
        *histogram.entry(name).or_default() += 1;
    }
    eprintln!("LONG_CONVO event histogram = {histogram:#?}");

    let cost = ConversationCost::from_events(&events);
    eprintln!("LONG_CONVO cost = {cost:#?}");

    let checkpoints = events
        .iter()
        .filter(|rec| matches!(rec.event, moa_core::Event::Checkpoint { .. }))
        .count();

    // A plain N-message conversation costs EXACTLY N model turns: the loop inserts no extra model
    // round-trips as history grows. This is the long-conversation turn floor — a regression that
    // adds a hidden per-message or history-rebuild model turn fails here.
    assert_eq!(
        cost.model_turns,
        MESSAGE_COUNT as u64,
        "exactly one model turn per user message; the loop adds no extra turns as history grows"
    );
    assert_eq!(cost.total_tool_calls, 0, "a plain conversation uses no tools");
    assert_eq!(cost.worker_spawns, 0, "plain conversation spawns no workers");
    assert_eq!(cost.error_events, 0, "no durable errors on the happy path");
    // Compaction (tier3) only triggers near the model's context ceiling (~160k tokens), so a
    // normal-length conversation must NOT compact and must NOT add summarization turns.
    assert_eq!(
        checkpoints, 0,
        "compaction must not trigger for a normal-length conversation"
    );
    // Non-delegating turns make no Session/Worker VO round-trips — coordination overhead does not
    // leak into a plain conversation, and it does not grow with history.
    assert!(
        cost.coordination.present,
        "TurnMetrics were persisted (MOA_PERSIST_TURN_METRICS=1)"
    );
    assert_eq!(
        cost.coordination.total_vo_calls(),
        0,
        "a plain conversation makes no VO round-trips, regardless of length"
    );
}

//! Deterministic coordination-cost measurement over the REAL Restate turn/worker path.
//!
//! A keyed scripted provider makes an auto-delegation fan-in run deterministically
//! (coordinator → N workers → `WorkerResultBundle` → synthesis), so the internal VO round-trips,
//! model turns, tool calls, and tokens are exactly reproducible. The durable event log is then
//! reconstructed into a [`moa_eval_core::ConversationCost`] and asserted. This is the reliable,
//! zero-LLM-noise regression gate for the coordination optimizations (single-owner fan-in,
//! wait fast-path, never-terminal recovery) and the baseline the further optimization work is
//! measured against.
//!
//! Run: the fixture boots Postgres + Restate + OpenFGA testcontainers and builds
//! `moa-orchestrator-bin` with `provider-overrides`. Requires Docker.
//! `cargo nextest run -p moa-orchestrator --features integration coordination_cost`.

#![cfg(feature = "integration")]

use moa_eval_core::ConversationCost;
use moa_test_support::{ConversationOptions, OrchestratorTestFixture, drive_conversation};
use serde_json::json;

/// Keyed script: every auto-delegation worker turn carries the stable task marker
/// "Complete this coordinator-delegated subtask", and the coordinator synthesis turn carries
/// "Auto-delegated worker results are complete". Synthesis is listed first (more specific) so it
/// wins over the worker match. Anything else (e.g. an internal query-rewrite call) falls through
/// to `default`. Matching (not FIFO position) keeps the script correct as round-trip counts change.
fn auto_delegation_script() -> serde_json::Value {
    json!({
        "default": { "content": "OK" },
        "keyed": [
            {
                "match": "Auto-delegated worker results are complete",
                "completion": {
                    "content": "FINAL: synthesized comparison of LISTEN/NOTIFY, polling, and SSE.",
                    "stop_reason": "end_turn"
                }
            },
            {
                "match": "Complete this coordinator-delegated subtask",
                "completion": {
                    "content": "Subtask outcome: analysis complete; no blockers.",
                    "stop_reason": "end_turn"
                }
            }
        ]
    })
}

#[tokio::test]
async fn auto_delegation_fan_in_coordination_cost_service_e2e() {
    let fixture = OrchestratorTestFixture::with_script_and_env(
        auto_delegation_script(),
        // Persist per-turn TurnMetrics so ConversationCost can read internal VO round-trips.
        vec![("MOA_PERSIST_TURN_METRICS".to_string(), "1".to_string())],
    )
    .await
    .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("coord-cost")
        .await
        .expect("create + init session");

    // A three-way comparison triggers the generic auto-delegation heuristic → 3 ready DAG nodes.
    let events = drive_conversation(
        test.client(),
        session_id,
        &["Compare LISTEN/NOTIFY, polling, and SSE for progress replay in our app."],
        ConversationOptions::default(),
    )
    .await
    .expect("drive the auto-delegation conversation to completion");

    let cost = ConversationCost::from_events(&events);
    // Printed so any baseline drift shows the exact KPI that moved (this is also the A/B signal
    // the optimization work reads: fewer model turns / tool calls / tokens / VO round-trips = win).
    eprintln!("BASELINE coordination_cost = {cost:#?}");

    // --- Model-side KPIs: a pure function of the durable event log and the scripted responses, so
    // fully deterministic. These are the tool-call/turn/token budget the optimization work drives
    // down; a change here means a regression (or, if intentional, a baseline to re-lock).
    assert_eq!(
        cost.model_turns, 4,
        "one compare-of-three request settles in exactly four session model turns; a change is a \
         turn-count regression (turns 3-4 make zero tool calls — a Phase-5 redundant-turn target)"
    );
    assert_eq!(
        cost.total_tool_calls, 3,
        "exactly three durable tool calls — the three auto-delegated spawn_worker records, with no \
         wait_worker/list_workers churn"
    );
    assert_eq!(
        cost.tool_calls_by_name.get("spawn_worker").copied(),
        Some(3),
        "the three fan-in workers are the only tool calls in the log"
    );
    assert_eq!(
        cost.total_input_tokens, 256,
        "four scripted turns x 64 uncached input tokens; a change is a context-size regression"
    );
    assert_eq!(
        cost.total_output_tokens, 53,
        "12+12+12 default turns + 17 synthesis output tokens"
    );

    // --- Final response regression guards: the coordinator's last model turn must carry the
    // scripted synthesis text. This pins the branch's fixed single-owner fan-in behavior — the
    // empty-final and raw-worker-leak failure modes were fixed here, and the KPI counts alone cannot
    // catch a regression back to an empty reply or to leaking raw worker output verbatim.
    let final_text = cost
        .final_text
        .as_deref()
        .expect("the coordinator emitted a final BrainResponse");
    assert!(
        !final_text.is_empty(),
        "the coordinator's final reply must be non-empty"
    );
    assert_eq!(
        final_text, "FINAL: synthesized comparison of LISTEN/NOTIFY, polling, and SSE.",
        "the final reply is the scripted synthesis turn, not a blank answer"
    );
    assert!(
        !final_text.contains("Subtask outcome: analysis complete; no blockers."),
        "the coordinator must synthesize, not leak the raw worker terminal output as its final reply"
    );

    // --- Structural coordination invariants.
    assert_eq!(
        cost.worker_spawns, 3,
        "a compare-of-three auto-delegates exactly three ready workers"
    );
    assert_eq!(cost.worker_result_bundles, 1, "exactly one fan-in bundle");
    assert_eq!(
        cost.bundled_results, 3,
        "all three worker results are bundled"
    );
    assert_eq!(
        cost.error_events, 0,
        "the deterministic fan-in produces no durable errors"
    );

    // --- Internal VO round-trip KPIs (from persisted TurnMetrics): the coordination-latency and
    // replay-cost budget the durable-coordination changes optimize. Locked exact so a batch/elide
    // optimization that lowers them fails the gate loudly and re-locks the win.
    assert!(
        cost.coordination_present,
        "TurnMetrics were persisted (MOA_PERSIST_TURN_METRICS=1), so VO round-trips are measured"
    );
    // These internal VO/replay counters couple to Session-VO mechanics that legitimately churn when
    // coordination code is refactored, so they are asserted as CEILINGS (<=), not exact equality:
    // they pin that coordination overhead does not grow while letting an optimization that lowers a
    // round-trip pass without re-locking a brittle exact number. Worker-VO stays exact-0 because it
    // is a structural invariant (fan-in reads go through the Session VO, never direct Worker VO).
    assert!(
        cost.coordination.session_vo_calls <= 8,
        "coordinator<->Session VO round-trips must not exceed the 8 round-trip fan-in ceiling; got {}",
        cost.coordination.session_vo_calls
    );
    assert_eq!(
        cost.coordination.worker_vo_calls, 0,
        "fan-in reads go through the Session VO, not direct Worker VO calls, on this path"
    );
    assert!(
        cost.coordination.vo_sends <= 3,
        "fire-and-forget worker dispatch sends must not exceed the 3 ceiling; got {}",
        cost.coordination.vo_sends
    );
    assert!(
        cost.coordination.durable_appends <= 11,
        "durable append steps must not exceed the 11 replay-cost ceiling; got {}",
        cost.coordination.durable_appends
    );
    assert!(
        cost.get_events_calls <= 4,
        "session event-log reads must not exceed the 4 replay-read ceiling; got {}",
        cost.get_events_calls
    );
    assert!(
        cost.coordination.total_vo_calls() <= 8,
        "total VO round-trips must not exceed the 8 round-trip ceiling; got {}",
        cost.coordination.total_vo_calls()
    );
}

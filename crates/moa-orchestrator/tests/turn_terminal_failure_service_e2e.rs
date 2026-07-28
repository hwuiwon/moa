//! Deterministic terminal turn-failure coverage over the REAL Restate path.
//!
//! Ordinary session and worker service E2Es drive turns that succeed, so they
//! never reach a turn workflow's catch-all failure boundary and cannot pin what
//! it records. This fixture is the injector: a scripted provider that fails
//! every completion far more times than the LLM gateway's bounded retry policy
//! absorbs, which completes the gateway's run entry terminally and drives the
//! turn into its catch-all.
//!
//! Run: the fixture boots Postgres + Restate + OpenFGA testcontainers and builds
//! `moa-orchestrator-bin` with `provider-overrides`. Requires Docker.
//! `cargo nextest run -p moa-orchestrator --features integration turn_terminal_failure`.

#![cfg(feature = "integration")]

use std::time::Duration;

use moa_core::events::{Event, TurnFailureActor};
use moa_core::types::events_stream::EventRecord;
use moa_core::types::identifiers::SessionId;
use moa_core::types::session::SessionStatus;
use moa_test_support::{
    ConversationOptions, OrchestratorTestFixture, TestApiClient, drive_conversation,
};
use serde_json::json;

/// Fragments that would only appear if a raw error rendering leaked into a
/// durable field. `TurnFailed.summary` and `TurnOutcome.message` are both
/// supposed to carry the fixed class sentence and nothing else.
const RAW_ERROR_MARKERS: &[&str] = &[
    "Retryable error",
    "Terminal error",
    "HandlerError",
    "scripted",
    "500",
    "Error:",
    "panic",
];

/// Fails every completion 100 times — far past the LLM gateway's bounded
/// `max_attempts(5)` retry policy — so the gateway's durable run entry completes
/// terminally instead of eventually succeeding. That terminal failure propagates
/// out of the model call and into the turn workflow's catch-all boundary, which
/// is the code path under test. A fault that recovers within the retry budget
/// would never reach it.
fn always_failing_script() -> serde_json::Value {
    json!({
        "default": {
            "completion": {
                "content": "unreachable",
                "tool_calls": [],
                "fault": { "fail_first_n": 100, "status": 500 }
            }
        }
    })
}

/// Returns every canonical failed-turn fact in the log, in sequence order.
fn turn_failures(events: &[EventRecord]) -> Vec<&Event> {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::TurnFailed { .. }))
        .map(|record| &record.event)
        .collect()
}

/// Re-fetches the session log until `predicate` holds, returning the final set.
///
/// The child's terminal lifecycle delivery (`WorkerStatusChanged` +
/// `WorkerNotificationDelivered`) and its failed-attention resume are two
/// independent detached chains off the same worker failure; the conversation can
/// settle on the resume before delivery lands. Asserting delivery without this
/// bounded wait races that ordering.
async fn wait_for_session_events(
    client: &TestApiClient,
    session_id: SessionId,
    initial: Vec<EventRecord>,
    predicate: impl Fn(&[EventRecord]) -> bool,
) -> Vec<EventRecord> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut events = initial;
    while !predicate(&events) {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for expected session events; got {} events",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        events = client
            .get_events(
                session_id,
                moa_core::types::events_stream::EventRange::all(),
            )
            .await
            .expect("re-fetch session events");
    }
    events
}

/// Reads the session's current durable lifecycle status.
///
/// Status lives on the session row and the VO, not in the event log, so it is
/// read back through the handler rather than reconstructed from events. The
/// handler takes no request body, so this goes through the fixture's
/// session-scoped `status()` (an empty-body call) rather than posting a
/// serialized `()`.
async fn session_status(client: &TestApiClient, session_id: SessionId) -> SessionStatus {
    client
        .session(session_id.to_string())
        .status()
        .await
        .expect("read session status")
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA testcontainers fixture"]
async fn root_turn_catch_all_records_one_canonical_failure_service_e2e() {
    // Pins: a root turn that dies at its catch-all boundary leaves exactly one
    // canonical `TurnFailed` fact in durable history, attributed to the
    // coordinator, carrying only the fixed class summary. Without the fact the
    // failure is invisible to anything reading the event log; with a raw
    // `format!("{err:?}")` summary the log becomes a leak channel for provider
    // and prompt material.
    let fixture = OrchestratorTestFixture::with_script(always_failing_script())
        .await
        .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("root-turn-terminal-failure")
        .await
        .expect("create + init session");

    let events = drive_conversation(
        test.client(),
        session_id,
        &["Answer this question."],
        ConversationOptions {
            // The gateway's five bounded attempts back off 1s/2s/4s/8s before the
            // run entry completes terminally, so a failing turn settles far
            // slower than a succeeding one.
            turn_timeout: Duration::from_secs(240),
            ..ConversationOptions::default()
        },
    )
    .await
    .expect("drive the failing turn to a settled session");

    let failures = turn_failures(&events);
    assert_eq!(
        failures.len(),
        1,
        "a failed root turn records exactly one canonical failure fact; got {failures:#?}"
    );

    let Event::TurnFailed {
        actor,
        turn_id,
        class,
        summary,
    } = failures[0]
    else {
        unreachable!("filtered to TurnFailed above");
    };

    assert_eq!(
        actor,
        &TurnFailureActor::Coordinator,
        "a root turn failure is attributed to the coordinator, not a worker"
    );
    assert!(
        !turn_id.is_empty(),
        "the failure fact names the turn it belongs to"
    );
    assert_eq!(
        summary,
        class.summary(),
        "the persisted summary is the fixed sentence for its class, not caller-supplied text"
    );
    for marker in RAW_ERROR_MARKERS {
        assert!(
            !summary.contains(marker),
            "raw error material {marker:?} leaked into the durable summary: {summary}"
        );
    }

    assert_eq!(
        session_status(test.client(), session_id).await,
        SessionStatus::Failed,
        "a coordinator turn that failed leaves the session Failed"
    );
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA testcontainers fixture"]
async fn replayed_failing_turn_does_not_duplicate_its_failure_fact_service_e2e() {
    // Pins: the failure fact is keyed by actor plus turn, not by the workflow's
    // append sequence. The gateway retries the model call five times with backoff,
    // so the turn workflow suspends and replays its journal repeatedly before it
    // ever reaches the catch-all. A sequence-derived key would then append a
    // second copy on replay, and an operator counting failures would double-count
    // every failed turn.
    let fixture = OrchestratorTestFixture::with_script(always_failing_script())
        .await
        .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("replayed-turn-terminal-failure")
        .await
        .expect("create + init session");

    let events = drive_conversation(
        test.client(),
        session_id,
        &["Answer this question."],
        ConversationOptions {
            turn_timeout: Duration::from_secs(240),
            ..ConversationOptions::default()
        },
    )
    .await
    .expect("drive the failing turn to a settled session");

    let failures = turn_failures(&events);
    assert_eq!(
        failures.len(),
        1,
        "journal replay across the gateway's bounded retries must not duplicate the fact"
    );

    // History-first recovery ordering: the failure is recorded against the turn it
    // belongs to, after that turn's user message, and the turn produces no
    // assistant reply. A fact that landed before its own user message, or a reply
    // emitted alongside a failure, would both mean the log no longer reconstructs
    // what actually happened.
    let failure_seq = events
        .iter()
        .find(|record| matches!(record.event, Event::TurnFailed { .. }))
        .map(|record| record.sequence_num)
        .expect("the failure fact is present");
    let user_message_seq = events
        .iter()
        .find(|record| matches!(record.event, Event::UserMessage { .. }))
        .map(|record| record.sequence_num)
        .expect("the turn recorded its user message");
    assert!(
        failure_seq > user_message_seq,
        "the failure fact belongs to its own turn's history \
         (user message at {user_message_seq}, failure at {failure_seq})"
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. })),
        "a turn that never reached the model must not record an assistant reply"
    );
}

/// Sentinel that appears only in the delegating user message.
const DELEGATION_REQUEST: &str = "Delegate the ZZQQ-AUDIT workstream to a child.";
/// Sentinel that appears only in the child's own instruction.
const WORKER_TASK: &str = "ZZQQ-AUDIT child instruction: audit the ledger and report.";
/// Sentinel emitted by the coordinator once the spawn tool result comes back.
const ROOT_FINAL_TEXT: &str = "ZZQQ-AUDIT delegation dispatched.";
/// Stable substring of the route-classifier system prompt.
const CLASSIFIER_PROMPT: &str = "You classify one user turn into MOA's public execution decision.";

/// Keyed script that lets the coordinator succeed while the child's turn fails.
///
/// Keyed entries match against joined System/User/Tool message text, are never
/// consumed, and resolve first-match-wins in registration order. That ordering is
/// load-bearing here:
///
/// 1. the child's instruction faults, so only the worker turn reaches its catch-all;
/// 2. the spawn tool result ends the coordinator's loop — registered before the
///    user-message entry so the follow-up iteration, whose messages contain both,
///    does not spawn a second child and loop forever;
/// 3. the classifier prompt (which embeds the user message) resolves to a route
///    before the user-message entry can hand it a tool call;
/// 4. the user message spawns exactly one child.
///
/// The fallback succeeds so best-effort auxiliary calls never masquerade as the
/// failure under test.
fn worker_only_failure_script() -> serde_json::Value {
    json!({
        "default": { "completion": { "content": "OK", "tool_calls": [] } },
        "keyed": [
            {
                "match": WORKER_TASK,
                "completion": {
                    "content": "unreachable",
                    "tool_calls": [],
                    "fault": { "fail_first_n": 100, "status": 500 }
                }
            },
            {
                "match": "worker_id",
                "completion": { "content": ROOT_FINAL_TEXT, "tool_calls": [] }
            },
            {
                "match": CLASSIFIER_PROMPT,
                "completion": {
                    "content": r#"{"label":"execute","strategy":"inline","rationale":"The turn delegates bounded work.","confidence_bps":9500,"missing_inputs":[]}"#,
                    "tool_calls": []
                }
            },
            {
                "match": DELEGATION_REQUEST,
                "completion": {
                    "content": "",
                    "tool_calls": [{
                        "name": "spawn_worker",
                        "id": "zzqq-audit-spawn",
                        "input": {
                            "task": WORKER_TASK,
                            "tool_subset": [],
                            "budget_tokens": 1200,
                            "max_turns": 1
                        }
                    }]
                }
            }
        ]
    })
}

#[tokio::test]
#[ignore = "requires Docker for the Postgres/Restate/OpenFGA testcontainers fixture"]
async fn worker_turn_catch_all_records_a_neutral_failure_service_e2e() {
    // Pins: a child turn that dies at its catch-all records its own canonical
    // failure fact in the PARENT's log, attributed to the worker, without
    // dragging the parent's scheduling state down with it. The plan's exact
    // requirement is that child failure facts cannot mask root scheduling state:
    // if a worker failure were scheduling-terminal, or if it settled the session
    // as Failed, a coordinator that still owes the user a reply would stall.
    let fixture = OrchestratorTestFixture::with_script(worker_only_failure_script())
        .await
        .expect("boot scripted orchestrator fixture");

    let test = fixture.isolated().await;
    let session_id = test
        .create_session("worker-turn-terminal-failure")
        .await
        .expect("create + init session");

    let events = drive_conversation(
        test.client(),
        session_id,
        &[DELEGATION_REQUEST],
        ConversationOptions {
            turn_timeout: Duration::from_secs(240),
            ..ConversationOptions::default()
        },
    )
    .await
    .expect("drive the delegating turn and its failing child to a settled session");

    // The failed child's terminal lifecycle delivery is a detached chain that can
    // land after the failed-attention resume settles the conversation; wait for it
    // before asserting anything about the final log.
    let events = wait_for_session_events(test.client(), session_id, events, |events| {
        events
            .iter()
            .any(|record| matches!(record.event, Event::WorkerStatusChanged { .. }))
            && events
                .iter()
                .any(|record| matches!(record.event, Event::WorkerNotificationDelivered { .. }))
    })
    .await;

    let failures = turn_failures(&events);
    assert_eq!(
        failures.len(),
        1,
        "the child's failure is recorded exactly once, and the coordinator's own \
         turn contributes no failure fact; got {failures:#?}"
    );

    let Event::TurnFailed {
        actor,
        turn_id,
        class,
        summary,
    } = failures[0]
    else {
        unreachable!("filtered to TurnFailed above");
    };

    let worker_id = match actor {
        TurnFailureActor::Worker { worker_id } => worker_id.clone(),
        TurnFailureActor::Coordinator => {
            panic!("the coordinator's turn succeeded; the failure belongs to the child")
        }
    };
    assert!(
        !worker_id.is_empty(),
        "the failure fact names the child it belongs to"
    );
    assert!(
        !turn_id.is_empty(),
        "the failure fact names the child turn it belongs to"
    );
    assert_eq!(
        summary,
        class.summary(),
        "the persisted summary is the fixed sentence for its class, not caller-supplied text"
    );
    for marker in RAW_ERROR_MARKERS {
        assert!(
            !summary.contains(marker),
            "raw error material {marker:?} leaked into the durable summary: {summary}"
        );
    }

    // Scheduling-neutral: the child died, the coordinator did not.
    assert_ne!(
        session_status(test.client(), session_id).await,
        SessionStatus::Failed,
        "a child's failure must not settle the owning session as Failed"
    );
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::BrainResponse { text, .. } if text.contains(ROOT_FINAL_TEXT)
        )),
        "the coordinator completed its own turn after the child failed"
    );

    // Existing worker attention and lifecycle facts coexist with the terminal
    // fact and are counted separately: the canonical failure never replaces them,
    // and they never stand in for it.
    let signals = events
        .iter()
        .filter(|record| matches!(record.event, Event::WorkerSignalReceived { .. }))
        .count();
    let status_changes = events
        .iter()
        .filter(|record| matches!(record.event, Event::WorkerStatusChanged { .. }))
        .count();
    assert!(
        signals > 0,
        "the child's Failed attention signal still reaches the coordinator"
    );
    assert!(
        status_changes > 0,
        "the child's lifecycle delivery events are still recorded"
    );
}

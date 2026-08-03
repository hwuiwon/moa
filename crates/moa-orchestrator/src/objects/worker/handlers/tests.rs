//! Unit tests for Worker handler decisions and journal payloads.

use super::{
    CleanupDecision, JournaledWorkerToolCatalog, activate_worker_security_owner, decide_cleanup,
    release_worker_hands_request,
};
use crate::action_reviews::scheduling::QueuedActionReviewContinuation;
use crate::objects::worker::WorkerVoState;
use moa_core::types::action_policy::{
    ActionReviewContinuation, ActionReviewOutcome, ActionReviewOwner, ActionReviewReceipt,
};
use moa_core::types::identifiers::{SessionId, ToolCallId};
use moa_core::types::security::SecurityCircuitOwner;
use moa_core::types::worker::state::WorkerState;
use uuid::Uuid;

#[test]
fn worker_tool_catalog_journal_payload_round_trips() {
    // Pins: the external session/authz catalog read journals only stable,
    // secret-free schemas and the exact execution pin used by the turn.
    let payload = JournaledWorkerToolCatalog {
        tool_schemas: vec![serde_json::json!({
            "name": "connector_action",
            "input_schema": {"type": "object"}
        })],
        tool_catalog_pin: moa_hands::ToolCatalogPin {
            contract_hash: "catalog-contract".to_string(),
            mcp_catalog_revision: "connector-revision".to_string(),
            tools: Vec::new(),
        },
    };

    let encoded = serde_json::to_value(&payload).expect("serialize worker catalog journal");
    let decoded: JournaledWorkerToolCatalog =
        serde_json::from_value(encoded).expect("deserialize worker catalog journal");

    assert_eq!(decoded, payload);
}

#[test]
fn worker_turn_admission_installs_the_security_owner() {
    // Pins: a worker assessment can only mutate the exact owner installed
    // before its turn workflow was dispatched.
    let mut state = WorkerVoState::default();

    activate_worker_security_owner(&mut state, "worker-3", "turn-9", 4);

    assert_eq!(
        state.security_circuit.owner,
        Some(SecurityCircuitOwner::Worker {
            worker_id: "worker-3".to_string(),
            turn_id: "turn-9".to_string(),
            generation: 4,
        })
    );
}

fn continuation_for(
    review_id: Uuid,
    session_id: SessionId,
    worker_id: &str,
    generation: u64,
) -> ActionReviewContinuation {
    ActionReviewContinuation {
        receipt: ActionReviewReceipt {
            review_id,
            owner: ActionReviewOwner::Worker {
                session_id,
                worker_id: worker_id.to_string(),
                turn_id: format!("{worker_id}-turn-1"),
                generation,
            },
            tool_name: "bash".to_string(),
            executed_tool_call_id: Some(ToolCallId::new()),
            outcome: ActionReviewOutcome::Cleared(
                moa_core::types::action_policy::ToolTerminalFact::Result(
                    moa_core::types::action_policy::ToolResultSecurityMetadata {
                        success: true,
                        assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                        capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
                    },
                ),
            ),
        },
    }
}

#[test]
fn pending_worker_reviews_hold_lifecycle_until_ordered_continuations_finish() {
    // Pins: a worker whose model loop ended while its own actions await tenant-admin
    // decisions is NOT finished. While any current-generation review is unresolved it
    // must stay nonterminal — no parent-result delivery, no cleanup — and its two
    // reviews must continue in the order they were raised, not the order the admin
    // happened to decide them. Only after the last continuation is drained does the
    // worker become eligible to report and self-clean.
    let session_id = SessionId::new();
    let worker_id = "worker-lifecycle-hold-1";
    let first_review = Uuid::from_u128(0x13_1001);
    let second_review = Uuid::from_u128(0x13_1002);
    let mut state = WorkerVoState {
        status: Some(WorkerState::Completed),
        parent_session: Some(session_id),
        notification_delivered: false,
        ..WorkerVoState::default()
    };
    let generation = state.advance_generation();

    assert!(state.register_action_review(first_review, format!("{worker_id}-turn-1"), generation));
    assert!(state.register_action_review(second_review, format!("{worker_id}-turn-1"), generation));
    assert!(
        state.action_review_holds_lifecycle(),
        "an unresolved current-generation review keeps the worker nonterminal"
    );
    assert_eq!(
        decide_cleanup(
            true,
            crate::delegation::is_terminal_worker_state(state.current_status())
                && !state.action_review_holds_lifecycle(),
            false,
            true,
        ),
        CleanupDecision::Skip,
        "cleanup must not clear the local history the continuation still needs"
    );

    // The admin decides the second review first; registration order still wins.
    let second = state
        .resolve_action_review(second_review)
        .expect("second review is registered");
    assert!(
        state.queue_action_review_continuation(QueuedActionReviewContinuation {
            continuation: continuation_for(second_review, session_id, worker_id, generation),
            turn_id: format!("{worker_id}-continuation-2"),
            generation: second.generation,
            ordinal: second.ordinal,
        })
    );
    let first = state
        .resolve_action_review(first_review)
        .expect("first review is registered");
    assert!(
        state.queue_action_review_continuation(QueuedActionReviewContinuation {
            continuation: continuation_for(first_review, session_id, worker_id, generation),
            turn_id: format!("{worker_id}-continuation-1"),
            generation: first.generation,
            ordinal: first.ordinal,
        })
    );
    assert!(
        state.action_review_holds_lifecycle(),
        "a queued continuation still holds the worker open"
    );

    assert_eq!(
        state
            .take_action_review_continuation()
            .map(|entry| entry.continuation.receipt.review_id),
        Some(first_review),
        "continuations run in durable registration order"
    );
    assert!(state.action_review_holds_lifecycle());
    assert_eq!(
        state
            .take_action_review_continuation()
            .map(|entry| entry.continuation.receipt.review_id),
        Some(second_review)
    );

    assert!(
        !state.action_review_holds_lifecycle(),
        "the worker is releasable once its last continuation is drained"
    );
    assert_eq!(
        decide_cleanup(
            true,
            crate::delegation::is_terminal_worker_state(state.current_status())
                && !state.action_review_holds_lifecycle(),
            false,
            true,
        ),
        CleanupDecision::Proceed
    );
}

#[test]
fn a_worker_follow_up_supersedes_and_releases_an_older_action_review() {
    // Pins: an unresolved review must never pin a worker forever. New parent
    // instructions advance the generation, strand the older review, and release the
    // lifecycle the review was holding — so a late approval cannot preempt the newer
    // work or resurrect a worker that has moved on.
    let session_id = SessionId::new();
    let review_id = Uuid::from_u128(0x13_1003);
    let mut state = WorkerVoState {
        status: Some(WorkerState::Completed),
        parent_session: Some(session_id),
        ..WorkerVoState::default()
    };
    let generation = state.advance_generation();
    state.register_action_review(review_id, "worker-supersede-turn-1".to_string(), generation);
    assert!(state.action_review_holds_lifecycle());

    let newer = state.advance_generation();
    assert_eq!(newer, 2);
    assert!(
        !state.action_review_holds_lifecycle(),
        "a superseded review no longer holds the worker open"
    );
    assert!(
        state.resolve_action_review(review_id).is_none(),
        "the superseded registration is gone, so its callback is a no-op"
    );
}

#[test]
fn cleanup_release_request_targets_owning_session_and_child() {
    // Pins: a finishing worker's hand release is keyed by its OWNING session id
    // (where its hands were provisioned) and its own id, so cleanup frees exactly its
    // own scope; a child with no owning session issues no release.
    let session_id = SessionId::new();
    let request = release_worker_hands_request(Some(session_id), "sub-7")
        .expect("a child with a parent session releases its scoped hands");
    assert_eq!(request.session_id, session_id);
    assert_eq!(request.worker_id, "sub-7");
    assert!(
        release_worker_hands_request(None, "sub-7").is_none(),
        "a child with no owning session issues no hand release"
    );
}

#[test]
fn cleanup_skips_on_stale_generation() {
    // Pins: a fired cleanup whose generation no longer matches (the child was revived
    // or rescheduled during the grace window) is a no-op, never tearing down.
    assert_eq!(
        decide_cleanup(false, true, false, true),
        CleanupDecision::Skip
    );
}

#[test]
fn cleanup_skips_when_revived_to_non_terminal() {
    // Pins: a child that a follow-up revived back to Running is not terminal, so
    // cleanup must skip even when the generation still matches.
    assert_eq!(
        decide_cleanup(true, false, false, true),
        CleanupDecision::Skip
    );
}

#[test]
fn cleanup_defers_while_non_terminal_child_exists() {
    // Pins: teardown is bottom-up; a terminal parent with a still-running child
    // reschedules rather than clearing.
    assert_eq!(
        decide_cleanup(true, true, true, true),
        CleanupDecision::Defer
    );
}

#[test]
fn cleanup_skips_when_report_not_durable() {
    // Pins: the durable-report guard — cleanup never clears a terminal leaf whose
    // result was not yet recorded on the parent.
    assert_eq!(
        decide_cleanup(true, true, false, false),
        CleanupDecision::Skip
    );
}

#[test]
fn cleanup_proceeds_on_durable_terminal_leaf() {
    // Pins: a terminal leaf with a durable report and a live generation is released.
    assert_eq!(
        decide_cleanup(true, true, false, true),
        CleanupDecision::Proceed
    );
}

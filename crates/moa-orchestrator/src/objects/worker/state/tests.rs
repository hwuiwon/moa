//! Unit tests for the durable Worker VO state families.

use moa_core::{
    traits::{Identity, IdentityType},
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::identifiers::TenantId,
    types::identifiers::UserId,
    types::session::TurnOutcome,
    types::worker::state::WorkerInitialTask,
    types::worker::state::WorkerMessage,
};

use super::{
    ClaimedHistoryEntry, HISTORY_CLAIM_CHECK_THRESHOLD_BYTES, HISTORY_INLINE_TAIL,
    INPUT_DELIVERY_HISTORY_LIMIT, WORKER_BUDGET_EXHAUSTED_MESSAGE, WorkerHistoryEntry,
    WorkerVoState, latest_assistant_text,
};
use crate::objects::worker::UserReplyDeliveryAck;
use moa_core::{
    types::context::ContextMessage, types::context::MessageRole, types::events_stream::ClaimCheck,
    types::worker::state::WorkerInputTarget, types::worker::state::WorkerPendingInput,
    types::worker::state::WorkerState,
};

/// Builds one registration owned by an exact worker turn and generation.
fn pending_input(
    input_request_id: &str,
    turn_id: &str,
    generation: u64,
    awakeable_id: &str,
) -> WorkerPendingInput {
    WorkerPendingInput {
        turn_id: turn_id.to_string(),
        generation,
        input_request_id: input_request_id.to_string(),
        awakeable_id: awakeable_id.to_string(),
        waiting_workflow_id: turn_id.to_string(),
    }
}

fn initial_task() -> WorkerMessage {
    let tenant_id = TenantId::new();
    WorkerMessage::InitialTask(Box::new(WorkerInitialTask {
        task: "summarize repo status".to_string(),
        identity: Identity {
            identity_type: IdentityType::Operator,
            id: uuid::Uuid::now_v7(),
            tenant_id,
            api_key_id: Some(uuid::Uuid::now_v7()),
            acting_on_behalf_of: None,
        },
        tool_subset: vec!["web_fetch".to_string()],
        budget_tokens: 512,
        max_turns: Some(3),
        parent_session: SessionId::new(),
        depth: 1,
        tenant_id,
        user_id: UserId::new("user-1"),
        model: ModelId::new("test-model"),
        trusted_sandbox_manifest: None,
    }))
}

#[test]
fn initial_task_seeds_state() {
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");

    assert_eq!(state.current_status(), WorkerState::Running);
    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.tool_subset, vec!["web_fetch".to_string()]);
    assert_eq!(state.budget_remaining, 512);
    assert_eq!(state.max_turns, Some(3));
}

#[test]
fn initial_task_rejects_zero_max_turns() {
    // Pins: max_turns is a real execution cap and zero is never treated as unlimited.
    let mut message = initial_task();
    let WorkerMessage::InitialTask(initial) = &mut message else {
        panic!("helper should build initial task");
    };
    initial.max_turns = Some(0);

    let error = WorkerVoState::default()
        .initialize(&message)
        .expect_err("zero max_turns should fail closed");

    assert!(error.to_string().contains("max_turns must be at least 1"));
}

#[test]
fn follow_up_queues_message() {
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    state
        .enqueue_follow_up("continue".to_string())
        .expect("follow-up should queue");

    assert_eq!(state.pending.len(), 2);
    assert_eq!(state.pending[1].text, "continue");
}

#[test]
fn token_usage_reduces_budget() {
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    state.record_token_usage(200);

    assert_eq!(state.tokens_used, 200);
    assert_eq!(state.budget_remaining, 312);
    assert!(!state.budget_exhausted());
}

#[test]
fn exhausted_budget_completion_preserves_visible_result() {
    // Pins: budget-capped workers must return a useful terminal result, not the
    // previous progress summary such as "Calling tool session_search".
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    state.budget_remaining = 0;

    state.complete_after_budget_exhausted();
    let result = state.build_result("worker-1".to_string());

    assert!(result.success);
    assert_eq!(result.output, WORKER_BUDGET_EXHAUSTED_MESSAGE);
    assert_eq!(
        latest_assistant_text(&state.history).as_deref(),
        Some(WORKER_BUDGET_EXHAUSTED_MESSAGE)
    );
}

#[test]
fn build_result_uses_terminal_state() {
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    state.status = Some(WorkerState::Completed);
    state.last_turn_summary = Some("finished".to_string());
    let result = state.build_result("parent-1-child-1".to_string());

    assert!(result.success);
    assert_eq!(result.output, "finished");
}

#[test]
fn default_state_is_not_terminal_successful() {
    // Pins: an uninitialized Worker VO must not look like a completed child result.
    let state = WorkerVoState::default();

    assert!(
        !matches!(
            state.status_view().state,
            WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
        ),
        "default state should not be terminal, got {:?}",
        state.status_view().state
    );

    let result = state.build_result("uninitialized-child".to_string());
    assert!(
        !result.success,
        "uninitialized state must not build a successful terminal result"
    );
}

#[test]
fn terminal_result_requires_explicit_terminal_lifecycle() {
    // Pins: result success comes from an explicit terminal lifecycle, not from resident state.
    let mut running = WorkerVoState::default();
    running
        .initialize(&initial_task())
        .expect("initial task should seed running state");

    let running_result = running.build_result("running-child".to_string());
    assert!(!running_result.success);
    assert_eq!(
        running_result.error.as_deref(),
        Some("worker finished before reaching a terminal state")
    );

    let mut completed = WorkerVoState::default();
    completed
        .initialize(&initial_task())
        .expect("initial task should seed completed state");
    completed.last_turn_summary = Some("finished".to_string());
    completed.apply_turn_outcome(TurnOutcome::Idle);
    let completed_result = completed.build_result("completed-child".to_string());
    assert!(completed_result.success);
    assert_eq!(completed_result.output, "finished");
    assert_eq!(completed_result.error, None);
}

#[test]
fn task_hash_uses_shared_dispatch_hash() {
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");

    assert_eq!(state.task_hash(), "c024b456687bf734");
}

#[test]
fn workflow_turn_ownership_is_single_active_id() {
    // Pins: worker workflow admission keeps exactly one active turn owner.
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");

    assert!(state.start_workflow_turn("turn-1".to_string()));
    assert!(!state.start_workflow_turn("turn-2".to_string()));
    assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));
}

#[test]
fn workflow_turn_clear_requires_matching_owner() {
    // Pins: stale workflow completions cannot clear a newer active worker turn.
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    assert!(state.start_workflow_turn("turn-1".to_string()));

    assert!(!state.clear_active_turn("turn-2"));
    assert_eq!(state.active_turn_id.as_deref(), Some("turn-1"));
    assert!(state.clear_active_turn("turn-1"));
    assert_eq!(state.active_turn_id, None);

    assert!(state.start_workflow_turn("turn-2".to_string()));
    assert!(!state.clear_active_turn("turn-1"));
    assert_eq!(state.active_turn_id.as_deref(), Some("turn-2"));
}

#[test]
fn progress_summary_reports_state_and_heartbeat_fields() {
    // Pins: the compact fan-in summary carries the child's live state, last summary,
    // budget, and heartbeat, and derives staleness from the heartbeat age.
    use chrono::{Duration, Utc};

    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");
    state.record_token_usage(100);
    state.last_turn_summary = Some("searching docs".to_string());
    state.active_turn_id = Some("turn-1".to_string());
    let now = Utc::now();
    let heartbeat = now - Duration::milliseconds(5_000);
    state.last_heartbeat_at = Some(heartbeat);

    let fresh = state.progress_summary("child-1".to_string(), now, 60_000);
    assert_eq!(fresh.worker_id, "child-1");
    assert_eq!(fresh.state, WorkerState::Running);
    assert_eq!(fresh.active_turn_id.as_deref(), Some("turn-1"));
    assert_eq!(fresh.last_summary.as_deref(), Some("searching docs"));
    assert_eq!(fresh.tokens_used, 100);
    assert_eq!(fresh.budget_remaining, 412);
    assert_eq!(fresh.last_heartbeat_at, Some(heartbeat));
    assert!(!fresh.stale, "a recent heartbeat must not be stale");

    // A heartbeat older than the threshold flips the stale flag.
    let stale = state.progress_summary("child-1".to_string(), now, 1_000);
    assert!(stale.stale, "an aged heartbeat must be stale");

    // No heartbeat yet is never stale.
    state.last_heartbeat_at = None;
    let no_heartbeat = state.progress_summary("child-1".to_string(), now, 1);
    assert!(!no_heartbeat.stale);
    assert_eq!(no_heartbeat.last_heartbeat_at, None);
    // No pending input request: the child is not awaiting input.
    assert!(!no_heartbeat.awaiting_input);

    // A pending request_input round-trip surfaces awaiting_input so the watchdog can
    // exempt the child even with an aged (or absent) heartbeat.
    state.register_input_request(pending_input("req-1", "worker-turn-1", 1, "awk-1"));
    let awaiting = state.progress_summary("child-1".to_string(), now, 1);
    assert!(
        awaiting.awaiting_input,
        "a pending request_input must surface awaiting_input"
    );
}

#[test]
fn cleaned_state_rejects_follow_up_but_terminal_child_is_revivable() {
    // Pins: a follow-up to a cleaned (cleared) VO must be rejected, while a
    // still-initialized terminal child within the grace window stays revivable.
    let cleaned = WorkerVoState::default();
    assert!(
        !cleaned.accepts_follow_up(),
        "a cleared/uninitialized child must not accept follow-ups"
    );

    let mut terminal = WorkerVoState::default();
    terminal
        .initialize(&initial_task())
        .expect("initial task should seed state");
    terminal.apply_turn_outcome(TurnOutcome::Idle);
    assert_eq!(terminal.current_status(), WorkerState::Completed);
    assert!(
        terminal.accepts_follow_up(),
        "a terminal-but-not-cleaned child must still be revivable"
    );
}

#[test]
fn accepted_message_bumps_cleanup_generation_invalidating_pending_cleanup() {
    // Pins: a message arriving during the grace window bumps cleanup_generation so a
    // cleanup tick scheduled for the prior generation is recognized as stale.
    let mut state = WorkerVoState::default();
    state
        .initialize(&initial_task())
        .expect("initial task should seed state");

    // Terminal delivery schedules cleanup for this generation.
    state.bump_cleanup_generation();
    let scheduled_generation = state.cleanup_generation;

    // A revive follow-up arriving mid-grace bumps the generation again.
    state.bump_cleanup_generation();

    assert_ne!(
        scheduled_generation, state.cleanup_generation,
        "an accepted message must supersede the pending cleanup generation"
    );
}

#[test]
fn bump_cleanup_generation_resets_release_attempts() {
    // Pins: a fresh cleanup cycle (or a revive) starts with a clean release-attempt
    // budget, so a stale counter from a prior cycle cannot prematurely force-clear.
    let mut state = WorkerVoState {
        cleanup_release_attempts: super::MAX_CLEANUP_RELEASE_ATTEMPTS - 1,
        ..WorkerVoState::default()
    };
    state.bump_cleanup_generation();
    assert_eq!(state.cleanup_release_attempts, 0);
}

#[test]
fn pending_input_request_clear_removes_only_the_registering_invocations_entry() {
    // Pins: register stores one awakeable per input_request_id (idempotent); a timing-out
    // invocation clears only the registration IT owns, so a different owner's live
    // round-trip and a replacement registered by a retry both survive its clear.
    let mut state = WorkerVoState::default();
    assert!(state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awk-1")));
    // Duplicate registration of the same request id is a no-op.
    assert!(!state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awk-1b")));
    assert!(state.register_input_request(pending_input("req-2", "worker-turn-2", 4, "awk-2")));

    let owned = WorkerInputTarget {
        turn_id: "worker-turn-1".to_string(),
        generation: 3,
        input_request_id: "req-1".to_string(),
    };
    // A clear naming another owner's coordinates removes nothing.
    for foreign in [
        WorkerInputTarget {
            input_request_id: "req-2".to_string(),
            ..owned.clone()
        },
        WorkerInputTarget {
            generation: 4,
            ..owned.clone()
        },
        WorkerInputTarget {
            turn_id: "worker-turn-2".to_string(),
            ..owned.clone()
        },
    ] {
        assert!(
            state
                .clear_input_request_for_workflow(&foreign, "worker-turn-1")
                .is_none(),
            "a clear must not remove a target it does not own: {foreign:?}"
        );
    }
    // Nor does the right target cleared by the wrong waiting invocation.
    assert!(
        state
            .clear_input_request_for_workflow(&owned, "worker-turn-1-retry")
            .is_none(),
        "only the invocation parked on the awakeable may retract it"
    );
    assert_eq!(state.pending_input_requests.len(), 2);

    let cleared = state
        .clear_input_request_for_workflow(&owned, "worker-turn-1")
        .expect("the registering invocation clears its own target");
    assert_eq!(cleared.awakeable_id, "awk-1");
    assert_eq!(cleared.target(), owned);
    assert_eq!(state.pending_input_requests.len(), 1);
    assert_eq!(state.pending_input_requests[0].input_request_id, "req-2");
    // A second clear of the same target is an idempotent no-op.
    assert!(
        state
            .clear_input_request_for_workflow(&owned, "worker-turn-1")
            .is_none()
    );
}

#[test]
fn worker_input_clear_by_turn_and_by_worker_retract_exactly_their_scope() {
    // Pins: a turn that reported its outcome takes only its own registrations down,
    // and the whole-worker clear (cancellation, terminal outcome) returns every
    // remaining one so each advertised reply target can be retracted.
    let mut state = WorkerVoState::default();
    state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awk-1"));
    state.register_input_request(pending_input("req-2", "worker-turn-1", 3, "awk-2"));
    state.register_input_request(pending_input("req-3", "worker-turn-2", 4, "awk-3"));

    let dead_turn = state.clear_input_requests_for_turn("worker-turn-1");
    assert_eq!(
        dead_turn
            .iter()
            .map(|entry| entry.input_request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["req-1", "req-2"]
    );
    assert_eq!(state.pending_input_requests.len(), 1);
    assert_eq!(state.pending_input_requests[0].input_request_id, "req-3");

    let remaining = state.clear_all_input_requests();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].target().turn_id, "worker-turn-2");
    assert!(state.pending_input_requests.is_empty());
    assert!(state.clear_all_input_requests().is_empty());
}

#[test]
fn worker_input_delivery_distinguishes_apply_replay_conflict_and_unknown() {
    // Pins: only the first exact pending reply applies; identical retries replay while a
    // changed duplicate or unknown request conflicts without resolving another awakeable.
    let mut state = WorkerVoState::default();
    assert!(state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awake-1")));
    let reply = serde_json::Value::String("answer".to_string());
    let (acknowledgement, applied) = state
        .apply_input_reply("req-1", &reply)
        .expect("first exact reply should apply");
    assert_eq!(acknowledgement, UserReplyDeliveryAck::Applied);
    assert_eq!(
        applied.map(|entry| entry.awakeable_id),
        Some("awake-1".to_string())
    );
    assert_eq!(state.input_delivery_history.len(), 1);
    assert_eq!(
        state.input_delivery_history[0].acknowledgement,
        UserReplyDeliveryAck::Applied
    );
    assert_eq!(
        state
            .apply_input_reply("req-1", &reply)
            .expect("identical duplicate should replay"),
        (UserReplyDeliveryAck::Replayed, None)
    );
    assert_eq!(
        state
            .apply_input_reply("req-1", &serde_json::Value::String("changed".to_string()),)
            .expect("changed duplicate should return a typed conflict"),
        (UserReplyDeliveryAck::Conflict, None)
    );
    assert_eq!(
        state
            .apply_input_reply("unknown", &serde_json::Value::String("answer".to_string()),)
            .expect("unknown request should return a typed conflict"),
        (UserReplyDeliveryAck::Conflict, None)
    );
    assert_eq!(state.input_delivery_history.len(), 1);
}

#[test]
fn worker_user_input_reply_requires_the_exact_owner() {
    // Pins: a user-addressed reply resolves only the round-trip whose turn AND
    // generation it names; an answer written for superseded work must conflict
    // rather than unblock whatever currently holds that request id.
    let mut state = WorkerVoState::default();
    state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awake-first"));
    let reply = serde_json::Value::String("answer".to_string());

    for stale in [
        WorkerInputTarget {
            turn_id: "worker-turn-1".to_string(),
            generation: 2,
            input_request_id: "req-1".to_string(),
        },
        WorkerInputTarget {
            turn_id: "worker-turn-0".to_string(),
            generation: 3,
            input_request_id: "req-1".to_string(),
        },
    ] {
        assert_eq!(
            state
                .apply_user_input_reply(&stale, &reply)
                .expect("a superseded owner is a typed conflict"),
            (UserReplyDeliveryAck::Conflict, None),
            "reply matching must fence on the exact owner: {stale:?}"
        );
    }
    assert_eq!(
        state.pending_input_requests.len(),
        1,
        "a conflicting reply must leave the live registration untouched"
    );

    let owner = WorkerInputTarget {
        turn_id: "worker-turn-1".to_string(),
        generation: 3,
        input_request_id: "req-1".to_string(),
    };
    let (acknowledgement, applied) = state
        .apply_user_input_reply(&owner, &reply)
        .expect("the exact owner applies");
    assert_eq!(acknowledgement, UserReplyDeliveryAck::Applied);
    assert_eq!(
        applied.map(|entry| entry.awakeable_id),
        Some("awake-first".to_string())
    );

    let (late, resolved) = state
        .apply_user_input_reply(&owner, &reply)
        .expect("a late duplicate of a delivered reply is a replay");
    assert_eq!(late, UserReplyDeliveryAck::Replayed);
    assert_eq!(resolved, None);
}

#[test]
fn a_late_worker_input_duplicate_cannot_resolve_a_replacement_awakeable() {
    // Pins: delivery history outlives the awakeable it retired. The coordinator
    // path matches on the request id alone, so history is the ONLY thing between a
    // late duplicate and the *replacement* round-trip a newer turn registered under
    // that id — consulting the pending registrations first would unblock the newer
    // turn with an answer written for the one it replaced.
    let mut state = WorkerVoState::default();
    state.register_input_request(pending_input("req-1", "worker-turn-1", 3, "awake-first"));
    let reply = serde_json::Value::String("answer".to_string());
    let (acknowledgement, applied) = state
        .apply_input_reply("req-1", &reply)
        .expect("the pending reply applies");
    assert_eq!(acknowledgement, UserReplyDeliveryAck::Applied);
    assert_eq!(
        applied.map(|entry| entry.awakeable_id),
        Some("awake-first".to_string())
    );

    // A newer worker turn re-registers the same request id on a fresh awakeable.
    state.register_input_request(pending_input(
        "req-1",
        "worker-turn-2",
        4,
        "awake-replacement",
    ));

    let (late, resolved) = state
        .apply_input_reply("req-1", &reply)
        .expect("a late duplicate replays against delivery history");
    assert_eq!(late, UserReplyDeliveryAck::Replayed);
    assert_eq!(
        resolved, None,
        "a replayed duplicate must resolve no awakeable at all"
    );
    assert_eq!(
        state.pending_input_requests.len(),
        1,
        "the replacement awakeable must still be waiting for its own answer"
    );
    assert_eq!(
        state.pending_input_requests[0].awakeable_id,
        "awake-replacement"
    );
}

#[test]
fn worker_input_parent_scope_fails_closed_on_missing_or_mismatched_owner() {
    // Pins: authorization of a caller-supplied Session is insufficient unless the loaded
    // Worker state names that exact Session as its owning parent.
    let owning_session = SessionId::new();
    let different_session = SessionId::new();
    let mut state = WorkerVoState {
        parent_session: Some(owning_session),
        ..WorkerVoState::default()
    };

    state
        .ensure_parent_session_scope(owning_session)
        .expect("exact owning Session should be accepted");
    let mismatch = state
        .ensure_parent_session_scope(different_session)
        .expect_err("different authorized Session must fail closed");
    let mismatch: &(dyn std::error::Error + Send + Sync) = mismatch.as_ref();
    assert_eq!(
        mismatch.to_string(),
        "Terminal error [403]: worker parent session scope mismatch"
    );

    state.parent_session = None;
    let missing = state
        .ensure_parent_session_scope(owning_session)
        .expect_err("uninitialized Worker scope must fail closed");
    let missing: &(dyn std::error::Error + Send + Sync) = missing.as_ref();
    assert_eq!(
        missing.to_string(),
        "Terminal error [403]: worker parent session scope mismatch"
    );
}

#[test]
fn worker_input_delivery_history_evicts_oldest_after_128_entries() {
    // Pins: replay state is bounded and ordered; the 129th applied reply evicts only the
    // oldest request while the newest 128 remain exact-replayable.
    let mut state = WorkerVoState::default();
    for index in 0..=INPUT_DELIVERY_HISTORY_LIMIT {
        let input_request_id = format!("req-{index:03}");
        assert!(state.register_input_request(pending_input(
            &input_request_id,
            "worker-turn-1",
            1,
            &format!("awake-{index:03}"),
        )));
        assert_eq!(
            state
                .apply_input_reply(
                    &input_request_id,
                    &serde_json::Value::String(format!("reply-{index:03}")),
                )
                .expect("pending reply should apply")
                .0,
            UserReplyDeliveryAck::Applied
        );
    }

    assert_eq!(
        state.input_delivery_history.len(),
        INPUT_DELIVERY_HISTORY_LIMIT
    );
    assert_eq!(state.input_delivery_history[0].input_request_id, "req-001");
    assert_eq!(
        state
            .input_delivery_history
            .last()
            .expect("bounded history should retain a newest entry")
            .input_request_id,
        "req-128"
    );
    assert_eq!(
        state
            .apply_input_reply(
                "req-000",
                &serde_json::Value::String("reply-000".to_string()),
            )
            .expect("evicted request should be unknown"),
        (UserReplyDeliveryAck::Conflict, None)
    );
    assert_eq!(
        state
            .apply_input_reply(
                "req-128",
                &serde_json::Value::String("reply-128".to_string()),
            )
            .expect("newest request should remain replayable"),
        (UserReplyDeliveryAck::Replayed, None)
    );
}

#[test]
fn result_waiters_are_unique_and_take_clears_registry() {
    // Pins: wait timeouts cannot accumulate duplicate result awakeables.
    let mut state = WorkerVoState::default();

    assert!(state.add_result_waiter("awake-1".to_string()));
    assert!(!state.add_result_waiter("awake-1".to_string()));
    assert!(state.add_result_waiter("awake-2".to_string()));
    assert_eq!(
        state.take_result_waiters(),
        vec!["awake-1".to_string(), "awake-2".to_string()]
    );
    assert!(state.result_waiters.is_empty());
}

#[test]
fn history_claim_check_selects_only_large_aged_out_entries() {
    // Pins: the claim-check sweep offloads only inline entries older than the inline tail
    // whose serialized body exceeds the threshold; sub-threshold entries and every entry
    // inside the hot tail (even a large one) stay inline so the next turn never hydrates.
    let mut state = WorkerVoState::default();
    let big = "x".repeat(HISTORY_CLAIM_CHECK_THRESHOLD_BYTES + 100);
    let small = "small".to_string();
    // idx 0: large + aged out -> the only candidate.
    state
        .history
        .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
            "t0",
            big.clone(),
            None,
        )));
    // idx 1: small + aged out -> below threshold, not a candidate.
    state
        .history
        .push(WorkerHistoryEntry::inline(ContextMessage::assistant(
            small.clone(),
        )));
    // Fill the inline tail; its first entry is large but must stay inline (hot tail).
    for i in 0..HISTORY_INLINE_TAIL {
        let text = if i == 0 { big.clone() } else { small.clone() };
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::assistant(text)));
    }

    let candidates = state
        .history_entries_to_claim_check()
        .expect("history entries serialize");
    assert_eq!(
        candidates.iter().map(|(idx, _)| *idx).collect::<Vec<_>>(),
        vec![0],
        "only the large aged-out entry is a claim-check candidate"
    );
    // A history no larger than the tail never offloads anything.
    let mut short = WorkerVoState::default();
    for _ in 0..HISTORY_INLINE_TAIL {
        short
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::assistant(
                big.clone(),
            )));
    }
    assert!(
        short
            .history_entries_to_claim_check()
            .expect("serialize")
            .is_empty(),
        "entries within the inline tail are never offloaded even when large"
    );
}

#[test]
fn claim_history_entry_replaces_inline_with_compact_reference() {
    // Pins: offloading an entry swaps the inline body for a compact reference that keeps
    // the role, blob id/size, and a non-empty content preview for fallbacks.
    let mut state = WorkerVoState::default();
    let body = "hello world tool output ".repeat(50);
    state
        .history
        .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
            "tool-1",
            body.clone(),
            None,
        )));
    let claim = ClaimCheck {
        blob_id: "blob-abc".to_string(),
        size: 4096,
        preview: "unused-store-preview".to_string(),
    };

    state.claim_history_entry(0, claim);

    match &state.history[0] {
        WorkerHistoryEntry::Claimed(claimed) => {
            assert_eq!(claimed.blob_id, "blob-abc");
            assert_eq!(claimed.size, 4096);
            assert_eq!(claimed.role, MessageRole::Tool);
            assert!(!claimed.preview.is_empty());
            assert!(
                body.starts_with(&claimed.preview),
                "preview is a prefix of the offloaded content"
            );
            assert!(claimed.token_estimate > 0);
        }
        other => panic!("expected a claimed entry, got {other:?}"),
    }
}

#[test]
fn history_entries_round_trip_through_json_with_references() {
    // Pins: a mix of inline and claim-checked slots survives K_HISTORY (de)serialization,
    // so a reloaded Worker VO reconstructs the buffered history losslessly.
    let history = vec![
        WorkerHistoryEntry::inline(ContextMessage::user("hi".to_string())),
        WorkerHistoryEntry::Claimed(ClaimedHistoryEntry {
            role: MessageRole::Tool,
            blob_id: "blob-xyz".to_string(),
            size: 20_000,
            preview: "preview text".to_string(),
            token_estimate: 5_000,
        }),
    ];

    let json = serde_json::to_string(&history).expect("history serializes");
    let decoded: Vec<WorkerHistoryEntry> =
        serde_json::from_str(&json).expect("history deserializes");
    assert_eq!(decoded, history);
}

#[test]
fn latest_assistant_text_falls_back_to_claimed_preview() {
    // Pins: the terminal-result fallback reads a claimed assistant entry's preview without
    // hydrating its blob, so a claim-checked final assistant turn still yields output.
    let history = vec![
        WorkerHistoryEntry::inline(ContextMessage::user("q".to_string())),
        WorkerHistoryEntry::Claimed(ClaimedHistoryEntry {
            role: MessageRole::Assistant,
            blob_id: "b".to_string(),
            size: 30_000,
            preview: "the answer preview".to_string(),
            token_estimate: 7_000,
        }),
    ];

    assert_eq!(
        latest_assistant_text(&history).as_deref(),
        Some("the answer preview")
    );
}

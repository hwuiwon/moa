//! Unit tests for Session durable state families.
use chrono::Utc;
use moa_core::{types::channel::Channel, types::identifiers::ModelId};

use super::{
    CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES, CoordinatorPendingInput, PendingUserReplyTarget,
    SessionVoState,
};
use moa_core::{
    types::events_stream::ClaimCheck, types::identifiers::AgentSignalId,
    types::worker::commands::MarkWorkerChildTerminalInput, types::worker::state::ChildSignalKind,
    types::worker::state::UnreadChildSignal, types::worker::state::WorkerChildRef,
    types::worker::state::WorkerInputTarget,
};

fn test_meta() -> moa_core::types::session::SessionMeta {
    moa_core::types::session::SessionMeta {
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        channel: Channel::Chat,
        model: ModelId::new("test-model"),
        ..moa_core::types::session::SessionMeta::default()
    }
}

fn worker_terminal(
    worker_id: &str,
    output: &str,
) -> moa_core::types::worker::state::WorkerTerminalResult {
    moa_core::types::worker::state::WorkerTerminalResult {
        state: moa_core::types::worker::state::WorkerState::Completed,
        result: moa_core::types::worker::state::WorkerResult {
            worker_id: worker_id.to_string(),
            success: true,
            output: output.to_string(),
            tokens_used: 17,
            tools_invoked: 1,
            error: None,
        },
    }
}

fn pending_child(id: &str) -> moa_core::types::worker::state::WorkerChildRef {
    moa_core::types::worker::state::WorkerChildRef {
        id: id.to_string(),
        task_hash: format!("hash-{id}"),
        budget_tokens: 128,
        terminal: None,
    }
}

#[test]
fn session_vo_requires_meta_before_use() {
    // Pins: an uninitialized session VO refuses to act instead of inventing
    // metadata, so every handler that needs tenant or model context fails
    // closed until `SessionStore` has initialized the object.
    let state = SessionVoState::default();
    let error = state
        .ensure_initialized()
        .expect_err("an uninitialized VO must not yield metadata");

    assert!(error.to_string().contains("Session metadata missing"));
}

#[test]
fn session_vo_destroy_clears_projection() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .children
        .push(moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 0,
            terminal: None,
        });
    state.last_turn_summary = Some("summary".to_string());
    state.destroy();

    assert_eq!(state, SessionVoState::default());
}

#[test]
fn session_child_registry_is_idempotent_by_child_id() {
    // Pins: root delegation registration preserves one active child ref per id.
    let mut state = SessionVoState::default();
    let child = moa_core::types::worker::state::WorkerChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
        budget_tokens: 128,
        terminal: None,
    };

    assert!(state.register_child(child.clone()));
    assert!(!state.register_child(child));
    assert_eq!(state.children.len(), 1);
    assert!(state.owns_child("child-1"));
}

#[test]
fn session_child_registry_remove_is_exact() {
    // Pins: root delegation cleanup removes only the requested active child ref.
    let mut state = SessionVoState::default();
    state.register_child(moa_core::types::worker::state::WorkerChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
        budget_tokens: 128,
        terminal: None,
    });
    state.register_child(moa_core::types::worker::state::WorkerChildRef {
        id: "child-2".to_string(),
        task_hash: "hash-2".to_string(),
        budget_tokens: 256,
        terminal: None,
    });

    assert!(state.remove_child("child-1"));
    assert!(!state.remove_child("missing"));
    assert_eq!(
        state.children,
        vec![moa_core::types::worker::state::WorkerChildRef {
            id: "child-2".to_string(),
            task_hash: "hash-2".to_string(),
            budget_tokens: 256,
            terminal: None,
        }]
    );
}

#[test]
fn session_child_terminal_result_is_consumed_once() {
    // Pins: root wait consumes a cached terminal child result exactly once.
    let mut state = SessionVoState::default();
    state.register_child(moa_core::types::worker::state::WorkerChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
        budget_tokens: 128,
        terminal: None,
    });
    let terminal = moa_core::types::worker::state::WorkerTerminalResult {
        state: moa_core::types::worker::state::WorkerState::Completed,
        result: moa_core::types::worker::state::WorkerResult {
            worker_id: "child-1".to_string(),
            success: true,
            output: "done".to_string(),
            tokens_used: 17,
            tools_invoked: 2,
            error: None,
        },
    };

    assert!(state.mark_child_terminal(
        moa_core::types::worker::commands::MarkWorkerChildTerminalInput {
            worker_id: "child-1".to_string(),
            terminal: terminal.clone(),
        }
    ));
    assert!(!state.mark_child_terminal(
        moa_core::types::worker::commands::MarkWorkerChildTerminalInput {
            worker_id: "child-1".to_string(),
            terminal: terminal.clone(),
        }
    ));
    assert_eq!(state.consume_child_terminal("child-1"), Some(terminal));
    assert_eq!(state.consume_child_terminal("child-1"), None);
    assert!(!state.owns_child("child-1"));
}

#[test]
fn session_owns_only_registered_child_signals() {
    // Pins: workers are root-session-owned only; signal acceptance is the root
    // session child registry, not a nested worker tree.
    let mut state = SessionVoState::default();
    state.register_child(moa_core::types::worker::state::WorkerChildRef {
        id: "child".to_string(),
        task_hash: "hash".to_string(),
        budget_tokens: 128,
        terminal: None,
    });
    let root_signal = resume_signal(
        moa_core::types::worker::state::ChildSignalKind::Blocked,
        moa_core::types::worker::state::ParentResumePolicy::IfIdle,
    );
    let mut missing_signal = root_signal.clone();
    missing_signal.worker_id = "missing".to_string();

    assert!(state.owns_signal_worker(&root_signal));
    assert!(!state.owns_signal_worker(&missing_signal));
}

fn unread_entry(
    signal_id: moa_core::types::identifiers::AgentSignalId,
    kind: moa_core::types::worker::state::ChildSignalKind,
) -> moa_core::types::worker::state::UnreadChildSignal {
    moa_core::types::worker::state::UnreadChildSignal {
        signal_id,
        worker_id: "child".to_string(),
        kind,
        summary: "summary".to_string(),
        input_request: None,
    }
}

fn resume_signal(
    kind: moa_core::types::worker::state::ChildSignalKind,
    resume_policy: moa_core::types::worker::state::ParentResumePolicy,
) -> moa_core::types::worker::state::WorkerSignal {
    moa_core::types::worker::state::WorkerSignal {
        signal_id: moa_core::types::identifiers::AgentSignalId::new(),
        worker_id: "child".to_string(),
        parent_session: moa_core::types::identifiers::SessionId::new(),
        kind,
        severity: moa_core::types::worker::state::SignalSeverity::Warning,
        summary: "needs attention".to_string(),
        payload: serde_json::Value::Null,
        created_at: Utc::now(),
        resume_policy,
        input_request: None,
    }
}

#[test]
fn unread_child_signal_push_is_idempotent_by_signal_id() {
    // Pins: a retried child-signal delivery records exactly one unread entry.
    let mut state = SessionVoState::default();
    let signal_id = moa_core::types::identifiers::AgentSignalId::new();
    let entry = unread_entry(
        signal_id,
        moa_core::types::worker::state::ChildSignalKind::Finding,
    );

    assert!(state.push_unread_child_signal(entry.clone()));
    assert!(!state.push_unread_child_signal(entry));
    assert_eq!(state.unread_child_signals.len(), 1);
}

#[test]
fn unread_child_signal_cap_evicts_findings_before_action_required() {
    // Pins: when the unread window overflows, NeedsInput/Blocked are preserved while
    // informational Findings are evicted first.
    use moa_core::types::worker::state::ChildSignalKind;
    let mut state = SessionVoState::default();

    let blocked_id = moa_core::types::identifiers::AgentSignalId::new();
    assert!(state.push_unread_child_signal(unread_entry(blocked_id, ChildSignalKind::Blocked)));
    let needs_input_id = moa_core::types::identifiers::AgentSignalId::new();
    assert!(
        state.push_unread_child_signal(unread_entry(needs_input_id, ChildSignalKind::NeedsInput,))
    );
    for _ in 0..super::MAX_UNREAD_CHILD_SIGNALS + 5 {
        state.push_unread_child_signal(unread_entry(
            moa_core::types::identifiers::AgentSignalId::new(),
            ChildSignalKind::Finding,
        ));
    }

    assert_eq!(
        state.unread_child_signals.len(),
        super::MAX_UNREAD_CHILD_SIGNALS
    );
    assert!(
        state
            .unread_child_signals
            .iter()
            .any(|signal| signal.signal_id == blocked_id),
        "Blocked signal must be preserved over evicted Findings"
    );
    assert!(
        state
            .unread_child_signals
            .iter()
            .any(|signal| signal.signal_id == needs_input_id),
        "NeedsInput signal must be preserved over evicted Findings"
    );
}

const TEST_RESUME_MAX: u32 = 6;
const TEST_RESUME_WINDOW_MS: u64 = 600_000;

#[test]
fn resume_gate_arms_only_when_idle_eligible_and_under_budget() {
    // Pins: the resume-eligibility gate arms a pending resume only for an idle
    // coordinator on a resume-eligible IfIdle signal under budget, and never
    // dispatches a turn (it only mutates VO state).
    use moa_core::{
        types::worker::state::ChildSignalKind, types::worker::state::ParentResumePolicy,
    };
    let now = Utc::now();

    let mut idle = SessionVoState::default();
    let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);
    assert!(idle.maybe_arm_parent_resume(
        &signal,
        None,
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(idle.pending_parent_resume_signal, Some(signal.signal_id));

    let mut busy = SessionVoState::default();
    assert!(!busy.maybe_arm_parent_resume(
        &signal,
        Some("turn-1"),
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(busy.pending_parent_resume_signal, None);

    let mut finding = SessionVoState::default();
    let finding_signal = resume_signal(ChildSignalKind::Finding, ParentResumePolicy::IfIdle);
    assert!(!finding.maybe_arm_parent_resume(
        &finding_signal,
        None,
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(finding.pending_parent_resume_signal, None);

    let mut never = SessionVoState::default();
    let never_signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::Never);
    assert!(!never.maybe_arm_parent_resume(
        &never_signal,
        None,
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(never.pending_parent_resume_signal, None);

    let mut exhausted = SessionVoState::default();
    exhausted.resume_budget.window_start = Some(now);
    exhausted.resume_budget.count = TEST_RESUME_MAX;
    assert!(!exhausted.maybe_arm_parent_resume(
        &signal,
        None,
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(exhausted.pending_parent_resume_signal, None);
}

#[test]
fn resume_gate_does_not_rearm_once_a_resume_turn_is_active() {
    // Pins: after a resume is dispatched (turn active), a repeated delivery of the
    // same signal does not arm a second resume — the active-turn gate blocks it.
    use moa_core::{
        types::worker::state::ChildSignalKind, types::worker::state::ParentResumePolicy,
    };
    let now = Utc::now();
    let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);

    let mut state = SessionVoState::default();
    assert!(state.maybe_arm_parent_resume(
        &signal,
        None,
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);

    // The dispatched resume turn is now active; a retried signal cannot re-arm.
    assert!(!state.maybe_arm_parent_resume(
        &signal,
        Some("resume-turn"),
        now,
        TEST_RESUME_MAX,
        TEST_RESUME_WINDOW_MS
    ));
    assert_eq!(state.pending_parent_resume_signal, Some(signal.signal_id));
    assert_eq!(state.resume_budget.count, 1);
}

#[test]
fn resume_budget_window_resets_after_elapsed_window() {
    // Pins: the rolling resume budget caps within a window but reopens once the
    // window elapses, and a zero cap disables resume entirely.
    let base = Utc::now();
    let mut budget = super::ResumeBudget::default();
    for _ in 0..TEST_RESUME_MAX {
        assert!(budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
        budget.consume(base, TEST_RESUME_WINDOW_MS);
    }
    // Cap reached inside the window.
    assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
    // After the window elapses the cap reopens.
    let later = base + chrono::Duration::milliseconds(TEST_RESUME_WINDOW_MS as i64 + 1);
    assert!(budget.allows(later, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
    // A zero cap disables resume regardless of window state.
    assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, 0));
}

#[test]
fn child_liveness_is_single_outstanding_with_monotonic_generations() {
    // Pins: arming a child's liveness check is single-outstanding (a second arm while
    // one is outstanding is a no-op), generations are monotonic so a re-armed child
    // never reuses a prior generation, and a fired check only matches the live
    // generation of an outstanding entry.
    let mut state = SessionVoState::default();

    let first = state
        .arm_child_liveness("child-1")
        .expect("first arm schedules a check");
    // Single-outstanding: a second arm while one is outstanding does not reschedule.
    assert_eq!(state.arm_child_liveness("child-1"), None);
    // The live generation matches; a superseded/older generation does not.
    assert!(state.liveness_generation_matches("child-1", first));
    assert!(!state.liveness_generation_matches("child-1", first.wrapping_sub(1)));
    assert!(!state.liveness_generation_matches("missing", first));

    // A distinct active child gets its own, strictly newer generation.
    let other = state
        .arm_child_liveness("child-2")
        .expect("second child arms independently");
    assert_ne!(first, other);

    // Clearing (terminal/stale/removed) stops scheduling; a stray tick no longer matches.
    state.clear_child_liveness("child-1");
    assert!(!state.liveness_generation_matches("child-1", first));

    // Re-arming after a clear draws a fresh, strictly newer generation, so any stray
    // in-flight tick carrying `first` can never match the re-armed child.
    let rearmed = state
        .arm_child_liveness("child-1")
        .expect("re-arm after clear schedules a new check");
    assert_ne!(first, rearmed);
    assert!(rearmed > other);
    assert!(!state.liveness_generation_matches("child-1", first));
    assert!(state.liveness_generation_matches("child-1", rearmed));
}

#[test]
fn remove_child_clears_outstanding_liveness_check() {
    // Pins: removing a child (e.g. on self-clean) drops its outstanding liveness
    // watchdog so a later fired check recognizes it as superseded.
    let mut state = SessionVoState::default();
    state.register_child(moa_core::types::worker::state::WorkerChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
        budget_tokens: 128,
        terminal: None,
    });
    let generation = state
        .arm_child_liveness("child-1")
        .expect("active child arms a liveness check");
    assert!(state.liveness_generation_matches("child-1", generation));

    assert!(state.remove_child("child-1"));
    assert!(!state.liveness_generation_matches("child-1", generation));
}

#[test]
fn clear_resume_on_outcome_drains_only_dispatch_snapshot() {
    // Pins: completing the resume turn drains exactly the dispatch-time unread
    // snapshot and clears the pending signal, leaving mid-turn arrivals queued.
    use moa_core::types::worker::state::ChildSignalKind;
    let now = Utc::now();
    let mut state = SessionVoState::default();
    let snap_a = moa_core::types::identifiers::AgentSignalId::new();
    let snap_b = moa_core::types::identifiers::AgentSignalId::new();
    state.push_unread_child_signal(unread_entry(snap_a, ChildSignalKind::Blocked));
    state.push_unread_child_signal(unread_entry(snap_b, ChildSignalKind::NeedsInput));
    state.pending_parent_resume_signal = Some(snap_a);

    state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);
    assert_eq!(state.resume_budget.count, 1);

    // A signal that arrives mid-turn must NOT be drained on outcome.
    let mid_turn = moa_core::types::identifiers::AgentSignalId::new();
    state.push_unread_child_signal(unread_entry(mid_turn, ChildSignalKind::Finding));

    // A non-matching turn id is a no-op.
    assert!(!state.clear_resume_on_outcome("other-turn"));
    assert!(state.resume_turn.is_some());

    assert!(state.clear_resume_on_outcome("resume-turn"));
    assert_eq!(state.pending_parent_resume_signal, None);
    assert!(state.resume_turn.is_none());
    let remaining: Vec<_> = state
        .unread_child_signals
        .iter()
        .map(|signal| signal.signal_id)
        .collect();
    assert_eq!(remaining, vec![mid_turn]);
}

#[test]
fn child_terminal_output_offload_round_trip() {
    // Pins: a terminal child whose output exceeds the threshold is reported for offload,
    // compacted to a preview in place, and its claim-check reference is retrievable exactly
    // once for hydration; a small output stays inline with no reference.
    let mut state = SessionVoState::default();
    state.register_child(pending_child("worker-1"));
    let big = "y".repeat(CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES + 10);
    assert!(state.mark_child_terminal(MarkWorkerChildTerminalInput {
        worker_id: "worker-1".to_string(),
        terminal: worker_terminal("worker-1", &big),
    }));
    // Over-threshold output is surfaced verbatim for the handler to store to a blob.
    assert_eq!(
        state.large_child_terminal_output("worker-1"),
        Some(big.clone())
    );

    let claim = ClaimCheck {
        blob_id: "blob-1".to_string(),
        size: big.len(),
        preview: "unused".to_string(),
    };
    state.compact_child_terminal_output("worker-1", claim.clone());
    // The inline copy is now a preview, so it no longer flags as large.
    assert_eq!(state.large_child_terminal_output("worker-1"), None);
    // The reference hydrates exactly once.
    assert_eq!(state.take_child_terminal_blob("worker-1"), Some(claim));
    assert_eq!(state.take_child_terminal_blob("worker-1"), None);

    // A small output is never offloaded.
    let mut small = SessionVoState::default();
    small.register_child(pending_child("worker-2"));
    small.mark_child_terminal(MarkWorkerChildTerminalInput {
        worker_id: "worker-2".to_string(),
        terminal: worker_terminal("worker-2", "short output"),
    });
    assert_eq!(small.large_child_terminal_output("worker-2"), None);
    assert_eq!(small.take_child_terminal_blob("worker-2"), None);
}

#[test]
fn remove_child_drops_claim_check_reference() {
    // Pins: removing a child (worker self-cleanup) also drops its output claim-check
    // reference so evicted children never leak references in VO state.
    let mut state = SessionVoState::default();
    state.register_child(pending_child("worker-1"));
    let big = "q".repeat(CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES + 1);
    state.mark_child_terminal(MarkWorkerChildTerminalInput {
        worker_id: "worker-1".to_string(),
        terminal: worker_terminal("worker-1", &big),
    });
    state.compact_child_terminal_output(
        "worker-1",
        ClaimCheck {
            blob_id: "b".to_string(),
            size: big.len(),
            preview: "p".to_string(),
        },
    );

    assert!(state.remove_child("worker-1"));
    assert_eq!(state.take_child_terminal_blob("worker-1"), None);
}

fn pending_input(request: &str, generation: u64, awakeable: &str) -> CoordinatorPendingInput {
    CoordinatorPendingInput {
        turn_id: "turn-1".to_string(),
        generation,
        input_request_id: request.to_string(),
        awakeable_id: awakeable.to_string(),
        waiting_workflow_id: format!("workflow-{awakeable}"),
    }
}

#[test]
fn coordinator_input_registration_is_idempotent_and_fenced_on_generation() {
    // Pins: a replayed registration must not create a second entry (the orphan
    // would never be resolved), and a reply naming a superseded generation must
    // resolve nothing rather than unblock a turn with an answer meant for
    // different work.
    let mut state = SessionVoState::default();

    assert!(state.register_coordinator_input(pending_input("req-1", 4, "awk-1")));
    assert!(
        !state.register_coordinator_input(pending_input("req-1", 4, "awk-1b")),
        "a duplicate request id must be a no-op"
    );
    assert_eq!(state.pending_coordinator_inputs.len(), 1);

    assert_eq!(
        state.take_coordinator_input_awakeable("turn-1", 5, "req-1"),
        None,
        "a superseded generation must not resolve the awakeable"
    );
    assert_eq!(
        state.take_coordinator_input_awakeable("turn-other", 4, "req-1"),
        None,
        "another turn must not resolve this turn's awakeable"
    );
    assert_eq!(
        state.take_coordinator_input_awakeable("turn-1", 4, "req-1"),
        Some("awk-1".to_string()),
        "the exact owner resolves its own awakeable"
    );
}

#[test]
fn a_late_duplicate_reply_cannot_resolve_a_replacement_awakeable() {
    // Pins: delivery history outlives the awakeable. Without it, a late second
    // reply carrying the same request id would resolve whatever awakeable a
    // *newer* request had since registered under that id — unblocking the wrong
    // turn with a stale answer.
    let mut state = SessionVoState::default();
    state.register_coordinator_input(pending_input("req-1", 4, "awk-first"));

    assert_eq!(
        state.take_coordinator_input_awakeable("turn-1", 4, "req-1"),
        Some("awk-first".to_string())
    );
    assert!(state.coordinator_input_already_delivered("req-1"));

    // A newer request reuses the id and registers a fresh awakeable.
    state.register_coordinator_input(pending_input("req-1", 4, "awk-replacement"));
    assert!(
        state.coordinator_input_already_delivered("req-1"),
        "history must still record the earlier delivery"
    );
}

#[test]
fn ending_a_turn_clears_the_coordinator_inputs_it_raised() {
    // Pins: a cancelled or failed turn must not leave its pending target
    // advertised. Otherwise the next plain user message is swallowed as a reply
    // to work that no longer exists.
    let mut state = SessionVoState::default();
    state.register_coordinator_input(pending_input("req-old", 4, "awk-old"));
    state.register_coordinator_input(pending_input("req-new", 6, "awk-new"));
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
        turn_id: "turn-1".to_string(),
        generation: 4,
        input_request_id: "req-old".to_string(),
    });

    let cleared = state.clear_coordinator_inputs_before(6);

    assert_eq!(cleared, vec!["awk-old".to_string()]);
    assert_eq!(state.pending_coordinator_inputs.len(), 1);
    assert_eq!(
        state.pending_coordinator_inputs[0].input_request_id, "req-new",
        "the live generation's request must survive"
    );
    assert!(
        state.pending_user_reply_targets.is_empty(),
        "clearing must also retract the advertised reply target, or the next \
         plain user message is swallowed as a reply to dead work"
    );
}

#[test]
fn clearing_by_waiting_workflow_spares_a_retry_of_the_same_turn() {
    // Pins: a retried invocation of one logical turn registers its own
    // awakeable. Clearing by turn id would take the live retry down with the
    // dead original, stranding the retry on an awakeable no reply can resolve.
    let mut state = SessionVoState::default();
    let mut original = pending_input("req-a", 4, "awk-original");
    original.waiting_workflow_id = "workflow-original".to_string();
    let mut retry = pending_input("req-b", 4, "awk-retry");
    retry.waiting_workflow_id = "workflow-retry".to_string();
    state.register_coordinator_input(original);
    state.register_coordinator_input(retry);

    let cleared = state.clear_coordinator_inputs_for_workflow("workflow-original");

    assert_eq!(cleared, vec!["awk-original".to_string()]);
    assert_eq!(state.pending_coordinator_inputs.len(), 1);
    assert_eq!(
        state.pending_coordinator_inputs[0].waiting_workflow_id, "workflow-retry",
        "the live retry's registration must survive"
    );
}

/// Advertises one worker input target and its paired unread `NeedsInput` signal.
fn advertise_worker_input(
    state: &mut SessionVoState,
    worker_id: &str,
    turn_id: &str,
    generation: u64,
    input_request_id: &str,
) {
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::WorkerInput {
        worker_id: worker_id.to_string(),
        turn_id: turn_id.to_string(),
        generation,
        input_request_id: input_request_id.to_string(),
    });
    state.push_unread_child_signal(UnreadChildSignal {
        signal_id: AgentSignalId::new(),
        worker_id: worker_id.to_string(),
        kind: ChildSignalKind::NeedsInput,
        summary: "which cluster?".to_string(),
        input_request: Some(moa_core::types::worker::state::WorkerInputRequest {
            turn_id: turn_id.to_string(),
            generation,
            input_request_id: input_request_id.to_string(),
            audience: moa_core::types::worker::state::InputAudience::User,
        }),
    });
}

#[test]
fn clearing_a_worker_input_retracts_exactly_the_target_the_child_cleared() {
    // Pins: a child clearing one dead round-trip retracts that advertised target and
    // its unread question, and nothing else. Clearing by request id alone (or by
    // worker alone) here would silently un-advertise a live sibling round-trip, and
    // the user's answer to it would then start an ordinary turn instead.
    let mut state = SessionVoState::default();
    advertise_worker_input(&mut state, "worker-1", "worker-turn-1", 3, "req-1");
    advertise_worker_input(&mut state, "worker-1", "worker-turn-2", 4, "req-2");
    advertise_worker_input(&mut state, "worker-2", "worker-turn-1", 3, "req-1");
    assert_eq!(state.pending_user_reply_targets.len(), 3);

    let cleared = WorkerInputTarget {
        turn_id: "worker-turn-1".to_string(),
        generation: 3,
        input_request_id: "req-1".to_string(),
    };
    // A clear naming coordinates this session never advertised retracts nothing.
    assert!(!state.clear_worker_input_target(
        "worker-1",
        &WorkerInputTarget {
            generation: 9,
            ..cleared.clone()
        }
    ));
    assert_eq!(state.pending_user_reply_targets.len(), 3);

    assert!(state.clear_worker_input_target("worker-1", &cleared));

    assert_eq!(
        state.pending_user_reply_targets,
        vec![
            PendingUserReplyTarget::WorkerInput {
                worker_id: "worker-1".to_string(),
                turn_id: "worker-turn-2".to_string(),
                generation: 4,
                input_request_id: "req-2".to_string(),
            },
            PendingUserReplyTarget::WorkerInput {
                worker_id: "worker-2".to_string(),
                turn_id: "worker-turn-1".to_string(),
                generation: 3,
                input_request_id: "req-1".to_string(),
            },
        ],
        "only the cleared child's exact round-trip may be retracted"
    );
    assert_eq!(
        state.unread_child_signals.len(),
        2,
        "the answered child's question is dropped with its target"
    );
    // Retracting the same target again is an idempotent no-op.
    assert!(!state.clear_worker_input_target("worker-1", &cleared));
}

#[test]
fn removing_a_child_retracts_every_worker_input_target_it_advertised() {
    // Pins: a child that left the fan-out can answer nothing. Leaving its targets
    // advertised would make the next plain user message a reply to a worker that no
    // longer exists — and with two advertised, every later reply ambiguous.
    let mut state = SessionVoState::default();
    state.register_child(WorkerChildRef {
        id: "worker-1".to_string(),
        task_hash: "hash".to_string(),
        budget_tokens: 0,
        terminal: None,
    });
    advertise_worker_input(&mut state, "worker-1", "worker-turn-1", 3, "req-1");
    advertise_worker_input(&mut state, "worker-1", "worker-turn-1", 3, "req-2");
    advertise_worker_input(&mut state, "worker-2", "worker-turn-1", 3, "req-3");

    assert!(state.remove_child("worker-1"));

    assert_eq!(
        state.pending_user_reply_targets,
        vec![PendingUserReplyTarget::WorkerInput {
            worker_id: "worker-2".to_string(),
            turn_id: "worker-turn-1".to_string(),
            generation: 3,
            input_request_id: "req-3".to_string(),
        }],
        "a removed child's targets go with it; a sibling's stays"
    );
    assert_eq!(
        state
            .unread_child_signals
            .iter()
            .map(|signal| signal.worker_id.as_str())
            .collect::<Vec<_>>(),
        vec!["worker-2"]
    );
}

#[test]
fn re_registering_a_coordinator_input_at_a_new_generation_replaces_its_target() {
    // Pins: advertise-dedup identity for a coordinator request excludes the
    // generation. A turn that re-raises one request under a newer generation must
    // supersede its advertised target; two targets for the same question would make
    // the user's next unaddressed reply ambiguous and reject it instead of
    // delivering it.
    let mut state = SessionVoState::default();
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
        turn_id: "turn-1".to_string(),
        generation: 4,
        input_request_id: "req-1".to_string(),
    });
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
        turn_id: "turn-1".to_string(),
        generation: 5,
        input_request_id: "req-1".to_string(),
    });

    assert_eq!(
        state.pending_user_reply_targets,
        vec![PendingUserReplyTarget::CoordinatorInput {
            turn_id: "turn-1".to_string(),
            generation: 5,
            input_request_id: "req-1".to_string(),
        }],
        "the newer generation replaces the advertised target instead of adding one"
    );

    // A genuinely different request still accumulates: dedup is identity, not a cap.
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
        turn_id: "turn-1".to_string(),
        generation: 5,
        input_request_id: "req-2".to_string(),
    });
    assert_eq!(state.pending_user_reply_targets.len(), 2);
}

#[test]
fn re_registering_a_worker_input_at_a_new_generation_replaces_its_target() {
    // Pins: pending-target identity excludes the generation. If a re-registration
    // accumulated a second target for the same request, an unaddressed reply would
    // be rejected as ambiguous between two coordinates of one question.
    let mut state = SessionVoState::default();
    advertise_worker_input(&mut state, "worker-1", "worker-turn-1", 3, "req-1");
    advertise_worker_input(&mut state, "worker-1", "worker-turn-1", 4, "req-1");

    assert_eq!(
        state.pending_user_reply_targets,
        vec![PendingUserReplyTarget::WorkerInput {
            worker_id: "worker-1".to_string(),
            turn_id: "worker-turn-1".to_string(),
            generation: 4,
            input_request_id: "req-1".to_string(),
        }],
        "the newer generation replaces the advertised target instead of adding one"
    );
}

#[test]
fn coordinator_input_reply_target_matches_only_its_exact_coordinates() {
    // Pins: the reply matrix treats a coordinator input as exactly addressed.
    // Matching loosely on request id alone would deliver a reply across turns
    // or across generations.
    use moa_core::types::contact::MessageReplyTarget;

    let pending: PendingUserReplyTarget = PendingUserReplyTarget::CoordinatorInput {
        turn_id: "turn-1".to_string(),
        generation: 4,
        input_request_id: "req-1".to_string(),
    };

    assert!(
        pending.matches_reply_target(&MessageReplyTarget::CoordinatorInput {
            turn_id: "turn-1".to_string(),
            generation: 4,
            input_request_id: "req-1".to_string(),
        })
    );
    for mismatch in [
        MessageReplyTarget::CoordinatorInput {
            turn_id: "turn-2".to_string(),
            generation: 4,
            input_request_id: "req-1".to_string(),
        },
        MessageReplyTarget::CoordinatorInput {
            turn_id: "turn-1".to_string(),
            generation: 5,
            input_request_id: "req-1".to_string(),
        },
        MessageReplyTarget::CoordinatorInput {
            turn_id: "turn-1".to_string(),
            generation: 4,
            input_request_id: "req-2".to_string(),
        },
        MessageReplyTarget::WorkerInput {
            worker_id: "turn-1".to_string(),
            turn_id: "turn-1".to_string(),
            generation: 4,
            input_request_id: "req-1".to_string(),
        },
    ] {
        assert!(
            !pending.matches_reply_target(&mismatch),
            "every coordinate must be load bearing: {mismatch:?}"
        );
    }
}

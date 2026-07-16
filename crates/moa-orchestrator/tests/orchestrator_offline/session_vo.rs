//! Unit coverage for the Session virtual object's state projection helpers.

use chrono::Utc;
use moa_core::{
    types::channel::Channel, types::contact::SessionActorRef, types::identifiers::ModelId,
    types::identifiers::TenantId, types::session::CancelScope, types::session::SessionMeta,
    types::session::SessionStatus, types::worker::commands::UserReplyDeliveryAck,
    types::worker::state::WorkerProgressSummary, types::worker::state::WorkerResult,
    types::worker::state::WorkerState, types::worker::state::WorkerTerminalResult,
};
use moa_orchestrator::objects::session::{
    ActiveExecutionRunState, ChildProgressFetch, ExecutionSynthesisDedupe,
    ExecutionTemplateAdmissionReplayState, ExecutionTemplateAdmissionResume,
    PendingUserReplyTarget, SessionVoState, child_progress_in_plan_order,
    plan_child_progress_fan_in, terminal_child_summary,
};
use uuid::Uuid;

fn test_meta() -> SessionMeta {
    SessionMeta {
        tenant_id: TenantId::from(
            Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                .expect("fixture tenant id parses"),
        ),
        created_by: Some(SessionActorRef::Identity {
            id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("fixture identity id parses"),
        }),
        channel: Channel::Chat,
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn test_message(text: &str) -> moa_core::types::session::UserMessage {
    moa_core::types::session::UserMessage {
        text: text.to_string(),
        attachments: vec![],
    }
}

#[test]
fn execution_template_admission_replay_state_is_semantic() {
    // Pins: commit-before-result retries resume at the first missing durable boundary, while one
    // caller key reused for a changed request conflicts before another event, planning call, or run.
    let session_id = moa_core::types::identifiers::SessionId(Uuid::from_u128(81));
    let operation_uid = Uuid::from_u128(82);
    let execution_run_uid = Uuid::from_u128(83);
    let fingerprint = "a".repeat(64);
    let initial = ExecutionTemplateAdmissionReplayState {
        operation_uid,
        request_fingerprint: fingerprint.clone(),
        originating_user_sequence_num: None,
        execution_run_uid: None,
    };
    assert_eq!(
        initial
            .resume(&fingerprint, session_id)
            .expect("first admission should append its objective"),
        ExecutionTemplateAdmissionResume::AppendObjective
    );

    let objective_committed = ExecutionTemplateAdmissionReplayState {
        originating_user_sequence_num: Some(29),
        ..initial.clone()
    };
    assert_eq!(
        objective_committed
            .resume(&fingerprint, session_id)
            .expect("objective commit should resume at Task 7 start"),
        ExecutionTemplateAdmissionResume::StartExecution {
            originating_user_sequence_num: 29,
        }
    );

    let completed = ExecutionTemplateAdmissionReplayState {
        operation_uid,
        request_fingerprint: fingerprint.clone(),
        originating_user_sequence_num: Some(29),
        execution_run_uid: Some(execution_run_uid),
    };

    assert_eq!(
        completed
            .resume(&fingerprint, session_id)
            .expect("same semantic request should replay"),
        ExecutionTemplateAdmissionResume::Complete(
            moa_execution::wire::ExecutionTemplateAdmissionResponse {
                session_id,
                originating_user_sequence_num: 29,
                execution_run_uid,
            }
        )
    );
    for state in [&initial, &objective_committed, &completed] {
        let conflict = state
            .resume(&"b".repeat(64), session_id)
            .expect_err("changed semantic request must conflict at every durable boundary");
        assert!(matches!(
            conflict,
            moa_core::error::MoaError::ValidationError(message)
                if message == "execution-template admission idempotency key conflicts with the first request"
        ));
    }
    assert_eq!(completed.operation_uid, operation_uid);
    assert_eq!(completed.originating_user_sequence_num, Some(29));
    assert_eq!(completed.execution_run_uid, Some(execution_run_uid));
}

#[test]
fn session_vo_post_message_without_meta_errors() {
    let mut state = SessionVoState::default();
    let error = state
        .enqueue_message(test_message("hello"), Utc::now())
        .expect_err("enqueue should fail without metadata");

    assert!(error.to_string().contains("Session metadata missing"));
}

#[test]
fn session_vo_post_message_queues_in_state() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"), Utc::now())
        .expect("enqueue should succeed");

    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.pending[0].text, "hello");
}

#[test]
fn session_vo_post_message_updates_status_to_running_then_idle_parks_paused() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"), Utc::now())
        .expect("enqueue should succeed");
    assert_eq!(state.current_status(), SessionStatus::Running);

    state.drain_pending_messages();
    let status = state.apply_turn_outcome(moa_core::types::session::TurnOutcome::Idle, Utc::now());

    assert_eq!(status, SessionStatus::Paused);
    assert_eq!(state.current_status(), SessionStatus::Paused);
}

#[test]
fn accepted_execution_run_keeps_session_running_and_drains_queue() {
    // Pins: detached Run admission settles the root turn without idling the owning session.
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state.active_execution_runs.push(active_execution_run(
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("fixture run id parses"),
        1,
    ));
    state
        .enqueue_message(test_message("queued follow-up"), Utc::now())
        .expect("queue follow-up");

    assert_eq!(state.drain_pending_messages(), 1);
    state.apply_accepted_execution_turn(Utc::now());

    assert_eq!(state.current_status(), SessionStatus::Running);
    assert!(state.pending.is_empty());
    assert_eq!(
        state.last_turn_summary.as_deref(),
        Some("Execution accepted.")
    );
    assert_eq!(state.active_execution_runs.len(), 1);
}

fn active_execution_run(run_uid: Uuid, origin: u64) -> ActiveExecutionRunState {
    ActiveExecutionRunState {
        run_uid,
        originating_user_sequence_num: origin,
        progress: None,
        last_progress_signature: None,
        last_progress_at: None,
    }
}

fn execution_progress(run_uid: Uuid) -> moa_core::events::ExecutionProgress {
    moa_core::events::ExecutionProgress {
        run_uid,
        originating_user_sequence_num: 7,
        plan_revision: 1,
        status: "running".to_string(),
        total: 8,
        completed: 2,
        failed: 1,
        cancelled: 0,
    }
}

#[test]
fn session_progress_projects_exact_persisted_active_execution_values() {
    // Pins: Session/progress projects only persisted aggregate progress while retaining the
    // started run marker that has not emitted progress yet.
    let started_run_uid = Uuid::from_u128(68);
    let progressed_run_uid = Uuid::from_u128(69);
    let expected = moa_core::events::ExecutionProgress {
        run_uid: progressed_run_uid,
        originating_user_sequence_num: 41,
        plan_revision: 5,
        status: "waiting_input".to_string(),
        total: 13,
        completed: 8,
        failed: 3,
        cancelled: 1,
    };
    let mut started = active_execution_run(started_run_uid, 40);
    started.progress = None;
    let mut progressed = active_execution_run(progressed_run_uid, 41);
    progressed.progress = Some(expected.clone());
    let active_runs = vec![started, progressed];

    let projected = SessionVoState::project_active_execution_progress(&active_runs);

    assert_eq!(projected, vec![expected]);
    assert_eq!(active_runs[0].run_uid, started_run_uid);
    assert_eq!(active_runs[0].progress, None);
}

#[test]
fn execution_progress_requires_cadence_and_changed_exact_aggregate_tuple() {
    // Pins: every tuple member participates in delta detection, while an early changed tuple
    // and a due identical tuple are both suppressed.
    let run_uid = Uuid::from_u128(70);
    let start = Utc::now();
    let baseline = execution_progress(run_uid);
    let mut state = SessionVoState::default();
    state
        .active_execution_runs
        .push(active_execution_run(run_uid, 7));
    assert!(
        state
            .apply_execution_progress(baseline.clone(), start, 1_000)
            .expect("first aggregate should emit")
    );

    let mut early_change = baseline.clone();
    early_change.completed += 1;
    assert!(
        !state
            .apply_execution_progress(
                early_change,
                start + chrono::Duration::milliseconds(999),
                1_000,
            )
            .expect("early aggregate should be cadence-suppressed")
    );
    assert!(
        !state
            .apply_execution_progress(
                baseline.clone(),
                start + chrono::Duration::milliseconds(1_000),
                1_000,
            )
            .expect("identical aggregate should be delta-suppressed")
    );

    let mut plan_revision_changed = baseline.clone();
    plan_revision_changed.plan_revision += 1;
    let mut status_changed = baseline.clone();
    status_changed.status = "waiting_input".to_string();
    let mut total_changed = baseline.clone();
    total_changed.total += 1;
    let mut completed_changed = baseline.clone();
    completed_changed.completed += 1;
    let mut failed_changed = baseline.clone();
    failed_changed.failed += 1;
    let mut cancelled_changed = baseline.clone();
    cancelled_changed.cancelled += 1;
    let changed_members = [
        ("plan_revision", plan_revision_changed),
        ("status", status_changed),
        ("total", total_changed),
        ("completed", completed_changed),
        ("failed", failed_changed),
        ("cancelled", cancelled_changed),
    ];
    for (member, changed) in changed_members {
        let mut isolated = SessionVoState::default();
        isolated
            .active_execution_runs
            .push(active_execution_run(run_uid, 7));
        assert!(
            isolated
                .apply_execution_progress(baseline.clone(), start, 1_000)
                .expect("baseline aggregate should emit")
        );
        assert!(
            isolated
                .apply_execution_progress(
                    changed,
                    start + chrono::Duration::milliseconds(1_000),
                    1_000,
                )
                .expect("changed due aggregate should emit"),
            "tuple member {member} must participate in delta detection"
        );
    }
}

#[test]
fn terminal_synthesis_dispatch_clears_active_state_once_and_replays_stable_marker() {
    // Pins: run state remains active before dispatch, then one stable run+origin marker clears
    // active progress/pending input and exact replay returns the same turn identity.
    let run_uid = Uuid::from_u128(71);
    let mut state = SessionVoState::default();
    state
        .active_execution_runs
        .push(active_execution_run(run_uid, 7));
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::ExecutionInput {
        run_uid,
        task_id: Uuid::from_u128(72),
        generation: 3,
    });
    assert_eq!(state.active_execution_runs.len(), 1);

    let marker = ExecutionSynthesisDedupe {
        run_uid,
        originating_user_sequence_num: 7,
        turn_id: format!("execution-synthesis:{run_uid}:7"),
    };
    state
        .record_execution_synthesis_dispatch(marker.clone())
        .expect("first durable synthesis dispatch should commit");
    assert!(state.active_execution_runs.is_empty());
    assert!(state.pending_user_reply_targets.is_empty());
    assert_eq!(state.execution_synthesis_marker(run_uid, 7), Some(&marker));

    state
        .record_execution_synthesis_dispatch(marker.clone())
        .expect("exact terminal replay should reuse the marker");
    assert_eq!(state.execution_synthesis_dedupe, vec![marker.clone()]);
    let conflict = state.record_execution_synthesis_dispatch(ExecutionSynthesisDedupe {
        turn_id: "different-turn".to_string(),
        ..marker
    });
    assert!(conflict.is_err(), "changed replay must conflict");
}

#[test]
fn plain_reply_has_a_target_only_when_exactly_one_user_addressed_target_exists() {
    // Pins: zero and ambiguous execution/worker targets remain ordinary turns; exactly one
    // target is consumed and only its exact acknowledgement may clear it.
    let mut state = SessionVoState::default();
    assert_eq!(state.exact_pending_user_reply_target(), None);

    let execution = PendingUserReplyTarget::ExecutionInput {
        run_uid: Uuid::from_u128(73),
        task_id: Uuid::from_u128(74),
        generation: 2,
    };
    state.upsert_pending_user_reply_target(execution.clone());
    assert_eq!(
        state.exact_pending_user_reply_target(),
        Some(execution.clone())
    );

    let worker = PendingUserReplyTarget::WorkerInput {
        worker_id: "worker-1".to_string(),
        input_request_id: "request-1".to_string(),
    };
    state.upsert_pending_user_reply_target(worker.clone());
    assert_eq!(state.exact_pending_user_reply_target(), None);
    assert!(state.clear_pending_user_reply_target(&execution));
    assert_eq!(
        state.exact_pending_user_reply_target(),
        Some(worker.clone())
    );
    assert!(!state.apply_pending_user_reply_ack(&worker, UserReplyDeliveryAck::Conflict));
    assert_eq!(
        state.exact_pending_user_reply_target(),
        Some(worker.clone()),
        "conflicted delivery must remain pending for a later exact retry"
    );
    assert!(state.apply_pending_user_reply_ack(&worker, UserReplyDeliveryAck::Applied));
    assert_eq!(state.exact_pending_user_reply_target(), None);

    state.upsert_pending_user_reply_target(worker.clone());
    assert!(state.apply_pending_user_reply_ack(&worker, UserReplyDeliveryAck::Replayed));
    assert_eq!(state.exact_pending_user_reply_target(), None);
}

#[test]
fn session_vo_cancel_sets_flag() {
    let mut state = SessionVoState::default();
    state.set_cancel_flag(CancelScope::CoordinatorOnly);

    assert_eq!(state.take_cancel_flag(), Some(CancelScope::CoordinatorOnly));
    assert_eq!(state.take_cancel_flag(), None);
}

fn test_child_ref(id: &str) -> moa_core::types::worker::state::WorkerChildRef {
    moa_core::types::worker::state::WorkerChildRef {
        id: id.to_string(),
        task_hash: format!("hash-{id}"),
        budget_tokens: 0,
        terminal: None,
    }
}

#[test]
fn coordinator_only_cancel_preserves_child_refs() {
    // Pins: a CoordinatorOnly cancel stops only the coordinator turn and does not cascade to
    // children, so the handler must not forward Worker/cancel and the child refs stay registered.
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state.children.push(test_child_ref("child-1"));
    state.children.push(test_child_ref("child-2"));

    state.set_cancel_flag(CancelScope::CoordinatorOnly);

    // The production branch the handler uses to decide whether to cascade to children.
    assert!(!CancelScope::CoordinatorOnly.cancels_task_tree());
    // Children remain registered on the coordinator (left running).
    assert_eq!(state.children.len(), 2);
    assert_eq!(state.take_cancel_flag(), Some(CancelScope::CoordinatorOnly));
}

#[test]
fn task_tree_cancel_cancels_children() {
    // Pins: a TaskTree cancel reproduces today's behavior — the handler forwards Worker/cancel
    // to every registered child in addition to cancelling the coordinator turn.
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state.children.push(test_child_ref("child-1"));
    state.children.push(test_child_ref("child-2"));

    state.set_cancel_flag(CancelScope::TaskTree);

    // The production branch the handler uses to decide whether to cascade to children.
    assert!(CancelScope::TaskTree.cancels_task_tree());
    // TaskTree is also the default scope (a bare "stop" cancels everything).
    assert_eq!(CancelScope::default(), CancelScope::TaskTree);
    // Child refs remain available for the handler's forward-to-children loop.
    assert_eq!(state.children.len(), 2);
    assert_eq!(state.take_cancel_flag(), Some(CancelScope::TaskTree));
}

fn terminal_child_ref(id: &str, output: &str) -> moa_core::types::worker::state::WorkerChildRef {
    moa_core::types::worker::state::WorkerChildRef {
        id: id.to_string(),
        task_hash: format!("hash-{id}"),
        budget_tokens: 256,
        terminal: Some(WorkerTerminalResult {
            state: WorkerState::Completed,
            result: WorkerResult {
                worker_id: id.to_string(),
                success: true,
                output: output.to_string(),
                tokens_used: 42,
                tools_invoked: 1,
                error: None,
            },
        }),
    }
}

#[test]
fn session_progress_fan_in_includes_active_child_and_synthesizes_terminal() {
    // Pins: Session/progress builds child_progress by bounded on-demand fan-in — an active
    // child is scheduled for a live progress_summary read, a terminal child is synthesized
    // in place from its cached parent ref without a live call, and their mixed plan order is
    // stable even when the live reads later complete out of order.
    let children = vec![
        test_child_ref("active-1"),
        terminal_child_ref("done-1", "summary for done-1"),
        test_child_ref("active-2"),
    ];

    let plan = plan_child_progress_fan_in(&children, 4);
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0], ChildProgressFetch::Fetch("active-1".to_string()));
    match &plan[1] {
        ChildProgressFetch::Ready(summary) => {
            assert_eq!(summary.worker_id, "done-1");
            assert_eq!(summary.state, WorkerState::Completed);
            assert_eq!(summary.last_summary.as_deref(), Some("summary for done-1"));
            assert_eq!(summary.tokens_used, 42);
            assert!(!summary.stale);
            assert_eq!(summary.active_turn_id, None);
        }
        other => panic!("expected a synthesized terminal summary, got {other:?}"),
    }
    assert_eq!(plan[2], ChildProgressFetch::Fetch("active-2".to_string()));

    // The synthesized summary matches the standalone synthesis helper.
    let direct = terminal_child_summary(
        &children[1],
        children[1]
            .terminal
            .as_ref()
            .expect("terminal child has a cached result"),
    );
    assert_eq!(ChildProgressFetch::Ready(direct), plan[1]);
}

#[test]
fn session_progress_fan_in_caps_live_child_calls() {
    // Pins: the fan-in never walks an unbounded tree — live progress_summary reads are
    // capped by the fan-out limit, while cached terminal children are always synthesized.
    let children = vec![
        test_child_ref("active-1"),
        test_child_ref("active-2"),
        test_child_ref("active-3"),
        terminal_child_ref("done-1", "done"),
    ];

    let plan = plan_child_progress_fan_in(&children, 2);
    let fetches = plan
        .iter()
        .filter(|item| matches!(item, ChildProgressFetch::Fetch(_)))
        .count();
    let ready = plan
        .iter()
        .filter(|item| matches!(item, ChildProgressFetch::Ready(_)))
        .count();
    assert_eq!(fetches, 2, "live fan-out is capped at max_live");
    assert_eq!(ready, 1, "cached terminal children are always synthesized");
}

#[test]
fn session_progress_fan_in_restores_plan_order_and_omits_failed_reads() {
    // Pins: live reads may complete in any order and one may fail, but Session/progress and
    // list_workers both emit the successful summaries in their original bounded-plan order.
    let summary = |worker_id: &str| WorkerProgressSummary {
        worker_id: worker_id.to_string(),
        state: WorkerState::Running,
        active_turn_id: None,
        last_summary: None,
        tokens_used: 0,
        budget_remaining: 100,
        last_heartbeat_at: None,
        stale: false,
        awaiting_input: false,
    };
    let mut completion_slots = vec![None, None, None, None];

    completion_slots[3] = Some(summary("fourth-completed-first"));
    completion_slots[0] = Some(summary("first-completed-second"));
    completion_slots[2] = Some(summary("third-completed-last"));
    // Slot 1 remains empty to model one failed Worker/progress_summary read.

    let ordered = child_progress_in_plan_order(completion_slots);
    let worker_ids: Vec<&str> = ordered
        .iter()
        .map(|summary| summary.worker_id.as_str())
        .collect();
    assert_eq!(
        worker_ids,
        vec![
            "first-completed-second",
            "third-completed-last",
            "fourth-completed-first",
        ],
        "failed reads are omitted and successful reads retain plan order"
    );
}

#[test]
fn session_vo_destroy_clears_state() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"), Utc::now())
        .expect("enqueue should succeed");
    state.last_turn_summary = Some("summary".to_string());
    state
        .children
        .push(moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 0,
            terminal: None,
        });
    state.set_cancel_flag(CancelScope::TaskTree);
    state.destroy();

    assert_eq!(state, SessionVoState::default());
}

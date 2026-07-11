//! Unit coverage for the Session virtual object's state projection helpers.

use chrono::Utc;
use moa_core::{
    types::channel::Channel, types::contact::SessionActorRef, types::identifiers::ModelId,
    types::identifiers::TenantId, types::session::CancelScope, types::session::SessionMeta,
    types::session::SessionStatus, types::worker::state::WorkerProgressSummary,
    types::worker::state::WorkerResult, types::worker::state::WorkerState,
    types::worker::state::WorkerTerminalResult,
};
use moa_orchestrator::objects::session::{
    ChildProgressFetch, SessionVoState, child_progress_in_plan_order, plan_child_progress_fan_in,
    terminal_child_summary,
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

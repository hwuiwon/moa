//! Unit coverage for the Session virtual object's state projection helpers.

use moa_core::{CancelMode, ModelId, Platform, SessionMeta, SessionStatus, UserId, WorkspaceId};
use moa_orchestrator::objects::session::SessionVoState;

fn test_meta() -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new("workspace-1"),
        user_id: UserId::new("user-1"),
        platform: Platform::Cli,
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn test_message(text: &str) -> moa_core::UserMessage {
    moa_core::UserMessage {
        text: text.to_string(),
        attachments: vec![],
    }
}

#[test]
fn session_vo_post_message_without_meta_errors() {
    let mut state = SessionVoState::default();
    let error = state
        .enqueue_message(test_message("hello"))
        .expect_err("enqueue should fail without metadata");

    assert!(error.to_string().contains("Session metadata missing"));
}

#[test]
fn session_vo_post_message_queues_in_state() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"))
        .expect("enqueue should succeed");

    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.pending[0].text, "hello");
}

#[test]
fn session_vo_post_message_updates_status_to_running_then_idle_parks_paused() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"))
        .expect("enqueue should succeed");
    assert_eq!(state.current_status(), SessionStatus::Running);

    state.drain_pending_messages();
    let status = state.apply_turn_outcome(moa_core::TurnOutcome::Idle);

    assert_eq!(status, SessionStatus::Paused);
    assert_eq!(state.current_status(), SessionStatus::Paused);
}

#[test]
fn session_vo_cancel_sets_flag() {
    let mut state = SessionVoState::default();
    state.set_cancel_flag(CancelMode::Soft);

    assert_eq!(state.take_cancel_flag(), Some(CancelMode::Soft));
    assert_eq!(state.take_cancel_flag(), None);
}

#[test]
fn session_vo_destroy_clears_state() {
    let mut state = SessionVoState::default();
    state.set_meta(test_meta());
    state
        .enqueue_message(test_message("hello"))
        .expect("enqueue should succeed");
    state.pending_approval = Some("approval-1".to_string());
    state.last_turn_summary = Some("summary".to_string());
    state.children.push(moa_core::SubAgentChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
    });
    state.set_cancel_flag(CancelMode::Hard);
    state.destroy();

    assert_eq!(state, SessionVoState::default());
}

//! Unit coverage for the Session virtual object's state projection helpers.

use chrono::Utc;
use moa_core::{
    CancelMode, Channel, ModelId, SessionActorRef, SessionMeta, SessionStatus, TenantId,
};
use moa_orchestrator::objects::session::SessionVoState;
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
    let status = state.apply_turn_outcome(moa_core::TurnOutcome::Idle, Utc::now());

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
        .enqueue_message(test_message("hello"), Utc::now())
        .expect("enqueue should succeed");
    state.last_turn_summary = Some("summary".to_string());
    state.children.push(moa_core::SubAgentChildRef {
        id: "child-1".to_string(),
        task_hash: "hash-1".to_string(),
        budget_tokens: 0,
        terminal: None,
    });
    state.set_cancel_flag(CancelMode::Hard);
    state.destroy();

    assert_eq!(state, SessionVoState::default());
}

#[test]
fn session_vo_protected_handlers_authorize_before_state_access() {
    // Pins: caller-owned Session VO reads and mutations perform participant authz before state access.
    let source = include_str!("../src/objects/session/handlers.rs");

    assert_handler_authz_before_state_access(
        source,
        "async fn cancel(",
        "SessionVoState::load_from(&ctx).await?",
    );
    assert_handler_authz_before_state_access(
        source,
        "async fn snapshot(",
        "load_pending_state(&ctx).await?",
    );
}

#[test]
fn session_vo_set_meta_documents_internal_initialization_boundary() {
    // Pins: set_meta is classified explicitly because it initializes VO hot state from SessionStore.
    let source = include_str!("../src/objects/session/handlers.rs");
    let lines: Vec<_> = source.lines().collect();
    let set_meta_line = lines
        .iter()
        .position(|line| line.contains("async fn set_meta("))
        .expect("set_meta handler signature should exist");

    assert!(
        lines[set_meta_line - 1]
            .trim_start()
            .starts_with("// SAFETY: internal SessionStore initialization only;"),
        "set_meta must carry a one-line SAFETY comment immediately above the handler signature"
    );
}

fn assert_handler_authz_before_state_access(
    source: &str,
    handler_signature: &str,
    protected_state_access: &str,
) {
    let handler_start = source
        .find(handler_signature)
        .unwrap_or_else(|| panic!("{handler_signature} handler should exist"));
    let handler_source = &source[handler_start..];
    let authz_pos = handler_source
        .find("require_session_participant(&ctx, session_id).await?")
        .unwrap_or_else(|| panic!("{handler_signature} should require session participant authz"));
    let state_access_pos = handler_source
        .find(protected_state_access)
        .unwrap_or_else(|| panic!("{handler_signature} should access protected state"));

    assert!(
        authz_pos < state_access_pos,
        "{handler_signature} must authorize before {protected_state_access}"
    );
}

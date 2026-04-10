//! Shared trait-level session store tests that run against any backend.

use chrono::Utc;
use moa_core::{
    ApprovalRule, Event, EventFilter, EventRange, EventType, PendingSignal, PendingSignalType,
    PolicyAction, PolicyScope, SessionFilter, SessionMeta, SessionStatus, SessionStore, UserId,
    UserMessage, WorkspaceId,
};
use moa_security::ApprovalRuleStore;
use serde_json::json;
use uuid::Uuid;

fn test_session_meta(workspace: &str) -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new(workspace),
        user_id: UserId::new("u1"),
        model: "test-model".to_string(),
        ..SessionMeta::default()
    }
}

/// Verifies session creation, event append, and aggregate counters.
pub async fn test_create_and_get_session<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    let session_id = store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create session");

    let seq1 = store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "Hello".into(),
                attachments: vec![],
            },
        )
        .await
        .expect("emit user message");
    assert_eq!(seq1, 0);

    let seq2 = store
        .emit_event(
            session_id.clone(),
            Event::BrainResponse {
                text: "Hi there".into(),
                model: "test".into(),
                input_tokens: 10,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 100,
            },
        )
        .await
        .expect("emit assistant response");
    assert_eq!(seq2, 1);

    let events = store
        .get_events(session_id.clone(), EventRange::all())
        .await
        .expect("get events");
    assert_eq!(events.len(), 2);

    let session = store.get_session(session_id).await.expect("get session");
    assert_eq!(session.event_count, 2);
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_cost_cents, 1);
}

/// Verifies ranged event reads.
pub async fn test_emit_and_get_events<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    let session_id = store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create session");

    for index in 0..10 {
        store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("message {index}"),
                    attachments: vec![],
                },
            )
            .await
            .expect("emit message");
    }

    let ranged = store
        .get_events(
            session_id.clone(),
            EventRange {
                from_seq: Some(3),
                to_seq: Some(7),
                event_types: None,
                limit: None,
            },
        )
        .await
        .expect("get ranged events");
    assert_eq!(ranged.len(), 5);
    assert_eq!(ranged[0].sequence_num, 3);
    assert_eq!(ranged[4].sequence_num, 7);

    let filtered = store
        .get_events(
            session_id,
            EventRange {
                event_types: Some(vec![EventType::UserMessage]),
                ..Default::default()
            },
        )
        .await
        .expect("get filtered events");
    assert_eq!(filtered.len(), 10);
    assert!(
        filtered
            .iter()
            .all(|record| record.event_type == EventType::UserMessage)
    );
}

/// Verifies event search, including hyphenated queries.
pub async fn test_event_search<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    let session_id = store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create session");

    store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "Fix the OAuth refresh token bug".into(),
                attachments: vec![],
            },
        )
        .await
        .expect("emit oauth event");
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Debug the refresh-token rotation failure".into(),
                attachments: vec![],
            },
        )
        .await
        .expect("emit hyphen event");

    let oauth = store
        .search_events("OAuth refresh", EventFilter::default())
        .await
        .expect("search oauth");
    assert!(!oauth.is_empty());

    let hyphen = store
        .search_events("refresh-token", EventFilter::default())
        .await
        .expect("search hyphen");
    assert!(hyphen.iter().any(|record| matches!(
        &record.event,
        Event::UserMessage { text, .. } if text.contains("refresh-token")
    )));
}

/// Verifies persisted session status updates.
pub async fn test_session_status_update<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    let session_id = store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create session");

    store
        .update_status(session_id.clone(), SessionStatus::Completed)
        .await
        .expect("update status");

    let session = store.get_session(session_id).await.expect("get session");
    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());
}

/// Verifies workspace-filtered session listing.
pub async fn test_list_sessions_with_filter<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create ws1");
    store
        .create_session(test_session_meta("ws2"))
        .await
        .expect("create ws2");

    let sessions = store
        .list_sessions(SessionFilter {
            workspace_id: Some(WorkspaceId::new("ws1")),
            ..Default::default()
        })
        .await
        .expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].workspace_id, WorkspaceId::new("ws1"));
}

/// Verifies pending signal persistence and resolution.
pub async fn test_pending_signals<S>(store: &S)
where
    S: SessionStore + ?Sized,
{
    let session_id = store
        .create_session(test_session_meta("ws1"))
        .await
        .expect("create session");

    let signal = PendingSignal::queue_message(
        session_id.clone(),
        UserMessage {
            text: "queued follow-up".into(),
            attachments: vec![],
        },
    )
    .expect("build pending signal");

    let signal_id = store
        .store_pending_signal(session_id.clone(), signal.clone())
        .await
        .expect("store pending signal");
    assert_eq!(signal_id, signal.id);

    let pending = store
        .get_pending_signals(session_id.clone())
        .await
        .expect("get pending");
    assert_eq!(pending, vec![signal.clone()]);
    assert_eq!(pending[0].signal_type, PendingSignalType::QueueMessage);
    assert_eq!(
        pending[0].payload,
        json!({"text":"queued follow-up","attachments":[]})
    );

    store
        .resolve_pending_signal(signal_id)
        .await
        .expect("resolve pending");
    let pending = store
        .get_pending_signals(session_id)
        .await
        .expect("get pending after resolution");
    assert!(pending.is_empty());
}

/// Verifies persistent approval-rule CRUD.
pub async fn test_approval_rules<S>(store: &S)
where
    S: ApprovalRuleStore + ?Sized,
{
    let workspace_id = WorkspaceId::new("ws1");
    let rule = ApprovalRule {
        id: Uuid::new_v4(),
        workspace_id: workspace_id.clone(),
        tool: "bash".to_string(),
        pattern: "git status".to_string(),
        action: PolicyAction::Allow,
        scope: PolicyScope::Workspace,
        created_by: UserId::new("u1"),
        created_at: Utc::now(),
    };

    store
        .upsert_approval_rule(rule.clone())
        .await
        .expect("upsert approval rule");
    let rules = store
        .list_approval_rules(&workspace_id)
        .await
        .expect("list approval rules");
    assert!(rules.iter().any(|candidate| candidate.id == rule.id));

    store
        .delete_approval_rule(&workspace_id, &rule.tool, &rule.pattern)
        .await
        .expect("delete approval rule");
    let rules = store
        .list_approval_rules(&workspace_id)
        .await
        .expect("list approval rules after delete");
    assert!(!rules.iter().any(|candidate| candidate.id == rule.id));
}

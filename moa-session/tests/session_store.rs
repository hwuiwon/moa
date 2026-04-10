use moa_core::{
    Event, EventFilter, EventRange, EventType, MoaConfig, SessionFilter, SessionMeta,
    SessionStatus, SessionStore,
};
use moa_session::TursoSessionStore;
use tempfile::tempdir;

#[tokio::test]
async fn create_session_and_emit_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();

    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let seq1 = store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "Hello".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();
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
        .unwrap();
    assert_eq!(seq2, 1);

    let events = store
        .get_events(session_id.clone(), EventRange::all())
        .await
        .unwrap();
    assert_eq!(events.len(), 2);

    let session = store.get_session(session_id).await.unwrap();
    assert_eq!(session.event_count, 2);
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_cost_cents, 1);
}

#[tokio::test]
async fn get_events_with_range_filter() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

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
            .unwrap();
    }

    let events = store
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(3),
                to_seq: Some(7),
                event_types: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].sequence_num, 3);
    assert_eq!(events[4].sequence_num, 7);
}

#[tokio::test]
async fn get_events_filtered_by_type() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "hello".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    store
        .emit_event(
            session_id.clone(),
            Event::BrainResponse {
                text: "hi".into(),
                model: "test".into(),
                input_tokens: 1,
                output_tokens: 1,
                cost_cents: 1,
                duration_ms: 1,
            },
        )
        .await
        .unwrap();
    store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "bye".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();

    let events = store
        .get_events(
            session_id,
            EventRange {
                event_types: Some(vec![EventType::UserMessage]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event.event_type, EventType::UserMessage);
    }
}

#[tokio::test]
async fn wake_finds_checkpoint_and_recent_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    for index in 0..5 {
        store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("before checkpoint {index}"),
                    attachments: vec![],
                },
            )
            .await
            .unwrap();
    }

    store
        .emit_event(
            session_id.clone(),
            Event::Checkpoint {
                summary: "checkpoint summary".into(),
                events_summarized: 5,
                token_count: 20,
            },
        )
        .await
        .unwrap();

    for index in 0..3 {
        store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("after checkpoint {index}"),
                    attachments: vec![],
                },
            )
            .await
            .unwrap();
    }

    let wake_ctx = store.wake(session_id).await.unwrap();
    assert_eq!(
        wake_ctx.checkpoint_summary.as_deref(),
        Some("checkpoint summary")
    );
    assert_eq!(wake_ctx.recent_events.len(), 3);
}

#[tokio::test]
async fn wake_without_checkpoint_returns_all_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    for index in 0..5 {
        store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("event {index}"),
                    attachments: vec![],
                },
            )
            .await
            .unwrap();
    }

    let wake_ctx = store.wake(session_id).await.unwrap();
    assert!(wake_ctx.checkpoint_summary.is_none());
    assert_eq!(wake_ctx.recent_events.len(), 5);
}

#[tokio::test]
async fn fts_search_finds_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Fix the OAuth refresh token bug".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();

    let results = store
        .search_events("OAuth refresh", EventFilter::default())
        .await
        .unwrap();
    assert!(!results.is_empty());
    assert!(matches!(
        &results[0].event,
        Event::UserMessage { text, .. } if text.contains("OAuth")
    ));
}

#[tokio::test]
async fn fts_search_handles_hyphenated_queries() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "Debug the refresh-token rotation failure".into(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();

    let results = store
        .search_events("refresh-token", EventFilter::default())
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(matches!(
        &results[0].event,
        Event::UserMessage { text, .. } if text.contains("refresh-token")
    ));
}

#[tokio::test]
async fn list_sessions_filters_by_workspace() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();

    store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create_session(SessionMeta {
            workspace_id: "ws2".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let ws1_sessions = store
        .list_sessions(SessionFilter {
            workspace_id: Some("ws1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ws1_sessions.len(), 1);
    assert_eq!(ws1_sessions[0].workspace_id, "ws1".into());
}

#[tokio::test]
async fn schema_is_idempotent() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let _store1 = TursoSessionStore::new_local(&db_path).await.unwrap();
    let _store2 = TursoSessionStore::new_local(&db_path).await.unwrap();
}

#[tokio::test]
async fn from_config_uses_local_store_when_cloud_sync_is_disabled() {
    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.session_db = dir.path().join("sessions.db").display().to_string();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.cloud.enabled = false;
    config.cloud.turso_url = Some("libsql://example.turso.io".to_string());

    let store = TursoSessionStore::from_config(&config).await.unwrap();
    assert!(!store.cloud_sync_enabled());
}

#[tokio::test]
async fn cloud_sync_requires_file_backed_session_db() {
    let mut config = MoaConfig::default();
    config.local.session_db = ":memory:".to_string();
    config.cloud.enabled = true;
    config.cloud.turso_url = Some("libsql://example.turso.io".to_string());
    unsafe {
        std::env::set_var("TURSO_AUTH_TOKEN", "test-token");
    }

    let error = match TursoSessionStore::from_config(&config).await {
        Ok(_) => panic!("cloud sync should reject in-memory databases"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("file-backed"));
}

#[tokio::test]
async fn update_status_persists_changes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .update_status(session_id.clone(), SessionStatus::Completed)
        .await
        .unwrap();

    let session = store.get_session(session_id).await.unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    assert!(session.completed_at.is_some());
}

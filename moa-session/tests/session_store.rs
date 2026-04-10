#![cfg(feature = "turso")]

mod shared;

use moa_core::{Event, MoaConfig, PendingSignal, SessionMeta, SessionStore, UserMessage};
use moa_session::TursoSessionStore;
use tempfile::tempdir;

async fn new_local_store() -> TursoSessionStore {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.keep().join("test.db");
    TursoSessionStore::new_local(&db_path)
        .await
        .expect("local store")
}

#[tokio::test]
async fn create_session_and_emit_events() {
    let store = new_local_store().await;
    shared::test_create_and_get_session(&store).await;
}

#[tokio::test]
async fn get_events_with_range_filter() {
    let store = new_local_store().await;
    shared::test_emit_and_get_events(&store).await;
}

#[tokio::test]
async fn pending_signal_round_trip_and_resolution() {
    let store = new_local_store().await;
    shared::test_pending_signals(&store).await;
}

#[tokio::test]
async fn fts_search_finds_events() {
    let store = new_local_store().await;
    shared::test_event_search(&store).await;
}

#[tokio::test]
async fn list_sessions_filters_by_workspace() {
    let store = new_local_store().await;
    shared::test_list_sessions_with_filter(&store).await;
}

#[tokio::test]
async fn update_status_persists_changes() {
    let store = new_local_store().await;
    shared::test_session_status_update(&store).await;
}

#[tokio::test]
async fn approval_rules_round_trip() {
    let store = new_local_store().await;
    shared::test_approval_rules(&store).await;
}

#[tokio::test]
async fn wake_finds_checkpoint_and_recent_events() {
    let store = new_local_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .expect("create session");

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
            .expect("emit pre-checkpoint event");
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
        .expect("emit checkpoint");

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
            .expect("emit post-checkpoint event");
    }

    let wake_ctx = store.wake(session_id).await.expect("wake");
    assert_eq!(
        wake_ctx.checkpoint_summary.as_deref(),
        Some("checkpoint summary")
    );
    assert_eq!(wake_ctx.recent_events.len(), 3);
}

#[tokio::test]
async fn wake_without_checkpoint_returns_all_events() {
    let store = new_local_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .expect("create session");

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
            .expect("emit event");
    }

    let wake_ctx = store.wake(session_id).await.expect("wake");
    assert!(wake_ctx.checkpoint_summary.is_none());
    assert_eq!(wake_ctx.recent_events.len(), 5);
}

#[tokio::test]
async fn wake_returns_unresolved_pending_signals() {
    let store = new_local_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: "ws1".into(),
            user_id: "u1".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await
        .expect("create session");

    let unresolved = PendingSignal::queue_message(
        session_id.clone(),
        UserMessage {
            text: "recover me".into(),
            attachments: vec![],
        },
    )
    .expect("build unresolved");
    let resolved = PendingSignal::queue_message(
        session_id.clone(),
        UserMessage {
            text: "already flushed".into(),
            attachments: vec![],
        },
    )
    .expect("build resolved");

    store
        .store_pending_signal(session_id.clone(), unresolved.clone())
        .await
        .expect("store unresolved");
    let resolved_id = store
        .store_pending_signal(session_id.clone(), resolved)
        .await
        .expect("store resolved");
    store
        .resolve_pending_signal(resolved_id)
        .await
        .expect("resolve signal");

    let wake_ctx = store.wake(session_id).await.expect("wake");
    assert_eq!(wake_ctx.pending_signals, vec![unresolved]);
}

#[tokio::test]
async fn schema_is_idempotent() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let _store1 = TursoSessionStore::new_local(&db_path)
        .await
        .expect("first store");
    let _store2 = TursoSessionStore::new_local(&db_path)
        .await
        .expect("second store");
}

#[tokio::test]
async fn from_config_uses_local_store_when_cloud_sync_is_disabled() {
    let dir = tempdir().expect("tempdir");
    let mut config = MoaConfig::default();
    config.database.url = dir.path().join("sessions.db").display().to_string();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.cloud.enabled = false;
    config.cloud.turso_url = Some("libsql://example.turso.io".to_string());

    let store = TursoSessionStore::from_config(&config)
        .await
        .expect("store from config");
    assert!(!store.cloud_sync_enabled());
}

#[tokio::test]
async fn cloud_sync_requires_file_backed_session_db() {
    let mut config = MoaConfig::default();
    config.database.url = ":memory:".to_string();
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
async fn legacy_local_session_db_alias_is_resolved_into_database_url() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[local]\nsession_db = \"{}\"\n",
            dir.path().join("legacy.db").display()
        ),
    )
    .expect("write config");

    let config = MoaConfig::load_from_path(&config_path).expect("load config");
    assert!(config.local.session_db.ends_with("legacy.db"));
    assert!(config.database.url.ends_with("legacy.db"));
}

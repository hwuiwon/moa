use std::error::Error;
use std::time::Duration;

use libsql::Builder;
use moa_core::{Event, EventFilter, EventRange, SessionMeta, SessionStore};
use moa_session::TursoSessionStore;
use tempfile::tempdir;
use uuid::Uuid;

fn live_turso_env() -> Option<(String, String)> {
    let url = std::env::var("TURSO_DATABASE_URL").ok()?;
    let token = std::env::var("TURSO_AUTH_TOKEN").ok()?;
    Some((url, token))
}

async fn cleanup_remote_sessions(
    url: &str,
    token: &str,
    session_ids: &[String],
) -> Result<(), Box<dyn Error>> {
    let database = Builder::new_remote(url.to_string(), token.to_string())
        .build()
        .await?;
    let connection = database.connect()?;
    for session_id in session_ids {
        connection
            .execute(
                "DELETE FROM events WHERE session_id = ?",
                [session_id.clone()],
            )
            .await?;
        connection
            .execute("DELETE FROM sessions WHERE id = ?", [session_id.clone()])
            .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual live Turso test"]
async fn embedded_replica_roundtrips_local_and_remote_writes() -> Result<(), Box<dyn Error>> {
    let Some((url, token)) = live_turso_env() else {
        return Ok(());
    };

    println!("bootstrapping embedded replica");
    let dir = tempdir()?;
    let local_path = dir.path().join("replica.db");
    let local_store =
        TursoSessionStore::new_remote_replica(&local_path, &url, &token, Duration::from_secs(1))
            .await?;
    println!("opening remote primary");
    let remote_store = TursoSessionStore::new(&url).await?;
    assert!(local_store.cloud_sync_enabled());

    let tag = Uuid::new_v4().to_string();
    println!("writing local session {tag}");
    let local_session = local_store
        .create_session(SessionMeta {
            workspace_id: format!("turso-live-local-{tag}").into(),
            user_id: "turso-live-user".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await?;
    let local_text = format!("local-to-remote {tag}");
    local_store
        .emit_event(
            local_session.clone(),
            Event::UserMessage {
                text: local_text.clone(),
                attachments: vec![],
            },
        )
        .await?;
    println!("syncing local session to remote");
    local_store.sync_now().await?;

    println!("verifying local->remote replication");
    let remote_session = remote_store.get_session(local_session.clone()).await?;
    assert_eq!(remote_session.event_count, 1);
    let remote_events = remote_store
        .get_events(local_session.clone(), EventRange::all())
        .await?;
    assert_eq!(remote_events.len(), 1);
    assert!(matches!(
        &remote_events[0].event,
        Event::UserMessage { text, .. } if text == &local_text
    ));

    println!("writing remote session");
    let remote_session_id = remote_store
        .create_session(SessionMeta {
            workspace_id: format!("turso-live-remote-{tag}").into(),
            user_id: "turso-live-user".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await?;
    let remote_text = format!("remote-to-local {tag}");
    remote_store
        .emit_event(
            remote_session_id.clone(),
            Event::UserMessage {
                text: remote_text.clone(),
                attachments: vec![],
            },
        )
        .await?;

    println!("syncing remote session back to local replica");
    local_store.sync_now().await?;
    let local_remote_session = local_store.get_session(remote_session_id.clone()).await?;
    assert_eq!(local_remote_session.event_count, 1);
    let search_results = local_store
        .search_events(&tag, EventFilter::default())
        .await?;
    assert!(
        search_results.iter().any(|record| {
            record.session_id == local_session || record.session_id == remote_session_id
        }),
        "expected synced sessions to appear in local FTS search"
    );

    println!("cleaning up remote test sessions");
    cleanup_remote_sessions(
        &url,
        &token,
        &[local_session.to_string(), remote_session_id.to_string()],
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual live Turso test"]
async fn embedded_replica_sync_preserves_checkpoint_wake() -> Result<(), Box<dyn Error>> {
    let Some((url, token)) = live_turso_env() else {
        return Ok(());
    };

    println!("bootstrapping embedded replica for wake test");
    let dir = tempdir()?;
    let local_path = dir.path().join("replica.db");
    let local_store =
        TursoSessionStore::new_remote_replica(&local_path, &url, &token, Duration::from_secs(1))
            .await?;
    println!("opening remote primary for wake test");
    let remote_store = TursoSessionStore::new(&url).await?;

    let tag = Uuid::new_v4().to_string();
    println!("creating remote checkpointed session {tag}");
    let session_id = remote_store
        .create_session(SessionMeta {
            workspace_id: format!("turso-live-wake-{tag}").into(),
            user_id: "turso-live-user".into(),
            model: "test-model".into(),
            ..Default::default()
        })
        .await?;

    for index in 0..3 {
        remote_store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("before checkpoint {tag} {index}"),
                    attachments: vec![],
                },
            )
            .await?;
    }

    let checkpoint_summary = format!("checkpoint {tag}");
    remote_store
        .emit_event(
            session_id.clone(),
            Event::Checkpoint {
                summary: checkpoint_summary.clone(),
                events_summarized: 3,
                token_count: 42,
            },
        )
        .await?;

    for index in 0..2 {
        remote_store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: format!("after checkpoint {tag} {index}"),
                    attachments: vec![],
                },
            )
            .await?;
    }

    println!("syncing checkpointed session to local replica");
    local_store.sync_now().await?;
    let wake = local_store.wake(session_id.clone()).await?;
    assert_eq!(
        wake.checkpoint_summary.as_deref(),
        Some(checkpoint_summary.as_str())
    );
    assert_eq!(wake.recent_events.len(), 2);
    assert!(wake.recent_events.iter().all(|record| matches!(
        &record.event,
        Event::UserMessage { text, .. } if text.contains("after checkpoint")
    )));

    println!("cleaning up wake test session");
    cleanup_remote_sessions(&url, &token, &[session_id.to_string()]).await?;
    Ok(())
}

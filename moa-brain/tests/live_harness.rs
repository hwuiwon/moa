//! Live integration coverage for the Step 04 brain harness.

use std::sync::Arc;

use moa_brain::{TurnResult, build_default_pipeline, run_brain_turn};
use moa_core::{Event, EventRange, Result, SessionMeta, SessionStore, UserId, WorkspaceId};
use moa_providers::AnthropicProvider;
use moa_session::TursoSessionStore;
use tempfile::tempdir;

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY"]
async fn live_brain_turn_returns_brain_response() -> Result<()> {
    let dir = tempdir()?;
    let db_path = dir.path().join("brain-harness.db");
    let store = Arc::new(TursoSessionStore::new_local(&db_path).await?);
    let provider = Arc::new(AnthropicProvider::from_env("claude-sonnet-4-6")?);
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("live-harness"),
            user_id: UserId::new("integration-test"),
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        })
        .await?;
    let pipeline = build_default_pipeline(&moa_core::MoaConfig::default(), store.clone());

    store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "What is 2+2? Respond with just the answer.".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    let turn_result =
        run_brain_turn(session_id.clone(), store.clone(), provider, &pipeline).await?;
    let events = store.get_events(session_id, EventRange::all()).await?;
    let response_text = events.into_iter().find_map(|record| match record.event {
        Event::BrainResponse { text, .. } => Some(text),
        _ => None,
    });

    assert_eq!(turn_result, TurnResult::Complete);
    assert!(response_text.is_some(), "expected a BrainResponse event");
    assert!(response_text.unwrap_or_default().contains('4'));

    Ok(())
}

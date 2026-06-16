//! Wiremock offline coverage for a brain turn through the OpenAI provider path.

mod support;

use std::sync::Arc;

use moa_brain::{TurnResult, build_default_pipeline, run_brain_turn};
use moa_core::{Event, EventRange, LLMProvider, MoaConfig, SessionStore};
use moa_providers::OpenAIProvider;
use wiremock::MockServer;

use support::{MockSessionStore, captured_json_bodies, mount_openai_text, session_meta};

#[tokio::test]
async fn offline_brain_turn_returns_response() -> moa_core::Result<()> {
    let server = MockServer::start().await;
    mount_openai_text(&server, "4", 0).await;

    let mut config = MoaConfig::default();
    config.general.default_provider = "openai".to_string();
    config.models.main = "gpt-5.4".to_string();
    config.query_rewrite.enabled = false;

    let provider: Arc<dyn LLMProvider> = Arc::new(
        OpenAIProvider::new("test-key", "gpt-5.4")?
            .with_api_base(format!("{}/v1", server.uri()))?,
    );
    let session = session_meta("offline-brain-turn", "gpt-5.4");
    let session_id = session.id;
    let store = Arc::new(MockSessionStore::new(session, Vec::new()));
    let pipeline = build_default_pipeline(&config, store.clone());

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "What is 2+2? Respond with just the answer.".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    let turn_result = run_brain_turn(session_id, store.clone(), provider, &pipeline, None).await?;
    let events = store.get_events(session_id, EventRange::all()).await?;
    let response_text = events.into_iter().find_map(|record| match record.event {
        Event::BrainResponse { text, .. } => Some(text),
        _ => None,
    });

    assert_eq!(turn_result, TurnResult::Complete);
    assert_eq!(response_text.as_deref(), Some("4"));
    let bodies = captured_json_bodies(&server).await;
    assert!(
        bodies
            .iter()
            .any(|body| body.to_string().contains("What is 2+2?"))
    );

    Ok(())
}

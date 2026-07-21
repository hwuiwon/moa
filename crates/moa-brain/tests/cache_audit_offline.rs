//! Wiremock offline counterpart for prompt-cache audit live coverage.

#[path = "support/offline_session_store.rs"]
mod offline_session_store;
#[path = "support/openai_wiremock.rs"]
mod openai_wiremock;

include!("brain_turn_support/pipeline.rs");

use std::sync::Arc;

use moa_brain::{BrainTurnRequest, TurnResult, run_brain_turn};
use moa_core::{
    config::MoaConfig, events::Event, traits::LLMProvider, traits::SessionStore,
    types::events_stream::EventRange,
};
use moa_providers::OpenAIProvider;
use wiremock::MockServer;

use offline_session_store::{MockSessionStore, session_meta};
use openai_wiremock::{captured_json_bodies, mount_openai_text};

#[tokio::test]
async fn cache_audit_offline_tracks_stable_prefix_reuse_and_cached_usage()
-> moa_core::error::Result<()> {
    let server = MockServer::start().await;
    mount_openai_text(&server, "READY", 8).await;

    let mut config = MoaConfig::default();
    config.general.default_provider = "openai".to_string();
    config.models.main = "gpt-5.4".to_string();
    config.query_rewrite.enabled = false;
    config.general.workspace_instructions =
        Some("Offline cache audit stable instruction block.\n".repeat(24));

    let provider: Arc<dyn LLMProvider> = Arc::new(
        OpenAIProvider::new("test-key", "gpt-5.4")?
            .with_api_base(format!("{}/v1", server.uri()))?,
    );
    let session = session_meta("offline-cache-audit", "gpt-5.4");
    let identity = test_identity(session.tenant_id);
    let session_id = session.id;
    let store = Arc::new(MockSessionStore::new(session, Vec::new()));
    let pipeline = build_no_memory_test_pipeline(&config, store.clone());

    for prompt in [
        "Reply with READY and nothing else.",
        "Reply with STEADY and nothing else.",
    ] {
        store
            .emit_event(
                session_id,
                Event::UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                },
            )
            .await?;
        assert_eq!(
            run_brain_turn(BrainTurnRequest {
                identity: identity.clone(),
                session_id,
                session_store: store.clone(),
                llm_provider: provider.clone(),
                pipeline: &pipeline,
                tool_router: None,
            })
            .await?,
            TurnResult::Complete
        );
    }

    let bodies = captured_json_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    let first_key = bodies[0]["prompt_cache_key"]
        .as_str()
        .expect("first request should carry prompt_cache_key");
    let second_key = bodies[1]["prompt_cache_key"]
        .as_str()
        .expect("second request should carry prompt_cache_key");
    assert_eq!(first_key, second_key);

    let brain_responses = store
        .get_events(session_id, EventRange::all())
        .await?
        .into_iter()
        .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
        .count();
    assert_eq!(brain_responses, 2);

    Ok(())
}

fn test_identity(tenant_id: moa_core::types::identifiers::TenantId) -> moa_core::traits::Identity {
    moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c412),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

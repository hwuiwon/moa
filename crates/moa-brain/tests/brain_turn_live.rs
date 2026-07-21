// Live counterpart: see brain_turn_offline.rs for the wiremock version that runs in PR CI.

//! Live integration coverage for a brain turn through the real provider path.

use std::sync::Arc;

use moa_brain::{
    BrainTurnRequest, GraphMemoryPipelineOptions, TurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions, run_brain_turn,
};
use moa_config::MoaConfig;
use moa_core::{
    error::Result, events::Event, traits::Identity, traits::IdentityType, traits::LLMProvider,
    traits::SessionStore, types::contact::SessionActorRef, types::events_stream::EventRange,
    types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_providers::{build_provider_from_config, resolve_provider_selection};
use moa_session::testing;

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
async fn live_brain_turn_completes() -> Result<()> {
    if !env_flag_enabled("MOA_RUN_LIVE_PROVIDER_TESTS") {
        return Ok(());
    }

    let mut config = MoaConfig::load()?;
    let (provider_id, model_id) = resolve_provider_selection(&config, None)?;
    config.general.default_provider = provider_id.as_str().to_string();
    config.models.main = model_id.as_str().to_string();
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);
    let provider: Arc<dyn LLMProvider> = build_provider_from_config(&config)?;
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: uuid::Uuid::now_v7(),
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let session_id = store
        .create_session(SessionMeta {
            tenant_id: identity.tenant_id,
            model: config.models.main.clone().into(),
            created_by: Some(SessionActorRef::Identity { id: identity.id }),
            ..SessionMeta::default()
        })
        .await?;
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: store.pool().clone(),
            kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            segment_store: Some(store.clone()),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            identity_prompt_override: None,
            tool_schemas: Vec::new(),
            lineage: Arc::new(moa_core::traits::NullLineageHandle),
        },
    );

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "What is 2+2? Respond with just the answer.".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    let turn_result = run_brain_turn(BrainTurnRequest {
        identity,
        session_id,
        session_store: store.clone(),
        llm_provider: provider,
        pipeline: &pipeline,
        tool_router: None,
    })
    .await?;
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

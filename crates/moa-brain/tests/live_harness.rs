//! Live integration coverage for the Step 04 brain harness.

use std::sync::Arc;

use moa_brain::{
    GraphMemoryPipelineOptions, TurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions, run_brain_turn,
};
use moa_core::{
    Event, EventRange, LLMProvider, MoaConfig, Result, SessionMeta, SessionStore, UserId,
    WorkspaceId,
};
use moa_providers::{build_provider_from_config, resolve_provider_selection};
use moa_session::testing;

#[tokio::test]
#[ignore = "requires provider API key env"]
async fn live_brain_turn_returns_brain_response() -> Result<()> {
    let mut config = MoaConfig::default();
    let selection = resolve_provider_selection(&config, None)?;
    config.general.default_provider = selection.provider_name;
    config.general.default_model = selection.model_id.clone();
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);
    let provider: Arc<dyn LLMProvider> = build_provider_from_config(&config)?;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("live-harness"),
            user_id: UserId::new("integration-test"),
            model: config.general.default_model.clone().into(),
            ..SessionMeta::default()
        })
        .await?;
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: store.pool().clone(),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            discovered_workspace_instructions: None,
            tool_schemas: Vec::new(),
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

    let turn_result = run_brain_turn(session_id, store.clone(), provider, &pipeline).await?;
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

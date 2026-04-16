//! Stable cached-prefix coverage for the prompt compilation pipeline.

use std::sync::Arc;

use async_trait::async_trait;
use moa_brain::{TurnResult, build_default_pipeline_with_tools, run_brain_turn_with_tools};
use moa_core::{
    CompletionRequest, Event, MemoryPath, MemoryScope, MemorySearchResult, MemoryStore,
    ModelCapabilities, PageSummary, PageType, Result, SessionMeta, SessionStore, TokenPricing,
    ToolCallFormat, UserId, WikiPage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_providers::ScriptedProvider;
use moa_session::testing;
use serde_json::json;
use tempfile::TempDir;

#[derive(Default)]
struct NoopMemoryStore;

#[async_trait]
impl MemoryStore for NoopMemoryStore {
    async fn search(
        &self,
        _query: &str,
        _scope: MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Ok(Vec::new())
    }

    async fn read_page(&self, _scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
        Err(moa_core::MoaError::StorageError(format!(
            "memory page not found: {}",
            path.as_str()
        )))
    }

    async fn write_page(
        &self,
        _scope: MemoryScope,
        _path: &MemoryPath,
        _page: WikiPage,
    ) -> Result<()> {
        Ok(())
    }

    async fn delete_page(&self, _scope: MemoryScope, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    async fn list_pages(
        &self,
        _scope: MemoryScope,
        _filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        Ok(Vec::new())
    }

    async fn get_index(&self, _scope: MemoryScope) -> Result<String> {
        Ok(String::new())
    }

    async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn system_prompt_bytes_are_stable_across_compiles() -> Result<()> {
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;
    tokio::fs::write(
        workspace.join("AGENTS.md"),
        "Follow the cached-prefix rules.\n",
    )
    .await?;

    let mut config = moa_core::MoaConfig::default();
    config.general.default_model = "claude-sonnet-4-6".to_string();

    let memory_store: Arc<dyn MemoryStore> = Arc::new(NoopMemoryStore);
    let (session_store, _database_url, _schema_name) =
        testing::create_isolated_test_store().await?;
    let session_store: Arc<dyn SessionStore> = Arc::new(session_store);
    let workspace_id = WorkspaceId::new("stable-prefix");
    let user_id = UserId::new("stable-prefix-user");
    let router = Arc::new(ToolRouter::new_local(memory_store.clone(), &workspace).await?);
    router
        .remember_workspace_root(workspace_id.clone(), workspace.clone())
        .await;

    let provider = Arc::new(scripted_provider());
    let pipeline = build_default_pipeline_with_tools(
        &config,
        session_store.clone(),
        memory_store,
        extend_tool_schemas(router.tool_schemas()),
    );

    let first_session_id = session_store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            model: config.general.default_model.clone(),
            ..SessionMeta::default()
        })
        .await?;
    session_store
        .emit_event(
            first_session_id.clone(),
            Event::UserMessage {
                text: "First request".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    assert_eq!(
        run_brain_turn_with_tools(
            first_session_id.clone(),
            session_store.clone(),
            provider.clone(),
            &pipeline,
            Some(router.clone()),
        )
        .await?,
        TurnResult::Complete
    );

    let second_session_id = session_store
        .create_session(SessionMeta {
            workspace_id,
            user_id,
            model: config.general.default_model.clone(),
            ..SessionMeta::default()
        })
        .await?;
    session_store
        .emit_event(
            second_session_id.clone(),
            Event::UserMessage {
                text: "Second request".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    assert_eq!(
        run_brain_turn_with_tools(
            second_session_id,
            session_store,
            provider.clone(),
            &pipeline,
            Some(router),
        )
        .await?,
        TurnResult::Complete
    );

    let requests = provider.recorded_requests().await;
    assert_eq!(requests.len(), 2, "expected exactly two compiled requests");

    assert_eq!(
        stable_prefix_bytes(&requests[0])?,
        stable_prefix_bytes(&requests[1])?
    );

    let reminder = requests[1]
        .messages
        .iter()
        .find(|message| message.content.contains("<system-reminder>"))
        .expect("expected runtime context reminder");
    assert!(reminder.content.contains(&format!(
        "Current working directory: {}",
        workspace.display()
    )));

    Ok(())
}

fn scripted_provider() -> ScriptedProvider {
    ScriptedProvider::new(capabilities())
        .push_text("First response")
        .push_text("Second response")
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: "claude-sonnet-4-6".to_string(),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
        },
        native_tools: Vec::new(),
    }
}

fn extend_tool_schemas(mut schemas: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    schemas.push(json!({
        "name": "dummy_cache_padding",
        "description": "Synthetic padding tool to keep the stable prefix large enough for cache assertions.",
        "input_schema": {
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            }
        }
    }));
    schemas
}

fn stable_prefix_bytes(request: &CompletionRequest) -> Result<Vec<u8>> {
    let stable_message_count = request
        .cache_breakpoints
        .last()
        .copied()
        .unwrap_or_default()
        .min(request.messages.len());
    serde_json::to_vec(&json!({
        "messages": request.messages[..stable_message_count],
        "tools": request.tools,
    }))
    .map_err(Into::into)
}

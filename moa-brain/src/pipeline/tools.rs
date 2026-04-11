//! Stage 3: serializes the fixed tool loadout for the session.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{ContextProcessor, MemoryStore, ProcessorOutput, Result, WorkingContext};
use serde_json::Value;

use crate::tool_stats::{apply_tool_rankings, load_workspace_tool_stats};

use super::{estimate_tokens, sort_json_keys};

/// Injects deterministic tool schemas into the working context.
#[derive(Clone)]
pub struct ToolDefinitionProcessor {
    tool_schemas: Vec<Value>,
    memory_store: Option<Arc<dyn MemoryStore>>,
}

impl ToolDefinitionProcessor {
    /// Creates a tool processor from a fixed list of schemas.
    pub fn new(tool_schemas: Vec<Value>) -> Self {
        Self {
            tool_schemas,
            memory_store: None,
        }
    }

    /// Creates a tool processor that ranks schemas using workspace memory statistics.
    pub fn with_memory(tool_schemas: Vec<Value>, memory_store: Arc<dyn MemoryStore>) -> Self {
        Self {
            tool_schemas,
            memory_store: Some(memory_store),
        }
    }
}

#[async_trait]
impl ContextProcessor for ToolDefinitionProcessor {
    fn name(&self) -> &str {
        "tools"
    }

    fn stage(&self) -> u8 {
        3
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let mut tool_schemas = self.tool_schemas.clone();
        if let Some(memory_store) = &self.memory_store {
            match load_workspace_tool_stats(memory_store.as_ref(), &ctx.workspace_id).await {
                Ok(stats) => {
                    tool_schemas = apply_tool_rankings(tool_schemas, &stats);
                }
                Err(error) => {
                    tracing::warn!(
                        workspace_id = %ctx.workspace_id,
                        error = %error,
                        "failed to load workspace tool stats; using default tool order"
                    );
                }
            }
        }
        for schema in &mut tool_schemas {
            sort_json_keys(schema);
        }
        tool_schemas.truncate(30);

        let tokens_added = tool_schemas
            .iter()
            .map(|schema| estimate_tokens(&schema.to_string()))
            .sum();
        let items_included = tool_schemas
            .iter()
            .filter_map(|schema| schema.get("name").and_then(Value::as_str))
            .map(ToString::to_string)
            .collect();

        ctx.set_tools(tool_schemas);

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        MemoryStore, ModelCapabilities, PageSummary, PageType, Platform, SessionId, SessionMeta,
        TokenPricing, ToolCallFormat, UserId, WikiPage, WorkspaceId,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::tool_stats::{WorkspaceToolStats, write_workspace_tool_stats};

    #[tokio::test]
    async fn tool_processor_serializes_tool_schemas() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: "claude-sonnet-4-6".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
        };
        let mut ctx = WorkingContext::new(&session, capabilities);

        let output = ToolDefinitionProcessor::new(vec![json!({
            "description": "Run a shell command",
            "name": "bash",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string"}
                }
            }
        })])
        .process(&mut ctx)
        .await
        .unwrap();

        assert_eq!(ctx.tools()[0]["name"], "bash");
        assert_eq!(output.items_included, vec!["bash".to_string()]);
        assert!(output.tokens_added > 0);
    }

    #[tokio::test]
    async fn ranked_tools_prefer_successful_workspace_tools() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: "claude-sonnet-4-6".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
        };
        let memory_store = Arc::new(StaticMemoryStore::default());
        write_workspace_tool_stats(
            memory_store.as_ref(),
            &session.workspace_id,
            &WorkspaceToolStats {
                tools: HashMap::from([
                    (
                        "bash".to_string(),
                        crate::tool_stats::ToolStats {
                            tool_name: "bash".to_string(),
                            total_calls: 20,
                            ema_success_rate: 0.99,
                            ..crate::tool_stats::ToolStats::default()
                        },
                    ),
                    (
                        "web_search".to_string(),
                        crate::tool_stats::ToolStats {
                            tool_name: "web_search".to_string(),
                            total_calls: 20,
                            ema_success_rate: 0.55,
                            failures: 9,
                            common_errors: vec![("timeout".to_string(), 4)],
                            ..crate::tool_stats::ToolStats::default()
                        },
                    ),
                ]),
                ..WorkspaceToolStats::default()
            },
        )
        .await
        .unwrap();

        let mut ctx = WorkingContext::new(&session, capabilities);
        ToolDefinitionProcessor::with_memory(
            vec![
                json!({"name": "web_search", "description": "Search the web"}),
                json!({"name": "bash", "description": "Run shell commands"}),
            ],
            memory_store,
        )
        .process(&mut ctx)
        .await
        .unwrap();

        assert_eq!(ctx.tools()[0]["name"], "bash");
        let description = ctx.tools()[1]["description"].as_str().unwrap();
        assert!(description.contains("Workspace warning"));
    }

    #[derive(Default)]
    struct StaticMemoryStore {
        pages: tokio::sync::Mutex<HashMap<String, WikiPage>>,
    }

    #[async_trait]
    impl MemoryStore for StaticMemoryStore {
        async fn search(
            &self,
            _query: &str,
            _scope: moa_core::MemoryScope,
            _limit: usize,
        ) -> Result<Vec<moa_core::MemorySearchResult>> {
            Ok(Vec::new())
        }

        async fn read_page(
            &self,
            scope: moa_core::MemoryScope,
            path: &moa_core::MemoryPath,
        ) -> Result<WikiPage> {
            let key = page_key(&scope, path);
            self.pages.lock().await.get(&key).cloned().ok_or_else(|| {
                moa_core::MoaError::StorageError(format!(
                    "memory page not found: {}",
                    path.as_str()
                ))
            })
        }

        async fn write_page(
            &self,
            scope: moa_core::MemoryScope,
            path: &moa_core::MemoryPath,
            mut page: WikiPage,
        ) -> Result<()> {
            page.path = Some(path.clone());
            self.pages.lock().await.insert(page_key(&scope, path), page);
            Ok(())
        }

        async fn delete_page(
            &self,
            _scope: moa_core::MemoryScope,
            _path: &moa_core::MemoryPath,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_pages(
            &self,
            _scope: moa_core::MemoryScope,
            _filter: Option<PageType>,
        ) -> Result<Vec<PageSummary>> {
            Ok(Vec::new())
        }

        async fn get_index(&self, _scope: moa_core::MemoryScope) -> Result<String> {
            Ok(String::new())
        }

        async fn rebuild_search_index(&self, _scope: moa_core::MemoryScope) -> Result<()> {
            Ok(())
        }
    }

    fn page_key(scope: &moa_core::MemoryScope, path: &moa_core::MemoryPath) -> String {
        match scope {
            moa_core::MemoryScope::User(user_id) => {
                format!("user:{}:{}", user_id.as_str(), path.as_str())
            }
            moa_core::MemoryScope::Workspace(workspace_id) => {
                format!("workspace:{}:{}", workspace_id.as_str(), path.as_str())
            }
        }
    }
}

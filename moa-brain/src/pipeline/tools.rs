//! Stage 3: serializes the fixed tool loadout for the session.

use moa_core::{ContextProcessor, ProcessorOutput, Result, WorkingContext};
use serde_json::Value;

use super::{estimate_tokens, sort_json_keys};

/// Injects deterministic tool schemas into the working context.
#[derive(Debug, Clone, Default)]
pub struct ToolDefinitionProcessor {
    tool_schemas: Vec<Value>,
}

impl ToolDefinitionProcessor {
    /// Creates a tool processor from a fixed list of schemas.
    pub fn new(tool_schemas: Vec<Value>) -> Self {
        Self { tool_schemas }
    }
}

impl ContextProcessor for ToolDefinitionProcessor {
    fn name(&self) -> &str {
        "tools"
    }

    fn stage(&self) -> u8 {
        3
    }

    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let mut tool_schemas = self.tool_schemas.clone();
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
        ModelCapabilities, Platform, SessionId, SessionMeta, TokenPricing, ToolCallFormat, UserId,
        WorkspaceId,
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn tool_processor_serializes_tool_schemas() {
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
        .unwrap();

        assert_eq!(ctx.metadata["tool_schemas"][0]["name"], "bash");
        assert_eq!(output.items_included, vec!["bash".to_string()]);
        assert!(output.tokens_added > 0);
    }
}

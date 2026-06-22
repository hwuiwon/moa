//! Stage 3: serializes the fixed tool loadout for the session.

use async_trait::async_trait;
use moa_core::{
    AgentToolPolicy, ContextProcessor, ExcludedItem, ProcessorOutput, Result, WorkingContext,
};
use serde_json::Value;

use super::{estimate_tokens, sort_json_keys};

// WARNING: Tool schemas live in the cached prompt prefix.
// Keep ordering deterministic and do not inject workspace- or turn-specific metadata here.

/// Injects deterministic tool schemas into the working context.
#[derive(Clone)]
pub struct ToolDefinitionProcessor {
    tool_schemas: Vec<Value>,
}

impl ToolDefinitionProcessor {
    /// Creates a tool processor from a fixed list of schemas.
    pub fn new(tool_schemas: Vec<Value>) -> Self {
        Self { tool_schemas }
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
        let agent_tool_policy = ctx
            .agent_context
            .as_ref()
            .map(|agent_context| agent_context.parsed_policy_snapshot())
            .transpose()?
            .map(|snapshot| snapshot.tool_policy);
        let mut tool_schemas = Vec::new();
        let mut excluded_items = Vec::new();
        for schema in &self.tool_schemas {
            let name = tool_name(schema);
            if agent_allows_tool(agent_tool_policy.as_ref(), name) {
                tool_schemas.push(schema.clone());
            } else {
                excluded_items.push(ExcludedItem {
                    item: name.to_string(),
                    reason: "denied by pinned agent tool policy".to_string(),
                });
            }
        }
        for schema in &mut tool_schemas {
            sort_json_keys(schema);
        }
        tool_schemas.sort_by(|left, right| tool_name(left).cmp(tool_name(right)));
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
        let items_excluded = excluded_items
            .iter()
            .map(|item| item.item.clone())
            .collect::<Vec<_>>();

        ctx.set_tools(tool_schemas);

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            items_excluded,
            excluded_items,
            ..ProcessorOutput::default()
        })
    }
}

fn agent_allows_tool(tool_policy: Option<&AgentToolPolicy>, tool_name: &str) -> bool {
    match tool_policy {
        Some(policy) => policy.allows(tool_name),
        None => true,
    }
}

fn tool_name(schema: &Value) -> &str {
    schema.get("name").and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use moa_core::{
        Channel, ModelCapabilities, ModelId, SessionId, SessionMeta, TokenPricing, ToolCallFormat,
        UserId, WorkspaceId,
    };
    use serde_json::json;

    use super::*;

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
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
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn tool_processor_serializes_tool_schemas() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());

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
        .expect("tool schemas should compile");

        assert_eq!(ctx.tools()[0]["name"], "bash");
        assert_eq!(output.items_included, vec!["bash".to_string()]);
        assert!(output.tokens_added > 0);
    }

    #[tokio::test]
    async fn tool_processor_orders_schemas_by_name_for_stable_prefixes() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());

        ToolDefinitionProcessor::new(vec![
            json!({"name": "web_search", "description": "Search the web"}),
            json!({"name": "bash", "description": "Run shell commands"}),
        ])
        .process(&mut ctx)
        .await
        .expect("tool schemas should compile");

        assert_eq!(ctx.tools()[0]["name"], "bash");
        assert_eq!(ctx.tools()[1]["name"], "web_search");
    }

    #[tokio::test]
    async fn tool_processor_filters_schemas_by_agent_policy() {
        // Pins: prompt-visible tool schemas must match the session-pinned agent policy.
        let mut session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        session.agent_context = Some(agent_context_allowing(vec!["file_read"]));
        let mut ctx = WorkingContext::new(&session, capabilities());

        let output = ToolDefinitionProcessor::new(vec![
            json!({"name": "bash", "description": "Run shell commands"}),
            json!({"name": "file_read", "description": "Read files"}),
        ])
        .process(&mut ctx)
        .await
        .expect("tool schemas should compile");

        assert_eq!(ctx.tools().len(), 1);
        assert_eq!(ctx.tools()[0]["name"], "file_read");
        assert_eq!(output.items_included, vec!["file_read".to_string()]);
        assert_eq!(output.items_excluded, vec!["bash".to_string()]);
    }

    fn agent_context_allowing(tools: Vec<&str>) -> moa_core::AgentContext {
        let snapshot = moa_core::AgentPolicySnapshot {
            instructions: Vec::new(),
            tool_policy: moa_core::AgentToolPolicy {
                mode: moa_core::AgentToolPolicyMode::Allowlist,
                tools: tools.into_iter().map(ToString::to_string).collect(),
                denied_tools: Vec::new(),
            },
            revision_lock: None,
            ..moa_core::AgentPolicySnapshot::default()
        };
        moa_core::AgentContext {
            agent_id: None,
            installation_uid: Some(uuid::Uuid::now_v7()),
            deployment_uid: Some(uuid::Uuid::now_v7()),
            definition_ref: "agent://support".to_string(),
            revision_uid: uuid::Uuid::now_v7(),
            policy_hash: "policy-hash".to_string(),
            display_name: "Support".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
        }
    }
}

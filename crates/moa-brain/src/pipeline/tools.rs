//! Stage 3: serializes the fixed tool loadout for the session.

use async_trait::async_trait;
use moa_core::{
    error::Result, traits::ContextProcessor, types::agent::AgentToolPolicy,
    types::context::ExcludedItem, types::context::ProcessorOutput, types::context::WorkingContext,
    types::context::estimate_text_tokens,
};
use serde_json::Value;

/// Metadata key exposing the compiled tool loadout's precomputed token count so
/// later stages need not re-serialize and re-tokenize the schemas.
pub(crate) const TOOLS_TOKEN_COUNT_METADATA_KEY: &str = "_moa.tools.token_count";

// WARNING: Tool schemas live in the cached prompt prefix.
// Keep ordering deterministic and do not inject workspace- or turn-specific metadata here.

/// One tool schema pre-canonicalized once so per-turn work is just policy
/// filtering — no re-cloning, key sorting, re-serialization, or re-tokenizing of
/// the fixed loadout.
#[derive(Clone)]
struct CanonicalTool {
    name: String,
    schema: Value,
    token_count: usize,
}

/// Injects deterministic tool schemas into the working context.
#[derive(Clone)]
pub struct ToolDefinitionProcessor {
    canonical_tools: Vec<CanonicalTool>,
}

impl ToolDefinitionProcessor {
    /// Creates a tool processor from a fixed list of schemas.
    ///
    /// The loadout is canonicalized once here (object keys sorted, ordered by
    /// tool name, token count precomputed) because it is constant for the life
    /// of the processor; per-turn `process` only applies the pinned agent policy.
    pub fn new(tool_schemas: Vec<Value>) -> Self {
        let mut canonical_tools = tool_schemas
            .into_iter()
            .map(|mut schema| {
                sort_json_keys(&mut schema);
                let token_count = estimate_text_tokens(&schema.to_string());
                CanonicalTool {
                    name: tool_name(&schema).to_string(),
                    schema,
                    token_count,
                }
            })
            .collect::<Vec<_>>();
        canonical_tools.sort_by(|left, right| left.name.cmp(&right.name));
        Self { canonical_tools }
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
        const MAX_TOOLS: usize = 30;
        let mut tool_schemas = Vec::new();
        let mut items_included = Vec::new();
        let mut tokens_added = 0usize;
        let mut excluded_items = Vec::new();
        // The canonical loadout is already key-sorted and name-ordered; retain
        // the first `MAX_TOOLS` allowed tools, matching the prior
        // filter-then-truncate ordering.
        for tool in &self.canonical_tools {
            if agent_allows_tool(agent_tool_policy.as_ref(), &tool.name) {
                if tool_schemas.len() < MAX_TOOLS {
                    tool_schemas.push(tool.schema.clone());
                    tokens_added += tool.token_count;
                    items_included.push(tool.name.clone());
                } else {
                    // Cap exclusions must be observable: silent drops would
                    // read as "every allowed tool was offered" when it wasn't.
                    excluded_items.push(ExcludedItem {
                        item: tool.name.clone(),
                        reason: format!("omitted by the {MAX_TOOLS}-tool schema cap"),
                    });
                }
            } else {
                excluded_items.push(ExcludedItem {
                    item: tool.name.clone(),
                    reason: "denied by pinned agent tool policy".to_string(),
                });
            }
        }
        let items_excluded = excluded_items
            .iter()
            .map(|item| item.item.clone())
            .collect::<Vec<_>>();

        ctx.set_tools(tool_schemas);
        ctx.insert_metadata(
            TOOLS_TOKEN_COUNT_METADATA_KEY,
            serde_json::json!(tokens_added),
        );

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

/// Recursively sorts object keys so serialized tool schemas are deterministic.
fn sort_json_keys(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        Value::Object(map) => {
            let mut ordered = map
                .iter()
                .map(|(key, value)| {
                    let mut value = value.clone();
                    sort_json_keys(&mut value);
                    (key.clone(), value)
                })
                .collect::<Vec<_>>();
            ordered.sort_by(|left, right| left.0.cmp(&right.0));

            map.clear();
            for (key, value) in ordered {
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        types::channel::Channel, types::identifiers::ModelId, types::identifiers::SessionId,
        types::identifiers::TenantId, types::model::ModelCapabilities, types::model::TokenPricing,
        types::model::ToolCallFormat, types::session::SessionMeta,
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
            tenant_id: TenantId::new(),
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
            tenant_id: TenantId::new(),
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
            tenant_id: TenantId::new(),
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

    fn agent_context_allowing(tools: Vec<&str>) -> moa_core::types::agent::AgentContext {
        let snapshot = moa_core::types::agent::AgentPolicySnapshot {
            instructions: Vec::new(),
            tool_policy: moa_core::types::agent::AgentToolPolicy {
                mode: moa_core::types::agent::AgentToolPolicyMode::Allowlist,
                tools: tools.into_iter().map(ToString::to_string).collect(),
                denied_tools: Vec::new(),
            },
            revision_lock: None,
            ..moa_core::types::agent::AgentPolicySnapshot::default()
        };
        moa_core::types::agent::AgentContext {
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

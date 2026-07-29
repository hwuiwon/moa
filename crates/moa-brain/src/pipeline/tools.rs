//! Stage 3: serializes the fixed tool loadout for the session.

use async_trait::async_trait;
use moa_core::{
    error::Result, traits::ContextProcessor, types::agent::AgentContext,
    types::agent::AgentToolPolicy, types::context::ExcludedItem, types::context::ProcessorOutput,
    types::context::WorkingContext, types::context::estimate_text_tokens,
    types::tools::CONTROL_TOOL_NAMES,
};
use serde_json::Value;

/// Metadata key exposing the compiled tool loadout's precomputed token count so
/// later stages need not re-serialize and re-tokenize the schemas.
pub(crate) const TOOLS_TOKEN_COUNT_METADATA_KEY: &str = "_moa.tools.token_count";

/// Metadata key exposing the revision of the loadout compiled into this turn.
///
/// The offered schemas live in the cached prompt prefix while the router's
/// catalog can be refreshed underneath it. Recording the revision the turn was
/// compiled at makes a divergence observable instead of silent.
pub(crate) const TOOLS_LOADOUT_REVISION_METADATA_KEY: &str = "_moa.tools.loadout_revision";

/// Maximum number of tool schemas offered on one turn.
const MAX_TOOLS: usize = 30;

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

/// Why a tool was kept when the loadout had to be reduced to the schema cap.
///
/// Ordered highest priority first; `Ord` is the selection order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SelectionTier {
    /// The agent loop's own control tools.
    Control,
    /// A tool the pinned agent explicitly declared a dependency on.
    Declared,
    /// Everything else, ranked by the deployment's declared capability priority.
    Available,
}

/// Injects deterministic tool schemas into the working context.
#[derive(Clone)]
pub struct ToolDefinitionProcessor {
    canonical_tools: Vec<CanonicalTool>,
}

impl ToolDefinitionProcessor {
    /// Creates a tool processor from the loadout the router declared.
    ///
    /// Schemas are canonicalized once here (object keys sorted, token count
    /// precomputed) because the loadout is constant for the life of the
    /// processor. The supplied *order* is deliberately preserved: it is the
    /// deployment's declared capability priority — built-ins, then sandbox tools
    /// in their authored order, then connector tools in catalog order — and it
    /// is what selection ranks along when the loadout exceeds the schema cap.
    /// Sorting here would destroy that priority before anything got to use it.
    pub fn new(tool_schemas: Vec<Value>) -> Self {
        let canonical_tools = tool_schemas
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
        Self { canonical_tools }
    }

    /// Selects the tools to offer, highest priority first.
    ///
    /// Selection and presentation are separate steps on purpose. Selection ranks
    /// by control, then explicit agent declaration, then the declared capability
    /// priority — never by name, because a tool's name says nothing about
    /// whether the turn needs it, and lexical truncation is why a declared tool
    /// could vanish for being spelled late in the alphabet. Presentation order
    /// is canonicalized by the caller afterwards so the cached prompt prefix
    /// stays byte-stable.
    fn select(&self, agent_context: Option<&AgentContext>) -> Selection<'_> {
        let tool_policy = agent_context
            .map(AgentContext::parsed_policy_snapshot)
            .transpose()
            .ok()
            .flatten()
            .map(|snapshot| snapshot.tool_policy);
        let declared = agent_context
            .map(AgentContext::declared_tool_names)
            .unwrap_or_default();

        let mut ranked = Vec::new();
        let mut denied = Vec::new();
        for (position, tool) in self.canonical_tools.iter().enumerate() {
            if !agent_allows_tool(tool_policy.as_ref(), &tool.name) {
                denied.push(tool);
                continue;
            }
            let tier = if CONTROL_TOOL_NAMES.contains(&tool.name.as_str()) {
                SelectionTier::Control
            } else if declared.iter().any(|name| name == &tool.name) {
                SelectionTier::Declared
            } else {
                SelectionTier::Available
            };
            // Within `Declared`, rank by the agent's declaration order; within
            // the other tiers, by the loadout's declared priority order.
            let rank = match tier {
                SelectionTier::Declared => declared
                    .iter()
                    .position(|name| name == &tool.name)
                    .unwrap_or(position),
                SelectionTier::Control | SelectionTier::Available => position,
            };
            ranked.push((tier, rank, tool));
        }
        ranked.sort_by_key(|(tier, rank, _)| (*tier, *rank));

        let (kept, capped) = ranked.split_at(ranked.len().min(MAX_TOOLS));
        Selection {
            kept: kept.iter().map(|(_, _, tool)| *tool).collect(),
            capped: capped.iter().map(|(_, _, tool)| *tool).collect(),
            denied,
        }
    }
}

/// One turn's partition of the declared loadout.
struct Selection<'a> {
    kept: Vec<&'a CanonicalTool>,
    capped: Vec<&'a CanonicalTool>,
    denied: Vec<&'a CanonicalTool>,
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
        // Parsed here as well as inside `select` so a malformed pinned snapshot
        // is a turn error rather than a silently unfiltered loadout.
        if let Some(agent_context) = ctx.agent_context.as_ref() {
            agent_context.parsed_policy_snapshot()?;
        }
        let selection = self.select(ctx.agent_context.as_ref());

        // Canonicalize AFTER selection: what to offer is a priority question,
        // what order to offer it in is a prompt-cache question, and answering
        // both with one sort is what made a declared tool droppable for its
        // name. Sorting the kept set by name keeps the cached prefix byte-stable
        // across turns that select the same tools.
        let mut kept = selection.kept;
        kept.sort_by(|left, right| left.name.cmp(&right.name));

        let tokens_added = kept.iter().map(|tool| tool.token_count).sum::<usize>();
        let items_included = kept
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let tool_schemas = kept
            .iter()
            .map(|tool| tool.schema.clone())
            .collect::<Vec<_>>();
        let loadout_revision = loadout_revision(&kept);

        // Cap and policy exclusions must both be observable: a silent drop
        // reads as "every allowed tool was offered" when it was not.
        let mut excluded_items = selection
            .denied
            .iter()
            .map(|tool| ExcludedItem {
                item: tool.name.clone(),
                reason: "denied by pinned agent tool policy".to_string(),
            })
            .chain(selection.capped.iter().map(|tool| ExcludedItem {
                item: tool.name.clone(),
                reason: format!(
                    "omitted by the {MAX_TOOLS}-tool schema cap after control and declared \
                     dependencies were selected"
                ),
            }))
            .collect::<Vec<_>>();
        excluded_items.sort_by(|left, right| left.item.cmp(&right.item));
        let items_excluded = excluded_items
            .iter()
            .map(|item| item.item.clone())
            .collect::<Vec<_>>();

        ctx.set_tools(tool_schemas);
        ctx.insert_metadata(
            TOOLS_TOKEN_COUNT_METADATA_KEY,
            serde_json::json!(tokens_added),
        );
        ctx.insert_metadata(
            TOOLS_LOADOUT_REVISION_METADATA_KEY,
            serde_json::json!(loadout_revision),
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

/// Computes the revision of the loadout offered on one turn.
///
/// Covers each offered tool's name and its canonical schema, so a connector that
/// changes a tool's schema under a live prompt prefix produces a different
/// revision even though the tool list is unchanged.
fn loadout_revision(tools: &[&CanonicalTool]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.tools.loadout-revision.v1");
    for tool in tools {
        for part in [tool.name.as_bytes(), tool.schema.to_string().as_bytes()] {
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
    }
    hasher.finalize().to_hex().to_string()
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

    /// Builds a loadout of `count` tools in declared priority order, named so
    /// that declared priority and lexical order disagree.
    fn numbered_loadout(count: usize) -> Vec<Value> {
        (0..count)
            .map(|index| json!({"name": format!("tool_{index:02}"), "description": "T"}))
            .collect()
    }

    async fn offered_tool_names(
        schemas: Vec<Value>,
        agent_context: Option<moa_core::types::agent::AgentContext>,
    ) -> Vec<String> {
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            agent_context,
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        ToolDefinitionProcessor::new(schemas)
            .process(&mut ctx)
            .await
            .expect("tool schemas should compile");
        ctx.tools()
            .iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_declared_tool_past_the_cap_stays_available() {
        // Pins the acceptance criterion: with more tools than the cap, a tool the
        // agent explicitly declared survives even though it sits at lexical
        // position 39 and would have been the first thing an alphabetical
        // truncation dropped. Its neighbours at the same position do not survive,
        // which is what shows the declaration is doing the work rather than a
        // wider cap.
        let declared = "tool_39";
        let offered = offered_tool_names(
            numbered_loadout(40),
            Some(agent_context_declaring(vec![declared])),
        )
        .await;

        assert_eq!(offered.len(), 30, "the schema cap must still be enforced");
        assert!(
            offered.iter().any(|name| name == declared),
            "a declared tool must survive the cap regardless of its lexical position: {offered:?}"
        );
        assert!(
            !offered.iter().any(|name| name == "tool_38"),
            "an undeclared neighbour at the same lexical position must still be dropped"
        );
    }

    #[tokio::test]
    async fn control_tools_survive_the_cap_from_the_end_of_the_loadout() {
        // Pins: the loop's own control tools are kept ahead of capability tools
        // even when the deployment declares them last. Losing them leaves every
        // truncated or claim-checked tool output unreadable while the loadout
        // still looks complete.
        let mut schemas = numbered_loadout(40);
        for control in CONTROL_TOOL_NAMES {
            schemas.push(json!({"name": control, "description": "control"}));
        }

        let offered = offered_tool_names(schemas, None).await;

        for control in CONTROL_TOOL_NAMES {
            assert!(
                offered.iter().any(|name| name == control),
                "{control} must survive the cap from the end of the loadout: {offered:?}"
            );
        }
    }

    #[tokio::test]
    async fn offered_schemas_are_canonically_ordered_after_selection() {
        // Pins: selection ranks by priority, presentation is sorted by name. The
        // prompt prefix is cached, so two turns that select the same tools must
        // serialize them identically no matter what order selection produced.
        let offered = offered_tool_names(
            vec![
                json!({"name": "zulu", "description": "Z"}),
                json!({"name": "alpha", "description": "A"}),
                json!({"name": "mike", "description": "M"}),
            ],
            None,
        )
        .await;

        assert_eq!(offered, vec!["alpha", "mike", "zulu"]);
    }

    #[tokio::test]
    async fn the_same_loadout_and_agent_produce_the_same_revision() {
        // Pins: "same inputs and revision yield the same schemas and order" is
        // observable. The recorded revision is what a later reader compares
        // against to notice that the catalog moved under a cached prefix.
        async fn revision(declared: Vec<&str>) -> String {
            let session = SessionMeta {
                id: SessionId::new(),
                tenant_id: TenantId::new(),
                channel: Channel::Chat,
                model: ModelId::new("claude-sonnet-4-6"),
                agent_context: Some(agent_context_declaring(declared)),
                ..SessionMeta::default()
            };
            let mut ctx = WorkingContext::new(&session, capabilities());
            ToolDefinitionProcessor::new(numbered_loadout(40))
                .process(&mut ctx)
                .await
                .expect("tool schemas should compile");
            ctx.metadata()
                .get(TOOLS_LOADOUT_REVISION_METADATA_KEY)
                .and_then(Value::as_str)
                .expect("loadout revision should be recorded")
                .to_string()
        }

        assert_eq!(
            revision(vec!["tool_39"]).await,
            revision(vec!["tool_39"]).await
        );
        assert_ne!(
            revision(vec!["tool_39"]).await,
            revision(vec!["tool_38"]).await,
            "selecting a different tool must produce a different loadout revision"
        );
    }

    /// An agent whose revision lock pins `tools` as explicit dependencies.
    fn agent_context_declaring(tools: Vec<&str>) -> moa_core::types::agent::AgentContext {
        let mut context = agent_context_allowing(Vec::new());
        context.tool_dependencies = tools
            .into_iter()
            .map(|name| moa_core::types::agent::LockedToolRef {
                name: name.to_string(),
                identity_hash: format!("{name}-hash"),
                provider: None,
            })
            .collect();
        // The lock declares dependencies; the policy stays permissive so the
        // test isolates declaration-driven selection from allowlist filtering.
        context.policy_snapshot =
            serde_json::to_value(moa_core::types::agent::AgentPolicySnapshot::default())
                .expect("serialize permissive policy snapshot");
        context
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

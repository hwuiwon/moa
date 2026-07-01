//! Tool registration and default loadout definitions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::adapters::mcp::McpDiscoveredTool;
use crate::tools::{memory, session_search, tool_result};
use moa_core::{
    ActionClass, ActionPolicyEffect, BuiltInTool, IdempotencyClass, Result, SandboxTier,
    ToolBudgetConfig, ToolDefinition, ToolDiffStrategy, ToolInputShape, ToolPolicySpec,
};
use serde_json::Value;

use crate::tools::sandbox_descriptor::{
    SandboxToolDescriptor, default_sandbox_tool_descriptors, sandbox_tool_descriptors,
};

use super::DEFAULT_PROVIDER_NAME;

/// Tool execution routing target.
pub enum ToolExecution {
    /// Built-in Rust implementation.
    BuiltIn(Arc<dyn BuiltInTool>),
    /// Routed to a provisioned hand.
    Hand { provider: String, tier: SandboxTier },
    /// Reserved for MCP-backed tools.
    Mcp { server_name: String },
}

pub(super) struct RegisteredTool {
    pub(super) definition: ToolDefinition,
    pub(super) execution: ToolExecution,
}

impl RegisteredTool {
    fn builtin(tool: Arc<dyn BuiltInTool>) -> Self {
        Self {
            definition: tool.definition(),
            execution: ToolExecution::BuiltIn(tool),
        }
    }

    fn hand(
        name: &str,
        description: &str,
        schema: Value,
        policy: ToolPolicySpec,
        idempotency_class: IdempotencyClass,
    ) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                policy,
                idempotency_class,
                max_output_tokens: default_budget_for_tool(name),
            },
            execution: ToolExecution::Hand {
                provider: DEFAULT_PROVIDER_NAME.to_string(),
                tier: SandboxTier::Local,
            },
        }
    }

    fn sandbox_hand(descriptor: &SandboxToolDescriptor) -> Self {
        Self {
            definition: descriptor.definition(default_budget_for_tool(descriptor.name)),
            execution: ToolExecution::Hand {
                provider: DEFAULT_PROVIDER_NAME.to_string(),
                tier: SandboxTier::Local,
            },
        }
    }

    fn mcp(server_name: &str, tool: McpDiscoveredTool) -> Self {
        let name = tool.name;
        Self {
            definition: ToolDefinition {
                name: name.clone(),
                description: tool.description,
                schema: tool.input_schema,
                policy: ToolPolicySpec {
                    risk_level: moa_core::RiskLevel::High,
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: ActionClass::ExternalWrite,
                    input_shape: ToolInputShape::Json,
                    diff_strategy: ToolDiffStrategy::None,
                },
                idempotency_class: IdempotencyClass::NonIdempotent,
                max_output_tokens: 8_000,
            },
            execution: ToolExecution::Mcp {
                server_name: server_name.to_string(),
            },
        }
    }
}

/// In-memory registry of available tools.
pub struct ToolRegistry {
    pub(super) tools: HashMap<String, RegisteredTool>,
    default_loadout: Vec<String>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            default_loadout: Vec::new(),
        }
    }

    /// Returns the canonical local registry for Step 06.
    pub fn default_local() -> Self {
        let mut registry = Self::new();
        registry.register_builtin(Arc::new(memory::MemoryRememberTool));
        registry.register_builtin(Arc::new(memory::MemoryForgetTool));
        registry.register_builtin(Arc::new(memory::MemorySupersedeTool));
        registry.register_builtin(Arc::new(session_search::SessionSearchTool));
        registry.register_builtin(Arc::new(tool_result::ToolResultReadTool));
        registry.register_builtin(Arc::new(tool_result::ToolResultSearchTool));
        for descriptor in sandbox_tool_descriptors() {
            registry.register_sandbox_tool(descriptor);
        }
        registry.default_loadout = [
            "memory_remember".to_string(),
            "memory_forget".to_string(),
            "memory_supersede".to_string(),
            "session_search".to_string(),
            "tool_result_read".to_string(),
            "tool_result_search".to_string(),
        ]
        .into_iter()
        .chain(
            default_sandbox_tool_descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name.to_string()),
        )
        .collect();
        registry
    }

    /// Registers a built-in tool.
    pub fn register_builtin(&mut self, tool: Arc<dyn BuiltInTool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, RegisteredTool::builtin(tool));
    }

    /// Registers a hand-routed tool using the local provider.
    pub fn register_hand(
        &mut self,
        name: &str,
        description: &str,
        schema: Value,
        policy: ToolPolicySpec,
        idempotency_class: IdempotencyClass,
    ) {
        self.tools.insert(
            name.to_string(),
            RegisteredTool::hand(name, description, schema, policy, idempotency_class),
        );
    }

    fn register_sandbox_tool(&mut self, descriptor: &SandboxToolDescriptor) {
        self.tools.insert(
            descriptor.name.to_string(),
            RegisteredTool::sandbox_hand(descriptor),
        );
    }

    /// Registers a discovered MCP tool and adds it to the default loadout.
    pub fn register_mcp_tool(&mut self, server_name: &str, tool: McpDiscoveredTool) -> Result<()> {
        let name = tool.name.clone();
        if self.tools.contains_key(&name) {
            return Err(moa_core::MoaError::ConfigError(format!(
                "MCP server {server_name} discovered tool {name}, which conflicts with an existing local tool name"
            )));
        }
        self.tools
            .insert(name.clone(), RegisteredTool::mcp(server_name, tool));
        if !self
            .default_loadout
            .iter()
            .any(|candidate| candidate == &name)
        {
            self.default_loadout.push(name);
        }
        Ok(())
    }

    /// Retargets all hand-based tools to a different provider and sandbox tier.
    pub fn retarget_hand_tools(&mut self, provider: &str, tier: SandboxTier) {
        for tool in self.tools.values_mut() {
            if let ToolExecution::Hand {
                provider: current_provider,
                tier: current_tier,
            } = &mut tool.execution
            {
                *current_provider = provider.to_string();
                *current_tier = tier.clone();
            }
        }
    }

    /// Returns a tool definition by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name).map(|tool| &tool.definition)
    }

    /// Returns whether the named tool provisions a hand/sandbox to execute.
    ///
    /// Hand-routed tools ([`ToolExecution::Hand`]) are the only tools that
    /// provision a sandbox when invoked; built-in (in-process) and MCP tools
    /// never do. This execution-routing fact is the authoritative signal used to
    /// keep sandbox/compute tools out of the sandbox-free root coordinator's
    /// tool set. Unknown tool names are treated as not requiring a sandbox.
    pub fn tool_requires_sandbox(&self, name: &str) -> bool {
        matches!(
            self.tools.get(name).map(|tool| &tool.execution),
            Some(ToolExecution::Hand { .. })
        )
    }

    /// Returns the ordered default tool schemas for prompt compilation.
    pub fn default_tool_schemas(&self) -> Vec<Value> {
        self.default_loadout
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.definition.anthropic_schema())
            .collect()
    }

    /// Retains only the registered tools whose names are present in the allowlist.
    pub fn retain_only<I, S>(&mut self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = tool_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect::<HashSet<_>>();
        self.tools.retain(|name, _| allowed.contains(name));
        self.default_loadout.retain(|name| allowed.contains(name));
    }

    /// Applies configured per-tool output budgets to all registered tools.
    pub fn apply_budgets(&mut self, tool_budgets: &ToolBudgetConfig) {
        for (name, registered_tool) in &mut self.tools {
            registered_tool.definition.max_output_tokens = tool_budgets.for_tool(name);
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::default_local()
    }
}

fn default_budget_for_tool(tool_name: &str) -> u32 {
    ToolBudgetConfig::default().for_tool(tool_name)
}

#[cfg(test)]
mod tests {
    use crate::tools::sandbox_descriptor::{
        default_sandbox_tool_descriptors, sandbox_tool_descriptors,
    };

    use super::ToolRegistry;

    #[test]
    fn default_local_prompt_schemas_keep_structured_hand_tool_guidance() {
        // Pins: prompt-facing hand tool descriptions carry usage policy without changing schemas.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            let name = descriptor.name;
            let description = registry
                .get(name)
                .expect("default tool should exist")
                .description
                .as_str();
            assert!(
                description.contains("Purpose:"),
                "{name}: missing Purpose guidance"
            );
            assert!(
                description.contains("Use when:"),
                "{name}: missing Use when guidance"
            );
            assert!(
                description.contains("Do not use:"),
                "{name}: missing Do not use guidance"
            );
            assert!(
                description.contains("If blocked:"),
                "{name}: missing blocked/failure guidance"
            );
        }

        let tool_names = registry
            .default_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tool_names,
            vec![
                "memory_remember",
                "memory_forget",
                "memory_supersede",
                "session_search",
                "tool_result_read",
                "tool_result_search",
                "file_search",
                "grep",
                "file_outline",
                "file_read",
                "str_replace",
                "file_write",
                "bash",
            ],
            "default local loadout order changed"
        );
    }

    #[test]
    fn tool_requires_sandbox_flags_hand_tools_only() {
        // Pins: the coordinator-exclusion predicate tracks `ToolExecution::Hand`, so every
        // sandbox descriptor tool is hand-routed while built-in tools and unknown names are not.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            assert!(
                registry.tool_requires_sandbox(descriptor.name),
                "{} is a sandbox tool and must require a hand",
                descriptor.name
            );
        }
        assert!(registry.tool_requires_sandbox("bash"));
        assert!(registry.tool_requires_sandbox("file_read"));

        for builtin in [
            "memory_remember",
            "memory_forget",
            "memory_supersede",
            "session_search",
            "tool_result_read",
            "tool_result_search",
        ] {
            assert!(
                !registry.tool_requires_sandbox(builtin),
                "{builtin} is a built-in tool and must not require a hand"
            );
        }
        // Delegation tools are injected at the orchestrator layer and are never registered
        // as hand-routed router tools, so the predicate reports them as coordinator-safe.
        assert!(!registry.tool_requires_sandbox("spawn_worker"));
        assert!(!registry.tool_requires_sandbox("nonexistent_tool"));
    }

    #[test]
    fn default_local_uses_sandbox_descriptors_as_source_of_truth() {
        // Pins: default registry metadata is generated from sandbox descriptors.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            let definition = registry
                .get(descriptor.name)
                .expect("descriptor-owned tool should be registered");
            assert_eq!(definition.name, descriptor.name);
            assert_eq!(definition.description, descriptor.description);
            assert_eq!(definition.schema, (descriptor.schema)());
            assert_eq!(definition.policy, descriptor.policy);
            assert_eq!(definition.idempotency_class, descriptor.idempotency_class);
        }

        let registered_loadout = registry
            .default_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .skip(6)
            .collect::<Vec<_>>();
        let descriptor_loadout = default_sandbox_tool_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(registered_loadout, descriptor_loadout);
    }
}

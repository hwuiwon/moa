//! Tool registration and default loadout definitions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::mcp::McpDiscoveredTool;
use crate::tools::{memory, session_search};
use moa_core::{
    BuiltInTool, PolicyAction, SandboxTier, ToolDefinition, ToolDiffStrategy, ToolInputShape,
    ToolPolicySpec, read_tool_policy, write_tool_policy,
};
use serde_json::{Value, json};

use super::DEFAULT_PROVIDER_NAME;

pub(crate) fn execute_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: moa_core::RiskLevel::High,
        default_action: PolicyAction::RequireApproval,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

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

    fn hand(name: &str, description: &str, schema: Value, policy: ToolPolicySpec) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                policy,
            },
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
                policy: execute_tool_policy(ToolInputShape::Json),
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
        registry.register_builtin(Arc::new(memory::MemoryReadTool));
        registry.register_builtin(Arc::new(memory::MemorySearchTool));
        registry.register_builtin(Arc::new(memory::MemoryWriteTool));
        registry.register_builtin(Arc::new(memory::MemoryIngestTool));
        registry.register_builtin(Arc::new(session_search::SessionSearchTool));
        registry.register_hand(
            "bash",
            "Run a non-interactive shell command inside the active workspace root. Each bash call starts fresh; directory changes do not persist to later tool calls.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command to execute." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300, "description": "Optional timeout override in seconds." }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            execute_tool_policy(ToolInputShape::Command),
        );
        registry.register_hand(
            "file_read",
            "Read a UTF-8 text file from the active workspace root. Paths must be relative and must not use `..`.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Path),
        );
        registry.register_hand(
            "str_replace",
            "Replace one unique string match in a UTF-8 text file. Use this for edits to existing files; include enough surrounding context in old_str to make the match unique.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
                    "old_str": { "type": "string", "description": "Exact string to replace. Must match exactly once unless empty for insertion or creation." },
                    "new_str": { "type": "string", "description": "Replacement string. Empty deletes the matched region." },
                    "insert_after_line": { "type": "integer", "minimum": 0, "description": "Required when old_str is empty and you want to insert after a specific line." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::StrReplace),
        );
        registry.register_hand(
            "file_write",
            "Create or overwrite a UTF-8 text file inside the active workspace root. Paths must be relative and must not use `..`.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
                    "content": { "type": "string", "description": "Full file contents to write." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::FileWrite),
        );
        registry.register_hand(
            "file_search",
            "Find files inside the active workspace root using a glob pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs, evaluated from the workspace root." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Pattern),
        );
        registry.default_loadout = vec![
            "memory_read".to_string(),
            "memory_search".to_string(),
            "memory_write".to_string(),
            "memory_ingest".to_string(),
            "session_search".to_string(),
            "bash".to_string(),
            "file_read".to_string(),
            "str_replace".to_string(),
            "file_write".to_string(),
            "file_search".to_string(),
        ];
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
    ) {
        self.tools.insert(
            name.to_string(),
            RegisteredTool::hand(name, description, schema, policy),
        );
    }

    /// Registers a discovered MCP tool and adds it to the default loadout.
    pub fn register_mcp_tool(&mut self, server_name: &str, tool: McpDiscoveredTool) {
        let name = tool.name.clone();
        self.tools
            .insert(name.clone(), RegisteredTool::mcp(server_name, tool));
        if !self
            .default_loadout
            .iter()
            .any(|candidate| candidate == &name)
        {
            self.default_loadout.push(name);
        }
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
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::default_local()
    }
}

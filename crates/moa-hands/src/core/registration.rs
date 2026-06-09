//! Tool registration and default loadout definitions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::adapters::mcp::McpDiscoveredTool;
use crate::tools::{memory, session_search, tool_result};
use moa_core::{
    BuiltInTool, IdempotencyClass, PolicyAction, SandboxTier, ToolBudgetConfig, ToolDefinition,
    ToolDiffStrategy, ToolInputShape, ToolPolicySpec, read_tool_policy, write_tool_policy,
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

    fn mcp(server_name: &str, tool: McpDiscoveredTool) -> Self {
        let name = tool.name;
        Self {
            definition: ToolDefinition {
                name: name.clone(),
                description: tool.description,
                schema: tool.input_schema,
                policy: execute_tool_policy(ToolInputShape::Json),
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
        registry.register_hand(
            "bash",
            "Purpose: run a non-interactive shell command inside the active workspace root. Use when: tests, builds, package managers, git inspection, or commands native file tools cannot express. Do not use: routine repository navigation, source reading, or text edits that file_search, grep, file_outline, file_read, str_replace, or file_write can handle. If blocked: keep commands targeted, preserve stderr/stdout, and stop after repeated failures instead of looping.",
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
            IdempotencyClass::NonIdempotent,
        );
        registry.register_hand(
            "file_outline",
            "Purpose: inspect a Python file's symbol outline without reading the full file. Use when: a large Python source file needs class, function, method, or line-number orientation. Do not use: non-Python files or exact content searches where grep is better. If blocked: fall back to a narrow file_read range after locating the nearest symbol.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Currently supports Python files." },
                    "symbol": { "type": "string", "description": "Optional class, function, or method name to focus on." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Path),
            IdempotencyClass::Idempotent,
        );
        registry.register_hand(
            "grep",
            "Purpose: search workspace file contents with regex or literal patterns. Use when: locating symbols, strings, errors, tests, or references before reading files. Do not use: broad exploratory filesystem walks or generated/vendor directories. If blocked: narrow path, enable literal matching for exact strings, or read a small matching range.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for. Use literal for exact string matching." },
                    "path": { "type": "string", "description": "Optional subdirectory or file to search within. Defaults to the workspace root." },
                    "context_lines": { "type": "integer", "minimum": 0, "maximum": 5, "description": "Optional number of surrounding lines to include for each match. Default: 0." },
                    "literal": { "type": "boolean", "description": "Treat pattern as a literal string instead of a regex. Default: false." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Pattern),
            IdempotencyClass::Idempotent,
        );
        registry.register_hand(
            "file_read",
            "Purpose: read UTF-8 text from a workspace file. Use when: you already know the relevant file or line range. Do not use: whole large files before searching or outlining. If blocked: use grep or file_outline first, then retry with a narrower start_line/end_line range.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based first line to read, inclusive." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Optional 1-based last line to read, inclusive. Ranges are clamped and truncated to 200 lines." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Path),
            IdempotencyClass::Idempotent,
        );
        registry.register_hand(
            "str_replace",
            "Purpose: replace one unique string match in an existing UTF-8 text file. Use when: editing an existing file with enough surrounding context to make old_str match exactly once. Do not use: new files, ambiguous matches, or line-number-only insertions. If blocked: read a narrower span and retry with more exact context.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the workspace root. Bash `cd` state does not carry over." },
                    "old_str": { "type": "string", "description": "Exact existing string to replace. Must be non-empty and must match exactly once." },
                    "new_str": { "type": "string", "description": "Replacement string. Empty deletes the matched region." }
                },
                "required": ["path", "old_str", "new_str"],
                "additionalProperties": false
            }),
            write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::StrReplace),
            IdempotencyClass::NonIdempotent,
        );
        registry.register_hand(
            "file_write",
            "Purpose: create or deliberately overwrite a UTF-8 text file inside the active workspace root. Use when: adding a new file or replacing a whole generated/test fixture file intentionally. Do not use: small edits to existing source files where str_replace is safer. If blocked: verify the relative path and avoid `..` or paths outside the workspace.",
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
            IdempotencyClass::NonIdempotent,
        );
        registry.register_hand(
            "file_search",
            "Purpose: find files inside the active workspace root with a glob pattern. Use when: locating paths before reading or editing. Do not use: content search, shell globbing, or generated/vendor directory exploration. If blocked: tighten the glob or switch to grep when the identifier is content rather than a path.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs, evaluated from the workspace root." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Pattern),
            IdempotencyClass::Idempotent,
        );
        registry.default_loadout = vec![
            "memory_remember".to_string(),
            "memory_forget".to_string(),
            "memory_supersede".to_string(),
            "session_search".to_string(),
            "tool_result_read".to_string(),
            "tool_result_search".to_string(),
            "file_search".to_string(),
            "grep".to_string(),
            "file_outline".to_string(),
            "file_read".to_string(),
            "str_replace".to_string(),
            "file_write".to_string(),
            "bash".to_string(),
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
        idempotency_class: IdempotencyClass,
    ) {
        self.tools.insert(
            name.to_string(),
            RegisteredTool::hand(name, description, schema, policy, idempotency_class),
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
    match tool_name {
        "bash" => 4_000,
        "file_outline" => 2_000,
        "grep" | "file_search" => 4_000,
        "file_read" => 8_000,
        _ => 8_000,
    }
}

#[cfg(test)]
mod tests {
    use super::ToolRegistry;

    #[test]
    fn default_local_prompt_schemas_keep_structured_hand_tool_guidance() {
        // Pins: prompt-facing hand tool descriptions carry usage policy without changing schemas.
        let registry = ToolRegistry::default_local();

        for name in [
            "bash",
            "file_outline",
            "grep",
            "file_read",
            "str_replace",
            "file_write",
            "file_search",
        ] {
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
}

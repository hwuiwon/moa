//! Tool permission policy evaluation, command matching, and approval rule storage.

use async_trait::async_trait;
use globset::Glob;
use moa_core::{
    ApprovalRule, MoaConfig, MoaError, PolicyAction, PolicyScope, Result, RiskLevel, SessionMeta,
    UserId, WorkspaceId,
};
use serde_json::Value;

/// Persistent approval rule storage used by policy-aware tool routing.
#[async_trait]
pub trait ApprovalRuleStore: Send + Sync {
    /// Lists all approval rules visible to a workspace.
    async fn list_approval_rules(&self, workspace_id: &WorkspaceId) -> Result<Vec<ApprovalRule>>;

    /// Creates or updates an approval rule.
    async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()>;

    /// Deletes an approval rule by tool and pattern.
    async fn delete_approval_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()>;
}

/// Session-scoped inputs required for tool policy evaluation.
#[derive(Debug, Clone)]
pub struct ToolPolicyContext {
    /// Workspace associated with the current session.
    pub workspace_id: WorkspaceId,
    /// User associated with the current session.
    pub user_id: UserId,
}

impl ToolPolicyContext {
    /// Creates a policy context from a session metadata record.
    pub fn from_session(session: &SessionMeta) -> Self {
        Self {
            workspace_id: session.workspace_id.clone(),
            user_id: session.user_id.clone(),
        }
    }
}

/// Result of evaluating one tool invocation against the current policy set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyCheck {
    /// Action to take for this invocation.
    pub action: PolicyAction,
    /// Risk level displayed to the user when approval is needed.
    pub risk_level: RiskLevel,
    /// Concise input summary for approval prompts.
    pub input_summary: String,
    /// Normalized input string used for rule matching.
    pub normalized_input: String,
    /// Rule that matched, if any.
    pub matched_rule: Option<ApprovalRule>,
}

/// Policy engine for tool execution decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicies {
    default_posture: String,
    auto_approve: Vec<String>,
    always_deny: Vec<String>,
}

impl ToolPolicies {
    /// Creates policies from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Self {
        Self {
            default_posture: config.permissions.default_posture.clone(),
            auto_approve: config.permissions.auto_approve.clone(),
            always_deny: config.permissions.always_deny.clone(),
        }
    }

    /// Evaluates a tool invocation using persistent rules, config defaults, and tool category.
    pub fn check(
        &self,
        tool: &str,
        input: &Value,
        ctx: &ToolPolicyContext,
        rules: &[ApprovalRule],
    ) -> Result<PolicyCheck> {
        let risk_level = risk_level_for_tool(tool);
        let normalized_input = normalize_tool_input(tool, input)?;
        let input_summary = summarize_tool_input(tool, input, &normalized_input);

        for rule in rules {
            if !rule_visible_to_workspace(rule, &ctx.workspace_id) {
                continue;
            }
            if rule.tool != tool {
                continue;
            }
            if rule_matches(rule, tool, &normalized_input) {
                return Ok(PolicyCheck {
                    action: rule.action.clone(),
                    risk_level,
                    input_summary,
                    normalized_input,
                    matched_rule: Some(rule.clone()),
                });
            }
        }

        if self.always_deny.iter().any(|candidate| candidate == tool) {
            return Ok(PolicyCheck {
                action: PolicyAction::Deny,
                risk_level,
                input_summary,
                normalized_input,
                matched_rule: None,
            });
        }

        if self.auto_approve.iter().any(|candidate| candidate == tool) {
            return Ok(PolicyCheck {
                action: PolicyAction::Allow,
                risk_level,
                input_summary,
                normalized_input,
                matched_rule: None,
            });
        }

        let action = match categorize_tool(tool) {
            ToolCategory::Read => PolicyAction::Allow,
            ToolCategory::Write | ToolCategory::Execute | ToolCategory::Network => {
                PolicyAction::RequireApproval
            }
        };

        let action = if self.default_posture.eq_ignore_ascii_case("deny")
            && matches!(action, PolicyAction::RequireApproval)
        {
            PolicyAction::Deny
        } else {
            action
        };

        Ok(PolicyCheck {
            action,
            risk_level,
            input_summary,
            normalized_input,
            matched_rule: None,
        })
    }
}

impl Default for ToolPolicies {
    fn default() -> Self {
        Self::from_config(&MoaConfig::default())
    }
}

/// Performs glob matching against a normalized tool input string.
pub fn glob_match(pattern: &str, candidate: &str) -> bool {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(candidate))
        .unwrap_or(false)
}

/// Parses a bash command and matches it against a rule pattern.
pub fn parse_and_match_bash(command: &str, rule_pattern: &str) -> bool {
    let sub_commands = split_shell_chain(command);
    if sub_commands.len() > 1 {
        return sub_commands
            .iter()
            .all(|sub_command| glob_match(rule_pattern, sub_command));
    }

    shell_words::split(command)
        .map(|tokens| glob_match(rule_pattern, &tokens.join(" ")))
        .unwrap_or_else(|_| glob_match(rule_pattern, command.trim()))
}

/// Splits a shell command string into normalized sub-commands around chain operators.
pub fn split_shell_chain(command: &str) -> Vec<String> {
    let Ok(tokens) = shell_words::split(command) else {
        let trimmed = command.trim();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        };
    };

    let mut sub_commands = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        if matches!(token.as_str(), "&&" | "||" | ";" | "|") {
            if !current.is_empty() {
                sub_commands.push(current.join(" "));
                current.clear();
            }
            continue;
        }
        current.push(token);
    }
    if !current.is_empty() {
        sub_commands.push(current.join(" "));
    }

    sub_commands
}

fn rule_visible_to_workspace(rule: &ApprovalRule, workspace_id: &WorkspaceId) -> bool {
    matches!(rule.scope, PolicyScope::Global) || &rule.workspace_id == workspace_id
}

fn rule_matches(rule: &ApprovalRule, tool: &str, normalized_input: &str) -> bool {
    if tool == "bash" {
        return parse_and_match_bash(normalized_input, &rule.pattern);
    }

    glob_match(&rule.pattern, normalized_input)
}

fn normalize_tool_input(tool: &str, input: &Value) -> Result<String> {
    let value = match tool {
        "bash" => required_string_field(input, "cmd")?,
        "file_read" | "file_write" | "memory_read" | "memory_write" => {
            required_string_field(input, "path")?
        }
        "file_search" => required_string_field(input, "pattern")?,
        "memory_search" | "web_search" => required_string_field(input, "query")?,
        "web_fetch" => required_string_field(input, "url")?,
        _ => serde_json::to_string(input)?,
    };

    Ok(value.trim().to_string())
}

fn summarize_tool_input(tool: &str, input: &Value, normalized_input: &str) -> String {
    let summary = match tool {
        "bash" => format!("Command: {normalized_input}"),
        "file_read" | "file_write" => format!("Path: {normalized_input}"),
        "file_search" => format!("Pattern: {normalized_input}"),
        "memory_read" => format!("Path: {normalized_input}"),
        "memory_search" => format!("Query: {normalized_input}"),
        "memory_write" => format!("Path: {normalized_input}"),
        "web_search" => format!("Query: {normalized_input}"),
        "web_fetch" => format!("URL: {normalized_input}"),
        _ => normalized_input.to_string(),
    };

    if let Some(content) = input.get("content").and_then(Value::as_str)
        && tool == "memory_write"
    {
        return format!("{summary} | {} chars", content.chars().count());
    }

    summary
}

fn required_string_field(input: &Value, field: &str) -> Result<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            MoaError::ValidationError(format!(
                "tool input is missing required string field `{field}`"
            ))
        })
}

fn risk_level_for_tool(tool: &str) -> RiskLevel {
    match categorize_tool(tool) {
        ToolCategory::Read => RiskLevel::Low,
        ToolCategory::Write => RiskLevel::Medium,
        ToolCategory::Execute | ToolCategory::Network => RiskLevel::High,
    }
}

fn categorize_tool(tool: &str) -> ToolCategory {
    match tool {
        "file_read" | "file_search" | "memory_read" | "memory_search" => ToolCategory::Read,
        "file_write" | "memory_write" => ToolCategory::Write,
        "web_search" | "web_fetch" => ToolCategory::Network,
        "bash" => ToolCategory::Execute,
        _ => ToolCategory::Execute,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolCategory {
    Read,
    Write,
    Execute,
    Network,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{PolicyAction, PolicyScope, SessionMeta, UserId, WorkspaceId};
    use serde_json::json;
    use uuid::Uuid;

    use super::{ToolPolicies, ToolPolicyContext, parse_and_match_bash, split_shell_chain};

    fn session() -> SessionMeta {
        SessionMeta {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        }
    }

    #[test]
    fn read_tools_are_auto_approved_and_bash_requires_approval() {
        let policies = ToolPolicies::default();
        let ctx = ToolPolicyContext::from_session(&session());

        let read = policies
            .check("file_read", &json!({ "path": "src/lib.rs" }), &ctx, &[])
            .unwrap();
        let bash = policies
            .check("bash", &json!({ "cmd": "npm test" }), &ctx, &[])
            .unwrap();

        assert_eq!(read.action, PolicyAction::Allow);
        assert_eq!(bash.action, PolicyAction::RequireApproval);
    }

    #[test]
    fn persistent_rule_matching_uses_glob_patterns() {
        let policies = ToolPolicies::default();
        let ctx = ToolPolicyContext::from_session(&session());
        let rules = vec![moa_core::ApprovalRule {
            id: Uuid::new_v4(),
            workspace_id: WorkspaceId::new("workspace"),
            tool: "file_write".to_string(),
            pattern: "src/*.rs".to_string(),
            action: PolicyAction::Allow,
            scope: PolicyScope::Workspace,
            created_by: UserId::new("user"),
            created_at: Utc::now(),
        }];

        let check = policies
            .check("file_write", &json!({ "path": "src/lib.rs" }), &ctx, &rules)
            .unwrap();

        assert_eq!(check.action, PolicyAction::Allow);
        assert!(check.matched_rule.is_some());
    }

    #[test]
    fn shell_command_parsing_detects_chained_commands() {
        assert_eq!(
            split_shell_chain("npm test && rm -rf /"),
            vec!["npm test".to_string(), "rm -rf /".to_string()]
        );
        assert!(!parse_and_match_bash("npm test && rm -rf /", "npm test*"));
        assert!(parse_and_match_bash("npm test -- --watch", "npm test*"));
    }
}

//! Tool permission policy evaluation, command matching, and approval rule storage.

use async_trait::async_trait;
use globset::Glob;
use moa_core::{
    ApprovalRule, MoaConfig, PolicyAction, PolicyScope, Result, SessionMeta, ToolPolicyInput,
    UserId, WorkspaceId,
};

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
        input: &ToolPolicyInput,
        ctx: &ToolPolicyContext,
        rules: &[ApprovalRule],
    ) -> Result<PolicyCheck> {
        for rule in rules {
            if !rule_visible_to_workspace(rule, &ctx.workspace_id) {
                continue;
            }
            if rule.tool != input.tool_name {
                continue;
            }
            if rule_matches(rule, &input.tool_name, &input.normalized_input) {
                return Ok(PolicyCheck {
                    action: rule.action.clone(),
                    matched_rule: Some(rule.clone()),
                });
            }
        }

        if self
            .always_deny
            .iter()
            .any(|candidate| candidate == &input.tool_name)
        {
            return Ok(PolicyCheck {
                action: PolicyAction::Deny,
                matched_rule: None,
            });
        }

        if self
            .auto_approve
            .iter()
            .any(|candidate| candidate == &input.tool_name)
        {
            return Ok(PolicyCheck {
                action: PolicyAction::Allow,
                matched_rule: None,
            });
        }

        let action = if self.default_posture.eq_ignore_ascii_case("deny")
            && matches!(input.default_action, PolicyAction::RequireApproval)
        {
            PolicyAction::Deny
        } else {
            input.default_action.clone()
        };

        Ok(PolicyCheck {
            action,
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        PolicyAction, PolicyScope, RiskLevel, SessionMeta, ToolPolicyInput, UserId, WorkspaceId,
    };
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
            .check(
                &ToolPolicyInput {
                    tool_name: "file_read".to_string(),
                    normalized_input: "src/lib.rs".to_string(),
                    input_summary: "Path: src/lib.rs".to_string(),
                    risk_level: RiskLevel::Low,
                    default_action: PolicyAction::Allow,
                },
                &ctx,
                &[],
            )
            .unwrap();
        let bash = policies
            .check(
                &ToolPolicyInput {
                    tool_name: "bash".to_string(),
                    normalized_input: "npm test".to_string(),
                    input_summary: "Command: npm test".to_string(),
                    risk_level: RiskLevel::High,
                    default_action: PolicyAction::RequireApproval,
                },
                &ctx,
                &[],
            )
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
            .check(
                &ToolPolicyInput {
                    tool_name: "file_write".to_string(),
                    normalized_input: "src/lib.rs".to_string(),
                    input_summary: "Path: src/lib.rs".to_string(),
                    risk_level: RiskLevel::Medium,
                    default_action: PolicyAction::RequireApproval,
                },
                &ctx,
                &rules,
            )
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

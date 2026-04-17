//! Tool permission policy evaluation, command matching, and approval rule storage.

use async_trait::async_trait;
use globset::Glob;
use moa_core::shell::split_shell_chain;
use moa_core::{
    ApprovalRule, MoaConfig, PolicyAction, PolicyScope, Result, SessionMeta, ToolPolicyInput,
    UserId, WorkspaceId,
};

const OVERLY_BROAD_SHELL_RULE_PATTERNS: &[&str] = &["zsh *", "bash *", "sh *", "dash *"];

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

/// Deletes legacy shell approval rules that accidentally matched every wrapped command.
pub async fn cleanup_overly_broad_shell_rules(
    store: &dyn ApprovalRuleStore,
    workspace_id: &WorkspaceId,
) -> Result<usize> {
    let rules = store.list_approval_rules(workspace_id).await?;
    let mut cleaned = 0;

    for rule in rules {
        if rule.tool != "bash" || !OVERLY_BROAD_SHELL_RULE_PATTERNS.contains(&rule.pattern.as_str())
        {
            continue;
        }

        tracing::warn!(
            workspace_id = %workspace_id,
            rule_workspace_id = %rule.workspace_id,
            pattern = %rule.pattern,
            "deleting overly broad shell approval rule"
        );
        store
            .delete_approval_rule(&rule.workspace_id, &rule.tool, &rule.pattern)
            .await?;
        cleaned += 1;
    }

    Ok(cleaned)
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
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::shell::split_shell_chain;
    use moa_core::{
        ApprovalRule, ModelId, PolicyAction, PolicyScope, Result, RiskLevel, SessionMeta,
        ToolPolicyInput, UserId, WorkspaceId,
    };
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use super::{
        ApprovalRuleStore, ToolPolicies, ToolPolicyContext, cleanup_overly_broad_shell_rules,
        parse_and_match_bash,
    };

    #[derive(Clone, Default)]
    struct MemoryApprovalRuleStore {
        rules: Arc<Mutex<Vec<ApprovalRule>>>,
    }

    #[async_trait]
    impl ApprovalRuleStore for MemoryApprovalRuleStore {
        async fn list_approval_rules(
            &self,
            workspace_id: &WorkspaceId,
        ) -> Result<Vec<ApprovalRule>> {
            let rules = self.rules.lock().await;
            Ok(rules
                .iter()
                .filter(|rule| {
                    rule.workspace_id == *workspace_id || matches!(rule.scope, PolicyScope::Global)
                })
                .cloned()
                .collect())
        }

        async fn upsert_approval_rule(&self, rule: ApprovalRule) -> Result<()> {
            let mut rules = self.rules.lock().await;
            if let Some(existing) = rules.iter_mut().find(|existing| {
                existing.workspace_id == rule.workspace_id
                    && existing.tool == rule.tool
                    && existing.pattern == rule.pattern
            }) {
                *existing = rule;
            } else {
                rules.push(rule);
            }
            Ok(())
        }

        async fn delete_approval_rule(
            &self,
            workspace_id: &WorkspaceId,
            tool: &str,
            pattern: &str,
        ) -> Result<()> {
            let mut rules = self.rules.lock().await;
            rules.retain(|rule| {
                rule.workspace_id != *workspace_id || rule.tool != tool || rule.pattern != pattern
            });
            Ok(())
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: ModelId::new("claude-sonnet-4-6"),
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
            id: Uuid::now_v7(),
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

    #[tokio::test]
    async fn cleanup_overly_broad_shell_rules_removes_visible_legacy_patterns() {
        let workspace_id = WorkspaceId::new("workspace");
        let other_workspace_id = WorkspaceId::new("other");
        let store = MemoryApprovalRuleStore::default();

        for rule in [
            approval_rule(&workspace_id, "bash", "zsh *", PolicyScope::Workspace),
            approval_rule(&workspace_id, "bash", "npm *", PolicyScope::Workspace),
            approval_rule(&workspace_id, "file_write", "zsh *", PolicyScope::Workspace),
            approval_rule(&other_workspace_id, "bash", "bash *", PolicyScope::Global),
            approval_rule(&other_workspace_id, "bash", "sh *", PolicyScope::Workspace),
        ] {
            store.upsert_approval_rule(rule).await.unwrap();
        }

        let cleaned = cleanup_overly_broad_shell_rules(&store, &workspace_id)
            .await
            .unwrap();

        assert_eq!(cleaned, 2);

        let visible_rules = store.list_approval_rules(&workspace_id).await.unwrap();
        assert!(
            visible_rules
                .iter()
                .all(|rule| !(rule.tool == "bash" && rule.pattern == "zsh *"))
        );
        assert!(
            visible_rules
                .iter()
                .all(|rule| !(rule.tool == "bash" && rule.pattern == "bash *"))
        );
        assert!(
            visible_rules
                .iter()
                .any(|rule| rule.tool == "bash" && rule.pattern == "npm *")
        );
        assert!(
            visible_rules
                .iter()
                .any(|rule| rule.tool == "file_write" && rule.pattern == "zsh *")
        );
        assert!(
            store
                .list_approval_rules(&other_workspace_id)
                .await
                .unwrap()
                .iter()
                .any(|rule| rule.tool == "bash" && rule.pattern == "sh *")
        );
    }

    fn approval_rule(
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
        scope: PolicyScope,
    ) -> ApprovalRule {
        ApprovalRule {
            id: Uuid::now_v7(),
            workspace_id: workspace_id.clone(),
            tool: tool.to_string(),
            pattern: pattern.to_string(),
            action: PolicyAction::Allow,
            scope,
            created_by: UserId::new("user"),
            created_at: Utc::now(),
        }
    }
}

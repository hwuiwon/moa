//! Action-policy rule contract tests.

use chrono::Utc;
use moa_core::{ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, TenantId, UserId};
use moa_security::ActionPolicyRuleStore;
use uuid::Uuid;

/// Verifies persistent action-policy rule CRUD.
pub async fn test_action_policy_rules<S>(store: &S)
where
    S: ActionPolicyRuleStore + ?Sized,
{
    let tenant_id = TenantId::from(Uuid::from_u128(1));
    let other_tenant_id = TenantId::from(Uuid::from_u128(2));
    let user_id = UserId::new("u1");
    let other_user_id = UserId::new("u2");
    let rule = ActionPolicyRule {
        id: Uuid::now_v7(),
        tool: "bash".to_string(),
        pattern: "git status".to_string(),
        effect: ActionPolicyEffect::AdminReview,
        scope: ActionRuleScope::Tenant { tenant_id },
        reason: Some("review repository command".to_string()),
        created_by: user_id.clone(),
        created_at: Utc::now(),
    };
    let tenant_override_rule = ActionPolicyRule {
        id: Uuid::now_v7(),
        tool: "bash".to_string(),
        pattern: "git push".to_string(),
        effect: ActionPolicyEffect::Deny,
        scope: ActionRuleScope::Tenant { tenant_id },
        reason: Some("tenant-specific deny".to_string()),
        created_by: user_id.clone(),
        created_at: Utc::now(),
    };

    store
        .upsert_action_policy_rule(rule.clone())
        .await
        .expect("upsert action policy rule");
    store
        .upsert_action_policy_rule(tenant_override_rule.clone())
        .await
        .expect("upsert tenant-scoped action policy rule");
    let rules = store
        .list_action_policy_rules_for_tool(&tenant_id, &user_id, "bash")
        .await
        .expect("list action policy rules");
    assert!(
        rules.iter().any(|candidate| candidate.id == rule.id
            && candidate.effect == ActionPolicyEffect::AdminReview)
    );
    assert!(
        rules
            .iter()
            .any(|candidate| candidate.id == tenant_override_rule.id
                && candidate.effect == ActionPolicyEffect::Deny)
    );
    let other_user_rules = store
        .list_action_policy_rules_for_tool(&tenant_id, &other_user_id, "bash")
        .await
        .expect("list action policy rules for other user");
    assert!(
        other_user_rules
            .iter()
            .any(|candidate| candidate.id == rule.id)
    );
    assert!(
        other_user_rules
            .iter()
            .any(|candidate| candidate.id == tenant_override_rule.id)
    );

    let other_tenant_rules = store
        .list_action_policy_rules_for_tool(&other_tenant_id, &user_id, "bash")
        .await
        .expect("list action policy rules for other tenant");
    assert!(
        !other_tenant_rules
            .iter()
            .any(|candidate| candidate.id == rule.id)
    );
    assert!(
        !other_tenant_rules
            .iter()
            .any(|candidate| candidate.id == tenant_override_rule.id)
    );

    store
        .delete_action_policy_rule(&tenant_id, None, &rule.tool, &rule.pattern)
        .await
        .expect("delete action policy rule");
    let rules = store
        .list_action_policy_rules_for_tool(&tenant_id, &user_id, "bash")
        .await
        .expect("list action policy rules after delete");
    assert!(!rules.iter().any(|candidate| candidate.id == rule.id));
    assert!(
        rules
            .iter()
            .any(|candidate| candidate.id == tenant_override_rule.id)
    );
}

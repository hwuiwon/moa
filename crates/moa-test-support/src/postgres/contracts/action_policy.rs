//! Action-policy rule contract tests.

use chrono::Utc;
use moa_core::{ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, UserId, WorkspaceId};
use moa_security::{ActionPolicyRuleStore, GLOBAL_ACTION_POLICY_WORKSPACE_ID};
use uuid::Uuid;

/// Verifies persistent action-policy rule CRUD.
pub async fn test_action_policy_rules<S>(store: &S)
where
    S: ActionPolicyRuleStore + ?Sized,
{
    let workspace_id = WorkspaceId::new("ws1");
    let user_id = UserId::new("u1");
    let other_user_id = UserId::new("u2");
    let rule = ActionPolicyRule {
        id: Uuid::now_v7(),
        workspace_id: workspace_id.clone(),
        user_id: None,
        tool: "bash".to_string(),
        pattern: "git status".to_string(),
        effect: ActionPolicyEffect::AdminReview,
        scope: ActionRuleScope::Workspace,
        reason: Some("review repository command".to_string()),
        created_by: user_id.clone(),
        created_at: Utc::now(),
    };
    let user_rule = ActionPolicyRule {
        id: Uuid::now_v7(),
        workspace_id: workspace_id.clone(),
        user_id: Some(user_id.clone()),
        tool: "bash".to_string(),
        pattern: "git push".to_string(),
        effect: ActionPolicyEffect::Deny,
        scope: ActionRuleScope::Workspace,
        reason: Some("user-specific deny".to_string()),
        created_by: user_id.clone(),
        created_at: Utc::now(),
    };

    store
        .upsert_action_policy_rule(rule.clone())
        .await
        .expect("upsert action policy rule");
    store
        .upsert_action_policy_rule(user_rule.clone())
        .await
        .expect("upsert user-scoped action policy rule");
    let global_rule = ActionPolicyRule {
        id: Uuid::now_v7(),
        workspace_id: WorkspaceId::new("source-workspace"),
        user_id: None,
        tool: "bash".to_string(),
        pattern: "git fetch".to_string(),
        effect: ActionPolicyEffect::Deny,
        scope: ActionRuleScope::Global,
        reason: Some("deployment-wide deny".to_string()),
        created_by: user_id.clone(),
        created_at: Utc::now(),
    };
    store
        .upsert_action_policy_rule(global_rule.clone())
        .await
        .expect("upsert global action policy rule");
    let rules = store
        .list_action_policy_rules_for_tool(&workspace_id, &user_id, "bash")
        .await
        .expect("list action policy rules");
    assert!(
        rules.iter().any(|candidate| candidate.id == rule.id
            && candidate.effect == ActionPolicyEffect::AdminReview)
    );
    assert!(
        rules.iter().any(|candidate| candidate.id == user_rule.id
            && candidate.effect == ActionPolicyEffect::Deny)
    );
    assert!(rules.iter().any(|candidate| candidate.id == global_rule.id
        && candidate.workspace_id == WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID)));

    let other_user_rules = store
        .list_action_policy_rules_for_tool(&workspace_id, &other_user_id, "bash")
        .await
        .expect("list action policy rules for other user");
    assert!(
        other_user_rules
            .iter()
            .any(|candidate| candidate.id == rule.id)
    );
    assert!(
        !other_user_rules
            .iter()
            .any(|candidate| candidate.id == user_rule.id)
    );

    let other_workspace_rules = store
        .list_action_policy_rules_for_tool(&WorkspaceId::new("ws2"), &user_id, "bash")
        .await
        .expect("list action policy rules for other workspace");
    assert!(
        !other_workspace_rules
            .iter()
            .any(|candidate| candidate.id == rule.id)
    );
    assert!(
        other_workspace_rules
            .iter()
            .any(|candidate| candidate.id == global_rule.id
                && candidate.workspace_id == WorkspaceId::new(GLOBAL_ACTION_POLICY_WORKSPACE_ID))
    );

    store
        .delete_action_policy_rule(&workspace_id, None, &rule.tool, &rule.pattern)
        .await
        .expect("delete action policy rule");
    let rules = store
        .list_action_policy_rules_for_tool(&workspace_id, &user_id, "bash")
        .await
        .expect("list action policy rules after delete");
    assert!(!rules.iter().any(|candidate| candidate.id == rule.id));
    assert!(rules.iter().any(|candidate| candidate.id == user_rule.id));
}

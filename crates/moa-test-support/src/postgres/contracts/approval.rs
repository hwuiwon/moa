//! Approval-rule contract tests.

use chrono::Utc;
use moa_core::{ApprovalRule, PolicyAction, PolicyScope, UserId, WorkspaceId};
use moa_security::ApprovalRuleStore;
use uuid::Uuid;

/// Verifies persistent approval-rule CRUD.
pub async fn test_approval_rules<S>(store: &S)
where
    S: ApprovalRuleStore + ?Sized,
{
    let workspace_id = WorkspaceId::new("ws1");
    let rule = ApprovalRule {
        id: Uuid::now_v7(),
        workspace_id: workspace_id.clone(),
        tool: "bash".to_string(),
        pattern: "git status".to_string(),
        action: PolicyAction::Allow,
        scope: PolicyScope::Workspace,
        created_by: UserId::new("u1"),
        created_at: Utc::now(),
    };

    store
        .upsert_approval_rule(rule.clone())
        .await
        .expect("upsert approval rule");
    let rules = store
        .list_approval_rules(&workspace_id)
        .await
        .expect("list approval rules");
    assert!(rules.iter().any(|candidate| candidate.id == rule.id));

    store
        .delete_approval_rule(&workspace_id, &rule.tool, &rule.pattern)
        .await
        .expect("delete approval rule");
    let rules = store
        .list_approval_rules(&workspace_id)
        .await
        .expect("list approval rules after delete");
    assert!(!rules.iter().any(|candidate| candidate.id == rule.id));
}

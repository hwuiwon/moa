//! Integration tests for skill artifact scope precedence.

#![recursion_limit = "256"]

mod support;

use moa_core::{ActionRuleScope, WorkspaceId};
use support::{
    configured_test_db, load_active_skill, load_active_skill_markdown, purge_skill_name,
    seed_skill, skill_markdown, workspace_scope,
};
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn load_visible_skill_resolves_tenant_scope_first_when_present() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let _guard = TEST_LOCK.lock().await;
    let workspace_id = WorkspaceId::new("scope-precedence-tenant");
    purge_skill_name(&test_db, "scope-skill").await;
    let tenant = workspace_scope(&workspace_id);
    seed_skill(
        &test_db,
        ActionRuleScope::WorkspaceDefault,
        &scope_skill("default body"),
    )
    .await;
    seed_skill(&test_db, tenant, &scope_skill("tenant body")).await;

    let resolved = load_active_skill(&test_db, &tenant, "scope-skill").await;
    let markdown = load_active_skill_markdown(&test_db, &tenant, "scope-skill").await;

    assert!(markdown.contains("tenant body"));
    assert_eq!(resolved.scope, "workspace");
    purge_skill_name(&test_db, "scope-skill").await;
}

#[tokio::test]
async fn load_visible_skill_falls_through_to_workspace_default_when_tenant_empty() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let _guard = TEST_LOCK.lock().await;
    let workspace_id = WorkspaceId::new("scope-precedence-workspace");
    purge_skill_name(&test_db, "scope-skill").await;
    let tenant = workspace_scope(&workspace_id);
    seed_skill(
        &test_db,
        ActionRuleScope::WorkspaceDefault,
        &scope_skill("default body"),
    )
    .await;

    let resolved = load_active_skill(&test_db, &tenant, "scope-skill").await;
    let markdown = load_active_skill_markdown(&test_db, &tenant, "scope-skill").await;

    assert!(markdown.contains("default body"));
    assert_eq!(resolved.scope, "global");
    purge_skill_name(&test_db, "scope-skill").await;
}

fn scope_skill(body: &str) -> String {
    skill_markdown("scope-skill", "Scope precedence fixture", body, "1.0")
}

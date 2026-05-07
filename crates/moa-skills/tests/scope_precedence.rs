//! Integration tests for three-tier skill scope precedence.

mod support;

use moa_core::{MemoryScope, UserId, WorkspaceId};
use support::{
    configured_test_db, load_active_skill, purge_skill_name, seed_skill, skill_markdown,
    user_scope, workspace_scope,
};
use tokio::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn load_visible_skill_resolves_user_scope_first_when_present() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let _guard = TEST_LOCK.lock().await;
    let workspace_id = WorkspaceId::new("scope-precedence-user");
    let user_id = UserId::new("user-a");
    purge_skill_name(&test_db, "scope-skill").await;
    let workspace = workspace_scope(&workspace_id);
    let user = user_scope(&workspace_id, &user_id);
    seed_skill(&test_db, MemoryScope::Global, &scope_skill("global body")).await;
    seed_skill(&test_db, workspace, &scope_skill("workspace body")).await;
    seed_skill(&test_db, user.clone(), &scope_skill("user body")).await;

    let resolved = load_active_skill(&test_db, &user, "scope-skill").await;

    assert!(resolved.body.contains("user body"));
    assert_eq!(resolved.scope, "user");
    purge_skill_name(&test_db, "scope-skill").await;
}

#[tokio::test]
async fn load_visible_skill_falls_through_to_workspace_when_user_scope_empty() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let _guard = TEST_LOCK.lock().await;
    let workspace_id = WorkspaceId::new("scope-precedence-workspace");
    let user_id = UserId::new("user-a");
    purge_skill_name(&test_db, "scope-skill").await;
    let workspace = workspace_scope(&workspace_id);
    let user = user_scope(&workspace_id, &user_id);
    seed_skill(&test_db, MemoryScope::Global, &scope_skill("global body")).await;
    seed_skill(&test_db, workspace, &scope_skill("workspace body")).await;

    let resolved = load_active_skill(&test_db, &user, "scope-skill").await;

    assert!(resolved.body.contains("workspace body"));
    assert_eq!(resolved.scope, "workspace");
    purge_skill_name(&test_db, "scope-skill").await;
}

#[tokio::test]
async fn load_visible_skill_falls_through_to_global_when_user_and_workspace_empty() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let _guard = TEST_LOCK.lock().await;
    let workspace_id = WorkspaceId::new("scope-precedence-global");
    let user_id = UserId::new("user-a");
    purge_skill_name(&test_db, "scope-skill").await;
    let user = user_scope(&workspace_id, &user_id);
    seed_skill(&test_db, MemoryScope::Global, &scope_skill("global body")).await;

    let resolved = load_active_skill(&test_db, &user, "scope-skill").await;

    assert!(resolved.body.contains("global body"));
    assert_eq!(resolved.scope, "global");
    purge_skill_name(&test_db, "scope-skill").await;
}

fn scope_skill(body: &str) -> String {
    skill_markdown("scope-skill", "Scope precedence fixture", body, "1.0")
}

//! Ignored live tests for `NeonBranchManager`.

use std::sync::OnceLock;
use std::time::Duration;

use moa_core::{
    BranchManager, MoaConfig, ModelId, SessionFilter, SessionMeta, SessionStore, UserId,
    WorkspaceId,
};
use moa_session::{NeonBranchManager, PostgresSessionStore};
use reqwest::Client;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct NeonBranchListResponse {
    branches: Vec<NeonBranch>,
}

#[derive(Debug, Deserialize)]
struct NeonBranch {
    id: String,
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
}

async fn resolve_parent_branch_id(project_id: &str) -> Option<String> {
    if let Ok(parent_branch_id) = std::env::var("NEON_PARENT_BRANCH_ID") {
        return Some(parent_branch_id);
    }

    let api_key = std::env::var("NEON_API_KEY").ok()?;
    let url = format!(
        "https://console.neon.tech/api/v2/projects/{project_id}/branches?limit=100&sort_by=created_at&sort_order=desc"
    );
    let response = Client::new()
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }

    let payload: NeonBranchListResponse = response.json().await.ok()?;
    payload
        .branches
        .into_iter()
        .find(|branch| branch.parent_id.is_none() && !branch.name.starts_with("moa-checkpoint-"))
        .map(|branch| branch.id)
}

async fn live_neon_config() -> Option<MoaConfig> {
    let project_id = std::env::var("NEON_PROJECT_ID").ok()?;
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("NEON_DB_URL").ok())?;
    let parent_branch_id = resolve_parent_branch_id(&project_id).await?;

    let mut config = MoaConfig::default();
    config.database.url = database_url;
    config.database.neon.enabled = true;
    config.database.neon.project_id = project_id;
    config.database.neon.parent_branch_id = parent_branch_id;
    Some(config)
}

async fn live_neon_config_with_limit(limit: usize) -> Option<MoaConfig> {
    let mut config = live_neon_config().await?;
    config.database.neon.max_checkpoints = limit;
    Some(config)
}

fn neon_live_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn wait_for_workspace_session_count(
    store: &PostgresSessionStore,
    workspace_id: &WorkspaceId,
    minimum: usize,
) -> Vec<moa_core::SessionSummary> {
    for _attempt in 0..20 {
        let sessions = store
            .list_sessions(SessionFilter {
                workspace_id: Some(workspace_id.clone()),
                ..SessionFilter::default()
            })
            .await
            .expect("list sessions");
        if sessions.len() >= minimum {
            return sessions;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    store
        .list_sessions(SessionFilter {
            workspace_id: Some(workspace_id.clone()),
            ..SessionFilter::default()
        })
        .await
        .expect("final list sessions")
}

#[tokio::test]
#[ignore = "requires NEON_API_KEY, NEON_PROJECT_ID, TEST_DATABASE_URL/NEON_DB_URL, and optional NEON_PARENT_BRANCH_ID"]
async fn neon_branch_manager_create_list_get_rollback_and_discard_checkpoint() {
    let _guard = neon_live_lock().lock().await;
    let Some(config) = live_neon_config().await else {
        eprintln!("skipping live Neon test; missing env");
        return;
    };
    let manager = NeonBranchManager::from_config(&config).expect("manager");

    let checkpoint = manager
        .create_checkpoint("live-smoke", None)
        .await
        .expect("create checkpoint");
    let checkpoints = manager.list_checkpoints().await.expect("list checkpoints");
    assert!(
        checkpoints
            .iter()
            .any(|entry| entry.handle.id == checkpoint.id)
    );
    let fetched = manager
        .get_checkpoint(&checkpoint.id)
        .await
        .expect("get checkpoint")
        .expect("checkpoint exists");
    assert_eq!(fetched.handle.id, checkpoint.id);
    assert_eq!(fetched.handle.label, checkpoint.label);
    manager
        .rollback_to(&checkpoint)
        .await
        .expect("rollback selection succeeds");

    manager
        .discard_checkpoint(&checkpoint)
        .await
        .expect("discard checkpoint");
}

#[tokio::test]
#[ignore = "requires NEON_API_KEY, NEON_PROJECT_ID, TEST_DATABASE_URL/NEON_DB_URL, and optional NEON_PARENT_BRANCH_ID"]
async fn neon_checkpoint_branch_connection_is_copy_on_write() {
    let _guard = neon_live_lock().lock().await;
    let Some(config) = live_neon_config().await else {
        eprintln!("skipping live Neon test; missing env");
        return;
    };
    let manager = NeonBranchManager::from_config(&config).expect("manager");
    let main_store = PostgresSessionStore::new(&config.database.url)
        .await
        .expect("main store");
    let workspace_id = WorkspaceId::new(format!("neon-live-{}", Uuid::now_v7().simple()));
    let seed_session_id = main_store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("neon-live-user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create seed session on main");
    let fresh_main_store = PostgresSessionStore::new(&config.database.url)
        .await
        .expect("fresh main store");
    let visible_on_main =
        wait_for_workspace_session_count(&fresh_main_store, &workspace_id, 1).await;
    assert!(
        visible_on_main
            .iter()
            .any(|session| session.session_id == seed_session_id)
    );

    let checkpoint = manager
        .create_checkpoint("cow-check", Some(seed_session_id))
        .await
        .expect("create checkpoint");
    let branch_store = PostgresSessionStore::new(&checkpoint.connection_url)
        .await
        .expect("branch store");

    let inherited = wait_for_workspace_session_count(&branch_store, &workspace_id, 1).await;
    assert!(
        inherited
            .iter()
            .any(|session| session.session_id == seed_session_id)
    );

    let branch_only_workspace =
        WorkspaceId::new(format!("neon-branch-{}", Uuid::now_v7().simple()));
    let branch_only_session = branch_store
        .create_session(SessionMeta {
            workspace_id: branch_only_workspace.clone(),
            user_id: UserId::new("branch-only-user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create branch-only session");

    let branch_sessions =
        wait_for_workspace_session_count(&branch_store, &branch_only_workspace, 1).await;
    assert!(
        branch_sessions
            .iter()
            .any(|session| session.session_id == branch_only_session)
    );

    let main_sessions = main_store
        .list_sessions(SessionFilter {
            workspace_id: Some(branch_only_workspace),
            ..SessionFilter::default()
        })
        .await
        .expect("list main sessions");
    assert!(
        main_sessions.is_empty(),
        "branch-only writes should not appear on the parent branch"
    );

    manager
        .discard_checkpoint(&checkpoint)
        .await
        .expect("discard checkpoint");
}

#[tokio::test]
#[ignore = "requires NEON_API_KEY, NEON_PROJECT_ID, TEST_DATABASE_URL/NEON_DB_URL, and optional NEON_PARENT_BRANCH_ID"]
async fn neon_checkpoint_cleanup_without_expired_branches_returns_zero() {
    let _guard = neon_live_lock().lock().await;
    let Some(config) = live_neon_config().await else {
        eprintln!("skipping live Neon test; missing env");
        return;
    };
    let manager = NeonBranchManager::from_config(&config).expect("manager");

    let checkpoint = manager
        .create_checkpoint("cleanup-zero", None)
        .await
        .expect("create checkpoint");
    let deleted = manager.cleanup_expired().await.expect("cleanup");
    assert_eq!(deleted, 0);
    manager
        .discard_checkpoint(&checkpoint)
        .await
        .expect("discard checkpoint");
}

#[tokio::test]
#[ignore = "requires NEON_API_KEY, NEON_PROJECT_ID, TEST_DATABASE_URL/NEON_DB_URL, and optional NEON_PARENT_BRANCH_ID"]
async fn neon_checkpoint_capacity_limit_rejects_extra_branch() {
    let _guard = neon_live_lock().lock().await;
    let Some(base_config) = live_neon_config().await else {
        eprintln!("skipping live Neon test; missing env");
        return;
    };
    let base_manager = NeonBranchManager::from_config(&base_config).expect("manager");
    let existing = base_manager
        .list_checkpoints()
        .await
        .expect("list checkpoints")
        .len();
    let Some(config) = live_neon_config_with_limit(existing + 1).await else {
        eprintln!("skipping live Neon test; missing env");
        return;
    };
    let manager = NeonBranchManager::from_config(&config).expect("manager with limit");

    let first = manager
        .create_checkpoint("capacity-one", None)
        .await
        .expect("create first checkpoint");
    let second = manager.create_checkpoint("capacity-two", None).await;
    assert!(
        second.is_err(),
        "second checkpoint should exceed configured cap"
    );

    manager
        .discard_checkpoint(&first)
        .await
        .expect("discard first checkpoint");
}

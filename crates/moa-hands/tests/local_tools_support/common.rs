use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    Event, HandProvider, HandResources, HandSpec, ModelId, SandboxFile, SandboxTier,
    SessionActorRef, SessionMeta, SessionStore, TenantId, ToolBudgetConfig, ToolInvocation,
};
use moa_hands::{LocalHandProvider, ToolRouter};
use moa_session::{PostgresSessionStore, testing};
use serde_json::json;
use tempfile::{TempDir, tempdir, tempdir_in};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

async fn admin_review_router(sandbox_root: impl AsRef<Path>) -> ToolRouter {
    let mut config = moa_core::MoaConfig::default();
    config.permissions.default_effect = moa_core::ActionPolicyEffect::AdminReview;
    ToolRouter::new_local(sandbox_root)
        .await
        .unwrap()
        .with_policies(
            moa_security::ActionPolicies::from_config(&config)
                .expect("admin-review test policy config should be valid"),
        )
}

fn docker_mountable_tempdir() -> TempDir {
    let macos_docker_tmp = Path::new("/private/tmp");
    if macos_docker_tmp.exists() {
        return tempdir_in(macos_docker_tmp).expect("create Docker-mountable tempdir");
    }
    tempdir().expect("create tempdir")
}

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: TenantId::new(),
        model: ModelId::new("claude-sonnet-4-6"),
        created_by: Some(SessionActorRef::Identity {
            id: uuid::Uuid::now_v7(),
        }),
        ..SessionMeta::default()
    }
}

fn approximate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

async fn test_session_store() -> Arc<PostgresSessionStore> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await.unwrap();
    Arc::new(store)
}

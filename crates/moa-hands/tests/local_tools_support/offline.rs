// Offline local-tools test support.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    HandProvider, HandResources, HandSpec, SandboxFile, SandboxTier, ToolBudgetConfig,
    ToolInvocation,
};
use moa_hands::{LocalHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
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

fn approximate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

// Offline local-tools test support.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{traits::HandProvider, types::hands::SandboxFile, types::hands::SandboxTier, types::completion::ToolInvocation};
use moa_config::ToolBudgetConfig;
use moa_hands::{LocalHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

async fn admin_review_router(sandbox_root: impl AsRef<Path>) -> ToolRouter {
    let mut config = moa_config::MoaConfig::default();
    config.permissions.default_effect = moa_core::types::action_policy::ActionPolicyEffect::AdminReview;
    ToolRouter::new_local(sandbox_root)
        .await
        .unwrap()
        .with_policies(
            moa_security::ActionPolicies::from_config(&config)
                .expect("admin-review test policy config should be valid"),
        )
}

async fn deny_router(sandbox_root: impl AsRef<Path>, denied_tools: &[&str]) -> ToolRouter {
    let mut config = moa_config::MoaConfig::default();
    config.permissions.always_deny = denied_tools.iter().map(|tool| tool.to_string()).collect();
    ToolRouter::new_local(sandbox_root)
        .await
        .unwrap()
        .with_policies(
            moa_security::ActionPolicies::from_config(&config)
                .expect("deny test policy config should be valid"),
        )
}

fn approximate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

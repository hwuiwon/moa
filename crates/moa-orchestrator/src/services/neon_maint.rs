//! Periodic Neon branch maintenance triggered by the CronJob virtual object.
//!
//! If `MOA_NEON_API_KEY` is unset, the service is a no-op so local development
//! and self-hosted deployments do not require Neon.

use moa_core::{config::MoaConfig, traits::BranchManager};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Report returned by a Neon branch prune pass.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PruneReport {
    /// Human-readable prune summary.
    pub summary: String,
    /// Number of branches examined. The Neon manager reports deleted branches,
    /// so this is the deleted count until its API exposes scan counts.
    pub branches_examined: u64,
    /// Number of expired branches deleted.
    pub branches_deleted: u64,
}

/// Keyless Restate service for periodic Neon branch maintenance.
#[restate_sdk::service]
pub trait NeonMaint {
    /// Prunes expired Neon checkpoint branches.
    async fn prune_branches(
        req: Json<serde_json::Value>,
    ) -> Result<Json<PruneReport>, HandlerError>;
}

/// Concrete Neon maintenance service implementation.
pub struct NeonMaintImpl {
    config: Arc<MoaConfig>,
}

impl NeonMaintImpl {
    /// Creates the Neon maintenance adapter with its branch-manager configuration.
    #[must_use]
    pub fn new(config: Arc<MoaConfig>) -> Self {
        Self { config }
    }
}

impl NeonMaint for NeonMaintImpl {
    #[tracing::instrument(skip(self, ctx, _request))]
    // SAFETY: Internal CronJob/maintenance handler; prunes external Neon branches only.
    async fn prune_branches(
        &self,
        ctx: Context<'_>,
        _request: Json<serde_json::Value>,
    ) -> Result<Json<PruneReport>, HandlerError> {
        annotate_restate_handler_span("NeonMaint", "prune_branches");
        let config = self.config.clone();

        Ok(ctx
            .run(|| async move {
                let api_key_configured = std::env::var("MOA_NEON_API_KEY")
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
                if !api_key_configured {
                    tracing::info!("MOA_NEON_API_KEY unset; skipping Neon branch prune");
                    return Ok::<_, HandlerError>(Json::from(PruneReport {
                        summary: "skipped (no MOA_NEON_API_KEY)".to_string(),
                        branches_examined: 0,
                        branches_deleted: 0,
                    }));
                }

                let manager = moa_session::NeonBranchManager::from_config(config.as_ref())
                    .map_err(|error| TerminalError::new(format!("neon manager init: {error}")))?;
                let deleted = manager
                    .cleanup_expired()
                    .await
                    .map_err(|error| TerminalError::new(format!("cleanup_expired: {error}")))?;
                let deleted = u64::from(deleted);

                Ok::<_, HandlerError>(Json::from(PruneReport {
                    summary: format!("pruned {deleted} expired branches"),
                    branches_examined: deleted,
                    branches_deleted: deleted,
                }))
            })
            .name("neon-prune")
            .await?)
    }
}

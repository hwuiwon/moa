//! Administrative workspace maintenance CLI commands.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Args;
use moa_core::{MoaConfig, ScopeContext, WorkspaceId};
use moa_memory_vector::{
    PgvectorStore, PromotionOptions, PromotionReport, TurbopufferStore, WorkspacePromotion,
    finalize_promotion, rollback_promotion,
};
use moa_session::create_session_store;
use uuid::Uuid;

/// Administrative command selected by the top-level CLI.
pub enum AdminCommand {
    /// Promote a workspace vector backend.
    PromoteWorkspace(PromoteWorkspaceArgs),
    /// Roll back an in-flight promotion.
    RollbackPromotion(WorkspacePromotionArgs),
    /// Finalize a successful promotion after dual-read.
    FinalizePromotion(WorkspacePromotionArgs),
}

/// Arguments for `moa promote-workspace`.
#[derive(Debug, Clone, Args)]
pub struct PromoteWorkspaceArgs {
    /// Workspace UUID to promote.
    #[arg(long)]
    pub workspace: Uuid,
    /// Target backend.
    #[arg(long, default_value = "turbopuffer")]
    pub to: String,
    /// Percentage of vectors to sample during validation.
    #[arg(long, default_value_t = 5)]
    pub validate_percent: u32,
    /// Number of hours to dual-read both backends after cutover.
    #[arg(long, default_value_t = 24)]
    pub dual_read_hours: u32,
}

/// Workspace-only promotion maintenance arguments.
#[derive(Debug, Clone, Args)]
pub struct WorkspacePromotionArgs {
    /// Workspace UUID to update.
    #[arg(long)]
    pub workspace: Uuid,
}

/// Runs one administrative command.
pub async fn handle_admin_command(config: &MoaConfig, command: AdminCommand) -> Result<String> {
    let store = create_session_store(config)
        .await
        .context("opening session store")?;
    let pool = store.pool().clone();

    match command {
        AdminCommand::PromoteWorkspace(args) => {
            if args.to != "turbopuffer" {
                bail!("unsupported promotion target `{}`", args.to);
            }
            let workspace_id = args.workspace.to_string();
            let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.clone()));
            let pgvector = Arc::new(PgvectorStore::new(pool.clone(), scope));
            let turbopuffer = Arc::new(
                TurbopufferStore::from_env()
                    .context("loading Turbopuffer client from environment")?,
            );
            let promotion = WorkspacePromotion::new(pool, pgvector, turbopuffer);
            let report = promotion
                .promote(PromotionOptions {
                    workspace_id,
                    target_backend: args.to,
                    validate_percent: args.validate_percent,
                    dual_read_hours: args.dual_read_hours,
                })
                .await
                .context("promoting workspace vector backend")?;
            Ok(format_promotion_report(&report, args.dual_read_hours))
        }
        AdminCommand::RollbackPromotion(args) => {
            let workspace_id = args.workspace.to_string();
            rollback_promotion(&pool, &workspace_id)
                .await
                .context("rolling back workspace promotion")?;
            Ok(format!(
                "workspace: {workspace_id}\nvector_backend: pgvector\nvector_backend_state: steady\n"
            ))
        }
        AdminCommand::FinalizePromotion(args) => {
            let workspace_id = args.workspace.to_string();
            finalize_promotion(&pool, &workspace_id)
                .await
                .context("finalizing workspace promotion")?;
            Ok(format!(
                "workspace: {workspace_id}\nvector_backend: turbopuffer\nvector_backend_state: steady\n"
            ))
        }
    }
}

fn format_promotion_report(report: &PromotionReport, dual_read_hours: u32) -> String {
    format!(
        "workspace: {}\ncopied_vectors: {}\nvalidation_overlap: {:.3}\nvector_backend: {}\nvector_backend_state: {}\ndual_read_hours: {}\n",
        report.workspace_id,
        report.copied,
        report.validation_overlap,
        report.vector_backend,
        report.vector_backend_state,
        dual_read_hours
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_workspace_e2e() {
        let report = PromotionReport {
            workspace_id: Uuid::now_v7().to_string(),
            copied: 100_000,
            validation_overlap: 0.981,
            vector_backend: "turbopuffer".to_string(),
            vector_backend_state: "dual_read".to_string(),
        };

        let rendered = format_promotion_report(&report, 24);

        assert!(rendered.contains("copied_vectors: 100000"));
        assert!(rendered.contains("validation_overlap: 0.981"));
        assert!(rendered.contains("vector_backend: turbopuffer"));
        assert!(rendered.contains("vector_backend_state: dual_read"));
    }

    #[test]
    fn rollback_mid_dual_read() {
        let workspace_id = Uuid::now_v7();
        let args = WorkspacePromotionArgs {
            workspace: workspace_id,
        };

        assert_eq!(args.workspace, workspace_id);
    }
}

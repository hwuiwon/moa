//! CLI commands for OCSF security-audit operations.

use anyhow::Result;
use clap::{Args, Subcommand};
use uuid::Uuid;

/// Audit command arguments.
#[derive(Debug, Args)]
pub(crate) struct AuditCommand {
    /// Audit action to run.
    #[command(subcommand)]
    pub(crate) action: AuditAction,
}

/// Security-audit actions.
#[derive(Debug, Subcommand)]
pub(crate) enum AuditAction {
    /// Verify one signed security event.
    Verify {
        /// Security event UUID.
        #[arg(long)]
        event: Uuid,
    },
}

/// Run an audit command.
pub(crate) async fn handle_audit_command(command: AuditCommand) -> Result<String> {
    let client = crate::client::client_from_credentials().await?;
    match command.action {
        AuditAction::Verify { event } => {
            let response = client.audit_verify(event).await?;
            let status = if response.valid { "PASS" } else { "FAIL" };
            Ok(format!(
                "{status} event={} tenant={}\n",
                response.event_id, response.tenant_id
            ))
        }
    }
}

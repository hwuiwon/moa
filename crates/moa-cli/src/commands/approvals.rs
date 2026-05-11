//! CLI commands for builtin approval decisions.

use anyhow::Result;
use clap::{Args, Subcommand};
use uuid::Uuid;

/// Approval command arguments.
#[derive(Debug, Args)]
pub(crate) struct ApprovalsCommand {
    /// Approval action to run.
    #[command(subcommand)]
    pub(crate) action: ApprovalsAction,
}

/// Approval actions.
#[derive(Debug, Subcommand)]
pub(crate) enum ApprovalsAction {
    /// List approvals pending the current user's decision.
    List,
    /// Approve an approval by id.
    Approve {
        /// Approval id.
        id: Uuid,
    },
    /// Deny an approval by id.
    Deny {
        /// Approval id.
        id: Uuid,
        /// Optional denial reason.
        #[arg(long)]
        reason: Option<String>,
    },
}

/// Run an approval command.
pub(crate) async fn handle_approvals_command(command: ApprovalsCommand) -> Result<String> {
    let client = crate::client::client_from_credentials().await?;
    match command.action {
        ApprovalsAction::List => {
            let approvals = client.approvals_list_mine().await?;
            if approvals.is_empty() {
                return Ok("No pending approvals.\n".to_string());
            }
            let mut output = String::new();
            for approval in approvals {
                output.push_str(&format!(
                    "{}\n  session: {}\n  expires: {}\n  {}\n\n",
                    approval.id, approval.session_id, approval.expires_at, approval.action_summary
                ));
            }
            Ok(output)
        }
        ApprovalsAction::Approve { id } => {
            client
                .approvals_decide(id, "approved".to_string(), None)
                .await?;
            Ok(format!("Approved {id}.\n"))
        }
        ApprovalsAction::Deny { id, reason } => {
            client
                .approvals_decide(id, "denied".to_string(), reason)
                .await?;
            Ok(format!("Denied {id}.\n"))
        }
    }
}

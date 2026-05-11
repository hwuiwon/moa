//! CLI commands for authorization administration.

use anyhow::Result;
use clap::{Args, Subcommand};
use uuid::Uuid;

/// Authz command arguments.
#[derive(Debug, Args)]
pub(crate) struct AuthzCommand {
    /// Authz action to run.
    #[command(subcommand)]
    pub(crate) action: AuthzAction,
}

/// Authorization administration actions.
#[derive(Debug, Subcommand)]
pub(crate) enum AuthzAction {
    /// Enqueue a raw tuple write after tenant-admin authorization.
    TupleWrite {
        /// Tuple subject, such as `api_key:<id>`.
        #[arg(long)]
        user: String,
        /// Tuple relation, such as `scim_admin`.
        #[arg(long)]
        relation: String,
        /// Tuple object, such as `tenant:<id>`.
        #[arg(long)]
        object: String,
        /// Tenant id when it cannot be derived from the object.
        #[arg(long)]
        tenant: Option<Uuid>,
    },
}

/// Run an authz command.
pub(crate) async fn handle_authz_command(command: AuthzCommand) -> Result<String> {
    let client = crate::client::client_from_credentials().await?;
    match command.action {
        AuthzAction::TupleWrite {
            user,
            relation,
            object,
            tenant,
        } => {
            client
                .authz_write_tuple(user.clone(), relation.clone(), object.clone(), tenant)
                .await?;
            Ok(format!(
                "Enqueued tuple write: {user} {relation} {object}\n"
            ))
        }
    }
}

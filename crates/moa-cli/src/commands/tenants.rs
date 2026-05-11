//! CLI commands for tenant audit administration.

use anyhow::Result;
use clap::{Args, Subcommand};
use moa_orchestrator_client::SetAuditDestinationRequest;
use uuid::Uuid;

/// Tenant command arguments.
#[derive(Debug, Args)]
pub(crate) struct TenantsCommand {
    /// Tenant action to run.
    #[command(subcommand)]
    pub(crate) action: TenantsAction,
}

/// Tenant administration actions.
#[derive(Debug, Subcommand)]
pub(crate) enum TenantsAction {
    /// Ensure a tenant has an active audit signing key.
    EnsureSigningKey {
        /// Tenant UUID.
        #[arg(long)]
        tenant: Uuid,
    },
    /// Rotate a tenant audit signing key.
    RotateSigningKey {
        /// Tenant UUID.
        #[arg(long)]
        tenant: Uuid,
    },
    /// Set the tenant audit S3 destination.
    SetAuditDestination {
        /// Tenant UUID.
        #[arg(long)]
        tenant: Uuid,
        /// S3 bucket name.
        #[arg(long)]
        bucket: String,
        /// AWS region.
        #[arg(long)]
        region: String,
        /// Optional role ARN to assume.
        #[arg(long)]
        assume_role: Option<String>,
        /// Optional key prefix.
        #[arg(long)]
        key_prefix: Option<String>,
        /// Object Lock retention in days.
        #[arg(long)]
        retention_days: Option<i32>,
        /// Optional KMS key ARN.
        #[arg(long)]
        kms_key: Option<String>,
    },
}

/// Run a tenant command.
pub(crate) async fn handle_tenants_command(command: TenantsCommand) -> Result<String> {
    let client = crate::client::client_from_credentials().await?;
    match command.action {
        TenantsAction::EnsureSigningKey { tenant } => {
            let key_id = client.tenants_ensure_signing_key(tenant).await?;
            Ok(format!("Tenant {tenant} active signing key: {key_id}\n"))
        }
        TenantsAction::RotateSigningKey { tenant } => {
            let key_id = client.tenants_rotate_signing_key(tenant).await?;
            Ok(format!("Tenant {tenant} rotated signing key: {key_id}\n"))
        }
        TenantsAction::SetAuditDestination {
            tenant,
            bucket,
            region,
            assume_role,
            key_prefix,
            retention_days,
            kms_key,
        } => {
            client
                .tenants_set_audit_destination(SetAuditDestinationRequest {
                    tenant_id: tenant,
                    bucket_name: bucket.clone(),
                    region,
                    assume_role_arn: assume_role,
                    key_prefix,
                    object_lock_days: retention_days,
                    encryption_kms_key_arn: kms_key,
                })
                .await?;
            Ok(format!(
                "Configured audit destination for tenant {tenant}: {bucket}\n"
            ))
        }
    }
}

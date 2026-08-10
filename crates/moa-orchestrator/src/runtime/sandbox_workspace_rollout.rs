//! Fail-closed database bootstrap and startup checks for sandbox-workspace rollout.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_config::{MoaConfig, SandboxWorkspaceMode, SandboxWorkspaceProviderAccountRef};
use moa_core::types::identifiers::{ProviderAccountId, TenantId};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

/// Validates durable database state before runtime dependency construction.
///
/// Disabled replicas must remain completely dark and therefore reject startup
/// while any workspace/provider cleanup owner is still required. Maintenance
/// and admit replicas defer mutating bootstrap to their separately authenticated
/// maintenance database pool.
pub async fn validate_startup_state(config: &MoaConfig, runtime_pool: &PgPool) -> Result<()> {
    if config.sandbox_workspaces.mode == SandboxWorkspaceMode::Disabled {
        validate_disabled(runtime_pool).await?;
    }
    Ok(())
}

async fn validate_disabled(pool: &PgPool) -> Result<()> {
    let has_durable_state: bool =
        sqlx::query_scalar("SELECT moa.has_durable_sandbox_workspace_state()")
            .fetch_one(pool)
            .await
            .context("inspect durable sandbox workspace state before disabled startup")?;
    if has_durable_state {
        bail!(
            "sandbox_workspaces.mode=disabled cannot start while durable workspace state requires maintenance; use maintenance to drain and reconcile first"
        );
    }
    Ok(())
}

/// Result of reconciling one configured tenant quota during startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantQuotaBootstrapOutcome {
    /// The quota row was created or updated to the configured limits.
    Applied,
    /// The existing quota row already matched the configured limits.
    Verified,
    /// An active tenant purge fence prevented all tenant-owned writes.
    SkippedFenced,
}

/// Tenant-scoped outcome returned by sandbox capacity bootstrap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TenantQuotaBootstrapResult {
    /// Tenant whose configured quota was reconciled.
    pub tenant_id: TenantId,
    /// Exact database reconciliation outcome.
    pub outcome: TenantQuotaBootstrapOutcome,
}

/// Outcomes from one atomic sandbox deployment bootstrap transaction.
#[derive(Debug, Default)]
pub struct SandboxWorkspaceBootstrapReport {
    /// Per-tenant quota reconciliation outcomes.
    pub tenant_quotas: Vec<TenantQuotaBootstrapResult>,
}

impl SandboxWorkspaceBootstrapReport {
    /// Returns tenants whose active purge fence denied quota reconciliation.
    #[must_use]
    pub fn fenced_tenants(&self) -> HashSet<TenantId> {
        self.tenant_quotas
            .iter()
            .filter_map(|result| {
                (result.outcome == TenantQuotaBootstrapOutcome::SkippedFenced)
                    .then_some(result.tenant_id)
            })
            .collect()
    }
}

/// Atomically bootstraps deployment-owned provider accounts and tenant quotas.
///
/// The caller must first authenticate `maintenance_pool` with
/// `WorkspaceMaintenanceCoordinator::verify_maintenance_pool`. This function
/// assumes the exact NOLOGIN maintenance role only for its transaction; the
/// ordinary runtime database role cannot execute either bootstrap function.
pub async fn bootstrap_accounts_and_quotas(
    config: &MoaConfig,
    maintenance_pool: &PgPool,
) -> Result<SandboxWorkspaceBootstrapReport> {
    if config.sandbox_workspaces.mode == SandboxWorkspaceMode::Disabled {
        bail!("sandbox workspace provider-account bootstrap requires maintenance or admit mode");
    }
    let mut transaction = maintenance_pool
        .begin()
        .await
        .context("begin sandbox workspace deployment bootstrap")?;
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *transaction)
        .await
        .context("assume sandbox workspace maintenance role for deployment bootstrap")?;
    let mut provider_limits: HashMap<(ProviderAccountId, u64), ProviderCapacityLimits> =
        HashMap::new();
    let mut report = SandboxWorkspaceBootstrapReport::default();
    for route in &config.sandbox_workspaces.quota_routes {
        let limits = provider_limits
            .entry((route.provider_account_id, route.provider_account_generation))
            .or_default();
        limits.add_route(route)?;

        let outcome: String =
            sqlx::query_scalar("SELECT moa.bootstrap_sandbox_tenant_capacity_limit($1, $2)")
                .bind(route.tenant_id)
                .bind(tenant_limits(route))
                .fetch_one(&mut *transaction)
                .await
                .with_context(|| {
                    format!(
                        "bootstrap sandbox workspace capacity for tenant {}",
                        route.tenant_id
                    )
                })?;
        report.tenant_quotas.push(TenantQuotaBootstrapResult {
            tenant_id: route.tenant_id,
            outcome: parse_tenant_quota_outcome(&outcome)?,
        });
    }

    for ((account_id, generation), limits) in provider_limits {
        let account = config
            .sandbox_workspace_provider_account(account_id, generation)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "sandbox workspace quota account {account_id} generation {generation} is not configured"
                )
            })?;
        bootstrap_provider_account(config, &mut transaction, account, limits).await?;
    }
    transaction
        .commit()
        .await
        .context("commit sandbox workspace deployment bootstrap")?;
    Ok(report)
}

fn parse_tenant_quota_outcome(value: &str) -> Result<TenantQuotaBootstrapOutcome> {
    match value {
        "applied" => Ok(TenantQuotaBootstrapOutcome::Applied),
        "verified" => Ok(TenantQuotaBootstrapOutcome::Verified),
        "skipped_fenced" => Ok(TenantQuotaBootstrapOutcome::SkippedFenced),
        _ => bail!("database returned unknown sandbox tenant quota bootstrap outcome {value:?}"),
    }
}

async fn bootstrap_provider_account(
    config: &MoaConfig,
    connection: &mut PgConnection,
    account: SandboxWorkspaceProviderAccountRef<'_>,
    limits: ProviderCapacityLimits,
) -> Result<()> {
    let organization_fingerprint = account.project_fingerprint.map_or_else(
        || format!("local:{}", account.isolation_cell),
        ToOwned::to_owned,
    );
    let headroom = if account.provider == "daytona" {
        config
            .cloud
            .daytona_storage
            .account(account.provider_account_id)
            .map_or_else(
                || json!({}),
                |storage| json!({ "volumes": storage.admission_headroom }),
            )
    } else {
        json!({})
    };
    let generation = i64::try_from(account.generation)
        .context("sandbox provider-account generation exceeds Postgres bigint")?;
    sqlx::query("SELECT moa.bootstrap_sandbox_provider_account($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(account.provider_account_id)
        .bind(generation)
        .bind(account.provider)
        .bind(account.isolation_cell)
        .bind(organization_fingerprint)
        .bind(account.project_fingerprint)
        .bind(limits.as_json())
        .bind(headroom)
        .execute(connection)
        .await
        .with_context(|| {
            format!(
                "bootstrap sandbox provider account {} generation {}",
                account.provider_account_id, account.generation
            )
        })?;
    Ok(())
}

#[derive(Debug, Default)]
struct ProviderCapacityLimits {
    workspaces: u64,
    volumes: u64,
    checkpoints: u64,
    logical_bytes: u64,
}

impl ProviderCapacityLimits {
    fn add_route(&mut self, route: &moa_config::SandboxWorkspaceQuotaRouteConfig) -> Result<()> {
        self.workspaces = checked_sum(self.workspaces, route.max_workspaces, "workspaces")?;
        self.volumes = checked_sum(self.volumes, route.max_active_hands, "volumes")?;
        self.checkpoints = checked_sum(self.checkpoints, route.max_checkpoints, "checkpoints")?;
        self.logical_bytes =
            checked_sum(self.logical_bytes, route.max_logical_bytes, "logical_bytes")?;
        Ok(())
    }

    fn as_json(&self) -> Value {
        json!({
            "workspaces": self.workspaces,
            "volumes": self.volumes,
            "checkpoints": self.checkpoints,
            "logical_bytes": self.logical_bytes,
        })
    }
}

fn tenant_limits(route: &moa_config::SandboxWorkspaceQuotaRouteConfig) -> Value {
    json!({
        "workspaces": route.max_workspaces,
        "volumes": route.max_active_hands,
        "checkpoints": route.max_checkpoints,
        "logical_bytes": route.max_logical_bytes,
    })
}

fn checked_sum(current: u64, value: u64, dimension: &str) -> Result<u64> {
    current.checked_add(value).ok_or_else(|| {
        anyhow::anyhow!("sandbox provider-account {dimension} quota total overflowed")
    })
}

#[cfg(test)]
mod tests {
    use moa_config::SandboxWorkspaceQuotaRouteConfig;
    use moa_core::types::identifiers::TenantId;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn provider_capacity_totals_are_bounded_and_account_scoped() {
        // Pins: multi-tenant canary quotas become one explicit provider-account
        // ceiling instead of an absent limit interpreted as unbounded.
        let account_id = ProviderAccountId(Uuid::from_u128(1));
        let mut limits = ProviderCapacityLimits::default();
        for tenant in [2, 3] {
            limits
                .add_route(&SandboxWorkspaceQuotaRouteConfig {
                    tenant_id: TenantId(Uuid::from_u128(tenant)),
                    provider_account_id: account_id,
                    provider_account_generation: 1,
                    max_workspaces: 4,
                    max_active_hands: 2,
                    max_checkpoints: 10,
                    max_logical_bytes: 1_024,
                })
                .expect("bounded route should aggregate");
        }
        assert_eq!(
            limits.as_json(),
            json!({
                "workspaces": 8,
                "volumes": 4,
                "checkpoints": 20,
                "logical_bytes": 2_048,
            })
        );
    }
}

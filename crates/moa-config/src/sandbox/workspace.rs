//! Durable sandbox-workspace rollout, quota, and runtime validation configuration.

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};
use moa_core::types::identifiers::{ProviderAccountId, TenantId};

use crate::MoaConfig;
use crate::authz::AuthzEngine;
use crate::kms::KmsProviderKind;
use crate::object_store::ObjectStoreCredentialMode;
use crate::security::SecurityProfile;

/// One resolved deployment-owned provider-account mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxWorkspaceProviderAccountRef<'a> {
    /// Durable account identifier.
    pub provider_account_id: ProviderAccountId,
    /// Exact durable account generation.
    pub generation: u64,
    /// Stable provider registry name.
    pub provider: &'a str,
    /// Operator-defined physical isolation cell.
    pub isolation_cell: &'a str,
    /// Immutable cloud project fingerprint, absent only for the local provider.
    pub project_fingerprint: Option<&'a str>,
}

/// Rollout state for the durable sandbox-workspace subsystem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxWorkspaceMode {
    /// Do not construct mutation owners, maintenance jobs, or admission surfaces.
    #[default]
    Disabled,
    /// Run cleanup/reconciliation only and reject all new work.
    Maintenance,
    /// Run maintenance and permit authorized canary admission.
    Admit,
}

impl SandboxWorkspaceMode {
    /// Returns whether durable cleanup and reconciliation must run.
    #[must_use]
    pub const fn maintenance_enabled(self) -> bool {
        matches!(self, Self::Maintenance | Self::Admit)
    }

    /// Returns whether new workspace admission is permitted.
    #[must_use]
    pub const fn admission_enabled(self) -> bool {
        matches!(self, Self::Admit)
    }
}

/// Canary route selected exclusively by deployment configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceCanaryConfig {
    /// Persisted provider account selected for the canary.
    pub provider_account_id: ProviderAccountId,
    /// Exact persisted account generation selected for the canary.
    pub provider_account_generation: u64,
    /// Isolation cell that must match the provider-account bootstrap mapping.
    pub isolation_cell: String,
    /// Tenants allowed to create or attach writers while mode is `admit`.
    pub tenant_allowlist: Vec<TenantId>,
}

/// Explicit tenant and provider-account capacity fence for one canary route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceQuotaRouteConfig {
    /// Tenant receiving this bounded route.
    pub tenant_id: TenantId,
    /// Persisted provider account owning the capacity fence.
    pub provider_account_id: ProviderAccountId,
    /// Exact persisted account generation owning the capacity fence.
    pub provider_account_generation: u64,
    /// Maximum retained workspaces for this tenant/account route.
    pub max_workspaces: u64,
    /// Maximum concurrently attached or provisioning hands.
    pub max_active_hands: u64,
    /// Maximum retained checkpoint records.
    pub max_checkpoints: u64,
    /// Maximum logical checkpoint bytes retained by the route.
    pub max_logical_bytes: u64,
}

/// Deployment-owned durable sandbox-workspace rollout policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxWorkspacesConfig {
    /// Current rollout mode. Defaults dark and fail-safe.
    pub mode: SandboxWorkspaceMode,
    /// Server-side Restate retention for workspace operations and replay owners.
    pub operation_retention_seconds: u64,
    /// Maximum time one provider operation may remain in-flight before reconciliation.
    pub maximum_operation_seconds: u64,
    /// Lease duration for one ambiguous-operation reconciliation claim.
    pub reconciliation_claim_ttl_seconds: u64,
    /// Maximum age of a supervised reaper heartbeat accepted by readiness.
    pub reaper_heartbeat_maximum_age_seconds: u64,
    /// Optional deployment canary route; required in `admit`.
    pub canary: Option<SandboxWorkspaceCanaryConfig>,
    /// Explicit bounded tenant/account routes; required for every canary tenant in `admit`.
    pub quota_routes: Vec<SandboxWorkspaceQuotaRouteConfig>,
}

impl Default for SandboxWorkspacesConfig {
    fn default() -> Self {
        Self {
            mode: SandboxWorkspaceMode::Disabled,
            operation_retention_seconds: 7 * 24 * 60 * 60,
            maximum_operation_seconds: 24 * 60 * 60,
            reconciliation_claim_ttl_seconds: 60,
            reaper_heartbeat_maximum_age_seconds: 60,
            canary: None,
            quota_routes: Vec::new(),
        }
    }
}

impl SandboxWorkspacesConfig {
    /// Validates replica-consistent rollout, retention, canary, and quota policy.
    pub fn validate(&self) -> Result<()> {
        if !self.mode.maintenance_enabled() {
            return Ok(());
        }
        let minimum_retention = self
            .maximum_operation_seconds
            .checked_add(self.reconciliation_claim_ttl_seconds)
            .ok_or_else(|| {
                MoaError::ConfigError(
                    "sandbox workspace operation retention arithmetic overflowed".to_string(),
                )
            })?;
        let reaper_interval_seconds = (self.reaper_heartbeat_maximum_age_seconds / 3).clamp(1, 10);
        if self.maximum_operation_seconds == 0
            || self.reconciliation_claim_ttl_seconds == 0
            || self.reconciliation_claim_ttl_seconds > i64::MAX as u64
            || self.reconciliation_claim_ttl_seconds <= reaper_interval_seconds
            || self.reaper_heartbeat_maximum_age_seconds == 0
            || self.reconciliation_claim_ttl_seconds > self.reaper_heartbeat_maximum_age_seconds
            || self.operation_retention_seconds < minimum_retention
        {
            return Err(MoaError::ConfigError(
                "sandbox workspace operation retention must cover the maximum operation plus reconciliation claim TTL, and the claim TTL must exceed the positive reaper interval without exceeding the reaper heartbeat maximum age"
                    .to_string(),
            ));
        }
        if self.quota_routes.is_empty() {
            return Err(MoaError::ConfigError(
                "sandbox workspace maintenance requires explicit tenant/provider-account quota routes"
                    .to_string(),
            ));
        }
        let mut routes = std::collections::HashSet::new();
        for route in &self.quota_routes {
            let identity = (
                route.tenant_id,
                route.provider_account_id,
                route.provider_account_generation,
            );
            if !routes.insert(identity)
                || route.provider_account_generation == 0
                || route.max_workspaces == 0
                || route.max_active_hands == 0
                || route.max_checkpoints == 0
                || route.max_logical_bytes == 0
            {
                return Err(MoaError::ConfigError(
                    "sandbox workspace quota routes must be unique and have positive capacity limits"
                        .to_string(),
                ));
            }
        }
        if !self.mode.admission_enabled() {
            return Ok(());
        }

        let canary = self.canary.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "sandbox workspaces admit mode requires an explicit canary route".to_string(),
            )
        })?;
        if canary.provider_account_generation == 0
            || canary.isolation_cell.trim().is_empty()
            || canary.tenant_allowlist.is_empty()
        {
            return Err(MoaError::ConfigError(
                "sandbox workspace canary requires an account generation, isolation cell, and non-empty tenant allowlist"
                    .to_string(),
            ));
        }
        let mut tenants = std::collections::HashSet::new();
        if canary
            .tenant_allowlist
            .iter()
            .any(|tenant_id| !tenants.insert(*tenant_id))
        {
            return Err(MoaError::ConfigError(
                "sandbox workspace canary tenant allowlist contains duplicates".to_string(),
            ));
        }
        if self.quota_routes.iter().any(|route| {
            route.provider_account_id != canary.provider_account_id
                || route.provider_account_generation != canary.provider_account_generation
                || !tenants.contains(&route.tenant_id)
        }) {
            return Err(MoaError::ConfigError(
                "sandbox workspace admit quota routes must match the exact canary account and tenant set"
                    .to_string(),
            ));
        }
        for tenant_id in &canary.tenant_allowlist {
            if !routes.contains(&(
                *tenant_id,
                canary.provider_account_id,
                canary.provider_account_generation,
            )) {
                return Err(MoaError::ConfigError(format!(
                    "sandbox workspace canary tenant {tenant_id} has no exact provider-account quota route"
                )));
            }
        }
        Ok(())
    }
}

impl MoaConfig {
    /// Validates fail-closed dependencies required before workspace maintenance
    /// or admission owners are constructed.
    pub fn validate_sandbox_workspace_runtime(&self, skip_fga: bool) -> Result<()> {
        if !self.sandbox_workspaces.mode.maintenance_enabled() {
            return Ok(());
        }
        self.object_store.validate()?;
        self.sandbox_checkpoints.validate()?;
        self.sandbox_workspaces.validate()?;
        let maintenance_url = self
            .database
            .maintenance_url()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                MoaError::ConfigError(
                    "sandbox workspace maintenance requires database.maintenance_url with a dedicated least-privilege login"
                        .to_string(),
                )
            })?;
        if maintenance_url == self.database.runtime_url()
            || maintenance_url == self.database.admin_url()
        {
            return Err(MoaError::ConfigError(
                "sandbox workspace maintenance URL must be distinct from runtime and admin database credentials"
                    .to_string(),
            ));
        }
        if skip_fga {
            return Err(MoaError::ConfigError(
                "MOA_SKIP_FGA is valid only when sandbox_workspaces.mode is disabled".to_string(),
            ));
        }
        let openfga = self.authz.openfga.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "sandbox workspace maintenance requires configured OpenFGA".to_string(),
            )
        })?;
        if self.authz.engine != AuthzEngine::Openfga
            || openfga.url.trim().is_empty()
            || openfga.store_id.trim().is_empty()
            || openfga.model_id.trim().is_empty()
            || openfga.model_version != 7
        {
            return Err(MoaError::ConfigError(format!(
                "sandbox workspace maintenance requires configured OpenFGA and exact OpenFGA model version 7, got {}",
                openfga.model_version
            )));
        }
        if self.kms.provider != KmsProviderKind::Postgres || self.kms.allow_ephemeral {
            return Err(MoaError::ConfigError(
                "sandbox workspace maintenance requires durable Postgres KMS with ephemeral keys disabled"
                    .to_string(),
            ));
        }
        if !self.sandbox_checkpoints.enabled {
            return Err(MoaError::ConfigError(
                "sandbox workspace maintenance requires portable checkpoints".to_string(),
            ));
        }
        let cloud_accounts = self
            .cloud
            .hands
            .as_ref()
            .map(|hands| hands.provider_accounts.as_slice())
            .unwrap_or_default();
        if cloud_accounts.is_empty() && self.local.provider_account.is_none() {
            return Err(MoaError::ConfigError(
                "sandbox workspace maintenance requires provider-account bootstrap mappings"
                    .to_string(),
            ));
        }
        let mut account_ids = std::collections::HashSet::new();
        if let Some(account) = &self.local.provider_account {
            if account.generation == 0 || account.isolation_cell.trim().is_empty() {
                return Err(MoaError::ConfigError(
                    "local sandbox workspace provider-account bootstrap requires a positive generation and isolation cell"
                        .to_string(),
                ));
            }
            account_ids.insert(account.provider_account_id);
        }
        for account in cloud_accounts {
            if !account_ids.insert(account.provider_account_id)
                || account.generation == 0
                || account.isolation_cell.trim().is_empty()
                || account
                    .project_fingerprint
                    .as_deref()
                    .is_none_or(|fingerprint| fingerprint.trim().is_empty())
            {
                return Err(MoaError::ConfigError(
                    "sandbox workspace provider-account bootstrap requires unique ids, positive generations, isolation cells, and immutable project fingerprints"
                        .to_string(),
                ));
            }
        }
        if self.sandbox_workspaces.quota_routes.iter().any(|route| {
            self.sandbox_workspace_provider_account(
                route.provider_account_id,
                route.provider_account_generation,
            )
            .is_none()
        }) {
            return Err(MoaError::ConfigError(
                "sandbox workspace quota route does not match an exact provider-account bootstrap mapping"
                    .to_string(),
            ));
        }
        if let Some(canary) = &self.sandbox_workspaces.canary {
            let matches_account = self
                .sandbox_workspace_provider_account(
                    canary.provider_account_id,
                    canary.provider_account_generation,
                )
                .is_some_and(|account| account.isolation_cell == canary.isolation_cell);
            if !matches_account {
                return Err(MoaError::ConfigError(
                    "sandbox workspace canary does not match an exact provider-account bootstrap mapping"
                        .to_string(),
                ));
            }
        }
        if self.security_profile == SecurityProfile::Cloud
            && self.object_store.credential_mode != ObjectStoreCredentialMode::WorkloadIdentity
        {
            return Err(MoaError::ConfigError(
                "cloud sandbox workspace maintenance requires object-store workload identity"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Resolves one exact deployment-owned local or cloud provider-account mapping.
    ///
    /// This is the canonical routing/bootstrap lookup. It never falls back to a
    /// different account or generation.
    #[must_use]
    pub fn sandbox_workspace_provider_account(
        &self,
        provider_account_id: ProviderAccountId,
        generation: u64,
    ) -> Option<SandboxWorkspaceProviderAccountRef<'_>> {
        if let Some(account) = self.local.provider_account.as_ref().filter(|account| {
            account.provider_account_id == provider_account_id && account.generation == generation
        }) {
            return Some(SandboxWorkspaceProviderAccountRef {
                provider_account_id: account.provider_account_id,
                generation: account.generation,
                provider: "local",
                isolation_cell: &account.isolation_cell,
                project_fingerprint: None,
            });
        }
        let account = self
            .cloud
            .hands
            .as_ref()?
            .provider_accounts
            .iter()
            .find(|account| {
                account.provider_account_id == provider_account_id
                    && account.generation == generation
            })?;
        Some(SandboxWorkspaceProviderAccountRef {
            provider_account_id: account.provider_account_id,
            generation: account.generation,
            provider: account.provider.as_str(),
            isolation_cell: &account.isolation_cell,
            project_fingerprint: account.project_fingerprint.as_deref(),
        })
    }
}

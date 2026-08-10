//! Cloud hand and Daytona storage provider-account configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};
use moa_core::types::identifiers::ProviderAccountId;

/// Cloud runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    /// Optional alternate memory root for cloud deployments.
    pub memory_dir: Option<String>,
    /// Optional hands configuration.
    pub hands: Option<CloudHandsConfig>,
    /// Daytona tenant-volume isolation cells and admission ceilings.
    pub daytona_storage: DaytonaStorageConfig,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            memory_dir: None,
            hands: Some(CloudHandsConfig::default()),
            daytona_storage: DaytonaStorageConfig::default(),
        }
    }
}

/// Daytona tenant-volume settings keyed by persisted provider account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaytonaStorageConfig {
    /// Per-account security classes and volume admission bounds.
    pub accounts: Vec<DaytonaStorageAccountConfig>,
    /// Delay separating the two empty-prefix observations used by cleanup.
    pub consistency_window_seconds: u64,
}

impl DaytonaStorageConfig {
    /// Validates uniqueness and Daytona's documented organization ceiling.
    pub fn validate(&self) -> Result<()> {
        let mut account_ids = std::collections::HashSet::new();
        for account in &self.accounts {
            if !account_ids.insert(account.provider_account_id)
                || account.security_class.trim().is_empty()
                || account.volume_ceiling == 0
                || account.volume_ceiling > 100
                || account.admission_headroom >= account.volume_ceiling
            {
                return Err(MoaError::ConfigError(
                    "Daytona storage accounts require unique ids, a security class, a 1..=100 volume ceiling, and headroom below the ceiling".to_string(),
                ));
            }
        }
        if !self.accounts.is_empty() && self.consistency_window_seconds == 0 {
            return Err(MoaError::ConfigError(
                "Daytona storage cleanup consistency window must be positive".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the exact configured Daytona storage cell for an account.
    #[must_use]
    pub fn account(
        &self,
        provider_account_id: ProviderAccountId,
    ) -> Option<&DaytonaStorageAccountConfig> {
        self.accounts
            .iter()
            .find(|account| account.provider_account_id == provider_account_id)
    }
}

/// One Daytona organization/provider-account volume isolation cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaytonaStorageAccountConfig {
    /// Persisted provider account owning the organization/cell.
    pub provider_account_id: ProviderAccountId,
    /// Operator-authored workload isolation tier.
    pub security_class: String,
    /// Contract ceiling, never above Daytona's documented 100 volumes.
    pub volume_ceiling: u16,
    /// Capacity retained for cleanup, reconciliation, and operator recovery.
    pub admission_headroom: u16,
}

/// Cloud hand provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudHandsConfig {
    /// Default hand provider.
    pub default_provider: Option<String>,
    /// Ordered fallback cloud providers attempted when the selected cloud hand is unavailable.
    pub fallback_providers: Vec<String>,
    /// Operator-authored, non-secret provider-account mappings.
    pub provider_accounts: Vec<CloudHandProviderAccountConfig>,
}

/// A supported cloud sandbox control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudHandProviderKind {
    /// Daytona sandbox control plane.
    Daytona,
    /// E2B sandbox control plane.
    E2b,
}

impl CloudHandProviderKind {
    /// Returns the stable provider registry name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daytona => "daytona",
            Self::E2b => "e2b",
        }
    }
}

/// Typed selector for one operator-owned provider credential file.
///
/// The selector is non-secret. Runtime resolution rejects symlinks, requires
/// the configured Unix owner, and rejects every group/world permission bit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSecretFileSelector {
    /// Absolute path to the mounted credential file.
    pub path: PathBuf,
    /// Required Unix owner UID.
    pub owner_uid: u32,
}

/// Non-secret configuration for one persisted sandbox provider account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudHandProviderAccountConfig {
    /// Durable account identity persisted on workspaces and cloud hand handles.
    pub provider_account_id: ProviderAccountId,
    /// Durable account generation. Account replacement increments it.
    pub generation: u64,
    /// Provider implementation serving this account.
    pub provider: CloudHandProviderKind,
    /// Operator-defined tenant/isolation cell.
    pub isolation_cell: String,
    /// Exact canonical HTTPS control-plane origin, with no path.
    pub api_origin: String,
    /// Exact canonical HTTPS Daytona toolbox origin, with no path.
    pub toolbox_origin: Option<String>,
    /// E2B sandbox traffic domain.
    pub sandbox_domain: Option<String>,
    /// Provider-specific default image or template identifier.
    pub default_runtime: Option<String>,
    /// Optional project/organization fingerprint for physical isolation.
    pub project_fingerprint: Option<String>,
    /// Credential file selected by this account mapping.
    pub credential: ProviderSecretFileSelector,
}

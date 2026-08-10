//! General local hand, sandbox policy, and MCP configuration.

pub mod checkpoint;
pub mod cloud;
pub mod workspace;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use moa_core::error::Result;
use moa_core::types::hands::{
    CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit, SandboxPolicySnapshot,
    SandboxProfile,
};
use moa_core::types::identifiers::ProviderAccountId;
use moa_core::types::security::SensitivityClass;

/// Revision naming the built-in, deliberately unbounded local-development
/// sandbox policy.
///
/// It is a stated policy, not an inferred one: it has a name, it enters the
/// effective-profile hash like any operator-authored revision, and
/// `security_profile = cloud` rejects it outright, so a cloud deployment must
/// author its own six dimensions.
pub const LOCAL_DEVELOPMENT_SANDBOX_REVISION: &str = "local-development-unbounded";

/// One authored six-dimension sandbox policy layer.
///
/// Every dimension is required and typed. `Unbounded` is how a deployment says
/// "no limit" on purpose; there is no zero, no `None`, and no omitted field
/// that means the same thing by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfileConfig {
    /// Revision identifying this authored layer. Enters the policy identity hash.
    pub revision: String,
    /// CPU allocation.
    pub cpu: CpuLimit,
    /// Resident memory allocation.
    pub memory: MemoryLimit,
    /// Ephemeral scratch disk allocation.
    pub ephemeral_disk: DiskLimit,
    /// Outbound network policy.
    pub egress: EgressPolicy,
    /// Idle timeout.
    pub idle_timeout: LifetimeLimit,
    /// Hard maximum lifetime.
    pub max_lifetime: LifetimeLimit,
}

impl SandboxProfileConfig {
    /// Builds the validated policy snapshot this layer declares.
    pub fn snapshot(&self) -> Result<SandboxPolicySnapshot> {
        SandboxPolicySnapshot::new(
            &self.revision,
            SandboxProfile::new(
                self.cpu,
                self.memory,
                self.ephemeral_disk,
                self.egress.clone(),
                self.idle_timeout,
                self.max_lifetime,
            )?,
        )
    }
}

/// Deployment sandbox policy plus its per-route layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicyConfig {
    /// The deployment-wide layer, outermost of the four.
    pub deployment: SandboxProfileConfig,
    /// Per-hand-provider route layers, keyed by provider name (`local`,
    /// `daytona`, `e2b`). A provider with no authored entry contributes a
    /// named, non-restricting route layer rather than an absent one.
    #[serde(default)]
    pub routes: BTreeMap<String, SandboxProfileConfig>,
}

impl SandboxPolicyConfig {
    /// Returns whether the deployment layer is the built-in local-development
    /// policy rather than one an operator authored.
    #[must_use]
    pub fn is_local_development_default(&self) -> bool {
        self.deployment.revision == LOCAL_DEVELOPMENT_SANDBOX_REVISION
    }

    /// Returns the authored route layer for one hand provider, when present.
    #[must_use]
    pub fn route(&self, provider: &str) -> Option<&SandboxProfileConfig> {
        self.routes.get(provider)
    }
}

impl Default for SandboxPolicyConfig {
    fn default() -> Self {
        Self {
            deployment: SandboxProfileConfig {
                revision: LOCAL_DEVELOPMENT_SANDBOX_REVISION.to_string(),
                cpu: CpuLimit::Unbounded,
                memory: MemoryLimit::Unbounded,
                ephemeral_disk: DiskLimit::Unbounded,
                egress: EgressPolicy::Unrestricted,
                idle_timeout: LifetimeLimit::Unbounded,
                max_lifetime: LifetimeLimit::Unbounded,
            },
            routes: BTreeMap::new(),
        }
    }
}

/// Local runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// Whether local Docker hands are enabled.
    pub docker_enabled: bool,
    /// Sandbox working directory.
    pub sandbox_dir: String,
    /// Memory root directory.
    pub memory_dir: String,
    /// Optional durable provider-account identity for the local deterministic lane.
    pub provider_account: Option<LocalHandProviderAccountConfig>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            docker_enabled: true,
            sandbox_dir: "~/.moa/sandbox".to_string(),
            memory_dir: "~/.moa/memory".to_string(),
            provider_account: None,
        }
    }
}

/// Deployment-owned account identity for the local deterministic hand provider.
///
/// Local hands have no remote credential or control-plane origin, but durable
/// bindings still require the same non-caller-selectable account generation and
/// isolation-cell fence as cloud providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalHandProviderAccountConfig {
    /// Durable account identity persisted on workspaces and local hand handles.
    pub provider_account_id: ProviderAccountId,
    /// Durable account generation. Local account replacement increments it.
    pub generation: u64,
    /// Operator-defined single-replica or globally reachable isolation cell.
    pub isolation_cell: String,
}

/// Credential injection mode for an MCP server.
///
/// Every variant names an operator-authored environment variable that is read
/// once when the tool router is constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpCredentialConfig {
    /// Attach a bearer token from an environment variable.
    Bearer {
        /// Environment variable containing the token.
        token_env: String,
    },
    /// Attach an API key header from an environment variable.
    ApiKey {
        /// Header name expected by the upstream service.
        header: String,
        /// Environment variable containing the header value.
        value_env: String,
    },
}

/// One configured MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stable MCP server name.
    pub name: String,
    /// Remote MCP endpoint URL.
    pub url: String,
    /// Optional credential injection configuration.
    pub credentials: Option<McpCredentialConfig>,
    /// Whether standard MCP tool annotations from this server may affect retry safety.
    #[serde(default)]
    pub trust_tool_annotations: bool,
    /// Data classes this external MCP server is permitted to receive.
    ///
    /// This is a conservative egress allowlist. When empty (the default for an
    /// existing config), only [`SensitivityClass::None`] content may be sent to
    /// the server; `pii`, `phi`, and `restricted` payloads are blocked unless the
    /// operator explicitly lists them here.
    #[serde(default)]
    pub allowed_data_classes: Vec<SensitivityClass>,
    /// Whether this connector must be reachable for the deployment to serve.
    ///
    /// Optional (the default) means a discovery failure removes only this
    /// server's tools and leaves every other connector's tools available. A
    /// required connector's discovery failure is a startup failure carrying the
    /// connector's health, because a deployment that silently drops a required
    /// integration looks identical to one that never configured it.
    #[serde(default)]
    pub required: bool,
    /// When this connector's tools are discovered.
    #[serde(default)]
    pub discovery: McpDiscoveryMode,
}

/// When a connector's tool catalog is discovered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpDiscoveryMode {
    /// Discover this connector's tools while the router is being built.
    #[default]
    Eager,
    /// Contribute no tools at startup and discover on the first background
    /// catalog refresh, so a slow or unused connector never delays startup.
    Lazy,
}

impl McpDiscoveryMode {
    /// Returns whether discovery runs during router construction.
    #[must_use]
    pub fn is_eager(self) -> bool {
        matches!(self, Self::Eager)
    }
}

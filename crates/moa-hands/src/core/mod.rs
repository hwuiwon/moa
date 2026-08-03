//! Tool routing core, registry, policy, dispatch, and recovery for MOA hands.

mod construction;
mod dispatch;
pub mod leases;
mod lifecycle;
pub mod mcp_catalog;
mod normalization;
mod output_budget;
mod policy;
pub mod profile;
pub mod reaper;
mod recovery;
mod registration;
mod telemetry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_config::McpServerConfig;
use moa_config::ToolBudgetConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider, traits::MemoryRetrievalExecutor,
    traits::MemoryToolExecutor, traits::SessionStore, types::action_policy::CallOrigin,
    types::hands::HandHandle, types::hands::SandboxFile, types::hands::SandboxPolicySnapshot,
    types::identifiers::TenantId, types::resource::ResourceBudget,
};
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, McpDeploymentCredentials, McpEgressGuard,
    UnmatchedPermissionPattern,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::adapters::local::LocalHandProvider;

pub use dispatch::PendingConnectorToolOutput;
use leases::HandLeaseStore;
pub use mcp_catalog::{
    CandidateConnector, CatalogDefect, McpCatalogActivation, McpCatalogRefresh, McpConnectorHealth,
    PinnedToolContract, PinnedToolOwner, ToolCatalogDrift, ToolCatalogPin,
    spawn_mcp_catalog_refresh,
};
pub use policy::{ActionOrigin, PreparedActionInvocation};
pub use profile::TenantSandboxPolicyStore;
pub use profile::{
    PostgresTenantSandboxPolicyStore, deployment_sandbox_policy, local_development_sandbox_policy,
    route_sandbox_policy,
};
pub use reaper::{HandLeaseReaper, HandLeaseReaperConfig, PostgresExpiredHandLeaseClaims};
pub use registration::{
    HandRoute, MCP_TOOL_REFERENCE_PREFIX, ToolExecution, ToolRegistry,
    governed_tool_contract_revision, mcp_tool_reference,
};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything one tool dispatch needs to know about the scope that asked for it.
///
/// The two tokens and the budget answer different questions and none of them
/// substitutes for the others. `cancel_token` is the session's cooperative
/// stop; `hard_cancel_token` is the harder stop that also kills work already
/// running inside a sandbox; `budget` is what the *run* may still spend,
/// which is a bound nobody downstream can otherwise see. A sandbox provisioned
/// with a two-hour lifetime happily runs a five-minute command for a turn whose
/// deadline passed thirty seconds ago, because the sandbox's clock and the
/// run's clock are unrelated — the budget is the only thing that connects them.
///
/// [`Copy`] on purpose: it is threaded through provisioning, dispatch, retry,
/// and executor layers, and no layer may widen or mutate what it was handed.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCallScope<'a> {
    /// Cooperative session cancellation, when the caller has one.
    pub cancel_token: Option<&'a CancellationToken>,
    /// Hard cancellation that also terminates in-sandbox work.
    pub hard_cancel_token: Option<&'a CancellationToken>,
    /// What the calling run may still spend on this dispatch.
    pub budget: ResourceBudget,
}

impl<'a> ToolCallScope<'a> {
    /// A scope that bounds nothing: no tokens and no budget.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            cancel_token: None,
            hard_cancel_token: None,
            budget: ResourceBudget::UNBOUNDED,
        }
    }

    /// Builds a scope from the cancellation tokens a caller already holds.
    #[must_use]
    pub const fn from_tokens(
        cancel_token: Option<&'a CancellationToken>,
        hard_cancel_token: Option<&'a CancellationToken>,
    ) -> Self {
        Self {
            cancel_token,
            hard_cancel_token,
            budget: ResourceBudget::UNBOUNDED,
        }
    }

    /// Returns this scope narrowed by `budget`.
    #[must_use]
    pub fn with_budget(self, budget: ResourceBudget) -> Self {
        Self {
            budget: self.budget.restrict(budget),
            ..self
        }
    }

    /// Returns the token in-sandbox work must observe, preferring the hard one.
    #[must_use]
    pub const fn effective_cancel_token(&self) -> Option<&'a CancellationToken> {
        match self.hard_cancel_token {
            Some(token) => Some(token),
            None => self.cancel_token,
        }
    }

    /// Returns whether either token has already been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token
            .is_some_and(CancellationToken::is_cancelled)
            || self
                .hard_cancel_token
                .is_some_and(CancellationToken::is_cancelled)
    }

    /// Pre-dispatch gate: may this scope start a tool call at `now`?
    ///
    /// Asked *before* policy preparation, sandbox provisioning, and provider
    /// dispatch, so a cancelled or expired scope executes exactly zero tools
    /// and provisions exactly zero sandboxes. Racing the execution future
    /// against the token in a `select!` cannot pin this: the execution branch is
    /// still polled, and a fast built-in or a cached sandbox can win the race.
    pub fn admit_at(&self, now: DateTime<Utc>) -> Result<()> {
        if self.is_cancelled() {
            return Err(MoaError::Cancelled);
        }
        if self
            .budget
            .remaining
            .is_some_and(|remaining| remaining.tool_calls == 0)
        {
            return Err(MoaError::BudgetExhausted(
                "tool dispatch refused: no tool calls remain".to_string(),
            ));
        }
        if self.budget.deadline_passed(now) {
            return Err(MoaError::BudgetExhausted(format!(
                "tool dispatch refused: run deadline {} has passed",
                self.budget
                    .deadline
                    .map_or_else(|| "<unset>".to_string(), |deadline| deadline.to_rfc3339())
            )));
        }
        Ok(())
    }

    /// Pre-dispatch gate against the current wall clock. See [`Self::admit_at`].
    pub fn admit(&self) -> Result<()> {
        self.admit_at(Utc::now())
    }

    /// Returns the wall-clock time this dispatch may still run for.
    ///
    /// `None` means the run states no deadline, never "no time left": an
    /// expired run yields `Some(Duration::ZERO)`, which executors refuse.
    #[must_use]
    pub fn run_deadline(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.budget.time_remaining(now)
    }
}

/// One immutable publication of the executable registry, routes, and metadata.
pub struct ToolCatalogSnapshot {
    owner_id: uuid::Uuid,
    registry: Arc<ToolRegistry>,
    tool_schemas: Arc<Vec<serde_json::Value>>,
    pin: std::result::Result<ToolCatalogPin, String>,
}

impl ToolCatalogSnapshot {
    fn new(owner_id: uuid::Uuid, registry: ToolRegistry) -> Self {
        let tool_schemas = Arc::new(registry.default_tool_schemas());
        let pin = ToolCatalogPin::from_registry(&registry).map_err(|error| error.to_string());
        Self {
            owner_id,
            registry: Arc::new(registry),
            tool_schemas,
            pin,
        }
    }

    /// Returns the precomputed pin for this exact immutable publication.
    pub fn pin(&self) -> Result<ToolCatalogPin> {
        self.pin
            .clone()
            .map_err(|error| MoaError::ConfigError(format!("pin tool catalog: {error}")))
    }

    /// Returns the shared model-facing schemas in authored loadout order.
    #[must_use]
    pub fn tool_schema_snapshot(&self) -> Arc<Vec<serde_json::Value>> {
        Arc::clone(&self.tool_schemas)
    }

    /// Returns one definition from this exact catalog publication.
    #[must_use]
    pub fn tool_definition(&self, name: &str) -> Option<moa_core::types::tools::ToolDefinition> {
        self.registry.get(name).cloned()
    }

    /// Returns typed capability registrations from this exact immutable publication.
    ///
    /// Scoped planning and release consumers must use this method rather than a
    /// fresh global router read so connector provenance cannot drift away from
    /// the schemas and pin they compile against.
    #[must_use]
    pub fn capability_registrations(
        &self,
    ) -> Vec<(moa_core::types::tools::ToolDefinition, ToolExecution)> {
        self.registry.capability_registrations()
    }

    /// Returns whether the named tool provisions a hand in this publication.
    #[must_use]
    pub fn tool_requires_sandbox(&self, name: &str) -> bool {
        self.registry.tool_requires_sandbox(name)
    }

    /// Returns one tool's canonical governed contract revision.
    #[must_use]
    pub fn contract_revision(&self, name: &str) -> Option<&str> {
        self.pin.as_ref().ok()?.contract_revision(name)
    }
}

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    /// Live executable registry and prompt schemas, published as one snapshot.
    ///
    /// Held as an immutable `Arc` behind a lock rather than as a mutable
    /// registry so a background catalog refresh publishes atomically: every
    /// reader takes one snapshot and works from it, and no prompt compilation,
    /// capability listing, or dispatch can observe a half-refreshed connector.
    catalog: std::sync::RwLock<Arc<ToolCatalogSnapshot>>,
    /// Runtime identity that prevents snapshots crossing router instances.
    catalog_owner_id: uuid::Uuid,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    /// Last observed discovery outcome for every configured connector.
    ///
    /// This is the typed health the acceptance criteria require: an optional
    /// connector that is down is `Unavailable` (or `Degraded` while its
    /// last-known-good tools stay served) and every other connector's tools are
    /// unaffected, while a required connector that is down never reaches this
    /// map because startup fails with the same typed value.
    mcp_health: RwLock<std::collections::BTreeMap<String, McpConnectorHealth>>,
    mcp_credentials: McpDeploymentCredentials,
    /// Optional data-class egress guard for outbound MCP tool calls. When
    /// present, each call's serialized arguments are classified against the
    /// destination server's `allowed_data_classes` allowlist and blocked (fail
    /// closed) before dispatch when the payload carries a class the server is not
    /// permitted to receive. Absence is valid only when no MCP servers are
    /// configured: configured construction rejects it, and manually assembled
    /// routers fail closed at dispatch. The guard is held here rather than on
    mcp_egress_guard: Option<Arc<McpEgressGuard>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    preferred_hand_routes: RwLock<HashMap<String, String>>,
    hand_leases: Option<Arc<dyn HandLeaseStore>>,
    /// Deployment-level sandbox policy layer, injected at construction so
    /// provisioning can never substitute a default for the outermost layer.
    deployment_sandbox_policy: SandboxPolicySnapshot,
    /// Durable owner of each tenant's authored sandbox policy layer, read on
    /// every provisioning decision. `None` means no tenant has authored one,
    /// which contributes the named identity layer rather than an absent one.
    tenant_sandbox_policy: Option<Arc<dyn TenantSandboxPolicyStore>>,
    /// Whether the durable hand-lease reaper is running for this deployment.
    ///
    /// A provider that relies on the reaper to enforce a deadline may only
    /// serve a bounded deadline when this is true; otherwise admission refuses
    /// rather than provisioning a sandbox nothing will ever destroy.
    hand_lease_reaper_installed: bool,
    /// Trusted sandbox file manifests keyed by hand scope (`scope_key`):
    /// `"{session_id}:{worker_id}"`, where an empty worker segment is the
    /// session-level (coordinator) scope.
    trusted_sandbox_files: RwLock<HashMap<String, Vec<SandboxFile>>>,
    installed_files: RwLock<HashMap<String, Vec<SandboxFile>>>,
    workspace_roots: RwLock<HashMap<TenantId, PathBuf>>,
    policies: ActionPolicies,
    /// Provenance class of the runtime this router serves.
    ///
    /// Stated at construction as a deployment-level ceiling and composed with
    /// each session's durable origin. No tenant rule, deployment default, or
    /// tool effect can hand back a capability either origin refuses.
    call_origin: CallOrigin,
    /// Configured permission patterns that govern no registered tool.
    ///
    /// Recomputed whenever the tool catalog changes rather than once at startup,
    /// so a lazily discovered connector clears a warning that was legitimately
    /// true before its tools existed.
    unmatched_permission_patterns: std::sync::RwLock<Vec<UnmatchedPermissionPattern>>,
    rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    memory_tool_executor: RwLock<Option<Arc<dyn MemoryToolExecutor>>>,
    memory_retrieval_executor: RwLock<Option<Arc<dyn MemoryRetrievalExecutor>>>,
    lineage: Arc<dyn moa_core::traits::LineageHandle>,
    sandbox_root: Option<PathBuf>,
    tool_output: ToolOutputConfig,
    tool_budgets: ToolBudgetConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::resource::ResourceAmounts;

    #[test]
    fn bounded_scope_refuses_a_zero_tool_call_allowance_offline() {
        // Pins: receiving a target resource slice is an enforcement boundary,
        // not only a deadline hint passed through to the sandbox.
        let scope = ToolCallScope::unbounded().with_budget(ResourceBudget::new(
            None,
            Some(ResourceAmounts {
                cost_micro_usd: 1,
                tokens: 1,
                turns: 1,
                model_calls: 1,
                tool_calls: 0,
            }),
        ));

        let error = scope
            .admit_at(Utc::now())
            .expect_err("zero tool-call capacity must fail before dispatch");
        assert!(
            matches!(error, MoaError::BudgetExhausted(message) if message.contains("no tool calls"))
        );
    }
}

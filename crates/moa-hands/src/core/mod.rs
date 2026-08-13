//! Tool routing core, registry, policy, dispatch, and recovery for MOA hands.

mod construction;
mod dispatch;
pub mod leases;
mod lifecycle;
mod maintenance_provider_inventory;
pub mod mcp_catalog;
mod normalization;
mod output_budget;
mod policy;
pub mod profile;
pub mod provider_credentials;
pub mod reaper;
mod recovery;
mod registration;
pub mod sandbox_workspace;
pub mod telemetry;

pub use lifecycle::SessionHandReleasePageOutcome;

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_config::McpServerConfig;
use moa_config::ToolBudgetConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider, traits::MemoryRetrievalExecutor,
    traits::MemoryToolExecutor, traits::NullLineageHandle, traits::SandboxStorageProvider,
    traits::SessionStore, types::action_policy::CallOrigin, types::hands::HandHandle,
    types::hands::SandboxFile, types::hands::SandboxPolicySnapshot, types::identifiers::SessionId,
    types::identifiers::TenantId, types::resource::ResourceBudget,
};
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, McpDeploymentCredentials, McpEgressGuard,
    UnmatchedPermissionPattern,
};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::adapters::local::LocalHandProvider;

pub use dispatch::{
    AuthorizedToolCall, DeferredWorkspaceToolOutput, ExecutionHandReleaseRequest,
    JournaledWorkspaceCommit, PendingConnectorToolOutput,
};
use leases::{HAND_LEASE_SESSION_PAGE_SIZE, HandLeaseStore};
pub use maintenance_provider_inventory::SandboxProviderInventory;
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
pub use provider_credentials::{
    FileProviderCredentialSource, ProviderCredentialSource, ProviderEndpoint, ProviderHttpAttempt,
    ProviderSandboxAttempt,
};
pub use reaper::{HandLeaseReaper, HandLeaseReaperConfig, PostgresExpiredHandLeaseClaims};
pub use registration::{
    HandRoute, MCP_TOOL_REFERENCE_PREFIX, ToolExecution, ToolRegistry,
    governed_tool_contract_revision, installed_connector_tool_name, mcp_tool_reference,
};
use sandbox_workspace::capacity::PostgresWorkspaceCapacityRepository;
use sandbox_workspace::operations::PostgresWorkspaceOperationRepository;
use sandbox_workspace::repository::PostgresWorkspaceRepository;
pub use telemetry::truncate_tool_span_text;

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = crate::tools::bash::DEFAULT_BASH_TIMEOUT;

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

    /// Returns the declared durable-workspace effect for one sandbox tool.
    #[must_use]
    pub fn tool_workspace_effect(
        &self,
        name: &str,
    ) -> Option<moa_core::types::sandbox_workspace::WorkspaceEffect> {
        self.registry.workspace_effect(name)
    }

    /// Returns one tool's canonical governed contract revision.
    #[must_use]
    pub fn contract_revision(&self, name: &str) -> Option<&str> {
        self.pin.as_ref().ok()?.contract_revision(name)
    }
}

/// Owns the atomically published catalog for one router instance.
///
/// The owner ID and its immutable snapshots share the router lifetime. The
/// synchronous read/write lock protects only the `Arc` publication boundary:
/// readers clone one complete snapshot and publishers replace one complete
/// snapshot. No catalog lock may be held while a provider, tool, or network
/// future is awaited.
struct CatalogOwner {
    owner_id: uuid::Uuid,
    snapshot: std::sync::RwLock<Arc<ToolCatalogSnapshot>>,
}

impl CatalogOwner {
    /// Creates the first immutable catalog publication for a router.
    fn new(registry: ToolRegistry) -> Self {
        let owner_id = uuid::Uuid::now_v7();
        Self {
            owner_id,
            snapshot: std::sync::RwLock::new(Arc::new(ToolCatalogSnapshot::new(
                owner_id, registry,
            ))),
        }
    }

    /// Returns the identity that all snapshots owned by this router carry.
    fn owner_id(&self) -> uuid::Uuid {
        self.owner_id
    }

    /// Clones the currently activated immutable catalog.
    fn activated(&self) -> Arc<ToolCatalogSnapshot> {
        Arc::clone(
            &self
                .snapshot
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Replaces the active publication under one short lock acquisition.
    fn publish(&self, snapshot: ToolCatalogSnapshot) {
        *self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(snapshot);
    }
}

/// Owns deployment MCP configuration and connector health for one router.
///
/// Server definitions, deployment credentials, and the egress guard are
/// immutable after construction except for the synchronous loading phase.
/// Health has one async lock because refreshes publish observations after
/// network discovery. The lock is cloned into a local value before discovery
/// and reacquired only for the final health publication; per-route transport
/// single-flight state remains in the immutable catalog route itself.
struct McpOwner {
    servers: HashMap<String, McpServerConfig>,
    health: RwLock<BTreeMap<String, McpConnectorHealth>>,
    credentials: McpDeploymentCredentials,
    egress_guard: Option<Arc<McpEgressGuard>>,
    catalog_fresh_until: std::sync::RwLock<Option<Instant>>,
    refresh_requested: AtomicBool,
    refresh_notify: Notify,
}

impl Default for McpOwner {
    fn default() -> Self {
        Self {
            servers: HashMap::new(),
            health: RwLock::new(BTreeMap::new()),
            credentials: McpDeploymentCredentials::default(),
            egress_guard: None,
            catalog_fresh_until: std::sync::RwLock::new(None),
            refresh_requested: AtomicBool::new(false),
            refresh_notify: Notify::new(),
        }
    }
}

impl McpOwner {
    /// Loads server definitions and their deployment credentials before serving.
    fn configure(&mut self, servers: &[McpServerConfig]) -> Result<()> {
        self.credentials = McpDeploymentCredentials::from_mcp_servers(servers)?;
        self.servers.extend(
            servers
                .iter()
                .cloned()
                .map(|server| (server.name.clone(), server)),
        );
        Ok(())
    }

    /// Installs the host-side egress guard used by outbound MCP calls.
    fn set_egress_guard(&mut self, guard: Option<Arc<McpEgressGuard>>) {
        self.egress_guard = guard;
    }

    /// Returns an owned server configuration safe to retain across an await.
    fn server(&self, name: &str) -> Option<McpServerConfig> {
        self.servers.get(name).cloned()
    }

    /// Returns credentials-derived headers for one configured server.
    fn headers_for(&self, server: &McpServerConfig) -> Result<HashMap<String, String>> {
        self.credentials.headers_for(server)
    }

    /// Returns the configured egress guard without exposing owner internals.
    fn egress_guard(&self) -> Option<Arc<McpEgressGuard>> {
        self.egress_guard.clone()
    }

    /// Returns a stable-name-ordered copy of configured servers.
    fn configured_servers(&self) -> Vec<McpServerConfig> {
        let mut servers = self.servers.values().cloned().collect::<Vec<_>>();
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        servers
    }

    /// Publishes the earliest expiry of every currently served MCP catalog entry.
    fn publish_catalog_fresh_until(&self, fresh_until: Option<Instant>) {
        *self
            .catalog_fresh_until
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh_until;
    }

    /// Coalesces stale-catalog accesses into one wake-up for the refresh worker.
    fn request_refresh_if_stale(&self) {
        let fresh_until = *self
            .catalog_fresh_until
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !fresh_until.is_some_and(|deadline| Instant::now() >= deadline) {
            return;
        }
        if self
            .refresh_requested
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.refresh_notify.notify_one();
        }
    }

    /// Waits until a stale catalog access requests an early refresh.
    async fn wait_for_refresh_request(&self) {
        self.refresh_notify.notified().await;
    }

    /// Allows a later stale access to request another refresh attempt.
    fn complete_refresh_request(&self) {
        self.refresh_requested.store(false, Ordering::Release);
    }

    /// Clones the last observed connector health without retaining its lock.
    async fn health_snapshot(&self) -> BTreeMap<String, McpConnectorHealth> {
        self.health.read().await.clone()
    }

    /// Publishes connector health after discovery has completed.
    async fn publish_health(&self, health: BTreeMap<String, McpConnectorHealth>) {
        *self.health.write().await = health;
    }
}

/// Owns hand providers and process-local lifecycle caches for one router.
///
/// Provider registrations and policy configuration live for the router
/// lifetime. Active handles, preferred routes, trusted/installed manifests,
/// and workspace roots use independent async locks and are only caches or
/// transport setup state. Durable lease operations remain the correctness
/// authority in Postgres. Lifecycle code must clone or remove cache values
/// before awaiting provider or lease-store work; no lifecycle lock spans an
/// external await.
struct HandLifecycleOwner {
    providers: HashMap<String, Arc<dyn HandProvider>>,
    storage_providers: HashMap<String, Arc<dyn SandboxStorageProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    active_hands: RwLock<BTreeMap<HandProviderCacheKey, ActiveHand>>,
    preferred_hand_routes: RwLock<BTreeMap<HandScopeKey, String>>,
    hand_leases: Option<Arc<dyn HandLeaseStore>>,
    workspace_repository: Option<Arc<PostgresWorkspaceRepository>>,
    workspace_operations: Option<Arc<PostgresWorkspaceOperationRepository>>,
    workspace_capacity: Option<Arc<PostgresWorkspaceCapacityRepository>>,
    checkpoint_store: Option<Arc<sandbox_workspace::checkpoint::store::CheckpointObjectStore>>,
    deployment_sandbox_policy: SandboxPolicySnapshot,
    tenant_sandbox_policy: Option<Arc<dyn TenantSandboxPolicyStore>>,
    hand_lease_reaper_installed: bool,
    trusted_sandbox_files: RwLock<BTreeMap<HandScopeKey, Arc<TrustedSandboxManifest>>>,
    installed_files: RwLock<BTreeMap<HandScopeKey, BTreeMap<String, InstalledManifestMarker>>>,
    workspace_roots: RwLock<HashMap<TenantId, PathBuf>>,
    sandbox_root: Option<PathBuf>,
}

/// One process-local active binding, including its durable generation when present.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveHand {
    handle: HandHandle,
    generation: Option<i64>,
}

/// Exact conversational scope used by trusted and installed manifest caches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HandScopeKey {
    tenant_id: TenantId,
    session_id: SessionId,
    worker_id: String,
}

impl HandScopeKey {
    fn new(tenant_id: TenantId, session_id: SessionId, worker_id: impl Into<String>) -> Self {
        Self {
            tenant_id,
            session_id,
            worker_id: worker_id.into(),
        }
    }
}

impl PartialOrd for HandScopeKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for HandScopeKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (self.tenant_id.0, self.session_id.0, &self.worker_id).cmp(&(
            other.tenant_id.0,
            other.session_id.0,
            &other.worker_id,
        ))
    }
}

/// Exact process-local cache key for one provider binding under a typed owner.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HandProviderCacheKey {
    scope: HandScopeKey,
    provider: String,
}

impl HandProviderCacheKey {
    fn new(scope: HandScopeKey, provider: impl Into<String>) -> Self {
        Self {
            scope,
            provider: provider.into(),
        }
    }
}

impl PartialOrd for HandProviderCacheKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for HandProviderCacheKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (&self.scope, &self.provider).cmp(&(&other.scope, &other.provider))
    }
}

/// Cursor through the bounded process-local cache cleanup phases for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionCacheDrainCursor {
    PreferredRoutes(Option<HandScopeKey>),
    TrustedSandboxFiles(Option<HandScopeKey>),
    InstalledFiles(Option<HandScopeKey>),
    ActiveHands(Option<HandProviderCacheKey>),
    Complete,
}

impl SessionCacheDrainCursor {
    fn start() -> Self {
        Self::PreferredRoutes(None)
    }
}

/// One active process-local hand removed for provider teardown.
#[derive(Debug)]
struct CachedActiveHandRelease {
    key: HandProviderCacheKey,
    provider: String,
    hand: ActiveHand,
}

/// One bounded process-local session-cache cleanup page.
#[derive(Debug)]
struct SessionCacheDrainPage {
    removed_entries: usize,
    active_hands: Vec<CachedActiveHandRelease>,
    next_cursor: SessionCacheDrainCursor,
}

fn validate_scope_cache_cursor(
    tenant_id: TenantId,
    session_id: SessionId,
    cursor: Option<&HandScopeKey>,
) -> Result<()> {
    if cursor.is_some_and(|cursor| cursor.tenant_id != tenant_id || cursor.session_id != session_id)
    {
        return Err(MoaError::ValidationError(
            "session cache cursor does not belong to the requested tenant and session".to_string(),
        ));
    }
    Ok(())
}

fn validate_provider_cache_cursor(
    tenant_id: TenantId,
    session_id: SessionId,
    cursor: Option<&HandProviderCacheKey>,
) -> Result<()> {
    validate_scope_cache_cursor(tenant_id, session_id, cursor.map(|cursor| &cursor.scope))
}

fn session_scope_cache_keys<V>(
    entries: &BTreeMap<HandScopeKey, V>,
    tenant_id: TenantId,
    session_id: SessionId,
    after: Option<&HandScopeKey>,
) -> Vec<HandScopeKey> {
    let lower = after
        .cloned()
        .unwrap_or_else(|| HandScopeKey::new(tenant_id, session_id, String::new()));
    let lower_bound = if after.is_some() {
        Bound::Excluded(lower)
    } else {
        Bound::Included(lower)
    };
    entries
        .range((lower_bound, Bound::Unbounded))
        .take_while(|(key, _)| key.tenant_id == tenant_id && key.session_id == session_id)
        .take(HAND_LEASE_SESSION_PAGE_SIZE + 1)
        .map(|(key, _)| key.clone())
        .collect()
}

fn session_provider_cache_keys<V>(
    entries: &BTreeMap<HandProviderCacheKey, V>,
    tenant_id: TenantId,
    session_id: SessionId,
    after: Option<&HandProviderCacheKey>,
) -> Vec<HandProviderCacheKey> {
    let lower = after.cloned().unwrap_or_else(|| {
        HandProviderCacheKey::new(
            HandScopeKey::new(tenant_id, session_id, String::new()),
            String::new(),
        )
    });
    let lower_bound = if after.is_some() {
        Bound::Excluded(lower)
    } else {
        Bound::Included(lower)
    };
    entries
        .range((lower_bound, Bound::Unbounded))
        .take_while(|(key, _)| {
            key.scope.tenant_id == tenant_id && key.scope.session_id == session_id
        })
        .take(HAND_LEASE_SESSION_PAGE_SIZE + 1)
        .map(|(key, _)| key.clone())
        .collect()
}

/// One immutable trusted-file publication shared cheaply across dispatches.
#[derive(Debug)]
struct TrustedSandboxManifest {
    identity: uuid::Uuid,
    files: Arc<[SandboxFile]>,
}

/// Proof that one immutable manifest was installed on one exact active binding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstalledManifestMarker {
    manifest_identity: uuid::Uuid,
    handle: HandHandle,
    generation: Option<i64>,
}

impl HandLifecycleOwner {
    /// Creates the provider and cache domain for one router.
    fn new(
        providers: HashMap<String, Arc<dyn HandProvider>>,
        deployment_sandbox_policy: SandboxPolicySnapshot,
    ) -> Self {
        Self {
            providers,
            storage_providers: HashMap::new(),
            local_provider: None,
            active_hands: RwLock::new(BTreeMap::new()),
            preferred_hand_routes: RwLock::new(BTreeMap::new()),
            hand_leases: None,
            workspace_repository: None,
            workspace_operations: None,
            workspace_capacity: None,
            checkpoint_store: None,
            deployment_sandbox_policy,
            tenant_sandbox_policy: None,
            hand_lease_reaper_installed: false,
            trusted_sandbox_files: RwLock::new(BTreeMap::new()),
            installed_files: RwLock::new(BTreeMap::new()),
            workspace_roots: RwLock::new(HashMap::new()),
            sandbox_root: None,
        }
    }

    /// Returns the registered providers in stable name order for the reaper.
    fn providers(&self) -> Vec<Arc<dyn HandProvider>> {
        let mut names = self.providers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .filter_map(|name| self.providers.get(&name).cloned())
            .collect()
    }

    /// Removes one bounded, session-indexed page of process-local lifecycle state.
    ///
    /// Each call advances exactly one cache family. Active bindings are returned
    /// to the caller for provider teardown; the other cache families carry no
    /// external resources. Supplying a cursor for another tenant or session is
    /// rejected before any entry is removed.
    async fn drain_session_cache_page(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        cursor: SessionCacheDrainCursor,
    ) -> Result<SessionCacheDrainPage> {
        match cursor {
            SessionCacheDrainCursor::PreferredRoutes(after) => {
                validate_scope_cache_cursor(tenant_id, session_id, after.as_ref())?;
                let mut routes = self.preferred_hand_routes.write().await;
                let mut keys =
                    session_scope_cache_keys(&routes, tenant_id, session_id, after.as_ref());
                let has_more = keys.len() > HAND_LEASE_SESSION_PAGE_SIZE;
                keys.truncate(HAND_LEASE_SESSION_PAGE_SIZE);
                for key in &keys {
                    routes.remove(key);
                }
                let next_cursor = if has_more {
                    SessionCacheDrainCursor::PreferredRoutes(keys.last().cloned())
                } else {
                    SessionCacheDrainCursor::TrustedSandboxFiles(None)
                };
                Ok(SessionCacheDrainPage {
                    removed_entries: keys.len(),
                    active_hands: Vec::new(),
                    next_cursor,
                })
            }
            SessionCacheDrainCursor::TrustedSandboxFiles(after) => {
                validate_scope_cache_cursor(tenant_id, session_id, after.as_ref())?;
                let mut manifests = self.trusted_sandbox_files.write().await;
                let mut keys =
                    session_scope_cache_keys(&manifests, tenant_id, session_id, after.as_ref());
                let has_more = keys.len() > HAND_LEASE_SESSION_PAGE_SIZE;
                keys.truncate(HAND_LEASE_SESSION_PAGE_SIZE);
                for key in &keys {
                    manifests.remove(key);
                }
                let next_cursor = if has_more {
                    SessionCacheDrainCursor::TrustedSandboxFiles(keys.last().cloned())
                } else {
                    SessionCacheDrainCursor::InstalledFiles(None)
                };
                Ok(SessionCacheDrainPage {
                    removed_entries: keys.len(),
                    active_hands: Vec::new(),
                    next_cursor,
                })
            }
            SessionCacheDrainCursor::InstalledFiles(after) => {
                validate_scope_cache_cursor(tenant_id, session_id, after.as_ref())?;
                let mut installed = self.installed_files.write().await;
                let mut keys =
                    session_scope_cache_keys(&installed, tenant_id, session_id, after.as_ref());
                let has_more = keys.len() > HAND_LEASE_SESSION_PAGE_SIZE;
                keys.truncate(HAND_LEASE_SESSION_PAGE_SIZE);
                for key in &keys {
                    installed.remove(key);
                }
                let next_cursor = if has_more {
                    SessionCacheDrainCursor::InstalledFiles(keys.last().cloned())
                } else {
                    SessionCacheDrainCursor::ActiveHands(None)
                };
                Ok(SessionCacheDrainPage {
                    removed_entries: keys.len(),
                    active_hands: Vec::new(),
                    next_cursor,
                })
            }
            SessionCacheDrainCursor::ActiveHands(after) => {
                validate_provider_cache_cursor(tenant_id, session_id, after.as_ref())?;
                let mut hands = self.active_hands.write().await;
                let mut keys =
                    session_provider_cache_keys(&hands, tenant_id, session_id, after.as_ref());
                let has_more = keys.len() > HAND_LEASE_SESSION_PAGE_SIZE;
                keys.truncate(HAND_LEASE_SESSION_PAGE_SIZE);
                let mut active_hands = Vec::with_capacity(keys.len());
                for key in &keys {
                    if let Some(hand) = hands.remove(key) {
                        active_hands.push(CachedActiveHandRelease {
                            key: key.clone(),
                            provider: key.provider.clone(),
                            hand,
                        });
                    }
                }
                let next_cursor = if has_more {
                    SessionCacheDrainCursor::ActiveHands(keys.last().cloned())
                } else {
                    SessionCacheDrainCursor::Complete
                };
                Ok(SessionCacheDrainPage {
                    removed_entries: active_hands.len(),
                    active_hands,
                    next_cursor,
                })
            }
            SessionCacheDrainCursor::Complete => Ok(SessionCacheDrainPage {
                removed_entries: 0,
                active_hands: Vec::new(),
                next_cursor: SessionCacheDrainCursor::Complete,
            }),
        }
    }

    /// Clears every installed-manifest marker for one exact typed owner.
    async fn clear_installed_files_for_scope(&self, scope: &HandScopeKey) {
        self.installed_files.write().await.remove(scope);
    }

    /// Installs the provider-neutral persistent-workspace adapters.
    fn set_storage_providers(
        &mut self,
        storage_providers: HashMap<String, Arc<dyn SandboxStorageProvider>>,
    ) {
        self.storage_providers = storage_providers;
    }

    /// Installs the optional local provider used by local file setup paths.
    fn set_local_provider(&mut self, provider: Option<Arc<LocalHandProvider>>) {
        self.local_provider = provider;
    }

    /// Remembers the local workspace root used by local hand provisioning.
    fn set_sandbox_root(&mut self, sandbox_root: Option<PathBuf>) {
        self.sandbox_root = sandbox_root;
    }

    /// Attaches the durable tenant policy owner.
    fn set_tenant_sandbox_policy(&mut self, store: Arc<dyn TenantSandboxPolicyStore>) {
        self.tenant_sandbox_policy = Some(store);
    }

    /// Attaches the durable lease owner.
    fn set_hand_lease_store(&mut self, store: Arc<dyn HandLeaseStore>) {
        self.hand_leases = Some(store);
    }

    /// Attaches the durable workspace repositories used by tool dispatch.
    fn set_workspace_repositories(
        &mut self,
        workspaces: Arc<PostgresWorkspaceRepository>,
        operations: Arc<PostgresWorkspaceOperationRepository>,
        capacity: Arc<PostgresWorkspaceCapacityRepository>,
    ) {
        self.workspace_repository = Some(workspaces);
        self.workspace_operations = Some(operations);
        self.workspace_capacity = Some(capacity);
    }

    /// Records that the deployment starts its durable lease reaper.
    fn set_hand_lease_reaper_installed(&mut self) {
        self.hand_lease_reaper_installed = true;
    }

    /// Returns whether durable lease ownership is configured.
    fn has_hand_lease_store(&self) -> bool {
        self.hand_leases.is_some()
    }

    /// Returns whether the durable reaper owns deadline enforcement.
    fn hand_lease_reaper_installed(&self) -> bool {
        self.hand_lease_reaper_installed
    }

    /// Returns the local provider, when this deployment registered one.
    fn local_provider(&self) -> Option<Arc<LocalHandProvider>> {
        self.local_provider.clone()
    }

    /// Returns the deployment policy layer.
    fn deployment_policy(&self) -> &SandboxPolicySnapshot {
        &self.deployment_sandbox_policy
    }

    /// Returns the local workspace fallback, if configured.
    fn sandbox_root(&self) -> Option<PathBuf> {
        self.sandbox_root.clone()
    }
}

/// Owns built-in execution bindings and action-policy state for one router.
///
/// These bindings are immutable after composition except for the optional
/// retrieval executor and unmatched-pattern snapshot, each of which has its
/// own short async/synchronous lock. Accessors clone handles before a built-in,
/// session-store, or policy-rule await, so no binding lock crosses external
/// work. The owner also keeps output and tool budgets beside the bindings that
/// govern built-in execution and catalog derivation.
struct BuiltInBindings {
    policies: ActionPolicies,
    call_origin: CallOrigin,
    unmatched_permission_patterns: std::sync::RwLock<Vec<UnmatchedPermissionPattern>>,
    rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    memory_tool_executor: Option<Arc<dyn MemoryToolExecutor>>,
    memory_retrieval_executor: RwLock<Option<Arc<dyn MemoryRetrievalExecutor>>>,
    lineage: Arc<dyn moa_core::traits::LineageHandle>,
    tool_output: ToolOutputConfig,
    tool_budgets: ToolBudgetConfig,
}

impl Default for BuiltInBindings {
    fn default() -> Self {
        Self {
            policies: ActionPolicies::default(),
            call_origin: CallOrigin::Production,
            unmatched_permission_patterns: std::sync::RwLock::new(Vec::new()),
            rule_store: None,
            session_store: None,
            memory_tool_executor: None,
            memory_retrieval_executor: RwLock::new(None),
            lineage: Arc::new(NullLineageHandle),
            tool_output: ToolOutputConfig::default(),
            tool_budgets: ToolBudgetConfig::default(),
        }
    }
}

impl BuiltInBindings {
    /// Replaces the action-policy evaluator during router composition.
    fn set_policies(&mut self, policies: ActionPolicies) {
        self.policies = policies;
    }

    /// Replaces the deployment-level call-origin ceiling.
    fn set_call_origin(&mut self, call_origin: CallOrigin) {
        self.call_origin = call_origin;
    }

    /// Returns the deployment-level call-origin ceiling.
    fn call_origin(&self) -> CallOrigin {
        self.call_origin
    }

    /// Installs the durable policy-rule owner.
    fn set_rule_store(&mut self, store: Option<Arc<dyn ActionPolicyRuleStore>>) {
        self.rule_store = store;
    }

    /// Installs the session store used by built-ins and output artifacts.
    fn set_session_store(&mut self, store: Option<Arc<dyn SessionStore>>) {
        self.session_store = store;
    }

    /// Installs the graph-memory write executor.
    fn set_memory_tool_executor(&mut self, executor: Option<Arc<dyn MemoryToolExecutor>>) {
        self.memory_tool_executor = executor;
    }

    /// Returns the graph-memory write executor handle.
    fn memory_tool_executor(&self) -> Option<Arc<dyn MemoryToolExecutor>> {
        self.memory_tool_executor.clone()
    }

    /// Installs the read-only graph-memory retrieval executor.
    fn set_memory_retrieval_executor(
        &mut self,
        executor: Option<Arc<dyn MemoryRetrievalExecutor>>,
    ) {
        self.memory_retrieval_executor = RwLock::new(executor);
    }

    /// Replaces the read-only graph-memory retrieval executor asynchronously.
    async fn replace_memory_retrieval_executor(&self, executor: Arc<dyn MemoryRetrievalExecutor>) {
        *self.memory_retrieval_executor.write().await = Some(executor);
    }

    /// Returns the configured session store handle.
    fn session_store(&self) -> Option<Arc<dyn SessionStore>> {
        self.session_store.clone()
    }

    /// Returns the configured policy-rule store handle.
    fn rule_store(&self) -> Option<Arc<dyn ActionPolicyRuleStore>> {
        self.rule_store.clone()
    }

    /// Returns the lineage handle shared by built-in tools.
    fn lineage(&self) -> Arc<dyn moa_core::traits::LineageHandle> {
        Arc::clone(&self.lineage)
    }

    /// Replaces the output shaping configuration.
    fn set_tool_output(&mut self, tool_output: ToolOutputConfig) {
        self.tool_output = tool_output;
    }

    /// Returns the output shaping configuration.
    fn tool_output(&self) -> &ToolOutputConfig {
        &self.tool_output
    }

    /// Replaces the per-tool budget configuration.
    fn set_tool_budgets(&mut self, tool_budgets: ToolBudgetConfig) {
        self.tool_budgets = tool_budgets;
    }

    /// Returns the per-tool budget configuration.
    fn tool_budgets(&self) -> &ToolBudgetConfig {
        &self.tool_budgets
    }

    /// Replaces the current unmatched permission-pattern snapshot.
    fn set_unmatched_permission_patterns(&self, patterns: Vec<UnmatchedPermissionPattern>) {
        *self
            .unmatched_permission_patterns
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = patterns;
    }

    /// Returns a copy of the current unmatched permission-pattern snapshot.
    fn unmatched_permission_patterns(&self) -> Vec<UnmatchedPermissionPattern> {
        self.unmatched_permission_patterns
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    catalog: CatalogOwner,
    mcp: McpOwner,
    hands: HandLifecycleOwner,
    bindings: BuiltInBindings,
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

    #[tokio::test]
    async fn expired_catalog_access_coalesces_and_rearms_refresh_requests_offline() {
        // Pins: ttlMs=0 makes the next catalog access wake the refresh worker,
        // while concurrent readers produce only one early-refresh request.
        let owner = McpOwner::default();
        owner.publish_catalog_fresh_until(Some(Instant::now()));

        owner.request_refresh_if_stale();
        owner.request_refresh_if_stale();
        tokio::time::timeout(Duration::from_millis(50), owner.wait_for_refresh_request())
            .await
            .expect("an expired catalog should wake the refresh worker");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), owner.wait_for_refresh_request())
                .await
                .is_err(),
            "stale readers must coalesce behind one refresh"
        );

        owner.complete_refresh_request();
        owner.request_refresh_if_stale();
        tokio::time::timeout(Duration::from_millis(50), owner.wait_for_refresh_request())
            .await
            .expect("a failed or immediately stale refresh must be retryable");
    }

    #[tokio::test]
    async fn session_cache_drain_is_bounded_and_tenant_session_indexed_offline() {
        // Pins: terminal cleanup with more than one cache page removes only the
        // exact tenant/session through ordered ranges and returns every active
        // binding for teardown without a global retain or unbounded collect.
        let owner = HandLifecycleOwner::new(
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        let tenant_id = TenantId::new();
        let unrelated_tenant_id = TenantId::new();
        let session_id = SessionId::new();
        let count = HAND_LEASE_SESSION_PAGE_SIZE + 7;

        for index in 0..count {
            let scope = HandScopeKey::new(tenant_id, session_id, format!("owner-{index:03}"));
            let provider_key = HandProviderCacheKey::new(scope.clone(), "local");
            owner
                .preferred_hand_routes
                .write()
                .await
                .insert(scope.clone(), "local".to_string());
            owner.trusted_sandbox_files.write().await.insert(
                scope,
                Arc::new(TrustedSandboxManifest {
                    identity: uuid::Uuid::new_v4(),
                    files: Vec::<SandboxFile>::new().into(),
                }),
            );
            owner
                .installed_files
                .write()
                .await
                .entry(provider_key.scope.clone())
                .or_default()
                .insert(
                    "local".to_string(),
                    InstalledManifestMarker {
                        manifest_identity: uuid::Uuid::new_v4(),
                        handle: HandHandle::local(PathBuf::from(format!("/tmp/installed-{index}"))),
                        generation: Some(1),
                    },
                );
            owner.active_hands.write().await.insert(
                provider_key,
                ActiveHand {
                    handle: HandHandle::local(PathBuf::from(format!("/tmp/active-{index}"))),
                    generation: Some(1),
                },
            );
        }

        let unrelated_scope = HandScopeKey::new(unrelated_tenant_id, session_id, "unrelated-owner");
        let unrelated_provider_key = HandProviderCacheKey::new(unrelated_scope.clone(), "local");
        owner
            .preferred_hand_routes
            .write()
            .await
            .insert(unrelated_scope.clone(), "local".to_string());
        owner.trusted_sandbox_files.write().await.insert(
            unrelated_scope,
            Arc::new(TrustedSandboxManifest {
                identity: uuid::Uuid::new_v4(),
                files: Vec::<SandboxFile>::new().into(),
            }),
        );
        owner
            .installed_files
            .write()
            .await
            .entry(unrelated_provider_key.scope.clone())
            .or_default()
            .insert(
                "local".to_string(),
                InstalledManifestMarker {
                    manifest_identity: uuid::Uuid::new_v4(),
                    handle: HandHandle::local(PathBuf::from("/tmp/unrelated-installed")),
                    generation: Some(1),
                },
            );
        owner.active_hands.write().await.insert(
            unrelated_provider_key,
            ActiveHand {
                handle: HandHandle::local(PathBuf::from("/tmp/unrelated-active")),
                generation: Some(1),
            },
        );

        let first = owner
            .drain_session_cache_page(tenant_id, session_id, SessionCacheDrainCursor::start())
            .await
            .expect("drain first preferred-route page");
        assert_eq!(first.removed_entries, HAND_LEASE_SESSION_PAGE_SIZE);
        assert!(matches!(
            first.next_cursor,
            SessionCacheDrainCursor::PreferredRoutes(Some(_))
        ));

        let mut cursor = first.next_cursor;
        let mut removed = first.removed_entries;
        let mut released = first.active_hands.len();
        let mut calls = 1;
        while cursor != SessionCacheDrainCursor::Complete {
            let page = owner
                .drain_session_cache_page(tenant_id, session_id, cursor)
                .await
                .expect("drain next exact-session cache page");
            assert!(page.removed_entries <= HAND_LEASE_SESSION_PAGE_SIZE);
            removed += page.removed_entries;
            released += page.active_hands.len();
            calls += 1;
            assert!(calls <= 12, "the four two-page phases must terminate");
            cursor = page.next_cursor;
        }

        assert_eq!(removed, count * 4);
        assert_eq!(released, count);
        assert_eq!(owner.preferred_hand_routes.read().await.len(), 1);
        assert_eq!(owner.trusted_sandbox_files.read().await.len(), 1);
        assert_eq!(owner.installed_files.read().await.len(), 1);
        assert_eq!(owner.active_hands.read().await.len(), 1);
    }

    #[tokio::test]
    async fn installed_manifest_scope_removal_is_one_logical_owner_operation_offline() {
        // Pins: an exact owner with more providers than one cleanup page is
        // invalidated by removing its nested scope entry, without a provider-key
        // loop, while another tenant's identically shaped scope remains intact.
        let owner = HandLifecycleOwner::new(
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        let session_id = SessionId::new();
        let scope = HandScopeKey::new(TenantId::new(), session_id, "owner");
        let unrelated_scope = HandScopeKey::new(TenantId::new(), session_id, "owner");
        let mut providers = BTreeMap::new();
        for index in 0..=HAND_LEASE_SESSION_PAGE_SIZE {
            providers.insert(
                format!("provider-{index:03}"),
                InstalledManifestMarker {
                    manifest_identity: uuid::Uuid::new_v4(),
                    handle: HandHandle::local(PathBuf::from(format!("/tmp/provider-{index:03}"))),
                    generation: Some(1),
                },
            );
        }
        let unrelated = providers
            .first_key_value()
            .map(|(_, marker)| marker.clone())
            .expect("provider fixture is not empty");
        owner
            .installed_files
            .write()
            .await
            .insert(scope.clone(), providers);
        owner.installed_files.write().await.insert(
            unrelated_scope.clone(),
            BTreeMap::from([("local".to_string(), unrelated)]),
        );

        owner.clear_installed_files_for_scope(&scope).await;

        let installed = owner.installed_files.read().await;
        assert!(!installed.contains_key(&scope));
        assert_eq!(installed.get(&unrelated_scope).map(BTreeMap::len), Some(1));
    }
}

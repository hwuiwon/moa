//! Router construction, provider configuration, and MCP loading helpers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use moa_config::CloudHandProviderKind;
use moa_config::CloudHandsConfig;
use moa_config::LOCAL_DEVELOPMENT_SANDBOX_REVISION;
use moa_config::MoaConfig;
use moa_connectors::catalog::InstalledConnectorCatalogSnapshot;
use moa_connectors::domain::ConnectionDefinitionRef;
use moa_connectors::executor::ConnectorActionRuntime;
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider, traits::LineageHandle,
    traits::SandboxStorageProvider, traits::SessionStore, types::action_policy::ActionPolicyEffect,
    types::action_policy::CallOrigin, types::agent::AgentConnectorBinding,
    types::hands::BuiltinPolicyRevision, types::hands::SandboxPolicySnapshot,
    types::hands::SandboxTier,
};
use moa_crypto::KeyManagementProvider;
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, McpEgressGuard, UnmatchedPermissionPattern,
};
use sqlx::PgPool;

use super::normalization::expand_local_path;
use super::profile::{deployment_sandbox_policy, route_sandbox_policy};
use super::provider_credentials::{FileProviderCredentialSource, ProviderCredentialSource};
use super::{
    DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, HandRoute, ToolCatalogSnapshot, ToolRegistry,
    ToolRouter,
};
use crate::adapters::daytona::DaytonaHandProvider;
use crate::adapters::e2b::E2BHandProvider;
use crate::adapters::local::LocalHandProvider;

impl ToolRouter {
    /// Creates a router from explicit providers, a tool registry, and the
    /// deployment sandbox policy layer.
    ///
    /// `deployment_sandbox_policy` is a required parameter rather than a
    /// settable option: the outermost policy layer has to be stated before a
    /// router exists, so no code path can reach provisioning with a substituted
    /// default.
    pub fn new(
        registry: ToolRegistry,
        providers: HashMap<String, Arc<dyn HandProvider>>,
        deployment_sandbox_policy: SandboxPolicySnapshot,
    ) -> Self {
        Self {
            catalog: super::CatalogOwner::new(registry),
            mcp: super::McpOwner::default(),
            hands: super::HandLifecycleOwner::new(providers, deployment_sandbox_policy),
            bindings: super::BuiltInBindings::default(),
        }
    }

    /// Creates a local-only router rooted at a sandbox work directory.
    pub async fn new_local(sandbox_root: impl AsRef<Path>) -> Result<Self> {
        let local_provider = Arc::new(
            LocalHandProvider::new(sandbox_root.as_ref())
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
        );
        let provider: Arc<dyn HandProvider> = local_provider.clone();
        let mut providers = HashMap::new();
        providers.insert(DEFAULT_PROVIDER_NAME.to_string(), provider);
        let mut registry = ToolRegistry::default_local();
        registry.apply_budgets(&MoaConfig::default().tool_budgets);

        let mut router = Self::new(
            registry,
            providers,
            MoaConfig::default().sandbox_policy.deployment.snapshot()?,
        );
        router
            .hands
            .set_sandbox_root(Some(sandbox_root.as_ref().to_path_buf()));
        router.hands.set_local_provider(Some(local_provider));
        Ok(router)
    }

    /// Attaches the durable owner of each tenant's authored sandbox policy layer.
    #[must_use]
    pub fn with_tenant_sandbox_policy_store(
        mut self,
        store: Arc<dyn super::profile::TenantSandboxPolicyStore>,
    ) -> Self {
        self.hands.set_tenant_sandbox_policy(store);
        self
    }

    /// Declares that the durable hand-lease reaper is running for this deployment.
    ///
    /// Providers that rely on the reaper to enforce a deadline are admitted for
    /// bounded deadlines only once this is set, so a deployment that forgets the
    /// reaper fails admission instead of leaking sandboxes.
    #[must_use]
    pub fn with_hand_lease_reaper(mut self) -> Self {
        self.hands.set_hand_lease_reaper_installed();
        self
    }

    /// Creates a tool router from the loaded MOA config.
    ///
    /// The rule-store owner is supplied at construction because the `cloud`
    /// security profile cannot serve without one: a deny-by-default deployment
    /// with no persisted-rule owner could never authorize any action. Under the
    /// `local` profile `rule_store` may be `None` and local hands are used.
    ///
    pub async fn from_config(
        config: &MoaConfig,
        mcp_egress_guard: Option<Arc<McpEgressGuard>>,
        rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
    ) -> Result<Self> {
        Self::from_config_with_checkpoint_store(
            config,
            mcp_egress_guard,
            rule_store,
            None,
            None,
            None,
            true,
        )
        .await
    }

    /// Creates a router with an explicitly composed encrypted checkpoint store.
    pub async fn from_config_with_checkpoint_store(
        config: &MoaConfig,
        mcp_egress_guard: Option<Arc<McpEgressGuard>>,
        rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
        checkpoint_store: Option<
            Arc<super::sandbox_workspace::checkpoint::store::CheckpointObjectStore>,
        >,
        workspace_pool: Option<PgPool>,
        workspace_kms: Option<Arc<dyn KeyManagementProvider>>,
        workspace_providers_enabled: bool,
    ) -> Result<Self> {
        if !config.mcp_servers.is_empty() && mcp_egress_guard.is_none() {
            return Err(MoaError::ConfigError(
                "configured MCP servers require an egress guard".to_string(),
            ));
        }
        validate_mcp_server_configuration(config)?;

        let hand_routes = if workspace_providers_enabled {
            let routes = configured_hand_routes(config)?;
            validate_security_profile(config, &routes, rule_store.as_ref())?;
            routes
        } else {
            Vec::new()
        };

        let mut providers = HashMap::new();
        let mut storage_providers: HashMap<String, Arc<dyn SandboxStorageProvider>> =
            HashMap::new();
        let mut sandbox_root = None;
        let mut local_provider = None;
        if routes_include_local_provider(&hand_routes) {
            let expanded_sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
            let mut provider = LocalHandProvider::new_with_docker_detection(
                &expanded_sandbox_root,
                config.local.docker_enabled,
            )
            .await?
            .with_command_timeout(DEFAULT_TOOL_TIMEOUT);
            if let Some(store) = checkpoint_store.as_ref() {
                provider = provider.with_checkpoint_store(Arc::clone(store));
            }
            let provider = Arc::new(provider);
            let provider_trait: Arc<dyn HandProvider> = provider.clone();
            let storage_trait: Arc<dyn SandboxStorageProvider> = provider.clone();
            providers.insert(DEFAULT_PROVIDER_NAME.to_string(), provider_trait);
            storage_providers.insert(DEFAULT_PROVIDER_NAME.to_string(), storage_trait);
            sandbox_root = Some(expanded_sandbox_root);
            local_provider = Some(provider);
        }

        if workspace_providers_enabled && let Some(hands) = &config.cloud.hands {
            let cloud_requested = cloud_provider_requested(hands, "daytona")
                || cloud_provider_requested(hands, "e2b");
            let credentials = if cloud_requested {
                let source = Arc::new(FileProviderCredentialSource::from_config(hands)?);
                source.validate_all().await?;
                Some(source as Arc<dyn ProviderCredentialSource>)
            } else {
                None
            };
            if cloud_provider_requested(hands, "daytona") {
                let source = credentials.as_ref().ok_or_else(|| {
                    MoaError::ConfigError("Daytona credential source is unavailable".to_string())
                })?;
                let checkpoint_store = checkpoint_store.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "Daytona persistent workspaces require a checkpoint object store"
                            .to_string(),
                    )
                })?;
                let pool = workspace_pool.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "Daytona persistent workspaces require durable workspace repositories"
                            .to_string(),
                    )
                })?;
                let kms = workspace_kms.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "Daytona persistent workspaces require durable KMS marker authority"
                            .to_string(),
                    )
                })?;
                let provider = Arc::new(DaytonaHandProvider::new_with_storage(
                    Arc::clone(source),
                    crate::adapters::daytona::storage::DaytonaStorageDependencies {
                        config: config.cloud.daytona_storage.clone(),
                        checkpoint_store: Arc::clone(checkpoint_store),
                        workspaces: Arc::new(super::sandbox_workspace::repository::PostgresWorkspaceRepository::new(
                            pool.clone(),
                        )),
                        storage_resources: Arc::new(
                            super::sandbox_workspace::storage_resources::PostgresWorkspaceStorageResourceRepository::new(
                                pool.clone(),
                            ),
                        ),
                        operations: Arc::new(
                            super::sandbox_workspace::operations::PostgresWorkspaceOperationRepository::new(
                                pool.clone(),
                            ),
                        ),
                        capacity: Arc::new(
                            super::sandbox_workspace::capacity::PostgresWorkspaceCapacityRepository::new(
                                pool.clone(),
                            ),
                        ),
                        kms: Arc::clone(kms),
                    },
                )?);
                let hand: Arc<dyn HandProvider> = provider.clone();
                let storage: Arc<dyn SandboxStorageProvider> = provider;
                providers.insert("daytona".to_string(), hand);
                storage_providers.insert("daytona".to_string(), storage);
            }
            if cloud_provider_requested(hands, "e2b") {
                let source = credentials.as_ref().ok_or_else(|| {
                    MoaError::ConfigError("E2B credential source is unavailable".to_string())
                })?;
                let checkpoint_store = checkpoint_store.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "E2B persistent workspaces require a checkpoint object store".to_string(),
                    )
                })?;
                let provider = Arc::new(
                    E2BHandProvider::new(Arc::clone(source))
                        .with_checkpoint_store(Arc::clone(checkpoint_store)),
                );
                let hand: Arc<dyn HandProvider> = provider.clone();
                let storage: Arc<dyn SandboxStorageProvider> = provider;
                providers.insert("e2b".to_string(), hand);
                storage_providers.insert("e2b".to_string(), storage);
            }
        }

        let mut registry = ToolRegistry::default_local();
        registry.apply_budgets(&config.tool_budgets);
        if !is_local_only_route(&hand_routes) {
            for route in &hand_routes {
                if !providers.contains_key(&route.provider) {
                    return Err(MoaError::ConfigError(format!(
                        "cloud hand route provider {} was selected but not registered",
                        route.provider
                    )));
                }
            }
            registry.retarget_hand_tools(hand_routes);
        }

        let mut router = Self::new(registry, providers, deployment_sandbox_policy(config)?);
        router.hands.checkpoint_store = checkpoint_store;
        router.hands.set_storage_providers(storage_providers);
        if let Some(pool) = workspace_pool {
            router = router.with_workspace_repositories(pool);
        }
        router.hands.set_sandbox_root(sandbox_root);
        router.hands.set_local_provider(local_provider);
        router.mcp.set_egress_guard(mcp_egress_guard);
        router.bindings.set_rule_store(rule_store);
        router = router
            .with_tool_output_config(config.tool_output.clone())
            .with_tool_budgets(config.tool_budgets.clone())
            .with_policies(ActionPolicies::from_config(config)?);

        if !config.mcp_servers.is_empty() {
            router.load_mcp_servers(config).await?;
        }
        // Runs after discovery so connector tools are in the catalog being
        // checked against; a pattern authored for a tool nobody registered is
        // reported here rather than discovered when it fails to gate something.
        router.refresh_unmatched_permission_patterns();

        Ok(router)
    }

    /// Rejects a fully assembled cloud router that has no owner for its
    /// sandboxes' durable state or destruction.
    ///
    /// This runs after the builder chain rather than inside
    /// [`ToolRouter::from_config`], because the lease store and the reaper are
    /// attached by the composition root and simply do not exist yet while the
    /// router is being constructed. A cloud deployment that provisions cloud
    /// sandboxes with no durable lease owner cannot recover them across
    /// replicas, and one with no reaper cannot destroy them at all — so both are
    /// startup failures rather than something to discover in production.
    pub fn validate_cloud_startup(&self, config: &MoaConfig) -> Result<()> {
        if !config.security_profile.is_cloud() {
            return Ok(());
        }
        if !self.hands.has_hand_lease_store() {
            return Err(MoaError::ConfigError(
                "security_profile=cloud requires a durable hand lease store owner".to_string(),
            ));
        }
        if !self.hands.hand_lease_reaper_installed() {
            return Err(MoaError::ConfigError(
                "security_profile=cloud requires the durable hand-lease reaper; without it no \
                 sandbox deadline has a destruction owner"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Recomputes which configured permission patterns govern no registered tool.
    ///
    /// Called after the catalog is built and after every refresh, because the
    /// answer is a function of the live tool set: a pattern that matches nothing
    /// today may match once a lazy connector is discovered, and one that matches
    /// today stops mattering if its connector is withdrawn.
    pub(super) fn refresh_unmatched_permission_patterns(&self) {
        let tool_names = self.tool_names();
        let unmatched = self.bindings.policies.unmatched_patterns(&tool_names);
        for pattern in &unmatched {
            tracing::warn!(
                field = pattern.field,
                pattern = %pattern.pattern,
                registered_tools = tool_names.len(),
                "configured permission pattern matches no registered tool, so it governs \
                 nothing; a tool it was written to deny or gate would run ungated"
            );
        }
        self.bindings.set_unmatched_permission_patterns(unmatched);
    }

    /// Returns the configured permission patterns that govern no registered tool.
    #[must_use]
    pub fn unmatched_permission_patterns(&self) -> Vec<UnmatchedPermissionPattern> {
        self.bindings.unmatched_permission_patterns()
    }

    /// Returns the registered hand providers in stable name order.
    ///
    /// The durable reaper needs these to destroy sandboxes it claims, and it
    /// must see exactly the providers this router can provision through — a
    /// separately assembled list would silently stop reaping a provider someone
    /// added here.
    #[must_use]
    pub fn hand_providers(&self) -> Vec<Arc<dyn HandProvider>> {
        self.hands.providers()
    }

    /// Attaches the persistent-workspace adapter for an already registered hand provider.
    ///
    /// Explicit router composition uses the provider-declared stable name so
    /// compute and storage cannot be wired under different route identities.
    /// Duplicate storage registrations are rejected instead of silently
    /// replacing the adapter that owns durable workspace state.
    pub fn with_sandbox_storage_provider(
        mut self,
        provider: Arc<dyn SandboxStorageProvider>,
    ) -> Result<Self> {
        let name = provider.storage_provider_name().to_string();
        if name.is_empty() || !self.hands.providers.contains_key(&name) {
            return Err(MoaError::ConfigError(format!(
                "sandbox storage provider {name:?} has no matching hand provider"
            )));
        }
        if self.hands.storage_providers.contains_key(&name) {
            return Err(MoaError::ConfigError(format!(
                "sandbox storage provider {name} is already registered"
            )));
        }
        self.hands.storage_providers.insert(name, provider);
        Ok(self)
    }

    /// Returns every registered workspace-storage provider in stable name order.
    #[must_use]
    pub fn sandbox_storage_providers(&self) -> Vec<Arc<dyn SandboxStorageProvider>> {
        let mut providers = self
            .hands
            .storage_providers
            .iter()
            .map(|(name, provider)| (name.as_str(), Arc::clone(provider)))
            .collect::<Vec<_>>();
        providers.sort_by_key(|(name, _)| *name);
        providers
            .into_iter()
            .map(|(_, provider)| provider)
            .collect()
    }

    /// Attaches a persistent action-policy rule store to the router.
    #[must_use]
    pub fn with_rule_store(mut self, rule_store: Arc<dyn ActionPolicyRuleStore>) -> Self {
        self.bindings.set_rule_store(Some(rule_store));
        self
    }

    /// Attaches a session store so built-in tools can introspect session history.
    #[must_use]
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.bindings.set_session_store(Some(session_store));
        self
    }

    /// Attaches a durable hand lease store for cross-replica sandbox recovery.
    #[must_use]
    pub fn with_hand_lease_store(
        mut self,
        hand_leases: Arc<dyn super::leases::HandLeaseStore>,
    ) -> Self {
        self.hands.set_hand_lease_store(hand_leases);
        self
    }

    /// Attaches the tenant workspace and operation-ledger owners.
    ///
    /// Both repositories share one pool and are installed together because
    /// workspace lifecycle transitions require both durable records.
    #[must_use]
    pub fn with_workspace_repositories(mut self, pool: PgPool) -> Self {
        self.hands.set_workspace_repositories(
            Arc::new(
                super::sandbox_workspace::repository::PostgresWorkspaceRepository::new(
                    pool.clone(),
                ),
            ),
            Arc::new(
                super::sandbox_workspace::operations::PostgresWorkspaceOperationRepository::new(
                    pool,
                ),
            ),
        );
        self
    }

    /// Attaches a graph-memory executor used by the built-in memory tools.
    #[must_use]
    pub fn with_memory_tool_executor(
        mut self,
        executor: Arc<dyn moa_core::traits::MemoryToolExecutor>,
    ) -> Self {
        self.bindings.set_memory_tool_executor(Some(executor));
        self
    }

    /// Returns the explicitly installed graph-memory executor, when this host enables it.
    pub fn memory_tool_executor(&self) -> Option<Arc<dyn moa_core::traits::MemoryToolExecutor>> {
        self.bindings.memory_tool_executor()
    }

    /// Attaches the read-only retrieval executor backing the agentic memory tools.
    #[must_use]
    pub fn with_memory_retrieval_executor(
        mut self,
        executor: Arc<dyn moa_core::traits::MemoryRetrievalExecutor>,
    ) -> Self {
        self.bindings.set_memory_retrieval_executor(Some(executor));
        self
    }

    /// Installs or replaces the read-only retrieval executor backing the agentic memory tools.
    pub async fn set_memory_retrieval_executor(
        &self,
        executor: Arc<dyn moa_core::traits::MemoryRetrievalExecutor>,
    ) {
        self.bindings
            .replace_memory_retrieval_executor(executor)
            .await;
    }

    /// Attaches the hot-path lineage handle for built-in tools.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.bindings.lineage = lineage;
        self
    }

    /// Overrides the router's policy configuration.
    #[must_use]
    pub fn with_policies(mut self, policies: ActionPolicies) -> Self {
        self.bindings.set_policies(policies);
        self
    }

    /// Declares the provenance class of the runtime this router serves.
    ///
    /// This is a deployment-level ceiling on the whole router, for a router
    /// assembled to serve nothing but trials or generated code. The default is
    /// [`CallOrigin::Production`], which admits everything, and it is correct
    /// for the shared router the orchestrator builds once per process: that
    /// router serves production sessions and trial-owned sessions alike, so the
    /// per-call ceiling comes from the session instead — see
    /// [`ToolRouter::effective_call_origin`].
    #[must_use]
    pub fn with_call_origin(mut self, call_origin: CallOrigin) -> Self {
        self.bindings.set_call_origin(call_origin);
        self
    }

    /// Returns the provenance class of the runtime this router serves.
    #[must_use]
    pub fn call_origin(&self) -> CallOrigin {
        self.bindings.call_origin()
    }

    /// Returns the origin governing one call, composing both ceilings.
    ///
    /// A tool call has two independent statements of provenance: the runtime
    /// that assembled the router, and the runtime that created the session being
    /// served. Both are ceilings, so the governing origin is the stricter of the
    /// two — a shared production-origin router still fences a trial-owned
    /// session, and a trial-origin router still fences a session that was
    /// created for production.
    ///
    /// Every admission decision in this crate goes through here rather than
    /// reading either ceiling alone.
    #[must_use]
    pub fn effective_call_origin(
        &self,
        session: &moa_core::types::session::SessionMeta,
    ) -> CallOrigin {
        self.bindings
            .call_origin()
            .most_restrictive(session.call_origin)
    }

    /// Returns the live catalog snapshot.
    ///
    /// Every read takes one whole snapshot, so a caller that inspects several
    /// tools sees them all from the same catalog revision even if a background
    /// refresh publishes between its calls.
    pub(super) fn registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(&self.activated_catalog().registry)
    }

    /// Returns one immutable publication of schemas, contracts, and routes.
    ///
    /// Consumers that need more than one catalog fact must retain this handle
    /// for the whole decision instead of making independent router reads that a
    /// refresh can split across revisions.
    #[must_use]
    pub fn activated_catalog(&self) -> Arc<ToolCatalogSnapshot> {
        self.mcp.request_refresh_if_stale();
        self.catalog.activated()
    }

    /// Builds one ephemeral installed-connector overlay over an immutable base catalog.
    ///
    /// `installed` must already have been produced by the governed catalog for
    /// the authoritative caller and exactly the connection IDs in `bindings`.
    /// This method performs the second, structural join: every installed action
    /// must match one exact logical connector binding and its published artifact
    /// revision. The returned snapshot is never published into the process-wide
    /// router and therefore cannot leak one agent's connection catalog into
    /// another session.
    pub fn installed_connector_overlay(
        &self,
        base: &ToolCatalogSnapshot,
        installed: &InstalledConnectorCatalogSnapshot,
        bindings: &[AgentConnectorBinding],
        runtime: Arc<dyn ConnectorActionRuntime>,
    ) -> Result<Arc<ToolCatalogSnapshot>> {
        self.require_owned_catalog(base)?;
        if base.registry.tools.values().any(|registered| {
            matches!(
                &registered.execution,
                super::ToolExecution::InstalledConnectorAction { .. }
            )
        }) {
            return Err(MoaError::ValidationError(
                "installed connector overlays must be built from the immutable deployment catalog"
                    .to_string(),
            ));
        }

        let mut by_connection = HashMap::new();
        let mut connector_refs = HashSet::new();
        for binding in bindings {
            if binding.connector_ref.trim().is_empty()
                || binding.connector_ref.trim() != binding.connector_ref
            {
                return Err(MoaError::ValidationError(
                    "agent connector binding reference must be non-empty and trimmed".to_string(),
                ));
            }
            if !connector_refs.insert(binding.connector_ref.as_str()) {
                return Err(MoaError::ValidationError(format!(
                    "duplicate agent connector binding for `{}`",
                    binding.connector_ref
                )));
            }
            if by_connection
                .insert(binding.connection_id, binding)
                .is_some()
            {
                return Err(MoaError::ValidationError(format!(
                    "multiple logical connector bindings select connection {}",
                    binding.connection_id
                )));
            }
        }

        let mut registry = (*base.registry).clone();
        let mut observed_connections = HashSet::new();
        for action in installed.actions() {
            let binding = by_connection.get(&action.connection_id()).ok_or_else(|| {
                MoaError::ValidationError(format!(
                    "installed action for connection {} has no agent connector binding",
                    action.connection_id()
                ))
            })?;
            let expected = ConnectionDefinitionRef::Artifact {
                artifact_uid: binding.artifact_uid,
                revision_uid: binding.revision_uid,
            };
            if action.definition() != &expected {
                return Err(MoaError::ValidationError(format!(
                    "agent connector binding `{}` does not match the installed definition revision",
                    binding.connector_ref
                )));
            }
            registry.register_installed_connector_action(
                &binding.connector_ref,
                action,
                Arc::clone(&runtime),
            )?;
            observed_connections.insert(action.connection_id());
        }

        if let Some(missing) = bindings
            .iter()
            .find(|binding| !observed_connections.contains(&binding.connection_id))
        {
            return Err(MoaError::ValidationError(format!(
                "agent connector binding `{}` has no active installed action",
                missing.connector_ref
            )));
        }

        registry.apply_budgets(self.bindings.tool_budgets());
        Ok(Arc::new(ToolCatalogSnapshot::new(
            self.catalog.owner_id(),
            registry,
        )))
    }

    /// Rejects a snapshot minted by a different router instance.
    pub(super) fn require_owned_catalog(&self, catalog: &ToolCatalogSnapshot) -> Result<()> {
        if catalog.owner_id != self.catalog.owner_id() {
            return Err(MoaError::ValidationError(
                "tool catalog snapshot belongs to a different router instance".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the cached pin from the same snapshot dispatch reads.
    pub(super) fn catalog_pin(&self) -> Result<super::ToolCatalogPin> {
        self.activated_catalog().pin()
    }

    /// Publishes a new catalog snapshot and the prompt schemas derived from it.
    ///
    /// Both are replaced under the same publication so no caller can compile a
    /// prompt from one catalog revision and dispatch against another.
    pub(super) fn publish_registry(&self, registry: ToolRegistry) {
        self.publish_catalog_snapshot(ToolCatalogSnapshot::new(self.catalog.owner_id(), registry));
    }

    /// Publishes a fully derived immutable catalog in one lock acquisition.
    pub(super) fn publish_catalog_snapshot(&self, snapshot: ToolCatalogSnapshot) {
        self.catalog.publish(snapshot);
    }

    /// Returns the ordered tool schemas for prompt compilation.
    ///
    /// The order is the deployment's declared capability priority, not lexical
    /// order: a consumer that must fit a schema cap drops from the end.
    pub fn tool_schemas(&self) -> Vec<serde_json::Value> {
        (*self.tool_schema_snapshot()).clone()
    }

    /// Returns the shared prompt-schema snapshot of the live catalog.
    ///
    /// Callers that recompile a prompt per turn should read this rather than
    /// caching a copy at startup: a cached copy would keep advertising tools a
    /// catalog refresh has already changed or withdrawn.
    #[must_use]
    pub fn tool_schema_snapshot(&self) -> Arc<Vec<serde_json::Value>> {
        self.activated_catalog().tool_schema_snapshot()
    }

    /// Returns the prompt schemas for the read-only agentic memory tools.
    ///
    /// These are registered but excluded from the default loadout; the brain
    /// surfaces them onto a turn only when the agentic retrieval strategy is
    /// selected or the injected retrieval came back empty.
    pub fn agentic_memory_tool_schemas(&self) -> Vec<serde_json::Value> {
        self.registry()
            .tool_schemas_for(crate::tools::memory::AGENTIC_MEMORY_TOOL_NAMES)
    }

    /// Returns the stable registered tool names in sorted order.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self.registry().tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Returns whether a tool is currently registered on the router.
    pub fn has_tool(&self, name: &str) -> bool {
        self.registry().tools.contains_key(name)
    }

    /// Returns whether the named tool provisions a hand/sandbox to execute.
    ///
    /// Delegates to the registry's execution-routing classification. Hand-routed
    /// tools are the ones hard-excluded from the sandbox-free root coordinator's
    /// tool set; built-in and MCP tools are coordinator-safe.
    pub fn tool_requires_sandbox(&self, name: &str) -> bool {
        self.registry().tool_requires_sandbox(name)
    }

    /// Returns one registered tool definition by name.
    pub fn tool_definition(&self, name: &str) -> Option<moa_core::types::tools::ToolDefinition> {
        self.registry()
            .tools
            .get(name)
            .map(|registered| registered.definition.clone())
    }

    /// Returns every registered tool definition in stable name order.
    pub fn tool_definitions(&self) -> Vec<moa_core::types::tools::ToolDefinition> {
        let mut definitions = self
            .registry()
            .tools
            .values()
            .map(|registered| registered.definition.clone())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    /// Restricts the router to an explicit set of enabled tool names.
    #[must_use]
    pub fn with_enabled_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut registry = (*self.registry()).clone();
        registry.retain_only(tool_names);
        self.publish_registry(registry);
        self
    }

    /// Loads MCP credentials and discovers the configured connectors.
    async fn load_mcp_servers(&mut self, config: &MoaConfig) -> Result<()> {
        self.mcp.configure(&config.mcp_servers)?;
        self.load_mcp_catalog(config).await
    }
}

/// Rejects MCP server configurations a deterministic catalog cannot be built from.
///
/// Both rejections are startup failures because both are silent otherwise: a
/// duplicate server name would have one connector's configuration quietly
/// overwrite the other's, and a required connector configured for lazy discovery
/// would let startup pass without ever having verified the integration the
/// operator declared mandatory.
fn validate_mcp_server_configuration(config: &MoaConfig) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for server in &config.mcp_servers {
        if !seen.insert(server.name.as_str()) {
            return Err(MoaError::ConfigError(format!(
                "duplicate MCP server name configured: {}",
                server.name
            )));
        }
        if server.required && !server.discovery.is_eager() {
            return Err(MoaError::ConfigError(format!(
                "MCP server {} is required, so its tools cannot be discovered lazily",
                server.name
            )));
        }
    }
    Ok(())
}

fn is_local_only_route(routes: &[HandRoute]) -> bool {
    routes.len() == 1
        && routes[0].provider == DEFAULT_PROVIDER_NAME
        && matches!(routes[0].tier, SandboxTier::Local)
}

fn routes_include_local_provider(routes: &[HandRoute]) -> bool {
    routes.iter().any(|route| {
        route.provider == DEFAULT_PROVIDER_NAME && matches!(route.tier, SandboxTier::Local)
    })
}

/// Validates the configured security profile against the resolved hand routes
/// and the supplied rule-store owner before any router is returned.
///
/// The `cloud` profile fails closed on every one of its four requirements: a
/// deny-by-default permission posture, a real persisted-rule owner, a non-local
/// sandbox backend, and present credentials for every selected backend. The
/// `local` profile is the only posture under which local host hands may run.
///
/// Emitted diagnostics name the profile and the owner kinds only; configured
/// patterns, credentials, and invocation inputs are never logged here.
fn validate_security_profile(
    config: &MoaConfig,
    routes: &[HandRoute],
    rule_store: Option<&Arc<dyn ActionPolicyRuleStore>>,
) -> Result<()> {
    if !config.security_profile.is_cloud() {
        if routes_include_local_provider(routes) {
            tracing::info!(
                security_profile = config.security_profile.as_str(),
                rule_store_owner = if rule_store.is_some() {
                    "persistent"
                } else {
                    "none"
                },
                sandbox_backend = DEFAULT_PROVIDER_NAME,
                "tool router constructed with local host hands"
            );
        }
        return Ok(());
    }

    if config.permissions.default_effect != ActionPolicyEffect::Deny {
        return Err(MoaError::ConfigError(format!(
            "security_profile=cloud requires permissions.default_effect=deny, found {}",
            config.permissions.default_effect.as_str()
        )));
    }
    if rule_store.is_none() {
        return Err(MoaError::ConfigError(
            "security_profile=cloud requires a persistent action-policy rule store owner"
                .to_string(),
        ));
    }
    if routes.is_empty() {
        return Err(MoaError::ConfigError(
            "security_profile=cloud requires a configured cloud sandbox backend, found none"
                .to_string(),
        ));
    }
    if routes_include_local_provider(routes) {
        return Err(MoaError::ConfigError(
            "security_profile=cloud rejects the local hand provider; configure \
             cloud.hands.default_provider as daytona or e2b"
                .to_string(),
        ));
    }
    if config.sandbox_policy.is_local_development_default() {
        return Err(MoaError::ConfigError(format!(
            "security_profile=cloud requires an authored [sandbox_policy.deployment] section; \
             found the built-in local development policy `{LOCAL_DEVELOPMENT_SANDBOX_REVISION}`"
        )));
    }
    let hands = config.cloud.hands.as_ref().ok_or_else(|| {
        MoaError::ConfigError(
            "security_profile=cloud requires a cloud.hands configuration section".to_string(),
        )
    })?;
    for route in routes {
        if !cloud_provider_credential_present(hands, &route.provider) {
            return Err(MoaError::ConfigError(format!(
                "security_profile=cloud requires credentials for the selected {} sandbox backend",
                route.provider
            )));
        }
    }

    tracing::info!(
        security_profile = config.security_profile.as_str(),
        rule_store_owner = "persistent",
        sandbox_backend = routes
            .first()
            .map_or(DEFAULT_PROVIDER_NAME, |route| route.provider.as_str()),
        "tool router constructed with fail-closed cloud posture"
    );
    Ok(())
}

/// Returns whether the credential the named cloud backend needs is present.
fn cloud_provider_credential_present(hands: &CloudHandsConfig, provider: &str) -> bool {
    let expected = match provider {
        "daytona" => CloudHandProviderKind::Daytona,
        "e2b" => CloudHandProviderKind::E2b,
        _ => return false,
    };
    hands
        .provider_accounts
        .iter()
        .any(|account| account.provider == expected)
}

/// Attaches each route's authored sandbox policy layer, or the named
/// route-unset layer when the deployment authored none for that provider.
fn attach_route_policies(config: &MoaConfig, routes: &mut [HandRoute]) -> Result<()> {
    for route in routes {
        route.policy = route_sandbox_policy(config, &route.provider)?;
    }
    Ok(())
}

fn configured_hand_routes(config: &MoaConfig) -> Result<Vec<HandRoute>> {
    let mut routes = configured_hand_route_targets(config)?;
    attach_route_policies(config, &mut routes)?;
    Ok(routes)
}

fn configured_hand_route_targets(config: &MoaConfig) -> Result<Vec<HandRoute>> {
    let provider = config
        .cloud
        .hands
        .as_ref()
        .and_then(|hands| {
            hands
                .default_provider
                .as_deref()
                .map(str::trim)
                .filter(|provider| !provider.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
    let Some(hands) = config.cloud.hands.as_ref() else {
        return route_for_provider(&provider).map(|route| vec![route]);
    };

    let mut routes = vec![route_for_provider(&provider)?];
    if provider == DEFAULT_PROVIDER_NAME {
        if hands
            .fallback_providers
            .iter()
            .any(|provider| !provider.trim().is_empty())
        {
            return Err(MoaError::ConfigError(
                "cloud hand fallback providers require default_provider to be daytona or e2b"
                    .to_string(),
            ));
        }
        return Ok(routes);
    }

    for fallback in &hands.fallback_providers {
        let fallback = fallback.trim();
        if fallback.is_empty() || routes.iter().any(|route| route.provider == fallback) {
            continue;
        }
        let route = route_for_cloud_provider(fallback)?;
        routes.push(route);
    }
    Ok(routes)
}

fn route_for_provider(provider: &str) -> Result<HandRoute> {
    match provider {
        DEFAULT_PROVIDER_NAME => Ok(HandRoute::local()),
        cloud_provider => route_for_cloud_provider(cloud_provider),
    }
}

fn route_for_cloud_provider(provider: &str) -> Result<HandRoute> {
    let tier = match provider {
        "daytona" => SandboxTier::Container,
        "e2b" => SandboxTier::MicroVM,
        other => {
            return Err(MoaError::ConfigError(format!(
                "unsupported cloud hand provider configured: {other}"
            )));
        }
    };
    Ok(HandRoute {
        provider: provider.to_string(),
        tier,
        policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
    })
}

fn cloud_provider_requested(hands: &CloudHandsConfig, provider: &str) -> bool {
    hands
        .default_provider
        .as_deref()
        .is_some_and(|candidate| candidate.trim() == provider)
        || hands
            .fallback_providers
            .iter()
            .any(|candidate| candidate.trim() == provider)
        || hands
            .provider_accounts
            .iter()
            .any(|account| account.provider.as_str() == provider)
}

#[cfg(test)]
mod tests {
    use moa_config::CloudHandsConfig;
    use moa_config::MoaConfig;
    use moa_core::types::hands::SandboxTier;

    use super::{DEFAULT_PROVIDER_NAME, configured_hand_routes};

    #[test]
    fn configured_hand_routes_preserve_cloud_fallback_order() {
        // Pins: cloud hand fallback stays an ordered runtime route list.
        let mut config = MoaConfig::default();
        let hands = config
            .cloud
            .hands
            .get_or_insert_with(CloudHandsConfig::default);
        hands.default_provider = Some("daytona".to_string());
        hands.fallback_providers = vec!["e2b".to_string(), "daytona".to_string(), " ".to_string()];

        let routes = configured_hand_routes(&config).expect("routes should configure");

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].provider, "daytona");
        assert_eq!(routes[0].tier, SandboxTier::Container);
        assert_eq!(routes[1].provider, "e2b");
        assert_eq!(routes[1].tier, SandboxTier::MicroVM);
    }

    #[test]
    fn configured_hand_routes_reject_local_fallback_chain() {
        // Pins: local hands remain a single-provider route instead of a cloud chain.
        let mut config = MoaConfig::default();
        let hands = config
            .cloud
            .hands
            .get_or_insert_with(CloudHandsConfig::default);
        hands.default_provider = Some(DEFAULT_PROVIDER_NAME.to_string());
        hands.fallback_providers = vec!["e2b".to_string()];

        let error = configured_hand_routes(&config).expect_err("local fallback should fail closed");

        assert!(
            error
                .to_string()
                .contains("fallback providers require default_provider")
        );
    }
}

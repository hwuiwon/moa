//! Router construction, provider configuration, and MCP loading helpers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use moa_config::CloudHandsConfig;
use moa_config::LOCAL_DEVELOPMENT_SANDBOX_REVISION;
use moa_config::MoaConfig;
use moa_config::ToolBudgetConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    error::MoaError, error::Result, traits::HandProvider, traits::LineageHandle,
    traits::NullLineageHandle, traits::SessionStore, types::action_policy::ActionPolicyEffect,
    types::action_policy::CallOrigin, types::hands::BuiltinPolicyRevision,
    types::hands::SandboxPolicySnapshot, types::hands::SandboxTier,
};
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, McpDeploymentCredentials, McpEgressGuard,
    UnmatchedPermissionPattern,
};

use super::normalization::expand_local_path;
use super::profile::{deployment_sandbox_policy, route_sandbox_policy};
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
            catalog: std::sync::RwLock::new(Arc::new(ToolCatalogSnapshot::new(registry))),
            providers,
            deployment_sandbox_policy,
            tenant_sandbox_policy: None,
            hand_lease_reaper_installed: false,
            local_provider: None,
            mcp_clients: tokio::sync::RwLock::new(HashMap::new()),
            mcp_servers: HashMap::new(),
            mcp_health: tokio::sync::RwLock::new(std::collections::BTreeMap::new()),
            mcp_credentials: McpDeploymentCredentials::default(),
            mcp_egress_guard: None,
            active_hands: tokio::sync::RwLock::new(HashMap::new()),
            preferred_hand_routes: tokio::sync::RwLock::new(HashMap::new()),
            hand_leases: None,
            trusted_sandbox_files: tokio::sync::RwLock::new(HashMap::new()),
            installed_files: tokio::sync::RwLock::new(HashMap::new()),
            workspace_roots: tokio::sync::RwLock::new(HashMap::new()),
            policies: ActionPolicies::default(),
            call_origin: CallOrigin::Production,
            unmatched_permission_patterns: std::sync::RwLock::new(Vec::new()),
            rule_store: None,
            session_store: None,
            memory_tool_executor: tokio::sync::RwLock::new(None),
            memory_retrieval_executor: tokio::sync::RwLock::new(None),
            lineage: Arc::new(NullLineageHandle),
            sandbox_root: None,
            tool_output: ToolOutputConfig::default(),
            tool_budgets: ToolBudgetConfig::default(),
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

        Ok(Self {
            sandbox_root: Some(sandbox_root.as_ref().to_path_buf()),
            local_provider: Some(local_provider),
            ..Self::new(
                registry,
                providers,
                MoaConfig::default().sandbox_policy.deployment.snapshot()?,
            )
        })
    }

    /// Attaches the durable owner of each tenant's authored sandbox policy layer.
    #[must_use]
    pub fn with_tenant_sandbox_policy_store(
        mut self,
        store: Arc<dyn super::profile::TenantSandboxPolicyStore>,
    ) -> Self {
        self.tenant_sandbox_policy = Some(store);
        self
    }

    /// Declares that the durable hand-lease reaper is running for this deployment.
    ///
    /// Providers that rely on the reaper to enforce a deadline are admitted for
    /// bounded deadlines only once this is set, so a deployment that forgets the
    /// reaper fails admission instead of leaking sandboxes.
    #[must_use]
    pub fn with_hand_lease_reaper(mut self) -> Self {
        self.hand_lease_reaper_installed = true;
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
        if !config.mcp_servers.is_empty() && mcp_egress_guard.is_none() {
            return Err(MoaError::ConfigError(
                "configured MCP servers require an egress guard".to_string(),
            ));
        }
        validate_mcp_server_configuration(config)?;

        let hand_routes = configured_hand_routes(config)?;
        validate_security_profile(config, &hand_routes, rule_store.as_ref())?;

        let mut providers = HashMap::new();
        let mut sandbox_root = None;
        let mut local_provider = None;
        if routes_include_local_provider(&hand_routes) {
            let expanded_sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
            let provider = Arc::new(
                LocalHandProvider::new_with_docker_detection(
                    &expanded_sandbox_root,
                    config.local.docker_enabled,
                )
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
            );
            let provider_trait: Arc<dyn HandProvider> = provider.clone();
            providers.insert(DEFAULT_PROVIDER_NAME.to_string(), provider_trait);
            sandbox_root = Some(expanded_sandbox_root);
            local_provider = Some(provider);
        }

        if let Some(hands) = &config.cloud.hands
            && cloud_provider_requested(hands, "daytona")
        {
            providers.insert(
                "daytona".to_string(),
                Arc::new(DaytonaHandProvider::from_config(config)?),
            );
        }

        if let Some(hands) = &config.cloud.hands
            && cloud_provider_requested(hands, "e2b")
        {
            providers.insert(
                "e2b".to_string(),
                Arc::new(E2BHandProvider::from_config(config)?),
            );
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

        let mut router = Self {
            sandbox_root,
            local_provider,
            mcp_egress_guard,
            rule_store,
            ..Self::new(registry, providers, deployment_sandbox_policy(config)?)
        }
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
        if self.hand_leases.is_none() {
            return Err(MoaError::ConfigError(
                "security_profile=cloud requires a durable hand lease store owner".to_string(),
            ));
        }
        if !self.hand_lease_reaper_installed {
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
        let unmatched = self.policies.unmatched_patterns(&tool_names);
        for pattern in &unmatched {
            tracing::warn!(
                field = pattern.field,
                pattern = %pattern.pattern,
                registered_tools = tool_names.len(),
                "configured permission pattern matches no registered tool, so it governs \
                 nothing; a tool it was written to deny or gate would run ungated"
            );
        }
        *self
            .unmatched_permission_patterns
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = unmatched;
    }

    /// Returns the configured permission patterns that govern no registered tool.
    #[must_use]
    pub fn unmatched_permission_patterns(&self) -> Vec<UnmatchedPermissionPattern> {
        self.unmatched_permission_patterns
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns the registered hand providers in stable name order.
    ///
    /// The durable reaper needs these to destroy sandboxes it claims, and it
    /// must see exactly the providers this router can provision through — a
    /// separately assembled list would silently stop reaping a provider someone
    /// added here.
    #[must_use]
    pub fn hand_providers(&self) -> Vec<Arc<dyn HandProvider>> {
        let mut names = self.providers.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .filter_map(|name| self.providers.get(&name).cloned())
            .collect()
    }

    /// Attaches a persistent action-policy rule store to the router.
    #[must_use]
    pub fn with_rule_store(mut self, rule_store: Arc<dyn ActionPolicyRuleStore>) -> Self {
        self.rule_store = Some(rule_store);
        self
    }

    /// Attaches a session store so built-in tools can introspect session history.
    #[must_use]
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }

    /// Attaches a durable hand lease store for cross-replica sandbox recovery.
    #[must_use]
    pub fn with_hand_lease_store(
        mut self,
        hand_leases: Arc<dyn super::leases::HandLeaseStore>,
    ) -> Self {
        self.hand_leases = Some(hand_leases);
        self
    }

    /// Attaches a graph-memory executor used by the built-in memory tools.
    #[must_use]
    pub fn with_memory_tool_executor(
        mut self,
        executor: Arc<dyn moa_core::traits::MemoryToolExecutor>,
    ) -> Self {
        self.memory_tool_executor = tokio::sync::RwLock::new(Some(executor));
        self
    }

    /// Attaches the read-only retrieval executor backing the agentic memory tools.
    #[must_use]
    pub fn with_memory_retrieval_executor(
        mut self,
        executor: Arc<dyn moa_core::traits::MemoryRetrievalExecutor>,
    ) -> Self {
        self.memory_retrieval_executor = tokio::sync::RwLock::new(Some(executor));
        self
    }

    /// Installs or replaces the read-only retrieval executor backing the agentic memory tools.
    pub async fn set_memory_retrieval_executor(
        &self,
        executor: Arc<dyn moa_core::traits::MemoryRetrievalExecutor>,
    ) {
        *self.memory_retrieval_executor.write().await = Some(executor);
    }

    /// Attaches the hot-path lineage handle for built-in tools.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.lineage = lineage;
        self
    }

    /// Overrides the router's policy configuration.
    #[must_use]
    pub fn with_policies(mut self, policies: ActionPolicies) -> Self {
        self.policies = policies;
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
        self.call_origin = call_origin;
        self
    }

    /// Returns the provenance class of the runtime this router serves.
    #[must_use]
    pub fn call_origin(&self) -> CallOrigin {
        self.call_origin
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
        self.call_origin.most_restrictive(session.call_origin)
    }

    /// Returns the live catalog snapshot.
    ///
    /// Every read takes one whole snapshot, so a caller that inspects several
    /// tools sees them all from the same catalog revision even if a background
    /// refresh publishes between its calls.
    pub(super) fn registry(&self) -> Arc<ToolRegistry> {
        Arc::clone(
            &self
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .registry,
        )
    }

    /// Publishes a new catalog snapshot and the prompt schemas derived from it.
    ///
    /// Both are replaced under the same publication so no caller can compile a
    /// prompt from one catalog revision and dispatch against another.
    pub(super) fn publish_registry(&self, registry: ToolRegistry) {
        *self
            .catalog
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Arc::new(ToolCatalogSnapshot::new(registry));
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
        Arc::clone(
            &self
                .catalog
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tool_schemas,
        )
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

    /// Returns the live schema revision of one registered MCP tool.
    ///
    /// Durable execution compares this with the revision pinned in its immutable
    /// capability catalog immediately before governed dispatch.
    #[must_use]
    pub fn mcp_schema_revision(&self, name: &str) -> Option<String> {
        self.registry()
            .mcp_schema_revision(name)
            .map(ToOwned::to_owned)
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
        self.mcp_credentials = McpDeploymentCredentials::from_mcp_servers(&config.mcp_servers)?;

        for server in &config.mcp_servers {
            self.mcp_servers.insert(server.name.clone(), server.clone());
        }

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
    let credential = match provider {
        "daytona" => hands.daytona_api_key.as_deref(),
        "e2b" => hands.e2b_api_key.as_deref(),
        _ => None,
    };
    credential.is_some_and(|value| !value.trim().is_empty())
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
        || match provider {
            "daytona" => hands
                .daytona_api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            "e2b" => hands
                .e2b_api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
            _ => false,
        }
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

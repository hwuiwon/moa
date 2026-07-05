//! Router construction, provider configuration, and MCP loading helpers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use moa_core::{
    CloudHandsConfig, HandProvider, LineageHandle, MoaConfig, MoaError, NullLineageHandle, Result,
    SandboxTier, SessionStore, ToolBudgetConfig, ToolOutputConfig,
};
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, EnvironmentCredentialVault, MCPCredentialProxy,
};

use crate::adapters::daytona::DaytonaHandProvider;
use crate::adapters::e2b::E2BHandProvider;
use crate::adapters::local::LocalHandProvider;
use crate::adapters::mcp::MCPClient;

use super::normalization::expand_local_path;
use super::{DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, HandRoute, ToolRegistry, ToolRouter};

impl ToolRouter {
    /// Creates a router from explicit providers and a tool registry.
    pub fn new(registry: ToolRegistry, providers: HashMap<String, Arc<dyn HandProvider>>) -> Self {
        Self {
            registry,
            providers,
            local_provider: None,
            mcp_clients: tokio::sync::RwLock::new(HashMap::new()),
            mcp_servers: HashMap::new(),
            mcp_proxy: None,
            active_hands: tokio::sync::RwLock::new(HashMap::new()),
            preferred_hand_routes: tokio::sync::RwLock::new(HashMap::new()),
            hand_leases: None,
            trusted_sandbox_files: tokio::sync::RwLock::new(HashMap::new()),
            installed_files: tokio::sync::RwLock::new(HashMap::new()),
            workspace_roots: tokio::sync::RwLock::new(HashMap::new()),
            policies: ActionPolicies::default(),
            rule_store: None,
            session_store: None,
            memory_tool_executor: tokio::sync::RwLock::new(None),
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
            ..Self::new(registry, providers)
        })
    }

    /// Creates a local router from the loaded MOA config.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
        let local_provider = Arc::new(
            LocalHandProvider::new_with_docker_detection(
                &sandbox_root,
                config.local.docker_enabled,
            )
            .await?
            .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
        );
        let local_provider_trait: Arc<dyn HandProvider> = local_provider.clone();
        let mut providers = HashMap::new();
        providers.insert(DEFAULT_PROVIDER_NAME.to_string(), local_provider_trait);

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
        let hand_routes = configured_hand_routes(config)?;
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
            sandbox_root: Some(sandbox_root),
            local_provider: Some(local_provider),
            ..Self::new(registry, providers)
        }
        .with_tool_output_config(config.tool_output.clone())
        .with_tool_budgets(config.tool_budgets.clone())
        .with_policies(ActionPolicies::from_config(config)?);

        if !config.mcp_servers.is_empty() {
            router.load_mcp_servers(config).await?;
        }

        Ok(router)
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
        executor: Arc<dyn moa_core::MemoryToolExecutor>,
    ) -> Self {
        self.memory_tool_executor = tokio::sync::RwLock::new(Some(executor));
        self
    }

    /// Installs or replaces the graph-memory executor used by built-in memory tools.
    pub async fn set_memory_tool_executor(&self, executor: Arc<dyn moa_core::MemoryToolExecutor>) {
        *self.memory_tool_executor.write().await = Some(executor);
    }

    /// Attaches the hot-path lineage handle for built-in tools.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.lineage = lineage;
        self
    }

    /// Attaches an MCP credential proxy to the router.
    #[must_use]
    pub fn with_mcp_proxy(mut self, mcp_proxy: Arc<MCPCredentialProxy>) -> Self {
        self.mcp_proxy = Some(mcp_proxy);
        self
    }

    /// Overrides the router's policy configuration.
    #[must_use]
    pub fn with_policies(mut self, policies: ActionPolicies) -> Self {
        self.policies = policies;
        self
    }

    /// Returns the ordered tool schemas for prompt compilation.
    pub fn tool_schemas(&self) -> Vec<serde_json::Value> {
        self.registry.default_tool_schemas()
    }

    /// Returns the stable registered tool names in sorted order.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self.registry.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Returns whether a tool is currently registered on the router.
    pub fn has_tool(&self, name: &str) -> bool {
        self.registry.tools.contains_key(name)
    }

    /// Returns whether the named tool provisions a hand/sandbox to execute.
    ///
    /// Delegates to the registry's execution-routing classification. Hand-routed
    /// tools are the ones hard-excluded from the sandbox-free root coordinator's
    /// tool set; built-in and MCP tools are coordinator-safe.
    pub fn tool_requires_sandbox(&self, name: &str) -> bool {
        self.registry.tool_requires_sandbox(name)
    }

    /// Returns one registered tool definition by name.
    pub fn tool_definition(&self, name: &str) -> Option<moa_core::ToolDefinition> {
        self.registry
            .tools
            .get(name)
            .map(|registered| registered.definition.clone())
    }

    /// Returns every registered tool definition in stable name order.
    pub fn tool_definitions(&self) -> Vec<moa_core::ToolDefinition> {
        let mut definitions = self
            .registry
            .tools
            .values()
            .map(|registered| registered.definition.clone())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    /// Restricts the router to an explicit set of enabled tool names.
    #[must_use]
    pub fn with_enabled_tools<I, S>(mut self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.registry.retain_only(tool_names);
        self
    }

    async fn load_mcp_servers(&mut self, config: &MoaConfig) -> Result<()> {
        let mut registry = std::mem::take(&mut self.registry);
        if config
            .mcp_servers
            .iter()
            .any(|server| server.credentials.is_some())
            && self.mcp_proxy.is_none()
        {
            let vault = Arc::new(EnvironmentCredentialVault::from_mcp_servers(
                &config.mcp_servers,
            )?);
            self.mcp_proxy = Some(Arc::new(MCPCredentialProxy::new(vault)));
        }

        for server in &config.mcp_servers {
            let client = Arc::new(MCPClient::connect(server).await?);
            for tool in client.list_tools().await? {
                registry.register_mcp_tool(&server.name, tool)?;
            }
            self.mcp_servers.insert(server.name.clone(), server.clone());
            self.mcp_clients
                .write()
                .await
                .insert(server.name.clone(), client);
        }

        registry.apply_budgets(&self.tool_budgets);
        self.registry = registry;
        Ok(())
    }
}

fn is_local_only_route(routes: &[HandRoute]) -> bool {
    routes.len() == 1
        && routes[0].provider == DEFAULT_PROVIDER_NAME
        && matches!(routes[0].tier, SandboxTier::Local)
}

fn configured_hand_routes(config: &MoaConfig) -> Result<Vec<HandRoute>> {
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
        DEFAULT_PROVIDER_NAME => Ok(HandRoute {
            provider: DEFAULT_PROVIDER_NAME.to_string(),
            tier: SandboxTier::Local,
        }),
        cloud_provider => route_for_cloud_provider(cloud_provider),
    }
}

fn route_for_cloud_provider(provider: &str) -> Result<HandRoute> {
    match provider {
        "daytona" => Ok(HandRoute {
            provider: "daytona".to_string(),
            tier: SandboxTier::Container,
        }),
        "e2b" => Ok(HandRoute {
            provider: "e2b".to_string(),
            tier: SandboxTier::MicroVM,
        }),
        other => Err(MoaError::ConfigError(format!(
            "unsupported cloud hand provider configured: {other}"
        ))),
    }
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
    use moa_core::{CloudHandsConfig, MoaConfig, SandboxTier};

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

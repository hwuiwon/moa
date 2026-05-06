//! Router construction, provider configuration, and MCP loading helpers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use moa_core::{
    HandProvider, LineageHandle, MoaConfig, MoaError, NullLineageHandle, Result, SandboxTier,
    SessionStore, ToolBudgetConfig, ToolOutputConfig,
};
use moa_security::{
    ApprovalRuleStore, EnvironmentCredentialVault, MCPCredentialProxy, ToolPolicies,
};

#[cfg(feature = "daytona")]
use crate::daytona::DaytonaHandProvider;
#[cfg(feature = "e2b")]
use crate::e2b::E2BHandProvider;
use crate::local::LocalHandProvider;
use crate::mcp::MCPClient;

use super::normalization::expand_local_path;
use super::{DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, ToolRegistry, ToolRouter};

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
            workspace_roots: tokio::sync::RwLock::new(HashMap::new()),
            policies: ToolPolicies::default(),
            rule_store: None,
            session_store: None,
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

        #[cfg(feature = "daytona")]
        if config.cloud.enabled
            && let Some(hands) = &config.cloud.hands
            && (hands
                .default_provider
                .as_deref()
                .is_some_and(|provider| provider == "daytona")
                || hands.daytona_api_key_env.is_some())
        {
            providers.insert(
                "daytona".to_string(),
                Arc::new(DaytonaHandProvider::from_config(config)?),
            );
        }

        #[cfg(feature = "e2b")]
        if config.cloud.enabled
            && let Some(hands) = &config.cloud.hands
            && (hands
                .default_provider
                .as_deref()
                .is_some_and(|provider| provider == "e2b")
                || hands.e2b_api_key_env.is_some())
        {
            providers.insert(
                "e2b".to_string(),
                Arc::new(E2BHandProvider::from_config(config)?),
            );
        }

        let mut registry = ToolRegistry::default_local();
        registry.apply_budgets(&config.tool_budgets);
        if let Some((provider, tier)) = default_cloud_provider(config)? {
            registry.retarget_hand_tools(&provider, tier);
        }

        let mut router = Self {
            sandbox_root: Some(sandbox_root),
            local_provider: Some(local_provider),
            ..Self::new(registry, providers)
        }
        .with_tool_output_config(config.tool_output.clone())
        .with_tool_budgets(config.tool_budgets.clone())
        .with_policies(ToolPolicies::from_config(config));

        if !config.mcp_servers.is_empty() {
            router.load_mcp_servers(config).await?;
        }

        Ok(router)
    }

    /// Attaches a persistent approval rule store to the router.
    #[must_use]
    pub fn with_rule_store(mut self, rule_store: Arc<dyn ApprovalRuleStore>) -> Self {
        self.rule_store = Some(rule_store);
        self
    }

    /// Attaches a session store so built-in tools can introspect session history.
    #[must_use]
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
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
    pub fn with_policies(mut self, policies: ToolPolicies) -> Self {
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
                registry.register_mcp_tool(&server.name, tool);
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

fn default_cloud_provider(config: &MoaConfig) -> Result<Option<(String, SandboxTier)>> {
    if !config.cloud.enabled {
        return Ok(None);
    }
    let provider = config
        .cloud
        .hands
        .as_ref()
        .and_then(|hands| hands.default_provider.clone())
        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
    match provider.as_str() {
        DEFAULT_PROVIDER_NAME => Ok(None),
        "daytona" => {
            #[cfg(feature = "daytona")]
            {
                Ok(Some(("daytona".to_string(), SandboxTier::Container)))
            }
            #[cfg(not(feature = "daytona"))]
            {
                Err(MoaError::Unsupported(
                    "cloud hands are configured for Daytona but the `daytona` feature is disabled"
                        .to_string(),
                ))
            }
        }
        "e2b" => {
            #[cfg(feature = "e2b")]
            {
                Ok(Some(("e2b".to_string(), SandboxTier::MicroVM)))
            }
            #[cfg(not(feature = "e2b"))]
            {
                Err(MoaError::Unsupported(
                    "cloud hands are configured for E2B but the `e2b` feature is disabled"
                        .to_string(),
                ))
            }
        }
        other => Err(MoaError::ConfigError(format!(
            "unsupported cloud hand provider configured: {other}"
        ))),
    }
}

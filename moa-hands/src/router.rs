//! Tool registry and router for built-in, hand, and future MCP tools.

use std::collections::HashMap;
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ApprovalField, ApprovalFileDiff, ApprovalRule, HandHandle, HandProvider, HandResources,
    HandSpec, McpServerConfig, MemoryStore, MoaConfig, MoaError, PolicyAction, Result, RiskLevel,
    SandboxTier, SessionMeta, ToolInvocation, ToolOutput, ToolPolicyInput, UserId,
};
use moa_security::{
    ApprovalRuleStore, EnvironmentCredentialVault, MCPCredentialProxy, PolicyCheck, ToolPolicies,
    ToolPolicyContext,
};
use serde_json::{Value, json};
use tokio::fs;
use tokio::sync::RwLock;
use uuid::Uuid;

#[cfg(feature = "daytona")]
use crate::daytona::DaytonaHandProvider;
#[cfg(feature = "e2b")]
use crate::e2b::E2BHandProvider;
use crate::local::LocalHandProvider;
use crate::mcp::{MCPClient, McpDiscoveredTool};
use crate::tools::file_read::resolve_sandbox_path;
use crate::tools::{memory, stub};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
pub enum ToolInputShape {
    Command,
    Path,
    Pattern,
    Query,
    Url,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub enum ToolDiffStrategy {
    None,
    FileWrite,
}

/// Static policy and approval metadata for one registered tool.
#[derive(Debug, Clone)]
pub struct ToolPolicySpec {
    /// Risk level shown to the user for this tool.
    pub risk_level: RiskLevel,
    /// Default action when no config override or approval rule matches.
    pub default_action: PolicyAction,
    /// Input shape used for normalization and approval summaries.
    pub input_shape: ToolInputShape,
    /// Diff strategy used for approval previews.
    pub diff_strategy: ToolDiffStrategy,
}

pub(crate) fn read_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Low,
        default_action: PolicyAction::Allow,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

pub(crate) fn write_tool_policy(
    input_shape: ToolInputShape,
    diff_strategy: ToolDiffStrategy,
) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::Medium,
        default_action: PolicyAction::RequireApproval,
        input_shape,
        diff_strategy,
    }
}

pub(crate) fn execute_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: RiskLevel::High,
        default_action: PolicyAction::RequireApproval,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Execution context passed to built-in tools.
pub struct ToolContext<'a> {
    /// Active session metadata.
    pub session: &'a SessionMeta,
    /// Shared memory store.
    pub memory_store: &'a dyn MemoryStore,
}

/// Async built-in tool handler.
#[async_trait]
pub trait BuiltInTool: Send + Sync {
    /// Returns the stable tool name.
    fn name(&self) -> &'static str;

    /// Returns the tool description shown to the model.
    fn description(&self) -> &'static str;

    /// Returns the JSON schema for tool parameters.
    fn input_schema(&self) -> Value;

    /// Returns the policy and approval metadata for the tool.
    fn policy_spec(&self) -> ToolPolicySpec;

    /// Executes the built-in tool.
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;
}

/// Prepared metadata for a concrete tool invocation.
#[derive(Debug, Clone)]
pub struct PreparedToolInvocation {
    /// Normalized policy-facing description of the invocation.
    pub policy_input: ToolPolicyInput,
    /// Result of evaluating the invocation against the active policies.
    pub policy: PolicyCheck,
    /// Suggested rule pattern for "Always Allow".
    pub always_allow_pattern: String,
    /// Structured approval fields for the local UI.
    pub approval_fields: Vec<ApprovalField>,
    /// Optional inline file diffs for the local UI.
    pub approval_diffs: Vec<ApprovalFileDiff>,
}

/// Tool execution routing target.
pub enum ToolExecution {
    /// Built-in Rust implementation.
    BuiltIn(Arc<dyn BuiltInTool>),
    /// Routed to a provisioned hand.
    Hand { provider: String, tier: SandboxTier },
    /// Reserved for MCP-backed tools.
    Mcp { server_name: String },
}

/// Static metadata for one tool.
pub struct ToolDefinition {
    /// Stable tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for parameters.
    pub schema: Value,
    /// Execution target.
    pub execution: ToolExecution,
    /// Policy and approval metadata owned by the tool definition.
    policy: ToolPolicySpec,
}

impl ToolDefinition {
    /// Converts the definition into the Anthropic tool schema shape.
    pub fn anthropic_schema(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.schema,
        })
    }
}

/// In-memory registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, ToolDefinition>,
    default_loadout: Vec<String>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            default_loadout: Vec::new(),
        }
    }

    /// Returns the canonical local registry for Step 06.
    pub fn default_local() -> Self {
        let mut registry = Self::new();
        registry.register_builtin(Arc::new(memory::MemoryReadTool));
        registry.register_builtin(Arc::new(memory::MemorySearchTool));
        registry.register_builtin(Arc::new(memory::MemoryWriteTool));
        registry.register_hand(
            "bash",
            "Run a shell command inside the active sandbox.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command to execute." },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 300, "description": "Optional timeout override in seconds." }
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            execute_tool_policy(ToolInputShape::Command),
        );
        registry.register_hand(
            "file_read",
            "Read a UTF-8 text file from the sandbox.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the sandbox." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Path),
        );
        registry.register_hand(
            "file_write",
            "Create or overwrite a UTF-8 text file inside the sandbox.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path within the sandbox." },
                    "content": { "type": "string", "description": "Full file contents to write." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::FileWrite),
        );
        registry.register_hand(
            "file_search",
            "Find files inside the sandbox using a glob pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            read_tool_policy(ToolInputShape::Pattern),
        );
        registry.register_builtin(Arc::new(stub::StubTool::new(
            "web_search",
            "Search the web for current information.",
            RiskLevel::Medium,
        )));
        registry.register_builtin(Arc::new(stub::StubTool::new(
            "web_fetch",
            "Fetch and summarize a specific web page.",
            RiskLevel::Medium,
        )));
        registry.default_loadout = vec![
            "memory_read".to_string(),
            "memory_search".to_string(),
            "memory_write".to_string(),
            "bash".to_string(),
            "file_read".to_string(),
            "file_write".to_string(),
            "file_search".to_string(),
            "web_search".to_string(),
            "web_fetch".to_string(),
        ];
        registry
    }

    /// Registers a built-in tool.
    pub fn register_builtin(&mut self, tool: Arc<dyn BuiltInTool>) {
        let name = tool.name().to_string();
        let policy = tool.policy_spec();
        self.tools.insert(
            name.clone(),
            ToolDefinition {
                name,
                description: tool.description().to_string(),
                schema: tool.input_schema(),
                execution: ToolExecution::BuiltIn(tool.clone()),
                policy,
            },
        );
    }

    /// Registers a hand-routed tool using the local provider.
    pub fn register_hand(
        &mut self,
        name: &str,
        description: &str,
        schema: Value,
        policy: ToolPolicySpec,
    ) {
        self.tools.insert(
            name.to_string(),
            ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                execution: ToolExecution::Hand {
                    provider: DEFAULT_PROVIDER_NAME.to_string(),
                    tier: SandboxTier::Local,
                },
                policy,
            },
        );
    }

    /// Registers a discovered MCP tool and adds it to the default loadout.
    pub fn register_mcp_tool(&mut self, server_name: &str, tool: McpDiscoveredTool) {
        let name = tool.name.clone();
        self.tools.insert(
            name.clone(),
            ToolDefinition {
                name: name.clone(),
                description: tool.description,
                schema: tool.input_schema,
                execution: ToolExecution::Mcp {
                    server_name: server_name.to_string(),
                },
                policy: execute_tool_policy(ToolInputShape::Json),
            },
        );
        if !self
            .default_loadout
            .iter()
            .any(|candidate| candidate == &name)
        {
            self.default_loadout.push(name);
        }
    }

    /// Retargets all hand-based tools to a different provider and sandbox tier.
    pub fn retarget_hand_tools(&mut self, provider: &str, tier: SandboxTier) {
        for tool in self.tools.values_mut() {
            if let ToolExecution::Hand {
                provider: current_provider,
                tier: current_tier,
            } = &mut tool.execution
            {
                *current_provider = provider.to_string();
                *current_tier = tier.clone();
            }
        }
    }

    /// Returns a tool definition by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    /// Returns the ordered default tool schemas for prompt compilation.
    pub fn default_tool_schemas(&self) -> Vec<Value> {
        self.default_loadout
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(ToolDefinition::anthropic_schema)
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::default_local()
    }
}

/// Routes tool invocations to built-ins, local hands, or future MCP backends.
pub struct ToolRouter {
    registry: ToolRegistry,
    memory_store: Arc<dyn MemoryStore>,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    mcp_clients: HashMap<String, Arc<MCPClient>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    policies: ToolPolicies,
    rule_store: Option<Arc<dyn ApprovalRuleStore>>,
    sandbox_root: Option<PathBuf>,
}

impl ToolRouter {
    /// Creates a router from explicit providers and a tool registry.
    pub fn new(
        registry: ToolRegistry,
        memory_store: Arc<dyn MemoryStore>,
        providers: HashMap<String, Arc<dyn HandProvider>>,
    ) -> Self {
        Self {
            registry,
            memory_store,
            providers,
            mcp_clients: HashMap::new(),
            mcp_servers: HashMap::new(),
            mcp_proxy: None,
            active_hands: RwLock::new(HashMap::new()),
            policies: ToolPolicies::default(),
            rule_store: None,
            sandbox_root: None,
        }
    }

    /// Creates a local-only router rooted at a sandbox work directory.
    pub async fn new_local(
        memory_store: Arc<dyn MemoryStore>,
        sandbox_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let provider: Arc<dyn HandProvider> = Arc::new(
            LocalHandProvider::new(sandbox_root.as_ref())
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
        );
        let mut providers = HashMap::new();
        providers.insert(DEFAULT_PROVIDER_NAME.to_string(), provider);

        Ok(Self {
            sandbox_root: Some(sandbox_root.as_ref().to_path_buf()),
            ..Self::new(ToolRegistry::default_local(), memory_store, providers)
        })
    }

    /// Creates a local router from the loaded MOA config.
    pub async fn from_config(
        config: &MoaConfig,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Result<Self> {
        let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
        let local_provider: Arc<dyn HandProvider> = Arc::new(
            LocalHandProvider::new(&sandbox_root)
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
        );
        let mut providers = HashMap::new();
        providers.insert(DEFAULT_PROVIDER_NAME.to_string(), local_provider);

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
        if let Some((provider, tier)) = default_cloud_provider(config)? {
            registry.retarget_hand_tools(&provider, tier);
        }

        let mut router = Self {
            sandbox_root: Some(sandbox_root),
            ..Self::new(registry, memory_store, providers)
        }
        .with_policies(ToolPolicies::from_config(config));

        if !config.mcp_servers.is_empty() {
            router.load_mcp_servers(config).await?;
        }

        Ok(router)
    }

    /// Attaches a persistent approval rule store to the router.
    pub fn with_rule_store(mut self, rule_store: Arc<dyn ApprovalRuleStore>) -> Self {
        self.rule_store = Some(rule_store);
        self
    }

    /// Attaches an MCP credential proxy to the router.
    pub fn with_mcp_proxy(mut self, mcp_proxy: Arc<MCPCredentialProxy>) -> Self {
        self.mcp_proxy = Some(mcp_proxy);
        self
    }

    /// Overrides the router's policy configuration.
    pub fn with_policies(mut self, policies: ToolPolicies) -> Self {
        self.policies = policies;
        self
    }

    /// Returns the ordered tool schemas for prompt compilation.
    pub fn tool_schemas(&self) -> Vec<Value> {
        self.registry.default_tool_schemas()
    }

    /// Returns the stable registered tool names in sorted order.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self.registry.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
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
            self.mcp_clients.insert(server.name.clone(), client);
        }

        self.registry = registry;
        Ok(())
    }

    /// Evaluates the policy action for a tool invocation in the current session.
    pub async fn check_policy(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PolicyCheck> {
        Ok(self.prepare_invocation(session, invocation).await?.policy)
    }

    /// Prepares a tool invocation for policy evaluation and approval rendering.
    pub async fn prepare_invocation(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PreparedToolInvocation> {
        let tool_definition = self
            .registry
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;
        let policy_input = self
            .describe_invocation(tool_definition, invocation)
            .await?;
        let rules = if let Some(rule_store) = &self.rule_store {
            rule_store
                .list_approval_rules(&session.workspace_id)
                .await?
        } else {
            Vec::new()
        };
        let policy = self.policies.check(
            &policy_input,
            &ToolPolicyContext::from_session(session),
            &rules,
        )?;

        Ok(PreparedToolInvocation {
            always_allow_pattern: approval_pattern_for(
                tool_definition.policy.input_shape,
                &policy_input.normalized_input,
            ),
            approval_fields: approval_fields_for(
                self.sandbox_root.as_deref(),
                tool_definition.policy.input_shape,
                invocation,
            ),
            approval_diffs: approval_diffs_for(
                self.sandbox_root.as_deref(),
                tool_definition.policy.diff_strategy,
                invocation,
            )
            .await?,
            policy_input,
            policy,
        })
    }

    /// Persists an approval rule for the current workspace.
    pub async fn store_approval_rule(
        &self,
        session: &SessionMeta,
        tool: &str,
        pattern: &str,
        action: PolicyAction,
        created_by: UserId,
    ) -> Result<()> {
        let Some(rule_store) = &self.rule_store else {
            return Err(MoaError::Unsupported(
                "tool router does not have an approval rule store".to_string(),
            ));
        };

        rule_store
            .upsert_approval_rule(ApprovalRule {
                id: Uuid::new_v4(),
                workspace_id: session.workspace_id.clone(),
                tool: tool.to_string(),
                pattern: pattern.to_string(),
                action,
                scope: moa_core::PolicyScope::Workspace,
                created_by,
                created_at: chrono::Utc::now(),
            })
            .await
    }

    /// Executes a single tool invocation for a session.
    pub async fn execute(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        let prepared = self.prepare_invocation(session, invocation).await?;
        match prepared.policy.action {
            PolicyAction::Allow => self.execute_authorized(session, invocation).await,
            PolicyAction::Deny => Err(MoaError::PermissionDenied(format!(
                "tool {} denied by policy",
                invocation.name
            ))),
            PolicyAction::RequireApproval => Err(MoaError::PermissionDenied(format!(
                "tool {} requires approval: {}",
                invocation.name, prepared.policy_input.input_summary
            ))),
        }
    }

    /// Executes a tool invocation after approval has already been granted.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_definition = self
            .registry
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;

        match &tool_definition.execution {
            ToolExecution::BuiltIn(tool) => {
                let ctx = ToolContext {
                    session,
                    memory_store: &*self.memory_store,
                };
                let output = tool.execute(&invocation.input, &ctx).await?;
                Ok((None, output))
            }
            ToolExecution::Hand { provider, tier } => {
                let hand = self
                    .get_or_provision_hand(provider, tier.clone(), session)
                    .await?;
                let provider_impl = self.providers.get(provider).ok_or_else(|| {
                    MoaError::ProviderError(format!("unknown hand provider: {provider}"))
                })?;
                let status = provider_impl.status(&hand).await?;
                if matches!(
                    status,
                    moa_core::HandStatus::Paused | moa_core::HandStatus::Stopped
                ) {
                    provider_impl.resume(&hand).await?;
                }
                let serialized_input = serde_json::to_string(&invocation.input)?;
                let output = provider_impl
                    .execute(&hand, &invocation.name, &serialized_input)
                    .await?;
                Ok((Some(hand_id(&hand)), output))
            }
            ToolExecution::Mcp { server_name } => {
                let server = self.mcp_servers.get(server_name).ok_or_else(|| {
                    MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
                })?;
                let client = self.mcp_clients.get(server_name).ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "missing MCP client for configured server: {server_name}"
                    ))
                })?;
                let extra_headers = if let (Some(proxy), Some(_credentials)) =
                    (&self.mcp_proxy, server.credentials.as_ref())
                {
                    let token = proxy
                        .create_session_token(&session.id, server_name, server_name)
                        .await?;
                    let headers = proxy
                        .enrich_headers(&token, server.credentials.as_ref())
                        .await?;
                    proxy.revoke_session_token(&token).await;
                    headers
                } else {
                    HashMap::new()
                };
                let output = client
                    .call_tool(&invocation.name, invocation.input.clone(), extra_headers)
                    .await?;
                Ok((None, output))
            }
        }
    }

    async fn describe_invocation(
        &self,
        definition: &ToolDefinition,
        invocation: &ToolInvocation,
    ) -> Result<ToolPolicyInput> {
        let normalized_input =
            normalized_input_for(definition.policy.input_shape, &invocation.input)?;
        Ok(ToolPolicyInput {
            tool_name: invocation.name.clone(),
            input_summary: summary_for(
                definition.policy.input_shape,
                &invocation.input,
                &normalized_input,
            ),
            normalized_input,
            risk_level: definition.policy.risk_level.clone(),
            default_action: definition.policy.default_action.clone(),
        })
    }

    async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }

        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier,
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount: None,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;

        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }
}

fn session_provider_key(session: &SessionMeta, provider: &str) -> String {
    format!("{}:{provider}", session.id)
}

fn hand_id(handle: &HandHandle) -> String {
    match handle {
        HandHandle::Local { sandbox_dir } => sandbox_dir.display().to_string(),
        HandHandle::Docker { container_id } => container_id.clone(),
        HandHandle::Daytona { workspace_id } => workspace_id.clone(),
        HandHandle::E2B { sandbox_id } => sandbox_id.clone(),
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

fn normalized_input_for(input_shape: ToolInputShape, input: &Value) -> Result<String> {
    let value = match input_shape {
        ToolInputShape::Command => required_string_field(input, "cmd")?,
        ToolInputShape::Path => required_string_field(input, "path")?,
        ToolInputShape::Pattern => required_string_field(input, "pattern")?,
        ToolInputShape::Query => required_string_field(input, "query")?,
        ToolInputShape::Url => required_string_field(input, "url")?,
        ToolInputShape::Json => serde_json::to_string(input)?,
    };

    Ok(value.trim().to_string())
}

fn summary_for(input_shape: ToolInputShape, input: &Value, normalized_input: &str) -> String {
    match input_shape {
        ToolInputShape::Command => normalized_input.to_string(),
        ToolInputShape::Path => {
            if let Some(content) = input.get("content").and_then(Value::as_str) {
                format!(
                    "Path: {normalized_input} | {} chars",
                    content.chars().count()
                )
            } else {
                format!("Path: {normalized_input}")
            }
        }
        ToolInputShape::Pattern => format!("Pattern: {normalized_input}"),
        ToolInputShape::Query => format!("Query: {normalized_input}"),
        ToolInputShape::Url => format!("URL: {normalized_input}"),
        ToolInputShape::Json => normalized_input.to_string(),
    }
}

fn approval_pattern_for(input_shape: ToolInputShape, normalized_input: &str) -> String {
    if matches!(input_shape, ToolInputShape::Command) {
        let tokens = shell_words::split(normalized_input).unwrap_or_default();
        if let Some(command) = tokens.first() {
            return if tokens.len() == 1 {
                command.clone()
            } else {
                format!("{command} *")
            };
        }
    }

    normalized_input.to_string()
}

fn approval_fields_for(
    sandbox_root: Option<&Path>,
    input_shape: ToolInputShape,
    invocation: &ToolInvocation,
) -> Vec<ApprovalField> {
    match input_shape {
        ToolInputShape::Command => {
            let command = invocation
                .input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut fields = vec![ApprovalField {
                label: "Command".to_string(),
                value: command,
            }];
            if let Some(sandbox_root) = sandbox_root {
                fields.push(ApprovalField {
                    label: "Working dir".to_string(),
                    value: sandbox_root.display().to_string(),
                });
            }
            fields
        }
        ToolInputShape::Path => {
            let mut fields = single_approval_field("Path", &invocation.input, "path");
            if invocation.name == "file_write" {
                let content_len = invocation
                    .input
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|content| content.chars().count())
                    .unwrap_or_default();
                fields.push(ApprovalField {
                    label: "Content".to_string(),
                    value: format!("{content_len} chars"),
                });
            }
            fields
        }
        ToolInputShape::Pattern => single_approval_field("Pattern", &invocation.input, "pattern"),
        ToolInputShape::Query => single_approval_field("Query", &invocation.input, "query"),
        ToolInputShape::Url => single_approval_field("URL", &invocation.input, "url"),
        ToolInputShape::Json => serde_json::to_string_pretty(&invocation.input)
            .map(|value| {
                vec![ApprovalField {
                    label: "Input".to_string(),
                    value,
                }]
            })
            .unwrap_or_default(),
    }
}

fn single_approval_field(label: &str, input: &Value, field: &str) -> Vec<ApprovalField> {
    let value = input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    vec![ApprovalField {
        label: label.to_string(),
        value,
    }]
}

async fn approval_diffs_for(
    sandbox_root: Option<&Path>,
    diff_strategy: ToolDiffStrategy,
    invocation: &ToolInvocation,
) -> Result<Vec<ApprovalFileDiff>> {
    if !matches!(diff_strategy, ToolDiffStrategy::FileWrite) {
        return Ok(Vec::new());
    }

    let Some(sandbox_root) = sandbox_root else {
        return Ok(Vec::new());
    };
    let Some(path) = invocation.input.get("path").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    let Some(content) = invocation.input.get("content").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };

    let file_path = resolve_sandbox_path(sandbox_root, path)?;
    let before = read_existing_text_file(&file_path).await?;

    Ok(vec![ApprovalFileDiff {
        path: path.to_string(),
        before,
        after: content.to_string(),
        language_hint: language_hint_for_path(path),
    }])
}

async fn read_existing_text_file(path: &Path) -> Result<String> {
    match fs::read(path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn language_hint_for_path(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(ToOwned::to_owned)
}

fn required_string_field(input: &Value, field: &str) -> Result<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            MoaError::ValidationError(format!(
                "tool input is missing required string field `{field}`"
            ))
        })
}

fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

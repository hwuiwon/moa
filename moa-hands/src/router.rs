//! Tool registry and router for built-in, hand, and future MCP tools.

use std::collections::HashMap;
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule, BuiltInTool,
    HandHandle, HandProvider, HandResources, HandSpec, McpServerConfig, MemoryStore, MoaConfig,
    MoaError, PolicyAction, Result, SandboxTier, SessionMeta, SessionStore, ToolContext,
    ToolDefinition, ToolDiffStrategy, ToolInputShape, ToolInvocation, ToolOutput, ToolPolicyInput,
    ToolPolicySpec, TraceContext, UserId, read_tool_policy, write_tool_policy,
};
use moa_security::{
    ApprovalRuleStore, EnvironmentCredentialVault, MCPCredentialProxy, PolicyCheck, ToolPolicies,
    ToolPolicyContext,
};
use opentelemetry::trace::Status;
use serde_json::{Value, json};
use tokio::fs;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

#[cfg(feature = "daytona")]
use crate::daytona::DaytonaHandProvider;
#[cfg(feature = "e2b")]
use crate::e2b::E2BHandProvider;
use crate::local::LocalHandProvider;
use crate::mcp::{MCPClient, McpDiscoveredTool};
use crate::tools::file_read::resolve_sandbox_path;
use crate::tools::{memory, session_search};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn execute_tool_policy(input_shape: ToolInputShape) -> ToolPolicySpec {
    ToolPolicySpec {
        risk_level: moa_core::RiskLevel::High,
        default_action: PolicyAction::RequireApproval,
        input_shape,
        diff_strategy: ToolDiffStrategy::None,
    }
}

/// Prepared metadata for a concrete tool invocation.
#[derive(Debug, Clone)]
pub struct PreparedToolInvocation {
    /// Normalized policy-facing description of the invocation.
    policy_input: ToolPolicyInput,
    /// Result of evaluating the invocation against the active policies.
    policy: PolicyCheck,
    /// Suggested rule pattern for "Always Allow".
    always_allow_pattern: String,
    /// Structured approval fields for the local UI.
    approval_fields: Vec<ApprovalField>,
    /// Optional inline file diffs for the local UI.
    approval_diffs: Vec<ApprovalFileDiff>,
}

impl PreparedToolInvocation {
    /// Returns the policy evaluation outcome for the invocation.
    pub fn policy(&self) -> &PolicyCheck {
        &self.policy
    }

    /// Returns the normalized policy input used for rule evaluation.
    pub fn policy_input(&self) -> &ToolPolicyInput {
        &self.policy_input
    }

    /// Returns the concise invocation summary for tool cards and errors.
    pub fn input_summary(&self) -> &str {
        &self.policy_input.input_summary
    }

    /// Builds the approval prompt for this invocation with the given request identifier.
    pub fn approval_prompt(&self, request_id: Uuid) -> ApprovalPrompt {
        ApprovalPrompt {
            request: ApprovalRequest {
                request_id,
                tool_name: self.policy_input.tool_name.clone(),
                input_summary: self.policy_input.input_summary.clone(),
                risk_level: self.policy_input.risk_level.clone(),
            },
            pattern: self.always_allow_pattern.clone(),
            parameters: self.approval_fields.clone(),
            file_diffs: self.approval_diffs.clone(),
        }
    }
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

struct RegisteredTool {
    definition: ToolDefinition,
    execution: ToolExecution,
}

impl RegisteredTool {
    fn builtin(tool: Arc<dyn BuiltInTool>) -> Self {
        Self {
            definition: tool.definition(),
            execution: ToolExecution::BuiltIn(tool),
        }
    }

    fn hand(name: &str, description: &str, schema: Value, policy: ToolPolicySpec) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                policy,
            },
            execution: ToolExecution::Hand {
                provider: DEFAULT_PROVIDER_NAME.to_string(),
                tier: SandboxTier::Local,
            },
        }
    }

    fn mcp(server_name: &str, tool: McpDiscoveredTool) -> Self {
        let name = tool.name;
        Self {
            definition: ToolDefinition {
                name: name.clone(),
                description: tool.description,
                schema: tool.input_schema,
                policy: execute_tool_policy(ToolInputShape::Json),
            },
            execution: ToolExecution::Mcp {
                server_name: server_name.to_string(),
            },
        }
    }
}

/// In-memory registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
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
        registry.register_builtin(Arc::new(session_search::SessionSearchTool));
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
        registry.default_loadout = vec![
            "memory_read".to_string(),
            "memory_search".to_string(),
            "memory_write".to_string(),
            "session_search".to_string(),
            "bash".to_string(),
            "file_read".to_string(),
            "file_write".to_string(),
            "file_search".to_string(),
        ];
        registry
    }

    /// Registers a built-in tool.
    pub fn register_builtin(&mut self, tool: Arc<dyn BuiltInTool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, RegisteredTool::builtin(tool));
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
            RegisteredTool::hand(name, description, schema, policy),
        );
    }

    /// Registers a discovered MCP tool and adds it to the default loadout.
    pub fn register_mcp_tool(&mut self, server_name: &str, tool: McpDiscoveredTool) {
        let name = tool.name.clone();
        self.tools
            .insert(name.clone(), RegisteredTool::mcp(server_name, tool));
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
        self.tools.get(name).map(|tool| &tool.definition)
    }

    /// Returns the ordered default tool schemas for prompt compilation.
    pub fn default_tool_schemas(&self) -> Vec<Value> {
        self.default_loadout
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.definition.anthropic_schema())
            .collect()
    }

    /// Retains only the registered tools whose names are present in the allowlist.
    pub fn retain_only<I, S>(&mut self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = tool_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect::<std::collections::HashSet<_>>();
        self.tools.retain(|name, _| allowed.contains(name));
        self.default_loadout.retain(|name| allowed.contains(name));
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
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_clients: HashMap<String, Arc<MCPClient>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    policies: ToolPolicies,
    rule_store: Option<Arc<dyn ApprovalRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
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
            local_provider: None,
            mcp_clients: HashMap::new(),
            mcp_servers: HashMap::new(),
            mcp_proxy: None,
            active_hands: RwLock::new(HashMap::new()),
            policies: ToolPolicies::default(),
            rule_store: None,
            session_store: None,
            sandbox_root: None,
        }
    }

    /// Creates a local-only router rooted at a sandbox work directory.
    pub async fn new_local(
        memory_store: Arc<dyn MemoryStore>,
        sandbox_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let local_provider = Arc::new(
            LocalHandProvider::new(sandbox_root.as_ref())
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT),
        );
        let provider: Arc<dyn HandProvider> = local_provider.clone();
        let mut providers = HashMap::new();
        providers.insert(DEFAULT_PROVIDER_NAME.to_string(), provider);

        Ok(Self {
            sandbox_root: Some(sandbox_root.as_ref().to_path_buf()),
            local_provider: Some(local_provider),
            ..Self::new(ToolRegistry::default_local(), memory_store, providers)
        })
    }

    /// Creates a local router from the loaded MOA config.
    pub async fn from_config(
        config: &MoaConfig,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Result<Self> {
        let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
        let local_provider = Arc::new(
            LocalHandProvider::new(&sandbox_root)
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
        if let Some((provider, tier)) = default_cloud_provider(config)? {
            registry.retarget_hand_tools(&provider, tier);
        }

        let mut router = Self {
            sandbox_root: Some(sandbox_root),
            local_provider: Some(local_provider),
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

    /// Attaches a session store so built-in tools can introspect session history.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
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

    /// Returns whether a tool is currently registered on the router.
    pub fn has_tool(&self, name: &str) -> bool {
        self.registry.tools.contains_key(name)
    }

    /// Restricts the router to an explicit set of enabled tool names.
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
        Ok(self
            .prepare_invocation(session, invocation)
            .await?
            .policy()
            .clone())
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
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &prepared.policy().action,
            );
            let result = match &prepared.policy().action {
                PolicyAction::Allow => {
                    self.execute_authorized_inner(session, invocation, None, None)
                        .await
                }
                PolicyAction::Deny => {
                    tool_span.set_attribute("moa.tool.denied", true);
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} denied by policy",
                        invocation.name
                    )))
                }
                PolicyAction::RequireApproval => Err(MoaError::PermissionDenied(format!(
                    "tool {} requires approval: {}",
                    invocation.name,
                    prepared.input_summary()
                ))),
            };

            record_tool_execution_result(&tool_span, started_at.elapsed(), &result);
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    /// Executes a tool invocation after approval has already been granted.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        self.execute_authorized_with_cancel(session, invocation, None, None)
            .await
    }

    /// Executes a tool invocation after approval has already been granted with cancellation hooks.
    pub async fn execute_authorized_with_cancel(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &prepared.policy().action,
            );
            let result = self
                .execute_authorized_inner(session, invocation, cancel_token, hard_cancel_token)
                .await;
            record_tool_execution_result(&tool_span, started_at.elapsed(), &result);
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    async fn execute_authorized_inner(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let registered_tool = self
            .registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;

        match &registered_tool.execution {
            ToolExecution::BuiltIn(tool) => {
                let ctx = ToolContext {
                    session,
                    memory_store: &*self.memory_store,
                    session_store: self.session_store.as_deref(),
                    cancel_token,
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
                let output = if provider == DEFAULT_PROVIDER_NAME {
                    let local_provider = self.local_provider.as_ref().ok_or_else(|| {
                        MoaError::ProviderError(
                            "local provider missing from tool router".to_string(),
                        )
                    })?;
                    local_provider
                        .execute_with_cancel(
                            &hand,
                            &invocation.name,
                            &serialized_input,
                            hard_cancel_token,
                        )
                        .await?
                } else if let Some(hard_cancel_token) = hard_cancel_token {
                    tokio::select! {
                        result = provider_impl.execute(&hand, &invocation.name, &serialized_input) => result?,
                        _ = hard_cancel_token.cancelled() => return Err(MoaError::Cancelled),
                    }
                } else {
                    provider_impl
                        .execute(&hand, &invocation.name, &serialized_input)
                        .await?
                };
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

fn tool_execution_span(session: &SessionMeta, invocation: &ToolInvocation) -> tracing::Span {
    let span_name = format!("execute_tool {}", invocation.name);
    let span = tracing::info_span!("tool_execution", otel.name = %span_name);
    TraceContext::from_session_meta(session, None).apply_to_span(&span);
    span.set_attribute("gen_ai.tool.name", invocation.name.clone());
    if let Some(tool_call_id) = invocation.id.as_ref() {
        span.set_attribute("gen_ai.tool.call.id", tool_call_id.clone());
    }
    if let Ok(serialized_input) = serde_json::to_string(&invocation.input) {
        span.set_attribute("moa.tool.input", truncate_tool_span_text(serialized_input));
    }
    span.set_attribute("moa.tool.denied", false);
    span
}

fn record_tool_invocation_metadata(
    span: &tracing::Span,
    session: &SessionMeta,
    execution: &ToolExecution,
    action: &PolicyAction,
) {
    TraceContext::from_session_meta(session, None).apply_to_span(span);

    let (category, sandbox_tier) = match execution {
        ToolExecution::BuiltIn(_) => ("builtin", "none"),
        ToolExecution::Hand { tier, .. } => ("hand", sandbox_tier_label(tier)),
        ToolExecution::Mcp { .. } => ("mcp", "external"),
    };

    span.set_attribute("langfuse.observation.metadata.tool_category", category);
    span.set_attribute("langfuse.observation.metadata.sandbox_tier", sandbox_tier);
    span.set_attribute(
        "langfuse.observation.metadata.approval_required",
        matches!(action, PolicyAction::RequireApproval),
    );
}

fn sandbox_tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

fn record_tool_execution_result(
    span: &tracing::Span,
    duration: Duration,
    result: &Result<(Option<String>, ToolOutput)>,
) {
    span.set_attribute("moa.tool.duration_ms", duration.as_millis() as i64);

    match result {
        Ok((_, output)) => {
            let succeeded = !output.is_error;
            span.set_attribute("moa.tool.success", succeeded);
            span.set_attribute("moa.tool.output", truncate_tool_span_text(output.to_text()));
            if output.is_error {
                span.set_status(Status::error(output.to_text()));
            }
        }
        Err(MoaError::PermissionDenied(_)) => {
            span.set_attribute("moa.tool.success", false);
        }
        Err(MoaError::Cancelled) => {
            span.set_attribute("moa.tool.success", false);
        }
        Err(error) => {
            span.set_attribute("moa.tool.success", false);
            span.set_status(Status::error(error.to_string()));
        }
    }
}

fn truncate_tool_span_text(mut value: String) -> String {
    const LIMIT: usize = 8 * 1024;
    if value.len() <= LIMIT {
        return value;
    }

    let mut truncate_at = LIMIT;
    while !value.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    value.truncate(truncate_at);
    value.push('…');
    value
}

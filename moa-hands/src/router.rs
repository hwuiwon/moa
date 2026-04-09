//! Tool registry and router for built-in, hand, and future MCP tools.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ApprovalRule, HandHandle, HandProvider, HandResources, HandSpec, MemoryStore, MoaConfig,
    MoaError, PolicyAction, Result, RiskLevel, SandboxTier, SessionMeta, ToolInvocation,
    ToolOutput, UserId,
};
use moa_security::{ApprovalRuleStore, PolicyCheck, ToolPolicies, ToolPolicyContext};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::local::LocalHandProvider;
use crate::tools::{memory, stub};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

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

    /// Returns the default risk level for the tool.
    fn risk_level(&self) -> RiskLevel;

    /// Returns whether the tool should require approval by default.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Executes the built-in tool.
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;
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
    /// Default risk level.
    pub risk_level: RiskLevel,
    /// Whether approval is required by default.
    pub requires_approval: bool,
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
            RiskLevel::High,
            true,
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
            RiskLevel::Low,
            false,
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
            RiskLevel::Medium,
            true,
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
            RiskLevel::Low,
            false,
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
        self.tools.insert(
            name.clone(),
            ToolDefinition {
                name,
                description: tool.description().to_string(),
                schema: tool.input_schema(),
                execution: ToolExecution::BuiltIn(tool.clone()),
                risk_level: tool.risk_level(),
                requires_approval: tool.requires_approval(),
            },
        );
    }

    /// Registers a hand-routed tool using the local provider.
    pub fn register_hand(
        &mut self,
        name: &str,
        description: &str,
        schema: Value,
        risk_level: RiskLevel,
        requires_approval: bool,
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
                risk_level,
                requires_approval,
            },
        );
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
    active_hands: RwLock<HashMap<String, HandHandle>>,
    policies: ToolPolicies,
    rule_store: Option<Arc<dyn ApprovalRuleStore>>,
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
            active_hands: RwLock::new(HashMap::new()),
            policies: ToolPolicies::default(),
            rule_store: None,
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

        Ok(Self::new(
            ToolRegistry::default_local(),
            memory_store,
            providers,
        ))
    }

    /// Creates a local router from the loaded MOA config.
    pub async fn from_config(
        config: &MoaConfig,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Result<Self> {
        let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
        Ok(Self::new_local(memory_store, sandbox_root)
            .await?
            .with_policies(ToolPolicies::from_config(config)))
    }

    /// Attaches a persistent approval rule store to the router.
    pub fn with_rule_store(mut self, rule_store: Arc<dyn ApprovalRuleStore>) -> Self {
        self.rule_store = Some(rule_store);
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

    /// Evaluates the policy action for a tool invocation in the current session.
    pub async fn check_policy(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PolicyCheck> {
        let rules = if let Some(rule_store) = &self.rule_store {
            rule_store
                .list_approval_rules(&session.workspace_id)
                .await?
        } else {
            Vec::new()
        };

        self.policies.check(
            &invocation.name,
            &invocation.input,
            &ToolPolicyContext::from_session(session),
            &rules,
        )
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
        let policy = self.check_policy(session, invocation).await?;
        match policy.action {
            PolicyAction::Allow => self.execute_authorized(session, invocation).await,
            PolicyAction::Deny => Err(MoaError::PermissionDenied(format!(
                "tool {} denied by policy",
                invocation.name
            ))),
            PolicyAction::RequireApproval => Err(MoaError::PermissionDenied(format!(
                "tool {} requires approval: {}",
                invocation.name, policy.input_summary
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
                let serialized_input = serde_json::to_string(&invocation.input)?;
                let output = provider_impl
                    .execute(&hand, &invocation.name, &serialized_input)
                    .await?;
                Ok((Some(hand_id(&hand)), output))
            }
            ToolExecution::Mcp { server_name } => Err(MoaError::Unsupported(format!(
                "MCP tool routing is not implemented yet for server {server_name}"
            ))),
        }
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

fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

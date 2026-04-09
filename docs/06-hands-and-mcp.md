# 06 — Hands & MCP

_HandProvider trait, Daytona/E2B/Local, MCP protocol, tool routing, lazy provisioning._

---

## Core principle

Hands are **cattle, not pets**. They are provisioned lazily on the first tool call, paused when idle, and destroyed when the session ends. Credentials never enter the sandbox.

The brain interacts with hands exclusively through `execute(tool_name, input) → output`. The brain does not know or care whether the hand is a Docker container, a microVM, a local process, or an MCP server.

---

## HandProvider implementations

### LocalHandProvider (zero-setup)

For TUI on-ramp. No Docker, no cloud.

```rust
pub struct LocalHandProvider {
    work_dir: PathBuf,
    docker_available: bool,
    allowed_commands: CommandPolicy,
}

impl LocalHandProvider {
    pub fn new(work_dir: PathBuf) -> Self {
        let docker_available = Command::new("docker").arg("info").output().is_ok();
        Self {
            work_dir,
            docker_available,
            allowed_commands: CommandPolicy::default_local(),
        }
    }
}

#[async_trait]
impl HandProvider for LocalHandProvider {
    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        match spec.sandbox_tier {
            SandboxTier::None | SandboxTier::Local => {
                // Just create a working directory
                let sandbox_dir = self.work_dir.join(format!("sandbox-{}", Uuid::new_v4()));
                fs::create_dir_all(&sandbox_dir).await?;
                Ok(HandHandle::local(sandbox_dir))
            }
            SandboxTier::Container if self.docker_available => {
                // Start a local Docker container
                let container_id = docker_run(&spec).await?;
                Ok(HandHandle::docker(container_id))
            }
            SandboxTier::Container => {
                // Docker not available, fall back to local with warning
                tracing::warn!("Docker not available, running in local sandbox");
                let sandbox_dir = self.work_dir.join(format!("sandbox-{}", Uuid::new_v4()));
                fs::create_dir_all(&sandbox_dir).await?;
                Ok(HandHandle::local(sandbox_dir))
            }
            SandboxTier::MicroVM => {
                Err(Error::UnsupportedLocally("MicroVM sandboxes require E2B (cloud mode)"))
            }
        }
    }
    
    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                execute_local(sandbox_dir, tool, input, &self.allowed_commands).await
            }
            HandHandle::Docker { container_id } => {
                execute_docker(container_id, tool, input).await
            }
            _ => unreachable!(),
        }
    }
    
    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                fs::remove_dir_all(sandbox_dir).await.ok();
            }
            HandHandle::Docker { container_id } => {
                Command::new("docker").args(["rm", "-f", container_id]).output().await?;
            }
            _ => {}
        }
        Ok(())
    }
}
```

### DaytonaHandProvider (default cloud)

```rust
pub struct DaytonaHandProvider {
    client: DaytonaClient,
    default_image: String,
    idle_timeout: Duration,
}

#[async_trait]
impl HandProvider for DaytonaHandProvider {
    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        let workspace = self.client.create_workspace(DaytonaCreateRequest {
            image: spec.image.unwrap_or(self.default_image.clone()),
            resources: DaytonaResources {
                cpu: spec.resources.cpu_millicores,
                memory_mb: spec.resources.memory_mb,
            },
            env: spec.env,
            auto_stop_interval: self.idle_timeout,
            ..Default::default()
        }).await?;
        
        Ok(HandHandle::daytona(workspace.id))
    }
    
    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        let workspace_id = handle.daytona_id()?;
        
        // Ensure workspace is running (auto-resume if stopped)
        self.client.ensure_running(workspace_id).await?;
        
        let result = self.client.exec(workspace_id, &ExecRequest {
            command: format_tool_command(tool, input),
            timeout: Duration::from_secs(300),
        }).await?;
        
        Ok(ToolOutput {
            stdout: result.stdout,
            stderr: result.stderr,
            exit_code: result.exit_code,
            duration: result.duration,
        })
    }
    
    async fn pause(&self, handle: &HandHandle) -> Result<()> {
        self.client.stop_workspace(handle.daytona_id()?).await
    }
    
    async fn resume(&self, handle: &HandHandle) -> Result<()> {
        self.client.start_workspace(handle.daytona_id()?).await
    }
    
    async fn destroy(&self, handle: &HandHandle) -> Result<()> {
        self.client.delete_workspace(handle.daytona_id()?).await
    }
}
```

### E2BHandProvider (security-critical)

Same interface, but provisions Firecracker microVMs via E2B's API. Used when `SandboxTier::MicroVM` is specified.

---

## Tool routing

The brain doesn't call hands directly. It calls the `ToolRouter`, which decides where to execute:

```rust
pub struct ToolRouter {
    providers: HashMap<String, Arc<dyn HandProvider>>,
    mcp_clients: HashMap<String, MCPClient>,
    tool_registry: ToolRegistry,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    policies: ToolPolicies,
}

impl ToolRouter {
    pub async fn execute(
        &self, 
        tool_name: &str, 
        input: &str, 
        session_ctx: &SessionContext
    ) -> Result<ToolOutput> {
        let tool_def = self.tool_registry.get(tool_name)
            .ok_or(Error::UnknownTool(tool_name.to_string()))?;
        
        // Check permissions
        self.policies.check(tool_name, input, session_ctx)?;
        
        match &tool_def.execution {
            ToolExecution::BuiltIn(handler) => {
                // Built-in tools (memory_search, memory_write, etc.)
                handler.execute(input, session_ctx).await
            }
            ToolExecution::Hand { provider, tier } => {
                // Get or provision a hand
                let hand = self.get_or_provision_hand(provider, *tier, session_ctx).await?;
                let provider = self.providers.get(provider.as_str())
                    .ok_or(Error::ProviderNotFound(provider.clone()))?;
                provider.execute(&hand, tool_name, input).await
            }
            ToolExecution::MCP { server_name } => {
                // Route to MCP server
                let client = self.mcp_clients.get(server_name)
                    .ok_or(Error::MCPServerNotFound(server_name.clone()))?;
                client.call_tool(tool_name, input).await
            }
        }
    }
    
    /// Lazy provisioning: only create hand when first tool call arrives
    async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        ctx: &SessionContext,
    ) -> Result<HandHandle> {
        let key = format!("{}:{}", ctx.session_id, provider);
        
        // Check if already provisioned
        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }
        
        // Provision new hand
        let provider_impl = self.providers.get(provider)
            .ok_or(Error::ProviderNotFound(provider.to_string()))?;
        let handle = provider_impl.provision(HandSpec {
            sandbox_tier: tier,
            workspace_mount: ctx.workspace_path.clone(),
            ..Default::default()
        }).await?;
        
        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }
}
```

## Tool registry

Tools are registered from three sources:

1. **Built-in tools**: memory_search, memory_write, web_search, web_fetch
2. **Hand tools**: bash, file_read, file_write, file_search (routed to hand)
3. **MCP tools**: Discovered from connected MCP servers at startup

```rust
pub struct ToolRegistry {
    tools: HashMap<String, ToolDefinition>,
}

pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,  // JSON Schema for parameters
    pub execution: ToolExecution,
    pub risk_level: RiskLevel,
    pub requires_approval: bool,     // default, overridable by rules
}

pub enum ToolExecution {
    BuiltIn(Arc<dyn Tool>),
    Hand { provider: String, tier: SandboxTier },
    MCP { server_name: String },
}
```

---

## MCP integration

### MCP as the primary external tool protocol

Every external integration (GitHub, databases, browsers, calendars) is accessed via MCP servers. The brain calls MCP tools the same way it calls any other tool — through the `ToolRouter`.

### MCP client

```rust
pub struct MCPClient {
    server_url: String,
    transport: MCPTransport,  // stdio | sse | streamable-http
    tools: Vec<MCPToolDef>,   // discovered at connection time
    credential_proxy: Arc<CredentialProxy>,
}

impl MCPClient {
    pub async fn connect(config: MCPServerConfig) -> Result<Self> {
        let transport = match &config.transport {
            MCPTransportConfig::Stdio { command, args } => {
                MCPTransport::stdio(command, args).await?
            }
            MCPTransportConfig::SSE { url } => {
                MCPTransport::sse(url).await?
            }
            MCPTransportConfig::HTTP { url } => {
                MCPTransport::http(url).await?
            }
        };
        
        // Discover available tools
        let tools = transport.send_request("tools/list", json!({})).await?;
        
        Ok(Self {
            server_url: config.url,
            transport,
            tools: serde_json::from_value(tools)?,
            credential_proxy: Arc::new(CredentialProxy::new(config.credentials)),
        })
    }
    
    pub async fn call_tool(&self, name: &str, input: &str) -> Result<ToolOutput> {
        // Inject credentials via proxy (never expose to brain)
        let enriched_input = self.credential_proxy.enrich(name, input).await?;
        
        let result = self.transport.send_request("tools/call", json!({
            "name": name,
            "arguments": enriched_input
        })).await?;
        
        Ok(ToolOutput::from_mcp_response(result))
    }
}
```

### MCP server configuration

```toml
# ~/.moa/config.toml
[[mcp_servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN_ENV = "GITHUB_TOKEN" }  # credential reference, not value

[[mcp_servers]]
name = "filesystem"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "--root", "."]

[[mcp_servers]]
name = "custom-api"
transport = "sse"
url = "https://my-mcp-server.example.com/sse"
credentials = { type = "bearer", token_env = "CUSTOM_API_TOKEN" }
```

### Credential proxy (security)

The MCP credential proxy sits between the brain and external MCP servers. It:

1. Receives tool calls with session-scoped opaque tokens
2. Looks up real credentials in the vault
3. Injects credentials into the MCP request
4. Forwards to the MCP server
5. Returns results to the brain (credentials stripped)

The brain never sees real API keys, OAuth tokens, or passwords. See `08-security.md` for details.

---

## Default tool loadout

At session start, the brain gets a curated set of tools. The full registry may have hundreds of MCP tools, but only a relevant subset is active per session.

```rust
fn default_tool_loadout(workspace: &Workspace) -> Vec<String> {
    let mut tools = vec![
        // Always available (built-in)
        "memory_search",
        "memory_write",
        
        // Standard hand tools
        "bash",
        "file_read",
        "file_write",
        "file_search",
        "web_search",
        "web_fetch",
    ];
    
    // Add workspace-configured MCP tools
    for mcp in &workspace.mcp_servers {
        for tool in &mcp.enabled_tools {
            tools.push(tool);
        }
    }
    
    // Cap at 30 tools (context confusion beyond this)
    tools.truncate(30);
    tools
}
```

The brain can request additional tools mid-session via a `tool_request` mechanism, but the loadout itself stays fixed for cache optimization (see `07-context-pipeline.md`).

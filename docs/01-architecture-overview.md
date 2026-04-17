# 01 — Architecture Overview

_System diagram, component interactions, trait hierarchy, Rust workspace layout._

---

## System diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER INTERFACES                          │
│ Telegram │ Slack │ Discord │ Desktop App │ CLI (exec) │ Future: Web │
└─────┬─────┴───┬───┴────┬────┴──┬──┴──────┬─────┴───────────────┘
      │         │        │       │         │
      ▼         ▼        ▼       ▼         ▼
┌─────────────────────────────────────────────────────────────────┐
│                    GATEWAY / ROUTER                              │
│  Normalizes inbound messages → routes to BrainOrchestrator      │
│  Receives outbound events → renders per-platform                │
│  PlatformAdapter trait per channel                               │
└───────────────────────────┬─────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                   BRAIN ORCHESTRATOR                             │
│  Cloud: Temporal.io workflows + Fly.io Machines                 │
│  Local: LocalOrchestrator (tokio tasks + mpsc channels)         │
│                                                                  │
│  Responsibilities:                                               │
│  - Spawn/recover brains from session log                        │
│  - Route signals (approvals, stop, queue) to running brains     │
│  - Manage brain lifecycle (health check, crash recovery)        │
│  - Schedule cron jobs (memory consolidation, skill improvement) │
└────────┬──────────────────┬─────────────────┬───────────────────┘
         │                  │                 │
         ▼                  ▼                 ▼
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│   BRAIN A    │  │   BRAIN B    │  │   BRAIN C    │
│  (stateless  │  │  (stateless  │  │  (stateless  │
│   harness)   │  │   harness)   │  │   harness)   │
└──┬───┬───┬───┘  └──┬───┬───┬───┘  └──┬───┬───┬───┘
   │   │   │        │   │   │        │   │   │
   ▼   ▼   ▼        ▼   ▼   ▼        ▼   ▼   ▼
┌──────────────────────────────────────────────────────┐
│                    HANDS (pluggable)                  │
│  Daytona containers │ E2B microVMs │ MCP servers │..│
│  execute(name, input) → output                       │
└──────────────────────────────────────────────────────┘
         │               │               │
         ▼               ▼               ▼
┌──────────────────────────────────────────────────────┐
│                 DURABLE SESSION LOG                   │
│             Postgres (append-only events)             │
│            getEvents() │ emitEvent() │ wake()        │
└──────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────────────────────────────────────┐
│                   MEMORY (file-wiki)                  │
│  User wiki │ Workspace wiki │ Search index │ Skills  │
│  Consolidation cron │ Git-branch concurrent writes   │
└──────────────────────────────────────────────────────┘
```

---

## Core trait hierarchy

These traits define the stable interfaces between components. Implementations can be swapped (cloud ↔ local) without changing the brain logic.

```rust
// ─── Brain Orchestration ───

#[async_trait]
pub trait BrainOrchestrator: Send + Sync {
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle>;
    async fn resume_session(&self, session_id: SessionId) -> Result<SessionHandle>;
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()>;
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;
    async fn observe(&self, session_id: SessionId, level: ObserveLevel) -> Result<EventStream>;
    async fn schedule_cron(&self, spec: CronSpec) -> Result<CronHandle>;
}

pub enum SessionSignal {
    QueueMessage(UserMessage),
    SoftCancel,
    HardCancel,
    ApprovalDecided { request_id: Uuid, decision: ApprovalDecision },
}

pub enum ObserveLevel {
    Summary,   // checkpoints, errors, start/end
    Normal,    // + tool calls, thinking summaries
    Verbose,   // + streaming tokens, full tool results
}

// Observation semantics:
// - `observe()` replays durable session history first
// - then tails live events from the active orchestrator, if one exists
// - if the live tail lags beyond its in-memory buffer, the stream returns an
//   error so the caller can reopen from durable history instead of silently
//   missing events

// ─── Session Store ───

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId>;
    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum>;
    async fn get_events(&self, session_id: SessionId, range: EventRange) -> Result<Vec<EventRecord>>;
    async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta>;
    async fn update_status(&self, session_id: SessionId, status: SessionStatus) -> Result<()>;
    async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>>;
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>>;
}

pub struct EventRange {
    pub from_seq: Option<SequenceNum>,
    pub to_seq: Option<SequenceNum>,
    pub event_types: Option<Vec<EventType>>,
    pub limit: Option<usize>,
}

// ─── Hand Provider ───

#[async_trait]
pub trait HandProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    async fn provision(&self, spec: HandSpec) -> Result<HandHandle>;
    async fn execute(&self, handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput>;
    async fn status(&self, handle: &HandHandle) -> Result<HandStatus>;
    async fn pause(&self, handle: &HandHandle) -> Result<()>;
    async fn resume(&self, handle: &HandHandle) -> Result<()>;
    async fn destroy(&self, handle: &HandHandle) -> Result<()>;
}

pub struct HandSpec {
    pub sandbox_tier: SandboxTier,
    pub image: Option<String>,
    pub resources: HandResources,
    pub env: HashMap<String, String>,
    pub workspace_mount: Option<PathBuf>,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
}

pub enum SandboxTier {
    None,       // Tier 0: no sandbox, brain-only ops
    Container,  // Tier 1: Docker/Daytona (default for cloud)
    MicroVM,    // Tier 2: Firecracker/E2B (untrusted code)
    Local,      // Direct execution on host (desktop app / CLI on-ramp)
}

// ─── LLM Provider ───

#[async_trait]
pub trait LLMProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> ModelCapabilities;
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream>;
}

pub struct ModelCapabilities {
    pub model_id: String,
    pub context_window: usize,
    pub max_output: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_prefix_caching: bool,
    pub cache_ttl: Option<Duration>,
    pub tool_call_format: ToolCallFormat,
    pub pricing: TokenPricing,
}

pub struct TokenPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cached_input_per_mtok: Option<f64>,
}

// ─── Platform Adapter ───

#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform(&self) -> Platform;
    fn capabilities(&self) -> PlatformCapabilities;
    async fn start(&self, event_tx: mpsc::Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId>;
    async fn edit(&self, msg_id: &MessageId, msg: OutboundMessage) -> Result<()>;
    async fn delete(&self, msg_id: &MessageId) -> Result<()>;
}

pub struct PlatformCapabilities {
    pub max_message_length: usize,
    pub supports_inline_buttons: bool,
    pub supports_modals: bool,
    pub supports_ephemeral: bool,
    pub supports_threads: bool,
    pub supports_code_blocks: bool,
    pub supports_edit: bool,
    pub supports_reactions: bool,
    pub min_edit_interval: Duration,
}

// ─── Memory ───

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn search(&self, query: &str, scope: MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>>;
    async fn search_with_mode(
        &self,
        query: &str,
        scope: MemoryScope,
        limit: usize,
        mode: MemorySearchMode,
    ) -> Result<Vec<MemorySearchResult>>;
    async fn read_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage>;
    async fn write_page(&self, scope: MemoryScope, path: &MemoryPath, page: WikiPage) -> Result<()>;
    async fn delete_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<()>;
    async fn list_pages(&self, scope: MemoryScope, filter: Option<PageType>) -> Result<Vec<PageSummary>>;
    async fn get_index(&self, scope: MemoryScope) -> Result<String>;
    async fn rebuild_search_index(&self, scope: MemoryScope) -> Result<()>;
}

pub enum MemoryScope {
    User(UserId),
    Workspace(WorkspaceId),
}

pub enum MemorySearchMode {
    Hybrid,
    Keyword,
    Semantic,
}

// ─── Context Compiler ───

pub trait ContextProcessor: Send + Sync {
    fn name(&self) -> &str;
    fn stage(&self) -> u8;
    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput>;
}

pub struct WorkingContext {
    pub messages: Vec<ContextMessage>,
    pub token_count: usize,
    pub token_budget: usize,
    pub model_capabilities: ModelCapabilities,
    pub session_id: SessionId,
    pub user_id: UserId,
    pub workspace_id: WorkspaceId,
    pub cache_breakpoints: Vec<usize>,
    pub metadata: HashMap<String, serde_json::Value>,
}

pub struct ProcessorOutput {
    pub tokens_added: usize,
    pub tokens_removed: usize,
    pub items_included: Vec<String>,
    pub items_excluded: Vec<String>,
    pub duration: Duration,
}
```

---

## Rust workspace layout

```
moa/
├── Cargo.toml                    # Workspace root
├── moa-core/                     # Core types, traits, config
│   └── src/ { lib, types, traits, config, error }
├── moa-brain/                    # Brain harness loop + context pipeline
│   └── src/ { lib, harness, pipeline/*, compaction }
├── moa-session/                  # Session store (Postgres)
│   └── src/ { lib, store, schema, queries }
├── moa-memory/                   # File-wiki memory system
│   └── src/ { lib, wiki, index, fts, consolidation, branching, ingest }
├── moa-hands/                    # Hand providers
│   └── src/ { lib, local, daytona, e2b, mcp, router }
├── moa-providers/                # LLM provider implementations
│   └── src/ { lib, anthropic, openai, gemini, common }
├── moa-orchestrator/             # Brain orchestration
│   └── src/ { lib, temporal, local, cron }
├── moa-gateway/                  # Messaging gateway
│   └── src/ { lib, telegram, slack, discord, renderer, approval }
├── moa-desktop/                  # Desktop application (GPUI)
│   └── src/ { main, app, panels/*, components/*, services/* }
├── moa-cli/                      # CLI entry point
│   └── src/ { main, exec }
├── moa-security/                 # Credential vault + sandbox policies
│   └── src/ { lib, vault, mcp_proxy, policies, injection }
└── moa-skills/                   # Skill management
    └── src/ { lib, format, distiller, registry, improver }
```

---

## Two runtime modes

The same trait hierarchy supports both cloud and local operation:

### Cloud mode (`moa --cloud`)

```
BrainOrchestrator  →  TemporalOrchestrator
SessionStore       →  PostgresSessionStore
HandProvider       →  DaytonaHandProvider / E2BHandProvider
PlatformAdapter    →  TelegramAdapter, SlackAdapter, DiscordAdapter
CredentialVault    →  VaultCredentialStore (HashiCorp Vault)
```

### Local mode (`moa-desktop` / `moa exec`)

```
BrainOrchestrator  →  LocalOrchestrator (tokio tasks + mpsc channels)
SessionStore       →  PostgresSessionStore
HandProvider       →  LocalHandProvider (direct exec or local Docker)
Local client       →  Desktop app or CLI
CredentialVault    →  FileCredentialStore (~/.moa/vault.enc)
```

The brain harness code (`moa-brain/src/harness.rs`) is identical in both modes. It only interacts with traits, never with concrete implementations.

---

## Data flow for a typical request

```
1.  User sends "deploy to staging" via Telegram
2.  TelegramAdapter normalizes → InboundMessage { user_id, workspace_id, text }
3.  Gateway routes to BrainOrchestrator.start_session() or .signal(QueueMessage)
4.  Orchestrator spawns/signals a Brain workflow
5.  Brain.wake(session_id) → loads pending events from SessionStore
6.  Brain runs context pipeline:
    a. IdentityProcessor → system prompt
    b. ToolDefinitionProcessor → available tools
    c. SkillInjector → "deploy-to-fly" skill metadata
    d. MemoryRetriever → workspace deploy conventions
    e. HistoryCompiler → recent turns
    f. CacheOptimizer → stable prefix markers
7.  Brain calls LLMProvider.complete(compiled_context)
8.  LLM responds with tool_call: bash("fly deploy --app staging")
9.  Brain emits ApprovalRequested event to SessionStore
10. Orchestrator routes approval to Gateway → Telegram sends inline buttons
11. User taps [Allow Once]
12. TelegramAdapter sends ApprovalDecided signal to Orchestrator
13. Orchestrator routes signal to Brain
14. Brain provisions hand: HandProvider.provision(Tier1)
15. Brain executes: hand.execute("bash", "fly deploy --app staging")
16. Hand returns output
17. Brain emits ToolResult event, continues LLM loop
18. LLM says "Deployment complete. Staging is now running v2.3.1."
19. Brain emits BrainResponse event
20. Brain checks: should I write memory? → Yes, writes deploy skill update
21. Brain emits SessionCompleted
22. Gateway renders final message to Telegram
```

# 02 — Brain Orchestration

_Temporal workflows, Fly.io hosting, local runtime mode, brain lifecycle._

---

## Overview

Brains are stateless harness loops. They wake from a session log, compile context, call an LLM, route tool calls to hands, and emit events. Nothing in a brain needs to survive a crash — the session log is the recovery mechanism.

Two orchestrator implementations share the same `BrainOrchestrator` trait:

| Mode | Orchestrator | When |
|---|---|---|
| Cloud | `TemporalOrchestrator` | Production: `moa --cloud` |
| Local | `LocalOrchestrator` | Zero-setup: `moa` |

---

## Cloud mode: Temporal.io

### Workflow structure

Each session maps to one Temporal workflow:

```rust
// Pseudocode for the Temporal workflow definition
#[workflow]
async fn session_workflow(ctx: WorkflowContext, session_id: SessionId) -> Result<()> {
    // 1. Wake: load session metadata
    let session = activity!(get_session, session_id).await?;
    
    loop {
        // 2. Run one brain turn (activity)
        let turn_result = activity!(brain_turn, session_id).await?;
        
        match turn_result {
            TurnResult::Continue => continue,
            TurnResult::Complete => break,
            TurnResult::NeedsApproval(request) => {
                // 3. Wait for human signal (indefinitely)
                emit_event(session_id, Event::ApprovalRequested(request));
                let decision = ctx.wait_for_signal::<ApprovalDecided>().await;
                emit_event(session_id, Event::ApprovalDecided(decision));
            }
            TurnResult::Error(e) => {
                emit_event(session_id, Event::Error(e));
                break;
            }
        }
        
        // 4. Check for queued messages
        if let Some(msg) = ctx.try_receive_signal::<QueuedMessage>() {
            emit_event(session_id, Event::QueuedMessage(msg));
        }
        
        // 5. Check for cancellation
        if ctx.try_receive_signal::<CancelRequested>().is_some() {
            emit_event(session_id, Event::SessionCancelled);
            break;
        }
    }
    
    Ok(())
}
```

### Activity: `brain_turn`

One turn of the brain loop, implemented as a Temporal activity:

```rust
#[activity]
async fn brain_turn(session_id: SessionId) -> Result<TurnResult> {
    let store = get_session_store();
    let memory = get_memory_store();
    let llm = get_llm_provider();
    let hands = get_hand_router();
    
    // 1. Load recent events
    let events = store.get_events(session_id, EventRange::recent(100)).await?;
    
    // 2. Compile context through 7-stage pipeline
    let mut ctx = WorkingContext::new(session_id, llm.capabilities());
    for processor in get_pipeline() {
        processor.process(&mut ctx)?;
    }
    
    // 3. Call LLM
    let response = llm.complete(ctx.into_request()).await?;
    
    // 4. Process response
    for block in response.content {
        match block {
            ContentBlock::Text(text) => {
                store.emit_event(session_id, Event::BrainResponse(text)).await?;
            }
            ContentBlock::ToolCall(call) => {
                // Check if approval needed
                if needs_approval(&call, &session.permissions) {
                    return Ok(TurnResult::NeedsApproval(call.into()));
                }
                
                // Execute tool
                store.emit_event(session_id, Event::ToolCall(call.clone())).await?;
                let result = hands.execute(&call.tool, &call.input).await;
                store.emit_event(session_id, Event::ToolResult(result)).await?;
            }
        }
    }
    
    // 5. Check if done
    if response.stop_reason == StopReason::EndTurn {
        // Consider writing memory
        maybe_write_memory(session_id, &events, &response).await?;
        return Ok(TurnResult::Complete);
    }
    
    Ok(TurnResult::Continue)
}
```

### Temporal signals mapping

| User action | Temporal signal | Handler |
|---|---|---|
| Send message while running | `QueuedMessage` | Appended to session, processed next turn |
| Tap "Allow Once" | `ApprovalDecided { decision: AllowOnce }` | Unblocks waiting workflow |
| Tap "Always Allow" | `ApprovalDecided { decision: AlwaysAllow }` | Unblocks + stores permission rule |
| Tap "Deny" | `ApprovalDecided { decision: Deny }` | Unblocks + brain handles denial |
| Tap "Stop" | `CancelRequested { mode: Soft }` | Complete current tool, then stop |
| Force stop | `CancelRequested { mode: Hard }` | Abort immediately |

### Sub-brains via child workflows

When the main brain needs to dispatch parallel specialist work:

```rust
// Main brain spawns a child workflow for a sub-task
let child = ctx.spawn_child_workflow(
    "session_workflow",
    ChildSessionRequest {
        parent_session_id: session_id,
        task: "Research current Fly.io pricing",
        tools: vec!["web_search", "web_fetch"],
        max_turns: 10,
    }
).await?;

// Wait for child to complete and get summary
let result = child.result().await?;
```

### Temporal configuration

```toml
# ~/.moa/config.toml (cloud section)
[temporal]
address = "your-namespace.tmprl.cloud:7233"
namespace = "moa-production"
task_queue = "moa-brains"
api_key = "..." # or via TEMPORAL_API_KEY env var

# Workflow settings
workflow_execution_timeout = "24h"
activity_start_to_close_timeout = "5m"
activity_retry_max_attempts = 3
activity_retry_initial_interval = "1s"
activity_retry_backoff_coefficient = 2.0

[flyio]
api_token = "..." # or via FLY_API_TOKEN
app_name = "moa-brains"
region = "iad"  # primary region
min_machines = 0
max_machines = 10
machine_size = "shared-cpu-1x"
memory_mb = 256
auto_suspend_timeout = "5m"
```

### Hosting: Fly.io Machines

Brains run as Fly.io Machines. Key behaviors:

- **Auto-suspend**: Idle brains suspend after 5 minutes (only storage cost: $0.15/GB/month)
- **Auto-resume**: Sub-second resume when a new message arrives
- **Scale-to-zero**: No sessions active → no machines running
- **Multi-region**: Deploy close to users for lower latency
- **Single binary**: The `moa-cli` binary is the Machine entrypoint

Deployment:
```bash
# Build the single binary
cargo build --release --features cloud,temporal

# Deploy to Fly.io
fly deploy --image moa:latest
```

---

## Local mode: LocalOrchestrator

The local orchestrator provides the same `BrainOrchestrator` interface without any cloud dependencies.

### Implementation

```rust
pub struct LocalOrchestrator {
    sessions: Arc<RwLock<HashMap<SessionId, LocalBrainHandle>>>,
    session_store: Arc<dyn SessionStore>,
    memory_store: Arc<dyn MemoryStore>,
    llm_provider: Arc<dyn LLMProvider>,
    tool_router: Arc<ToolRouter>,
}

struct LocalBrainHandle {
    signal_tx: mpsc::Sender<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
    finished: Arc<AtomicBool>,
}

#[async_trait]
impl BrainOrchestrator for LocalOrchestrator {
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle> {
        let session_id = self.session_store.create_session(req.into()).await?;
        let (signal_tx, signal_rx) = mpsc::channel(32);
        let (event_tx, _) = broadcast::channel(256);
        let (runtime_tx, _) = broadcast::channel(512);
        let cancel_token = CancellationToken::new();
        let hard_cancel_token = CancellationToken::new();
        let finished = Arc::new(AtomicBool::new(false));
        
        // Spawn the session task, then supervise its exit.
        let task = tokio::spawn({
            let store = self.session_store.clone();
            let memory = self.memory_store.clone();
            let llm = self.llm_provider.clone();
            let tools = self.tool_router.clone();
            let event_tx = event_tx.clone();
            let runtime_tx = runtime_tx.clone();
            let cancel_token = cancel_token.clone();
            let hard_cancel_token = hard_cancel_token.clone();
            
            async move {
                run_session_task(
                    session_id,
                    store,
                    memory,
                    llm,
                    tools,
                    signal_rx,
                    event_tx,
                    runtime_tx,
                    cancel_token,
                    hard_cancel_token,
                ).await
            }
        });

        tokio::spawn({
            let store = self.session_store.clone();
            let tools = self.tool_router.clone();
            let finished = finished.clone();

            async move {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        store.emit_event(session_id, Event::Error {
                            message: error.to_string(),
                            recoverable: false,
                        }).await?;
                        store.update_status(session_id, SessionStatus::Failed).await?;
                    }
                    Err(join_error) => {
                        store.emit_event(session_id, Event::Error {
                            message: join_error.to_string(),
                            recoverable: false,
                        }).await?;
                        store.update_status(session_id, SessionStatus::Failed).await?;
                    }
                }

                tools.destroy_session_hands(&session_id).await;
                finished.store(true, Ordering::SeqCst);
                Ok::<(), Error>(())
            }
        });
        
        let handle = LocalBrainHandle {
            signal_tx: signal_tx.clone(),
            event_tx: event_tx.clone(),
            runtime_tx: runtime_tx.clone(),
            cancel_token,
            hard_cancel_token,
            finished,
        };
        
        self.sessions.write().await.insert(session_id, handle);
        
        Ok(SessionHandle { session_id })
    }
    
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        let sessions = self.sessions.read().await;
        let handle = sessions.get(&session_id)
            .ok_or(Error::SessionNotFound(session_id))?;
        handle.signal_tx.send(signal).await
            .map_err(|_| Error::SessionClosed(session_id))?;
        Ok(())
    }
    
    async fn observe(&self, session_id: SessionId, _level: ObserveLevel) 
        -> Result<EventStream> 
    {
        let history = self.session_store
            .get_events(session_id, EventRange::all())
            .await?;
        let sessions = self.sessions.read().await;
        if let Some(handle) = sessions.get(&session_id) {
            return Ok(EventStream::from_history_and_broadcast(
                history,
                handle.event_tx.subscribe(),
            ));
        }
        Ok(EventStream::from_events(history))
    }
    
    async fn schedule_cron(&self, spec: CronSpec) -> Result<CronHandle> {
        // Use tokio-cron-scheduler for local cron jobs
        let scheduler = JobScheduler::new().await?;
        let job = Job::new_async(spec.schedule.as_str(), move |_uuid, _lock| {
            let task = spec.task.clone();
            Box::pin(async move { task.run().await })
        })?;
        scheduler.add(job).await?;
        scheduler.start().await?;
        Ok(CronHandle::Local(scheduler))
    }
}
```

### The brain loop (shared between local and Temporal)

```rust
async fn brain_loop(
    session_id: SessionId,
    store: Arc<dyn SessionStore>,
    memory: Arc<dyn MemoryStore>,
    llm: Arc<dyn LLMProvider>,
    hands: Arc<dyn HandProvider>,
    mut signal_rx: mpsc::Receiver<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
) -> Result<()> {
    let pipeline = build_pipeline(llm.capabilities());
    
    store.update_status(session_id, SessionStatus::Running).await?;
    
    loop {
        // Check for signals before each turn
        while let Ok(signal) = signal_rx.try_recv() {
            match signal {
                SessionSignal::SoftCancel | SessionSignal::HardCancel => {
                    store.update_status(session_id, SessionStatus::Cancelled).await?;
                    return Ok(());
                }
                SessionSignal::QueueMessage(msg) => {
                    store.emit_event(session_id, Event::QueuedMessage(msg)).await?;
                }
                SessionSignal::ApprovalDecided { request_id, decision } => {
                    store.emit_event(session_id, Event::ApprovalDecided { 
                        request_id, decision 
                    }).await?;
                }
            }
        }
        
        // Run one turn
        let turn_result = run_brain_turn(
            session_id, &store, &memory, &llm, &hands, &pipeline, &event_tx
        ).await?;
        
        match turn_result {
            TurnResult::Continue => continue,
            TurnResult::Complete => {
                store.update_status(session_id, SessionStatus::Completed).await?;
                return Ok(());
            }
            TurnResult::NeedsApproval(request) => {
                store.emit_event(session_id, Event::ApprovalRequested(request)).await?;
                store.update_status(session_id, SessionStatus::WaitingApproval).await?;
                
                // Block until approval signal arrives
                loop {
                    match signal_rx.recv().await {
                        Some(SessionSignal::ApprovalDecided { decision, .. }) => {
                            store.emit_event(session_id, Event::ApprovalDecided { 
                                request_id: request.id, decision 
                            }).await?;
                            break;
                        }
                        Some(SessionSignal::HardCancel) => return Ok(()),
                        Some(other) => { /* handle other signals */ }
                        None => return Err(Error::ChannelClosed),
                    }
                }
                store.update_status(session_id, SessionStatus::Running).await?;
            }
            TurnResult::Error(e) => {
                store.emit_event(session_id, Event::Error(e.to_string())).await?;
                store.update_status(session_id, SessionStatus::Failed).await?;
                return Err(e);
            }
        }
    }
}
```

Two local-mode details matter in practice:

- `observe()` always replays durable history before attaching a live broadcast tail, so callers can safely reconnect after a restart.
- The local orchestrator supervises each spawned session task. Panics and task errors are translated into durable `Error` + `Failed` state, and cached hands are destroyed on terminal exit.

### Local hand execution

When Docker is not available locally, the `LocalHandProvider` runs commands directly:

```rust
pub struct LocalHandProvider {
    work_dir: PathBuf,            // ~/.moa/workspaces/{id}/sandbox/
    allowed_commands: Vec<String>, // Configurable allowlist
    docker_available: bool,        // Detected at startup
}

#[async_trait]
impl HandProvider for LocalHandProvider {
    async fn execute(&self, _handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        match tool {
            "bash" => {
                if self.docker_available {
                    // Run in local Docker container
                    docker_exec(&self.work_dir, input).await
                } else {
                    // Run directly with restrictions
                    local_exec(&self.work_dir, input, &self.allowed_commands).await
                }
            }
            "file_read" | "file_write" | "file_search" => {
                // File operations restricted to work_dir
                file_tool(&self.work_dir, tool, input).await
            }
            _ => Err(Error::UnknownTool(tool.to_string())),
        }
    }
    
    async fn provision(&self, _spec: HandSpec) -> Result<HandHandle> {
        // Local: no provisioning needed, just return a handle
        Ok(HandHandle::local(self.work_dir.clone()))
    }
    
    async fn destroy(&self, _handle: &HandHandle) -> Result<()> {
        // Local: optionally clean up sandbox directory
        Ok(())
    }
}
```

### Local cron (memory consolidation)

Without Temporal, consolidation runs via `tokio-cron-scheduler`:

```rust
// In LocalOrchestrator initialization
let consolidation_job = Job::new_async("0 0 * * * *", |_uuid, _lock| { // hourly check
    Box::pin(async move {
        let store = get_session_store();
        let memory = get_memory_store();
        
        // Check if consolidation conditions met (≥3 sessions AND ≥24h)
        if should_consolidate(&store, &memory).await {
            run_consolidation(&store, &memory).await.ok();
        }
    })
})?;
scheduler.add(consolidation_job).await?;
```

---

## Startup flow

### `moa-desktop` (local desktop app)

```
1. Launch the desktop binary
2. Load config from ~/.moa/config.toml (create defaults if missing)
3. Detect Docker availability
4. Initialize:
   - SessionStore → TursoSessionStore("~/.moa/sessions.db")
   - MemoryStore → FileMemoryStore("~/.moa/memory/")
   - LLMProvider → from config (default: Anthropic)
   - HandProvider → LocalHandProvider
   - Orchestrator → LocalOrchestrator
5. Start cron scheduler (consolidation, skill improvement)
6. Open the desktop window and connect it to the local runtime
```

### `moa --cloud`

```
1. Parse CLI args (--cloud → connect to Temporal)
2. Load config from ~/.moa/config.toml
3. Initialize:
   - SessionStore → TursoSessionStore(config.turso.url)
   - MemoryStore → FileMemoryStore with Turso sync
   - LLMProvider → from config
   - HandProvider → DaytonaHandProvider
   - Orchestrator → TemporalOrchestrator
4. Start messaging gateway (Telegram + Slack + Discord per config)
5. Gateway receives messages → routes to Orchestrator
6. Optionally also start a local desktop client for monitoring
```

### `moa exec "deploy to staging"` (non-interactive)

```
1. Parse CLI args (exec subcommand → one-shot mode)
2. Same initialization as local mode
3. Create session, submit prompt, stream events to stderr
4. Print final response to stdout
5. Exit with 0 (success) or 1 (error)
```

---

## Configuration

```toml
# ~/.moa/config.toml

[general]
default_provider = "anthropic"  # anthropic | openai | google
default_model = "claude-sonnet-4-6"
reasoning_effort = "medium"     # low | medium | high | xhigh

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"  # read from env var

[providers.openai]
api_key_env = "OPENAI_API_KEY"

[providers.google]
api_key_env = "GOOGLE_API_KEY"

[database]
backend = "turso"
url = "~/.moa/sessions.db"
# admin_url = "postgresql://..." # optional direct URL for migrations/admin tasks
pool_min = 1
pool_max = 5
connect_timeout_secs = 10

[local]
docker_enabled = true           # use Docker for local hands if available
sandbox_dir = "~/.moa/sandbox"
memory_dir = "~/.moa/memory"

[cloud]
enabled = false                 # set to true for cloud mode
turso_url = ""                  # Turso Cloud database URL
turso_auth_token_env = "TURSO_AUTH_TOKEN"

[cloud.temporal]
address = ""
namespace = ""
task_queue = "moa-brains"
api_key_env = "TEMPORAL_API_KEY"

[cloud.flyio]
api_token_env = "FLY_API_TOKEN"
app_name = ""
region = "iad"

[cloud.hands]
default_provider = "daytona"    # daytona | e2b | local
daytona_api_key_env = "DAYTONA_API_KEY"
e2b_api_key_env = "E2B_API_KEY"

[gateway]
telegram_token_env = "TELEGRAM_BOT_TOKEN"
slack_token_env = "SLACK_BOT_TOKEN"
slack_app_token_env = "SLACK_APP_TOKEN"
discord_token_env = "DISCORD_BOT_TOKEN"

[desktop]
theme = "default"               # default | dark | light
sidebar_auto = true             # auto-show at 120+ cols
tab_limit = 8
diff_style = "auto"             # auto | side-by-side | unified

[permissions]
default_posture = "approve"     # approve | auto | full
auto_approve = ["file_read", "file_search", "web_search"]
always_deny = []
```

# 02 — Brain Orchestration

_Restate-backed cloud orchestration, local runtime mode, brain lifecycle._

---

## Overview

Brains are stateless harness loops. They wake from a session log, compile context, call an LLM, route tool calls to hands, and emit events. Nothing in a brain needs to survive a crash — the session log is the recovery mechanism.

Two runtime shapes share the same brain harness and durable event log:

| Mode | Orchestrator | When |
|---|---|---|
| Cloud | Restate `Session` / `SubAgent` objects plus Services and Workflows | Production deployment |
| Local | `LocalOrchestrator` | Zero-setup: `moa` |

Migration history: MOA previously used a different cloud orchestration stack.
The production design is now Restate-only; older rollout notes remain only
for historical reference.

## Converged lifecycle layer

The orchestrators now share one lifecycle rule set instead of each
re-implementing session wakeup semantics:

- `moa-orchestrator/src/session_engine.rs` owns the shared
  `session_requires_processing` rule.
- `SessionStore::transition_status(...)` is the canonical path for
  persisted status changes. It updates the session row, emits the
  matching `SessionStatusChanged` event, and clears snapshots on cancel.
- The brain harness uses that same `transition_status(...)` API when it
  enters `WaitingApproval` or resumes `Running`, so Local and Restate
  persist identical status transitions.
- Local and Restate keep only adapter-specific concerns:
  - Local: Tokio task supervision, runtime broadcasts, local filesystem root wiring
  - Restate: Virtual Object turn boundaries, awakeables, service/workflow connectivity

This means new orchestrator backends should reuse the shared session
engine and the store-level transition API instead of copying lifecycle
rules into a new adapter.

---

## Cloud mode: Restate

Production orchestration runs through Restate. Each session is a keyed
`Session` Virtual Object, tool execution and provider calls live behind
stateless Services, and one-shot scheduled work such as consolidation runs
as Workflows. The full handler map and Kubernetes deployment topology live
in [`12-restate-architecture.md`](12-restate-architecture.md); this document
focuses on the lifecycle shape.

### Session structure

```rust
#[restate_sdk::object]
trait Session {
    async fn post_message(ctx: ObjectContext<'_>, msg: UserMessage) -> Result<(), HandlerError>;
    async fn approve(ctx: ObjectContext<'_>, decision: ApprovalDecision) -> Result<(), HandlerError>;
    async fn cancel(ctx: ObjectContext<'_>, mode: CancelMode) -> Result<(), HandlerError>;
    #[shared]
    async fn status(ctx: SharedObjectContext<'_>) -> Result<SessionStatus, HandlerError>;
    async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError>;
}
```

The important properties are:

- One session key has single-writer semantics, so concurrent messages queue
  naturally without extra locking.
- Durable state keeps only orchestration hot-path data such as pending
  messages, awakeable ids, and turn counters.
- Product-visible history remains in Postgres through `SessionStore`.
- LLM calls and tool calls are delegated to `LLMGateway` and `ToolExecutor`
  services so the side effects can be wrapped in `ctx.run()` and replayed
  safely.

### Durable control flow

| User action | Restate primitive | Effect |
|---|---|---|
| Send message while running | `Session::post_message` | Queued by the object and processed in order |
| Tap "Allow Once" | `Session::approve` resolving an awakeable | Resumes the waiting turn |
| Tap "Always Allow" | `Session::approve` + policy write | Resumes turn and persists a rule |
| Tap "Deny" | `Session::approve` | Resumes turn with denial |
| Tap "Stop" | `Session::cancel(Soft)` | Finish current tool, then stop |
| Force stop | `Session::cancel(Hard)` | Exit at the next durable cancellation point |

### Sub-agents

When the main brain needs specialist work, it dispatches a `SubAgent`
Virtual Object and awaits its durable result rather than spawning a second
workflow engine:

```rust
let result = dispatch_sub_agent(
    &ctx,
    parent_session_id,
    None,
    current_depth,
    "Research current provider pricing".to_string(),
    vec!["web_search".to_string(), "web_fetch".to_string()],
    10_000,
).await?;
```

### Hosting: Kubernetes + Restate

Production deployment now runs as:

- A `RestateCluster` in Kubernetes
- A `RestateDeployment` for `moa-orchestrator`
- Managed Postgres / Neon for the event log and memory state
- Alloy / Grafana for traces, metrics, and logs
- Daytona or E2B for remote hands

See [`12-restate-architecture.md`](12-restate-architecture.md) for the
durable-execution design and [`../k8s/`](../k8s/) for the deployment
manifests.

---

## Local mode: LocalOrchestrator

The local orchestrator provides the same `BrainOrchestrator` interface without any cloud dependencies.

## Contract tests vs. adapter tests

The orchestrator test strategy is split in two layers:

- Contract tests:
  `moa-orchestrator/tests/support/orchestrator_contract.rs`
  These assert shared lifecycle behavior such as:
  - blank sessions wait for the first message
  - queued messages stay FIFO
  - approval resume ordering is stable
  - soft cancel while waiting for approval cancels cleanly
- Adapter tests:
  `local_orchestrator.rs` and the Restate integration suites under
  `tests/integration/`
  These keep only backend-specific checks such as local runtime
  broadcasts or Restate replay/recovery behavior.

Recommended commands:

```bash
# Local adapter + shared contract suite
cargo test -p moa-orchestrator --test local_orchestrator

# Restate integration coverage against a local restate-server
cargo test -p moa-orchestrator --test integration

# Optional live-provider Restate smoke
MOA_RUN_LIVE_PROVIDER_TESTS=1 cargo test -p moa-orchestrator --test llm_gateway_e2e -- --ignored --exact --nocapture
```

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

### The brain loop (shared between local runtime and Restate)

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

In the local runtime, consolidation runs via `tokio-cron-scheduler`:

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
   - SessionStore → PostgresSessionStore("postgres://moa:moa@localhost:5432/moa")
   - MemoryStore → FileMemoryStore("~/.moa/memory/")
   - LLMProvider → from config (default: Anthropic)
   - HandProvider → LocalHandProvider
   - Orchestrator → LocalOrchestrator
5. Start cron scheduler (consolidation, skill improvement)
6. Open the desktop window and connect it to the local runtime
```

### Cloud deployment

```
1. Build and push the `moa-orchestrator` image
2. Apply the `k8s/` manifests for the Restate cluster and orchestrator deployment
3. Configure secrets for Postgres, provider API keys, and observability
4. Register the orchestrator handlers with Restate
5. Route gateway traffic to the Restate ingress
6. Observe sessions through the cluster dashboards and traces
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
url = "postgres://moa:moa@localhost:5432/moa"
# admin_url = "postgresql://..." # optional direct URL for migrations/admin tasks
max_connections = 20
connect_timeout_seconds = 10

[local]
docker_enabled = true           # use Docker for local hands if available
sandbox_dir = "~/.moa/sandbox"
memory_dir = "~/.moa/memory"

[cloud]
enabled = false                 # set to true for cloud mode
# memory_dir = "/data/memory"

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

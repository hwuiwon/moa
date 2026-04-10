# Step 37: Multi-Process Runtime Boundaries

## What this step is about
MOA's runtime assumes a single local process with in-memory `broadcast` channels. This blocks three capabilities: remote/cloud observation of running sessions, multi-client daemon usage with isolated state, and operator-friendly debug logging. This step addresses all three by adding an SSE observation bridge, per-client daemon scoping, and a file-based debug logging mode.

## Files to read
- `moa-core/src/traits.rs` — `BrainOrchestrator` trait (`observe()` returns `EventStream`, no `observe_runtime()`)
- `moa-core/src/types.rs` — `RuntimeEvent` enum
- `moa-core/src/daemon.rs` — `DaemonCommand`, `DaemonReply`, `DaemonStreamEvent`
- `moa-core/src/telemetry.rs` — `init_observability()`, `LevelFilter::WARN` default
- `moa-orchestrator/src/local.rs` — `LocalOrchestrator::observe_runtime()` (concrete, not on trait)
- `moa-orchestrator/src/temporal.rs` — `TemporalOrchestrator` (no observe_runtime)
- `moa-cli/src/daemon.rs` — `DaemonState` with `Arc<Mutex<ChatRuntime>>`, `ObserveSession` handler
- `moa-cli/src/main.rs` — CLI entry point, telemetry init
- `moa-tui/src/runner.rs` — `DaemonChatRuntime`, how TUI consumes daemon events

## Goal
1. Any client (local TUI, remote web, messaging gateway) can observe a running session's `RuntimeEvent` stream over a network transport.
2. Multiple TUI clients connecting to the daemon get isolated workspace/model state.
3. Operators can enable rich debug logging without corrupting the TUI.

---

## Part A: SSE Observation Bridge

### Goal
Promote `observe_runtime()` to the `BrainOrchestrator` trait and add an HTTP/SSE endpoint that bridges `broadcast::Receiver<RuntimeEvent>` to remote clients. The `TemporalOrchestrator` implements this by polling Temporal workflow history for new events.

### Tasks

#### A1. Add `observe_runtime()` to `BrainOrchestrator` trait
In `moa-core/src/traits.rs`:
```rust
/// Subscribes to live runtime events for a running session.
/// Returns None if observation is not supported or the session is not active.
async fn observe_runtime(
    &self,
    session_id: SessionId,
) -> Result<Option<tokio::sync::broadcast::Receiver<RuntimeEvent>>>;
```

#### A2. Implement on `LocalOrchestrator`
Move the existing concrete `observe_runtime()` into the trait impl. It already returns `broadcast::Receiver<RuntimeEvent>` — just wrap in `Ok(Some(...))`.

#### A3. Implement on `TemporalOrchestrator`
Temporal has no push-based event stream. Use a poll-and-emit pattern:
```rust
async fn observe_runtime(&self, session_id: SessionId) -> Result<Option<broadcast::Receiver<RuntimeEvent>>> {
    let (tx, rx) = broadcast::channel(256);
    let store = self.session_store.clone();
    let poll_interval = Duration::from_millis(250);

    tokio::spawn(async move {
        let mut last_seq = 0u64;
        loop {
            tokio::time::sleep(poll_interval).await;
            // Poll new events from session store
            let events = store.get_events(session_id.clone(), EventRange {
                from_seq: Some(last_seq + 1),
                ..EventRange::all()
            }).await;

            match events {
                Ok(new_events) => {
                    for event in &new_events {
                        last_seq = event.sequence_num;
                        // Convert Event → RuntimeEvent (lossy: no token deltas, only completed turns)
                        if let Some(runtime_event) = event_to_runtime_event(&event.event) {
                            if tx.send(runtime_event).is_err() {
                                return; // no receivers left
                            }
                        }
                    }
                }
                Err(_) => return,
            }

            // Check if session is terminal
            if let Ok(session) = store.get_session(session_id.clone()).await {
                if session.status.is_terminal() {
                    let _ = tx.send(RuntimeEvent::TurnCompleted);
                    return;
                }
            }
        }
    });

    Ok(Some(rx))
}
```

Add a helper `event_to_runtime_event()` that maps persisted `Event` variants to `RuntimeEvent` variants. Not all map (e.g., there's no `AssistantDelta` in persisted events — only `BrainResponse`). This is expected: cloud observation is coarser than local.

#### A4. Add SSE endpoint in a new `moa-api` crate (or feature-gated module in `moa-cli`)
Create an Axum-based HTTP server with an SSE endpoint:

```rust
// GET /sessions/{session_id}/stream
async fn session_stream(
    Path(session_id): Path<SessionId>,
    State(state): State<ApiState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let rx = state.orchestrator
        .observe_runtime(session_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let stream = async_stream::try_stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    yield axum::response::sse::Event::default()
                        .event(event.event_type())
                        .data(json);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "SSE observer lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
    ))
}
```

#### A5. Add `RuntimeEvent::event_type()` helper
For SSE event naming:
```rust
impl RuntimeEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::AssistantStarted => "assistant_started",
            Self::AssistantDelta(_) => "assistant_delta",
            Self::AssistantFinished { .. } => "assistant_finished",
            Self::ToolUpdate(_) => "tool_update",
            Self::ApprovalRequested(_) => "approval_requested",
            Self::UsageUpdated { .. } => "usage_updated",
            Self::Notice(_) => "notice",
            Self::TurnCompleted => "turn_completed",
            Self::Error(_) => "error",
        }
    }
}
```

#### A6. Optionally start the SSE server in cloud mode
In `moa-cli/src/main.rs`, when `--cloud` is passed, start the Axum server alongside the messaging gateway:
```rust
if config.cloud.enabled {
    let api_port = config.cloud.api_port.unwrap_or(8080);
    tokio::spawn(start_api_server(orchestrator.clone(), api_port));
}
```

The SSE endpoint is also useful for the future web dashboard.

---

## Part B: Per-Client Daemon Scoping

### Goal
Each TUI client connecting to the daemon gets its own workspace and model context. Session execution remains shared (sessions belong to the daemon, not the client), but the "current workspace" and "current model" are per-connection, not global.

### Tasks

#### B1. Add `ClientContext` to daemon connection handling
```rust
struct ClientContext {
    id: Uuid,
    workspace_id: Option<WorkspaceId>,
    model: Option<String>,
}
```

Each `handle_connection()` call creates a `ClientContext`. `SetWorkspace` and `SetModel` commands mutate this context, not the shared `DaemonState`.

#### B2. Update `DaemonCommand::CreateSession` to use client context
When creating a session, use the client's workspace/model if set, falling back to the daemon's defaults:
```rust
DaemonCommand::CreateSession => {
    let workspace = client_ctx.workspace_id
        .clone()
        .unwrap_or_else(|| state.default_workspace_id());
    let model = client_ctx.model
        .clone()
        .unwrap_or_else(|| state.default_model());
    // create session with these values
}
```

#### B3. Update `SetWorkspace` / `SetModel` to be client-scoped
```rust
DaemonCommand::SetWorkspace { workspace_id } => {
    client_ctx.workspace_id = Some(workspace_id);
    DaemonReply::Ack
}
DaemonCommand::SetModel { model } => {
    client_ctx.model = Some(model);
    DaemonReply::Ack
}
```

Remove the global mutation on `DaemonState.runtime` for these commands.

#### B4. Lazy session creation
Instead of starting the daemon with one default session, start with zero:
- Remove the auto-session-creation in daemon startup
- `DaemonCommand::CreateSession` creates on demand
- `DaemonCommand::ListSessionPreviews` returns empty list when no sessions exist
- The TUI handles the "no sessions" state by showing a welcome screen or auto-creating

In `ChatRuntime`, change the active session from `SessionId` to `Option<SessionId>`. Methods that need a session return an error or create one lazily when `None`.

---

## Part C: Debug Logging

### Goal
Add `--debug` flag and `--log-file` option to all CLI entry points. Debug logs go to a file, never the terminal. The TUI remains clean by default.

### Tasks

#### C1. Add CLI flags
In `moa-cli/src/main.rs` (clap):
```rust
#[derive(Parser)]
struct Cli {
    /// Enable debug logging (writes to ~/.moa/moa.log)
    #[arg(long)]
    debug: bool,

    /// Custom log file path
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    // ... existing args
}
```

#### C2. Update `init_observability` to support file logging
In `moa-core/src/telemetry.rs`:

```rust
pub struct TelemetryConfig {
    pub debug: bool,
    pub log_file: Option<PathBuf>,
}

pub fn init_observability(config: &MoaConfig, telemetry: &TelemetryConfig) -> Result<TelemetryGuard> {
    // Console layer: always WARN (never corrupts TUI)
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(LevelFilter::WARN);

    // File layer: DEBUG when --debug, otherwise disabled
    let file_layer = if telemetry.debug || telemetry.log_file.is_some() {
        let path = telemetry.log_file.clone()
            .unwrap_or_else(|| default_log_path(config));
        let file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&path)?;
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        Some((
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_filter(if telemetry.debug {
                    LevelFilter::DEBUG
                } else {
                    LevelFilter::INFO
                }),
            guard,
        ))
    } else {
        None
    };

    // ... compose layers, store guard in TelemetryGuard
}
```

The `TelemetryGuard` must own the `tracing_appender::non_blocking::WorkerGuard` to keep the file writer alive.

#### C3. Default log path
```rust
fn default_log_path(config: &MoaConfig) -> PathBuf {
    // ~/.moa/moa.log
    PathBuf::from(shellexpand::tilde("~/.moa/moa.log").to_string())
}
```

#### C4. Add `RUST_LOG` env override
The existing `EnvFilter` from `tracing-subscriber` respects `RUST_LOG` automatically. Just document that `RUST_LOG=moa_brain=debug` works for targeted debugging.

#### C5. Add `moa doctor` log path output
`moa doctor` should print the log file location so operators know where to look:
```
Log file: ~/.moa/moa.log (--debug to enable)
```

## Deliverables
```
# Part A: SSE observation
moa-core/src/traits.rs              # + observe_runtime() on BrainOrchestrator
moa-core/src/types.rs               # + RuntimeEvent::event_type()
moa-orchestrator/src/local.rs       # Move observe_runtime into trait impl
moa-orchestrator/src/temporal.rs    # Poll-based observe_runtime
moa-cli/src/api.rs                  # (new) Axum SSE server
moa-cli/src/main.rs                 # Start API server in cloud mode

# Part B: Per-client daemon
moa-cli/src/daemon.rs               # ClientContext, per-connection scoping, lazy sessions
moa-tui/src/runner.rs               # Handle Option<SessionId> in DaemonChatRuntime

# Part C: Debug logging
moa-core/src/telemetry.rs           # TelemetryConfig, file layer, WorkerGuard
moa-cli/src/main.rs                 # --debug, --log-file flags
```

## Acceptance criteria

**Part A:**
1. `BrainOrchestrator::observe_runtime()` is on the trait.
2. `LocalOrchestrator` returns a `broadcast::Receiver<RuntimeEvent>`.
3. `TemporalOrchestrator` returns a poll-based receiver that emits events from the session store.
4. `GET /sessions/{id}/stream` returns an SSE stream of `RuntimeEvent`s (when API server is running).
5. Multiple SSE clients can observe the same session concurrently.
6. SSE `KeepAlive` prevents idle connection timeouts.
7. Lagged observers skip missed events and continue (not fatal).

**Part B:**
8. Two TUI clients connected to the same daemon can have different workspaces.
9. `SetWorkspace` / `SetModel` only affect the calling client's context.
10. Sessions created by different clients use their respective workspace/model contexts.
11. Daemon starts with zero sessions (lazy creation).
12. TUI handles "no active session" gracefully.

**Part C:**
13. `moa --debug` writes DEBUG-level logs to `~/.moa/moa.log`.
14. `moa --log-file /tmp/moa.log` writes to a custom path.
15. Default TUI remains clean (only WARN+ to stderr).
16. `RUST_LOG=moa_brain=debug moa` works for targeted module debugging.
17. `moa doctor` shows the log file path.

## Tests

**Part A:**
- `observe_runtime()` on `LocalOrchestrator` returns a receiver that gets events during a turn
- `event_to_runtime_event()` correctly maps `Event::BrainResponse` → `RuntimeEvent::AssistantFinished`
- `event_to_runtime_event()` returns `None` for events with no RuntimeEvent equivalent
- SSE endpoint returns `200` with `text/event-stream` content type
- SSE endpoint returns `404` for unknown session IDs
- Two concurrent SSE subscribers both receive the same events

**Part B:**
- Client A sets workspace "foo", Client B sets workspace "bar" → sessions created by each use the correct workspace
- Daemon with zero sessions → `ListSessionPreviews` returns empty
- `CreateSession` when no sessions exist → succeeds

**Part C:**
- `--debug` flag → log file is created and contains DEBUG entries
- No `--debug` flag → no log file created (or exists but empty)
- Console output at default level contains no DEBUG/INFO lines

```bash
cargo test -p moa-core
cargo test -p moa-orchestrator
cargo test -p moa-cli
```

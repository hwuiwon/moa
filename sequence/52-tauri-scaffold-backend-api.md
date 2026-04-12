# Step 52 — Tauri v2 Scaffold + Backend API Layer

_Initialize the Tauri v2 + React + TypeScript project. Create `#[tauri::command]` functions wrapping ChatRuntime. Define `StreamEvent` Channel for LLM streaming. Remove moa-tui dependency from default build._

---

## 1. What this step is about

This is the foundation step. It creates the Tauri project structure, wires up the MOA Rust backend as managed state, and exposes every operation the frontend will need as `#[tauri::command]` functions. No frontend UI yet — just the Rust→JS bridge and a blank window that proves the plumbing works.

The key architectural decision: **reuse `ChatRuntime` from `moa-tui/src/runner.rs`** as the backend. The existing `LocalChatRuntime` and `DaemonChatRuntime` already encapsulate all orchestrator, session, memory, and tool operations. Rather than rewriting this, extract the runtime logic into a shared crate (`moa-runtime`) that both the Tauri app and the CLI can depend on.

---

## 2. Files/directories to read

- **`moa-tui/src/runner.rs`** — `ChatRuntime` enum with `LocalChatRuntime` and `DaemonChatRuntime`. ~30 methods covering sessions, memory, tools, approvals, streaming. This is the API surface to expose via Tauri commands.
- **`moa-core/src/types.rs`** — `RuntimeEvent` enum (lines 1683-1708). The streaming protocol: `AssistantStarted`, `AssistantDelta(char)`, `AssistantFinished`, `ToolUpdate`, `ApprovalRequested`, `UsageUpdated`, `Notice`, `TurnCompleted`, `Error`. This maps to the Tauri `Channel<StreamEvent>`.
- **`moa-cli/src/main.rs`** — How the CLI constructs `ChatRuntime` and launches `run_tui()`. The Tauri app replaces `run_tui()`.
- **`moa-orchestrator/src/local.rs`** — `LocalOrchestrator`. Managed as Tauri state.
- **`moa-tui/Cargo.toml`** — Dependencies the runtime needs (moa-brain, moa-core, moa-hands, moa-memory, moa-orchestrator, moa-providers, moa-session).

Also reference:
- Tauri v2 docs: https://v2.tauri.app/develop/calling-rust/ — commands, state, channels
- Tauri v2 project structure: https://v2.tauri.app/start/project-structure/

---

## 3. Goal

After this step:
1. `cargo tauri dev` opens a Tauri window (blank React page with "MOA" header).
2. The Rust backend initializes `LocalOrchestrator`, `SessionStore`, `MemoryStore` as managed Tauri state.
3. All 30+ ChatRuntime operations are exposed as `#[tauri::command]` functions.
4. A `Channel<StreamEvent>` is defined for real-time LLM token streaming.
5. The frontend can call `invoke("list_sessions")` and get back data.
6. The CLI still works independently (`moa exec`, `moa eval`, etc.).

---

## 4. Rules

- **Extract runtime into `moa-runtime` crate.** Don't duplicate `ChatRuntime` logic between moa-tui and the Tauri app. Create a new `moa-runtime` crate that both depend on.
- **Tauri app is a separate crate: `moa-app`** with its own `Cargo.toml` inside `src-tauri/`. It depends on `moa-runtime`.
- **Use `tauri::State<Mutex<ChatRuntime>>`** for mutable backend access. Use `std::sync::Mutex` since most operations are quick dispatches to async methods.
- **Commands must return `Result<T, MoaAppError>`** where `MoaAppError` implements `Serialize`. Tauri requires serializable errors.
- **Channel streaming uses a tagged enum.** `StreamEvent` variants map 1:1 to `RuntimeEvent` but with serde tags for TypeScript discriminated unions.
- **Frontend scaffold uses Vite + React 19 + TypeScript.** Set up with `npm create vite@latest`.
- **shadcn/ui + Tailwind CSS v4.** Install from the start — don't bolt on later.
- **Do NOT delete moa-tui yet.** That's the final step (59). For now, just make it optional in the workspace.

---

## 5. Tasks

### 5a. Create `moa-runtime` crate

Extract `ChatRuntime`, `LocalChatRuntime`, `DaemonChatRuntime`, and `SessionPreview` from `moa-tui/src/runner.rs` into a new `moa-runtime/src/lib.rs`. The TUI becomes a thin rendering layer on top of `moa-runtime`. This crate has the same dependencies as `moa-tui` minus all the ratatui/crossterm/UI crates.

```
moa-runtime/
├── Cargo.toml
└── src/
    └── lib.rs    # ChatRuntime, LocalChatRuntime, DaemonChatRuntime
```

Update `moa-tui` to depend on `moa-runtime` and re-export `ChatRuntime`.

### 5b. Initialize Tauri v2 project

```bash
cd moa/
npm create tauri-app@latest moa-app -- --template react-ts --manager npm
# This creates src-tauri/ and the React frontend scaffold
```

The resulting structure:
```
moa/
├── src-tauri/           # Tauri Rust backend
│   ├── Cargo.toml       # depends on moa-runtime, tauri v2
│   ├── src/
│   │   ├── main.rs      # Tauri setup, state management
│   │   ├── commands.rs  # All #[tauri::command] functions
│   │   ├── stream.rs    # StreamEvent enum, Channel handling
│   │   └── error.rs     # MoaAppError with Serialize
│   ├── tauri.conf.json
│   └── capabilities/
│       └── default.json # IPC permissions
├── src/                 # React frontend
│   ├── main.tsx
│   ├── App.tsx
│   └── ...
├── package.json
├── tsconfig.json
├── vite.config.ts
└── ...existing moa crates...
```

Add `src-tauri` to the workspace `Cargo.toml` members.

### 5c. Define `StreamEvent` tagged enum

In `src-tauri/src/stream.rs`:

```rust
use serde::Serialize;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "event", content = "data")]
pub enum StreamEvent {
    AssistantStarted,
    AssistantDelta { text: String },
    AssistantFinished { text: String },
    ToolUpdate {
        call_id: String,
        tool_name: String,
        status: String,      // "pending" | "running" | "done" | "error"
        summary: Option<String>,
    },
    ApprovalRequired {
        request_id: String,
        tool_name: String,
        risk_level: String,
        input_summary: String,
        diff_preview: Option<String>,
    },
    UsageUpdated { total_tokens: usize },
    Notice { message: String },
    TurnCompleted,
    Error { message: String },
}
```

### 5d. Implement `#[tauri::command]` functions

In `src-tauri/src/commands.rs`, create commands for every ChatRuntime method. Group by domain:

**Session commands:**
```rust
#[tauri::command]
async fn create_session(state: State<'_, Mutex<ChatRuntime>>) -> Result<String, MoaAppError> {
    let mut rt = state.lock().unwrap();
    let id = rt.create_session().await?;
    Ok(id.to_string())
}

#[tauri::command]
async fn list_sessions(state: State<'_, Mutex<ChatRuntime>>) -> Result<Vec<SessionSummaryDto>, MoaAppError> { ... }

#[tauri::command]
async fn get_session(state: State<'_, Mutex<ChatRuntime>>, session_id: String) -> Result<SessionMetaDto, MoaAppError> { ... }

#[tauri::command]
async fn set_workspace(state: State<'_, Mutex<ChatRuntime>>, workspace_id: String) -> Result<String, MoaAppError> { ... }
```

**Chat commands (with Channel streaming):**
```rust
#[tauri::command]
async fn send_message(
    session_id: String,
    prompt: String,
    on_event: Channel<StreamEvent>,
    state: State<'_, Mutex<ChatRuntime>>,
) -> Result<(), MoaAppError> {
    // 1. Queue message
    // 2. Subscribe to RuntimeEvent broadcast
    // 3. Forward RuntimeEvents as StreamEvents through the Channel
    // 4. Return when TurnCompleted or Error
}

#[tauri::command]
async fn stop_session(state: State<'_, Mutex<ChatRuntime>>, session_id: String) -> Result<(), MoaAppError> { ... }

#[tauri::command]
async fn respond_to_approval(
    state: State<'_, Mutex<ChatRuntime>>,
    request_id: String,
    decision: String,  // "allow_once" | "always_allow" | "deny"
) -> Result<(), MoaAppError> { ... }
```

**Memory commands:**
```rust
#[tauri::command]
async fn list_memory_pages(state: State<'_, Mutex<ChatRuntime>>, filter: Option<String>) -> Result<Vec<PageSummaryDto>, MoaAppError> { ... }

#[tauri::command]
async fn read_memory_page(state: State<'_, Mutex<ChatRuntime>>, path: String) -> Result<WikiPageDto, MoaAppError> { ... }

#[tauri::command]
async fn search_memory(state: State<'_, Mutex<ChatRuntime>>, query: String, limit: usize) -> Result<Vec<MemorySearchResultDto>, MoaAppError> { ... }

#[tauri::command]
async fn delete_memory_page(state: State<'_, Mutex<ChatRuntime>>, path: String) -> Result<(), MoaAppError> { ... }
```

**Config commands:**
```rust
#[tauri::command]
fn get_config(state: State<'_, Mutex<ChatRuntime>>) -> Result<MoaConfigDto, MoaAppError> { ... }

#[tauri::command]
async fn set_model(state: State<'_, Mutex<ChatRuntime>>, model: String) -> Result<String, MoaAppError> { ... }

#[tauri::command]
fn get_tool_names(state: State<'_, Mutex<ChatRuntime>>) -> Result<Vec<String>, MoaAppError> { ... }
```

### 5e. Define DTO types for the IPC boundary

Create `src-tauri/src/dto.rs` with serializable DTOs that map from internal MOA types. Don't expose raw `SessionMeta`, `WikiPage`, etc. directly — create lean DTOs with only the fields the frontend needs.

### 5f. Wire up Tauri state in `main.rs`

```rust
fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let config = MoaConfig::load()?;
            let rt = tokio::runtime::Runtime::new()?;
            let chat_runtime = rt.block_on(ChatRuntime::from_config(config, Platform::Desktop))?;
            app.manage(Mutex::new(chat_runtime));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_session,
            commands::list_sessions,
            commands::send_message,
            commands::stop_session,
            commands::respond_to_approval,
            commands::list_memory_pages,
            commands::read_memory_page,
            commands::search_memory,
            commands::get_config,
            commands::set_model,
            // ... all commands
        ])
        .run(tauri::generate_context!())
        .expect("error running MOA");
}
```

### 5g. Set up React frontend scaffold

```bash
cd moa/
npm install
npm install -D tailwindcss @tailwindcss/vite
npx shadcn@latest init
npm install zustand @tanstack/react-query @tauri-apps/api @tauri-apps/plugin-notification
```

Create a minimal `App.tsx` that calls `invoke("list_sessions")` and displays the result to prove IPC works.

### 5h. Update workspace Cargo.toml

Add `moa-runtime` and `src-tauri` to workspace members. Make `moa-tui` optional (move from `default-members` to just `members`).

---

## 6. How it should be implemented

The most important decision: **use `tokio::sync::Mutex` for `ChatRuntime` state**, not `std::sync::Mutex`. Most Tauri commands are `async` and call `.await` inside the lock. `std::sync::Mutex` cannot be held across await points without `Send` issues. Use:

```rust
app.manage(tokio::sync::Mutex::new(chat_runtime));
```

And in commands:
```rust
let rt = state.lock().await;
```

For the `send_message` command with Channel streaming, the pattern is:
1. Lock the runtime
2. Call `run_turn()` which starts the brain and returns immediately
3. Subscribe to the `RuntimeEvent` broadcast receiver
4. Release the lock
5. Loop: receive RuntimeEvents, convert to StreamEvents, send through Channel
6. Break on `TurnCompleted` or `Error`

This ensures the lock isn't held during the entire streaming operation.

---

## 7. Deliverables

- [ ] `moa-runtime/` — New crate with `ChatRuntime` extracted from moa-tui
- [ ] `moa-tui/src/runner.rs` — Refactored to re-export from moa-runtime
- [ ] `src-tauri/Cargo.toml` — Tauri v2 backend crate depending on moa-runtime
- [ ] `src-tauri/src/main.rs` — Tauri setup with managed state
- [ ] `src-tauri/src/commands.rs` — All `#[tauri::command]` functions
- [ ] `src-tauri/src/stream.rs` — `StreamEvent` enum
- [ ] `src-tauri/src/dto.rs` — Serializable DTO types
- [ ] `src-tauri/src/error.rs` — `MoaAppError` with Serialize
- [ ] `src-tauri/tauri.conf.json` — Window config, build config
- [ ] `src-tauri/capabilities/default.json` — IPC permissions
- [ ] `src/` — React scaffold (Vite + React 19 + TypeScript)
- [ ] `package.json` — Frontend dependencies (shadcn/ui, Tailwind, Zustand, TanStack Query)
- [ ] `Cargo.toml` (workspace) — Updated members

---

## 8. Acceptance criteria

1. `cargo tauri dev` launches a desktop window with a React page.
2. The React page successfully calls `invoke("list_sessions")` and displays results.
3. All command functions compile with correct type signatures.
4. `moa exec "hello"` still works via CLI (no regression).
5. `StreamEvent` is a tagged union that TypeScript can discriminate on `.event`.
6. The Rust backend initializes without errors (config loading, orchestrator, session store).
7. `moa-runtime` crate compiles independently.

---

## 9. Testing

**Test 1:** `cargo build -p moa-runtime` succeeds.
**Test 2:** `cargo tauri build --debug` produces a binary.
**Test 3:** Frontend calls `invoke("list_sessions")` → receives JSON array.
**Test 4:** Frontend calls `invoke("create_session")` → receives session ID string.
**Test 5:** `cargo test -p moa-tui` still passes (runtime extraction didn't break TUI).
**Test 6:** `cargo test -p moa-cli` still passes.

---

## 10. Additional notes

- **Why extract `moa-runtime`?** Without it, we'd duplicate all the ChatRuntime logic in the Tauri backend, or the Tauri crate would depend on `moa-tui` (which pulls in ratatui, crossterm, etc.). A clean `moa-runtime` crate is the right factoring.
- **DTO pattern.** Don't serialize raw `SessionMeta` or `WikiPage` across IPC — they may contain types that don't serialize cleanly, or expose more fields than the frontend needs. Lean DTOs are better for IPC performance and TypeScript type generation.
- **tauri-specta.** Consider adding `tauri-specta` for auto-generating TypeScript types from Rust command signatures. This eliminates type drift between frontend and backend. Can be added later as an enhancement.
- **Platform::Desktop.** Add a `Desktop` variant to the `Platform` enum in `moa-core/src/types.rs` for sessions created from the Tauri app.

# Step 06: Tool Registry + File Tools + Local Hand

## What this step is about
Building the tool system: a registry of available tools, built-in file tools, the bash tool, and a local hand provider for executing them.

## Files to read
- `docs/06-hands-and-mcp.md` — `HandProvider` trait, `LocalHandProvider`, `ToolRouter`, tool registry
- `docs/01-architecture-overview.md` — `HandProvider` trait

## Goal
The agent can read files, write files, search files, and execute shell commands on the local machine. Tools are registered in a `ToolRegistry` with JSON Schema definitions.

## Rules
- `LocalHandProvider` runs commands directly if Docker is unavailable, in Docker if available
- File tools restrict operations to a sandbox directory (configurable working directory)
- Bash tool captures stdout, stderr, and exit code
- Tool execution has a configurable timeout (default: 5 minutes)
- Each tool has a JSON Schema for its parameters and a `RiskLevel`
- Memory tools route through the graph ingestion and retrieval crates.

## Tasks
1. **`moa-hands/src/local.rs`**: `LocalHandProvider` — direct execution + Docker detection
2. **`moa-hands/src/router.rs`**: `ToolRouter` — routes tool calls to the right handler
3. **Built-in tools**: `bash`, `file_read`, `file_write`, `file_search`, `web_search` (stub), `web_fetch` (stub), `memory_search`, `memory_write`
4. **`moa-hands/src/registry.rs`** (or inline in router): `ToolRegistry` with tool definitions and JSON schemas
5. **Update `moa-brain/harness.rs`**: When the LLM returns tool_use blocks, route them through the `ToolRouter`, emit `ToolCall` and `ToolResult` events, and feed results back to the LLM

## Deliverables
```
moa-hands/src/
├── lib.rs
├── local.rs         # LocalHandProvider
├── router.rs        # ToolRouter + ToolRegistry
└── tools/
    ├── mod.rs
    ├── bash.rs
    ├── file_read.rs
    ├── file_write.rs
    ├── file_search.rs
    └── memory.rs    # memory_search, memory_write wrappers
```

## Acceptance criteria
1. `file_read` reads a file and returns its contents
2. `file_write` creates/overwrites a file
3. `file_search` finds files by glob pattern
4. `bash` executes a command and returns stdout/stderr/exit_code
5. Tool execution respects timeout
6. File operations are restricted to the sandbox directory (no path traversal)
7. Brain harness now completes multi-turn loops with tool use

## Tests
- Unit test: Each tool executes correctly with valid input
- Unit test: File operations reject paths outside sandbox (path traversal prevention)
- Unit test: Bash tool captures stdout and stderr separately
- Unit test: Bash tool respects timeout (run `sleep 10` with 1s timeout → error)
- Integration test: Brain harness + mock LLM that returns tool_use → verify ToolCall + ToolResult events in session

```bash
cargo test -p moa-hands
cargo test -p moa-brain  # re-run brain tests with tools
```

---

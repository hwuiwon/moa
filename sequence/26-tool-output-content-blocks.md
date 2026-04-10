# Step 26: ToolOutput Content Blocks

## What this step is about
`ToolOutput` is currently shell-oriented (`stdout`, `stderr`, `exit_code`, `duration`). Built-in tools like `memory_search` and `memory_read` flatten structured results into `stdout` as a string. This works but loses structure that matters for UI rendering, MCP compatibility, and richer tool-result conditioning in the context pipeline.

This step replaces the shell-only `ToolOutput` with a content-block model aligned with MCP's `CallToolResult`, keeping backward compatibility for process-backed tools.

## Files to read
- `moa-core/src/types.rs` — current `ToolOutput` struct
- `moa-hands/src/tools/memory.rs` — built-in tools constructing `ToolOutput`
- `moa-hands/src/tools/bash.rs` — bash tool constructing `ToolOutput`
- `moa-hands/src/tools/file_read.rs` — file tools constructing `ToolOutput`
- `moa-hands/src/tools/file_write.rs`
- `moa-hands/src/tools/file_search.rs`
- `moa-hands/src/router.rs` — `BuiltInTool` trait, `ToolRouter::execute`
- `moa-hands/src/local.rs` — `LocalHandProvider::execute`
- `moa-brain/src/harness.rs` — brain processes `ToolOutput` into events
- `moa-core/src/events.rs` — `Event::ToolResult` stores tool output
- `moa-brain/src/pipeline/history.rs` — history compiler formats tool results for context

## Goal
`ToolOutput` becomes a content-block type that naturally represents both shell output and structured results. MCP tool results can flow through without lossy conversion. The brain and UI can distinguish text, structured data, and errors at the type level.

## Rules
- The new `ToolOutput` must be a superset of the current capabilities — no information loss for bash/file tools.
- Align with MCP's `CallToolResult` shape: a list of content blocks + an `is_error` flag + optional structured data.
- Keep it simple: start with `Text` and `Json` content variants. Do NOT add `Image`, `Audio`, or `Resource` variants yet — those can come when vision/multimodal tools are added.
- `duration` stays as a field (it's framework metadata, not content).
- All existing tool implementations must be updated to construct the new type.
- The brain harness must still be able to serialize tool results into the LLM context as text (the LLM always sees text).
- The `Event::ToolResult` payload should carry the richer type so the TUI/messaging can render structured results later.

## Tasks

### 1. Define new types in `moa-core/src/types.rs`

```rust
/// A single block of tool output content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Plain text (stdout, human-readable summaries, etc.)
    Text { text: String },
    /// Structured JSON data (search results, API responses, etc.)
    Json { data: serde_json::Value },
}

/// Result of a tool execution, aligned with MCP CallToolResult.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Content blocks for LLM and UI consumption.
    pub content: Vec<ToolContent>,
    /// Whether this result represents an error.
    pub is_error: bool,
    /// Optional structured data for programmatic use (not sent to LLM).
    pub structured: Option<serde_json::Value>,
    /// Execution duration.
    pub duration: Duration,
}
```

### 2. Add convenience constructors on `ToolOutput`

```rust
impl ToolOutput {
    /// Creates a successful text result (replaces most current usage).
    pub fn text(text: impl Into<String>, duration: Duration) -> Self { ... }

    /// Creates a successful result from shell command output.
    pub fn from_process(stdout: String, stderr: String, exit_code: i32, duration: Duration) -> Self { ... }

    /// Creates a structured JSON result with a text summary.
    pub fn json(summary: impl Into<String>, data: serde_json::Value, duration: Duration) -> Self { ... }

    /// Creates an error result.
    pub fn error(message: impl Into<String>, duration: Duration) -> Self { ... }

    /// Renders all content blocks to a single string for LLM context.
    pub fn to_text(&self) -> String { ... }
}
```

`from_process` should produce content like:
- If `exit_code == 0` and `stderr` is empty: single `Text` block with `stdout`
- If `exit_code == 0` and `stderr` is non-empty: `Text` for stdout + `Text` for stderr (prefixed)
- If `exit_code != 0`: `is_error = true`, include exit code in text

### 3. Update `BuiltInTool::execute` return type
No change needed — it already returns `Result<ToolOutput>`. The type itself changes shape.

### 4. Update all built-in tools in `moa-hands/src/tools/`

**`bash.rs`**: Use `ToolOutput::from_process(stdout, stderr, exit_code, duration)`

**`file_read.rs`**: Use `ToolOutput::text(contents, duration)`

**`file_write.rs`**: Use `ToolOutput::text("Wrote N bytes to path", duration)`

**`file_search.rs`**: Use `ToolOutput::json(summary, json_results, duration)` where `summary` is the human-readable list and `data` is the structured match array.

**`memory.rs`**:
- `memory_search`: Use `ToolOutput::json(rendered_summary, structured_results, duration)` where structured results include path, title, scope, confidence per hit.
- `memory_read`: Use `ToolOutput::text(page_content, duration)`
- `memory_write`: Use `ToolOutput::text(confirmation_message, duration)`

### 5. Update `LocalHandProvider::execute` and `HandProvider` trait
`HandProvider::execute` already returns `Result<ToolOutput>`. The `LocalHandProvider` constructs `ToolOutput` from process output — update to use `ToolOutput::from_process()`.

### 6. Update `ToolRouter::execute`
Should work without changes since it already returns `Result<ToolOutput>`. Verify.

### 7. Update brain harness (`moa-brain/src/harness.rs`)
Where the brain feeds tool results back to the LLM context, use `tool_output.to_text()` to flatten content blocks into a string for the LLM message. The `Event::ToolResult` should store the full `ToolOutput` (or its serialized form) so the event log retains structure.

### 8. Update `Event::ToolResult` in `moa-core/src/events.rs`
Change the `output: String` field to carry the richer type:
```rust
ToolResult {
    tool_id: Uuid,
    output: ToolOutput,  // was: String
    success: bool,       // can now be derived from output.is_error, consider removing
    duration_ms: u64,    // can now be derived from output.duration, consider removing
}
```
If removing `success` and `duration_ms` would break too many things, keep them and populate from the `ToolOutput` fields. But avoid redundancy if feasible.

### 9. Update history compiler (`moa-brain/src/pipeline/history.rs`)
When formatting `ToolResult` events into context messages, use `output.to_text()` or format the content blocks appropriately.

### 10. Update tests
Every test that constructs a `ToolOutput` directly needs to use the new constructors.

## Deliverables
```
moa-core/src/types.rs              # New ToolOutput + ToolContent types
moa-core/src/events.rs             # Updated Event::ToolResult
moa-hands/src/tools/bash.rs        # Updated constructors
moa-hands/src/tools/file_read.rs   # Updated constructors
moa-hands/src/tools/file_write.rs  # Updated constructors
moa-hands/src/tools/file_search.rs # Updated constructors
moa-hands/src/tools/memory.rs      # Updated constructors
moa-hands/src/local.rs             # Updated LocalHandProvider
moa-hands/src/router.rs            # Verify compatibility
moa-brain/src/harness.rs           # Use to_text() for LLM context
moa-brain/src/pipeline/history.rs  # Format ToolResult from new type
```

## Acceptance criteria
1. `ToolOutput` uses content blocks (`Vec<ToolContent>`) instead of `stdout`/`stderr`/`exit_code`.
2. `ToolOutput::from_process()` preserves all shell output information.
3. `ToolOutput::to_text()` produces a clean string suitable for LLM context.
4. `memory_search` returns structured JSON alongside the text summary.
5. `Event::ToolResult` stores the full `ToolOutput` (no information loss in the event log).
6. Brain harness still correctly feeds tool results back to the LLM.
7. All existing tests pass.

## Tests

**Unit tests in `moa-core`:**
- `ToolOutput::text()` creates a single text content block, `is_error = false`
- `ToolOutput::from_process()` with exit 0 → `is_error = false`, stdout in content
- `ToolOutput::from_process()` with exit 1 → `is_error = true`, exit code in content
- `ToolOutput::from_process()` with stderr → stderr included as separate block or appended
- `ToolOutput::json()` creates text + json content blocks
- `ToolOutput::error()` → `is_error = true`
- `ToolOutput::to_text()` concatenates all blocks cleanly
- Round-trip: serialize `ToolOutput` to JSON → deserialize → equal

**Unit tests in `moa-hands`:**
- Each tool produces the expected `ToolOutput` variant
- `memory_search` output has both text and structured content
- `bash` output from a failing command has `is_error = true`

**Integration:**
- Brain turn with tool use → `Event::ToolResult` contains the full `ToolOutput` → history compiler formats it correctly for the next LLM call

```bash
cargo test -p moa-core
cargo test -p moa-hands
cargo test -p moa-brain
```

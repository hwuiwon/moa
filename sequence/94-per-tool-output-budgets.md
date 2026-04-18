# Step 94 — Per-Tool Output Token Budgets

_Step 75 added smart head/tail truncation at a global level. This step adds per-tool token budgets enforced at the tool router level, capping output before it enters the event log and context. Prevents single bloated tool results from consuming the context window._

---

## 1. What this step is about

In a typical coding session, one `bash` call returning a full test suite output (50K tokens) can consume 25% of the context window in a single turn. Step 75's truncation helps but uses a uniform strategy. Different tools have different reasonable output sizes:

- `file_read`: 8K tokens (500 lines × ~16 tok/line)
- `bash` stdout: 4K tokens; stderr: 2K tokens
- `grep` / `file_search`: 4K tokens (match listings)
- `memory_search`: 3K tokens (3-5 page snippets)
- `file_outline`: 2K tokens

---

## 2. Files to read

- `moa-hands/src/router.rs` — tool dispatch. Budget enforcement happens here, after tool execution, before event emission.
- `moa-core/src/truncation.rs` — existing head/tail truncation from step 75.
- `moa-core/src/types/tool.rs` — `ToolDefinition` / tool registry.
- `moa-core/src/config.rs` — config for budget overrides.

---

## 3. Goal

1. Every tool has a `max_output_tokens: u32` field in its `ToolDefinition`.
2. After execution, the router truncates output to `max_output_tokens` using step 75's head/tail strategy.
3. When truncation occurs, append a footer: `[output truncated from ~{original_tokens} to {budget} tokens]`.
4. Per-tool budgets are configurable via `[tool_budgets]` in config, with sensible defaults.
5. The `ToolResult` event carries both `output` (truncated) and `original_output_tokens` (pre-truncation count) for diagnostics.

---

## 4. Rules

- **Budget enforcement is at the router, not inside each tool handler.** Tools return their full output; the router truncates uniformly.
- **Don't over-engineer token counting.** Use `output.len() / 4` as a rough estimate. Exact tokenization is too expensive per-tool-call.
- **Errors are never truncated.** If `success == false`, return the full error regardless of budget. Error context prevents repeated failures.
- **Budget is per-call, not per-session.** Two `bash` calls each get their own 4K budget.

---

## 5. Tasks

### 5a. Add `max_output_tokens` to `ToolDefinition`

Default values for built-in tools. MCP tools get a configurable default (8K).

### 5b. Truncation in `ToolRouter::execute`

After `handler.execute(...)` returns:
```rust
let truncated = if output.success && estimated_tokens(&output.text) > tool_def.max_output_tokens {
    let result = truncate_head_tail(&output.text, tool_def.max_output_tokens * 4); // chars ≈ tokens * 4
    ToolOutput {
        text: format!("{}\n[output truncated from ~{} to ~{} tokens]",
            result, estimated_tokens(&output.text), tool_def.max_output_tokens),
        original_output_tokens: Some(estimated_tokens(&output.text)),
        ..output
    }
} else { output };
```

### 5c. Config

```toml
[tool_budgets]
file_read = 8000
bash_stdout = 4000
bash_stderr = 2000
grep = 4000
file_search = 4000
memory_search = 3000
file_outline = 2000
default = 8000
```

### 5d. Metric

`moa_tool_output_truncated_total` counter by tool name. Tracks how often truncation fires — if a tool is always truncated, its budget is too low (or the agent is misusing it).

### 5e. Tests

- `bash` returning 20K tokens → truncated to 4K with footer.
- `bash` returning error → NOT truncated.
- `file_read` returning 500 lines → within budget, no truncation.
- Config override: set `file_read = 2000`, verify truncation at 2K.

---

## 6. Deliverables

- [ ] `max_output_tokens` on `ToolDefinition` with per-tool defaults.
- [ ] Router-level truncation after execution.
- [ ] `original_output_tokens` field on `ToolResult` event.
- [ ] `[tool_budgets]` config section.
- [ ] Truncation counter metric.
- [ ] Tests.

---

## 7. Acceptance criteria

1. A `bash` call producing 50K tokens of output appears in the event log at ≤4K tokens with a truncation footer.
2. Error outputs are never truncated.
3. `original_output_tokens` in the event lets step 92's views report "total tokens saved by truncation."
4. `cargo test --workspace` green.

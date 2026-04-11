# Step 46 — Adaptive Tool Ranking from Memory

_Track tool outcomes in workspace memory. Rank and annotate tool definitions based on historical success rates._

---

## 1. What this step is about

MOA has something no other open-source agent has: a persistent file-wiki memory that compounds across sessions. This step uses that memory to make tool selection *adaptive* — the agent learns which tools work well in each workspace and adjusts its behavior accordingly.

The `ToolDefinitionProcessor` (pipeline stage 3) currently injects all tool schemas identically. After this step, it reads tool performance data from workspace memory and:
- Reorders tool definitions so reliable tools appear first (LLMs attend more to early tools)
- Annotates tool descriptions with workspace-specific tips ("npm test typically takes 30s in this project")
- Warns about tools with high failure rates ("web_search has been timing out frequently")
- Deprioritizes tools that consistently fail

---

## 2. Files/directories to read

- **`moa-brain/src/pipeline/tools.rs`** — `ToolDefinitionProcessor`. Where tool definitions are serialized into context.
- **`moa-core/src/types.rs`** — `ToolDefinition`, `ToolPolicySpec`.
- **`moa-memory/src/`** — `FileMemoryStore`. Tool stats stored as a wiki page.
- **`moa-core/src/events.rs`** — `Event::ToolCall`, `Event::ToolResult`, `Event::ToolError`. Data sources for tracking.
- **`moa-brain/src/harness.rs`** — Where tool results are processed. Stats updates happen here.

---

## 3. Goal

After 20 sessions in a workspace, tool definitions the LLM sees look like:

```
bash: Execute shell commands in the workspace sandbox.
  [Workspace note: avg 2.3s, 94% success. Common failure: timeout on npm install — consider npm ci.]

file_read: Read file contents.
  [Workspace note: 99% success. Most-used tool in this workspace.]

web_search: Search the web.
  [Workspace note: 60% success — frequent timeouts. Consider cached docs when possible.]
```

---

## 4. Rules

- **Stats stored in workspace memory** at `entities/tool-stats.md`. Same format as other entity pages.
- **Stats updated after each session**, not after each tool call. Batch updates at session completion.
- **Ranking does not remove tools.** Low success rates get warnings, not removal.
- **No cross-workspace contamination.** Each workspace has its own stats.
- **Stats decay over time.** Exponential moving average with ~30-day half-life. Old failures don't permanently poison a tool.
- **Annotations are concise.** Max 2 lines per tool.
- **Cache stability preserved.** Stats change per-session (not per-turn), so cache hit rates remain high.

---

## 5. Tasks

### 5a. Define `ToolStats` data structure

```rust
pub struct ToolStats {
    pub tool_name: String,
    pub total_calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub avg_duration_ms: f64,
    pub common_errors: Vec<(String, u32)>,  // (error pattern, count)
    pub last_used: DateTime<Utc>,
    pub ema_success_rate: f64,
    pub workspace_tips: Vec<String>,
}

pub struct WorkspaceToolStats {
    pub tools: HashMap<String, ToolStats>,
    pub last_updated: DateTime<Utc>,
    pub sessions_tracked: u64,
}
```

### 5b. Implement stats collection at session end

Aggregate tool call results from session events. Update running stats in workspace memory. Detect common error patterns.

### 5c. Modify `ToolDefinitionProcessor` to inject stats

Load workspace tool stats, rank tools by success rate (highest first), annotate descriptions with workspace-specific notes.

### 5d. Implement ranking logic

Sort: frequently successful first, rarely used middle, frequently failing last. Within tiers, alphabetical for determinism.

### 5e. Implement annotation generation

For tools with >10 calls: success rate, avg duration, most common error pattern (if failure rate > 20%).

### 5f. Implement EMA decay

```rust
fn update_ema(current: f64, observation: f64, alpha: f64) -> f64 {
    alpha * observation + (1.0 - alpha) * current
}
// alpha = 0.1 gives ~30-day half-life with daily sessions
```

### 5g. Store stats as workspace memory page

Write `entities/tool-stats.md` with YAML frontmatter + markdown body containing a performance table.

---

## 6. How it should be implemented

New files:
```
moa-brain/src/pipeline/tools.rs  — Modified (add stats loading + ranking)
moa-brain/src/tool_stats.rs      — New: ToolStats, collection, EMA, annotation
```

Stats update is a post-session hook in `moa-orchestrator/src/local.rs` or `moa-brain/src/harness.rs` — after the `SessionCompleted` event.

---

## 7. Deliverables

- [ ] `moa-brain/src/tool_stats.rs` — `ToolStats`, `WorkspaceToolStats`, collection, EMA, annotation
- [ ] `moa-brain/src/pipeline/tools.rs` — Modified to load stats and rank/annotate tools
- [ ] `moa-brain/src/harness.rs` — Post-session stats update hook
- [ ] Memory page format for `entities/tool-stats.md`

---

## 8. Acceptance criteria

1. After 5+ sessions, `entities/tool-stats.md` exists with per-tool stats.
2. Tool definitions in context are ordered by success rate.
3. Tools with < 80% success rate have a warning annotation.
4. EMA decays old failures — a tool broken last month but working now shows high current rate.
5. No cross-workspace contamination.
6. Sessions with 0 tool calls don't corrupt stats.
7. Cache hit rate is not degraded.

---

## 9. Testing

**Test 1:** `tool_stats_round_trip` — Create stats, serialize to memory, deserialize, verify.
**Test 2:** `ranking_puts_successful_tools_first` — 3 tools: 95%, 60%, 99% → order: 99, 95, 60.
**Test 3:** `annotation_warns_on_low_success` — Tool with 50% rate gets warning.
**Test 4:** `ema_decays_old_failures` — Failed 100% last month, 100% success this week → EMA > 0.5.
**Test 5:** `no_annotation_below_threshold` — Tool with 3 calls gets no annotation.
**Test 6:** `stats_update_from_events` — Feed 10 ToolCall + ToolResult events, verify stats.
**Test 7:** `cache_stability` — Two turns same session, tool definitions byte-identical.

---

## 10. Additional notes

- **This is MOA's differentiator.** No other agent adapts tool definitions based on historical performance. Compounding value.
- **User-editable tips.** Users can manually add tips to `entities/tool-stats.md`. Auto-generated and user sections separated.
- **Future: auto-generated skills from tool patterns.** Frequent successful tool sequences are candidates for skill distillation.

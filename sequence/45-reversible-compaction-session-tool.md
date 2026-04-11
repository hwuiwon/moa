# Step 45 — Reversible Compaction + Session-as-Tool

_Non-destructive context compaction in the HistoryCompiler. Expose session event log as a searchable brain tool._

---

## 1. What this step is about

Two tightly related improvements that solve MOA's context window ceiling:

**Reversible compaction.** Anthropic does irreversible compaction — they summarize and discard. MOA can do better. Keep the full session event log untouched, but have the `HistoryCompiler` serve a *compressed view* to the LLM: a summary of old events plus recent events verbatim. If the summary is wrong, you replay from the full log — no data ever lost. This is a genuine architectural advantage over Anthropic's approach.

**Session-as-tool.** Instead of loading all history into context, let the brain *search* its own session log via a tool call. MOA's `SessionStore::search_events()` already exists but isn't exposed as a brain tool. Adding a `session_search` tool transforms the session from passive storage into an active, queryable context object — the key insight from Anthropic's managed agents post, implemented as a first-class feature.

---

## 2. Files/directories to read

- **`moa-brain/src/pipeline/history.rs`** — Current `HistoryCompiler` implementation. This is where compaction logic lives.
- **`moa-brain/src/compaction.rs`** — Existing compaction stubs/types.
- **`moa-core/src/events.rs`** — `Event` enum, especially `Checkpoint` variant.
- **`moa-core/src/traits.rs`** — `SessionStore::search_events()`, `ContextProcessor` trait.
- **`moa-core/src/types.rs`** — `EventRange`, `WorkingContext`, `ProcessorOutput`.
- **`moa-hands/src/builtin/`** — Where built-in tools live (memory_search, memory_write, etc.). New `session_search` tool goes here.
- **`moa-brain/src/pipeline/mod.rs`** — Pipeline stage ordering.

---

## 3. Goal

After this step:

1. Sessions with 200+ events don't degrade. The HistoryCompiler automatically summarizes older events while preserving recent ones verbatim.
2. The full event log is never modified — compaction produces a *view*, not a mutation.
3. The brain can search its own session history via `session_search` tool — finding past errors, decisions, or tool results without them all being in context.
4. The combination eliminates the context window as a practical limit for long-running sessions.

---

## 4. Rules

- **Never mutate the event log for compaction.** The session store's append-only log is sacred. Compaction produces `Checkpoint` events that *summarize* older events — the originals remain queryable.
- **Compaction is transparent.** The brain doesn't know it's seeing a compacted view. From its perspective, it gets a context window with a summary section and a recent-events section.
- **Errors are always preserved verbatim.** Error events are never summarized — they're the strongest signal for avoiding repeated mistakes.
- **`session_search` is a regular built-in tool.** Same pattern as `memory_search`. The brain calls it like any other tool; it returns formatted event snippets.
- **Compaction threshold is configurable.** Default: trigger when event count since last checkpoint exceeds 100 OR estimated tokens exceed 70% of context window.
- **Compaction uses the LLM** to generate summaries. This costs tokens — the summary generation itself should be tracked and costed.

---

## 5. Tasks

### 5a. Implement automatic compaction trigger in `HistoryCompiler`

Modify `moa-brain/src/pipeline/history.rs` to detect when compaction is needed:

```rust
async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let events = self.store.get_events(ctx.session_id, EventRange::all()).await?;
    let last_checkpoint = find_last_checkpoint(&events);
    let events_since = events_since_checkpoint(&events, &last_checkpoint);
    
    if should_compact(events_since.len(), ctx.token_budget, &ctx.model_capabilities) {
        let summary = self.generate_compaction_summary(&events_since).await?;
        self.store.emit_event(ctx.session_id, Event::Checkpoint {
            summary: summary.clone(),
            events_summarized: events_since.len() as u64,
            token_count: estimate_tokens(&summary),
        }).await?;
    }
    
    // Build context view: checkpoint summary + recent events verbatim
    self.build_compacted_view(ctx, &events).await
}
```

### 5b. Implement `generate_compaction_summary()`

Use the LLM to produce a summary that preserves: all errors and their resolutions, architectural decisions, unresolved items, active file paths, and key facts discovered. Before summarizing, flush important facts to memory.

### 5c. Implement `build_compacted_view()`

```
If checkpoint exists:
  [Checkpoint summary] + [Last N turns verbatim]
  
If no checkpoint:
  [All events, budget-limited from most recent]

Errors are ALWAYS included regardless of age.
```

The "last N turns" count is dynamic — fill remaining budget after the summary.

### 5d. Implement `SessionSearchTool` in `moa-hands/src/builtin/`

```rust
pub struct SessionSearchTool;

impl BuiltInTool for SessionSearchTool {
    fn name(&self) -> &'static str { "session_search" }
    
    fn description(&self) -> &'static str {
        "Search the current session's event history for past tool calls, \
         errors, decisions, or any other events. Use this to recall \
         information from earlier in the session that may no longer \
         be in your context window."
    }
    
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms" },
                "event_type": { 
                    "type": "string", 
                    "enum": ["tool_call", "tool_result", "brain_response", "error", "all"]
                },
                "last_n": { "type": "integer", "description": "Return last N matching events" }
            },
            "required": ["query"]
        })
    }
    
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let query = input["query"].as_str().unwrap_or("");
        let results = ctx.session_store.search_events(query, EventFilter {
            session_id: Some(ctx.session.id.clone()), ..
        }).await?;
        format_event_results(&results)
    }
}
```

### 5e. Register `session_search` in the tool loadout

Add alongside `memory_search` and `memory_write`. Always available — read-only introspection tool.

### 5f. Add compaction config to `MoaConfig`

```toml
[compaction]
enabled = true
event_threshold = 100
token_ratio_threshold = 0.7
recent_turns_verbatim = 5
preserve_errors = true
```

---

## 6. How it should be implemented

The key insight: compaction is a *view transformation* in the pipeline, not a mutation of the event log. The `HistoryCompiler` reads the full log, decides what to show the LLM, and constructs a context that fits the budget. The `Checkpoint` event is just a cached summary — if the summary is bad, regenerate it.

For `session_search`, it wraps `SessionStore::search_events()` which already has FTS5 indexing. The tool formats results for LLM consumption — truncated event payloads with timestamps and types.

The compaction LLM call should use a cheaper/faster model if available (e.g., Haiku for summarization). Make the compaction model configurable.

---

## 7. Deliverables

- [ ] `moa-brain/src/pipeline/history.rs` — Automatic compaction trigger + `build_compacted_view()` + `generate_compaction_summary()`
- [ ] `moa-brain/src/compaction.rs` — Compaction logic extracted (summary generation, error preservation, memory flush)
- [ ] `moa-hands/src/builtin/session_search.rs` — `SessionSearchTool` implementation
- [ ] `moa-core/src/config.rs` — `CompactionConfig` struct
- [ ] Tool registration in `moa-hands/src/builtin/mod.rs`
- [ ] `docs/sample-config.toml` — `[compaction]` section

---

## 8. Acceptance criteria

1. A session with 150+ events produces a Checkpoint event automatically.
2. After compaction, `get_events(session_id, EventRange::all())` still returns all original events.
3. The LLM sees: checkpoint summary + last 5 turns. Not 150+ raw events.
4. Error events from turn 3 are visible in context even after checkpoint at turn 100.
5. Brain can call `session_search({"query": "deploy error"})` and get formatted results from earlier.
6. Compacted view fits within `token_budget` even for sessions with thousands of events.
7. Short sessions (< 100 events) behave identically to before.
8. Compaction cost tracked in session metrics.

---

## 9. Testing

**Test 1:** `compaction_triggers_at_threshold` — Create 150 events, run pipeline, verify Checkpoint emitted.

**Test 2:** `compacted_view_fits_budget` — 500 events, small token budget, verify output tokens < budget.

**Test 3:** `errors_always_preserved` — Create error at event 10, compact at event 150, verify error in context view.

**Test 4:** `full_log_intact_after_compaction` — Compact, then `get_events(all)`, verify all originals present.

**Test 5:** `session_search_finds_old_events` — Create events, search by keyword, verify results.

**Test 6:** `session_search_filters_by_type` — Search with `event_type = "error"`, verify only errors returned.

**Test 7:** `no_compaction_below_threshold` — 50 events, verify no Checkpoint emitted.

**Test 8:** `compaction_config_respected` — Set `event_threshold = 200`, verify no compaction at 150 events.

---

## 10. Additional notes

- **MOA's advantage over Anthropic.** Anthropic's compaction is irreversible — they transform messages and discard originals. MOA keeps the full log and generates views. You can re-compact with a better model later, debug summary quality issues, and never lose data. Strictly better.
- **`session_search` enables a new interaction pattern.** The brain can say "I remember seeing an error about X earlier" and actually search for it, rather than relying on the context window.
- **Compaction model selection.** Using a cheaper model for summarization saves ~90% of compaction cost versus using the main session model. Sufficient for factual extraction.

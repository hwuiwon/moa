# Step 77 — File Read Deduplication in History Replay

_When the same file is read multiple times across turns, keep only the most recent version in the compiled context. Eliminates the dominant source of context pollution in long coding sessions._

---

## 1. What this step is about

In the 2026-04-15 e2e test, the brain read `server/core/views.py` (19K lines) **multiple times** across different turns — to search for patterns, to check its own edits, and to prepare for the next edit. Each read added ~76K tokens to the context history. By turn 15, three reads of the same file consumed ~228K tokens of the context window — more than the total budget of most models.

Claude Code addresses this by deduplicating older file reads: when the same file is read multiple times, only the most recent version is kept in context, with older reads replaced by a short placeholder. This is the single highest-impact context optimization for coding tasks.

The Roo-Code community proposed this as issue #6279 but it was closed without implementation. MOA can be first to ship it properly.

---

## 2. Files to read

- **`moa-brain/src/pipeline/history.rs`** — `compile_messages()` and `event_to_context_message()`. This is where deduplication logic belongs — during history compilation, not at event storage time.
- **`moa-brain/src/compaction.rs`** — Compaction already summarizes old events. Deduplication is complementary: it operates on individual tool results within the unsummarized tail, while compaction summarizes the tail as a whole.
- **`moa-core/src/types/event.rs`** — `Event::ToolCall` and `Event::ToolResult` — need to extract the `path` from `file_read` tool calls.

---

## 3. Goal

After this step:
1. During history compilation, when `file_read` results for the same path appear multiple times, only the **most recent** result is kept verbatim
2. Older reads of the same path are replaced with a short placeholder: `[file previously read — see latest version below]`
3. The deduplication only applies to `file_read` tool results, not to `bash`, `str_replace`, or other tools
4. The deduplication happens at **context compilation time** (stage 6), not at event storage time — the full history is preserved in the session log for replay/debugging

---

## 4. Rules

- **Deduplication is path-keyed.** Two `file_read` calls to the same `path` are considered duplicates. Different line ranges of the same file are NOT deduplicated (they contain different content).
- **Keep the most recent read.** When compiling context, scan tool results in reverse chronological order. The first (most recent) `file_read` for each path keeps its full content. All older reads of the same path get their content replaced with a placeholder.
- **Only deduplicate `file_read` and full-file reads.** Do not deduplicate partial reads (those with `start_line`/`end_line` from Step 73) — they intentionally capture different sections. Do not deduplicate `str_replace`, `bash`, or other tool results.
- **The placeholder must preserve the tool_use_id.** Provider APIs require that every `tool_use` block has a matching `tool_result`. The placeholder replaces the content but keeps the message structure intact.
- **Do not deduplicate within the last 2 turns.** Recent reads are likely relevant to the current task flow. Only deduplicate reads from older turns (turns before `recent_turns_verbatim` in the compaction config).
- **Log deduplication stats.** The `ProcessorOutput` from the HistoryCompiler should report how many file reads were deduplicated and how many tokens were saved.

---

## 5. Tasks

### 5a. Add deduplication pass in `compile_messages`

In `history.rs`, after collecting all context messages from events, run a deduplication pass:

```rust
fn deduplicate_file_reads(
    messages: &mut Vec<ContextMessage>,
    recent_turn_start: usize,
) -> DeduplicationStats {
    let mut stats = DeduplicationStats::default();
    let mut latest_read_index: HashMap<String, usize> = HashMap::new();

    // First pass: find the latest read index for each path
    for (idx, msg) in messages.iter().enumerate() {
        if idx >= recent_turn_start {
            break; // don't track within recent turns
        }
        if let Some(path) = extract_file_read_path(msg) {
            latest_read_index.insert(path, idx);
        }
    }

    // Second pass: replace older duplicates with placeholders
    let mut seen_paths: HashSet<String> = HashSet::new();
    // Iterate in reverse to identify which reads are "latest"
    for idx in (0..recent_turn_start).rev() {
        if let Some(path) = extract_file_read_path(&messages[idx]) {
            if seen_paths.contains(&path) {
                // This is an older read — replace content with placeholder
                let original_tokens = estimate_tokens(&messages[idx].content);
                messages[idx].content = format!(
                    "[file {} previously read — see latest version below]",
                    path
                );
                if let Some(blocks) = &mut messages[idx].content_blocks {
                    *blocks = vec![ToolContent::Text {
                        text: messages[idx].content.clone(),
                    }];
                }
                stats.deduplicated_count += 1;
                stats.tokens_saved += original_tokens.saturating_sub(
                    estimate_tokens(&messages[idx].content)
                );
            } else {
                seen_paths.insert(path);
            }
        }
    }

    stats
}

fn extract_file_read_path(message: &ContextMessage) -> Option<String> {
    // Check if this is a tool_result for a file_read call
    // The path is embedded in the preceding tool_call event
    // OR we can extract it from the tool_result content pattern
    //
    // Implementation depends on how tool results are structured.
    // If the content contains the marker from file_read output,
    // extract the path from the structured data or content pattern.
    None // placeholder — see implementation notes
}

#[derive(Default)]
struct DeduplicationStats {
    deduplicated_count: usize,
    tokens_saved: usize,
}
```

### 5b. Track file_read paths in tool call events

The cleanest approach is to pair `ToolCall` events with their `ToolResult` events during compilation. When a `ToolCall` has `tool_name == "file_read"` and the input contains a `path` field, tag the corresponding `ToolResult` with that path. Then deduplication can look up the path from the tool call:

```rust
fn build_file_read_path_map(events: &[&EventRecord]) -> HashMap<uuid::Uuid, String> {
    let mut map = HashMap::new();
    for record in events {
        if let Event::ToolCall { tool_id, tool_name, input, .. } = &record.event {
            if tool_name == "file_read" {
                if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                    // Only track full-file reads (no start_line/end_line)
                    if input.get("start_line").is_none() {
                        map.insert(*tool_id, path.to_string());
                    }
                }
            }
        }
    }
    map
}
```

### 5c. Integrate into `HistoryCompiler::compile_messages`

After building the initial message list and before returning:

```rust
let file_read_paths = build_file_read_path_map(&visible_events);
let dedup_stats = deduplicate_file_reads(&mut messages, recent_start_in_messages);
if dedup_stats.deduplicated_count > 0 {
    tracing::info!(
        deduplicated = dedup_stats.deduplicated_count,
        tokens_saved = dedup_stats.tokens_saved,
        "deduplicated file read results in history compilation"
    );
}
tokens_used -= dedup_stats.tokens_saved;
```

### 5d. Add deduplication stats to ProcessorOutput

Include in the `ProcessorOutput` metadata:

```rust
output.metadata = json!({
    "file_reads_deduplicated": dedup_stats.deduplicated_count,
    "tokens_saved_by_dedup": dedup_stats.tokens_saved,
});
```

### 5e. Add tests

```rust
#[test]
fn deduplicates_repeated_file_reads() {
    // Create events: read foo.py, read bar.py, read foo.py again
    // Compile messages
    // Assert: first foo.py read has placeholder, second has full content
    // Assert: bar.py read is untouched
}

#[test]
fn does_not_deduplicate_within_recent_turns() {
    // Create events where both reads are within the recent_turns_verbatim window
    // Assert: both reads keep full content
}

#[test]
fn does_not_deduplicate_partial_reads() {
    // Create events: read foo.py lines 1-50, read foo.py lines 100-150
    // Assert: both reads keep full content (different ranges)
}

#[test]
fn preserves_tool_use_id_in_placeholder() {
    // Assert: the placeholder message retains the original tool_use_id
    // so provider APIs don't reject the context for mismatched IDs
}
```

---

## 6. Deliverables

- [ ] `moa-brain/src/pipeline/history.rs` — `deduplicate_file_reads()`, `build_file_read_path_map()`, integration into `compile_messages()`
- [ ] Deduplication stats in `ProcessorOutput` metadata
- [ ] Tracing log for deduplication events
- [ ] Tests covering deduplication, recent-turn exemption, partial-read exemption, and ID preservation

---

## 7. Acceptance criteria

1. A session that reads the same file 3 times across 3 turns compiles only the most recent read verbatim; older reads show `[file previously read — see latest version below]`.
2. Token count in compiled context decreases proportionally — 3 reads of a 500-line file saves ~2,000 tokens.
3. Reads within the last 2 turns (configurable via `recent_turns_verbatim`) are never deduplicated.
4. Partial reads (with `start_line`/`end_line`) are never deduplicated.
5. The session event log retains all full reads unchanged (deduplication is context-compilation-time only).
6. `cargo test -p moa-brain` passes.

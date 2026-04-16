# Step 87 — Incremental HistoryCompiler

_Replace `compile_incremental`'s stub (which delegates to `compile_full`) with an actual incremental path that applies only new events to the snapshot and maintains per-turn cross-cutting state (step 77's file_read dedup set) across turns._

---

## 1. What this step is about

Step 86 set up the snapshot infrastructure: turn N's compiled state is persisted, turn N+1 loads it and receives only the delta events since the snapshot. But `compile_incremental` today is a stub that calls `compile_full` underneath. This step makes the savings real.

The challenge is that some pipeline stages have state that spans turns. `HistoryCompiler` in particular (step 77) has the `file_read` dedup set: "which paths have been read across the whole session." To correctly dedup turn 5's reads against reads from turns 1–4, the compiler must remember the state it had at the end of turn 4.

Done naively — snapshot just the output messages — turn 5's dedup set restarts fresh and dedup is broken.

Done correctly, the snapshot carries forward the dedup state (and any other cross-cutting state), and the incremental compile path applies just the delta events to update it.

---

## 2. Files to read

- `moa-brain/src/pipeline/history.rs` — `compile_messages()`, `deduplicate_file_reads()`, `build_file_read_path_map()`. Step 77's implementation.
- `moa-brain/src/pipeline/mod.rs` — pipeline composition.
- `moa-core/src/types/snapshot.rs` — `ContextSnapshot::file_read_dedup_state` field.
- Step 86's `compile_incremental` stub.

---

## 3. Goal

1. `Pipeline::compile_incremental(snapshot, delta_events)` produces a result byte-identical to `compile_full` for the same event log state. This is the correctness guarantee.
2. On a 10-turn session where each turn adds 5 events, `compile_incremental` does O(5) work per turn (the delta), not O(50).
3. The `file_read_dedup_state` correctly dedups across turns: a read in turn 2 still causes turn 5's read of the same path to be deduped.
4. Snapshot size grows bounded with session length, because dedup state is "names of files read" — a small set even for long sessions.

---

## 4. Rules

- **Byte-identical output.** A property-based test compares `compile_full` and `compile_incremental` on a shared sequence of events; outputs must match exactly. Any divergence is a bug.
- **Delta-only stage work.** Stages 1–4 (static prefix) need not re-run — their output is in the snapshot. Only stage 5 (memory) and stage 6 (history compiler) actually run on incremental compiles.
- **Cross-turn state lives in the snapshot.** Any pipeline stage whose behavior depends on prior-turn state must serialize that state into `ContextSnapshot`. If a stage is "pure" (only depends on current-turn events), nothing to add.
- **Stage 5 (MemoryRetriever) is per-turn dynamic.** It fetches workspace memory based on the most recent user message. Nothing to snapshot — it re-runs from the new delta event's text. This is correct.
- **If the delta is suspiciously large, fall back to full.** Heuristic: if `delta_events.len() > 50`, something went wrong (the snapshot was stale for many turns). Full-replay in that case to self-heal.

---

## 5. Tasks

### 5a. Define `FileReadDedupState`

```rust
// moa-core/src/types/snapshot.rs (extending step 86)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileReadDedupState {
    /// Map from file path to the tool_use_id of the most recent full-file read.
    pub latest_read_by_path: HashMap<String, Uuid>,

    /// Set of tool_use_ids whose content has been replaced with the placeholder.
    pub replaced_tool_use_ids: HashSet<Uuid>,
}

impl FileReadDedupState {
    pub fn record_read(&mut self, path: String, tool_use_id: Uuid, is_full_read: bool) {
        if !is_full_read { return; } // partial reads aren't deduped
        if let Some(prev) = self.latest_read_by_path.insert(path, tool_use_id) {
            self.replaced_tool_use_ids.insert(prev); // older one is now redundant
        }
    }
}
```

### 5b. `HistoryCompiler` split

Factor `compile_messages` into:

```rust
pub fn compile_messages_full(events: &[EventRecord]) -> (Vec<ContextMessage>, FileReadDedupState) {
    let mut state = FileReadDedupState::default();
    let mut messages = Vec::new();

    for event in events {
        apply_event_to_messages(event, &mut messages, &mut state);
    }

    finalize_dedup(&mut messages, &state);
    (messages, state)
}

pub fn compile_messages_incremental(
    prior_messages: Vec<ContextMessage>,
    prior_state: FileReadDedupState,
    delta: &[EventRecord],
) -> (Vec<ContextMessage>, FileReadDedupState) {
    let mut messages = prior_messages;
    let mut state = prior_state;

    for event in delta {
        apply_event_to_messages(event, &mut messages, &mut state);
    }

    finalize_dedup(&mut messages, &state);
    (messages, state)
}

fn apply_event_to_messages(
    event: &EventRecord,
    messages: &mut Vec<ContextMessage>,
    state: &mut FileReadDedupState,
) {
    match &event.event {
        Event::UserMessage { text, .. } => messages.push(ContextMessage::user(text.clone())),
        Event::BrainResponse { text, .. } => messages.push(ContextMessage::assistant(text.clone())),
        Event::ToolCall { tool_id, tool_name, input, .. } => {
            if tool_name == "file_read" {
                if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                    let is_full_read = input.get("start_line").is_none();
                    state.record_read(path.to_string(), *tool_id, is_full_read);
                }
            }
            messages.push(tool_call_to_context_message(event));
        }
        Event::ToolResult { tool_id, output, .. } => {
            messages.push(tool_result_to_context_message(event));
        }
        Event::Checkpoint { .. } => {
            // compaction boundary — drop prior messages; handled by step 88's compaction path
        }
        _ => {} // ignore bookkeeping events
    }
}

fn finalize_dedup(messages: &mut Vec<ContextMessage>, state: &FileReadDedupState) {
    for msg in messages.iter_mut() {
        if let Some(tool_use_id) = msg.tool_use_id() {
            if state.replaced_tool_use_ids.contains(&tool_use_id) {
                msg.replace_with_placeholder_preserving_id();
            }
        }
    }
}
```

Key property: `apply_event_to_messages` is deterministic and pure given its inputs. That's what makes the incremental path correct.

### 5c. Wire into `Pipeline`

`Pipeline::compile_incremental`:

```rust
pub async fn compile_incremental(
    &self,
    snapshot: ContextSnapshot,
    delta_events: Vec<EventRecord>,
) -> Result<PipelineResult> {
    if delta_events.len() > 50 {
        tracing::warn!(n = delta_events.len(), "delta too large; falling back to full replay");
        return self.compile_full(snapshot.session_id).await;
    }

    // Stages 1-4 (static): reuse from snapshot; do NOT re-run
    let system_prompt = snapshot.system_prompt; // already compiled
    let tool_definitions = snapshot.tool_definitions;

    // Stage 5 (memory): re-run on the current user message (fresh, dynamic)
    let memory_messages = self.memory_retriever
        .retrieve_for_latest_message(&delta_events)
        .await?;

    // Stage 6 (history): incremental path
    let (messages, new_dedup_state) = compile_messages_incremental(
        snapshot.messages,
        snapshot.file_read_dedup_state,
        &delta_events,
    );

    // Stage 5.5: runtime context (step 84) — recompute with current clock
    let runtime_ctx = self.runtime_context_processor.emit_for_current_turn().await?;

    // Stage 7 (cache optimizer): recompute breakpoints; BP4 position changes with growth
    let cache_breakpoints = self.cache_optimizer.place_breakpoints(&messages);

    // Assemble WorkingContext
    Ok(PipelineResult {
        working_context: WorkingContext {
            messages: interleave(memory_messages, messages, runtime_ctx),
            system_prompt,
            tool_definitions,
            cache_breakpoints,
            file_read_dedup_state: new_dedup_state,
            // ... token counts etc
        },
        pipeline_output: ProcessorOutput { /* summed from delta work */ },
    })
}
```

### 5d. Correctness property test

```rust
// moa-brain/tests/incremental_matches_full.rs

proptest! {
    #[test]
    fn compile_incremental_matches_compile_full(
        events in prop::collection::vec(arbitrary_event(), 1..30),
        split in 1..=events.len(),
    ) {
        let (prefix, suffix) = events.split_at(split);

        // Full path: compile everything at once
        let full = compile_messages_full(&events);

        // Incremental path: snapshot after prefix, apply suffix
        let (prefix_msgs, prefix_state) = compile_messages_full(prefix);
        let incremental = compile_messages_incremental(prefix_msgs, prefix_state, suffix);

        prop_assert_eq!(full.0, incremental.0, "messages must match exactly");
        prop_assert_eq!(full.1, incremental.1, "dedup state must match exactly");
    }
}
```

`arbitrary_event` is a proptest strategy that generates plausible `Event` variants with realistic field distributions.

### 5e. Performance test

```rust
#[tokio::test]
async fn incremental_is_faster_than_full() {
    let events = generate_session(40, 5); // 40 turns, 5 events each = 200 events

    let t_full = bench(|| compile_messages_full(&events)).await;
    let t_incremental = bench_per_turn(&events, 5).await; // snapshot every 5 events

    assert!(t_incremental < t_full / 3, "incremental should be at least 3× faster for long sessions");
}
```

### 5f. Snapshot size monitoring

Extend step 86's size warning: log if `ContextSnapshot` exceeds 5 MB. Dedup state alone should be tiny (path strings + UUIDs), so large snapshots indicate the message history itself is bloated — which means compaction (step 88) hasn't fired when it should have.

---

## 6. Deliverables

- [ ] `FileReadDedupState` type, serializable, stored in `ContextSnapshot` (from step 86 field).
- [ ] `compile_messages_incremental` implementation.
- [ ] `Pipeline::compile_incremental` uses the incremental history path and reuses snapshot stages 1–4.
- [ ] Property test verifying incremental == full for random event sequences.
- [ ] Performance test confirming incremental is measurably faster for long sessions.
- [ ] Snapshot size warning at 5 MB.

---

## 7. Acceptance criteria

1. `cargo test --workspace` green, including the property test with at least 1000 proptest cases.
2. `compile_incremental` and `compile_full` produce byte-identical outputs on any event sequence.
3. In the step 78 integration test, the `pipeline_compile_ms` on turn 5 (with snapshot hit) is at least 3× smaller than on turn 5 without snapshot (with snapshot disabled via config).
4. Dedup state survives across turn boundaries: a file read in turn 2 causes a turn 5 read of the same path to be replaced with the placeholder.
5. Snapshot size for a 20-turn coding session is < 1 MB; 40-turn < 5 MB (these bound the incremental path's memory footprint).
6. If `delta_events.len() > 50`, the pipeline logs a warning and falls back to full replay; correctness is preserved.

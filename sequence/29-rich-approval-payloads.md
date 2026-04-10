# Step 29: Rich Approval Event Payloads

## What this step is about
The durable `Event::ApprovalRequested` stores only a minimal payload (`request_id`, `tool_name`, `input_summary`, `risk_level`). The rich approval context — parsed parameters, file diffs, normalized "Always Allow" pattern — lives only in the TUI's `RuntimeEvent::ApprovalRequested(ApprovalPrompt)` at runtime. When a session is rehydrated from the event log (switching tabs, resuming from disk, future daemon/remote clients), the full approval card cannot be reconstructed.

This step stores the `ApprovalPrompt` in the durable event log so approval UX is fully replayable from persisted events.

## Files to read
- `moa-core/src/events.rs` — `Event::ApprovalRequested` (minimal shape)
- `moa-core/src/types.rs` — `ApprovalPrompt`, `ApprovalRequest`, `ApprovalField`, `ApprovalFileDiff`, `RuntimeEvent`
- `moa-orchestrator/src/local.rs` — where `ApprovalRequested` events are emitted + `RuntimeEvent` is sent
- `moa-tui/src/runner.rs` — how TUI consumes `RuntimeEvent::ApprovalRequested`
- `moa-tui/src/views/chat.rs` — how approval widgets render from `ApprovalPrompt`
- `moa-brain/src/harness.rs` — where `Event::ApprovalRequested` is originally emitted
- `moa-session/src/turso.rs` — event serialization/storage

## Goal
`Event::ApprovalRequested` stores the full `ApprovalPrompt` (or its serializable equivalent). Any client rehydrating a session from the event log can render a complete approval card with parameters, diffs, and pattern — without needing the live runtime.

## Rules
- The `ApprovalPrompt` struct is already `Serialize + Deserialize`. Use it directly in the event payload — no need for a separate "stored" version.
- The enriched event should be a **superset** of the current shape — keep `request_id`, `tool_name`, `input_summary`, `risk_level` as top-level fields for quick filtering/indexing without deserializing the full prompt.
- File diffs can be large. For the current local-only implementation, store them inline. Add a `// TODO: claim-check pattern for large diffs in cloud mode` comment for later. The threshold (~128KB) optimization is not needed yet for local SQLite.
- Do NOT change `RuntimeEvent::ApprovalRequested` — it should continue carrying `ApprovalPrompt`. The change is that the durable event now also carries it, so both paths have the same data.
- The `ApprovalPrompt` must be constructed in the orchestrator (where both the tool call details and policy context are available), not in the TUI.

## Tasks

### 1. Update `Event::ApprovalRequested` in `moa-core/src/events.rs`
Add the full prompt to the event:
```rust
ApprovalRequested {
    request_id: Uuid,
    tool_name: String,
    input_summary: String,
    risk_level: RiskLevel,
    /// Full approval prompt with parsed parameters, diffs, and pattern.
    prompt: ApprovalPrompt,
},
```

### 2. Update event emission in the orchestrator (`moa-orchestrator/src/local.rs`)
Where the orchestrator currently emits `Event::ApprovalRequested` with the minimal fields, now also include the `ApprovalPrompt`. The `ApprovalPrompt` should already be available at this point since the orchestrator constructs it before sending the `RuntimeEvent`. Find where both the event emission and the RuntimeEvent send happen and ensure the prompt is passed to both.

### 3. Update event emission in the brain harness (`moa-brain/src/harness.rs`)
If the brain harness also emits `ApprovalRequested` events directly, update it to include the prompt. If the brain doesn't have the full prompt (it may only have the raw tool call), then the prompt construction needs to happen before emission. Check whether the orchestrator or the brain is responsible for constructing the `ApprovalPrompt` — it should be the orchestrator since it has access to the policy engine and file system for diff generation.

### 4. Update TUI session rehydration
Where the TUI reconstructs session state from persisted events (resuming a session, switching tabs when the live cache is cold):
- Find the event replay logic that processes `Event::ApprovalRequested`
- Extract the `ApprovalPrompt` from the event instead of constructing a minimal placeholder
- Feed it into the approval widget the same way a live `RuntimeEvent::ApprovalRequested` would

### 5. Update event serialization tests
`Event::ApprovalRequested` now has a larger payload. Verify:
- Serialization round-trip works
- Existing event log queries still work (the `event_type` column is unchanged)
- The FTS index over events handles the larger payload gracefully

### 6. Backward compatibility
Old event logs won't have the `prompt` field. Handle deserialization gracefully:
- Make `prompt` an `Option<ApprovalPrompt>` in the event, OR
- Use `#[serde(default)]` so missing `prompt` deserializes as a default/empty prompt
- When replaying old events without a prompt, fall back to the current minimal card behavior

## Deliverables
```
moa-core/src/events.rs           # Enriched ApprovalRequested event
moa-orchestrator/src/local.rs    # Updated event emission with prompt
moa-brain/src/harness.rs         # Updated if it emits approval events
moa-tui/src/runner.rs            # Session rehydration uses persisted prompt
moa-tui/src/views/chat.rs        # (likely no change — already renders ApprovalPrompt)
```

## Acceptance criteria
1. `Event::ApprovalRequested` carries the full `ApprovalPrompt` including parameters, diffs, and pattern.
2. A session resumed from disk shows the same approval card fidelity as a live session.
3. Old event logs without the `prompt` field deserialize without error (backward compat).
4. The `RuntimeEvent::ApprovalRequested` and `Event::ApprovalRequested` carry the same `ApprovalPrompt` data.
5. Event serialization round-trip passes for the enriched payload.
6. All existing tests pass.

## Tests

**Unit tests (moa-core):**
- `Event::ApprovalRequested` with full `ApprovalPrompt` serializes and deserializes correctly
- `Event::ApprovalRequested` without `prompt` field (old format) deserializes with default/None
- `ApprovalPrompt` with file diffs serializes correctly (larger payload)

**Integration tests (moa-orchestrator):**
- Start a session → trigger a `file_write` tool call requiring approval → verify the emitted `Event::ApprovalRequested` contains the diff in `prompt.file_diffs`
- Resume the session from events only → verify the approval prompt is fully reconstructed

**TUI tests:**
- Simulate session rehydration from stored events → verify approval widget renders with full parameters and diff preview

```bash
cargo test -p moa-core
cargo test -p moa-orchestrator
cargo test -p moa-tui
```

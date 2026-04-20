# Step 86 — Context Snapshot: Eliminate O(N²) Event Replay

_Stop rebuilding the compiled context from the full event log every turn. After each turn, serialize the compiled context as a snapshot. On the next turn, load the snapshot and replay only events since its sequence number. Turn cost becomes O(1) + O(delta)._

---

## 1. What this step is about

Step 80's instrumentation will tell us precisely how many events are replayed per turn. The research report's prediction: `events_replayed` grows linearly with turn number, making a long session O(N²) in event-read work. Even if the per-event cost is tiny, the compounding of full replays during a 40-turn coding session is the dominant contributor to context-compile latency.

LangGraph solves this with `checkpointer`. Workflow engines solve it with event history snapshots. Claude Code implicitly solves it by rebuilding compact context representations internally. MOA currently solves it the simplest way — not at all — and pays every turn.

The fix: after each turn's pipeline run completes, serialize the resulting `WorkingContext` (or a carefully-chosen subset) as a "context snapshot." Key it by session_id and the sequence_num of the last event included. On the next turn, load the snapshot and feed only new events (since that sequence_num) through the incremental compiler path (step 87).

---

## 2. Files to read

- `moa-brain/src/pipeline/mod.rs` — pipeline composition.
- `moa-brain/src/pipeline/history.rs` — stage 6, the expensive one. This is what snapshot skips.
- `moa-orchestrator/src/local.rs` — the turn loop, where we'll load snapshots and persist new ones.
- `moa-session/src/postgres.rs` — schema. New table `context_snapshots` needed.
- `moa-core/src/types/session.rs` — `SessionMeta` may gain a `last_snapshot_seq` field.
- Step 80's output (`events_replayed` counts) — to confirm this is worth doing.

---

## 3. Goal

1. A new `context_snapshots` table (or column on `sessions`) holds the serialized `ContextSnapshot` for each active session.
2. After each turn completes, the orchestrator serializes `WorkingContext` → `ContextSnapshot` and persists it with `last_snapshot_seq = <sequence_num of last event consumed>`.
3. At turn start, the orchestrator loads the snapshot, reads only events with `sequence_num > last_snapshot_seq`, and feeds that delta into an incremental compile path.
4. If snapshot load/deserialize fails, fall back to full replay with a warning log. This is the safety net.
5. Per-turn `events_replayed` count becomes small (the delta since last turn), roughly constant regardless of session length. Step 80's dashboard shows the new curve flattening.

---

## 4. Rules

- **Snapshot is a cache, not truth.** The event log is truth. If snapshots disappear (DB wipe, migration, corruption), the system must rebuild correctly from events. Never write event-log rows that assume a snapshot exists.
- **Snapshot per session per turn.** Overwrite the prior snapshot; don't accumulate. Snapshots are big — hundreds of KB for a mature session.
- **Serialize conservatively.** `WorkingContext` contains a lot. Snapshot only the parts that are expensive to rebuild: compiled message history, dedup state, token counts, cache breakpoint positions. Do not snapshot derived state like LLM request bodies.
- **Version the snapshot format.** Store `format_version: u32` in every snapshot. On load, if the version doesn't match the current code's expected version, discard and rebuild. This lets us evolve the snapshot schema without migration code.
- **Ephemeral store uses in-memory snapshots.** `InMemorySessionStore` (step 83) stores them in its own HashMap. Don't try to make them survive process restart — ephemeral is ephemeral.
- **Snapshot size must be bounded.** If compaction (step 88) shrinks the conversation, snapshots shrink too. If for some reason a snapshot exceeds 10 MB, log a warning — something is probably wrong.

---

## 5. Tasks

### 5a. Define `ContextSnapshot`

```rust
// moa-core/src/types/snapshot.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub format_version: u32,                          // current: 1
    pub session_id: SessionId,
    pub last_sequence_num: SequenceNum,
    pub created_at: DateTime<Utc>,

    // Compiled message history (the output of stage 6)
    pub messages: Vec<ContextMessage>,

    // Per-stage derived state we don't want to recompute
    pub file_read_dedup_state: FileReadDedupState,    // step 77 — running set of seen paths
    pub token_count: usize,
    pub cache_breakpoints: Vec<CacheBreakpointMarker>,

    // Source attribution — tells the incremental compiler "start from here"
    pub stage_inputs_hash: u64,                       // hash of static stages' inputs; for drift detection
}

pub const CONTEXT_SNAPSHOT_FORMAT_VERSION: u32 = 1;
```

### 5b. Schema addition

Add to the Postgres schema (in the same `schema_postgres.rs` where `sessions` and `events` live):

```sql
CREATE TABLE IF NOT EXISTS context_snapshots (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    format_version INTEGER NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS context_snapshots_last_seq ON context_snapshots(session_id, last_sequence_num);
```

`JSONB` is fine at this size. If snapshots exceed a few MB consistently, consider `bytea` with a compression codec (zstd). Defer that optimization.

### 5c. `SessionStore` trait additions

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    // ... existing methods ...

    /// Persist the latest context snapshot for a session. Overwrites prior.
    async fn put_snapshot(&self, session_id: SessionId, snapshot: ContextSnapshot) -> Result<()>;

    /// Load the latest snapshot, or None if no snapshot exists.
    async fn get_snapshot(&self, session_id: SessionId) -> Result<Option<ContextSnapshot>>;

    /// Delete snapshot (used during session delete).
    async fn delete_snapshot(&self, session_id: SessionId) -> Result<()>;
}
```

Implement for `PostgresSessionStore` and `InMemorySessionStore` (from step 83).

### 5d. Turn loop integration

In `run_session_task` within `moa-orchestrator/src/local.rs`:

```rust
// Turn start: try to load snapshot
let snapshot = context.session_store.get_snapshot(context.session_id.clone()).await.ok().flatten();

let pipeline_result = if let Some(snap) = snapshot {
    if snap.format_version == CONTEXT_SNAPSHOT_FORMAT_VERSION
        && snap.stage_inputs_hash == current_stage_inputs_hash(&config, &workspace_instructions)
    {
        // HAPPY PATH: load snapshot + delta
        let delta_events = context.session_store.get_events(
            context.session_id.clone(),
            EventRange { from_seq: Some(snap.last_sequence_num + 1), ..Default::default() },
        ).await?;

        pipeline.compile_incremental(snap, delta_events).await
    } else {
        tracing::warn!(
            session_id = %context.session_id,
            snapshot_version = snap.format_version,
            expected_version = CONTEXT_SNAPSHOT_FORMAT_VERSION,
            "snapshot drift detected; falling back to full replay"
        );
        pipeline.compile_full(context.session_id.clone()).await
    }
} else {
    pipeline.compile_full(context.session_id.clone()).await
}?;

// ... run LLM + tools ...

// Turn end: write new snapshot
let new_snapshot = ContextSnapshot::from_working_context(
    &pipeline_result.working_context,
    context.session_id.clone(),
    last_event_seq_this_turn,
);
if let Err(err) = context.session_store.put_snapshot(context.session_id.clone(), new_snapshot).await {
    tracing::warn!(session_id = %context.session_id, error = %err, "snapshot persist failed; next turn will full-replay");
}
```

### 5e. Pipeline factoring

The pipeline needs two entry points:

```rust
impl Pipeline {
    pub async fn compile_full(&self, session_id: SessionId) -> Result<PipelineResult> { ... }

    pub async fn compile_incremental(
        &self,
        snapshot: ContextSnapshot,
        delta_events: Vec<EventRecord>,
    ) -> Result<PipelineResult> { ... }
}
```

`compile_full` is what exists today. `compile_incremental` is stubbed here and fleshed out by step 87.

For this step alone, `compile_incremental` can call `compile_full` internally while we build snapshot infrastructure, then gradually specialize. Don't try to land steps 86 and 87 together — it's too big a change at once.

### 5f. Delete snapshot on session delete / cancel

When `SessionStatus::Cancelled` is set OR the session is deleted, delete the snapshot too. Otherwise orphaned snapshots accumulate.

Also: on the periodic `prune_empty_sessions` sweep, clean up any orphan snapshots where the session row is gone.

### 5g. Metrics

Add to step 81's turn-latency breakdown:

```
snapshot_load_ms    (time to fetch + deserialize snapshot, or 0 if full-replay)
snapshot_hit        (true/false: did we use a snapshot this turn?)
snapshot_write_ms   (time to serialize + persist new snapshot after turn)
```

### 5h. Tests

- Unit: `ContextSnapshot::from_working_context` round-trips.
- Unit: format-version drift → `compile_incremental` refuses to use, falls through to full.
- Unit: stage_inputs_hash mismatch (e.g., workspace instructions changed between turns) → snapshot discarded.
- Integration (extend step 78): after 3 turns, assert the per-turn `events_replayed` count on turn 3 is smaller than on turn 1 (because only delta is replayed). If using `FullReplayMode` via a config flag, the counts should match.
- Integration: corrupt the snapshot payload on disk mid-session, trigger next turn, assert the warn-log fires and the session completes correctly via full replay.

### 5i. Configuration

Add to `MoaConfig`:

```toml
[context_snapshot]
enabled = true                  # turn off for debugging
max_size_bytes = 10_000_000     # warn at this size
```

---

## 6. Deliverables

- [ ] `ContextSnapshot` type with format-version and stage-inputs-hash.
- [ ] `context_snapshots` Postgres table + schema migration.
- [ ] `SessionStore::{put_snapshot, get_snapshot, delete_snapshot}` implemented for Postgres and InMemory.
- [ ] `Pipeline::compile_incremental` stub (real implementation in step 87).
- [ ] Turn loop load-snapshot-then-delta logic.
- [ ] Fallback to full replay on any snapshot load failure; warn-log on version/hash drift.
- [ ] Snapshot persist at turn end; failure is non-fatal (warn only).
- [ ] Orphan cleanup in `prune_empty_sessions`.
- [ ] Metrics: `snapshot_load_ms`, `snapshot_hit`, `snapshot_write_ms` in turn summary.
- [ ] Tests cover round-trip, version drift, corruption fallback, and O(N) → O(delta) event replay.

---

## 7. Acceptance criteria

1. `cargo test --workspace` green.
2. Step 78 integration test extended: `events_replayed` on turn 5 is less than `events_replayed` on turn 1 (with snapshot enabled). With snapshot disabled via config, the count grows linearly.
3. A 20-turn real session shows step 80's `events_replayed` histogram as flat (roughly constant across turns), not linearly growing. Before this step, it grew linearly.
4. Cumulative `pipeline_compile_ms` (from step 81) across the 20-turn session drops by at least 40% vs. the pre-step-86 baseline.
5. Deleting the snapshot mid-session (for example via manual DB DELETE) and starting a new turn produces a warn log and a correct result from full replay.
6. Format version bump (changing `CONTEXT_SNAPSHOT_FORMAT_VERSION` from 1 to 2 in code) invalidates all existing snapshots cleanly — the next turn in any existing session falls back to full replay, persists a v2 snapshot, and subsequent turns hit the snapshot normally.
7. Snapshot DB rows are removed when the session is cancelled or deleted.

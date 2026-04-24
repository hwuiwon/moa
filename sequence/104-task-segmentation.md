# 104 — Task Segmentation Layer

## Purpose

Introduce the **task segment** as a first-class unit of work within MOA sessions. A single session can contain multiple sequential tasks ("deploy to staging," then "why is this test failing," then "update the README"). Today, MOA treats the entire session as one undifferentiated conversation. This prompt adds in-session segment detection, per-segment state tracking, segment-boundary context refresh, and the Postgres schema to store segment metadata.

End state: every session automatically segments into discrete tasks. Each segment has an intent label (or `undefined`), a start/end boundary, tool calls attributed to it, and is the unit against which resolution will be tracked (prompt 105). The `QueryRewriter` (prompt 101) signals segment transitions. The `SkillInjector` refreshes its manifest at segment boundaries. The `HistoryCompiler` can compact previous segments to avoid context pollution.

## Prerequisites

- Prompt 101 (QueryRewriter) landed and functional — its `QueryRewriteResult` is the primary segment transition signal.
- Prompt 100 (multi-skill composition) landed — SkillInjector has budget-aware manifest emission.
- `moa-session` uses Postgres (`PostgresSessionStore`).
- `moa-orchestrator` uses Restate VOs for Session and SubAgent.

## Read before starting

```
cat moa-core/src/types.rs                           # Event enum, SessionMeta
cat moa-brain/src/pipeline/query_rewrite.rs          # QueryRewriteResult
cat moa-brain/src/pipeline/mod.rs                    # Pipeline stages
cat moa-brain/src/pipeline/skills.rs                 # SkillInjector
cat moa-brain/src/pipeline/memory.rs                 # MemoryRetriever
cat moa-brain/src/pipeline/history.rs                # HistoryCompiler
cat moa-orchestrator/src/objects/session.rs           # SessionVoState
cat moa-orchestrator/src/objects/sub_agent.rs         # SubAgentVoState
cat moa-orchestrator/src/turn/runner.rs               # TurnRunner
cat moa-session/src/schema.rs                        # Postgres migrations
```

## Architecture

### What a task segment is

A task segment is a contiguous slice of turns within a session where the user and agent are working toward one goal. It has:

- A start point (the turn where the user introduced the task)
- An optional intent label (classified or `undefined`)
- Tool calls attributed to it
- A resolution outcome (tracked by prompt 105 — this prompt only creates the structure)
- Duration and cost attributed to it

### Segment transition detection

The `QueryRewriter` (prompt 101) already analyzes each user message against conversation history. Extend its output with `is_new_task: bool` and `task_summary: Option<String>`:

**Signals that indicate a new task (detected by the rewriter):**
1. Topic shift: new entities/concepts not present in the current segment
2. Explicit transition language: "now let's...", "next thing...", "also can you..."
3. Context discontinuity: the message doesn't reference any tool results or agent responses from the current segment

**Signals that indicate continuation (NOT a new task):**
1. Coreference to current work: "that file", "the error above", "try again"
2. Follow-up questions about current results
3. Corrections or refinements of current output
4. "also" or "and" that extends the current task, not starts a new one

### Data flow

```
User message arrives
  → QueryRewriter runs (pipeline stage 5)
  → QueryRewriteResult.is_new_task = true?
    → YES: emit SegmentCompleted for current segment
           emit SegmentStarted for new segment
           refresh SkillInjector manifest for new intent
           optionally compact previous segment in history
    → NO:  continue with current segment, update segment metadata
  → MemoryRetriever uses rewritten query (may use new segment's intent for search)
  → rest of pipeline continues
```

## Steps

### 1. Add segment types to `moa-core/src/types.rs`

```rust
/// Unique identifier for a task segment within a session.
pub type SegmentId = uuid::Uuid;

/// A task segment represents one discrete unit of work within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSegment {
    pub id: SegmentId,
    pub session_id: SessionId,
    pub tenant_id: String,
    pub segment_index: u32,              // 0-based within session
    pub intent_label: Option<String>,    // None = undefined
    pub intent_confidence: Option<f64>,
    pub task_summary: Option<String>,    // short description of the task
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: Option<chrono::DateTime<chrono::Utc>>,
    pub turn_count: u32,
    pub tools_used: Vec<String>,
    pub skills_activated: Vec<String>,
    pub token_cost: u64,
    pub previous_segment_id: Option<SegmentId>,
    // Resolution fields populated by prompt 105
    pub resolution: Option<String>,       // resolved|abandoned|failed|pivoted|unknown
    pub resolution_confidence: Option<f64>,
}

/// Lightweight segment reference stored in session VO state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSegment {
    pub id: SegmentId,
    pub segment_index: u32,
    pub intent_label: Option<String>,
    pub task_summary: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub tools_used: Vec<String>,
    pub skills_activated: Vec<String>,
    pub turn_count: u32,
    pub token_cost: u64,
}
```

### 2. Add segment events to the `Event` enum

```rust
// In the Event enum (which should already be #[non_exhaustive] from prompt 103):

/// A new task segment has started within the session.
SegmentStarted {
    segment_id: SegmentId,
    segment_index: u32,
    task_summary: Option<String>,
    intent_label: Option<String>,
    intent_confidence: Option<f64>,
    previous_segment_id: Option<SegmentId>,
},

/// The current task segment has completed.
SegmentCompleted {
    segment_id: SegmentId,
    segment_index: u32,
    intent_label: Option<String>,
    task_summary: Option<String>,
    turn_count: u32,
    tools_used: Vec<String>,
    skills_activated: Vec<String>,
    token_cost: u64,
    duration_ms: u64,
},
```

### 3. Add `task_segments` table to Postgres schema

In `moa-session/src/schema.rs`, add a migration:

```sql
CREATE TABLE IF NOT EXISTS {schema}.task_segments (
    id              UUID PRIMARY KEY,
    session_id      UUID NOT NULL REFERENCES {schema}.sessions(id),
    tenant_id       TEXT NOT NULL,
    segment_index   INT NOT NULL,
    intent_label    TEXT,
    intent_confidence NUMERIC(4,3),
    task_summary    TEXT,
    started_at      TIMESTAMPTZ NOT NULL,
    ended_at        TIMESTAMPTZ,
    resolution      TEXT,
    resolution_signal TEXT,
    resolution_confidence NUMERIC(4,3),
    tools_used      TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count      INT NOT NULL DEFAULT 0,
    token_cost      BIGINT NOT NULL DEFAULT 0,
    previous_segment_id UUID,
    UNIQUE(session_id, segment_index)
);

CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_intent
    ON {schema}.task_segments (tenant_id, intent_label, resolution);
CREATE INDEX IF NOT EXISTS idx_task_segments_session
    ON {schema}.task_segments (session_id, segment_index);
CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_time
    ON {schema}.task_segments (tenant_id, started_at DESC);
```

### 4. Add segment CRUD to `PostgresSessionStore`

Add methods:

```rust
pub async fn create_segment(&self, segment: &TaskSegment) -> Result<()>;
pub async fn complete_segment(&self, segment_id: SegmentId, update: SegmentCompletion) -> Result<()>;
pub async fn get_active_segment(&self, session_id: SessionId) -> Result<Option<TaskSegment>>;
pub async fn list_segments(&self, session_id: SessionId) -> Result<Vec<TaskSegment>>;
pub async fn update_segment_resolution(&self, segment_id: SegmentId, resolution: &str, confidence: f64) -> Result<()>;
```

### 5. Extend `QueryRewriteResult` with segment transition signal

In the `QueryRewriteResult` struct (from prompt 101), add:

```rust
/// Whether the rewriter detected this message as starting a new task.
pub is_new_task: bool,
/// Short summary of the new task, if is_new_task is true.
pub task_summary: Option<String>,
```

Update the rewriter's LLM prompt to include:
```
- Determine if this message starts a NEW task or continues the current one.
- A new task means the user is asking about something unrelated to the current work.
- Set is_new_task=true only when the topic genuinely shifts, not for follow-up questions.
- If is_new_task=true, provide a short task_summary (1 sentence).
```

Update the JSON schema to include `is_new_task` and `task_summary`.

### 6. Add `SegmentTracker` to the context pipeline

Create `moa-brain/src/pipeline/segments.rs`:

This is NOT a pipeline stage — it's a utility called by the orchestrator at turn boundaries. The `SegmentTracker` reads `QueryRewriteResult` from `ctx.metadata` and decides whether to emit segment events:

```rust
pub struct SegmentTracker;

impl SegmentTracker {
    /// Called after QueryRewriter runs, before MemoryRetriever.
    /// Returns true if a segment transition occurred.
    pub async fn check_transition(
        ctx: &WorkingContext,
        session_store: &dyn SessionStore,
        session_id: SessionId,
        tenant_id: &str,
        current_segment: &Option<ActiveSegment>,
    ) -> Result<Option<SegmentTransition>> {
        let rewrite = ctx.metadata.get("query_rewrite");
        // ... check is_new_task from rewrite result
        // If transition: build SegmentCompleted + SegmentStarted events
    }
}

pub struct SegmentTransition {
    pub completed: SegmentCompleted,
    pub started: SegmentStarted,
}
```

### 7. Wire segment tracking into Session VO

In `moa-orchestrator/src/objects/session.rs`:

Add `current_segment: Option<ActiveSegment>` to `SessionVoState`.

In the `SessionTurnAdapter::build_request` path (which calls `prepare_turn_request`), after the QueryRewriter runs:
1. Check if `is_new_task` is set in the pipeline metadata
2. If yes, call segment completion/creation logic
3. Emit `SegmentStarted` and `SegmentCompleted` events to session store
4. Update `current_segment` in VO state

On first message of a session (no current segment), automatically create the first segment with `segment_index: 0`.

### 8. Update `AgentAdapter` with segment hooks

Add to the `AgentAdapter` trait:

```rust
/// Returns the current active segment, if any.
async fn current_segment(&self, ctx: &ObjectContext<'_>) -> Result<Option<ActiveSegment>, HandlerError>;

/// Records that a tool was used in the current segment.
async fn record_segment_tool_use(
    &self,
    ctx: &ObjectContext<'_>,
    tool_name: &str,
) -> Result<(), HandlerError>;

/// Records that a skill was activated in the current segment.
async fn record_segment_skill_activation(
    &self,
    ctx: &ObjectContext<'_>,
    skill_name: &str,
) -> Result<(), HandlerError>;
```

Implement for both `SessionTurnAdapter` and `SubAgentTurnAdapter`. SubAgents inherit the parent session's segment tracking — they don't create their own segments.

### 9. Wire tool/skill usage tracking into `TurnRunner`

In `TurnRunner::handle_tool_call`, after a successful tool execution, call `adapter.record_segment_tool_use(ctx, &tool_name)`.

In the SkillInjector (or wherever skills are activated via `memory_read`), call `adapter.record_segment_skill_activation(ctx, &skill_name)`.

### 10. Optional: segment-aware history compaction

In `moa-brain/src/pipeline/history.rs`, add a heuristic: when compiling history for a new segment, aggressively summarize (or drop) tool results from previous segments. The detailed tool outputs from "deploy to staging" are noise when the user has moved on to "why is this test failing." Keep only the segment checkpoint summaries for prior segments.

This is an optimization — not required for correctness. Flag it with a config option `compact_prior_segments: bool` (default: true).

### 11. Tests

- Unit: `SegmentTracker` detects transition when `is_new_task=true`
- Unit: `SegmentTracker` does NOT detect transition on follow-up questions
- Unit: First message in session creates segment 0
- Unit: Segment transition creates segment N+1 with correct `previous_segment_id`
- Unit: Tool usage recorded on active segment
- Unit: `SessionVoState` with `current_segment` serializes/deserializes correctly
- Integration: session with 3 tasks → 3 segments in Postgres with correct boundaries
- Integration: SubAgent inherits parent segment, does not create its own

## Files to create or modify

- `moa-core/src/types.rs` — add `SegmentId`, `TaskSegment`, `ActiveSegment`, segment events
- `moa-session/src/schema.rs` — add `task_segments` table migration
- `moa-session/src/postgres.rs` (or equivalent) — add segment CRUD methods
- `moa-brain/src/pipeline/query_rewrite.rs` — extend `QueryRewriteResult` with `is_new_task`, `task_summary`
- `moa-brain/src/pipeline/segments.rs` — new: `SegmentTracker`
- `moa-brain/src/pipeline/mod.rs` — add `pub mod segments`
- `moa-orchestrator/src/objects/session.rs` — add `current_segment` to `SessionVoState`, wire transitions
- `moa-orchestrator/src/turn/adapter.rs` — add segment hooks to `AgentAdapter`
- `moa-orchestrator/src/turn/runner.rs` — call `record_segment_tool_use` after tool execution
- `moa-orchestrator/src/services/session_store.rs` — expose segment CRUD as Restate service methods
- `moa-brain/src/pipeline/history.rs` — optional: segment-aware compaction

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] A session with one task produces exactly one segment in `task_segments`.
- [ ] A session where the user switches topics mid-conversation produces multiple segments.
- [ ] Each segment has correct `tools_used` and `skills_activated` arrays.
- [ ] `SegmentStarted` and `SegmentCompleted` events appear in the session event log.
- [ ] SubAgents do NOT create their own segments (they attribute to the parent's).
- [ ] A single-message session (no topic switch) still creates one segment.
- [ ] The `resolution` column is NULL for all segments (filled by prompt 105).

## Notes

- **The segment boundary is detected by the QueryRewriter, NOT by a separate classifier.** The rewriter already analyzes the query in the context of conversation history — adding `is_new_task` is a natural extension, not a new LLM call.
- **Segments are sequential, not nested.** Sub-agents don't create sub-segments. They contribute to the parent session's current segment. If we need nesting later, it's additive.
- **The `task_summary` field is optional and best-effort.** If the rewriter can't summarize, the segment still works — it just has a NULL summary. The intent classifier (prompt 106) fills intent labels later.
- **Do NOT block on segment creation.** Segment writes to Postgres should be fire-and-forget from the hot path. The `TurnRunner` should not wait for segment persistence before continuing to the LLM call. Use Restate's side-effect mechanism for durability without blocking.

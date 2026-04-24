# 05 — Session & Event Log

_Postgres schema, append-only events, task segments, replay, and compaction._

## Storage

MOA uses Postgres for session storage in both local and cloud modes. Local development uses the repo Postgres dev stack; cloud deployments use managed Postgres/Neon. The `moa-session` crate owns migrations and the `PostgresSessionStore`.

Postgres stores:

- session metadata
- append-only event records
- approval rules
- pending signals
- context snapshots
- task segments
- tenant intents and global catalog intents
- learning log entries
- analytics views and materialized views

## Core Tables

The current migration lives in `moa-session/src/schema.rs`. The important tables are:

```sql
CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT NOT NULL,
    platform_channel TEXT,
    model TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    parent_session_id UUID REFERENCES sessions(id),
    total_input_tokens_uncached BIGINT DEFAULT 0,
    total_input_tokens_cache_write BIGINT DEFAULT 0,
    total_input_tokens_cache_read BIGINT DEFAULT 0,
    total_input_tokens BIGINT GENERATED ALWAYS AS (
        COALESCE(total_input_tokens_uncached, 0)
      + COALESCE(total_input_tokens_cache_write, 0)
      + COALESCE(total_input_tokens_cache_read, 0)
    ) STORED,
    total_output_tokens BIGINT DEFAULT 0,
    total_cost_cents BIGINT DEFAULT 0,
    event_count BIGINT DEFAULT 0,
    turn_count BIGINT DEFAULT 0,
    cache_hit_rate DOUBLE PRECISION GENERATED ALWAYS AS (...) STORED,
    last_checkpoint_seq BIGINT
);

CREATE TABLE events (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    sequence_num BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    brain_id UUID,
    hand_id TEXT,
    token_count INTEGER,
    search_vector TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(event_type, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(payload::text, '')), 'B')
    ) STORED,
    UNIQUE(session_id, sequence_num)
);

CREATE TABLE task_segments (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    segment_index INT NOT NULL,
    intent_label TEXT,
    intent_confidence NUMERIC(4,3),
    task_summary TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    resolution TEXT,
    resolution_signal TEXT,
    resolution_confidence NUMERIC(4,3),
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    previous_segment_id UUID,
    UNIQUE(session_id, segment_index)
);
```

The event table uses a generated `tsvector` column and a GIN index for cross-session search. There is no separate application-side rollup writer for session counters; the trigger and generated columns own aggregate updates.

## Event Types

`moa-core/src/events.rs` defines the serialized event enum. Current major groups:

| Group | Events |
|---|---|
| Session lifecycle | `SessionCreated`, `SessionStatusChanged`, `SessionCompleted` |
| Task segmentation | `SegmentStarted`, `SegmentCompleted` |
| User input | `UserMessage`, `QueuedMessage` |
| Brain output | `BrainThinking`, `BrainResponse`, `CacheReport` |
| Tools | `ToolCall`, `ToolResult`, `ToolError` |
| Approvals | `ApprovalRequested`, `ApprovalDecided` |
| Memory | `MemoryRead`, `MemoryWrite`, `MemoryIngest` |
| Hands | `HandProvisioned`, `HandDestroyed`, `HandError` |
| Compaction | `Checkpoint` |
| Diagnostics | `Error`, `Warning` |

`SegmentStarted` records segment ID, index, summary, intent label/confidence, and previous segment ID. `SegmentCompleted` records final counters and duration.

## Task Segment Rows

`task_segments` is the queryable state for segment analytics and learning. It stores the current or final state for each segment:

- tenant and session scope
- segment index and previous segment edge
- optional intent classification
- task summary
- start/end timestamps
- resolution label, confidence, and serialized signal breakdown
- tools and skills used
- turn and token counters

Materialized views derived from `task_segments` include:

- `skill_resolution_rates`
- `intent_transitions`
- `segment_baselines`

These feed skill ranking, intent transition analysis, and structural resolution scoring.

## Learning Tables

The session schema also owns:

- `tenant_intents`
- `global_intent_catalog`
- `learning_log`

Learning log rows are append-only records with tenant ID, learning type, target, payload, confidence, source refs, actor, validity interval, optional batch ID, and version. Rollback invalidates rows by setting `valid_to`; it does not delete history.

## Replay

Replay is history-first:

1. Load session metadata.
2. Load event records ordered by `sequence_num`.
3. Reconstruct visible messages, tool state, approvals, and checkpoints.
4. Attach to live runtime streams when available.

The local orchestrator can publish live runtime events. Cloud runtime state is queryable through Restate and recoverable from the durable event log.

## Compaction

Compaction is segment-aware because segment start/completion events remain durable boundaries. The history compiler uses checkpoints and recent events to stay under model context limits while preserving:

- recent turns
- errors and warnings
- active tool context
- segment boundaries
- unresolved approvals
- checkpoint summaries

The compactor stage can create checkpoint events, but it does not remove event history from Postgres.

## Analytics

Session rollups come from generated columns and triggers. Views and materialized views support operational reads and learning:

- `session_summary`
- `tool_call_analytics`
- `tool_call_summary`
- `session_turn_metrics`
- `daily_workspace_metrics`
- `skill_resolution_rates`
- `intent_transitions`
- `segment_baselines`

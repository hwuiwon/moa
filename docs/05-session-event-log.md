# 05 — Session & Event Log

_Postgres schema, append-only events, task segments, replay, and compaction._

## Storage

MOA uses Postgres for session storage in both local and cloud modes. Local development uses the repo Postgres dev stack; cloud deployments use managed Postgres/Neon. The `moa-session` crate owns the session-store runtime code; session/event DDL lives in the central `moa-migrations` refinery baseline.

Postgres stores:

- session metadata
- append-only event records
- action policy rules
- pending signals
- context snapshots
- task segments
- learning log entries
- live behavior experiment run metadata
- graph changelog outbox rows and per-workspace changelog versions
- analytics views and materialized views

## Core Tables

The session schema baseline lives in `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql`. Production applies central refinery migrations once. Schema-isolated tests replay the session baseline through `moa_migrations::run_session_schema`. The important tables are:

```sql
CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    CONSTRAINT sessions_user_id_nonempty CHECK (btrim(user_id) <> ''),
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    channel TEXT NOT NULL DEFAULT 'chat',
    active_channel_binding_id UUID,
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
    last_checkpoint_seq BIGINT,
    contact_id UUID,
    contact_tenant_id UUID,
    contact_state TEXT,
    contact_canonical_id UUID,
    contact_linked_ids UUID[] NOT NULL DEFAULT '{}',
    contact_scopes TEXT[] NOT NULL DEFAULT '{}',
    created_by_actor_type TEXT,
    created_by_actor_id UUID,
    contact_promoted_from_id UUID
);

CREATE TABLE session_agent_context (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    agent_id UUID,
    installation_uid UUID,
    deployment_uid UUID,
    agent_definition_ref TEXT NOT NULL,
    agent_revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid),
    policy_hash TEXT NOT NULL,
    display_name TEXT NOT NULL,
    policy_snapshot JSONB NOT NULL,
    artifact_dependencies JSONB NOT NULL DEFAULT '[]'::JSONB,
    tool_dependencies JSONB NOT NULL DEFAULT '[]'::JSONB
);

CREATE TABLE contact_channel_accounts (...);
CREATE TABLE session_channel_bindings (...);

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
    task_summary TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    outcome TEXT,
    assessment TEXT,
    outcome_confidence NUMERIC(4,3),
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    previous_segment_id UUID,
    UNIQUE(session_id, segment_index)
);
```

`contact_channel_accounts` stores provider-native identities or delivery
accounts for a contact. Email and SMS accounts reference a `contact_point_id`
rather than duplicating raw addresses or phone numbers. `session_channel_bindings`
stores the active and historical route for a session with normalized lookup
columns such as channel, external tenant key, external conversation key, and
external thread key. The current active binding is also referenced from
`sessions.active_channel_binding_id`.

Every committed session must have one `session_agent_context` row and a
non-empty `sessions.user_id`. `PostgresSessionStore::create_session` enforces
this before insert, and the database also has a deferred constraint trigger so
raw SQL or future transaction paths cannot commit a session without the agent
sidecar. Existing valid sessions are backfilled to the built-in
`agent://system-default` revision during the tenant-configurable agents
migration; tenant-authored sessions should use an installed or explicitly
selected agent revision instead.

The event table uses a generated `tsvector` column and a GIN index for cross-session search. There is no separate application-side rollup writer for session counters; the trigger and generated columns own aggregate updates.

Context compilation preserves event provenance in-memory when replaying session
history. Compiled context messages can carry the source event id, event sequence
number, and tool id; context lineage copies those references so citations can be
joined back to the durable event rows without parsing rendered prompt text.

Contact-bound sessions persist contact metadata on the `sessions` row. The
session id is the observability anchor for turns and tool calls; the contact id
is derived from the session metadata when needed. A contact may exist without a
session, but a contact session always carries the contact binding. Promotion
updates the session to the verified contact and records the prior contact in
`contact_promoted_from_id` while preserving linked contact ids for replay.

## Event Types

`crates/moa-core/src/events.rs` defines the serialized event enum. Current major groups:

| Group | Events |
|---|---|
| Session lifecycle | `SessionCreated`, `SessionStatusChanged`, `SessionCompleted` |
| Task segmentation | `SegmentStarted`, `SegmentCompleted` |
| User input | `UserMessage`, `QueuedMessage` |
| Brain output | `BrainThinking`, `BrainResponse`, `CacheReport` |
| Tools | `ToolCall`, `ToolResult`, `ToolError` |
| Action review | `ActionReviewRequested`, `ActionReviewDecided` |
| Memory | `MemoryRead`, `MemoryWrite`, `MemoryIngest` |
| Hands | `HandProvisioned`, `HandDestroyed`, `HandError` |
| Compaction | `Checkpoint` |
| Diagnostics | `Error`, `Warning` |

`SegmentStarted` records segment ID, index, summary, and previous segment ID. `SegmentCompleted` records final counters and duration.

## Task Segment Rows

`task_segments` is the queryable state for segment analytics and learning. It stores the current or final state for each segment:

- tenant and session scope
- segment index and previous segment edge
- task summary
- start/end timestamps
- assessed outcome, confidence, and serialized evidence
- tools and skills used
- turn and token counters

Materialized views derived from `task_segments` include:

- `skill_resolution_rates`
- `segment_baselines`

These feed skill ranking and structural segment assessment.

## Learning Tables

The session schema also owns `learning_log`.

Learning log rows are append-only records with tenant ID, learning type, target, payload, confidence, source refs, actor, validity interval, optional batch ID, and version. Rollback invalidates rows by setting `valid_to`; it does not delete history.

## Live Behavior Experiment Tables

Live behavior experiment runs are stored in a dedicated ledger instead of being
encoded as session events, `analytics.scores`, or artifact workflow runs.

`analytics.score_run` is the FK-able parent for a scored run. It stores the
score run UUID, three-tier scope columns, source label, and timestamps. Eval,
experiment, and future scored run types can attach many `analytics.scores` rows
to one parent run ID while preserving scoped reads.

`moa.experiment_run` stores one live behavior experiment run:

- `run_uid` is the public experiment run identifier.
- `workspace_id`, `user_id`, generated `scope`, and three-tier RLS match the
  artifact and learning-candidate model.
- `target_kind` is `agent_loop` or `workflow`.
- `status` is `accepted`, `dispatched`, `running`,
  `completed`, `failed`, or `cancelled`.
- `target`, `variant`, and `scorecard` are the accepted experiment payloads.
- `score_run_id` references `analytics.score_run(run_id)` and is the join key
  for `analytics.scores`.
- `session_id` references `sessions(id)` for agent-loop runs or workflow runs
  associated with a session.
- `workflow_run_uid` references `moa.artifact_run(run_uid)` for
  artifact-backed workflow experiments.
- `artifact_revision_uids` is the fast-read list of pinned artifact revisions.
- `idempotency_key`, `created_by_identity`, `error`, and timestamps describe
  admission, ownership, and terminal state.

`moa.experiment_run_artifact_revision` is the enforceable many-to-many link
from an experiment run to pinned `moa.artifact_revision` rows. The denormalized
`artifact_revision_uids` array on `moa.experiment_run` exists for API reads; it
does not replace the FK table.

`moa.experiment_trial` stores per-trial behavior-lab execution. It belongs to a
run, carries the workspace/user scope, trial key, status, target kind, variant
key, pinned plan revision, selected persona/profile/scenario/data-bundle IDs,
simulator settings, session or workflow run link, score run ID, turn count,
stop reason, error, trace ID, and timestamps. `ExperimentTrialRun` updates this
row as simulator turns dispatch target sessions or workflow runs.

The `Experiments` service exposes `generate_plan`, `run`, `status`, `list`,
`trials`, `trial_status`, `cancel`, `propose_improvements`, `scores`, and
`compare`. `generate_plan`, `run`, `cancel`, and `propose_improvements` require
`Workspace:Editor`. `status`, `list`, `trials`, `trial_status`, `scores`, and
`compare` require `Workspace:Member`. `Analytics/experiment_stats` also
requires `Workspace:Member`. These service checks sit above the RLS scope on
`moa.experiment_run`, `moa.experiment_trial`,
`moa.experiment_run_artifact_revision`, and `analytics.score_run`.

## Graph Changelog

`moa.graph_changelog` is the immutable outbox for graph-memory mutations. Every node or edge create, update, supersession, invalidation, erase, or crypto-shred writes a changelog row in the same transaction as the graph change. An insert trigger bumps `moa.workspace_state.changelog_version`, which cache invalidation uses as the per-workspace freshness marker.

The changelog is monthly range-partitioned, protected by FORCE RLS, and grants application roles only `SELECT` and `INSERT`. `moa_changelog_pub` publishes the table for Debezium PostgreSQL CDC; `moa_replicator` consumes it through the `moa_changelog_slot` logical replication slot.

## Replay

Replay is history-first:

1. Load session metadata.
2. Load event records ordered by `sequence_num`.
3. Reconstruct visible messages, tool state, action reviews, and checkpoints.
4. Attach to live runtime streams when available.

The orchestrator publishes live runtime events during turn execution. Cloud runtime state is queryable through Restate and recoverable from the durable event log.

Replay uses persisted session contact metadata; clients cannot provide a new
contact per message to change historical attribution. Tool-call records only
need the session id because the session store can recover the contact binding.

## Compaction

Compaction is segment-aware because segment start/completion events remain durable boundaries. The history compiler uses checkpoints and recent events to stay under model context limits while preserving:

- recent turns
- errors and warnings
- active tool context
- segment boundaries
- pending action reviews
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
- `segment_baselines`

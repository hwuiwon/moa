# 05 — Session & Event Log

_Postgres schema, append-only events, task segments, replay, and compaction._

## Storage

MOA uses Postgres for session storage in both local and cloud modes. Local development uses the repo Postgres dev stack; cloud deployments use managed Postgres/Neon. The `moa-session` crate owns the session-store runtime code; session/event DDL lives in the central `moa-migrations` refinery baseline.

Postgres stores:

- session metadata
- append-only event records
- session-event idempotency dedupe rows (`session_event_dedupe`)
- action policy rules
- pending signals
- context snapshots
- task segments
- learning log entries
- live behavior experiment run metadata
- execution run/task state, immutable plan snapshots, and completion results
- normalized execution route, planner-call, and compiler audit records
- graph changelog outbox rows and per-tenant changelog versions
- large event payload claim-check blobs
- durable hand leases for sandbox reuse and cleanup
- analytics views and materialized views

## Core Tables

The session schema baseline lives in `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql`. Production and test-template staging databases use the same canonical `moa_migrations::run` refinery path. Session tests clone a fully migrated physical database with `CREATE DATABASE ... TEMPLATE` and use its `public` schema; they do not replay a curated migration subset into synthetic schemas. The important tables are:

```sql
CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
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
    contact_state TEXT,
    contact_canonical_id UUID,
    created_by_actor_type TEXT,
    created_by_actor_id UUID,
    contact_promoted_from_id UUID
);

CREATE TABLE session_agent_context (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL,
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
    id UUID NOT NULL,
    session_id UUID NOT NULL REFERENCES sessions(id),
    tenant_id UUID NOT NULL,
    contact_id UUID,
    sequence_num BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    brain_id UUID,
    hand_id TEXT,
    token_count INTEGER,
    PRIMARY KEY (id, session_id),
    UNIQUE(session_id, sequence_num)
) PARTITION BY HASH (session_id);

CREATE TABLE task_segments (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
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

CREATE INDEX idx_task_segments_scope
    ON task_segments (storage_partition_id, scope, user_id);
```

`contact_channel_accounts` stores provider-native identities or delivery
accounts for a contact. Email and SMS accounts reference a `contact_point_id`
rather than duplicating raw addresses or phone numbers. `session_channel_bindings`
stores the active and historical route for a session with normalized lookup
columns such as channel, external tenant key, external conversation key, and
external thread key. The current active binding is also referenced from
`sessions.active_channel_binding_id`.

Every committed session must have one `session_agent_context` row, one
`tenant_id`, and either contact attribution or an admin/operator creator actor.
`PostgresSessionStore::create_session` enforces this before insert, and the
database also has a deferred constraint trigger so raw SQL or future transaction
paths cannot commit a session without the agent sidecar. Existing valid
sessions are backfilled to the built-in `agent://system-default` revision during
the tenant-configurable agents migration; tenant-authored sessions should use an
installed or explicitly selected agent revision instead.

The event table is HASH-partitioned on `session_id` across 16 child tables to spread append contention, which is why the primary key is `(id, session_id)`. There is no stored `search_vector` `tsvector` column or GIN index: the rare, admin-only cross-session search computes `to_tsvector` on the fly instead of taxing every hot-path append. There is no separate application-side rollup writer for session counters; the trigger and generated columns own aggregate updates.

Context compilation preserves event provenance in-memory when replaying session
history. Compiled context messages can carry the source event id, event sequence
number, and tool id; context lineage copies those references so citations can be
joined back to the durable event rows without parsing rendered prompt text.

Large event payloads use claim-check storage before the event is committed.
The default cloud backend is Postgres (`session_blobs`) so a replay on another
pod can resolve the blob reference. The local filesystem backend is explicit
and requires a persistent mounted path.

User-visible uploads are separate from claim-check blobs. Contact message
uploads store metadata and object keys in `session_attachments`, while bytes
are stored through `object_store` in RustFS locally or AWS S3/GCS in cloud.
`UserMessage.attachments` stores `Attachment` metadata with durable attachment
ids. Replaying a session does not require local pod state; any edge or worker
pod can resolve the attachment metadata from the event and the bytes from the
configured object store.

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
| Session lifecycle | `SessionCreated`, `SessionStatusChanged` |
| Task segmentation | `SegmentStarted`, `SegmentCompleted` |
| User input | `UserMessage`, `QueuedMessage` |
| Brain output | `BrainThinking`, `BrainResponse`, `CacheReport` |
| Tools | `ToolCall`, `ToolResult`, `ToolError` |
| Action review | `ActionReviewRequested`, `ActionReviewDecided` |
| Memory | `MemoryRead`, `MemoryWrite`, `MemoryIngest` |
| Worker coordination | `WorkerSpawned`, `WorkerMessageSent`, `WorkerStatusChanged`, `WorkerNotificationDelivered`, `WorkerSignalReceived`, `WorkerParentResumeRequested`, `WorkerHeartbeatStale`, `ProgressNarrated` |
| Execution runs | `ExecutionRunStarted`, `ExecutionProgress`, `ExecutionInputRequired`, `ExecutionCompleted`, `ExecutionFailed`, `ExecutionCancelled`, `ExecutionSynthesisRequested` |
| Compaction | `Checkpoint` |
| Diagnostics | `Error`, `Warning` |

The serialized enum is `Event` (not `SessionEvent`); each variant uses
`#[serde(tag = "type", content = "data")]` with snake_case field names.

Execution planning evidence is not a session event. Route decisions, planner
calls, and compiler outcomes are written directly to the normalized
`moa.execution_route_audit`, `moa.execution_planner_call_audit`, and
`moa.execution_compile_audit` tables. This keeps internal prompts, candidates,
and compiler reports out of model-visible history, session search, compaction,
contact progress, and public SSE by construction.

The route audit accepts only the normalized decision/strategy matrix:

| Stage | Decision | Strategy | Source |
|---|---|---|---|
| Initial | Respond | none | classifier |
| Initial | NeedsInput | none | blank-objective preflight or classifier |
| Initial | Execute | Inline | classifier, including every classifier fallback |
| Initial | Execute | Durable | classifier or selected execution template |
| Durable upgrade | Execute | Durable | workflow-owned `request_durable_execution` control transition |

The row retains typed source and classifier-outcome provenance plus bounded
model, prompt, hash, confidence, usage, cost, and duration metadata. Classifier
rationale is session-local and ephemeral: neither route audits nor durable run
facts store it. They also never store raw objective or classifier response text.

`SegmentStarted` records segment ID, index, summary, and previous segment ID. `SegmentCompleted` records final counters and duration.

`WorkerSpawned`, `WorkerMessageSent`, `WorkerStatusChanged`, and
`WorkerNotificationDelivered` are the pre-existing spawn/steer/terminal events.
The durable main-agent/worker coordination feature adds four:

- `WorkerSignalReceived` records one control-plane attention signal
  (`ChildSignalKind` = `Finding`/`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale`)
  recorded on the owning coordinator, with `signal_id`, severity, summary, and —
  for `NeedsInput` — the awakeable `input_request_id` and `input_audience`.
- `WorkerParentResumeRequested` records that a signal triggered a guarded
  coordinator auto-resume turn (`signal_id`, `worker_id`, `turn_id`, `reason`).
- `WorkerHeartbeatStale` records a watchdog stale detection
  (`last_heartbeat_at`, `threshold_ms`).
- `ProgressNarrated` is one durable, rate-limited natural-language progress
  update for the whole session (`source`, `text`, optional per-child `segments`,
  `model`, `tokens_used`). It is the one intentional low-rate telemetry event;
  `model = "none"`/`tokens_used = 0` marks the no-LLM N=1 short-circuit.

`ProgressUpdate` remains a decodable event variant for old rows, but new turn
progress is projection state surfaced through `TurnExecution/progress` and
`Session/progress`; it is not appended as a per-tick event-log row. Heartbeats
stay in `Worker` VO state and append an event only on the stale transition above.

Execution events link the owning session to compact run state. Progress events
are cadence/delta limited aggregates rather than task heartbeats. Terminal
events carry the run ID, status, coverage summary, compact output/citations, and
explicit gaps; full task results remain in execution persistence. A guarded
`ExecutionSynthesisRequested` event causes at most one final synthesis turn for
the originating user sequence and run ID.

## Execution Run Tables

`moa.execution_run` and `moa.execution_task` are the product source of truth for
durable typed DAG work. They are separate from session events and protected by
the same tenant/contact/admin scope rules.

An execution-run row stores its immutable `ExecutionGoalContract`, canonical
initial plan, active plan, plan revision and append-only amendment history,
plan hashes, skill-template or compiled-plan provenance, input/output,
completion-check evidence, terminal gaps, status, integer budget and usage,
aggregate counters, owning session/tenant/user scope, idempotency key, and
timestamps. It cannot be marked `completed` while a required deliverable,
coverage item, schema check, citation requirement, or budget/deadline check is
unsatisfied.

An execution-task row stores one logical node or map-item instance, unique by
`(run_uid, node_id, item_key)`. It records requirement IDs, plan revision,
status, attempt, generation fence, input/output/error, reserved and actual
usage, citations, and timestamps. Atomic SQL reserves every worst-case budget
dimension before dispatch. Generation-fenced completion prevents a stale retry
from overwriting current state; cancellation prevents new reservations while
leaving completed results queryable.

## Idempotent Append

Control-plane signals, the heartbeat watchdog, and progress narration all run in
Restate handlers that can be retried after a partial failure, so their appends
must be idempotent. `AppendEventRequest` carries an optional `dedupe_key`, and
`emit_event_record` enforces it inside the same `sessions ... FOR UPDATE` lock and
transaction that guards the event insert: when a `dedupe_key` is present and a row
already exists for `(session_id, dedupe_key)`, the original `sequence_num` is
returned and no second event is inserted; otherwise the event and the dedupe row
are inserted together. Callers that pass no key always append. Enforcing this in
`emit_event_record` (not only on the Restate wire path) means direct callers get
the guarantee too.

Dedupe state lives in a separate `session_event_dedupe(session_id, dedupe_key,
sequence_num, created_at)` table whose primary key doubles as the uniqueness
guard (migration `V000318__session_event_dedupe.sql`). It is kept off the hot, trigger-heavy, append-only
`events` table on purpose: adding a unique index to `events` would need a
write-blocking, non-concurrent `CREATE UNIQUE INDEX` (refinery runs each
migration in a transaction), which stalls writes during deploy. The dedupe path
is INSERT-only and never weakens append-only semantics. Stable dedupe keys
include `worker_signal:{signal_id}`, `worker_stale:{worker_id}:{last_heartbeat_at_ms}`,
and `narration:{session_id}:{narration_seq}`.

## Task Segment Rows

`task_segments` is the queryable state for segment analytics and learning. It stores the current or final state for each segment:

- storage partition, generated scope tier, optional user, tenant, and session
  scope
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
encoded as session events, `analytics.scores`, or execution runs.

`analytics.score_run` is the FK-able parent for a scored run. It stores the
score run UUID, tenant attribution for runtime scoring, source label, and
timestamps. Eval, experiment, and future scored run types can attach many
`analytics.scores` rows to one parent run ID while preserving scoped reads.

`moa.experiment_run` stores one live behavior experiment run:

- `run_uid` is the public experiment run identifier.
- `tenant_id` is the runtime isolation key; creator actor fields record the
  admin/operator principal that admitted the run.
- `target_kind` is `agent_loop` or `execution_run`.
- `status` is `accepted`, `dispatched`, `running`,
  `completed`, `failed`, or `cancelled`.
- `target`, `variant`, and `scorecard` are the accepted experiment payloads.
- `score_run_id` references `analytics.score_run(run_id)` and is the join key
  for `analytics.scores`.
- `session_id` references `sessions(id)` for agent-loop or session-linked
  execution runs.
- `execution_run_uid` references `moa.execution_run(run_uid)` for execution
  experiments.
- `artifact_revision_uids` is the fast-read list of pinned artifact revisions.
- `idempotency_key`, `created_by_identity`, `error`, and timestamps describe
  admission, ownership, and terminal state.

`moa.experiment_run_artifact_revision` is the enforceable many-to-many link
from an experiment run to pinned `moa.artifact_revision` rows. The denormalized
`artifact_revision_uids` array on `moa.experiment_run` exists for API reads; it
does not replace the FK table.

`moa.experiment_trial` stores per-trial behavior-lab execution. It belongs to a
run, carries the tenant and optional contact attribution, trial key, status,
target kind, variant key, pinned plan revision, selected
persona/profile/scenario/data-bundle IDs, simulator settings, session or
execution-run link, score run ID, turn count, stop reason, error, trace ID, and
timestamps. `ExperimentTrialRun` updates this row as simulator turns dispatch
target sessions or execution runs.

The `Experiments` service exposes `generate_plan`, `run`, `status`, `list`,
`trials`, `trial_status`, `cancel`, `propose_improvements`, `scores`, and
`compare`. `generate_plan`, `run`, `cancel`, and `propose_improvements` require
tenant admin or tenant operator authorization. `status`, `list`, `trials`,
`trial_status`, `scores`, and `compare` require tenant authorization for the
target tenant and resource. The direct edge `POST /v1/analytics/query` route can
read experiment analytics through curated datasets and requires tenant operator
or tenant admin authorization for the requested tenant when supplied, otherwise
for the authenticated identity's tenant. These checks sit above the tenant RLS
scope on `moa.experiment_run`, `moa.experiment_trial`,
`moa.experiment_run_artifact_revision`, and `analytics.score_run`.

## Graph Changelog

`moa.graph_changelog` is the immutable outbox for graph-memory mutations. Every node or edge create, update, supersession, invalidation, erase, or crypto-shred writes a changelog row in the same transaction as the graph change. An insert trigger bumps the tenant changelog version, which cache invalidation uses as the per-tenant freshness marker.

The changelog is monthly range-partitioned, protected by FORCE RLS, and grants application roles only `SELECT` and `INSERT`. `moa_changelog_pub` publishes the table for Debezium PostgreSQL CDC; `moa_replicator` consumes it through the `moa_changelog_slot` logical replication slot.

## Replay

Replay is history-first:

1. Load session metadata.
2. Load event records ordered by `sequence_num`.
3. Reconstruct visible messages, tool state, action reviews, and checkpoints.
4. Attach to live runtime streams when available.

The orchestrator publishes live runtime events during turn execution. Visible
history is recoverable from the durable event log; hot turn/worker progress
is queryable through Restate where a durable execution primitive owns it.

Replay uses persisted session contact metadata; clients cannot provide a new
contact per message to change historical attribution. Tool-call records only
need the session id because the session store can recover the contact binding.

Sandbox bindings are not recovered from session events. The authoritative
runtime binding for a session/provider is `moa.hand_leases`, which stores the
tenant, provider, tier, serialized handle, generation, status, and expiry.
`ToolRouter` process maps are reconnect caches only; cleanup reads durable
leases so terminal session teardown works even when the current pod never
provisioned the original hand.

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
- `daily_storage_partition_metrics`
- `skill_resolution_rates`
- `segment_baselines`

Read-only analytics routes are served directly by `moa-edge` after authz and
read Postgres/domain stores without a Restate service hop. The same applies to
whoami, audit signature verification, and lineage explain/query/verify reads.
Lineage export and erase are not direct read handlers until a durable workflow
owns their side effects.

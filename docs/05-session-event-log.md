# 05 — Session & Event Log

_Postgres schema, append-only events, task segments, replay, and compaction._

## Storage

MOA uses Postgres for session storage in both local and cloud modes. Local development uses the repo Postgres dev stack; cloud deployments use managed Postgres/Neon. The `moa-session` crate owns the session-store runtime code; session/event DDL lives in the central `moa-migrations` refinery baseline.

Postgres stores:

- session metadata
- append-only event records
- archived terminal-session history (`session_event_archives`)
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
- sandbox workspace ownership, checkpoint metadata, operation/grant/capacity
  ledgers, retention, and delete fences (portable checkpoint bytes remain in
  provider/object storage)
- analytics views and materialized views

## Core Tables

The `session_baseline` logical migration owns the session schema inside the
central `crates/moa-migrations/migrations/postgres/` chain. Production and
test-template staging databases use the same canonical `moa_migrations::run`
refinery path. Session tests clone a fully migrated physical database with
`CREATE DATABASE ... TEMPLATE` and use its `public` schema; they do not replay a
curated migration subset into synthetic schemas. The important tables are:

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
    contact_promoted_from_id UUID,
    events_archived_at TIMESTAMPTZ
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
paths cannot commit a session without the agent sidecar. The schema includes the
built-in `agent://system-default` revision for internal and fixture sessions;
tenant-authored sessions should use an installed or explicitly selected agent
revision instead.

The event table is HASH-partitioned on `session_id` across 16 child tables to spread append contention, which is why the primary key is `(id, session_id)`. Every row carries a required positive `turn_number`, assigned by the append path while it holds the session-row lock: it is one plus the count of earlier `BrainResponse` rows, so the response that closes a turn has that turn's ordinal and the following event starts the next ordinal. Dedupe hits never advance it. The column has no default or trigger fallback; raw administrative/test inserts must supply the authoritative value. There is no stored `search_vector` `tsvector` column or GIN index: the rare, admin-only cross-session search computes `to_tsvector` on the fly instead of taxing every hot-path append. The append transaction folds event-derived counters into `sessions` once per batch, while generated columns own pure row-local totals.

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
ids. Those ids are derived, not random: the primary key is a UUIDv5 over the
attachment's slot (tenant, session, client message id, ordinal), which is what makes a
retried upload land on the existing row instead of creating a duplicate attachment. Replaying a session does not require local pod state; any edge or worker
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
| User input | `UserMessage`, `QueuedMessage`, `QueuedMessageRejected` |
| Brain output | `BrainThinking`, `BrainResponse`, `CacheReport` |
| Tools | `ToolCall`, `ToolResult`, `ToolError` |
| Action review | `ActionReviewRequested`, `ActionReviewDecided`, `ActionReviewContinuationRequested` |
| Memory | `MemoryRead`, `MemoryWrite`, `MemoryIngest` |
| Worker coordination | `WorkerSpawned`, `WorkerMessageSent`, `WorkerStatusChanged`, `WorkerNotificationDelivered`, `WorkerSignalReceived`, `WorkerParentResumeRequested`, `WorkerHeartbeatStale`, `ProgressNarrated` |
| Execution runs | `ExecutionRunStarted`, `ExecutionProgress`, `ExecutionInputRequired`, `ExecutionCompleted`, `ExecutionFailed`, `ExecutionCancelled`, `ExecutionSynthesisRequested` |
| Turn disposition | `TurnFailed` |
| Security | `PromptInjectionCircuitTransition` |
| Compaction | `Checkpoint` |
| Diagnostics | `Error`, `Warning` |

The serialized enum is `Event` (not `SessionEvent`); each variant uses
`#[serde(tag = "type", content = "data")]` with snake_case field names.

### Prompt-injection circuit transitions

`PromptInjectionCircuitTransition` is the one fact recorded when a classified
tool output moves a capability's security circuit to a new stage. It carries no
output: only the typed assessment class, the detector revision, the
generation-fenced owner and canonical capability identifiers, the prior and
reached stage and score, the closed-vocabulary detector signals, and the
redacted-span and deduplicated-carrier counts. Matched spans, raw carriers, and
provider text never appear here, so the event is safe in model-visible history.

`ToolResult` additionally carries the required `assessment` and `capability` of
the output it wraps. Security metadata is never optional on that variant: an
output reaching the log without an assessment would be an unclassified output,
which is indistinguishable from a safe one after the fact.

For a sandbox tool declared `MayWrite`, append of its successful `ToolResult`
is also the final side of the workspace commit barrier: the writer is quiesced,
the portable checkpoint manifest and chunks are verified, and the immutable
workspace head advances first. The event may reference that committed revision;
it does not contain or make durable arbitrary sandbox files. Read-only tools do
not advance workspace state. The complete protocol is in
[Sandbox Workspaces](25-sandbox-workspaces.md).

The append is keyed by the transition digest itself —
`prompt_injection_circuit:v1:<64 lowercase blake3 hex>` over domain-separated
canonical JSON of the schema version, session, owner, capability, tool-call id,
prior stage, and reached stage — through the same custom-key path `TurnFailed`
uses. A replayed or retried owner therefore re-derives the identical key and
collapses onto one fact instead of appending a second copy. That same key is the
`finding_info.uid` of the matching OCSF Detection Finding, so the session log
and the signed audit trail join on it directly.

`ProcessingEffect` is `Neutral` for every owner. Warning and disable stages are
informational, and for worker and execution-task owners even a suspend or halt
is neutral in the shared session log: their own signals and task outcomes own
the suspension or termination, so a child's circuit tripping must not read as
terminal root work.

### Terminal turn failure

`TurnFailed` is the one canonical failed-turn fact, for both root coordinator
turns and worker turns. Its payload is closed and secret-free: an actor
(`Coordinator` or `Worker { worker_id }`), the `turn_id`, a coarse
`TurnFailureClass` derived from the turn's own durable `TurnPhase`, and a fixed
bounded `summary` that is a function of the class alone. A raw error rendering is
never persisted here or in `TurnOutcome.message`; the error is logged for
operators instead.

Both turn workflows append it at their catch-all failure boundary, before the
owner callback and before any failed attention signal, so a failure stays
durably visible when the callback, signal, or notification that follows is lost.
The append is keyed `turn_failed:{actor_key}:{turn_id}` through the
`TurnEventAppender` custom-key path rather than the per-workflow append
sequence, so a workflow replay re-derives one identical append and the fact
materializes exactly once per actor and turn.

It does not replace an `Error` a production path already recorded, and it does
not replace `WorkerSignalReceived` (control-plane attention) or
`WorkerStatusChanged` / `WorkerNotificationDelivered` (worker-lifecycle
delivery). Those coexist with it and are counted separately.

`ProcessingEffect` classification is actor-dependent: a coordinator `TurnFailed`
is `Terminal` because it concludes the session's turn loop, and a worker
`TurnFailed` is `Neutral` so a child's failure cannot mask genuinely pending
root work in the shared session log.

`QueuedMessageRejected` records one already-accepted queued message that will
never run, carrying its original `queued_at`, its FIFO `queue_index`, and a
typed `QueuedMessageRejection`. Whole-task-tree cancellation appends exactly one
per discarded message, in queue order, so acknowledged work is never silently
dropped.

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
guard. The `session_event_dedupe` logical migration keeps it off the hot,
trigger-heavy, append-only
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

Sandbox bindings and filesystem contents are not recovered from session events.
`moa.hand_leases` is authoritative only for an ephemeral compute attachment and
its compute generation. The tenant-scoped `moa.sandbox_workspaces` aggregate,
its checkpoint/operation rows, and the portable checkpoint object store are
authoritative for retained filesystem state. `ToolRouter` process maps are
reconnect caches only. Compute cleanup reads durable leases; workspace cleanup
uses its separate generation-fenced lifecycle and cannot be implied by terminal
session teardown. See [Sandbox Workspaces](25-sandbox-workspaces.md).

## Compaction

Compaction is segment-aware because segment start/completion events remain durable boundaries. The history compiler uses checkpoints and recent events to stay under model context limits while preserving:

- recent turns
- errors and warnings
- active tool context
- segment boundaries
- pending action reviews
- checkpoint summaries

The compactor stage can create checkpoint events, but it does not remove event history from Postgres. Removing history from Postgres is retention's job, described next, and it is a different mechanism with a different guarantee: compaction shortens what the model sees, retention moves what the database stores.

## Archival And Retention

`events` is append-only, and append-only with no lifecycle boundary means a
session that ended a year ago still occupies the hottest and most heavily
indexed table in the system, and every backup taken since. Retention gives
terminal-session history a normal end state without making it unrecoverable.

The unit is one session, never a time range. `events` is HASH-partitioned on
`session_id`, so deleting one session's history prunes to exactly one of the
sixteen partitions; a retention pass expressed as a timestamp range would fan
out across all sixteen and cost more than leaving the data alone. The retention
boundary selects *which sessions* are eligible; the delete is always keyed by
session.

`session_event_archives`, owned by the logical migration of the same name,
holds one row per archived
session: the full history serialized in sequence order exactly as the rows were
stored, its BLAKE3 digest, the event count and sequence span, and the archival
timestamp. The archive row and the deletion of the rows it replaces are written
in **one transaction**, so there is no state in which an archive exists that
does not match the history it stands for. Before any delete, that transaction
reads the archive back out of the database, re-derives the digest from the bytes
Postgres is actually holding, decodes the body, and compares it event for event
against the rows about to be removed; a mismatch aborts the transaction and the
live history survives. The delete then asserts it removed exactly the number of
rows the archive captured.

`moa-session` owns the whole decision. `crate::archive` owns the serialized
format and the pure rules; `crate::store::session_archive` owns the SQL. A
session is archived only when, under its own row lock:

- its status is terminal (`completed`, `cancelled`, `failed`) — a session that
  can still append would be archived as a prefix;
- it reached that state at or before a caller-supplied boundary, never a
  boundary the storage layer derives from its own clock;
- no active `moa.legal_hold` row covers the tenant or the session's subject;
- no `moa.destruction_operation_fence` row shows a durable erasure or tenant
  purge already owns those rows;
- and the archive just written verifies.

The hold and fence checks run after the transaction takes
`pg_advisory_xact_lock` on the same `moa:destruction:tenant:<id>` key that
`place_hold` and `start_destruction` take, so a hold landing concurrently
either wins outright or waits for the pass to finish. There is exactly one
enforcement point: the durable workflow above it schedules and reports, it does
not re-decide.

Retention is the one path that opts out of the append-only guard. It sets
`moa.events_maintenance = 'on'` transaction-locally, which is the escape hatch
`events_append_only_guard()` has always carried; the guard stays in force for
every other writer, and active history is never touched. The archive write, the
delete, and the marker are one transaction, so a failure at any point between
them rolls back together — a session whose events were deleted without its
archive becoming durable would be history that no longer exists. The archive's
composite foreign key `(session_id, tenant_id) -> sessions(id, tenant_id)` makes
session ownership structural rather than merely intended: an archive cannot
name a session from another tenant. The key does not cascade.

`sessions.events_archived_at` marks a session whose history now lives in the
archive. It is set in the same transaction, and it lives on `sessions` rather
than being derived from the archive table because the append path already holds
that row under `FOR UPDATE`: an append to an archived session is refused there
with no extra round trip. Without that refusal a later append would resurrect
rows for an archived session and permanently hide the archive from the read
path. Status updates also refuse any archived-to-nonterminal transition, so a
queued transition cannot reopen an archived session after winning the row lock.
Terminal-to-terminal corrections remain allowed.

Replay of an archived session is indistinguishable from replay of a live one.
Which store holds a session's history is a *fact* — `events_archived_at` — and
`get_events` reads that marker and the ranged live rows in one SQL
statement/snapshot, then hydrates the archive when marked. It does not infer
storage from an empty live result: emptiness is also what a range past the last
sequence returns for a live session. A session marked archived with no archive
row is an error rather than an empty history. Hydration reproduces the same
`EventRecord` values with the same ids, sequence numbers, and timestamps. The
requested range is applied to serialized archive rows before claim-check blob
collection and decoding, while type and limit semantics still match the live
query. Claim-check references resolve normally because retention never touches
`session_blobs`. A hydration whose digest does not match is an error, never a
shorter history: a truncated archive must not be servable as an authentic
conversation.

`SessionRetention` is the durable workflow, dispatched per tenant and logical
date by `SessionStore/start_session_retention`, which requires **tenant admin**
on the target tenant — retention is the same class of irreversible act as a
purge. Callers do not supply the logical date; the start handler derives the
current UTC date for workflow identity and dispatch. The pass captures its
timestamp as its first durable step and derives one boundary from it, so replay
does not drift forward with the wall clock. Each session is archived behind its
own journaled step, so a crashed pass resumes rather than restarting, and a
replayed step sees `AlreadyArchived`. Storage failures remain retryable through
Restate; validation, not-found, and serialization failures are terminal, while
a hold or other archive refusal is reported as an ordinary per-session outcome.
A retention window below one day is refused outright: zero would turn a
misconfigured schedule into an immediate mass delete of history users are still
looking at.

There is deliberately **no default cron job** for retention. The feature is
inert until an operator schedules or invokes it with an explicit window and
per-pass session cap, because a data-deleting maintenance pass should not become
active merely by deploying the code that implements it.

The archive is immutable: an UPDATE trigger refuses every rewrite, so the copy
that replaced live history can never be silently altered. Its foreign key to
`sessions` deliberately does **not** cascade, so the tenant-purge step that
removes archived history stays falsifiable — without that step, a tenant's
session delete fails on a foreign-key violation instead of quietly leaving a
purged tenant's conversations in the archive.

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

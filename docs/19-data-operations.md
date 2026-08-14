# 19 - Data Operations

_Execution operations, Postgres extensions, analytics recovery, relational
graph replication, pgaudit retention, and the PII sidecar._

## Local Postgres Extensions

MOA's graph memory uses ordinary Postgres tables: nodes in `moa.node_index`,
edges in `moa.edge_index`, sidecar rows, and `moa.graph_changelog`. Standard
managed Postgres is sufficient for graph storage.

The local Postgres service is built on the Postgres 17 line and pins:

- pgvector `v0.8.2` when the pgvector vector backend is enabled;
- Debian `postgresql-17-pgaudit`.

Start the local database:

```bash
docker compose up -d postgres
```

When pgaudit is enabled, Postgres must preload it:

```text
shared_preload_libraries=pgaudit
```

Graph queries do not require a graph-specific extension or transaction-local
search-path setup. MOA's `ScopedConn` installs tenant row-level-security GUCs
before tenant queries run. The tenant is the hard runtime isolation boundary;
deployment maintenance reads must use an explicit control-plane scope instead
of the default tenant connection.

## Execution Run Operations

### Fresh Execution Analytics Inspection

Validate the execution analytics cutover from a fresh schema, not a developer
database whose migration history may contain an older checksum. For an
explicitly disposable local compose stack only, stop the stack and remove its
volumes, start Postgres again, and run the canonical refinery migration path.
Never use a volume reset on shared, staging, or production data.

```bash
docker compose down -v
docker compose up -d postgres
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo test -p moa-migrations --test run_idempotency_db --locked \
  execution_analytics_fresh_cutover_and_exact_contract_db -- \
  --ignored --exact --nocapture
```

After migration, verify that `moa.execution_route_audit` has `decision`,
nullable `strategy`, and bounded typed source/classifier provenance columns
without free-form rationale. Verify that `moa.execution_run` and
`analytics.execution_run_fact` retain typed source and plan-hash provenance
without rationale or a constant run-mode dimension. Run the focused
clean-apply/idempotency database test before trusting a second apply. Migration
history is validated by semantic identity and checksum through the canonical
runner; operators should not use a hard-coded version probe as a schema check.

### Capacity And Backpressure

`max_tasks` bounds logical work; cost, tokens, tool calls, retrieved bytes, and
the absolute deadline bound what that work may consume. Physical execution is
admitted separately so a valid large plan cannot monopolize compute or durable
queues.

Physical backpressure is supplied independently:

- Session `TurnAdmission` durably waits on shared Valkey fleet and tenant
  leases before dispatching coordinator work. Tune
  `MOA_SESSION_LIMITS_TURN_ADMISSION_FLEET_LIMIT`,
  `MOA_SESSION_LIMITS_TURN_ADMISSION_TENANT_LIMIT`,
  `MOA_SESSION_LIMITS_TURN_ADMISSION_LEASE_TTL_MS`, and
  `MOA_SESSION_LIMITS_TURN_ADMISSION_RETRY_AFTER_MS` together; the lease TTL
  must remain long enough for three generation-fenced heartbeat opportunities.
- Provider calls acquire an in-flight permit before request/input pacing and
  hold it for the request. Tune
  `MOA_PROVIDERS_CONCURRENCY_DEFAULT_MAX_IN_FLIGHT`,
  `MOA_<PROVIDER>_MAX_CONCURRENT_REQUESTS`,
  `MOA_PROVIDERS_CONCURRENCY_SCOPE`,
  `MOA_PROVIDERS_CONCURRENCY_LEASE_TTL_MS`, and
  `MOA_PROVIDERS_CONCURRENCY_BLOCK_THRESHOLD_MS` to the credential tier.
- Capability and agent tasks remain governed through
  `ActionPolicy`/`ToolExecutor`/`HandProvider`. Tool, MCP, sandbox, and external
  service quotas queue, retry, or return typed failures at that boundary; they
  do not reduce logical map coverage.
- Postgres capacity buckets enforce tenant and fleet ceilings for active runs,
  active attempts, parked runs, scheduled triggers, and external jobs. The
  controller dispatch batch and activation-step limit bound each activation;
  weighted tenant dispatch prevents a hot tenant from consuming all ready
  slots. Saturation parks durable work without holding a Restate invocation or
  sandbox.

### Budgets And Terminal Semantics

Every task atomically reserves its worst-case values before dispatch, and a
failed reservation starts no work. The five integer dimensions are:

| Dimension | Reserved field | Actual field |
|---|---|---|
| Cost | `reserved_cost_microusd` | `actual_cost_microusd` / run `consumed_cost_microusd` |
| Tokens | `reserved_tokens` | `actual_tokens` / run `consumed_tokens` |
| Logical tasks | `reserved_tasks` | `actual_tasks` / run `consumed_tasks` |
| Tool calls | `reserved_tool_calls` | `actual_tool_calls` / run `consumed_tool_calls` |
| Retrieved bytes | `reserved_retrieved_bytes` | `actual_retrieved_bytes` / run `consumed_retrieved_bytes` |

Completion reconciles actual usage and releases the reservation in the same
transaction. Every terminal run must have all five reserved values at zero.

Deadline and budget stops are typed; no operator or analytics path infers them
from gaps, error text, cancellation prose, or status alone. When both are
exhausted, `deadline_exceeded` wins. A deadline or budget stop produces:

- `partial` with the exact typed reason when useful output exists or at least
  one goal requirement is satisfied;
- `failed` with the same typed reason when no useful result exists.

Ordinary incomplete completion with no typed limit produces
`goal_incomplete`. Cancellation produces `cancelled`; scheduler, replan, task,
unsupported, blocked, and internal failures retain their own closed terminal
reasons.

### Stuck-Run Checklist

Inspect sources in this order. Do not skip directly to logs or traces:

1. Inspect the durable run through `Execution/status` and
   `moa.execution_run`: `status`, controller generation/activation state,
   `next_wake_at`,
   immutable goal contract, active plan hash/revision, typed terminal reason,
   completion-check evidence, budget/reservation totals, waiting reasons, and
   timestamps. Do not infer the execution path from a constant mode field; use
   the persisted typed route source and planning audit.
2. Inspect active `moa.execution_task` rows: state, `task_id`, attempt dispatch,
   generation/lease fence, reservation/actual values, and
   `reserved_at`/`started_at`/`completed_at`. A stale generation must never
   overwrite the current one.
3. Inspect the exact input, review, signal, timer, external-job, or pause state,
   its persisted `due_at`, immutable trigger, and owning task generation.
   Resolve user-owned waits through
   `Execution/deliver_input`, `Execution/decide_review`, or
   `Execution/deliver_signal`; do not edit the task.
4. Inspect `moa.execution_dispatch_outbox` and `moa.execution_trigger` claim,
   delivery, retry, dead-letter, generation, and error fields. For external
   jobs also inspect callback disposition and last reconciliation.
5. Inspect only the current bounded `ExecutionRunController`,
   `ExecutionTaskAttempt`, or `ExecutionTrigger` invocation. A parked run should
   have none; if it does, treat that as a resource-leak incident.
6. Query spans by stable session/run/task/action-review attributes, then inspect
   any persisted W3C parent/link contexts on durable callbacks. Do not infer
   causality from an attempt-local header embedded in a Restate journal command.

### Cancellation And Replay Safety

Use the parent-scoped product cancellation mutation:

- REST: `POST /v1/execution-runs/cancel`;
- MCP: `execution_run_cancel`;
- internal Restate: `Execution/cancel`.

The cancellation transaction fences new reservations, advances active attempt
generations, releases their active-capacity and budget reservations, preserves
completed task evidence, writes the typed cause/reason, and enqueues controller
activation. `compensate_committed` remains nonterminal until bounded reverse
compensation settles. Confirm the result through
`Execution/status` or `execution_run_status`.

Restate admin cancellation is only a hard stop for a stuck invocation. It is
not the product-state transition and does not replace `Execution/cancel`.
Never repair, cancel, advance, release, or terminalize a run with ad hoc SQL.

Mutation results distinguish durable effects:

- `Applied` means this call committed the logical mutation and carries the
  persisted evidence used for follow-up work and metrics.
- `Replayed` means the same logical mutation was already committed, including
  commit-before-handler-result recovery; it repeats no logical effect and emits
  no mutation metric.
- conflicts, stale generations, and rejections carry no applied evidence.

Duplicate Restate sends are therefore safe only through the typed repository
contract. They are not permission to make an external non-idempotent tool call
twice.

### Execution Incident Regression Policy

Every production execution incident that affects routing, contract fidelity,
coverage, recovery, authorization, budget accounting, or terminal honesty must
add a stable execution-eval scenario or corpus case. Key the fixture by the
persisted failure fingerprint when one exists; otherwise key it by the minimum
run/task/audit evidence that reproduces the failure. Assert the typed state
predicate that would have prevented or honestly reported the incident.

The regression corpus grows monotonically. Fixing an incident does not remove
its case, and old cases are never deleted or weakened to improve aggregate
scores. A superseding case may replace one only when it exercises the same
production path and strictly contains the old failure condition; record that
relationship in the scenario comment.

### Long-Horizon Maintenance And Retention

The singleton `moa-orchestrator maintenance` deployment is the only fleet
owner for trigger/outbox repair, execution retention, action-review/authz
reconciliation, workspace/hand reaping, and provider inventory. Serving
Restate revisions do not run these scans. Due trigger delivery remains frequent;
full inventory and retention use separate adaptive cadences, account-sharded
leases, and exponential idle backoff.

Page on overdue deadlines, oldest-ready age, stuck attempt leases, trigger or
outbox dead letters, stale durable maintenance reconciliation, parked tasks retaining
hands, and old deployment drain age. Capacity saturation is normally a warning:
inspect tenant maximum share and fairness before raising fleet ceilings.

Retention archives and page-deletes terminal run details, tasks, triggers,
outbox rows, external jobs, and compensation evidence only after tenant
retention/legal-hold policy permits it. Active, waiting, paused, compensating,
or unknown-outcome state is never selected. The maintenance owner must prove a
terminal generation and preserve compact run/session/audit evidence before
deletion.

The one-time V59/V60 hard cut is executed only with
`scripts/cutover-long-horizon-execution.sh`. It always prints the Postgres
nonterminal-run inventory and exact old Restate deployment invocations before
mutation, requires explicit targets and confirmation, archives terminal
execution tables, applies the repository migration runner, clears only the
three retired execution services, and verifies the bounded service inventory.
Admission remains gated until maintenance readiness and archive durability are
independently confirmed.

## Sandbox Workspace Operations

Postgres is the ownership and lifecycle authority for durable sandbox
workspaces. Provider inventory, mutable volumes, compute instances, and portable
checkpoint objects are external evidence that maintenance must reconcile to the
durable tenant/workspace/account-generation fences; none of them grants access
on its own. Operators must not repair workspace rows, clear operation fences, or
delete provider resources with ad hoc SQL or provider-console actions.

The `MOA Sandbox Workspace Fleet` dashboard and
`ops/prometheus/alerts/sandbox-workspaces.yaml` cover lifecycle counts and
latency, durable workspace/resource states, quota decisions and fleet
utilization, reaper health/backlog, checkpoint bytes and latency, and provider
inventory drift. Metric dimensions are closed vocabularies: `provider_kind`,
`operation`, `result`, `state`, `dimension`, `decision`, and `classification`.
Tenant, user, workspace, checkpoint, provider-account, and raw provider-resource
identities are trace fields or protected database evidence, never metric labels.
Paths, object keys, wrapped keys, credentials, file names, and file content must
not enter metric labels, dashboard legends, alert annotations, or public status
responses.

When investigating an incident, correlate a bounded metric series to a request
or workspace only after moving to protected traces and tenant-scoped repository
reads. Do not paste raw provider inventory, signed ownership markers, archive
manifests, checkpoint paths, or credential errors into tickets or chat tools.

### Workspace Rollout And Rollback

The deployment starts with `MOA_SANDBOX_WORKSPACE_MODE=disabled`. Apply V58 and
OpenFGA v7 while dark, drain legacy hands, then select `maintenance` only after
configuring durable Postgres KMS, external checkpoint storage, provider-account
bootstrap mappings, bounded retention/quotas, and the canary route. Maintenance
must become ready and converge reconciliation/alerts before changing to
`admit`. Admission is limited to the configured account generation/cell and
tenant allowlist; expand by changing deployment configuration, never through a
request field or provider/model output.

Rollback is `admit` to `maintenance`. This stops new admission and writer claims
while retaining deletion, retention, reconciliation, purge, and the supervised
reaper. Confirm every operation and attachment is terminal or reconcilable and
provider inventory has converged before considering `disabled`. Do not use
disabled as an emergency stop while durable work remains; use the local access
and writer fences while maintenance drains.

Tenant offboarding is unavailable in `disabled`: the purge workflow refuses at
`Pending`, before deleting vectors or relational rows, because only the
maintenance coordinator can prove provider-side absence. Enable `maintenance`,
complete external-first purge, and drain all durable state before returning dark.

### Workspace Reaper Failure

`MOASandboxWorkspaceReaperUnready` or
`MOASandboxWorkspaceReaperHeartbeatStale` means the supervised maintenance job
on at least one serving replica is unhealthy. Readiness must remove that replica
from service and an unexpected job exit is process-fatal. Check, in order:

1. replica readiness and the reaper's bounded unready reason/task result;
2. `moa_sandbox_workspace_reaper_heartbeat_age_seconds`, backlog, and oldest-work
   age across replicas;
3. Postgres, checkpoint object-store, KMS, and provider-account reachability;
4. durable operation/retry fences through tenant-scoped repository or admin
   surfaces; and
5. provider status after the durable evidence has identified the affected
   bounded provider class.

Restart only after preserving Postgres, object-store, and provider state. A
restart reclaims expired durable work; it is not permission to retry an
ambiguous external mutation manually. During rollback, move admission to
maintenance mode first and keep the coordinator/reaper running until backlog
and inventory findings converge. Never switch directly to disabled mode while
durable work remains.

### Workspace Maintenance Backlog

`MOASandboxWorkspaceReaperBacklogAge` means retention, deletion, absence proof,
or reconciliation is not converging. Split the dashboard by bounded lifecycle
operation and provider class, then inspect the oldest durable claim. Common
causes are a provider outage, a partial checkpoint upload, an expired deletion
claim, bucket-versioning drift, or an inventory finding waiting for quarantine
review.

Do not shorten claim TTLs below the configured retry/heartbeat window and do not
clear claims by hand. Expired claims are reclaimable; stale claimants are fenced
from finalization. A provider outage must leave purge incomplete and resources
inaccessible, not discard the durable finding or relational ownership record.

### Workspace Capacity Pressure

`MOASandboxWorkspaceQuotaNearCapacity` reports fleet-level utilization by
capacity dimension. Confirm provider-observed usage, durable reservations, and
configured limits together before raising a limit. A reservation may be pending
provider visibility, so adding observed usage and reservations without applying
the provider-specific no-double-count rule overstates pressure.

Quota rejections are expected at a hard limit and execute no provider mutation.
Investigate sustained rejection rates for the affected dimension. For Daytona
volumes, preserve one tenant-dedicated resource per tenant/account generation
and security class; do not pool tenant subpaths to work around account limits.

### Portable Checkpoint Failures

`MOASandboxWorkspaceCheckpointFailures` reports failed or ambiguous create,
restore, or delete operations. The last verified committed revision remains the
authority. Check object-store/KMS reachability, bounded archive limits, manifest
digest verification, and the exact durable operation fence. Never restore from
a partial upload or a provider-native snapshot in place of the portable
checkpoint authority.

The checkpoint bucket must use the configured, verified versioning policy. MOA
currently requires a bucket whose provider reports versioning was never enabled;
enabled or suspended buckets retain historical bytes and are unsupported. An
unknown or changed policy fails startup/purge closed. Absence requires bounded enumeration
of the exact checkpoint prefix and two separated empty observations whose
inventory digest remains unchanged; a missing manifest alone proves nothing.

### Provider Inventory Drift

`MOASandboxWorkspaceInventoryDrift` is critical because provider inventory and
durable MOA ownership disagree. The maintenance coordinator compares only
provider-verified MOA ownership metadata with persisted storage/hand rows.
Unknown, duplicate, wrong-account, wrong-owner, and missing classifications are
written to the maintenance-only finding ledger and quarantined.

Never auto-delete a finding. Establish the persisted provider account and
generation, verify signed/encrypted ownership metadata, inspect every matching
durable workspace/storage/hand row, and obtain two-empty external absence proof
before resolving it. If ownership remains ambiguous, keep the resource
quarantined and access-fenced. Resolution must preserve first/last-seen time,
evidence digest, and resolution audit data.

## Graph Changelog Replication

`moa.graph_changelog` is the immutable outbox for graph-memory mutations.
Postgres publishes it through `moa_changelog_pub`; Debezium consumes it with
`ops/debezium/moa-changelog-connector.json`.

Local compose starts Postgres with:

```text
wal_level=logical
max_replication_slots=10
max_wal_senders=10
```

Managed Postgres must set the same values before running the changelog
migration. `wal_level` requires a database restart.

Migration-owned objects:

- `moa.graph_changelog`, range-partitioned by month and append-only for
  application roles;
- tenant freshness state, bumped in the same transaction as each changelog
  insert;
- `moa_changelog_pub` with `publish_via_partition_root=true`;
- `moa_replicator`, a `LOGIN REPLICATION` role;
- `moa.ensure_changelog_replication_slot()`, which reserves
  `moa_changelog_slot`.

Set the replicator password out of band:

```sql
ALTER ROLE moa_replicator WITH PASSWORD '<secret>';
```

After enabling logical WAL, reserve the slot:

```sql
SELECT moa.ensure_changelog_replication_slot();
```

Register the connector:

```bash
curl -X PUT \
  -H 'Content-Type: application/json' \
  --data @ops/debezium/moa-changelog-connector.json \
  http://localhost:8083/connectors/moa-changelog/config
```

Expected topic:

```text
moa.cdc.moa.graph_changelog
```

Smoke checks:

```sql
SHOW wal_level;
SELECT pubname FROM pg_publication WHERE pubname = 'moa_changelog_pub';
SELECT slot_name, plugin FROM pg_replication_slots WHERE slot_name = 'moa_changelog_slot';
```

`moa_app` must not be able to update or delete changelog rows. Older monthly
partitions are detached for S3/Object Lock retention before physical pruning.

## ClickHouse Copies And Tenant Deletion

When `[clickhouse]` is configured, ClickHouse holds analytics-export copies
(`events_raw`, `dim_*`, `turn_fact`, and `tool_call_fact`) that tenant
offboarding must reach. `AnalyticsClickHouseClient::purge_tenant` runs from the
edge purge path after the Postgres transaction commits and skips tables that
were never created, so a ClickHouse failure surfaces without rolling back the
relational purge.

Lineage is not copied to ClickHouse. Its storage, reads, retention, and tenant
deletion stay in Postgres. The ClickHouse analytics tables currently have no
TTL; their Postgres sources are the retention authority until the events
tiering phase.

### Execution Analytics Bootstrap And Schema Drift Recovery

Execution-dimension bootstrap and normal incremental export are resumable state
machines under the existing single exporter lease. Missing current ClickHouse
tables are created automatically. Existing execution tables must match the
exact ordered columns/types, a non-nil Atomic table UUID,
`ReplacingMergeTree(export_version)`, an empty partition key, and the exact
sorting/primary keys. The exporter never alters, rebuilds, copies, or translates
a mismatched execution table.

If startup reports `ClickHouse analytics reset required`, drop exactly
`dim_execution_runs` and `dim_execution_tasks` together inside the existing
ClickHouse analytics database, then restart the exporter. Never drop or
recreate the whole database: Postgres cursors for the other ClickHouse copies
survive and cannot prove a complete historical replay. A changed database UUID
is therefore rejected without adding a bootstrap generation. Restore the
original database from the deployment backup/recovery path; do not delete
Postgres cursor state as an ad hoc repair. Do not issue ad hoc `ALTER`, `RENAME`,
or `INSERT SELECT` repairs, and do not reset Postgres source data.

For an interrupted bootstrap after schema validation, inspect the latest row in
`analytics.clickhouse_schema_upgrade_state` for `execution_dimensions` ordered
by `generation DESC`. Confirm that its `run_table_uuid` and `task_table_uuid`
match ClickHouse `system.tables.uuid` and that its `database_uuid` matches
`system.databases.uuid`. The database UUID is immutable across every
generation. The durable stages are:

```text
pending
  -> schema_upgraded
  -> cursors_reset
  -> runs_exported
  -> tasks_exported
  -> complete
```

Restart the exporter after correcting ClickHouse availability or credentials.
It resumes the recorded stage, revalidates the exact current schema, and
continues from the persisted per-dataset page cursor to the stored run/task
high-water tuples. After a paired execution-table reset, new execution-table
UUIDs append one generation only when both UUIDs change under the same database
UUID, reset both execution cursors, and restore all source rows through new
fixed high waters. Replayed startup with unchanged table UUIDs reuses the latest
generation. A one-table reset is rejected; drop both execution tables together
and retry. Do not delete prior generation rows; their version floors keep reset
recovery monotonic under clock skew.

Normal execution export also persists an immutable active pass bound
`(pass_high_water_seq, pass_high_water_id)` and `pass_started_at`. After a
restart it resumes that same bound; it never recaptures or advances an
unbounded cursor. Each page advances only after the idempotent
`ReplacingMergeTree` insert returns. The pass becomes caught up only when the
regular `(cursor_seq, cursor_id)` reaches the bound and the active-pass fields
are cleared in the same transaction.

Sequence-backed freshness is the durable caught-up time, not a source-row
`updated_at` watermark. Zero/reset state uses Unix epoch, and an active or
partially exported pass leaves `cursor_ts` and `exported_at` unchanged, so it
cannot appear fresh. `read_model_updated_at` is:

```sql
MIN(CASE WHEN cursor_seq IS NULL THEN cursor_ts ELSE exported_at END)
```

across export-state rows. Existing timestamp-backed datasets continue to use
`cursor_ts`; execution datasets use their last completed pass time.

## Audit Log Retention

MOA keeps two local audit trails for graph memory:

- `moa.graph_changelog`: queryable in-database changelog for memory mutations
  and redacted erase markers;
- PostgreSQL pgaudit logs: operational database audit records retained in the
  `moa-pg-audit` volume for local development.

Application security events are separately signed per tenant and stored in the
Postgres `security_events` table. MOA does not currently run a separate audit
export worker; remote archival is an operator concern outside the runtime.

Start Postgres locally:

```bash
docker compose up -d postgres
```

Verify pgaudit emitted a relation-level audit line:

```bash
docker compose exec postgres sh -lc \
  'grep -R "AUDIT:.*moa.node_index" /var/log/postgresql || true'
```

For breach response, preserve the database and `moa-pg-audit` volume, export
matching audit rows, compare pgaudit timestamps with
`moa.graph_changelog.created_at`, and do not copy raw logs into chat tools or
tickets.

## PII Service

`moa-pii-service` is the out-of-process inference sidecar used by
`moa-memory-pii`. It wraps the HuggingFace `openai/privacy-filter`
token-classification model behind a small FastAPI HTTP API so Rust crates do
not link Python, transformers, or torch.

Run locally:

```bash
docker compose --profile pii up -d moa-pii-service
export MOA_PII_SERVICE_URL=http://127.0.0.1:10050
curl -s http://localhost:10050/healthz
```

For a Compose-hosted orchestrator, set
`MOA_PII_SERVICE_URL=http://moa-pii-service:8080` when starting or recreating
the orchestrator.

Classify text:

```bash
curl -s http://localhost:10050/classify \
  -H 'content-type: application/json' \
  -d '{"text":"My SSN is 123-45-6789","return_spans":true}'
```

Response shape:

```json
{
  "spans": [
    { "start": 10, "end": 21, "category": "SSN", "confidence": 0.97 }
  ],
  "abstained": false,
  "model_version": "openai/privacy-filter:v1.0"
}
```

Configuration:

- `MODEL`: HuggingFace model id. Default: `openai/privacy-filter`.
- `DEVICE`: `cpu` or `cuda`. Default: `cpu`.

Rust callers use `moa_memory_pii::OpenAiPrivacyFilterClassifier`. The client
fails closed by default: network, HTTP, or parse failures return
`SensitivityClass::Pii` with `abstained = true`. Callers that need hard errors can
disable fail-closed behavior explicitly.

Operational notes:

- keep inference out of MOA Rust binaries;
- tune thresholds in Rust through `PrivacyFilterThresholds`;
- warm the sidecar before high-volume ingestion because the first request loads
  model weights.

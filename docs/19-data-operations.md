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

### Fresh V000337 Inspection

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

After migration, inspect `_refinery_schema_history` for version 337 and verify
that `moa.execution_route_audit` has `decision`, nullable `strategy`, and bounded
typed source/classifier provenance columns without free-form rationale. Verify
that `moa.execution_run` and `analytics.execution_run_fact` retain typed source
and plan-hash provenance without rationale or a constant run-mode dimension.
Run the focused clean-apply/idempotency database test before trusting a second
apply.

### Capacity And Backpressure

Execution plans have no application active-worker, plan-node, map-item, or task
fan-out ceiling below the approved run budget. `max_tasks` bounds logical work;
the other four resource dimensions and the deadline bound what that work may
consume. Do not add an application fan-out constant to mitigate provider or
tool pressure.

Physical backpressure is supplied independently:

- Restate scoped concurrency queues invocations. Expensive tenant-scoped work
  uses the `tenant-{tenant_id}` scope; the default wildcard rule is
  `concurrency 1000` per scope. Inspect `sys_rules` and `sys_user_limits` before
  changing it.
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
   `moa.execution_run`: `status`, `wake_epoch`, `processed_wake_epoch`,
   immutable goal contract, active plan hash/revision, typed terminal reason,
   completion-check evidence, budget/reservation totals, waiting reasons, and
   timestamps. A greater `wake_epoch` means the latest scheduling mutation is
   not yet acknowledged. Do not infer the execution path from a constant mode
   field; use the persisted typed route source and planning audit.
2. Inspect active `moa.execution_task` rows: state, `task_id`, attempt,
   generation fence, reservation/actual values, and
   `reserved_at`/`started_at`/`completed_at`. A stale generation must never
   overwrite the current one.
3. Inspect the exact waiting input, review, or signal state in the run's
   waiting reasons and the owning task generation. Resolve it through
   `Execution/deliver_input`, `Execution/decide_review`, or
   `Execution/deliver_signal`; do not edit the task.
4. For action reviews, inspect `moa.execution_action_review_outbox`:
   `attempt_count`, `next_attempt_at`, `claimed_at`, `delivered_at`, and
   `last_error`, plus the matching tenant action-review row.
5. Inspect Restate invocation and journal state for the keyed `ExecutionRun`
   and `ExecutionTask` workflows, including retries and scoped-concurrency
   admission.
6. Query spans by stable session/run/task/action-review attributes, then inspect
   any persisted W3C parent/link contexts on durable callbacks. Do not infer
   causality from an attempt-local header embedded in a Restate journal command.

### Cancellation And Replay Safety

Use the parent-scoped product cancellation mutation:

- REST: `POST /v1/execution-runs/cancel`;
- MCP: `execution_run_cancel`;
- internal Restate: `Execution/cancel`.

The terminal cancellation transaction fences new reservations, replaces every
active task outcome with cancellation, releases all five reservation
dimensions, preserves completed task evidence, writes the typed cancellation
cause/reason, and wakes terminal delivery. Confirm the result through
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

When `[clickhouse]` is configured, ClickHouse holds copies that tenant
offboarding must reach (both run from the edge purge path after the Postgres
transaction commits, so a ClickHouse failure surfaces without rolling back the
relational purge):

- `turn_lineage` rows: `ClickHouseStore::delete_partition_rows`;
- analytics-export tables (`events_raw`, `dim_*`, `turn_fact`,
  `tool_call_fact`): `AnalyticsClickHouseClient::purge_tenant`, which skips
  tables that were never created.

Retention inside ClickHouse: `turn_lineage` drops via table TTL
(`clickhouse.lineage_ttl_days`, default 30); the analytics tables currently
have no TTL — their Postgres sources are the retention authority until the
events tiering phase.

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

MOA keeps two audit trails for graph memory:

- `moa.graph_changelog`: queryable in-database changelog for memory mutations
  and redacted erase markers;
- PostgreSQL pgaudit logs: immutable operational audit stream shipped to S3
  Object Lock.

The audit bucket is created with Object Lock enabled and default retention set
to 2190 days. Uploaded objects use `ObjectLockMode=COMPLIANCE` and a
`RetainUntilDate` 2190 days after upload.

Start Postgres and the shipper locally:

```bash
docker compose up -d postgres moa-audit-shipper
```

The shipper scans stable PostgreSQL `*.log` and `*.csv` files, gzip-compresses
them, and uploads to:

```text
s3://moa-audit-{env}/tenant=unknown/year=YYYY/month=MM/<log-file>.gz
```

It records uploaded file versions in its state volume and skips the newest log
file so the active collector segment is not uploaded before rotation completes.

Create a bucket once per environment:

```bash
ENV=dev REGION=us-east-1 ops/audit/bootstrap.sh
```

Verify pgaudit emitted a relation-level audit line:

```bash
docker compose exec postgres sh -lc \
  'grep -R "AUDIT:.*moa.node_index" /var/log/postgresql || true'
```

Verify S3 retention:

```bash
aws s3api get-object-retention \
  --bucket moa-audit-dev \
  --key tenant=unknown/year=YYYY/month=MM/<log-file>.gz
```

For breach response, preserve the audit bucket, enable legal hold on relevant
object versions, export matching audit rows, compare pgaudit timestamps with
`moa.graph_changelog.created_at`, and do not copy raw logs into chat tools or
tickets.

## PII Service

`moa-pii-service` is the out-of-process inference sidecar used by
`moa-memory-pii`. It wraps the HuggingFace `openai/privacy-filter`
token-classification model behind a small FastAPI HTTP API so Rust crates do
not link Python, transformers, or torch.

Run locally:

```bash
docker compose up -d moa-pii-service
curl -s http://localhost:10050/healthz
```

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

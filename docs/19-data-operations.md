# 19 - Data Operations

_Postgres extensions, graph changelog replication, pgaudit retention, and the
PII sidecar._

## Apache AGE And Local Postgres

MOA's local Postgres image is built on the Postgres 17 line and pins:

- Apache AGE `release/PG17/1.7.0`;
- pgvector `v0.8.2`;
- Debian `postgresql-17-pgaudit`.

Build and start the local database:

```bash
docker compose build postgres
docker compose up -d postgres
```

The compose service starts Postgres with:

```text
shared_preload_libraries=age,pgaudit
session_preload_libraries=age
```

AGE still requires transaction-local search path setup for Cypher. MOA's
`ScopedConn` installs `search_path = ag_catalog, "$user", public` alongside
tenant row-level-security GUCs before tenant queries run. The tenant is the
hard runtime isolation boundary; deployment maintenance reads must use an
explicit control-plane scope instead of the default tenant connection.

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
`PiiClass::Pii` with `abstained = true`. Callers that need hard errors can
disable fail-closed behavior explicitly.

Operational notes:

- keep inference out of MOA Rust binaries;
- tune thresholds in Rust through `PrivacyFilterThresholds`;
- warm the sidecar before high-volume ingestion because the first request loads
  model weights.

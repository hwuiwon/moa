# Logical Replication For Graph Changelog

`moa.graph_changelog` is the immutable outbox for graph-memory mutations. Postgres publishes it through `moa_changelog_pub`; Debezium consumes it with `ops/debezium/moa-changelog-connector.json`.

## Postgres Settings

Local compose starts Postgres with:

```text
wal_level=logical
max_replication_slots=10
max_wal_senders=10
```

Managed Postgres must set the same values before running the M06 migration. `wal_level` requires a database restart.

## Migration-Owned Objects

The M06 migration creates:

- `moa.graph_changelog`, range-partitioned by month and append-only for application roles.
- `moa.workspace_state`, bumped by `graph_changelog_bump_workspace_state` in the same transaction as each changelog insert.
- `moa_changelog_pub`, with `publish_via_partition_root=true`.
- `moa_replicator`, a `LOGIN REPLICATION` role without a password.
- `moa.ensure_changelog_replication_slot()`, a helper for reserving `moa_changelog_slot` after logical WAL is active.

Set the replicator password out-of-band:

```sql
ALTER ROLE moa_replicator WITH PASSWORD '<secret>';
```

## Slot Bootstrap

After the restart that enables logical WAL, reserve the slot:

```sql
SELECT moa.ensure_changelog_replication_slot();
```

If the slot already exists, the helper returns `moa_changelog_slot` without changing it. Debezium can also create the slot on first start when `moa_replicator` has replication privileges, but pre-creating it makes startup failures easier to diagnose.

## Connector Deployment

Register the connector against Kafka Connect:

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

## Smoke Checks

```sql
SHOW wal_level;
SELECT pubname FROM pg_publication WHERE pubname = 'moa_changelog_pub';
SELECT slot_name, plugin FROM pg_replication_slots WHERE slot_name = 'moa_changelog_slot';
```

Insert test changelog rows through `moa-memory-graph::write_and_bump`, then confirm:

```sql
SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = '<workspace>';
```

`moa_app` should not be able to update or delete changelog rows:

```sql
SET ROLE moa_app;
UPDATE moa.graph_changelog SET pii_class = 'none' WHERE false;
```

The update must fail with a permission error.

## Retention

The migration keeps 12 historical monthly partitions, the current month, and the next month online. HIPAA audit retention is six years; M22 ships detached older partitions to S3 Object Lock before physical pruning.

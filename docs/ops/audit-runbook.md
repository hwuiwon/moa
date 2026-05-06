# Audit log retention runbook

MOA keeps two audit trails for graph memory:

- `moa.graph_changelog` is the queryable in-database changelog for memory
  mutations and redacted erase markers.
- PostgreSQL pgaudit logs are the immutable operational audit stream. M22 ships
  completed PostgreSQL log files to S3 Object Lock in COMPLIANCE mode.

## Retention

The audit bucket is created with Object Lock enabled and default retention set
to 2190 days. This is six years, matching the HIPAA documentation retention
window used by the memory-pack audit design.

Objects are uploaded with per-object `ObjectLockMode=COMPLIANCE` and a
`RetainUntilDate` 2190 days after upload. Compliance-mode retention cannot be
shortened by normal users after it is applied.

## Local development

Start Postgres and the shipper:

```sh
docker compose up -d postgres moa-audit-shipper
```

The compose stack writes PostgreSQL logs to the `moa-pg-audit` volume and mounts
that volume read-only into `moa-audit-shipper`. The shipper scans stable
`*.log` and `*.csv` files, gzip-compresses them, and uploads them to:

```text
s3://moa-audit-{env}/workspace=unknown/year=YYYY/month=MM/<log-file>.gz
```

Workspace partitioning can be added later by parsing pgaudit log content. v1
ships at log-file granularity.

The shipper records uploaded file versions in its state volume and leaves the
PostgreSQL log volume untouched. It skips the newest log file so the active
collector segment is not uploaded before rotation completes.

## Bucket bootstrap

Create the bucket once per environment:

```sh
ENV=dev REGION=us-east-1 ops/audit/bootstrap.sh
```

The script enables bucket versioning, Object Lock COMPLIANCE default retention,
and S3 public-access blocks.

## Verification

After writing graph memory data, verify pgaudit emitted a relation-level audit
line without bind parameters:

```sh
docker compose exec postgres sh -lc \
  'grep -R "AUDIT:.*moa.node_index" /var/log/postgresql || true'
```

Verify S3 retention:

```sh
aws s3api get-object-retention \
  --bucket moa-audit-dev \
  --key workspace=unknown/year=YYYY/month=MM/<log-file>.gz
```

The expected mode is `COMPLIANCE`, with a retain-until date roughly 2190 days
after upload.

## Legal hold

For breach response, apply legal hold to specific object versions:

```sh
aws s3api put-object-legal-hold \
  --bucket moa-audit-prod \
  --key <key> \
  --legal-hold Status=ON
```

Record the object key, version ID, incident ID, and reviewer in the incident
record. Remove legal hold only after counsel approves.

## Breach response

1. Preserve the audit bucket and enable legal hold on relevant log objects.
2. Export matching `moa.audit_logs` rows for fast investigation.
3. Compare pgaudit log timestamps with `moa.graph_changelog.created_at`.
4. Confirm pgaudit parameters are disabled before sharing logs externally.
5. Keep redacted reports in the incident folder; do not copy raw logs into chat
   tools or tickets.

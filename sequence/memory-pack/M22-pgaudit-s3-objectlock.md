# Step M22 — pgaudit configuration + S3 Object Lock shipping pipeline

_Configure pgaudit to log WRITE/DDL/ROLE always plus object-level READ on PHI tables, and stand up the audit-shipping pipeline that writes Postgres audit logs into S3 with Object Lock COMPLIANCE mode for 6-year HIPAA retention._

## 1 What this step is about

pgaudit emits structured audit records to PostgreSQL's standard logging stream. We configure it to log every WRITE/DDL/ROLE event always (cheap) and READ events at object level only on tables containing PHI (`moa.embeddings`, `moa.node_index` rows where `pii_class IN ('phi','restricted')`). A sidecar shipper rotates and uploads completed audit log segments to S3 with Object Lock COMPLIANCE mode for 6 years (HIPAA 45 CFR 164.316(b)(2)(i)).

## 2 Files to read

- M00 stack-pin (pgaudit 17.x)
- M02/M21 (RLS + envelope) — pgaudit config must coexist
- AWS S3 Object Lock COMPLIANCE docs

## 3 Goal

1. pgaudit installed in base image; `shared_preload_libraries=age,pgaudit`.
2. Postgres `postgresql.conf` settings for pgaudit: `pgaudit.log='write,ddl,role'`, `pgaudit.log_relation=on`, plus per-PHI-table `ALTER TABLE ... SECURITY LABEL` for object audit.
3. Sidecar shipper container (`moa-audit-shipper`) tails `/var/log/postgresql/*.log`, gzip-rotates hourly, uploads to `s3://moa-audit-{env}/<workspace>/<yyyy-mm>/<seg>.gz` with Object Lock retention.
4. S3 bucket policy enforces Object Lock COMPLIANCE 6yr.
5. CI test asserts log line is written for a sample WRITE.

## 4 Rules

- **Never log PHI cleartext**. pgaudit logs query text — bind parameters logged separately. We DISABLE `pgaudit.log_parameter` and use parameter-id-only via `pgaudit.log_parameter_max_size=0`.
- **Object-level READ audit** only on PHI-bearing tables (otherwise read volume floods the log). Use `SECURITY LABEL FOR pgaudit ON TABLE moa.node_index IS 'READ, WRITE'` for tables we want fine-grained audit on.
- **S3 Object Lock COMPLIANCE mode** (not GOVERNANCE) — even the bucket owner cannot override retention.
- **Retention 2190 days (6 years)**; legal hold available for breach response.
- **Multipart upload** for segments larger than 100MB.

## 5 Tasks

### 5a docker-compose

```yaml
postgres:
  command:
    - postgres
    - -c
    - shared_preload_libraries=age,pgaudit
    - -c
    - logging_collector=on
    - -c
    - log_destination=stderr,csvlog
    - -c
    - log_directory=/var/log/postgresql
    - -c
    - log_rotation_age=60min
    - -c
    - log_rotation_size=0
    - -c
    - pgaudit.log=write,ddl,role
    - -c
    - pgaudit.log_relation=on
    - -c
    - pgaudit.log_parameter=off
    - -c
    - pgaudit.log_catalog=off
    - -c
    - pgaudit.log_statement_once=on
  volumes:
    - moa_pg_audit:/var/log/postgresql
moa-audit-shipper:
  image: moa/audit-shipper:0.1
  volumes:
    - moa_pg_audit:/var/log/postgresql:ro
  environment:
    AWS_REGION: us-east-1
    BUCKET: moa-audit-dev
    OBJECT_LOCK_DAYS: "2190"
volumes:
  moa_pg_audit: {}
```

### 5b Migration: object-level audit

`migrations/M22_pgaudit.sql`:

```sql
CREATE EXTENSION IF NOT EXISTS pgaudit;

-- Mark PHI tables for object audit
SECURITY LABEL FOR pgaudit ON TABLE moa.node_index    IS 'READ, WRITE';
SECURITY LABEL FOR pgaudit ON TABLE moa.embeddings    IS 'READ, WRITE';
SECURITY LABEL FOR pgaudit ON TABLE moa.graph_changelog IS 'READ, WRITE';

-- Auditor role can read changelog directly (already from M06).
GRANT USAGE ON SCHEMA moa TO moa_auditor;
GRANT SELECT ON ALL TABLES IN SCHEMA moa TO moa_auditor;
```

### 5c S3 bucket bootstrap (Terraform / CDK / AWS CLI script — pick one)

`ops/audit/bootstrap.sh`:

```sh
aws s3api create-bucket --bucket moa-audit-${ENV} --region ${REGION} --object-lock-enabled-for-bucket
aws s3api put-object-lock-configuration --bucket moa-audit-${ENV} --object-lock-configuration '{
  "ObjectLockEnabled":"Enabled",
  "Rule":{"DefaultRetention":{"Mode":"COMPLIANCE","Days":2190}}}'
aws s3api put-public-access-block --bucket moa-audit-${ENV} --public-access-block-configuration '{
  "BlockPublicAcls":true,"IgnorePublicAcls":true,"BlockPublicPolicy":true,"RestrictPublicBuckets":true}'
```

### 5d Audit shipper

`services/audit-shipper/main.py` (small Python; not Rust because the shipper is purely operational glue):

```python
# pseudocode
while True:
    completed_logs = scan_completed_files(LOG_DIR)
    for f in completed_logs:
        gz = compress(f)
        key = f"workspace=unknown/year={dt.year}/month={dt.month:02}/{f.name}.gz"
        s3.upload_file(gz, BUCKET, key, ExtraArgs={
            "ObjectLockMode": "COMPLIANCE",
            "ObjectLockRetainUntilDate": (now + timedelta(days=int(os.environ['OBJECT_LOCK_DAYS']))).isoformat(),
            "ServerSideEncryption": "aws:kms",
        })
        os.unlink(f)
    sleep(60)
```

(Workspace partitioning happens at log-line level via a parser if needed — for v1 we ship at file granularity.)

### 5e Auditor view

Create a thin Postgres view for the auditor role:

```sql
CREATE VIEW moa.audit_logs AS
  SELECT * FROM moa.graph_changelog ORDER BY created_at DESC;
GRANT SELECT ON moa.audit_logs TO moa_auditor;
```

(Postgres logs go to S3 via the shipper. The internal `graph_changelog` is the secondary, queryable audit trail.)

### 5f CI smoke

`tests/audit_smoke.rs`:

```rust
#[tokio::test]
async fn audit_writes_log_line() {
    // 1. Insert a fact
    // 2. Wait 5s; tail postgresql.log
    // 3. Assert "AUDIT: SESSION,..." line containing INSERT and table moa.node_index
}
```

## 6 Deliverables

- Updated `docker-compose.yml`.
- `migrations/M22_pgaudit.sql`.
- `ops/audit/bootstrap.sh`.
- `services/audit-shipper/{Dockerfile,main.py,requirements.txt}`.
- `docs/ops/audit-runbook.md` (retention policy, legal hold, breach response).

## 7 Acceptance criteria

1. After M22, every INSERT/UPDATE/DELETE on `moa.node_index` produces a pgaudit log line.
2. SELECT on `moa.node_index` produces a READ audit line (object-level).
3. Audit shipper uploads at least one segment within 1 hour of activity.
4. S3 GetObjectRetention shows COMPLIANCE / 2190 days.
5. PHI plaintext does NOT appear in audit lines (parameters off).

## 8 Tests

```sh
docker compose up -d postgres moa-audit-shipper
cargo run --bin migrate
cargo test --test audit_smoke
aws s3 ls s3://moa-audit-dev/ --recursive
```

## 9 Cleanup

- Remove any homegrown audit logging code that wrote to local files / Postgres tables in an ad-hoc shape.
- Remove any `# TODO: HIPAA audit` markers.

## 10 What's next

**M23 — `moa privacy export` CLI (GDPR Art. 15 subject access).**

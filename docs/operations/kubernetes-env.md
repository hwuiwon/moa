# Kubernetes Environment

MOA services run without sticky routing. Any request can land on any `moa-edge`
or `moa-orchestrator` pod, so correctness state must live in Postgres, Restate,
or the Redis-backed runtime cache.

## Required Runtime State

Set the Redis runtime cache explicitly:

```bash
MOA_RUNTIME_CACHE_BACKEND=redis
MOA_RUNTIME_CACHE_REDIS_URL=redis://<managed-redis-host>:6379
```

If the runtime cache resolves to memory, the orchestrator fails startup. The
memory backend is per-pod best-effort only; it must not be used for Slack
edit/delete refs, send pacing, or future cross-replica coordination.

Postgres remains the source of truth for durable session state:

- `moa.hand_leases` stores hand/sandbox bindings, lease generations, expiry,
  status, and serialized provider handles for cross-pod reuse and cleanup.
- `session_blobs` stores default claim-check payloads so another pod can replay
  or resolve large event payload references.
- Trusted sandbox file manifests are carried by durable request references, not
  by process-local router memory.

Session attachment bytes must use cloud object storage so any `moa-edge` pod can
serve reload/download requests:

```bash
# AWS S3
MOA_SESSION_ATTACHMENT_BACKEND=s3
MOA_SESSION_ATTACHMENT_BUCKET=<attachment-bucket>
MOA_SESSION_ATTACHMENT_PREFIX=session-attachments
MOA_SESSION_ATTACHMENT_REGION=<aws-region>
MOA_SESSION_ATTACHMENT_ALLOW_HTTP=false

# GCS alternative
MOA_SESSION_ATTACHMENT_BACKEND=gcs
MOA_SESSION_ATTACHMENT_BUCKET=<attachment-bucket>
MOA_SESSION_ATTACHMENT_PREFIX=session-attachments
MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH=/var/run/secrets/gcp/application-default.json
```

Managed cloud deployments should point attachment storage at AWS S3 or GCS.
Local compose uses RustFS with an explicit local HTTP endpoint.

## Lineage And Audit Metrics

The Postgres-backed lineage sink exposes queue and writer health separately:

- `moa_lineage_accepted_total{durability="postgres"}` counts events after their
  append to `analytics.lineage_journal` commits.
- `moa_lineage_dropped_total{mode="best_effort",event_class}` counts events
  rejected because the process-local best-effort ingress channel was full. It
  is not an audit-loss signal for durable writes.
- `moa_lineage_failed_total{mode,reason}` counts failed acceptance paths. The
  bounded values are `mode="best_effort",reason="channel_closed|accept_failed"`
  and `mode="durable",reason="accept_timeout"`.
- `moa_lineage_written_total` counts journal rows written to the lineage store.
- `moa_lineage_journal_depth` and
  `moa_lineage_journal_oldest_age_seconds` report the committed queue backlog.

Dashboards should use journal depth and oldest age for writer-capacity pressure.
Alerts should use `moa_lineage_failed_total` as the compliance/audit acceptance
failure signal when compliance lineage is enabled.

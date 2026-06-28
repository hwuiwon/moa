# Kubernetes Environment

MOA services run without sticky routing. Any request can land on any `moa-edge`
or `moa-orchestrator` pod, so correctness state must live in Postgres, Restate,
or the Redis-backed runtime cache.

## Required Runtime State

Set cloud mode and Redis runtime cache explicitly:

```bash
MOA_CLOUD_ENABLED=true
MOA_RUNTIME_CACHE_BACKEND=redis
MOA_RUNTIME_CACHE_REDIS_URL=redis://<managed-redis-host>:6379
```

If `MOA_CLOUD_ENABLED=true` and the runtime cache resolves to memory, the
orchestrator fails startup. The memory backend is local-dev and per-pod
best-effort only; it must not be used for Slack edit/delete refs, send pacing,
or future cross-replica coordination.

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

Cloud startup fails if attachment storage points at a local RustFS endpoint.

## Lineage And Audit Metrics

Lineage queue pressure now separates accepted, backpressured, failed, and
explicitly lossy events:

- `moa_lineage_accepted_total{durability="journal"}` counts events after the
  durable journal append succeeds.
- `moa_lineage_backpressure_total{mode="durable"}` means the event is already
  journaled but the writer notification channel was full.
- `moa_lineage_dropped_total{mode="lossy_telemetry"}` is only for explicitly
  lossy telemetry/score paths; it must not be used as an audit-loss signal.
- `moa_lineage_failed_total{mode="durable"}` means durable acceptance failed
  and should page if compliance lineage is enabled.

Dashboards and alerts should treat `moa_lineage_backpressure_total` as a writer
capacity signal and `moa_lineage_failed_total` as the compliance/audit failure
signal.

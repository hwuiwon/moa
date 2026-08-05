# 10 — Technology Stack

_Crates, services, build targets, and deployment dependencies._

## Rust Workspace

The root workspace package inventory comes from `cargo metadata --no-deps`:

- Core/runtime: `moa-core`, `moa-config` (the `MoaConfig` tree and `EnvOverlay`),
  `moa-wire` (shared HTTP wire DTOs), `moa-brain`, `moa-execution`, `moa-db`,
  `moa-session`, `moa-runtime-store`, `moa-edge`, `moa-orchestrator`,
  `moa-migrations`.
- Memory/knowledge: `moa-knowledge`, `moa-memory-graph`,
  `moa-memory-ingest`, `moa-memory-lifecycle`, `moa-memory-pii`,
  `moa-memory-types`, `moa-memory-vector`,
  `moa-retrieval` (hybrid graph-memory retrieval engine and query planner).
- Auth/security/audit: `moa-authz`, `moa-authz-schema`,
  `moa-auth-providers` (identity, feature-gated Auth0/OIDC and CIBA, and the
  durable tenant credential owner), `moa-fga-bootstrap`,
  `moa-ocsf`, `moa-security`, `moa-crypto` (envelope encryption and BYOK),
  `moa-kms` (Postgres-backed key management).
- Lineage/observability/analytics: `moa-lineage-core`,
  `moa-lineage-citation`, `moa-lineage-sink`,
  `moa-lineage-audit`, `moa-observability`, `moa-analytics`,
  `moa-analytics-export` (incremental Postgres-to-ClickHouse exporter; see
  [ClickHouse Analytics: Supported, Not Deployed](#clickhouse-analytics-supported-not-deployed)).
- Product domains: `moa-agents`, `moa-contacts`, `moa-artifacts`, `moa-connectors`,
  `moa-experiments`, `moa-messaging`, `moa-skills`.
- Providers/tools/eval/dev: `moa-hands`, `moa-providers` (including
  provider-egress DLP governance),
  `moa-eval-core`, `moa-eval`, `moa-loadtest`, `moa-test-support`,
  `workspace-hack`, `xtask`.

Build-graph boundaries keep optional tooling out of ordinary builds:

- `xtask` gates evaluation dependencies behind `eval-tools`;
- `moa-test-support` gates orchestrator fixtures behind `orchestrator-fixture`;
- `moa-brain` gates evaluation-only harness code behind `eval-harness`;
- `moa-eval-core` has no direct `moa-providers` or `sqlx` dependency;
  provider-aware construction lives in `moa-eval`;
- score and experiment persistence stay in `moa-experiments`; and
- `moa-memory-lifecycle` has no dependency on `moa-memory-ingest`.

The orchestrator constructs one `RuntimeDeps` graph per process. That graph owns
one shared `IngestRuntime` and one shared `MemoryRetrievalEngine`; brain context
retrieval, memory service/tool reads, and fast/slow ingestion adapters receive
those instances explicitly rather than installing process globals.

## Core Dependencies

| Area | Crates |
|---|---|
| Async runtime | `tokio`, `tokio-util`, `async-trait` |
| Serialization | `serde`, `serde_json`; `toml` remains for eval and skill-suite fixtures |
| IDs and time | `uuid`, `chrono` |
| Errors | `thiserror` for libraries, `anyhow` for binaries |
| Logging/observability | `tracing`, `tracing-subscriber`, `opentelemetry`, `tracing-opentelemetry` |
| Repo binaries | `clap` for repo tools such as load tests and bootstraps |
| HTTP | `reqwest`, `axum` |
| Database | `sqlx` with Postgres for runtime queries; `refinery` for all Postgres schema migrations |
| Orchestration | `restate-sdk` |
| Execution plans | `serde_json`, `serde_canonical_json`, `blake3`, UUIDv5, and `jsonschema` 0.47 with Draft 2020-12 validation and remote/file retrieval disabled in `moa-execution` |
| Scheduling | Restate `CronJob` virtual object |
| Runtime cache | Redis-backed coordination for the orchestrator; in-process memory exists only for isolated non-orchestrator tests |
| Security | `secrecy`, `shell-words`; `moa-crypto` envelope encryption backed by Postgres KMS state and mounted generation keyrings |
| Containers/tools | Docker integration, Daytona/E2B HTTP clients, MCP transports |
| Lineage and audit | OTel/OpenInference bridge, Parquet/Arrow cold export, Object Lock audit storage |

`moa-migrations` owns the fresh-install-only, contiguous 54-file PostgreSQL
chain and the central table-ownership manifest. The current 143 table
families span 148 `CREATE TABLE` declarations and map one-to-one to 143
ownership entries. `cargo run -p xtask --locked -- check-migrations` enforces
this contract.

## External Services

### Required For Local Development

| Service | Purpose |
|---|---|
| Postgres 17.6+ with pgaudit; pgvector when the pgvector backend is enabled | Session store, relational graph memory, event search, sidecar indexes, embeddings, learning tables |
| OpenFGA v1.8 | Authorization engine. Postgres-backed. Self-hosted by default; Auth0 FGA is a future managed swap-in. |
| Redis or Valkey | Shared runtime cache for pacing and cross-replica transient references |
| LLM provider | Anthropic, OpenAI, or Google Gemini |

Docker is used by the dev stack and optionally by local hand providers.
The out-of-process `moa-pii-service` classifier is optional and runs only under
the Compose `pii` profile when `MOA_PII_SERVICE_URL` is configured.

### Required For Cloud Runtime

| Service | Purpose |
|---|---|
| Restate | Durable orchestration engine |
| Postgres/Neon | Product data store, relational graph storage, and pgvector transactional vector source |
| Redis or Valkey | Shared runtime cache for orchestrator replicas |
| AWS S3 or GCS | Session attachment byte storage in cloud |
| Turbopuffer | Cloud vector backend for storage partitions configured away from local pgvector |
| LLM provider | Model calls and optional embeddings |
| Hand provider | Runtime-configured local, Daytona, or E2B execution |
| Kubernetes or equivalent | Hosting Restate and MOA services |
| Debezium + Kafka-compatible broker | Optional graph changelog CDC for audit export, bridge sync, and cache invalidation |

### Optional

| Service | Purpose |
|---|---|
| Neon branching | Database checkpoint/rollback support |
| External secret manager | Deployment-time injection for provider/operator secrets; tenant connector credential series remain in the Postgres/KMS-backed `CredentialVault` |
| Grafana/Loki/Tempo/Mimir stack | Logs, metrics, and traces. MOA pushes all three signals over OTLP to one collector base URL and exposes no scrape port; `MOA_METRICS_EXPORTER=prometheus` is a development-only mode |
| Messaging platforms | Slack adapter |
| Linked integration providers | Nango and Merge for tenant knowledge linked-account flow, sync trigger, changed-record listing, and webhooks |
| Document parsers | `liteparse` for native local file parsing; LlamaParse, Unstructured, and Reducto for configured external tenant knowledge parsing when native parsing is insufficient |
| RustFS | Local S3-compatible attachment storage for docker-compose development |

## ClickHouse Analytics: Supported, Not Deployed

ClickHouse is an optional analytics backend, selected by the
presence of the `[clickhouse]` config section (in practice `MOA_CLICKHOUSE_URL`).
Its status was settled by measurement on 2026-07-28 and is recorded here so the
next reader does not have to re-derive it.

**Decision: supported, gated, and deliberately not deployed.** Postgres
materialized views remain the only analytics backend any shipped deployment
uses. No Kubernetes overlay references ClickHouse, and none should until
analytics volume justifies one — at the time of the decision the measurement
could not be made, because there was no production deployment and no analytics
volume to measure.

| Property | Value |
|---|---|
| Owner | `moa-analytics-export` (exporter and ClickHouse schema), `moa-analytics` (dual-dialect query compiler) |
| Deployment | Local only, via `docker compose --profile clickhouse`. Absent from every Kubernetes overlay. |
| SLO | Export freshness: `moa_analytics_export_lag_seconds <= 300` for any exported table, alerted by `MOAAnalyticsExportLag` after 10 minutes |
| Supported test path | `make test-clickhouse` (nextest profile `clickhouse-docker`), triggered in CI by the `clickhouse-analytics` job in `.github/workflows/integration-tests.yml` |
| Runbook | [Data Operations](19-data-operations.md#clickhouse-copies-and-tenant-deletion) |

Two properties are load-bearing and easy to lose:

- **Lineage stays in Postgres.** Enabling ClickHouse switches only analytics
  read models and their exporter. `analytics.turn_lineage`, compliance chaining,
  lineage queries, and tenant lineage deletion remain Postgres-owned.
- **`CREATE TABLE IF NOT EXISTS` is not a migration.** Against an existing table
  it is a silent no-op that reports success, so the bootstrap validates every
  table against the columns its own DDL declares and refuses to start on drift.
  Adding a column to `TABLE_DDL` is therefore a breaking change for any existing
  ClickHouse database, which must be migrated or rebuilt.

The cost of keeping this backend is concentrated in one place: the ClickHouse
dialect carries roughly 169 hand-written per-`(dataset, field)` SQL expressions,
so a catalog change is a four-to-five-place edit spanning the catalog, the
dialect, the exporter row structs, and the ClickHouse DDL. Compile cost, by
contrast, is not a meaningful factor — the `clickhouse` crate and its five
exclusive transitive dependencies add about nine seconds to a cold build.

## Observability Collection And Alert Delivery

MOA pushes application logs, metrics, and traces; nothing scrapes MOA. Production runs the orchestrator
and edge behind non-sticky Services with autoscaled replica counts, so a scrape
through the Service lands on an arbitrary replica each interval and produces a
series that blends unrelated processes — counters go backwards, gauges flip, and
no query over it means anything. `MOA_METRICS_EXPORTER=otlp` is the default and
the only production setting; `prometheus` is a single-process development mode
that requires an explicit listen address.

Collection is one Grafana Alloy deployment in the `observability` namespace,
manifested under `k8s/observability/`.

| Property | Value | Why it is not a preference |
|---|---|---|
| Replicas | exactly 1, `Recreate` strategy | Two collectors behind one Service split the OTLP stream across two independent write-ahead logs, and two rule synchronizers reconcile the same Mimir namespace against each other |
| Buffer | 20Gi `ReadWriteOnce` PVC at `/var/lib/alloy` | The WAL is the delivery guarantee. On an `emptyDir`, a backend outage plus a pod restart is silent permanent loss |
| Image | exact release tag, never `latest` | A collector that picks up a new version on an unrelated restart changes pipeline semantics with no change to this repository |
| Config | `k8s/observability/config.alloy`, generated into a ConfigMap with a content-hash name | A standalone `.alloy` file is checkable by `alloy validate`; the name hash is what rolls the pod when the config changes |
| Restate scrape discovery | Kubernetes pod discovery, namespace/cluster filtered, port 5122 | Pod identity remains distinct through rollout; Restate network peers explicitly admit Alloy rather than routing samples through a Service |
| Rule delivery | `mimir.rules.kubernetes`, selecting `moa.dev/rule-sync=mimir` | One owner for Mimir's rule namespace. An unselected synchronizer adopts, and can overwrite, rules this deployment does not own |

Alert rules are `PrometheusRule` resources under `ops/prometheus/alerts/`,
rendered by `k8s/observability` and synchronized into Mimir by that component.
A rule missing the `moa.dev/rule-sync` label is deployed to the cluster and
evaluated by nothing.

Grafana dashboards are separate from Kubernetes delivery. Their canonical JSON
lives under `dashboards/grafana/`, and the `sync-grafana-dashboards` GitHub
workflow imports every dashboard through Grafana's dashboard API after changes
land on `main`. Stable dashboard UIDs plus overwrite mode make that operation
idempotent. The required service-account credentials and optional destination
folder are documented beside the dashboards.

Two settings on `otelcol.exporter.prometheus` are load-bearing and easy to
misread as defaults worth keeping. `add_metric_suffixes = false` stops the
OTLP-to-Prometheus translator appending its own type and unit suffixes to names
that already follow Prometheus convention — that suffixing is a rename, and a
renamed metric makes every alert and dashboard query return no data while the
export itself is working perfectly. `resource_to_telemetry_conversion = true`
copies `service.name`, `deployment.environment` and `service.version` onto every
series; without it they exist only on a separate `target_info` metric and no
query can scope itself to a service.

### Manifest And Observability Contracts

Nothing in the Rust build reads a manifest, an Alloy config, or an alert rule.
Three checks close that gap, all offline, all run by the `manifests` CI job:

| Check | Command | Catches |
|---|---|---|
| Rendered manifests | `./k8s/scripts/smoke.sh --validate-manifests` | Strict `kubeconform` against real schemas including the vendored CRDs, plus content assertions for what schemas cannot see |
| Observability contracts | `./k8s/scripts/validate-observability.sh` | `alloy validate`, `promtool check rules`, and the metric names linking alert expressions to the Rust source |
| Schema reproducibility | `./k8s/schemas/refresh.sh` | A hand-edited vendored schema, which would quietly widen what validation accepts |

CRD schemas are vendored under `k8s/schemas/`, pinned by upstream release tag
and content checksum in `sources.json`. They exist so manifest validation never
needs `-ignore-missing-schemas`: with that flag every custom resource passes
unchecked while the summary still reports success.

One documented limit, because a validator's coverage should not be assumed from
the fact that it passes. `RestateDeployment.spec.template.spec` carries
`x-kubernetes-preserve-unknown-fields: true` upstream, so the **entire pod spec
inside a RestateDeployment — probes, env, grace periods — is free-form to every
schema validator**. Fields there are covered by explicit content assertions in
`smoke.sh --validate-manifests` instead, which is why the termination grace
periods are asserted by name.

## Build Targets

```bash
cargo build
cargo nextest run --locked
cargo test --locked --doc
cargo test --workspace --no-run --locked --timings
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
MOA_DATABASE_URL=postgres://... cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
MOA_DATABASE_URL=postgres://runtime-role@... MOA_DATABASE_ADMIN_URL=postgres://migration-role@... cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- migrate
```

Run `migrate` as a distinct deployment phase before any runtime replica. The
default orchestrator command opens only `MOA_DATABASE_URL`, validates the exact
complete embedded history, and fails closed without executing migration DDL.

## Configuration

Runtime config loads from flat `MOA_...` environment variables. Kubernetes
deployments should inject non-secret values with ConfigMaps and secret values
with Secrets. The root `.env.example` lists the canonical env names for local
and deployment setup. Key groups:

| Env group | Controls |
|---|---|
| `MOA_MODELS_*` and `MOA_<PROVIDER>_API_KEY` | model routing and provider API keys |
| `MOA_DATABASE_*` | Postgres URL, admin URL, pool settings, Neon branching |
| `MOA_RUNTIME_CACHE_*` | required Redis-compatible Valkey backend and URL for orchestrator admission and transient coordination |
| `MOA_KMS_*` | durable KMS provider, mounted generation-key directory, and required active generation |
| `MOA_MEMORY_*`, `MOA_PII_SERVICE_URL`, and `MOA_TURBOPUFFER_*` | memory directory, embedding and reranker `provider:model` selectors, PII service, and Turbopuffer cloud vector backend credentials |
| `MOA_KNOWLEDGE_*` | tenant knowledge provider enablement, parser selection, sync limits, and chunking limits |
| `MOA_QUERY_REWRITE_*` | fail-open, retrieval-scoped query rewrite gating and timeout behavior |
| `MOA_RESOLUTION_*` | automated segment assessment weights and thresholds |
| `MOA_SKILL_BUDGET_*` | skill manifest budget controls |
| `MOA_EXECUTION_*` | planner repair, task/token/tool/retrieval/cost defaults, unattended confirmation threshold, and deadlines |
| `MOA_CLOUD_*` | remote hand provider settings |
| `MOA_RESTATE_*` and `MOA_ORCHESTRATOR_*` | Normal-runtime Restate ingress and optional health URL; bootstrap Admin access is an explicit command argument |
| `MOA_AUTH_*`, `MOA_AUTHZ_*`, `MOA_ASYNC_AUTHZ_*`, `MOA_AUDIT_SECURITY_*` | identity, authorization, builtin async authorization challenges, and OCSF security-event audit |
| `MOA_SESSION_BLOB_*` | claim-check blob backend, threshold, and explicit local path when filesystem blobs are used |
| `MOA_SESSION_ATTACHMENT_*` | session upload object storage backend, bucket, prefix, endpoint, and cloud credentials |
| `MOA_PRIVACY_*`, `MOA_LINEAGE_AUDIT_*`, and `MOA_PII_VAULT_SECRET_HEX` | privacy approval verification, DSAR/export signing, lineage audit signing, and PII-vault pseudonymization |
| `MOA_MESSAGING_*` | messaging adapter settings |
| `MOA_PERMISSIONS_*` | default action-policy posture for tool execution |
| `MOA_COMPACTION_*` | history compaction thresholds |

## Current Implementation State

Implemented architectural pillars:

- Restate cloud orchestration with session, worker, tenant, service, and workflow handlers.
- Dynamic `respond`/`act`/`run` routing with `ExecutionRun` and `ExecutionTask` as the only durable typed-DAG runtime.
- `moa-execution` ownership of canonical plan compilation, bindings, pure scheduling, pure integer budget transitions, completion checks, and replan-stop evaluation; these core APIs have no I/O, provider, Restate, or persistence dependencies.
- One `moa-orchestrator` production binary for local development and cloud execution, with domain logic kept behind in-process application and repository boundaries.
- Constructor-based runtime composition: `RuntimeDeps::build` constructs the
  concrete graph and `build_endpoint` binds it, including the durable
  `TenantPurge` workflow. The graph contains one retrieval engine, one ingestion
  runtime, and one delivery sink; turn preparation and OpenFGA are injected into
  their consumers. Runtime cache is passed explicitly to provider and embedding
  composition; live handles are not stored inside serializable `MoaConfig`.
- Plan-backed Behavior Lab runs: run admission requires an immutable
  `experiment_plan` revision, and `PlanTrialPager` is the only trial-expansion
  path.
- Nango/Merge-only tenant knowledge sync over six narrow repository capabilities
  for connections, sync, ingestion, ACLs, contact groups, and provider events.
- Reviewed custom connectors use one HTTP-only artifact contract; code-owned
  Nango/Merge parents remain actionless and internal to knowledge linking.
- Postgres session store with tenant-isolated event log, analytics, task segments, and learning log.
- Postgres hand leases and Postgres-backed claim-check blobs for cross-pod sandbox and replay correctness. A lease carries the exact sandbox policy identity it was provisioned under plus a renewable idle deadline and an immutable hard deadline, and an independent durable reaper destroys expired generations with `SKIP LOCKED` claims rather than waiting for traffic.
- Redis-backed runtime cache for the production orchestrator; the in-memory implementation is limited to isolated non-orchestrator tests and embeddings.
- Provider coordination bounds every runtime-cache operation at 250ms before
  applying its configured fail-closed or bounded-degraded policy. Shared
  cooldown and retry state is keyed by credential, not model.
- Graph memory with relational Postgres nodes and edges, sidecar search, configured vector retrieval, and privacy filtering.
- Query rewriting, segment creation, automated segment assessment, and tenant-level skill resolution-rate ranking.
- Draft-only tenant skill distillation/improvement proposals with explicit review acceptance before learning-log emission; tenant learning remains tenant-local.
- Lineage, eval score storage, cold export support, and opt-in compliance audit tables.
- Hosted API automation surfaces.

Areas still evolving:

- REST product API shape and admin UI details.
- Richer messaging callback typing.
- More complete tenant admin dashboard workflows.
- Production deployment automation around Restate registration and hand provider configuration.

## Deployment Notes

Cloud deployments need:

```bash
MOA_DATABASE_URL=postgres://...
MOA_RESTATE_INGRESS_URL=http://...
MOA_OPENAI_API_KEY=... # or another configured provider key
MOA_KMS_PROVIDER=postgres
MOA_KMS_ROOT_KEY_DIR=/var/run/secrets/moa-kms/root-keys
MOA_KMS_REQUIRED_GENERATION=primary
```

Production Kubernetes provisions `moa-kms-root-keys` externally and mounts it
read-only into orchestrator pods only. The edge never receives root-key
material. The local Kustomize overlay and Docker Compose use a fixed public
development-only key with the same Postgres KMS topology, so local encrypted
data remains readable after process restarts and multi-container behavior
matches production. Key rotation and the opt-in maintenance Jobs are documented
in [KMS Root-Key Rotation](operations/kms-root-key-rotation.md).

Configure a Redis-compatible Valkey runtime cache for every orchestrator:

```bash
MOA_RUNTIME_CACHE_BACKEND=redis
MOA_RUNTIME_CACHE_REDIS_URL=redis://...
```

`moa-orchestrator` fails startup if runtime cache resolution lands on the
in-memory backend. Memory is per-process best effort only and must not be used
for request handling in a distributed deployment.

Configure session attachment object storage for cloud:

```bash
# AWS S3 or S3-compatible HTTPS endpoint
MOA_SESSION_ATTACHMENT_BACKEND=s3
MOA_SESSION_ATTACHMENT_BUCKET=moa-session-attachments
MOA_SESSION_ATTACHMENT_PREFIX=session-attachments
MOA_SESSION_ATTACHMENT_REGION=us-east-1
MOA_SESSION_ATTACHMENT_ALLOW_HTTP=false

# GCS alternative
MOA_SESSION_ATTACHMENT_BACKEND=gcs
MOA_SESSION_ATTACHMENT_BUCKET=moa-session-attachments
MOA_SESSION_ATTACHMENT_PREFIX=session-attachments
MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH=/var/run/secrets/gcp/application-default.json
```

Optional hand and messaging settings depend on the chosen deployment:

```bash
MOA_CLOUD_HANDS_DEFAULT_PROVIDER=daytona
MOA_CLOUD_HANDS_FALLBACK_PROVIDERS=e2b
MOA_CLOUD_HANDS_DAYTONA_API_KEY=...
MOA_CLOUD_HANDS_E2B_API_KEY=...
SLACK_BOT_TOKEN=...
SLACK_APP_TOKEN=...
MOA_MESSAGING_EMAIL_FROM="MOA <no-reply@example.com>"
POSTMARK_SERVER_API_TOKEN=...
TWILIO_ACCOUNT_SID=...
TWILIO_AUTH_TOKEN=...
TWILIO_FROM_NUMBER=...
```

The orchestrator exposes the Restate handler endpoint and a health/readiness
endpoint. Process readiness checks Postgres, KMS, and the optional lineage
writer only. RestateDeployment status is the registration gate; edge startup
calls the side-effect-free public `Health/check` Restate handler.

Provider registration is deployment-static. `ProviderRegistry` is built at
startup from `MoaConfig` and directly injected provider API keys; changing provider
availability requires a rollout unless a future shared provider store is added.

Kubernetes routing is non-sticky. Correctness-sensitive state must be stored in
Postgres, Restate, or Redis-backed `RuntimeCacheStore`. The orchestrator rejects
the process-local memory backend; it remains available only to isolated
non-orchestrator tests.

### Durable Coordination Topology

Durable main-agent/worker coordination keeps working across non-sticky
replicas because all of its correctness state lives in Restate VO/workflow state
and Postgres, never in process memory or Redis:

- **Restate + Postgres are required for correctness.** Child attention signals,
  guarded parent resume, terminal results, heartbeat-stale detection, narration
  scheduling, and self-cleanup are all driven by `Session`/`Worker` VO state,
  Restate awakeables/delayed self-calls, and idempotent Postgres event appends
  (the `session_event_dedupe` guard). Any orchestrator replica can pick up the
  next message or fired tick.
- **Redis is runtime cache only**, never a correctness owner for signals, resume,
  or terminal results.
- **Coordinator/worker/sandbox topology.** The root coordinator
  (`Session`/`TurnExecution`) is sandbox-free; each worker owns one ephemeral
  sandbox keyed `(session_id, worker_id, provider)` in `moa.hand_leases`,
  released on worker self-cleanup. Sandboxes are refreshable: durable state
  lives in the event log and object store, and a crashed sandbox is reprovisioned
  under a new lease generation. RestateDeployment readiness, rather than an
  Admin API call from each replica, gates registered service traffic.

Durable execution runs use a separate topology. `ExecutionRun` and
`ExecutionTask` workflows recover from Postgres execution rows plus Restate
journals; the `Session` VO stores only compact linkage and terminal-synthesis
dedupe state. Ready map items have stable logical task identities and no
application fan-out cap. Atomic run-budget reservations bound admission, while
provider pacing and governed capability or hand capacity supply physical
backpressure.

# 10 — Technology Stack

_Crates, services, build targets, and deployment dependencies._

## Rust Workspace

The root workspace package inventory comes from `cargo metadata --no-deps`:

- Core/runtime: `moa-core`, `moa-brain`, `moa-execution`, `moa-db`, `moa-session`,
  `moa-runtime-store`, `moa-edge`, `moa-orchestrator`, `moa-migrations`.
- Memory/knowledge: `moa-knowledge`, `moa-memory-graph`,
  `moa-memory-ingest`, `moa-memory-lifecycle`, `moa-memory-pii`,
  `moa-memory-types`, `moa-memory-vector`.
- Auth/security/audit: `moa-authz`, `moa-authz-schema`,
  `moa-auth-providers`, `moa-auth-providers-auth0`, `moa-fga-bootstrap`,
  `moa-ocsf`, `moa-security`.
- Lineage/observability/analytics: `moa-lineage-core`,
  `moa-lineage-citation`, `moa-lineage-sink`,
  `moa-lineage-audit`, `moa-observability`, `moa-analytics`.
- Product domains: `moa-agents`, `moa-contacts`, `moa-artifacts`,
  `moa-experiments`, `moa-scoring`, `moa-messaging`, `moa-skills`.
- Providers/tools/eval/dev: `moa-hands`, `moa-providers`,
  `moa-eval-core`, `moa-eval`, `moa-loadtest`, `moa-test-support`,
  `workspace-hack`, `xtask`.

Build-graph boundaries keep optional tooling out of ordinary builds:

- `xtask` gates evaluation dependencies behind `eval-tools`;
- `moa-test-support` gates orchestrator fixtures behind `orchestrator-fixture`;
- `moa-brain` gates evaluation-only harness code behind `eval-harness`;
- `moa-eval-core` has no direct `moa-providers` or `sqlx` dependency;
  provider-aware construction lives in `moa-eval`;
- scoring and experiment persistence stay in their owning crates; and
- `moa-memory-lifecycle` has no dependency on `moa-memory-ingest`.

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
| Runtime cache | Redis-backed coordination for the orchestrator; in-process memory exists only for isolated local/test code |
| Security | `secrecy`, `shell-words`; `moa-crypto` envelope encryption backed by Postgres KMS state and mounted generation keyrings |
| Containers/tools | Docker integration, Daytona/E2B HTTP clients, MCP transports |
| Lineage and audit | OTel/OpenInference bridge, Parquet/Arrow cold export, Object Lock audit storage |

`moa-migrations` owns the central table-ownership manifest. The current 98
table declarations resolve to 90 owned logical families, enforced by
`cargo run -p xtask --locked -- check-migrations`.

## External Services

### Required For Local Development

| Service | Purpose |
|---|---|
| Postgres 17.6+ with pgaudit; pgvector when the pgvector backend is enabled | Session store, relational graph memory, event search, sidecar indexes, embeddings, learning tables |
| OpenFGA v1.8 | Authorization engine. Postgres-backed. Self-hosted by default; Auth0 FGA is a future managed swap-in. |
| Redis or Valkey | Shared runtime cache for pacing and cross-replica transient references |
| `moa-pii-service` | Out-of-process `openai/privacy-filter` inference for memory privacy classification |
| LLM provider | Anthropic, OpenAI, or Google Gemini |

Docker is used by the dev stack and optionally by local hand providers.

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
| Debezium + Kafka-compatible broker | Optional graph changelog CDC for audit shipping, bridge sync, and cache invalidation |

### Optional

| Service | Purpose |
|---|---|
| Neon branching | Database checkpoint/rollback support |
| HashiCorp Vault or similar | Cloud credential storage |
| Grafana/Tempo/Prometheus stack | Metrics and traces |
| Messaging platforms | Slack adapter |
| Linked integration providers | Nango and Merge for tenant knowledge linked-account flow, sync trigger, changed-record listing, and webhooks |
| Document parsers | `liteparse` for native local file parsing; LlamaParse, Unstructured, and Reducto for configured external tenant knowledge parsing when native parsing is insufficient |
| RustFS | Local S3-compatible attachment storage for docker-compose development |

## Build Targets

```bash
cargo build
cargo nextest run --locked
cargo test --locked --doc
cargo test --workspace --no-run --locked --timings
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
MOA_DATABASE_URL=postgres://... cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
MOA_DATABASE_ADMIN_URL=postgres://... cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- migrate
```

## Configuration

Runtime config loads from flat `MOA_...` environment variables. Kubernetes
deployments should inject non-secret values with ConfigMaps and secret values
with Secrets. The root `.env.example` lists the canonical env names for local
and deployment setup. Key groups:

| Env group | Controls |
|---|---|
| `MOA_MODELS_*` and `MOA_<PROVIDER>_API_KEY` | model routing and provider API keys |
| `MOA_DATABASE_*` | Postgres URL, admin URL, pool settings, Neon branching |
| `MOA_RUNTIME_CACHE_*` | runtime cache backend selection and Redis URL for shared transient coordination |
| `MOA_KMS_*` | durable KMS provider, mounted generation-key directory, and required active generation |
| `MOA_MEMORY_*`, `MOA_PII_SERVICE_URL`, and `MOA_TURBOPUFFER_*` | memory directory, embedding and reranker `provider:model` selectors, PII service, and Turbopuffer cloud vector backend credentials |
| `MOA_KNOWLEDGE_*` | tenant knowledge provider enablement, parser selection, sync limits, and chunking limits |
| `MOA_QUERY_REWRITE_*` | fail-open, retrieval-scoped query rewrite gating and timeout behavior |
| `MOA_RESOLUTION_*` | automated segment assessment weights and thresholds |
| `MOA_SKILL_BUDGET_*` | skill manifest budget controls |
| `MOA_EXECUTION_*` | planner repair, task/token/tool/retrieval/cost defaults, unattended confirmation threshold, and deadlines |
| `MOA_CLOUD_*` | remote hand provider settings |
| `MOA_RESTATE_*` and `MOA_ORCHESTRATOR_*` | Restate ingress/admin endpoints and optional health URL |
| `MOA_AUTH_*`, `MOA_AUTHZ_*`, `MOA_TOKEN_VAULT_*`, `MOA_ASYNC_AUTHZ_*`, `MOA_AUDIT_SECURITY_*` | identity, authorization, token vault, builtin async authorization challenges, and OCSF security-event audit |
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
  `TenantPurge` workflow.
- Postgres session store with tenant-isolated event log, analytics, task segments, and learning log.
- Postgres hand leases and Postgres-backed claim-check blobs for cross-pod sandbox and replay correctness.
- Redis-backed runtime cache for the production orchestrator; the in-memory implementation is limited to isolated non-orchestrator tests and embeddings.
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
MOA_RESTATE_ADMIN_URL=http://...
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

Configure Redis when runtime cache state should coordinate across replicas:

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

The orchestrator exposes the Restate handler endpoint and a health/readiness endpoint. Readiness checks Postgres and can optionally require registered Restate services.

Provider registration is deployment-static. `ProviderRegistry` is built at
startup from `MoaConfig` and directly injected provider API keys; changing provider
availability requires a rollout unless a future shared provider store is added.

Kubernetes routing is non-sticky. Correctness-sensitive state must be stored in
Postgres, Restate, or Redis-backed `RuntimeCacheStore`. The memory runtime cache
backend is per process and suitable only for local development or best-effort
transient behavior.

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
  under a new lease generation. Readiness should still require the orchestrator's
  Restate services to be registered before the replica takes traffic.

Durable execution runs use a separate topology. `ExecutionRun` and
`ExecutionTask` workflows recover from Postgres execution rows plus Restate
journals; the `Session` VO stores only compact linkage and terminal-synthesis
dedupe state. Ready map items have stable logical task identities and no
application fan-out cap. Atomic run-budget reservations bound admission, while
Restate concurrency rules and provider pacing supply physical backpressure.

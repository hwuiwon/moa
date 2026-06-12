# 10 — Technology Stack

_Crates, services, build targets, and deployment dependencies._

## Rust Workspace

The root workspace currently contains:

| Crate | Purpose |
|---|---|
| `moa-core` | Shared traits, DTOs, config, events, telemetry, analytics helpers |
| `moa-brain` | Context pipeline, query rewriting, task segmentation helpers, resolution scoring |
| `moa-session` | Postgres session store, event log, task segments, learning log, analytics |
| `moa-memory/graph` (`moa-memory-graph`) | Graph-memory sidecar tables, RLS, changelog, and AGE projection helpers |
| `moa-memory/ingest` (`moa-memory-ingest`) | Slow-path graph-memory ingestion DTOs and deterministic helpers |
| `moa-memory/pii` (`moa-memory-pii`) | PII classification client and privacy-class aggregation helpers |
| `moa-memory/vector` (`moa-memory-vector`) | VectorStore trait, Gemini/Cohere embedders, pgvector halfvec backend, and Turbopuffer opt-in backend |
| `moa-lineage/core` (`moa-lineage-core`) | Lineage record and score record types |
| `moa-lineage/sink` (`moa-lineage-sink`) | Async lineage sink writers |
| `moa-lineage/otel` (`moa-lineage-otel`) | OTel/OpenInference bridge |
| `moa-lineage/citation` (`moa-lineage-citation`) | Citation/provenance adapters |
| `moa-lineage/cold` (`moa-lineage-cold`) | Cold lineage export and partition support |
| `moa-lineage/audit` (`moa-lineage-audit`) | Compliance audit hash chain, Merkle root, signing, and DSAR support |
| `moa-auth/authz-schema` (`moa-authz-schema`) | Typed OpenFGA tuple keys and model constants |
| `moa-auth/authz` (`moa-authz`) | OpenFGA client, authorization checks, transactional outbox, and outbox poller |
| `moa-auth/providers` (`moa-auth-providers`) | Local API keys, disabled auth, builtin approvals, null token vault, and provider bundle construction |
| `moa-auth/auth0` (`moa-auth-providers-auth0`) | Optional Auth0 and generic OIDC providers gated by the `auth0` feature |
| `moa-auth/fga-bootstrap` (`moa-fga-bootstrap`) | OpenFGA store and authorization-model bootstrap binary |
| `moa-ocsf` | OCSF v1.3 security-event types, emit helpers, signing, and persistence |
| `moa-hands` | Tool router, local/Docker hands, Daytona, E2B, MCP |
| `moa-providers` | Anthropic, OpenAI, Gemini, embedding provider wiring |
| `moa-orchestrator` | Restate services, virtual objects, workflows, `moa-orchestrator-bin` cloud binary |
| `moa-gateway` | Telegram, Slack, Discord adapters and renderers |
| `moa-security` | Credential vault, policies, MCP proxy, prompt-injection controls |
| `moa-skills` | Skill parser, DB-backed package registry, distillation, improvement, regression generation |
| `moa-eval` | Evaluation harness |
| `moa-loadtest` | Direct HTTP load-test harness for hosted orchestrator APIs |
| `workspace-hack` | Generated `cargo-hakari` feature unification crate |
| `xtask` | Repo-local audits and maintenance commands |

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
| Database | `sqlx` with Postgres, migrations, JSON, UUID, chrono |
| Orchestration | `restate-sdk` |
| Scheduling | Restate `CronJob` virtual object |
| Security | `age`, `secrecy`, `shell-words` |
| Containers/tools | Docker integration, Daytona/E2B HTTP clients, MCP transports |
| Lineage and audit | OTel/OpenInference bridge, Parquet/Arrow cold export, Object Lock audit storage |

## External Services

### Required For Local Development

| Service | Purpose |
|---|---|
| Postgres 17.6+ with Apache AGE, pgvector, and pgaudit | Session store, graph memory, event search, sidecar indexes, embeddings, learning tables |
| OpenFGA v1.8 | Authorization engine. Postgres-backed. Self-hosted by default; Auth0 FGA is a future managed swap-in. |
| `moa-pii-service` | Out-of-process `openai/privacy-filter` inference for memory privacy classification |
| LLM provider | Anthropic, OpenAI, or Google Gemini |

Docker is used by the dev stack and optionally by local hand providers.

### Required For Cloud Runtime

| Service | Purpose |
|---|---|
| Restate | Durable orchestration engine |
| Postgres/Neon | Product data store |
| LLM provider | Model calls and optional embeddings |
| Hand provider | Daytona, E2B, or configured local/container execution |
| Kubernetes or equivalent | Hosting Restate and MOA services |
| Debezium + Kafka-compatible broker | Optional graph changelog CDC for audit shipping, bridge sync, and cache invalidation |

### Optional

| Service | Purpose |
|---|---|
| Neon branching | Database checkpoint/rollback support |
| HashiCorp Vault or similar | Cloud credential storage |
| Grafana/Tempo/Prometheus stack | Metrics and traces |
| Messaging platforms | Telegram, Slack, Discord adapters |

## Build Targets

```bash
cargo build
cargo test --workspace --no-run
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
MOA_DATABASE_URL=postgres://... cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
```

## Configuration

Runtime config loads from flat `MOA_...` environment variables. Kubernetes
deployments should inject non-secret values with ConfigMaps and secret values
with Secrets. The root `.env.example` lists the canonical env names for local
and deployment setup. Key groups:

| Env group | Controls |
|---|---|
| `MOA_MODELS_*` and `MOA_PROVIDERS_*` | model routing and provider API key env names |
| `MOA_DATABASE_*` | Postgres URL, admin URL, pool settings, Neon branching |
| `MOA_MEMORY_*`, `MOA_PII_SERVICE_URL`, and `MOA_TURBOPUFFER_*` | memory directory, embedding provider/model, PII service, and vector backend |
| `MOA_QUERY_REWRITE_*` | fail-open, retrieval-scoped query rewrite gating and timeout behavior |
| `MOA_RESOLUTION_*` | automated resolution scoring weights and thresholds |
| `MOA_SKILL_BUDGET_*` | skill manifest budget controls |
| `MOA_CLOUD_*` | cloud mode and hand provider settings |
| `MOA_RESTATE_*` and `MOA_ORCHESTRATOR_*` | Restate ingress/admin endpoints and optional health URL |
| `MOA_AUTH_*`, `MOA_AUTHZ_*`, `MOA_TOKEN_VAULT_*`, `MOA_ASYNC_AUTHZ_*`, `MOA_AUDIT_SECURITY_*` | identity, authorization, token vault, async approvals, and OCSF security-event audit |
| `MOA_GATEWAY_*` | messaging adapter token env names |
| `MOA_PERMISSIONS_*` | default approval posture |
| `MOA_COMPACTION_*` | history compaction thresholds |

## Current Implementation State

Implemented architectural pillars:

- Restate cloud orchestration with session, sub-agent, workspace, service, and workflow handlers.
- Restate orchestrator for local development and cloud execution.
- Postgres session store with event log, analytics, task segments, and learning log.
- Graph memory with Postgres sidecar search, AGE projection helpers, pgvector semantic search, and privacy filtering.
- Query rewriting, segment creation, automated resolution scoring, and skill resolution-rate ranking.
- Skill distillation/improvement with learning-log emission.
- Lineage, eval score storage, cold export support, and opt-in compliance audit tables.
- Hosted API automation surfaces.

Areas still evolving:

- REST product API shape and admin UI details.
- Richer gateway callback typing.
- More complete tenant admin dashboard workflows.
- Production deployment automation around Restate registration and hand provider configuration.

## Deployment Notes

Cloud deployments need:

```bash
MOA_DATABASE_URL=postgres://...
MOA_RESTATE_ADMIN_URL=http://...
MOA_RESTATE_INGRESS_URL=http://...
OPENAI_API_KEY=... # or another configured provider key
```

Optional hand and gateway settings depend on the chosen deployment:

```bash
DAYTONA_API_KEY=...
E2B_API_KEY=...
TELEGRAM_BOT_TOKEN=...
SLACK_BOT_TOKEN=...
SLACK_APP_TOKEN=...
DISCORD_BOT_TOKEN=...
```

The orchestrator exposes the Restate handler endpoint and a health/readiness endpoint. Readiness checks Postgres and can optionally require registered Restate services.

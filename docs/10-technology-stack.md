# 10 — Technology Stack

_Crates, services, build targets, and deployment dependencies._

## Rust Workspace

The root workspace currently contains:

| Crate | Purpose |
|---|---|
| `moa-core` | Shared traits, DTOs, config, events, telemetry, analytics helpers |
| `moa-brain` | Context pipeline, query rewriting, task segmentation helpers, intent classification, resolution scoring |
| `moa-session` | Postgres session store, event log, task segments, intents, learning log, analytics |
| `moa-memory` | File-wiki memory, Postgres keyword/trigram search, pgvector embeddings, consolidation |
| `moa-memory-graph` | Graph-memory sidecar tables and AGE projection helpers |
| `moa-memory-vector` | VectorStore trait, Cohere Embed v4 client, and pgvector halfvec backend |
| `moa-hands` | Tool router, local/Docker hands, Daytona, E2B, MCP |
| `moa-providers` | Anthropic, OpenAI, Gemini, embedding provider wiring |
| `moa-orchestrator` | Restate services, virtual objects, workflows, cloud binary |
| `moa-orchestrator-local` | Tokio-task local orchestrator |
| `moa-gateway` | Telegram, Slack, Discord adapters and renderers |
| `moa-runtime` | Shared runtime assembly |
| `moa-cli` | CLI and daemon |
| `moa-security` | Credential vault, policies, MCP proxy, prompt-injection controls |
| `moa-skills` | Skill parser, registry, distillation, improvement, regression generation |
| `moa-eval` | Evaluation harness |
| `moa-loadtest` | Load-test harness |
| `moa-desktop` | GPUI desktop app, not a default workspace member |

## Core Dependencies

| Area | Crates |
|---|---|
| Async runtime | `tokio`, `tokio-util`, `async-trait` |
| Serialization | `serde`, `serde_json`, `toml` |
| IDs and time | `uuid`, `chrono` |
| Errors | `thiserror` for libraries, `anyhow` for binaries |
| Logging/observability | `tracing`, `tracing-subscriber`, `opentelemetry`, `tracing-opentelemetry` |
| CLI | `clap` |
| HTTP | `reqwest`, `axum` |
| Database | `sqlx` with Postgres, migrations, JSON, UUID, chrono |
| Orchestration | `restate-sdk` |
| Local scheduling | `tokio-cron-scheduler` |
| Desktop | `gpui`, `gpui-component`, `tray-icon`, `pulldown-cmark`, `syntect`, `similar` |
| Security | `age`, `secrecy`, `shell-words` |
| Containers/tools | Docker integration, Daytona/E2B HTTP clients, MCP transports |

## External Services

### Required For Local Development

| Service | Purpose |
|---|---|
| Postgres 17.6+ with Apache AGE, pgvector, and pgaudit | Session store, graph memory, event search, memory index, embeddings, learning tables |
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
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings

cargo build -p moa-desktop
cargo run -p moa-cli -- doctor
cargo run -p moa-orchestrator -- --port 9080 --health-port 9081
```

`moa-desktop` is excluded from default workspace builds and should be built explicitly.

## Configuration

Config loads from `~/.moa/config.toml` plus `MOA__...` environment overrides. Key sections:

| Section | Controls |
|---|---|
| `[models]` and `[providers]` | model routing and provider API key env vars |
| `[database]` | Postgres URL, admin URL, pool settings, Neon branching |
| `[memory]` | memory directory and embedding provider/model |
| `[query_rewrite]` | fail-open query rewriter behavior |
| `[resolution]` | automated resolution scoring weights and thresholds |
| `[intents]` | discovery window, min segments, cluster size, classification thresholds |
| `[skill_budget]` | skill manifest budget controls |
| `[cloud]` | cloud mode and hand provider settings |
| `[gateway]` | messaging adapter tokens |
| `[permissions]` | default approval posture |
| `[compaction]` | history compaction thresholds |

## Current Implementation State

Implemented architectural pillars:

- Restate cloud orchestration with session, sub-agent, workspace, service, and workflow handlers.
- Local orchestrator for CLI and desktop.
- Postgres session store with event log, analytics, task segments, intent tables, and learning log.
- File-wiki memory with Postgres keyword search, trigram fallback, and pgvector semantic search.
- Query rewriting, segment creation, automated resolution scoring, and skill resolution-rate ranking.
- Intent discovery workflow and intent manager service.
- Skill distillation/improvement with learning-log emission.
- GPUI desktop crate and CLI/daemon surfaces.

Areas still evolving:

- REST product API shape and admin UI details.
- Richer gateway callback typing.
- More complete tenant admin dashboard workflows.
- Production deployment automation around Restate registration and hand provider configuration.

## Deployment Notes

Cloud deployments need:

```bash
MOA__DATABASE__URL=postgres://...
RESTATE_ADMIN_URL=http://...
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

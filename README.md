# MOA

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/hwuiwon/moa)

**MOA is a cloud-first, Rust-based, multi-tenant AI agent platform that learns from work.**

MOA runs durable agent sessions on Restate, stores product data in Postgres/Neon with pgvector, segments conversations into discrete tasks, scores task resolution automatically, and feeds those outcomes into per-tenant learning. Tenants start with a blank intent taxonomy. MOA discovers candidate intents from their conversations, lets admins curate them, and uses confirmed intents to improve memory retrieval, skill ranking, and tool selection.

Local development uses the same core brain and storage model through the CLI. Cloud deployment uses Restate services, virtual objects, and workflows behind the REST/gateway surfaces.

Status: early active development. The architecture is stable enough to document, but APIs and product surfaces still move.

## What Matters

- **Durable orchestration:** Restate virtual objects own sessions and sub-agents; workflows own one-shot jobs such as memory consolidation and intent discovery.
- **Postgres everywhere:** sessions, events, analytics, task segments, memory indexes, embeddings, intents, and the learning log live in Postgres/Neon.
- **Task-aware sessions:** every session can contain multiple task segments, each with intent metadata, tool and skill usage, cost, and a resolution score.
- **Per-tenant learning:** tenants own their intent taxonomy and learning log. Global catalog intents are opt-in only.
- **Resolution-weighted skills:** skill ranking uses tenant-level resolution data, not only recency or usage count.
- **Inspectable operation:** every event, learning entry, tool call, approval, and materialized analytics view is queryable.
- **Pluggable hands:** local execution, Docker, Daytona, E2B, and MCP tools all route through the hand/tool abstraction.
- **Model-agnostic providers:** Anthropic, OpenAI, and Google Gemini are first-class provider targets.

## Quickstart

Prerequisites: Rust, Docker, Postgres from the repo dev stack, and at least one provider key.

```bash
make dev

export OPENAI_API_KEY=sk-...
# or ANTHROPIC_API_KEY=... / GOOGLE_API_KEY=...

cargo build
cargo run -p moa-cli -- init
cargo run -p moa-cli -- doctor
cargo run -p moa-cli -- exec "What's 2+2?"
```

## Cloud Runtime

Cloud mode runs the `moa-orchestrator` Restate handler service plus Postgres/Neon and the configured hand provider.

```bash
cargo run -p moa-orchestrator -- --port 9080 --health-port 9081
```

The binary registers these Restate surfaces: `Session`, `SubAgent`, `Workspace`, `SessionStore`, `ToolExecutor`, `LLMGateway`, `WorkspaceStore`, `IntentManager`, `Consolidate`, `IntentDiscovery`, and `Health`.

Required cloud configuration includes:

```bash
MOA__DATABASE__URL=postgres://...
RESTATE_ADMIN_URL=http://localhost:9070
OPENAI_API_KEY=...
DAYTONA_API_KEY=... # optional, depending on hand provider
```

## Architecture

```text
REST / Gateway / CLI
        |
        v
Restate handler service (`moa-orchestrator`)
        |
        +-- Session VO -> TurnRunner -> context pipeline -> LLMGateway
        +-- SubAgent VO -> bounded child agent execution
        +-- ToolExecutor -> ToolRouter -> hands / MCP / built-ins
        +-- Consolidate workflow -> memory compaction
        +-- IntentDiscovery workflow -> tenant intent proposals
        |
        v
Postgres / Neon
  sessions, events, task_segments, analytics views,
  graph memory, sidecar indexes, pgvector embeddings,
  tenant_intents, global_intent_catalog, learning_log
```

The context pipeline is byte-stable where possible for prompt caching. With query rewriting enabled, the current processors are: identity, instructions, tools, skills, query rewrite, memory, history, runtime context, compactor, and cache optimizer.

## Memory

Memory is split across four crates under `crates/moa-memory/`:

- `graph/` - Apache AGE adapter, bi-temporal write protocol
- `vector/` - pgvector / Turbopuffer, Cohere Embed v4
- `pii/` - redaction at ingestion via openai/privacy-filter HTTP service
- `ingest/` - slow-path Restate VO, fast-path API, contradiction detector

See `docs/architecture/type-placement.md` for how types are owned across these
crates and `crates/moa-memory/README.md` for crate-level details.

## Workspace Layout

| Crate | Role |
|---|---|
| [`moa-core`](crates/moa-core/) | Shared types, traits, config, events, telemetry, analytics DTOs |
| [`moa-brain`](crates/moa-brain/) | Context pipeline, query rewriting, segment helpers, intent classifier, resolution scoring, streamed turns |
| [`moa-session`](crates/moa-session/) | Postgres session store, event log, task segments, intent tables, learning log, analytics views |
| [`moa-memory-graph`](crates/moa-memory/graph/) | Graph memory store, SQL sidecars, RLS, bitemporal state, and changelog |
| [`moa-memory-ingest`](crates/moa-memory/ingest/) | Slow-path graph ingestion and fast memory write APIs |
| [`moa-memory-vector`](crates/moa-memory/vector/) | pgvector-backed graph embeddings and vector lookup |
| [`moa-memory-pii`](crates/moa-memory/pii/) | PII classification and privacy filtering for memory writes |
| [`moa-hands`](crates/moa-hands/) | Tool router, local/Docker hands, Daytona, E2B, MCP client |
| [`moa-providers`](crates/moa-providers/) | LLM and embedding providers |
| [`moa-orchestrator`](crates/moa-orchestrator/) | Restate services, virtual objects, workflows, and handler binary |
| [`moa-orchestrator-local`](crates/moa-orchestrator-local/) | Tokio-task local orchestrator for CLI and daemon flows |
| [`moa-gateway`](crates/moa-gateway/) | Telegram, Slack, Discord adapters and platform rendering |
| [`moa-runtime`](crates/moa-runtime/) | Shared runtime bootstrap |
| [`moa-cli`](crates/moa-cli/) | `moa` CLI and daemon commands |
| [`moa-security`](crates/moa-security/) | Credential vault, MCP proxy, policies, prompt-injection controls |
| [`moa-skills`](crates/moa-skills/) | Agent Skills parsing, distillation, improvement, regression suites |
| [`moa-eval`](crates/moa-eval/) | Evaluation harness |
| [`moa-loadtest`](crates/moa-loadtest/) | Load-test harness |

## Documentation

Start with [`docs/README.md`](docs/README.md), then read:

- [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md) for the system model and trait map.
- [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) for Restate session and sub-agent flow.
- [`docs/13-task-segmentation.md`](docs/13-task-segmentation.md) for segments and resolution scoring.
- [`docs/14-multi-tenancy-and-learning.md`](docs/14-multi-tenancy-and-learning.md) for tenants, intents, catalog adoption, and the learning log.

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

Apache-2.0. See [`LICENSE`](LICENSE).

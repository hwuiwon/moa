# MOA

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/hwuiwon/moa)

**MOA is a cloud-first, Rust-based, multi-tenant AI agent operations platform for enterprises.**

MOA runs durable agent sessions on Restate, stores product and audit data in Postgres/Neon with pgvector, segments conversations into discrete tasks, scores task resolution automatically, and feeds those outcomes into tenant-controlled learning. It is built for organizations that need governed agent execution: auditable event logs, isolated tool execution, tenant-owned intent taxonomies, approval flows, lineage, and rollbackable learning.

Local development uses the same Restate-backed orchestrator that production uses. The CLI is a thin client pointed at a Restate ingress endpoint.

Status: early active development. The architecture is stable enough to document, but APIs and product surfaces still move.

## What Matters

- **Enterprise tenancy:** teams own users, workspaces, sessions, memory, skills, intents, learning entries, lineage, and audit evidence.
- **Durable orchestration:** Restate virtual objects own sessions and sub-agents; workflows own one-shot jobs such as memory consolidation and intent discovery.
- **Postgres everywhere:** sessions, events, analytics, task segments, memory indexes, embeddings, intents, and the learning log live in Postgres/Neon.
- **Task-aware sessions:** every session can contain multiple task segments, each with intent metadata, tool and skill usage, cost, and a resolution score.
- **Per-tenant learning:** tenants own their intent taxonomy and learning log. Global catalog intents are opt-in only.
- **Governed execution:** risky actions require approval, secrets stay outside generated code, and hands/MCP tools run through explicit policy.
- **Resolution-weighted skills:** skill ranking uses tenant-level resolution data, not only recency or usage count.
- **Inspectable operation:** every event, learning entry, tool call, approval, and materialized analytics view is queryable.
- **Pluggable hands:** local execution, Docker, Daytona, E2B, and MCP tools all route through the hand/tool abstraction.
- **Model-agnostic providers:** Anthropic, OpenAI, and Google Gemini are first-class provider targets.

## Local Development

`make dev` brings up the full local stack:

- Postgres with AGE, pgvector, and pgaudit on `localhost:10040`
- `moa-edge` on `localhost:10000`
- Restate Server 1.6.2 on `localhost:10010` for ingress and `localhost:10011` for the UI
- `moa-orchestrator` with an internal-only Restate handler port and `localhost:10021` for health
- PII filter on `localhost:10050`
- audit log shipper with no exposed port

The full local port map lives in
[`docs/operations/local-ports.md`](docs/operations/local-ports.md).

The orchestrator registers its handlers with Restate automatically through the
`restate-register` sidecar service.

To wait for everything to be ready:

```bash
make dev-status
```

To open the Restate web UI and inspect invocations, K/V state, and deployments:

```bash
make dev-restate-ui
```

### Authorization (OpenFGA)

`make dev` brings up OpenFGA at <http://localhost:10030> and bootstraps the
schema. Store / model IDs are written to `.env.fga`; source it before running
the orchestrator manually:

```sh
source .env.fga
```

Skip OpenFGA for non-auth work:

```sh
MOA_SKIP_FGA=1 make dev
```

Install the `fga` CLI for schema iteration:

```sh
make fga-install
```

OpenFGA Playground: <http://localhost:10032>.

To stop everything while preserving data:

```bash
make dev-down
```

To stop and wipe all volumes, including Postgres data and Restate state:

```bash
make dev-wipe
```

If `localhost:10010` is already in use, override the Restate ingress port in a
local `compose.override.yml`.

Use the CLI against the local stack:

```bash
MOA__ORCHESTRATOR__ENDPOINT=http://localhost:10010 cargo run -p moa-cli -- exec "What is 2+2?"
```

For a remote orchestrator, set `MOA__ORCHESTRATOR__ENDPOINT` to that Restate
ingress URL or configure `[orchestrator].endpoint` in `~/.moa/config.toml`.

Run the deterministic load-test smoke profile against the local stack:

```bash
make loadtest-mock
```

For a live-provider run against the same Restate ingress:

```bash
make loadtest-live
```

`make loadtest-mock` restarts the orchestrator with
`MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/perf-gate.json`.

## Cloud Runtime

Cloud mode runs the `moa-orchestrator-bin` Restate handler service plus Postgres/Neon and the configured hand provider.

```bash
POSTGRES_URL=postgres://...
RESTATE_ADMIN_URL=http://localhost:10011
OPENAI_API_KEY=...
cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
```

The binary serves these Restate surfaces: `Session`, `SubAgent`, `Workspace`, `SessionStore`, `ToolExecutor`, `LLMGateway`, `WorkspaceStore`, `IntentManager`, `Consolidate`, `IntentDiscovery`, `IngestionVO`, and `Health`. Deployment registration is handled outside the binary.

The Docker image builds `moa-orchestrator-bin` and installs it as `/usr/local/bin/moa-orchestrator`.

Required cloud process configuration includes:

```bash
POSTGRES_URL=postgres://...
RESTATE_ADMIN_URL=http://localhost:10011
OPENAI_API_KEY=...
DAYTONA_API_KEY=... # optional, depending on hand provider
```

## Architecture

```text
REST / Gateway / CLI
        |
        v
Restate handler service (`moa-orchestrator-bin`)
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
  tenant_intents, global_intent_catalog, learning_log,
  lineage, scores, compliance audit tables
```

The context pipeline is byte-stable where possible for prompt caching. With query rewriting enabled, the current processors are: identity, instructions, tools, skills, query rewrite, memory, history, runtime context, compactor, and cache optimizer.

## Memory

Memory is split across four crates under `crates/moa-memory/`:

- `graph/` - Apache AGE adapter, bi-temporal write protocol
- `vector/` - pgvector / Turbopuffer, Gemini and Cohere embedders
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
| [`moa-lineage-core`](crates/moa-lineage/core/) | Lineage records and score record types |
| [`moa-lineage-sink`](crates/moa-lineage/sink/) | Async lineage sink writers |
| [`moa-lineage-otel`](crates/moa-lineage/otel/) | OTel/OpenInference bridge |
| [`moa-lineage-citation`](crates/moa-lineage/citation/) | Citation/provenance adapters |
| [`moa-lineage-cold`](crates/moa-lineage/cold/) | Cold lineage export and partition support |
| [`moa-lineage-audit`](crates/moa-lineage/audit/) | Compliance audit hashes, Merkle roots, signing, DSAR support |
| [`moa-hands`](crates/moa-hands/) | Tool router, local/Docker hands, Daytona, E2B, MCP client |
| [`moa-providers`](crates/moa-providers/) | LLM and embedding providers |
| [`moa-orchestrator`](crates/moa-orchestrator/) | Restate services, virtual objects, workflows, and handler binary |
| [`moa-gateway`](crates/moa-gateway/) | Telegram, Slack, Discord adapters and platform rendering |
| [`moa-runtime`](crates/moa-runtime/) | Thin runtime facade over the orchestrator HTTP client |
| [`moa-cli`](crates/moa-cli/) | Thin-client `moa` CLI and orchestrator diagnostics |
| [`moa-security`](crates/moa-security/) | Credential vault, MCP proxy, policies, prompt-injection controls |
| [`moa-skills`](crates/moa-skills/) | Agent Skills parsing, distillation, improvement, regression suites |
| [`moa-eval`](crates/moa-eval/) | Evaluation harness |
| [`moa-loadtest`](crates/moa-loadtest/) | Load-test harness |
| [`workspace-hack`](crates/workspace-hack/) | Generated `cargo-hakari` crate for dependency feature unification |
| [`xtask`](crates/xtask/) | Repo-local audit and maintenance commands |

## Documentation

Start with [`docs/README.md`](docs/README.md), then read:

- [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md) for the system model and trait map.
- [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) for Restate session and sub-agent flow.
- [`docs/13-task-segmentation.md`](docs/13-task-segmentation.md) for segments and resolution scoring.
- [`docs/14-multi-tenancy-and-learning.md`](docs/14-multi-tenancy-and-learning.md) for tenants, intents, catalog adoption, and the learning log.
- [`architecture.md`](architecture.md) for the current enterprise runtime map.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
```

### Testing

Provider request-body and other byte-stability snapshots follow the pattern in [`docs/testing/snapshot-pattern.md`](docs/testing/snapshot-pattern.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE).

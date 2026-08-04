# MOA

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/hwuiwon/moa)

**MOA is a cloud-first, Rust-based, multi-tenant AI agent operations platform for enterprises.**

MOA runs durable agent sessions on Restate, stores product and audit data in Postgres/Neon with pgvector, segments conversations into discrete tasks, scores task resolution automatically, and feeds those outcomes into tenant-controlled learning. It is built for organizations that need governed agent execution: auditable event logs, isolated tool execution, approval flows, lineage, and rollbackable learning.

Local development uses the same Restate-backed orchestrator and HTTP API surface that production uses.

Status: early active development. The architecture is stable enough to document, but APIs and product surfaces still move.

## What Matters

- **Enterprise tenancy:** tenants own contacts, sessions, memory, skills, learning entries, lineage, policies, and audit evidence.
- **Durable orchestration:** Restate virtual objects own sessions and workers; workflows own one-shot jobs such as memory consolidation.
- **Postgres everywhere:** sessions, events, analytics, task segments, memory indexes, embeddings, and the learning log live in Postgres/Neon.
- **Task-aware sessions:** every session can contain multiple task segments, each with tool and skill usage, cost, and a resolution score.
- **Per-tenant learning:** tenants own learning entries and outcome aggregates that improve skill ranking and memory updates.
- **Governed execution:** risky actions require approval, secrets stay outside generated code, and hands plus reviewed HTTP connector actions run through explicit policy.
- **Resolution-weighted skills:** skill ranking uses tenant-level resolution data, not only recency or usage count.
- **Inspectable operation:** every event, learning entry, tool call, approval, and materialized analytics view is queryable.
- **Pluggable execution:** local hands, Docker, Daytona, E2B, operator MCP tools, and tenant-scoped connector actions share one governed tool boundary.
- **Model-agnostic providers:** Anthropic, OpenAI, and Google Gemini are first-class provider targets.

## Local Development

`make dev` brings up the full local stack:

- Postgres with pgvector and pgaudit on `localhost:10040`
- `moa-edge` on `localhost:10000`
- Restate Server 1.7.2 on `localhost:10010` for ingress and `localhost:10011` for the UI
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

The `moa-fga-bootstrap` binary writes the checked-in OpenFGA JSON model through
the OpenFGA HTTP API; no external conversion tool is required.

OpenFGA Playground: <http://localhost:10032>.

### API keys

For local development, bootstrap a local API identity once, then create or seed
a dev API key through the hosted `ApiKeys` service:

```sh
./scripts/bootstrap-api-identity.sh
curl -X POST http://localhost:10010/ApiKeys/create \
  -H "Content-Type: application/json" \
  -H "x-moa-identity-type: operator" \
  -H "x-moa-identity-id: 00000000-0000-0000-0000-000000000101" \
  -H "x-moa-tenant-id: 00000000-0000-0000-0000-000000000201" \
  --data '{"name":"local","env":"dev","description":null,"for_agent_id":null}'
```

Present the returned key to the edge with `Authorization: Bearer <key>`.
Builtin approvals are resolved through the edge approval endpoints:

```sh
curl -H "Authorization: Bearer <key>" http://localhost:10000/v1/authz-challenges
curl -H "Authorization: Bearer <key>" http://localhost:10000/v1/action-reviews
```

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

Exercise the local stack through HTTP APIs:

```bash
curl -H "Authorization: Bearer <key>" http://localhost:10000/v1/whoami
```

For a remote deployment, call the public `moa-edge` URL with the same bearer
token model.

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
MOA_DATABASE_URL=postgres://runtime-role@... \
MOA_DATABASE_ADMIN_URL=postgres://migration-role@... \
  cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- migrate

MOA_DATABASE_URL=postgres://runtime-role@... \
MOA_RESTATE_ADMIN_URL=http://localhost:10011 \
MOA_OPENAI_API_KEY=...
cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
```

Migration is an explicit deployment phase. Normal runtime startup only
validates the already-complete migration history through `MOA_DATABASE_URL`;
it never applies DDL or reads `MOA_DATABASE_ADMIN_URL`.

The binary serves these Restate surfaces: virtual objects `Session`, `Worker`,
`Tenant`, `CronJob`, and `IngestionVO`; services `AgentDefinitions`, `Agents`,
`AdminMaintenance`, `Artifacts`, `ActionReviews`, `ApiKeys`, `Authz`,
`AuthzChallenges`, `Contacts`, `Experiments`, `GraphMemoryMaint`,
`Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`,
`SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, and `ActionPolicy`; and
workflows `ExecutionRun`, `ExecutionTask`, `KnowledgeSyncIngestion`,
`Consolidate`, `ExperimentRun`, `ExperimentTrialRun`, `TenantPurge`,
`TurnExecution`, and `WorkerTurnExecution`. Builds with `skill-learning` also
register the `SkillLearning` workflow. Deployment registration is handled
outside the binary.

The Docker image builds `moa-orchestrator-bin` and installs it as `/usr/local/bin/moa-orchestrator`.

Required cloud process configuration includes:

```bash
MOA_DATABASE_URL=postgres://runtime-role@...
MOA_RESTATE_ADMIN_URL=http://localhost:10011
MOA_OPENAI_API_KEY=...
DAYTONA_API_KEY=... # optional, depending on hand provider
```

## Architecture

```text
REST / Messaging / API automation
        |
        v
Restate handler service (`moa-orchestrator-bin`)
        |
        +-- Session VO -> TurnExecution workflow -> context pipeline -> LLMGateway
        +-- TurnExecution run mode -> ExecutionRun -> ExecutionTask
        +-- Worker VO -> explicit conversational child tools and terminal reporting
        +-- ToolExecutor -> ToolRouter -> hands / built-ins / operator MCP / connector actions
        +-- ConnectorConnections -> lifecycle / credentials / HTTP action bindings
        +-- Consolidate workflow -> memory compaction
        +-- IngestionVO -> graph memory updates
        |
        v
Postgres / Neon
  sessions, events, task_segments, analytics views,
  graph memory, sidecar indexes, pgvector embeddings,
  learning_log,
  lineage, scores, compliance audit tables
```

`TurnExecution` durably owns one admitted session turn and selects `respond`,
`act`, or `run`. `act` retains the explicit bounded conversational Worker tool
surface and ordinary per-worker terminal delivery. `run` compiles or
instantiates an immutable plan, starts `ExecutionRun` detached, and executes
stable logical tasks through `ExecutionTask`.

The context pipeline is byte-stable where possible for prompt caching. With
query rewriting and memory digests enabled, the current execution order is:
identity, agent instructions, instructions, tools, query rewrite, history,
skills, digest, memory, and runtime context. History owns replay and
compaction; the per-turn dynamic skill, digest, and memory sections follow it
to preserve the reusable prompt prefix.

## Memory

Memory is split across six crates under `crates/moa-memory/`:

- `graph/` - relational Postgres graph store, bi-temporal write protocol
- `vector/` - pgvector / Turbopuffer, Gemini and Cohere embedders
- `pii/` - redaction at ingestion via openai/privacy-filter HTTP service
- `ingest/` - slow-path Restate VO, fast-path API, contradiction detector
- `lifecycle/` - memory consolidation, quality scoring, and digest generation
- `types/` - shared memory domain types

See `docs/15-architecture-policy.md` for how types are owned across these
crates and `crates/moa-memory/README.md` for crate-level details.

## Workspace Layout

| Crate | Role |
|---|---|
| [`moa-core`](crates/moa-core/) | Shared types, traits, events, and errors used across all crates |
| [`moa-config`](crates/moa-config/) | Runtime configuration tree (`MoaConfig`) with flat env overlay |
| [`moa-wire`](crates/moa-wire/) | Wire DTOs for edge and internal service boundaries |
| [`moa-brain`](crates/moa-brain/) | Context pipeline, query rewriting, segment helpers, resolution scoring, streamed turns |
| [`moa-retrieval`](crates/moa-retrieval/) | Graph-memory retrieval engine and query planning |
| [`moa-execution`](crates/moa-execution/) | Pure compiler, interpreter, and persistence for durable execution runs |
| [`moa-db`](crates/moa-db/) | Database helpers shared by MOA storage crates (pools, scoped connections, RLS) |
| [`moa-crypto`](crates/moa-crypto/) | Envelope encryption, BYOK KMS seam, crypto-shred erasure |
| [`moa-session`](crates/moa-session/) | Postgres session store, event log, task segments, learning log, analytics views |
| [`moa-analytics`](crates/moa-analytics/) | Query catalog and read models for safe analytics API queries |
| [`moa-analytics-export`](crates/moa-analytics-export/) | Background Postgres-to-ClickHouse analytics exporter with single-writer lease |
| [`moa-runtime-store`](crates/moa-runtime-store/) | Runtime cache store implementations (in-memory and Redis/Valkey) |
| [`moa-migrations`](crates/moa-migrations/) | Central Postgres migrations and schema runners |
| [`moa-memory-graph`](crates/moa-memory/graph/) | Graph memory store, SQL sidecars, RLS, bitemporal state, and changelog |
| [`moa-memory-ingest`](crates/moa-memory/ingest/) | Slow-path graph ingestion and fast memory write APIs |
| [`moa-memory-vector`](crates/moa-memory/vector/) | pgvector-backed graph embeddings and vector lookup |
| [`moa-memory-pii`](crates/moa-memory/pii/) | PII classification and privacy filtering for memory writes |
| [`moa-memory-lifecycle`](crates/moa-memory/lifecycle/) | Memory lifecycle jobs for consolidation, promotion, and quality scoring |
| [`moa-memory-types`](crates/moa-memory/types/) | Shared memory domain types across the memory subcrates |
| [`moa-knowledge`](crates/moa-knowledge/) | Tenant knowledge-base domain, providers, parsers, and ingestion seams |
| [`moa-connectors`](crates/moa-connectors/) | Tenant connector lifecycle, reviewed HTTP action bindings, constrained HTTP runtime, and invocation ledgers |
| [`moa-lineage-core`](crates/moa-lineage/core/) | Lineage records and score record types |
| [`moa-lineage-citation`](crates/moa-lineage/citation/) | Provider citation normalization and BM25/NLI verification helpers |
| [`moa-lineage-sink`](crates/moa-lineage/sink/) | Async lineage sink writers plus the OTel/OpenInference span bridge |
| [`moa-lineage-audit`](crates/moa-lineage/audit/) | Compliance audit hashes, Merkle roots, signing, DSAR support |
| [`moa-observability`](crates/moa-observability/) | Runtime metrics, tracing bootstrap, and Restate observability helpers |
| [`moa-authz-schema`](crates/moa-auth/authz-schema/) | Typed OpenFGA object, relation, and tuple-key constants |
| [`moa-authz`](crates/moa-auth/authz/) | OpenFGA authorization checks, tuple outbox, and delegated access helpers |
| [`moa-auth-providers`](crates/moa-auth/providers/) | Local API keys, disabled auth, tenant credential storage, provider bundle construction, and feature-gated Auth0/OIDC identity and CIBA approvals |
| [`moa-fga-bootstrap`](crates/moa-auth/fga-bootstrap/) | OpenFGA store and authorization model bootstrap binary |
| [`moa-ocsf`](crates/moa-ocsf/) | OCSF security event types, signing, and persistence helpers |
| [`moa-edge`](crates/moa-edge/) | Hosted HTTP edge service and public API routing |
| [`moa-hands`](crates/moa-hands/) | Tool router, local/Docker hands, Daytona, E2B, MCP client |
| [`moa-providers`](crates/moa-providers/) | LLM, embedding, and rerank providers plus provider-egress DLP governance |
| [`moa-orchestrator`](crates/moa-orchestrator/) | Restate services, virtual objects, workflows, and handler binary |
| [`moa-agents`](crates/moa-agents/) | Tenant-configurable agent resolution and runtime policy locking |
| [`moa-contacts`](crates/moa-contacts/) | Contact identity domain and persistence helpers |
| [`moa-artifacts`](crates/moa-artifacts/) | Canonical artifact definitions for agents, skills, connectors, actions, and experiment plans |
| [`moa-experiments`](crates/moa-experiments/) | Domain types for experiment runs and scorecard configuration |
| [`moa-messaging`](crates/moa-messaging/) | Slack adapter, platform rendering, Postmark email connector, and Twilio SMS connector |
| [`moa-security`](crates/moa-security/) | Credential vault, MCP proxy, policies, prompt-injection controls |
| [`moa-kms`](crates/moa-kms/) | Postgres-backed KMS for envelope encryption and crypto-shred |
| [`moa-skills`](crates/moa-skills/) | Agent Skills parsing, distillation, improvement, regression suites |
| [`moa-eval-core`](crates/moa-eval/core/) | Shared evaluation engine types and scoring primitives |
| [`moa-eval`](crates/moa-eval/) | Evaluation harness |
| [`moa-loadtest`](crates/moa-loadtest/) | Direct HTTP load-test harness for hosted orchestrator APIs |
| [`moa-test-support`](crates/moa-test-support/) | Shared integration-test fixtures, Postgres helpers, and contract checks |
| [`workspace-hack`](crates/workspace-hack/) | Generated `cargo-hakari` crate for dependency feature unification |
| [`xtask`](crates/xtask/) | Repo-local audit and maintenance commands |

## Documentation

Start with [`docs/README.md`](docs/README.md), then read:

- [`docs/01-architecture-overview.md`](docs/01-architecture-overview.md) for the system model and trait map.
- [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md) for Restate session and worker flow.
- [`docs/13-task-segmentation.md`](docs/13-task-segmentation.md) for segments and resolution scoring.
- [`docs/14-multi-tenancy-and-learning.md`](docs/14-multi-tenancy-and-learning.md) for tenants, skills-first learning, rollback, and the learning log.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) for the current enterprise runtime map.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
```

### Testing

Provider request-body and other byte-stability snapshots follow the pattern in [`docs/20-testing.md`](docs/20-testing.md).

## License

Apache-2.0. See [`LICENSE`](LICENSE).

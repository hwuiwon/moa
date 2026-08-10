# MOA — Agent Instructions

MOA is a cloud-first general-purpose AI agent platform written in Rust.

## Source Of Truth

Read the relevant architecture doc before changing a subsystem. Trait
definitions in `docs/01-architecture-overview.md` are the interface source of
truth.

| File | Covers |
|---|---|
| `docs/00-direction.md` | Product identity and philosophy |
| `docs/01-architecture-overview.md` | System model, trait map, workspace layout |
| `docs/02-brain-orchestration.md` | Restate orchestration and brain loop |
| `docs/03-communication-layer.md` | Messaging/API communication, approvals, observation |
| `docs/04-memory-architecture.md` | Graph memory, privacy, retrieval, consolidation |
| `docs/05-session-event-log.md` | Postgres event schema and compaction |
| `docs/06-hands-and-mcp.md` | HandProvider, sandboxes, MCP, tool routing |
| `docs/07-context-pipeline.md` | Context compilation and cache optimization |
| `docs/08-security.md` | Credential vault, sandbox, prompt injection |
| `docs/09-skills-and-learning.md` | Agent Skills format and distillation |
| `docs/10-technology-stack.md` | Crates, build targets, deployment |
| `docs/24-connectors-and-connections.md` | Connector definitions, connections, actions, sources, credentials, and rollout |
| `docs/25-sandbox-workspaces.md` | Durable sandbox workspace ownership, lifecycle, checkpoints, and provider storage |

## Workspace Crates

Crates live under `crates/`. If a prompt references `<name>/...` for a workspace
crate, translate it to `crates/<name>/...`.

| Area | Crates |
|---|---|
| Core runtime | `moa-core` traits/types/config/events, `moa-brain` context pipeline and execution planning, `moa-execution` execution-plan compiler/interpreter/repository, `moa-db` shared storage helpers, `moa-session` Postgres event store, `moa-orchestrator` Restate services/workflows, `moa-runtime-store` runtime cache, `moa-edge` public HTTP edge, `moa-migrations` Postgres migrations |
| Memory and learning | `moa-memory-*` graph/ingest/lifecycle/PII/types/vector, `moa-knowledge` tenant knowledge base, `moa-skills` registry/distillation/improvement |
| Agents, artifacts, experiments | `moa-agents` agent resolution/policy, `moa-contacts` contact identity, `moa-artifacts` artifact definitions and optional skill execution-plan templates, `moa-experiments` experiment runs and score storage |
| Connectors, tools, and providers | `moa-connectors` connection lifecycle/action/source bindings and ledgers, `moa-hands` tool routing plus sandbox-workspace lifecycle/repositories/provider adapters, `moa-providers` LLM/embedding/rerank providers, `moa-messaging` Slack and notification adapters, `moa-security` destination/vault policy and MCP proxy |
| Auth, audit, lineage, observability | `moa-auth/*` identity/authz/OpenFGA bootstrap, `moa-ocsf` security events, `moa-lineage/*` citation, sinks, OTel, audit chain, `moa-observability` metrics/tracing |
| Eval and dev tooling | `moa-eval-core`, `moa-eval`, `moa-loadtest`, `moa-test-support`, `xtask`, `workspace-hack` |

## Rust Rules

1. Every public function and every module needs a doc comment.
2. Library errors use `thiserror`; binaries may use `anyhow`.
3. Use `tracing` for logging; never `println!`/`eprintln!` in library code.
4. Use `tokio`; all I/O must be async.
5. No `unwrap()` in library code. Use `?` or explicit handling.
6. Optional dependencies are controlled by feature flags such as `slack`
   and `experiments`.
7. Prefer direct imports from the owning crate/module. Do not add compatibility
   shims, wrapper functions, or `pub use` re-exports just to preserve old paths.

Conventions: IDs are `uuid::Uuid` newtypes, timestamps are
`chrono::DateTime<Utc>`, config is TOML via `config`, dynamic payloads are
`serde_json::Value`, paths are `PathBuf` for filesystem paths, and library
crates should expose one `Error` enum.

## Authorization

Every handler that touches caller-owned data must either:

1. Call `moa_authz::require_authz` or `require_authz_with_delegation` with the
   right `(ObjectType, object_id, Relation)` before protected reads; or
2. Carry a one-line `// SAFETY: ...` comment immediately above the handler
   signature. Acceptable reasons: purely informational, health/observability, or
   already checked by the caller.

Common failures: reading a resource before authz, using non-delegated authz for
agent writes, or deleting a resource without enqueueing the inverse OpenFGA
outbox tuple.

## Testing

- Locations: unit tests inline in `#[cfg(test)]`; integration tests under each
  crate's `tests/` directory. Offline/`_db`/`_db_memory` behavior files live as
  modules inside one per-lane harness binary per crate (e.g.
  `tests/orchestrator_db.rs` with `#[path = "orchestrator_db/foo_db.rs"] mod
  foo_db;`) — add new files to the matching harness instead of creating a new
  root file; e2e/live/eval binaries and names pinned by nextest profiles or
  scripts stay standalone.
- Names: test files/functions should say what behavior broke. Use lane suffixes
  like `_offline`, `_db`, `_db_memory`, `_service_e2e`, `_provider_e2e`, `_eval`,
  `_live`, or `_docker` when a runtime requirement matters.
- Parallelism: write tests so `cargo nextest` can run them concurrently. Use
  per-test IDs, temp dirs, schemas/databases, ports, mock servers, and fixtures.
  If serialization is unavoidable, document the shared resource and isolate it
  with the narrowest nextest group.
- Live/billed tests never run by default. They require both `#[ignore = "..."]`
  and an explicit env flag such as `MOA_RUN_LIVE_COHERE_TESTS=1`; if the flag is
  set but credentials are missing, fail clearly. Never write secrets to
  fixtures, git-tracked files, or shell command text.
- Value bar: every test must exercise a real production path, assert exact
  observable behavior, avoid private implementation coupling, and avoid
  duplicating a stronger eval or integration test. Delete tautological tests
  instead of weakening them.
- New-test workflow: write a `// Pins:` scenario comment, choose the name/lane,
  design for concurrency, write the assertion first, drive the real code path,
  and mutation-check substantial tests by briefly breaking the implementation.

## Verification

For Rust changes:

```bash
cargo fmt --all
# run focused crate/test targets for the changed surface
cargo clippy ... -D warnings
cargo build --workspace # when public types, shared crates, or workspace wiring changed
git diff --check
```

## Local Docker Compose Stack

Bring the MOA compose stack up only when a task needs local Postgres, Restate,
OpenFGA, edge, opt-in PII, or loadtest services. Check state with
`docker compose ps`. When no longer needed, stop it with `docker compose down`
to preserve volumes. Use `docker compose down -v` or `make dev-wipe` only for an
explicit reset.

## CodeGraph

For codebase questions, first use `codegraph_explore` when a `.codegraph/`
index exists. If MCP tools are not available, use
`./scripts/codegraph explore "<question>"`. For focused local checks, use
`./scripts/codegraph node`, `query`, `callers`, `callees`, or `impact`.

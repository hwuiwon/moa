# Cloud CLI Boundary Findings

## Initial Direction

- User wants a long-term plan for whether CLI support is needed in a cloud-hosted MOA.
- Agreed architecture direction: keep the CLI only as a thin cloud/client/control-plane tool.
- Avoid CLI-owned session orchestration, sandbox lifecycle, filesystem generation, memory retrieval, authz bypasses, or approval semantics.

## Discovery Notes

- No prior `task_plan.md`, `findings.md`, or `progress.md` existed in the project root.
- `graphify-out/GRAPH_REPORT.md` exists and must be consulted before further broad exploration.
- Graph report identifies `OrchestratorClient` as the highest-connected abstraction and has specific communities for CLI entry points, CLI command tests, API key service, LLM gateway, tool executor, session replay, and TurnExecution.
- `docs/00-direction.md` defines MOA as cloud-first and says CLI, REST/gateway, and messaging adapters are peers over the same runtime model.
- `docs/01-architecture-overview.md` says `moa-cli` and `moa-runtime` call configured Restate ingress through `moa-orchestrator-client`; client paths do not embed an orchestrator process.
- `docs/02-brain-orchestration.md` says thin clients call the Restate-backed orchestrator over the same API used by `make dev`.
- `docs/03-communication-layer.md` says the CLI no longer embeds or serves an in-process daemon and reads orchestrator endpoint/auth settings from config.
- `docs/03-communication-layer.md` also says durable state is not stored in the client; clients reconnect by replaying Postgres events and querying Restate status.
- `crates/moa-runtime/src/lib.rs` is already a thin facade over `moa-orchestrator-client`; it has compatibility-named constructors like `from_daemon_config`, `attach_to_daemon_session`, and `attach_to_local_session` that should be renamed or removed if no legacy wrappers are desired.
- `crates/moa-cli/src/exec.rs` uses `moa_runtime::ChatRuntime`; despite a stale comment saying "local chat runtime", this is already mostly cloud-client aligned.
- `crates/moa-cli/src/daemon/mod.rs` is endpoint/health-client logic, not an in-process daemon, but the `daemon` naming is legacy and should be removed or renamed.
- Several CLI paths still open Postgres/session stores directly instead of using cloud APIs, including `analytics.rs`, `memory.rs`, `lineage.rs`, `eval.rs`, `commands/admin.rs`, `commands/skills.rs`, and `commands/privacy/*`.
- `crates/moa-cli/src/support.rs` centralizes direct `create_session_store` and graph/ingestion helpers, making it the key seam for removing CLI-owned data/runtime paths.
- `crates/moa-orchestrator-client/src/client.rs` already covers thin-client APIs for session create/init/list/get/events, session VO handles, workspace cost, tool names, API keys, approvals, agent templates/agents, authz tuple writes, tenant audit keys/destinations, audit verify, whoami, and health.
- `crates/moa-cli/Cargo.toml` still depends directly on heavy server-side crates: `moa-eval`, `moa-brain`, `moa-lineage-*`, `moa-memory-*`, `moa-session`, and `moa-skills`. A cloud-thin CLI target should remove most or all of these from normal CLI dependencies after corresponding cloud APIs exist.
- `crates/moa-runtime/Cargo.toml` is already lightweight: `moa-core`, `moa-orchestrator-client`, `tokio`, `chrono`, `uuid`.
- CLI commands already cloud-client aligned: `exec` via `moa-runtime`, `sessions` and basic status via `OrchestratorClient`, auth key management, approvals, agents, authz tuple writes, tenant audit settings, and audit verify.
- CLI commands still server-side/direct-data: `session stats`, `workspace stats`, `tool stats`, `cache stats`, `memory search/show/ingest`, `retrieve --debug`, `explain`, `lineage query/export/verify/erase`, `skills import/export/list/bootstrap`, `privacy export/erase`, `promote-workspace`/rollback/finalize, `checkpoint`, and most `eval` subcommands.
- `memory.rs` performs graph lookup and hybrid retrieval locally, and memory ingest calls `ingest_turn_direct` through a CLI wrapper rather than an orchestrator service.
- `lineage.rs` issues raw SQL against `analytics.turn_lineage` and PII vault tables and writes DSAR bundles locally.
- `eval.rs` constructs and runs `EvalEngine` in the CLI process, then writes/replays scores through direct DB access.
- `commands/admin.rs` performs pgvector to Turbopuffer promotion directly from the CLI process.
- `checkpoint.rs` calls `NeonBranchManager` directly and mutates local config on rollback.
- Orchestrator currently binds `Health`, `SessionStore`, `LLMGateway`, `AgentRegistry`, `AgentTemplates`, `Agents`, `Approvals`, `ApiKeys`, `Audit`, `Authz`, `IngestionVO`, `ToolExecutor`, `WorkspaceStore`, `GraphMemoryMaint`, `NeonMaint`, `CronJob`, `Session`, `SubAgent`, `Tenants`, `Workspace`, `Whoami`, `Consolidate`, and `TurnExecution`.
- Existing orchestrator services do not provide full public/admin APIs for CLI analytics summaries, memory search/show/user-facing ingest, lineage query/export/verify/erase, privacy export/erase, skill import/export/list/bootstrap, eval run/replay/scores/compare, vector promotion, or checkpoint management.
- `IngestionVO` exists and can ingest a `SessionTurn`, but the current CLI `memory ingest` builds and calls ingestion locally rather than using the service.
- `NeonMaint` exists only for pruning expired checkpoint branches, not for user-facing checkpoint create/list/rollback/cleanup operations.
- Existing `moa-orchestrator-client` tests use `mockito` to pin exact HTTP paths, headers, and response decoding. This is the right pattern for every new client method.
- Existing `moa-cli/src/tests.rs` includes direct helper tests for config parsing, doctor output, eval exit codes, and memory ingest/search/show. These tests will need to be split: pure formatting/parsing tests stay in CLI, while DB-backed behavior should move to orchestrator/client tests.
- Existing CLI tests still set `config.daemon.*`, showing the legacy daemon config shape leaks into tests even though docs say the CLI no longer embeds a daemon.
- `MoaConfig` still contains `daemon: DaemonConfig`; loader defaults still set `daemon.socket_path`, `daemon.pid_file`, `daemon.log_file`, and `daemon.auto_connect`.
- `crates/moa-core/src/daemon.rs` still exposes `DaemonInfo`/`DaemonSessionPreview` types. These are legacy if the CLI/runtime no longer has a daemon concept.
- `crates/moa-cli/src/daemon/mod.rs` is functionally an orchestrator endpoint helper, and `daemon_cmd.rs` only renders orchestrator endpoint status. This should be renamed to an orchestrator/status surface and the `Daemon` CLI subcommand removed or renamed.
- Relevant likely surfaces discovered so far:
  - `crates/moa-cli/`
  - `crates/moa-orchestrator-client/`
  - `crates/moa-edge/`
  - `crates/moa-gateway/`
  - `crates/moa-core/src/config/`
  - `docs/00-direction.md`
  - `docs/01-architecture-overview.md`
  - `docs/02-brain-orchestration.md`
  - `docs/03-communication-layer.md`
  - `docs/08-security.md`
  - `docs/10-technology-stack.md`

## Open Questions To Resolve In Plan

- Which existing CLI commands are cloud-client control-plane commands and should remain?
- Which CLI commands currently execute local runtime/session behavior and should be removed or rewritten to call cloud APIs?
- Does `moa-orchestrator-client` already cover the API surface the CLI should use?
- What tests should pin that CLI commands call API clients instead of local runtime paths?

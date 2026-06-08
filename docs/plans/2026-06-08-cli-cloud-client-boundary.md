# CLI Cloud Client Boundary Implementation Plan

> **Worker note:** Execute this plan task-by-task using the run-plan skill or subagents. Each step uses checkbox (`- [ ]`) syntax for progress tracking.

**Goal:** Make `moa` a thin cloud/control-plane CLI whose normal commands call hosted MOA APIs instead of running session, memory, lineage, eval, sandbox, or database logic in the CLI process.

**Architecture:** `moa-cli` should parse args, load credentials/config, call `moa-orchestrator-client`, render responses, and return correct exit codes. All product data reads/writes, memory retrieval/ingestion, lineage/DSAR operations, eval execution, vector promotion, checkpoint management, approvals, authz, tool execution, sandbox/filesystem setup, and session orchestration live in `moa-orchestrator` or `moa-edge`. The implementation is a hard break: remove legacy daemon/local wrappers instead of preserving compatibility aliases.

**Tech Stack:** Rust, Restate services/workflows, `moa-orchestrator-client` over HTTP, `clap`, `reqwest`, Postgres via server-side `moa-session`, graph memory/vector crates inside orchestrator only, existing `mockito` client tests, existing Restate integration tests.

**Work Scope:**
- **In scope:** Remove daemon/local runtime naming from config and CLI, add missing orchestrator APIs, add client methods, rewrite CLI commands to call those APIs, remove direct CLI dependencies on server-side crates, update docs and tests.
- **Out of scope:** Removing the CLI binary, changing provider behavior, changing the gateway/messaging adapters, changing cloud auth provider semantics, preserving old `[daemon]` config keys, or keeping compatibility constructors whose only purpose is old local/daemon naming.

**Verification Strategy:**
- **Level:** integration plus e2e
- **Command:** `cargo fmt --all && cargo test -p moa-orchestrator-client --tests && cargo test -p moa-cli --tests && cargo test -p moa-orchestrator --tests --features integration -- --test-threads=1 && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings && cargo build --workspace && git diff --check`
- **What it validates:** New client methods hit the expected API paths, CLI rendering and exits still work, server-side handlers compile and pass deterministic tests, workspace-wide public type changes are valid, and formatting/diff hygiene is clean.

---

## Current Findings

- Docs already say `moa-cli` and `moa-runtime` are thin clients over `moa-orchestrator-client`; they should not embed an orchestrator process.
- `moa-runtime` is already mostly thin, but exposes compatibility names: `from_local_config`, `from_daemon_config`, `attach_to_daemon_session`, and `attach_to_local_session`.
- `moa-cli/src/daemon/mod.rs` is actually an orchestrator endpoint helper; `daemon_cmd.rs` only renders endpoint health.
- `MoaConfig` still includes `daemon: DaemonConfig`, and config loader defaults still populate `daemon.socket_path`, `daemon.pid_file`, `daemon.log_file`, and `daemon.auto_connect`.
- `moa-cli` still directly depends on server-side crates: `moa-session`, `moa-brain`, `moa-memory-*`, `moa-lineage-*`, `moa-eval`, and `moa-skills`.
- Already cloud-client aligned: `exec`, basic `sessions`, auth keys, approvals, agents, authz tuple writes, tenant audit settings, audit verify, and health/status plumbing.
- Still direct/server-side in the CLI: analytics reports, memory search/show/ingest/retrieve-debug, lineage query/export/verify/erase, privacy export/erase, skills import/export/list/bootstrap, eval run/replay/scores/compare, vector promotion, checkpoint operations, and parts of doctor.

---

## Task 1: Remove Daemon/Local Runtime Naming

**Dependencies:** None

**Files:**
- Delete: `crates/moa-cli/src/daemon/mod.rs`
- Delete: `crates/moa-cli/src/daemon_cmd.rs`
- Delete: `crates/moa-core/src/daemon.rs`
- Modify: `crates/moa-cli/src/main.rs`
- Modify: `crates/moa-cli/src/cli.rs`
- Modify: `crates/moa-cli/src/dispatch.rs`
- Modify: `crates/moa-cli/src/analytics.rs`
- Modify: `crates/moa-cli/src/doctor.rs`
- Modify: `crates/moa-cli/src/support.rs`
- Modify: `crates/moa-cli/src/tests.rs`
- Modify: `crates/moa-runtime/src/lib.rs`
- Modify: `crates/moa-runtime/tests/client_runtime.rs`
- Modify: `crates/moa-core/src/config/mod.rs`
- Modify: `crates/moa-core/src/config/loader.rs`
- Modify: `crates/moa-core/src/config/session.rs`
- Modify: `crates/moa-core/src/lib.rs`
- Create: `crates/moa-cli/src/orchestrator.rs`

**Acceptance Criteria:**
- [ ] No production code references `config.daemon`.
- [ ] No production code exports or imports `DaemonConfig`, `DaemonInfo`, or `DaemonSessionPreview`.
- [ ] `moa daemon status` is removed; `moa status` remains the supported endpoint/status command.
- [ ] `moa-runtime` exposes only cloud/orchestrator names: `from_config`, `from_endpoint`, and `attach_to_session`.
- [ ] `rg -n "DaemonConfig|config\\.daemon|from_daemon|attach_to_daemon|attach_to_local|from_local_config|daemon\\." crates docs` returns no production references.

- [ ] **Step 1: Move endpoint helper into an orchestrator module.**

Use `apply_patch` to move the contents of `crates/moa-cli/src/daemon/mod.rs` into `crates/moa-cli/src/orchestrator.rs`, update module imports in `main.rs`, `analytics.rs`, `doctor.rs`, and `daemon_cmd.rs` callers, then delete `daemon/mod.rs`.

Expected: `moa-cli` code calls `orchestrator::orchestrator_endpoint`, `orchestrator::orchestrator_health_url`, `orchestrator::health_check`, and `orchestrator::build_client`.

- [ ] **Step 2: Remove the daemon CLI subcommand.**

Edit `crates/moa-cli/src/cli.rs` and `crates/moa-cli/src/dispatch.rs` to remove `CommandKind::Daemon` and `DaemonCommand`. Delete `crates/moa-cli/src/daemon_cmd.rs`.

Expected: `moa status` is the only endpoint status command.

- [ ] **Step 3: Remove daemon config.**

Edit `crates/moa-core/src/config/session.rs` to delete `DaemonConfig`. Edit `crates/moa-core/src/config/mod.rs`, `crates/moa-core/src/config/loader.rs`, and `crates/moa-core/src/lib.rs` to remove daemon exports/defaults/fields.

Expected: TOML serialization no longer emits `[daemon]`.

- [ ] **Step 4: Remove compatibility runtime constructors.**

Edit `crates/moa-runtime/src/lib.rs` to remove `from_local_config`, `from_daemon_config`, `attach_to_daemon_session`, and `attach_to_local_session`; add `attach_to_session(config, platform, session_id)` as the single explicit attach constructor.

Expected: runtime public API uses orchestrator/cloud names only.

- [ ] **Step 5: Run focused verification.**

```bash
cargo test -p moa-core --lib
cargo test -p moa-runtime --tests
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: all focused tests pass after test fixtures stop setting `config.daemon.*`.

---

## Task 2: Add Shared Wire DTOs For CLI Cloud APIs

**Dependencies:** Task 1

**Files:**
- Modify: `crates/moa-core/src/wire.rs`
- Modify: `crates/moa-core/src/lib.rs`

**Acceptance Criteria:**
- [ ] All new request/response structs are defined in `moa-core::wire`.
- [ ] Every public struct and public field has a doc comment.
- [ ] DTOs use typed IDs where available: `SessionId`, `WorkspaceId`, `UserId`, `Uuid`, `DateTime<Utc>`.
- [ ] DTOs are shared by orchestrator handlers, orchestrator-client methods, and CLI command renderers.

- [ ] **Step 1: Add analytics DTOs.**

Edit `crates/moa-core/src/wire.rs` and add:
- `SessionStatsRequest`
- `SessionStatsResponse`
- `WorkspaceStatsRequest`
- `WorkspaceStatsResponse`
- `ToolStatsRequest`
- `ToolStatsResponse`
- `ToolStatsRow`
- `CacheStatsRequest`
- `CacheStatsResponse`
- `CacheDailyMetricRow`

Expected: these structs map the fields currently rendered in `crates/moa-cli/src/analytics.rs`.

- [ ] **Step 2: Add memory DTOs.**

Add:
- `MemorySearchRequest`
- `MemorySearchResponse`
- `MemoryHit`
- `MemoryShowRequest`
- `MemoryShowResponse`
- `MemoryNeighbor`
- `MemoryIngestDocument`
- `MemoryIngestRequest`
- `MemoryIngestResponse`
- `MemoryIngestResult`
- `MemoryRetrieveDebugRequest`
- `MemoryRetrieveDebugResponse`

Expected: these structs map current `memory search`, `memory show`, `memory ingest`, and `retrieve --debug` output.

- [ ] **Step 3: Add lineage/privacy DTOs.**

Add:
- `LineageExplainRequest`
- `LineageExplainResponse`
- `LineageRecordView`
- `LineageQueryRequest`
- `LineageQueryResponse`
- `LineageExportRequest`
- `LineageExportResponse`
- `LineageVerifyRequest`
- `LineageVerifyResponse`
- `LineageEraseRequest`
- `LineageEraseResponse`
- `PrivacyExportRequest`
- `PrivacyExportResponse`
- `PrivacyEraseRequest`
- `PrivacyEraseResponse`

Expected: these structs carry the current CLI fields without exposing raw database connections.

- [ ] **Step 4: Add skills/eval/admin DTOs.**

Add:
- `SkillExportRequest`
- `SkillExportResponse`
- `SkillImportDocument`
- `SkillImportRequest`
- `SkillImportResponse`
- `SkillListRequest`
- `SkillListResponse`
- `SkillSummary`
- `SkillBootstrapGlobalRequest`
- `SkillBootstrapGlobalResponse`
- `EvalPlanRequest`
- `EvalPlanResponse`
- `EvalRunRequest`
- `EvalRunResponse`
- `EvalDatasetRegisterRequest`
- `EvalDatasetRegisterResponse`
- `EvalDatasetListResponse`
- `EvalReplayRequest`
- `EvalReplayResponse`
- `EvalScoresRequest`
- `EvalScoresResponse`
- `EvalCompareRequest`
- `EvalCompareResponse`
- `VectorPromoteRequest`
- `VectorPromotionResponse`
- `VectorPromotionUpdateRequest`
- `CheckpointCreateRequest`
- `CheckpointCreateResponse`
- `CheckpointListResponse`
- `CheckpointRollbackRequest`
- `CheckpointRollbackResponse`
- `CheckpointCleanupResponse`

Expected: these names compile and are available to orchestrator/client/CLI without importing server-side crates into `moa-cli`.

- [ ] **Step 5: Run focused verification.**

```bash
cargo test -p moa-core --lib
```

Expected: `moa-core` compiles with all new DTO doc comments and serde derives.

---

## Task 3: Add Analytics Cloud Service And Client Methods

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/analytics.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/analytics.rs`
- Modify: `crates/moa-cli/src/doctor.rs`
- Modify: `crates/moa-cli/src/support.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/analytics.rs`
- Test: `crates/moa-cli/src/tests.rs`

**Acceptance Criteria:**
- [ ] CLI analytics commands no longer call `load_session_store`.
- [ ] Analytics handler checks authorization before reading protected workspace/session analytics.
- [ ] Client tests pin paths: `/Analytics/session_stats`, `/Analytics/workspace_stats`, `/Analytics/tool_stats`, `/Analytics/cache_stats`.
- [ ] `moa-cli` renders analytics from response DTOs only.

- [ ] **Step 1: Add Restate service.**

Create `analytics.rs` with `#[restate_sdk::service] pub trait Analytics` methods:
- `session_stats(Json<SessionStatsRequest>) -> Json<SessionStatsResponse>`
- `workspace_stats(Json<WorkspaceStatsRequest>) -> Json<WorkspaceStatsResponse>`
- `tool_stats(Json<ToolStatsRequest>) -> Json<ToolStatsResponse>`
- `cache_stats(Json<CacheStatsRequest>) -> Json<CacheStatsResponse>`

Use `SessionStore` methods currently called in `crates/moa-cli/src/analytics.rs`. For `session_stats`, call `moa_authz::require_authz` for `ObjectType::Session` and `Relation::Participant` before reading the session summary. For workspace-level reports, call `require_authz` for `ObjectType::Workspace` and `Relation::Member` before refreshing/read queries.

Expected: no handler reads protected data before authz.

- [ ] **Step 2: Bind the service.**

Update `services/mod.rs`, imports in `main.rs`, `EXPECTED_SERVICE_NAMES`, and the Restate endpoint binding chain.

Expected: Restate registration includes `Analytics`.

- [ ] **Step 3: Add client methods.**

Add `session_stats`, `workspace_stats`, `tool_stats`, and `cache_stats` to `OrchestratorClient`.

Expected: methods call the exact `/Analytics/*` paths.

- [ ] **Step 4: Rewrite CLI analytics rendering.**

Edit `analytics.rs` so report functions call `orchestrator::build_client(config)?` and render DTO responses. Remove analytics use of `load_session_store`.

Expected: `rg -n "load_session_store\\(config\\)" crates/moa-cli/src/analytics.rs crates/moa-cli/src/doctor.rs` shows no analytics direct DB calls.

- [ ] **Step 5: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test analytics -- --test-threads=1
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: all new analytics tests pass.

---

## Task 4: Add Memory Cloud Service And Client Methods

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/memory.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/memory.rs`
- Modify: `crates/moa-cli/src/support.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/memory_service.rs`
- Test: `crates/moa-cli/src/tests.rs`

**Acceptance Criteria:**
- [ ] CLI memory commands no longer import or call `AgeGraphStore`, `HybridRetriever`, `PgvectorStore`, `MpscSink`, or `ingest_turn_direct`.
- [ ] Memory service performs workspace authz before graph/vector reads or writes.
- [ ] `memory ingest` sends document contents to the orchestrator and receives an ingest report; the CLI does not construct or execute an ingestion VO locally.
- [ ] `retrieve --debug` records lineage through server-side lineage sink when requested.

- [ ] **Step 1: Add Restate service.**

Create `memory.rs` with service methods:
- `search(Json<MemorySearchRequest>) -> Json<MemorySearchResponse>`
- `show(Json<MemoryShowRequest>) -> Json<MemoryShowResponse>`
- `ingest_documents(Json<MemoryIngestRequest>) -> Json<MemoryIngestResponse>`
- `retrieve_debug(Json<MemoryRetrieveDebugRequest>) -> Json<MemoryRetrieveDebugResponse>`

Use the current production logic from `crates/moa-cli/src/memory.rs`, but construct graph/retriever/lineage from `OrchestratorCtx::current()` inside the orchestrator.

Expected: service owns memory retrieval, ingestion, and debug lineage.

- [ ] **Step 2: Bind service and add client methods.**

Update orchestrator binding and add client methods:
- `memory_search`
- `memory_show`
- `memory_ingest_documents`
- `memory_retrieve_debug`

Expected: client methods call `/Memory/search`, `/Memory/show`, `/Memory/ingest_documents`, and `/Memory/retrieve_debug`.

- [ ] **Step 3: Rewrite CLI memory module.**

Edit `memory.rs` so it only reads local files for `memory ingest`, builds DTOs, calls client methods, and renders returned DTOs.

Expected: `rg -n "load_graph_store|load_hybrid_retriever|load_ingestion_vo|ingest_turn_direct|HybridRetriever|AgeGraphStore|PgvectorStore" crates/moa-cli/src` returns no production matches.

- [ ] **Step 4: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test memory_service -- --test-threads=1
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: memory client, service, and CLI rendering tests pass.

---

## Task 5: Add Lineage And Privacy Cloud Services

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/lineage_admin.rs`
- Create: `crates/moa-orchestrator/src/services/privacy.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/lineage.rs`
- Modify: `crates/moa-cli/src/commands/privacy/mod.rs`
- Modify: `crates/moa-cli/src/commands/privacy/export.rs`
- Modify: `crates/moa-cli/src/commands/privacy/erase.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/lineage_admin.rs`
- Test: `crates/moa-orchestrator/tests/privacy_service.rs`
- Test: `crates/moa-cli/src/commands/privacy/tests.rs`

**Acceptance Criteria:**
- [ ] CLI lineage/privacy commands do not open `PostgresSessionStore` or run SQL directly.
- [ ] Lineage and privacy handlers authorize workspace/tenant access before reading or erasing data.
- [ ] Privacy erase and lineage erase both run on the server side and share the fixed PII vault crypto-shred behavior.
- [ ] CLI writes only local output files returned by the server API, not database-derived bundles assembled locally.

- [ ] **Step 1: Add LineageAdmin service.**

Create methods:
- `explain(Json<LineageExplainRequest>) -> Json<LineageExplainResponse>`
- `query(Json<LineageQueryRequest>) -> Json<LineageQueryResponse>`
- `export(Json<LineageExportRequest>) -> Json<LineageExportResponse>`
- `verify(Json<LineageVerifyRequest>) -> Json<LineageVerifyResponse>`
- `erase(Json<LineageEraseRequest>) -> Json<LineageEraseResponse>`

Move SQL preparation and verification helpers from `lineage.rs` into this service or into a private orchestrator module used by the service.

Expected: `crates/moa-cli/src/lineage.rs` contains rendering and request construction only.

- [ ] **Step 2: Add Privacy service.**

Create methods:
- `export(Json<PrivacyExportRequest>) -> Json<PrivacyExportResponse>`
- `erase(Json<PrivacyEraseRequest>) -> Json<PrivacyEraseResponse>`

Move the execution logic from `commands/privacy/export.rs` and `commands/privacy/erase.rs` into server-side modules. Keep CLI-side JWT/proof parsing only if the proof is supplied by CLI flags; server must validate the proof before data access.

Expected: privacy data access and PII vault shredding happen only inside orchestrator.

- [ ] **Step 3: Add client methods and rewrite CLI modules.**

Add `lineage_*` and `privacy_*` methods to `OrchestratorClient`. Rewrite CLI handlers to call those methods and render responses.

Expected: `rg -n "PostgresSessionStore|sqlx::query|sqlx::query_scalar|PiiVault|hard_purge_with_audit" crates/moa-cli/src/lineage.rs crates/moa-cli/src/commands/privacy` returns no production matches.

- [ ] **Step 4: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test lineage_admin -- --test-threads=1
cargo test -p moa-orchestrator --test privacy_service -- --test-threads=1
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: lineage/privacy service and CLI tests pass.

---

## Task 6: Add Skills Cloud Service

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/skills.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/commands/skills.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/skills_service.rs`
- Test: `crates/moa-cli/src/commands/skills.rs`

**Acceptance Criteria:**
- [ ] CLI skill commands do not construct `SkillRegistry`.
- [ ] Skill writes are authorized on tenant/workspace scope in the service.
- [ ] CLI can still import markdown files and export markdown files, but persistence is server-side.

- [ ] **Step 1: Add Skills service.**

Create methods:
- `export(Json<SkillExportRequest>) -> Json<SkillExportResponse>`
- `import(Json<SkillImportRequest>) -> Json<SkillImportResponse>`
- `list(Json<SkillListRequest>) -> Json<SkillListResponse>`
- `bootstrap_global(Json<SkillBootstrapGlobalRequest>) -> Json<SkillBootstrapGlobalResponse>`

Use the existing `SkillRegistry` logic server-side.

Expected: orchestrator owns skill persistence.

- [ ] **Step 2: Add client methods and rewrite CLI.**

Add `skills_export`, `skills_import`, `skills_list`, and `skills_bootstrap_global` to `OrchestratorClient`. Rewrite `commands/skills.rs` so local work is limited to reading and writing markdown files.

Expected: `rg -n "SkillRegistry|create_session_store|moa_skills" crates/moa-cli/src/commands/skills.rs` returns no production matches.

- [ ] **Step 3: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test skills_service -- --test-threads=1
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: skills service, client, and CLI tests pass.

---

## Task 7: Add Eval Cloud Service And Workflow

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/eval.rs`
- Create: `crates/moa-orchestrator/src/workflows/eval_run.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/workflows/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/eval.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/eval_service.rs`
- Test: `crates/moa-cli/src/tests.rs`

**Acceptance Criteria:**
- [ ] CLI no longer constructs `EvalEngine`.
- [ ] `eval run` starts a server-side workflow and polls a run result.
- [ ] `eval plan`, `datasets`, `replay`, `scores`, and `compare` call server-side APIs.
- [ ] CLI exit-code behavior for `--ci` remains: 0 pass, 1 failed evals, 2 errors.

- [ ] **Step 1: Add Eval service and workflow.**

Create service methods:
- `plan(Json<EvalPlanRequest>) -> Json<EvalPlanResponse>`
- `run(Json<EvalRunRequest>) -> Json<EvalRunResponse>`
- `datasets_register(Json<EvalDatasetRegisterRequest>) -> Json<EvalDatasetRegisterResponse>`
- `datasets_list() -> Json<EvalDatasetListResponse>`
- `replay(Json<EvalReplayRequest>) -> Json<EvalReplayResponse>`
- `scores(Json<EvalScoresRequest>) -> Json<EvalScoresResponse>`
- `compare(Json<EvalCompareRequest>) -> Json<EvalCompareResponse>`

Create `EvalRun` workflow for billed/long-running eval execution. The `run` service method starts the workflow and returns a run id plus terminal summary when the request is synchronous; use the same run summary fields already rendered by CLI reporters.

Expected: eval execution runs in orchestrator, not in CLI.

- [ ] **Step 2: Add client methods and rewrite CLI.**

Add `eval_*` methods to `OrchestratorClient`; rewrite `eval.rs` to load local suite/config files, send them to the server as request payloads, render returned results, and preserve `eval_exit_code`.

Expected: `rg -n "EvalEngine|load_session_store|replay_dataset_live|moa_eval::ReplayConfig" crates/moa-cli/src/eval.rs` returns no production matches.

- [ ] **Step 3: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test eval_service -- --test-threads=1
cargo test -p moa-cli ci_exit_code_distinguishes_failures_and_errors -- --test-threads=1
```

Expected: eval client/service tests pass and CLI exit-code tests remain green.

---

## Task 8: Add Admin Maintenance Cloud APIs

**Dependencies:** Task 2

**Files:**
- Create: `crates/moa-orchestrator/src/services/admin_maintenance.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator-client/src/client.rs`
- Modify: `crates/moa-cli/src/commands/admin.rs`
- Modify: `crates/moa-cli/src/checkpoint.rs`
- Test: `crates/moa-orchestrator-client/tests/client_smoke.rs`
- Test: `crates/moa-orchestrator/tests/admin_maintenance.rs`
- Test: `crates/moa-cli/src/tests.rs`

**Acceptance Criteria:**
- [ ] CLI vector-promotion commands do not construct `PgvectorStore`, `TurbopufferStore`, or `WorkspacePromotion`.
- [ ] CLI checkpoint commands do not construct `NeonBranchManager`.
- [ ] Server handlers require tenant/workspace admin authorization before promotion or checkpoint mutation.
- [ ] Existing `NeonMaint/prune_branches` remains for scheduled pruning; user-facing checkpoint commands use `AdminMaintenance`.

- [ ] **Step 1: Add AdminMaintenance service.**

Create methods:
- `promote_workspace(Json<VectorPromoteRequest>) -> Json<VectorPromotionResponse>`
- `rollback_promotion(Json<VectorPromotionUpdateRequest>) -> Json<VectorPromotionResponse>`
- `finalize_promotion(Json<VectorPromotionUpdateRequest>) -> Json<VectorPromotionResponse>`
- `checkpoint_create(Json<CheckpointCreateRequest>) -> Json<CheckpointCreateResponse>`
- `checkpoint_list() -> Json<CheckpointListResponse>`
- `checkpoint_rollback(Json<CheckpointRollbackRequest>) -> Json<CheckpointRollbackResponse>`
- `checkpoint_cleanup() -> Json<CheckpointCleanupResponse>`

Expected: promotion and checkpoint side effects run only inside orchestrator.

- [ ] **Step 2: Add client methods and rewrite CLI.**

Add `admin_*` methods to `OrchestratorClient`; rewrite `commands/admin.rs` and `checkpoint.rs` to render returned DTOs.

Expected: `rg -n "NeonBranchManager|WorkspacePromotion|PgvectorStore|TurbopufferStore|create_session_store" crates/moa-cli/src/commands/admin.rs crates/moa-cli/src/checkpoint.rs` returns no production matches.

- [ ] **Step 3: Run focused verification.**

```bash
cargo test -p moa-orchestrator-client --test client_smoke -- --test-threads=1
cargo test -p moa-orchestrator --test admin_maintenance -- --test-threads=1
cargo test -p moa-cli --tests -- --test-threads=1
```

Expected: admin maintenance tests pass.

---

## Task 9: Remove Direct Server-Side CLI Dependencies

**Dependencies:** Tasks 3, 4, 5, 6, 7, 8

**Files:**
- Modify: `crates/moa-cli/Cargo.toml`
- Modify: `crates/moa-cli/src/main.rs`
- Modify: `crates/moa-cli/src/support.rs`
- Modify: `crates/moa-cli/src/doctor.rs`
- Modify: `crates/moa-cli/src/init.rs`
- Modify: `crates/moa-cli/src/tests.rs`
- Modify: `Cargo.lock`

**Acceptance Criteria:**
- [ ] `moa-cli` no longer depends on `moa-session`, `moa-brain`, `moa-memory-graph`, `moa-memory-ingest`, `moa-memory-vector`, `moa-lineage-audit`, `moa-lineage-core`, `moa-lineage-sink`, `moa-eval`, `moa-skills`, or `sqlx`.
- [ ] `support.rs` contains only config parsing, path expansion, current workspace/user helpers, and formatting helpers.
- [ ] `doctor` checks cloud endpoints through client/health APIs and does not query Postgres directly.
- [ ] `init` creates only local config/credential directories needed by a client; it does not create sandbox or memory directories for hosted execution.

- [ ] **Step 1: Remove dependencies.**

Edit `crates/moa-cli/Cargo.toml` to remove the server-side crates listed in the acceptance criteria and remove `sqlx`.

Expected: compile errors point only to stale imports in CLI modules.

- [ ] **Step 2: Remove stale imports and helpers.**

Edit `main.rs`, `support.rs`, `doctor.rs`, `init.rs`, and tests to remove direct server-side imports and local sandbox/memory setup.

Expected: `rg -n "moa_(session|brain|memory|lineage|eval|skills)|sqlx|PostgresSessionStore|create_session_store|AgeGraphStore|HybridRetriever|EvalEngine|SkillRegistry|MpscSink|PiiVault" crates/moa-cli/src crates/moa-cli/Cargo.toml` returns no production matches.

- [ ] **Step 3: Validate dependency tree.**

```bash
cargo tree -p moa-cli
```

Expected: output does not include `moa-session`, `moa-brain`, `moa-memory-*`, `moa-lineage-*`, `moa-eval`, `moa-skills`, or `sqlx`.

---

## Task 10: Update Documentation And Markdown

**Dependencies:** Tasks 1 through 9

**Files:**
- Modify: `docs/01-architecture-overview.md`
- Modify: `docs/02-brain-orchestration.md`
- Modify: `docs/03-communication-layer.md`
- Modify: `docs/08-security.md`
- Modify: `docs/10-technology-stack.md`
- Modify: `AGENTS.md` if verification guidance references removed daemon/local CLI concepts

**Acceptance Criteria:**
- [ ] Docs say the CLI is a cloud/control-plane client only.
- [ ] Docs do not mention an embedded CLI daemon, CLI-owned sandbox lifecycle, or CLI-owned local memory/session runtime.
- [ ] Security docs identify cloud orchestrator/hands as the sandbox/code-execution boundary.
- [ ] Technology stack docs list `moa-cli` dependencies consistently with the new Cargo graph.

- [ ] **Step 1: Update architecture and communication docs.**

Edit `docs/01-architecture-overview.md`, `docs/02-brain-orchestration.md`, and `docs/03-communication-layer.md` to describe the CLI as a thin client over `moa-orchestrator-client` and remove daemon/local runtime wording.

Expected: `rg -n "daemon|in-process|local runtime|embeds|sandbox lifecycle|filesystem generation" docs/01-architecture-overview.md docs/02-brain-orchestration.md docs/03-communication-layer.md` returns only intentional historical-negative statements or no matches.

- [ ] **Step 2: Update security and technology docs.**

Edit `docs/08-security.md` and `docs/10-technology-stack.md` so CLI is not described as a sandbox/code-execution trust boundary.

Expected: cloud hands/orchestrator own execution policy and sandbox isolation.

- [ ] **Step 3: Run docs grep.**

```bash
rg -n "daemon|local runtime|in-process|CLI-owned|direct Postgres|session store" docs AGENTS.md
```

Expected: remaining matches are either valid service names or explicitly say the CLI does not own those responsibilities.

---

## Task 11 (Final): End-To-End Verification

**Dependencies:** All preceding tasks

**Files:** None (read-only verification)

- [ ] **Step 1: Run formatting and deterministic tests.**

```bash
cargo fmt --all
cargo test -p moa-core --lib
cargo test -p moa-runtime --tests
cargo test -p moa-orchestrator-client --tests
cargo test -p moa-cli --tests
cargo test -p moa-orchestrator --tests --features integration -- --test-threads=1
```

Expected: all commands pass.

- [ ] **Step 2: Run workspace lint/build.**

```bash
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --workspace
git diff --check
```

Expected: all commands pass.

- [ ] **Step 3: Run Restate e2e with local stack.**

Use a fresh test database if the existing local `moa` database has migration checksum drift:

```bash
set -a
. ./.env.fga
set +a
RESTATE_ADMIN_URL=http://127.0.0.1:10011 \
MOA_RESTATE_DEPLOYMENT_HOST=host.docker.internal \
RESTATE_INGRESS_URL=http://127.0.0.1:10010 \
TEST_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa_e2e_cli_cloud \
cargo test -p moa-orchestrator --test integration -- --ignored --nocapture --test-threads=1
```

Expected: ignored Restate integration tests pass.

- [ ] **Step 4: Verify no CLI server-side dependency regression.**

```bash
rg -n "moa_(session|brain|memory|lineage|eval|skills)|sqlx|PostgresSessionStore|create_session_store|AgeGraphStore|HybridRetriever|EvalEngine|SkillRegistry|MpscSink|PiiVault" crates/moa-cli/src crates/moa-cli/Cargo.toml
cargo tree -p moa-cli
```

Expected: first command returns no production matches; second command contains none of the removed server-side crates.

- [ ] **Step 5: Verify plan success criteria.**

Confirm:
- `moa-cli` is a parser/client/renderer only.
- `moa-runtime` uses only cloud/orchestrator naming.
- No daemon config or daemon CLI command remains.
- Session orchestration, memory, lineage, eval, privacy, vector promotion, checkpointing, sandbox/filesystem/code execution, authz, and approvals are server-side.

Expected: every criterion is true in code, tests, and docs.

---

## Self-Review Checklist

- [x] Every requirement maps to a task.
- [x] No placeholders or vague implementation instructions remain.
- [x] Types, names, and interfaces are consistent across tasks.
- [x] Tasks that touch the same files are ordered by dependency.
- [x] Verification strategy exists and includes e2e coverage.
- [x] Final verification task is last.

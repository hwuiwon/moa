# Service Boundary Extraction Readiness Implementation Plan

> **Worker note:** Execute this plan task-by-task using the run-plan skill or subagents. Each step uses checkbox (`- [ ]`) syntax for progress tracking.

**Goal:** Keep MOA as one production binary while restructuring high-growth business logic so future service extraction can replace one internal adapter or application boundary instead of touching many handlers and workflows.

**Architecture:** Treat MOA as a modular monolith. `moa-orchestrator` remains the Restate composition and transport boundary; domain crates and application modules own business rules, persistence queries, repository logic, and policy decisions. Future remote services should be able to replace an in-process implementation at the composition root, not by changing turn workflows, handlers, or product-domain call sites.

**Tech Stack:** Rust, Restate handlers/workflows, Postgres/sqlx, OpenFGA authz, `moa-core` shared contracts, domain crates under `crates/moa-*`, and existing cargo/nextest verification lanes.

**Work Scope:**
- **In scope:** Current-state documentation, architecture policy guardrails, composition-root decomposition, `OrchestratorCtx` dependency narrowing, application/repository boundaries for action reviews, authz challenges, identity/admin surfaces, experiments, learning review, privacy, lineage admin, and provider routing.
- **Out of scope:** Splitting any logic into a separate deployed service, adding network RPC between MOA internals, replacing Restate, replacing Postgres, changing public API behavior solely for architecture cleanup, or broad rewrites of stable domain crates that already have clear boundaries.

**Verification Strategy:**
- **Level:** build, lint, focused integration tests, and architecture guard checks
- **Command:**

```bash
cargo fmt --all
cargo test -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-experiments -p moa-artifacts -p moa-providers --locked
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo test -p moa-orchestrator --test integration_service_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo clippy -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-experiments -p moa-artifacts -p moa-providers -p moa-orchestrator --all-targets --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

- **What it validates:** The modular-monolith boundaries compile together, existing tool/action policy behavior still works, experiment and learning surfaces still compile, and the orchestrator remains one production binary with clearer internal replacement points.

---

## Current Findings

- The inspected source checkout was clean before this plan file was created and includes the action-policy auto-mode refactor. `ActionReviews`, `AuthzChallenges`, and `LearningReview` are now Restate service surfaces, and `SkillLearning` is feature-gated behind `moa-orchestrator/skill-learning`.
- The production artifact direction is still one `moa-orchestrator` binary. `moa-orchestrator` has empty default features, with `provider-overrides`, `skill-learning`, and `internal-eval-runner` gated in `crates/moa-orchestrator/Cargo.toml`.
- `moa-orchestrator/src/main.rs` still constructs the database pools, auth providers, provider registry, embedding provider, tool router, lineage sink, graph memory retriever, SCIM state, Restate endpoint, background jobs, health server, and shutdown handling in one file.
- `OrchestratorCtx` still exposes concrete shared dependencies process-wide: `PostgresSessionStore`, raw `PgPool`, FGA client, provider registry, embedding provider, `ToolRouter`, graph-memory retriever, and lineage handles.
- Direct SQL and global-context usage still concentrate in Restate handlers. Current hotspots include `services/scim/groups.rs`, `services/lineage_admin.rs`, `services/action_reviews.rs`, `services/scim/users.rs`, `services/privacy.rs`, `services/eval.rs`, `services/analytics.rs`, `services/agents.rs`, `services/authz_challenges.rs`, and `services/api_keys.rs`.
- Healthy boundaries already exist and should be reused instead of replaced: `GraphStore` and `VectorStore` in memory, `ToolRouter` and `HandProvider` in hands, `ExperimentStore` in experiments, artifact registries, skill registries, and `SessionStore`.
- Docs mostly reflect action-policy mode now, but `docs/implementation-caveats.md`, parts of `docs/03-communication-layer.md`, and some technology-stack wording still carry approval-era language that can confuse future boundary work.

## Boundary Rules

Use these rules for every task in this plan:

- `moa-orchestrator` handlers may authenticate, authorize, translate DTOs, call `ctx.run`, invoke Restate services/workflows, and record transport-level telemetry.
- Domain persistence queries belong in owning crates or clearly named application/repository modules, not inline in handler methods.
- Business decisions should be owned by domain/application modules. Handlers should not decide policy beyond authorization and transport validation.
- Keep `moa-core` for shared contracts, IDs, config, events, and trait surfaces. Do not move graph, action-review, SCIM, experiment, or lineage-specific SQL into `moa-core`.
- New internal crates are allowed only when a module has a stable owner and at least two call sites or test lanes benefit. Prefer module extraction first when ownership is still moving.
- Do not introduce RPC clients or network service abstractions until a deployed split is actively scheduled.
- A future remote implementation must be replaceable from the composition root with no changes to turn workflows or domain tests.

---

### Task 1: Document The Modular-Monolith Boundary Contract

**Dependencies:** None

**Files:**
- Modify: `docs/01-architecture-overview.md`
- Modify: `docs/02-brain-orchestration.md`
- Modify: `docs/12-restate-architecture.md`
- Modify: `docs/10-technology-stack.md`
- Modify: `docs/15-architecture-policy.md`
- Modify: `docs/implementation-caveats.md`

**Acceptance Criteria:**
- [ ] Docs state that MOA remains one production binary while domain logic should sit behind in-process application/repository boundaries.
- [ ] `docs/15-architecture-policy.md` defines the allowed responsibilities for Restate handlers, application services, repositories, domain crates, and composition code.
- [ ] `docs/implementation-caveats.md` removes stale blocking tool-approval wording or explicitly narrows it to builtin async-authz challenges where still accurate.
- [ ] The service list in architecture docs includes `ActionReviews`, `AuthzChallenges`, `LearningReview`, and feature-gated `SkillLearning` consistently.
- [ ] No doc suggests creating internal network services as the next step for this effort.

**Steps:**
- [ ] Add a "Modular Monolith Boundary Policy" section to `docs/15-architecture-policy.md`.
- [ ] Update the runtime/service lists in `docs/01-architecture-overview.md`, `docs/02-brain-orchestration.md`, and `docs/12-restate-architecture.md` to distinguish production services, feature-gated services, and internal application boundaries.
- [ ] Update `docs/10-technology-stack.md` so `MOA_PERMISSIONS_*` and async-authz wording no longer implies blocking tool approvals.
- [ ] Rewrite or delete stale approval-era caveats in `docs/implementation-caveats.md`.
- [ ] Run:

```bash
rg -n -g '!docs/engineering-discipline/plans/*.md' "blocking tool approval|unified approval|approval buttons|/approval|WaitingApproval|approval_wait|requires_approval" docs
git diff --check
```

Expected: remaining hits are only about builtin async-authz challenges, Auth0/CIBA protocol language, or historical migration references explicitly called out as such.

---

### Task 2: Split Orchestrator Startup Into Composition Modules

**Dependencies:** Task 1

**Files:**
- Create: `crates/moa-orchestrator/src/runtime/mod.rs`
- Create: `crates/moa-orchestrator/src/runtime/database.rs`
- Create: `crates/moa-orchestrator/src/runtime/deps.rs`
- Create: `crates/moa-orchestrator/src/runtime/endpoint.rs`
- Create: `crates/moa-orchestrator/src/runtime/jobs.rs`
- Modify: `crates/moa-orchestrator/src/lib.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator/src/ctx.rs`

**Acceptance Criteria:**
- [ ] `main.rs` handles CLI parsing, telemetry setup, server startup, shutdown coordination, and top-level error context.
- [ ] Database pool creation and migration application live in `runtime/database.rs`.
- [ ] Provider registry, auth providers, session store, embedding provider, tool router, lineage sink, graph memory retriever, and SCIM state construction live in `runtime/deps.rs`.
- [ ] Restate service/workflow binding lives in `runtime/endpoint.rs`.
- [ ] Cron bootstrap, challenge reaper startup, and other background job wiring live in `runtime/jobs.rs`.
- [ ] Feature-gated bindings for `internal-eval-runner` and `skill-learning` remain identical in behavior.
- [ ] Existing expected-service-name tests still pass or move with the endpoint builder.

**Steps:**
- [ ] Move helper functions out of `main.rs` one group at a time. Keep function signatures explicit; do not hide construction behind macros.
- [ ] Introduce a `RuntimeDeps` struct that owns constructed dependencies and can be installed into `OrchestratorCtx`.
- [ ] Keep endpoint registration order stable unless a test proves order is irrelevant.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: behavior is unchanged; `main.rs` is smaller and no handler/service files were touched except for imports forced by moved helpers.

---

### Task 3: Narrow `OrchestratorCtx` Into Typed Dependency Groups

**Dependencies:** Task 2

**Files:**
- Modify: `crates/moa-orchestrator/src/ctx.rs`
- Modify: `crates/moa-orchestrator/src/runtime/deps.rs`
- Modify: `crates/moa-orchestrator/src/services/action_reviews.rs`
- Modify: `crates/moa-orchestrator/src/services/authz_challenges.rs`
- Modify: `crates/moa-orchestrator/src/services/experiments.rs`
- Modify: `crates/moa-orchestrator/src/services/analytics.rs`
- Modify: `crates/moa-orchestrator/src/services/memory.rs`
- Modify: `crates/moa-orchestrator/src/services/agents.rs`
- Modify: `crates/moa-orchestrator/src/services/api_keys.rs`
- Modify: `crates/moa-orchestrator/src/services/tenants.rs`
- Modify: `crates/moa-orchestrator/src/services/privacy.rs`
- Modify: `crates/moa-orchestrator/src/services/lineage_admin.rs`
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-orchestrator/src/workflows/consolidate.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run.rs`
- Modify: `crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs`

**Acceptance Criteria:**
- [ ] `OrchestratorCtx` no longer exposes every dependency as a public field.
- [ ] Dependency groups are explicit, for example `PersistenceDeps`, `AuthDeps`, `ProviderDeps`, `ToolDeps`, `MemoryDeps`, and `LineageDeps`.
- [ ] Handlers call small accessors or receive domain-specific dependencies instead of freely reaching into raw global fields.
- [ ] There is still one installed process-wide context for Restate compatibility.
- [ ] No business logic is moved into `ctx.rs`.

**Steps:**
- [ ] Define dependency group structs in `ctx.rs` or `runtime/deps.rs`.
- [ ] Make `OrchestratorCtx` fields private where practical and add focused accessors.
- [ ] Convert call sites gradually, starting with simple read-only services such as `Whoami`, `Health`, `Artifacts`, and `Skills`, then larger services.
- [ ] Run:

```bash
rg -n "OrchestratorCtx::current\\(\\)\\.(graph_pool|session_store|providers|tool_router|auth_providers|embedding_provider|graph_memory_retriever)" crates/moa-orchestrator/src
cargo fmt --all
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-orchestrator --all-targets --locked -- -D warnings
```

Expected: the `rg` output is empty or only contains approved temporary call sites listed in comments near the accessors.

---

### Task 4: Extract Action Review And Authz Challenge Application Boundaries

**Dependencies:** Task 3

**Files:**
- Create: `crates/moa-orchestrator/src/action_reviews/mod.rs`
- Create: `crates/moa-orchestrator/src/action_reviews/store.rs`
- Create: `crates/moa-orchestrator/src/action_reviews/app.rs`
- Create: `crates/moa-orchestrator/src/authz_challenges/mod.rs`
- Create: `crates/moa-orchestrator/src/authz_challenges/store.rs`
- Create: `crates/moa-orchestrator/src/authz_challenges/app.rs`
- Modify: `crates/moa-orchestrator/src/lib.rs`
- Modify: `crates/moa-orchestrator/src/services/action_reviews.rs`
- Modify: `crates/moa-orchestrator/src/services/authz_challenges.rs`
- Modify: `crates/moa-orchestrator/src/services/authz_challenges_reaper.rs`

**Acceptance Criteria:**
- [ ] `services/action_reviews.rs` contains the Restate trait, authorization, DTO translation, `ctx.run` calls, and workflow/tool invocations only.
- [ ] Action-review SQL lives in `action_reviews/store.rs`.
- [ ] Action-review business rules, idempotency behavior, event-recording state, canary screening, and clear/deny transition validation live in `action_reviews/app.rs`.
- [ ] `services/authz_challenges.rs` contains the Restate trait, identity check, DTO translation, and awakeable resolution only.
- [ ] Builtin async-authz challenge SQL lives in `authz_challenges/store.rs`.
- [ ] Tool action review and builtin async-authz challenge language stays separate.

**Steps:**
- [ ] Move `insert_review`, `list_pending_reviews`, `decide_review`, row mapping, and database errors out of `services/action_reviews.rs`.
- [ ] Move `list_builtin_challenges`, `decide_builtin_challenge`, and row mapping out of `services/authz_challenges.rs`.
- [ ] Add focused unit tests for app/store transition behavior where a database is not needed; keep DB tests for SQL behavior.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test integration_service_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo clippy -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: action-review behavior is unchanged, but the service file is a thin Restate adapter.

---

### Task 5: Extract Identity And Admin Repository Boundaries

**Dependencies:** Task 3

**Files:**
- Create: `crates/moa-orchestrator/src/identity_admin/mod.rs`
- Create: `crates/moa-orchestrator/src/identity_admin/users.rs`
- Create: `crates/moa-orchestrator/src/identity_admin/groups.rs`
- Create: `crates/moa-orchestrator/src/identity_admin/agents.rs`
- Create: `crates/moa-orchestrator/src/identity_admin/api_keys.rs`
- Create: `crates/moa-orchestrator/src/identity_admin/tenants.rs`
- Modify: `crates/moa-orchestrator/src/lib.rs`
- Modify: `crates/moa-orchestrator/src/services/scim/users.rs`
- Modify: `crates/moa-orchestrator/src/services/scim/groups.rs`
- Modify: `crates/moa-orchestrator/src/services/scim/deactivation.rs`
- Modify: `crates/moa-orchestrator/src/services/agents.rs`
- Modify: `crates/moa-orchestrator/src/services/api_keys.rs`
- Modify: `crates/moa-orchestrator/src/services/tenants.rs`

**Acceptance Criteria:**
- [ ] SCIM services keep SCIM protocol parsing/rendering but move user/group persistence and group membership queries into `identity_admin`.
- [ ] Agent, API-key, and tenant persistence helpers move behind repository/application functions.
- [ ] OpenFGA tuple outbox writes remain transactional with product state writes.
- [ ] Service handlers still perform the required authz checks before protected reads or carry existing `// SAFETY:` comments for internal calls.
- [ ] No new crate is introduced in this task; the first pass makes ownership explicit inside the orchestrator crate.

**Steps:**
- [ ] Extract one surface at a time: SCIM users, SCIM groups, SCIM deactivation, agents, API keys, tenants.
- [ ] Preserve transaction boundaries exactly when outbox tuple writes occur.
- [ ] Add tests around repository functions that encode fragile SQL or tuple side effects.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-orchestrator --all-targets --locked -- -D warnings
rg -n "sqlx::query|query_as|query_scalar|SELECT |INSERT |UPDATE |DELETE " crates/moa-orchestrator/src/services/scim crates/moa-orchestrator/src/services/agents.rs crates/moa-orchestrator/src/services/api_keys.rs crates/moa-orchestrator/src/services/tenants.rs
git diff --check
```

Expected: the final `rg` output is zero or limited to transport-only count queries that are explicitly documented as temporary exceptions.

---

### Task 6: Extract Experiment, Skill Learning, And Learning Review Application Boundaries

**Dependencies:** Task 3

**Files:**
- Modify: `crates/moa-experiments/src/store.rs`
- Modify: `crates/moa-experiments/src/plan.rs`
- Modify: `crates/moa-skills/src/registry.rs`
- Modify: `crates/moa-skills/src/distiller.rs`
- Modify: `crates/moa-skills/src/improver.rs`
- Modify: `crates/moa-orchestrator/src/services/experiments.rs`
- Modify: `crates/moa-orchestrator/src/services/learning_review.rs`
- Modify: `crates/moa-orchestrator/src/workflows/skill_learning.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run/plan_expansion.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run/status.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run/status.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run/trial_simulator.rs`

**Acceptance Criteria:**
- [ ] `Experiments` service methods authorize and dispatch, but plan generation, run projection, score comparison assembly, and candidate proposal logic live in domain/application modules.
- [ ] `SkillLearning` workflow delegates skill draft generation and candidate creation to `moa-skills` APIs.
- [ ] `LearningReview` delegates skill publication/materialization to `moa-skills` or artifact registry APIs rather than owning mutation logic inline.
- [ ] Experiment and skill-learning behavior remains gated as currently documented.
- [ ] Live behavior experiments still require `learning_candidates` before any skill or workflow improvement becomes active.

**Steps:**
- [ ] Identify functions in `services/experiments.rs` and learning review/skill learning workflows that can move without changing Restate behavior.
- [ ] Add application functions in owning crates before editing handlers.
- [ ] Update tests to call domain/app functions for behavior and handlers for authz/transport mapping.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-experiments -p moa-skills --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo test -p moa-orchestrator --test behavior_lab_simulation_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo clippy -p moa-experiments -p moa-skills -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: experiment and skill-learning code is easier to test without Restate while runtime surfaces remain unchanged.

---

### Task 7: Extract Privacy And Lineage Admin Query Boundaries

**Dependencies:** Task 3

**Files:**
- Create: `crates/moa-lineage/audit/src/admin.rs`
- Create: `crates/moa-lineage/sink/src/admin.rs`
- Create: `crates/moa-memory/pii/src/erasure.rs`
- Modify: `crates/moa-orchestrator/src/services/privacy.rs`
- Modify: `crates/moa-orchestrator/src/services/lineage_admin.rs`
- Modify: `crates/moa-orchestrator/src/services/audit.rs`

**Acceptance Criteria:**
- [ ] `Privacy` service owns authorization, request validation, and response mapping, not graph erasure SQL.
- [ ] Erasure candidate enumeration and scoped hard-purge helpers live in `moa-memory-pii` or a memory privacy application module.
- [ ] `LineageAdmin` service owns authorization and query safety enforcement, while lineage/audit query helpers live in lineage crates.
- [ ] Compliance-audit warnings and attestation caveats remain intact.
- [ ] Existing RLS/ScopedConn behavior is preserved.

**Steps:**
- [ ] Move pure lineage query builders and row mapping into lineage crates.
- [ ] Move privacy erasure candidate enumeration and scoped transaction helpers into memory/PII-owned APIs.
- [ ] Keep dynamic SQL allowlisting and read-only transaction enforcement explicit and tested.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-lineage-core -p moa-lineage-sink -p moa-lineage-audit -p moa-memory-pii --locked
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-lineage-core -p moa-lineage-sink -p moa-lineage-audit -p moa-memory-pii -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: privacy and lineage admin handlers shrink without changing authorization or erasure semantics.

---

### Task 8: Move Provider Registry And Model Routing Out Of `LLMGateway`

**Dependencies:** Task 3

**Files:**
- Create: `crates/moa-providers/src/registry.rs`
- Create: `crates/moa-providers/src/routing.rs`
- Modify: `crates/moa-providers/src/lib.rs`
- Modify: `crates/moa-orchestrator/src/services/llm_gateway.rs`
- Modify: `crates/moa-orchestrator/src/runtime/deps.rs`
- Modify: `crates/moa-providers/tests/anthropic_offline.rs`
- Modify: `crates/moa-providers/tests/anthropic_provider.rs`
- Modify: `crates/moa-providers/tests/gemini_offline.rs`
- Modify: `crates/moa-providers/tests/openai_offline.rs`
- Modify: `crates/moa-providers/tests/openai_provider.rs`
- Modify: `crates/moa-providers/tests/provider_matrix_live.rs`
- Modify: `crates/moa-providers/tests/request_body_snapshots.rs`
- Modify: `crates/moa-providers/tests/support/mod.rs`

**Acceptance Criteria:**
- [ ] `ProviderRegistry`, provider kind/model resolution, default model constants, and configured-provider construction live in `moa-providers`.
- [ ] `LLMGateway` remains a durable Restate facade for completion calls, token/cost accounting, ingestion dispatch, and session event append.
- [ ] Scripted/mock provider overrides remain feature-gated and unavailable in production-like environments.
- [ ] Provider routing can be tested without Restate.

**Steps:**
- [ ] Move registry types and routing tests from `services/llm_gateway.rs` to `moa-providers`.
- [ ] Preserve public constructor behavior used by orchestrator startup.
- [ ] Add provider-level tests for requested model routing, default model fallback, and missing provider errors.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-providers --locked
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-providers -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: provider/model behavior is unchanged and can be replaced at runtime construction without editing `LLMGateway`.

---

### Task 9: Reduce `moa-brain` Runtime Coupling Where It Blocks Replacement

**Dependencies:** Tasks 3, 8

**Files:**
- Modify: `crates/moa-brain/src/pipeline/builder.rs`
- Modify: `crates/moa-brain/src/pipeline/memory.rs`
- Modify: `crates/moa-brain/src/pipeline/skills/mod.rs`
- Modify: `crates/moa-brain/src/harness/mod.rs`
- Modify: `crates/moa-brain/Cargo.toml`

**Acceptance Criteria:**
- [ ] Production context pipeline assembly can accept prebuilt memory, skill, and provider-facing dependencies from the composition root.
- [ ] `moa-brain` keeps ownership of context compilation and retrieval ranking, but avoids constructing runtime infrastructure that belongs to providers/session/hands when a caller can inject it.
- [ ] Eval harness dependencies remain behind `eval-harness` or dev/test usage, and no new production dependency is added to `moa-brain` for this task.
- [ ] No prompt or retrieval behavior changes without focused tests.

**Steps:**
- [ ] Audit `moa-brain` direct dependencies and classify each as domain-owned, runtime assembly, or test/eval harness.
- [ ] Add injection-oriented constructors before removing current constructors.
- [ ] Convert orchestrator startup to call the injection-oriented path.
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-brain --locked
cargo test -p moa-orchestrator --lib --locked
cargo clippy -p moa-brain -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
```

Expected: `moa-brain` remains the context/retrieval crate, but the runtime composition root owns infrastructure construction.

---

### Task 10: Add Architecture Guardrails To Prevent New Handler-Centric Logic

**Dependencies:** Tasks 1 through 9

**Files:**
- Create: `crates/xtask/src/check_architecture_boundaries.rs`
- Modify: `crates/xtask/src/main.rs`
- Modify: `docs/20-testing.md`
- Modify: `.github/workflows/integration-tests.yml`

**Acceptance Criteria:**
- [ ] `cargo run -p xtask -- check-architecture-boundaries` reports direct SQL and broad `OrchestratorCtx::current()` access in Restate handlers.
- [ ] The checker supports an explicit allowlist with comments for temporary exceptions.
- [ ] The checker fails on new direct SQL in `crates/moa-orchestrator/src/services/**` unless allowlisted.
- [ ] The checker fails on new direct access to raw `graph_pool`, `session_store`, `providers`, or `tool_router` from service handlers unless allowlisted.
- [ ] Documentation tells contributors when to add a domain repository or application module.

**Steps:**
- [ ] Implement the xtask as a simple source scanner using path allowlists checked into the xtask source or a small config file.
- [ ] Seed the allowlist with remaining approved exceptions after Tasks 1 through 9.
- [ ] Add a docs/testing note for when to run it.
- [ ] Run:

```bash
cargo fmt --all
cargo run -p xtask -- check-architecture-boundaries
cargo test -p xtask --locked
cargo clippy -p xtask --all-targets --locked -- -D warnings
git diff --check
```

Expected: the checker passes on current code and gives clear file/line diagnostics when a prohibited pattern is introduced.

---

### Task 11: Final End-To-End Verification

**Dependencies:** Tasks 1 through 10

**Files:** None for implementation. Read-only verification only.

**Acceptance Criteria:**
- [ ] All implementation tasks have passed their local verification commands.
- [ ] The final verification strategy command passes.
- [ ] Architecture docs, code ownership, and source guardrails agree with each other.
- [ ] `moa-orchestrator` still builds as one production binary and no internal RPC/service split was introduced.
- [ ] Existing action-policy, skill-learning, memory, experiment, and authz challenge behavior remains covered by focused tests or compile/no-run integration gates.

**Steps:**
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-experiments -p moa-artifacts -p moa-providers --locked
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo test -p moa-orchestrator --test integration_service_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo run -p xtask -- check-architecture-boundaries
cargo clippy -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-experiments -p moa-artifacts -p moa-providers -p moa-orchestrator -p xtask --all-targets --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Expected: every command exits 0.

- [ ] Run current-state scans:

```bash
rg -n "OrchestratorCtx::current\\(\\)\\.(graph_pool|session_store|providers|tool_router|auth_providers|embedding_provider|graph_memory_retriever)" crates/moa-orchestrator/src/services crates/moa-orchestrator/src/workflows
rg -n "sqlx::query|query_as|query_scalar|SELECT |INSERT |UPDATE |DELETE " crates/moa-orchestrator/src/services crates/moa-orchestrator/src/workflows
```

Expected: remaining hits are either in approved application/repository modules, in allowlisted temporary exceptions, or in transport code with documented rationale. The scans should be materially smaller than the pre-plan hotspots.

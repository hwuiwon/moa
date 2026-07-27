# MOA High-Value Improvements Implementation Plan

**Status:** Final, revalidated implementation plan — implementation in progress
**Validated against:** `main` at `c2bbe7c4` on 2026-07-22
**Implementation status (2026-07-25):** Tasks 0.1, 0.2, 1.1, 1.2, 2.1, 2.3,
and 3.1 are complete with full verification and mutation ledgers; the
`run-clean-e2e.sh --live` milestone gate passed end-to-end twice
(runs `20260724193230` and `20260724214424`, the latter including Task 1.2). Execution details, accepted deviations
(2.1 intrinsic-effect tier semantics; 2.3 claim table at V000358;
attachment reply-candidacy refinement), and defect ledgers live in the
repository-local `task_plan.md` / `findings.md` / `progress.md` working files.
**Planning artifacts:** `task_plan.md`, `findings.md`, `progress.md`
**Scope:** Correctness, security, enterprise knowledge, product completion,
learning/evaluation, operations, measured simplification, and hard-break contract cleanup

## Goal

Make MOA trustworthy under retries, failures, multi-replica deployment, enterprise data
access, and cloud tool execution before expanding the product surface. This plan retains
only current-tree gaps with a demonstrated user, security, correctness, or operational
consequence. Code volume or architectural fashion is not a reason to change a working
boundary.

## Why These Changes, in Priority Order

| Rank | Outcome | Why it is high value |
|---:|---|---|
| 1 | Durable interaction integrity | MOA can acknowledge work and then strand, reorder, duplicate, or fail to durably explain it. This directly affects user trust and billed work. |
| 2 | Fail-closed cloud security | Production can inherit permissive tool defaults; prompt-injection escalation and MCP credential ownership are incomplete security boundaries. |
| 3 | Correct enterprise knowledge | Process-local credentials, shared graph occurrence IDs, and absent source ACLs block safe multi-replica enterprise connectors. |
| 4 | Complete Slack and operator workflows | Slack drops ordinary messages and omits final answers. Administrative lifecycle/read paths also stop short of safe daily operation. |
| 5 | Privacy-safe learning and measurable Behavior Lab | Raw session evidence can reach learning providers, derived data is outside erasure closure, and scorecards can lack evaluator-produced scores. |
| 6 | Production truth and bounded growth | Telemetry, alerts, lineage, shutdown draining, and event retention do not yet form a verified production contract. |
| 7 | Measured scale and simplification | MCP discovery, tool selection, quota coordination, sandbox profiles, globals, and optional subsystems should change only at concrete seams and measured bottlenecks. |

## Validated Claim Ledger

`Confirmed` means the production path demonstrates the gap. `Adjusted` means the need is
real but the original scope was too broad. `Refuted` means the proposed change would
rework existing behavior or violate the intended architecture.

| Claim | Verdict | Evidence and retained need |
|---|---|---|
| Failed/cancelled turns can strand or reorder queued messages | Confirmed | `objects/session/handlers.rs` clears the active turn for all terminal outcomes but drains FIFO only for completed/accepted outcomes. |
| Generic root/worker boundary failures always produce a canonical durable terminal fact | Confirmed gap | Root and worker catch-all boundaries build outcomes/callbacks without guaranteeing one turn-correlated Postgres terminal event. |
| Public message retries are end-to-end idempotent | Confirmed gap | Public contact/turn DTOs have no `client_message_id` plus request-hash/original-response fence. |
| Turn-admission capacity leases are not idempotent | Refuted | Existing memory/Redis lease paths already renew same-session acquisition and tolerate repeated release. |
| Multiple reply targets can be selected explicitly | Confirmed gap | Session stores typed targets, but wire/contact requests have no `reply_to`; automatic forwarding works only for one target. |
| Conversational action reviews resume their root/worker owner | Confirmed gap | Review result persistence lacks the owner callback that execution-task review already has. `ActionEnvelope` already carries the origin. |
| Checked-in production enforces explicit cloud tool enablement | Confirmed gap | The production overlay selects no fail-closed security profile. A deployment-level `Deny` already dominates a tool-level `Allow`; the missing contract is typed profile selection, startup validation, and explicit scoped grants. |
| Prompt-injection escalation has a typed circuit breaker | Confirmed gap | Durable execution classifies input only. Output inspection exists only in the local harness, and the generic loop fingerprint contains tool name plus input—not output security classification. |
| MCP credentials are tenant/actor scoped | Confirmed gap | Dispatch resolves a deployment-wide `(server, operation)` environment credential; `session_id` is diagnostic only. |
| Knowledge credentials are durable across replicas | Adjusted gap | Provider-owned references exist, but MOA-managed material is backed by separate process-local/fresh-empty environment vaults. |
| Equal-content chunks have safe graph/citation occurrence identity | Confirmed gap | Graph chunk identity uses tenant plus content hash; hydration selects one newest provenance row. |
| Enterprise source/document ACLs are enforced | Confirmed gap | Connection information barriers exist, but source/document principal admission does not. |
| Vector/model upgrades have a supported rebuild workflow | Confirmed gap | Promotion already provides 0.95 top-K validation, dual-read, rollback, and finalization, but no production workflow owns rebuild generations/progress or writes the existing re-embed read fence. |
| Slack ordinary messages and final answers complete the documented front door | Confirmed gap | Runtime ingress ignores ordinary messages; terminal delivery sends status rather than the durable `BrainResponse`. |
| MOA lacks a substantial public/operator API | Adjusted | Retain only offboarding/role lifecycle, distributed auth abuse controls, workspace context, and approval read/notification gaps. |
| Email/SMS adapters are broken conversations | Refuted | They are intentionally asynchronous notification channels; expose that capability honestly. |
| Learning evidence is privacy-gated before provider use | Confirmed gap | Learning formats raw user/tool/assistant data with truncation but no deterministic privacy admission. |
| Subject erasure closes over derived learning data | Confirmed gap | Task segments, experience/attribution, candidates, learning log, and draft/suite provenance are outside current traversal. |
| Behavior Lab has no real tests | Refuted | Model, DB, service, and scripted-provider coverage exists. The gap is evaluator-produced, provenance-linked score completeness. |
| Provider concurrency is entirely process-local | Refuted | Global concurrency leases exist. Pacing, cooldown, and retry budgets remain process-local, and distributed concurrency falls back to a local semaphore on store failure or missing-store startup. |
| Semantic graph work is always disabled/wasted | Adjusted | Deterministic extraction defaults on, model extraction defaults off, while main tenant retrieval forces graph expansion off. Measure one policy. |
| Production observability/lineage is fully wired | Confirmed/adjusted gap | OTLP currently exports traces only while runtime metrics are Prometheus-only; alerts are unprovisioned and production does not select durable lineage. The retained contract makes OTLP the default for traces and metrics, with Prometheus opt-in. |
| Move general CRUD out of Restate | Refuted | Restate intentionally journals durable writes and side effects. Keep it; use typed edge translators and narrow traits. |

## Architectural Invariants

- Restate owns durable orchestration and hot virtual-object state.
- Postgres owns product-visible history and cross-pod correctness.
- Authorization happens before protected reads; agent writes use delegated authorization.
- Preserve generation fences, append-only events, history-first recovery, legal holds,
  tenant RLS, and connection information barriers.
- Action review, authz challenge, execution review, and privacy approval retain separate
  write state machines. A unified inbox is a read/notification projection.
- Use existing narrow traits and application boundaries. Do not add internal network
  services, a generic repository facade, routing DSL, or generic promotion handler.
- Keep content hashes for embedding/diff dedupe; use occurrence identity only for
  provenance, ACL, citation, and lifecycle.
- Local defaults may remain convenient; a cloud profile must be explicit and validated.
- OTLP is the default vendor-neutral transport for traces and runtime metrics.
  Prometheus exposure is an explicitly selected alternative exporter, not a production
  assumption.

## Hard-Break Policy — Applies to Every Task

- Implement exactly one post-change contract. Delete the replaced field, type, trait
  signature, route, config/env key, storage identity, and import path in the same task.
- Do not add `serde` aliases/defaults for old payloads, deprecated wrappers, `pub use`
  re-exports, translation facades, optional legacy fields, dual old/new writers/readers,
  or fallback-to-legacy behavior.
- Update all in-repository callers, fixtures, manifests, and docs atomically. Stale
  external clients/configuration receive a typed schema/config error; MOA does not infer
  or synthesize the missing old value.
- A forward SQL backfill may transform existing rows before the new code serves traffic.
  After migration, only the new representation is readable/writable. Rollback uses a
  prior artifact plus a database backup or an inactive generation—not live compatibility
  code.
- Bounded shadow comparison is allowed for validation, but it cannot serve mixed
  generations, choose the old result as a runtime fallback, or retain old vocabulary in
  active code after cutover.

## Dependency Map

```text
M0 Gate truth and clean baselines
  +--> M1 Durable interaction integrity --> M4 Slack/operator workflows
  +--> M2 Secure runtime boundaries -----> M3 Enterprise knowledge
  |                                      +-> M5 Privacy-safe learning
  +--> M6 Production operations and scale

M2 durable credential owner --> M2 tenant MCP resolution --> M6 MCP catalogs
M3 occurrence identity ------> M3 source ACL -----------> M3 rebuild workflow
M5 privacy gate -------------> M5 evaluator-linked Behavior Lab lane
All milestones ---------> M7 end-to-end certification
```

Implement milestone-sized branches. Within a milestone, only tasks explicitly marked
parallel-safe may run concurrently. Update checkboxes and record actual commands/results
in this document or the active execution checkpoint.

Focused `_service_e2e` commands below require a running local Postgres, Restate, and
OpenFGA stack and therefore include the repository's `provider-overrides,integration`
features and ignored-test opt-in. For a contamination-free milestone gate, prefer
`MOA_RUN_LIVE_E2E=1 ./scripts/run-clean-e2e.sh --live`, which creates isolated state and
uses scripted providers; this flag does not authorize billed provider tests.

Baseline at `c2bbe7c4`:

- `cargo nextest run --locked --profile fast-pr`: 2,872 passed, 34 skipped.
- `cargo nextest run --workspace --locked --no-fail-fast`: 3,455 passed, two failed,
  285 skipped. One failure is a deterministic stale journal-role fixture; the other is
  a knowledge DB uniqueness failure that passed focused rerun. M0 must repair the former
  and reproduce/isolate the latter—never blanket-whitelist both.

## M0 — Restore the Architecture Gate

### Task 0.1 — Make architecture policy executable again [P0] [x]

**Depends on:** none
**Why:** the checker aborts on a removed env-overlay path before reporting real coupling
and budget drift. A dead gate cannot protect later refactors.

**Files:**

- `crates/xtask/src/check_architecture_boundaries.rs`
- `crates/xtask/src/execution_trace_manifest.rs`
- `crates/moa-config/src/env_overlay/mod.rs`
- new `crates/moa-config/src/env_overlay/tests.rs`
- `crates/moa-edge/src/routes.rs`
- new `crates/moa-edge/src/routes/tests.rs`
- `crates/moa-core/src/types/worker/commands.rs`
- new `crates/moa-core/src/types/worker/commands/tests.rs`
- `crates/moa-orchestrator/src/services/dual_control.rs`
- new `crates/moa-orchestrator/src/services/dual_control/repository.rs`
- `crates/moa-orchestrator/src/workflows/tenant_purge/mod.rs`
- `crates/moa-orchestrator/src/workflows/tenant_purge/repository.rs`
- `crates/moa-orchestrator/src/workflows/turn_events.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/implementation.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/mod.rs`
- new `crates/moa-orchestrator/src/workflows/turn_execution/tests.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/tools.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/guardrails.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/segments.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/experience.rs`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `docs/15-architecture-policy.md`
- `docs/01-architecture-overview.md`

**Implementation:**

1. Replace the stale `crates/moa-core/src/config/env_overlay/mod.rs` owner with
   `crates/moa-config/src/env_overlay/mod.rs`. Before any scan, validate every configured
   scan root, allowance, LOC/symbol budget, sensitive consumer, the special core-types
   path, and every execution-trace manifest path. Unit tests must identify the configured
   owner and exact missing path. Both new tests carry `// Pins:` scenario comments; the
   synthetic missing-path case uses an isolated `tempfile::TempDir` so parallel runs do
   not share filesystem state.
2. Move only the large inline test modules into the four listed child `tests.rs` files.
   Keep env-overlay sibling-test helpers and edge `test_support` with their production
   owners. Give every new module a `//!` module doc. Preserve the existing production LOC
   caps: env overlay 1,664; edge routes 1,749; turn execution 1,535; worker commands 352.
3. Record the accepted current architecture explicitly instead of hiding it: 51 workspace
   packages, 48 default members, 43 direct and 46 transitive `moa-core` reverse
   dependencies, and worker state at 344 lines because `WorkerInitialTask` now owns the
   inherited authenticated identity. Add ADR 0003 with the accepted category-owner
   splits and require another decision for future growth.
4. Move all six SQL statements, RLS transactions, row mapping, and typed storage outcomes
   out of `services/dual_control.rs` into its private repository. Move
   `load_external_vector_uid_page` from tenant-purge workflow logic into its existing
   repository. Do not add SQL allowances or public repository re-exports.
5. Add `schedule_turn_admission_heartbeat` to the execution-trace sender manifest using
   the replay-safe trace helper.
6. Remove the raw `OrchestratorCtx::current()` in `workflows/turn_events.rs`. Construct
   and own the event-appender dependency explicitly in both root and worker workflow
   implementations, and thread it through all listed turn-execution modules and runtime
   endpoint construction. Do not add a new global-access allowance.
7. Make both architecture documents state the exact eight temporary `current_*`
   allowances: four reads in session execution runs and two each in experiment-run and
   experiment-trial target execution. Task 6.6 removes them; no raw `current()` allowance
   remains.

**Acceptance:** stale configured paths fail a focused test with their owner and exact
path; the checker reaches all rules and exits 0; dual-control and tenant-purge workflow
owners contain no direct SQL; the heartbeat sender is trace-manifested; root and worker
turn workflows use an explicitly constructed event appender; no raw `current()` or SQL
allowance is added; the four production-file LOC caps remain unchanged after test
extraction and all four child test modules have module docs; only the exact 51/48 package,
43/46 reverse-dependency, and 344 worker-state ratchets are accepted and documented with
why they exist.

**Verification:**

```bash
cargo fmt --all -- --check
cargo test -p moa-config --locked env_overlay
cargo test -p moa-core --locked types::worker
cargo test -p moa-edge --locked routes
cargo test -p moa-orchestrator --locked --lib
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db_memory \
  -E 'test(/dual_control|tenant_purge_repository/)'
cargo test -p xtask --locked check_architecture_boundaries
cargo run -p xtask --locked -- check-architecture-boundaries
cargo clippy \
  -p xtask -p moa-config -p moa-core -p moa-edge -p moa-orchestrator \
  --all-targets --all-features --locked -- -D warnings
git diff --check
```

### Task 0.2 — Restore a trustworthy full-workspace baseline [P0] [x]

**Depends on:** Task 0.1
**Why:** a deterministic stale assertion and a full-suite-only DB uniqueness failure can
hide or falsely implicate regressions introduced by later milestones.

**Files:**

- `crates/moa-orchestrator/tests/execution_execution_support/assertions.rs`
- `crates/moa-orchestrator/tests/knowledge_service/ingestion.rs`
- `crates/moa-knowledge/src/ingestion/page.rs`
- `crates/moa-knowledge/src/error.rs`
- `crates/moa-knowledge/src/observability.rs`
- `crates/moa-knowledge/src/repository/contact_group.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/contact_groups_db_memory.rs`
- `crates/moa-knowledge/tests/knowledge_offline/provider_nango.rs`
- `crates/moa-knowledge/tests/knowledge_offline/provider_merge.rs`
- `crates/moa-session/src/testing.rs`
- `crates/moa-test-support/src/postgres.rs`
- `crates/moa-test-support/tests/fixtures_round_trip.rs`
- `.config/nextest.toml` only if reproduction proves an unavoidable shared resource
- `progress.md` for the primary agent's execution record, not the worker write set

**Implementation:**

1. Update the journal-role fixture to use an initial-planner prompt containing
   `<response_schema>...</response_schema>` without strict `response_format`. Add a
   restricted-amendment request and assert it remains `Normal`. Mutation-verify the exact
   role vector by temporarily removing or negating the production response-schema branch,
   observing failure, restoring it, and rerunning green.
2. Reproduce the knowledge failure only through the existing `bootstrap_test_db` physical
   database isolation. Do not run `make dev-wipe`, `docker compose down -v`, hand-written
   `ALTER`, or any write against the Compose maintenance database. In bounded focused and
   full-suite stress, capture the physical database name, tenant, logical group key/UID,
   duplicate-key detail, timeout location, and cleanup state.
3. Let evidence choose the knowledge fix. If page-internal concurrency is responsible,
   correct the `moa-knowledge` ingestion/repository owner. If physical-database isolation
   or cleanup is broken, correct its test-support owner. Add a nextest group only if an
   unavoidable shared resource is identified; never broadly serialize unrelated tests.
   If bounded stress and full-suite runs do not reproduce the duplicate, record it as
   non-reproduced rather than inventing a guard, compatibility path, or serialization.
4. Any knowledge remediation gets a deterministic regression at the discovered seam and
   a controlled mutation: temporarily revert the production guard/conflict/isolation
   correction, observe that regression fail, restore it, and rerun green. Stress success
   alone is not mutation verification.
5. The primary agent records exact commands, nextest run IDs, result counts, durations,
   and any non-reproduced observation in `progress.md` before later milestones start.

**Acceptance:** the focused journal test fails under the old fixture and passes under the
current response-schema contract, including the restricted-amendment exclusion; the
knowledge stress completes all bounded iterations without a duplicate-key failure or
timeout; the full workspace passes; the journal controlled mutation fails before
restoration; when knowledge remediation is required, its deterministic regression also
fails under the controlled production mutation before restoration; no failure is globally
ignored, whitelisted, broadly serialized, or repaired with a destructive database reset;
`progress.md` records commands, run IDs, counts, durations, and any non-reproduced
observation.

**Verification:**

```bash
cargo fmt --all -- --check
cargo nextest run -p moa-orchestrator --locked \
  --test execution_run_service_e2e \
  -E 'test(/journal_classification_distinguishes/)'
cargo nextest run -p moa-orchestrator --locked \
  --test knowledge_service \
  -E 'test(/mock_connector_end_to_end/)' \
  --stress-count 20
# If moa-knowledge ingestion or persistence changes:
cargo nextest run -p moa-knowledge --locked --test knowledge_db_memory
# If physical-database isolation changes:
cargo test -p moa-session --locked testing::tests::
cargo nextest run -p moa-test-support --locked --test fixtures_round_trip
cargo nextest run --workspace --locked --no-fail-fast
cargo clippy \
  -p moa-orchestrator -p moa-knowledge -p moa-session -p moa-test-support \
  --all-targets --all-features --locked -- -D warnings
git diff --check
```

## M1 — Durable Interaction Integrity

Task 1.1 is complete. Task 1.2 intentionally waits for Task 2.3 because both hard-change
`moa-core` credential/attachment traits; after 2.3, Task 1.2 can run beside Task 3.1.
Task 1.3 then owns the reviewed-tool continuation boundary required by Task 2.2.

### Task 1.1 — Define one terminal outcome and FIFO queue disposition [P0] [x]

**Depends on:** Task 0.2
**Why:** acknowledged work can remain invisible forever or execute after newer work;
generic workflow failures can also be absent from durable history.

**Files:**

- `crates/moa-core/src/events.rs`
- `crates/moa-wire/src/turn.rs`
- `crates/moa-brain/src/compaction.rs`
- `crates/moa-brain/src/pipeline/history/errors.rs`
- `crates/moa-session/src/store/dashboard.rs`
- `crates/moa-loadtest/src/runner.rs`
- `crates/moa-eval/core/src/conversation_cost.rs`
- `crates/moa-orchestrator/src/objects/session/handlers.rs`
- `crates/moa-orchestrator/src/objects/session/mod.rs`
- `crates/moa-orchestrator/src/objects/session/state.rs`
- `crates/moa-orchestrator/src/objects/worker/handlers.rs`
- `crates/moa-orchestrator/src/workflows/turn_events.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/implementation.rs`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- `crates/moa-orchestrator/tests/execution_execution_support/evaluation.rs`
- `crates/moa-orchestrator/tests/session_turn_lifecycle_service_e2e.rs`
- `crates/moa-orchestrator/tests/worker_coordination_service_e2e.rs`
- `crates/moa-orchestrator/tests/orchestrator_offline/session_vo.rs`
- `crates/moa-orchestrator/tests/integration/session_vo_e2e.rs`
- new `crates/moa-orchestrator/tests/turn_terminal_failure_service_e2e.rs`
- `.config/nextest.toml`
- `docs/02-brain-orchestration.md`
- `docs/05-session-event-log.md`
- `docs/12-restate-architecture.md`

**Implementation:**

1. Modify the production owner,
   `SessionPendingState.pending_messages: VecDeque<PendingMessage>`, and its
   `start_turn_inner` / `record_turn_outcome` transitions. Completed, accepted, failed,
   and coordinator-only cancelled outcomes dispatch the oldest pending message in FIFO.
   Stale or replayed callbacks for an older turn are complete no-ops: they cannot rewrite
   `last_outcome`/summary or affect a newer active turn. Clear a coordinator-only
   cancellation fence only after its matching outcome is recorded.
2. Whole-task-tree cancellation appends one typed queued-message rejection event/reason
   per already accepted message in FIFO order, drains immediately, and retains a fence
   that rejects future starts. It never dispatches queued work after the matching
   cancelled callback. Do not treat the existing write-only `cancel_flag` as sufficient.
3. Remove the legacy-only `SessionVoState.pending`, `K_PENDING`, `enqueue_message`,
   `drain_pending_messages`, `apply_turn_outcome`, load/persist plumbing, and tests that
   exercise that dead projection. Do not read or clear old `K_PENDING` as a compatibility
   path; tests must drive the live handler-owned queue.
4. Define one typed canonical failed-turn event with actor
   `Coordinator | Worker { worker_id }`, `turn_id`, coarse safe class, and fixed/bounded
   safe summary. Append it before any failed attention signal or owner callback with
   dedupe key `turn_failed:{actor_key}:{turn_id}` through an explicit custom-key
   `TurnEventAppender` path. Never persist `format!("{err:?}")` or provider/tool secrets in
   this event or `TurnOutcome.message`, and do not duplicate an inner `Event::Error`
   already recorded by a production path.
5. Add required trusted `parent_session` to `RunWorkerTurnRequest`, populated from
   `WorkerVoState` at every dispatch site, so even failures before the first prepared
   iteration can append the parent-session fact. This is the only request contract; do
   not infer the missing parent or retain the old wire shape.
6. Classify coordinator `TurnFailed` as `ProcessingEffect::Terminal` and worker
   `TurnFailed` as `Neutral`, so child failure facts cannot mask root scheduling state in
   the shared event log. Update every exhaustive event/history/dashboard/error-accounting
   consumer and document the contract.
7. Preserve `WorkerSignalReceived` as control-plane attention and
   `WorkerStatusChanged` / `WorkerNotificationDelivered` as worker-lifecycle delivery.
   They may coexist and are not substitutes for or duplicates of the turn terminal fact.
8. Add a deterministic scripted-provider failure fixture that exercises root and worker
   catch-all boundaries plus replay. Existing session/worker service E2Es are not
   sufficient failure injectors. Preserve history-first recovery ordering.

**Acceptance:** exact UserMessage/terminal ordering for A/B/C remains FIFO when A fails or
is coordinator-only cancelled; task-tree cancellation emits the exact queued-rejection
count/order, drains the queue, fences later admission, and dispatches nothing afterward;
duplicate/stale owner callbacks are no-ops. Replay yields exactly one new canonical
failed-turn fact per actor plus `turn_id`, before its owner outcome, with no injected raw
secret. Coordinator failure is scheduling-terminal and worker failure scheduling-neutral.
Existing worker attention/lifecycle facts may coexist and are counted separately. The old
pending projection, old worker request shape, raw debug summaries, and sequence-only
canonical dedupe path have no remaining callers or compatibility behavior.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-core --locked events
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration --run-ignored ignored-only --test turn_terminal_failure_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration --run-ignored ignored-only --test session_turn_lifecycle_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration --run-ignored ignored-only --test worker_coordination_service_e2e
cargo clippy -p moa-core -p moa-wire -p moa-brain -p moa-session \
  -p moa-orchestrator -p moa-loadtest -p moa-eval-core \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must restore the old Completed/Accepted-only queue pop, remove the
task-tree fence, remove/change the actor-plus-turn dedupe key, and classify worker failure
as scheduling-terminal. Each corresponding targeted test must fail before restoration and
pass afterward.

### Task 1.2 — Add message retry identity and explicit reply correlation [P0] [x]

**Depends on:** Tasks 1.1 and 2.3
**Why:** a disconnect before the first event can duplicate attachments and paid turns;
concurrent requests for user input are otherwise ambiguous.

**Files:**

- `crates/moa-core/src/types/contact.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-core/src/error.rs`
- `crates/moa-wire/src/turn.rs`
- `crates/moa-session/src/store/session_attachments.rs`
- `crates/moa-session/src/attachment_storage.rs`
- `crates/moa-session/tests/session_attachments_docker.rs`
- `crates/moa-edge/src/routes.rs`
- `crates/moa-edge/src/routes/contact_messages.rs`
- `crates/moa-edge/src/routes/session_stream.rs`
- `crates/moa-orchestrator/src/objects/session/mod.rs`
- `crates/moa-orchestrator/src/objects/session/handlers.rs`
- `crates/moa-orchestrator/src/objects/session/state.rs`
- `crates/moa-orchestrator/src/services/contacts.rs`
- `crates/moa-orchestrator/src/workflows/experiment_run/target_execution.rs`
- `crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs`
- `crates/moa-loadtest/src/backend.rs`
- `crates/moa-loadtest/src/edge_backend.rs`
- `crates/moa-loadtest/src/runner.rs`
- `crates/moa-test-support/src/orchestrator_fixture/conversation.rs`
- `crates/moa-orchestrator/tests/session_turn_lifecycle_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/session_vo_e2e.rs`
- `crates/moa-orchestrator/tests/orchestrator_offline/session_vo.rs`
- `crates/moa-orchestrator/tests/coordinator_worker_behavior_provider_e2e.rs`
- `crates/moa-orchestrator/tests/execution_eval_provider_e2e.rs`
- `crates/moa-orchestrator/tests/execution_execution_support/fixtures.rs`
- `crates/moa-orchestrator/tests/integration/action_policy_flow_e2e.rs`
- `crates/moa-orchestrator/tests/integration/guardrails_e2e.rs`
- `crates/moa-orchestrator/tests/integration/turn_responsiveness_e2e.rs`
- `crates/moa-orchestrator/tests/turn_terminal_failure_service_e2e.rs`
- `crates/moa-test-support/tests/orchestrator_fixture_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/agent_artifacts_e2e.rs`
- `crates/moa-orchestrator/tests/integration/session_brain_e2e.rs`
- `crates/moa-orchestrator/tests/support/session_fixtures.rs`
- `crates/moa-orchestrator/tests/support/mod.rs`
- `crates/moa-edge/tests/direct_read_routes_db.rs`
- new `crates/moa-edge/tests/direct_read_routes_db/session_messages_db.rs`
- new `crates/moa-edge/tests/session_message_attachments_docker.rs`
- `docs/01-architecture-overview.md`
- `docs/02-brain-orchestration.md`
- `docs/03-communication-layer.md`
- `docs/05-session-event-log.md`
- `docs/12-restate-architecture.md`
- `SEQUENCE-DIAGRAMS.md`

**Implementation:**

1. Add a required public `ClientMessageId` of 1–256 UTF-8 bytes with control characters
   rejected, able to represent REST IDs and Slack event IDs, plus an optional typed
   `reply_to`. Carry them through
   `StartTurnRequest`, `QueueMessageRequest`, pending-message state, load/eval/support
   callers, and every request literal. Delete `Session/post_message` and migrate its
   callers to the single `start_turn`/`queue_message` contract; do not keep `UserMessage`
   transport or synthesize an ID for old clients.
2. Add a domain-versioned canonical request hash over every semantic admission field:
   text, ordered attachment metadata/content digests, model, resolved contact, maximum
   turns, execution template, and `reply_to`. Exclude credentials such as
   `contact_token`. Session owns a dedicated new VO key/projection for `(tenant,
   session, client_message_id, request_hash, original StartTurnResponse)`; do not add a
   `serde(default)` field to old state or preserve a legacy state shape.
3. Consult the fence before reply delivery, queue mutation, turn dispatch, or any other
   Session side effect. Persist one admission result containing the original
   `StartTurnResponse` and pre-admission `SequenceNum`. During its guarantee window, same
   key/hash returns that original response/cursor, including after a queued turn starts;
   same key/different hash returns a typed conflict. Never evict unresolved or queued
   admissions. An admission becomes terminal
   only when its turn/reply has a terminal disposition, not when admission returns. Then
   retain it until the earlier of 24 hours or 256 newer terminal admissions in that
   session, with explicit eviction metrics/docs; after eviction the ID may be admitted
   again and no longer carries an idempotency guarantee.
4. Hard-change `SessionAttachmentStore::put` and its Postgres implementation to accept a
   deterministic slot identity derived from tenant/session/client message ID and
   attachment ordinal. Persist and compare content digest plus metadata against the slot.
   Object storage uses create-only/compare-before-overwrite semantics so a retry cannot
   overwrite an existing object before SQL detects the conflict. Return whether storage
   was created or replayed; reject a slot with changed digest/metadata; rejection cleanup
   removes only objects created by that request, never a replayed original.
5. Define the reply matrix exactly: no `reply_to` plus zero targets is an ordinary turn;
   no `reply_to` plus one target uses the convenience delivery; no `reply_to` plus
   multiple targets is a typed rejection with no turn/queue mutation; explicit current
   targets deliver only to that execution-confirmation, execution-input, or worker-input
   target; explicit stale/nonmatching targets conflict without mutation; replay returns
   the fenced response without a second delivery.
6. On the first request without `Last-Event-ID`, submit and store the pre-admission cursor.
   A same-ID retry without `Last-Event-ID` ignores the newly observed stream head and
   returns the stored cursor. A retry with `Last-Event-ID` still invokes the fence for
   ID/hash validation, then resumes from `Last-Event-ID + 1`; the transport cursor is not
   part of the semantic request hash. Add public edge DB behavior tests for missing ID,
   replay, conflict, and reconnect; register the exact module in the existing lane.
   Mark the multipart edge Docker test `#[ignore = "requires RustFS/object storage"]`,
   require `MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1` inside it, and pin attachment
   replay/create-only/cleanup behavior there.
7. Reuse Slack event IDs in Task 4.1. Internal experiment/eval/load callers derive IDs
   deterministically from stable workflow coordinates such as run UID, trial UID, and
   message ordinal—never clocks or randomness. Do not alter the already-correct capacity
   leases or reserve a migration for this task; deterministic attachment IDs use the
   existing primary key and the Session VO owns its durable replay state. Existing opaque
   attachment UUID rows are not a second readable identity contract; new writes contain
   no random attachment-ID generation.

**Acceptance:** retries before/after attachment upload, admission, queueing, turn start,
and first SSE delivery create no second user event, attachment, queue item, reply
delivery, or paid turn, and return the exact original response. Missing/empty/oversized
IDs, same-key/different-request hashes, attachment identity/digest collisions, explicit
stale targets, and ambiguous implicit replies fail with typed safe errors before Session
mutation. Root execution-confirmation/input and worker-input reply matrices are exact.
`Last-Event-ID` reconnect cannot bypass the fence and retries return the correct stored
or caller-resume cursor. Unresolved/queued entries survive
bounded-cache pressure; terminal entries deduplicate only within the declared 24-hour/
256-newer-entry window, whose expiration is observable and documented. No server-
generated ID, Session `post_message`, old Session `UserMessage` wire route, compatibility
field, or random attachment-ID generation for new writes remains.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-core --locked types::contact
cargo test -p moa-wire --locked turn
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test session_turn_lifecycle_service_e2e
cargo nextest run -p moa-edge --locked --test direct_read_routes_db \
  -E 'test(/session_message/)'
MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 cargo nextest run -p moa-edge \
  --locked --run-ignored ignored-only --test session_message_attachments_docker
MOA_RUN_SESSION_ATTACHMENT_DOCKER_TESTS=1 cargo nextest run -p moa-session \
  --locked --run-ignored ignored-only --test session_attachments_docker
cargo clippy -p moa-core -p moa-wire -p moa-session -p moa-edge \
  -p moa-orchestrator -p moa-loadtest -p moa-test-support \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
! rg -n 'post_message' crates/moa-orchestrator/src/objects/session
! rg -n 'Session(::|/|: +)post_message' docs SEQUENCE-DIAGRAMS.md \
  --glob '!docs/engineering-discipline/plans/**'
git diff --check
```

Every added behavior test carries `// Pins:` and isolated tenant/session/message IDs.
Mutation verification must bypass the same-key fence, accept a changed canonical hash,
generate random attachment IDs, delete a replayed attachment during rejection cleanup,
treat an ambiguous reply as an ordinary turn, and let reconnect fabricate a fresh
response. Each targeted assertion must fail before restoration and pass afterward.

### Task 1.3 — Continue conversational owners after action review [P0] [ ]

**Depends on:** Tasks 1.1 and 1.2
**Why:** an approved tool can finish after the nonblocking model loop ends, leaving the
user without a synthesis and worker-local state unresumed.

**Files:**

- `crates/moa-core/src/events.rs`
- `crates/moa-core/src/types/action_policy.rs`
- `crates/moa-core/src/types/worker/commands.rs`
- `crates/moa-wire/src/turn.rs`
- `crates/moa-hands/src/core/policy.rs`
- `crates/moa-brain/src/pipeline/history/conversion.rs`
- `crates/moa-brain/src/compaction.rs`
- `crates/moa-session/src/store/dashboard.rs`
- `crates/moa-edge/src/routes/session_stream.rs`
- `crates/moa-orchestrator/src/action_reviews/app.rs`
- `crates/moa-orchestrator/src/action_reviews/store.rs`
- `crates/moa-orchestrator/src/services/action_reviews.rs`
- `crates/moa-orchestrator/src/services/action_policy.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-orchestrator/src/tool_invocation/governed.rs`
- `crates/moa-orchestrator/src/objects/session/mod.rs`
- `crates/moa-orchestrator/src/objects/session/handlers.rs`
- `crates/moa-orchestrator/src/objects/session/state.rs`
- `crates/moa-orchestrator/src/objects/worker/mod.rs`
- `crates/moa-orchestrator/src/objects/worker/handlers.rs`
- `crates/moa-orchestrator/src/objects/worker/state.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/mod.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/tools.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/tests.rs`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- affected `ActionEnvelope` fixtures in `crates/moa-core/`, `crates/moa-hands/`,
  `crates/moa-messaging/`, `crates/moa-session/`, `crates/moa-skills/`, and
  `crates/moa-orchestrator/tests/`
- `crates/moa-orchestrator/tests/action_policy_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/action_policy_flow_e2e.rs`
- `crates/moa-orchestrator/tests/tool_executor_service_e2e.rs`
- `crates/moa-orchestrator/tests/worker_coordination_service_e2e.rs`
- `crates/moa-orchestrator/tests/execution_run_service_e2e/routing.rs`
- focused Session-stream continuation coverage in the existing `moa-edge` test owner
- `docs/01-architecture-overview.md`
- `docs/02-brain-orchestration.md`
- `docs/03-communication-layer.md`
- `docs/05-session-event-log.md`
- `docs/08-security.md`
- `docs/12-restate-architecture.md`

**Implementation:**

1. Hard-replace `ActionEnvelope`'s optional session/worker/origin fields with exactly one
   `ActionReviewOwner`: `Coordinator { session_id, turn_id, generation }`, `Worker {
   session_id, worker_id, turn_id, generation }`, or `ExecutionTask { session_id,
   origin: ExecutionTaskOrigin }`. Remove fallback inference/aliases. Add required
   generation to root/worker turn requests; Session and Worker monotonically advance it
   on new user/worker follow-up admission, while review continuations retain the origin.
2. `ActionReviews/request` synchronously registers a conversational review on its typed
   owner before returning Pending. A Worker with a current-generation pending review
   remains nonterminal after its model loop: it does not resolve parent waiters, emit
   terminal delivery, schedule cleanup, or discard local history until resolution or
   supersession. Duplicate registration/resolution by review ID is a no-op.
3. Persist a typed conversational resolution receipt: `ClearedSuccess`,
   `ClearedToolError`, or `Denied`,
   containing review/tool IDs, owner, safe output/failure class, and exact terminal event
   sequence. Callback occurs only after `ActionReviewDecided` and the cleared tool's
   terminal `ToolResult`/`ToolError` are durable; recover an already-durable error receipt
   without re-execution, but send no callback after a pre-durability infrastructure
   failure. Mint a fresh MOA tool ID and clear the original `provider_tool_use_id` before
   reviewed execution.
4. Add neutral payload-safe `ActionReviewContinuationRequested`, deduped by
   `action_review_continuation:{review_id}`, carrying the dispatched continuation turn ID
   and typed receipt, and render it as a system directive. Compaction elides it or emits
   only a bounded safe class; dashboard summary contains only redacted review/turn IDs.
   Session SSE `follow_on_terminal_turn` retargets to that turn only for Coordinator
   owners so an origin-active stream observes the follow-up; Worker/ExecutionTask facts
   never retarget the contact stream. Add
   `TurnTrigger::ActionReview` plus required typed root/worker continuation context.
   Exact matrix: no fake UserMessage, no classifier/planner/tools/durable upgrade; root
   runs one bounded Respond call and emits at most one visible answer; worker runs one
   no-tools synthesis turn, updates local history/result, then resumes normal parent-
   result/cleanup ownership.
5. Fence and schedule by generation. A callback while its origin/another continuation is
   active queues once. A resolved same-generation continuation runs before ordinary FIFO
   only if no newer user/follow-up admission superseded it. Unresolved review never blocks
   later user messages; later admission, task-tree/worker cancellation, or failed owner
   generation makes it stale and releases worker lifecycle. Multiple reviews use durable
   registration event sequence then review ID order and receive one follow-up per review;
   replay neither inserts twice nor reorders user FIFO.
6. Keep `ExecutionTask` on its existing run/task/generation outbox/ack path and route zero
   callbacks to Session/Worker handlers. Timeout remains fail-closed with no automatic
   conversational resume; adding timeout delivery/reaper behavior is a separate durable
   outbox task. No migration is needed: typed owner stays in the existing envelope JSONB;
   the conversational receipt lives in the deduped continuation event payload, with VO
   state only a derived scheduling index. If the continuation fact is absent on replay,
   reconstruct it from durable `ActionReviewDecided` plus the exact `ToolResult`/
   `ToolError`, never by mutating envelope JSON. Generation/fences remain Restate VO
   state. Hard-break old persisted local shapes; add no `serde(default)` compatibility
   loader.

**Acceptance:** Coordinator and Worker × cleared success, cleared tool error, and denied
all register before origin completion and resume the exact owner once. Test origin-active,
same-generation idle/continuation-active, newer generation admitted/active, cancelled,
failed, duplicate replay, and multiple-review order. Assert exact counts for tool
execution, decision, terminal tool event, continuation fact/turn, root visible reply,
worker history/result, parent result, and cleanup. A pending-review worker cannot self-
complete. Cleared calls have a fresh MOA ID and no reused provider tool-use ID. Stale/
cancelled callbacks produce no continuation. ExecutionTask reviews produce zero
conversational callbacks; timeout produces none. No old owner fields, inferred owner,
compatibility state, fake message, planner/classifier/tool-enabled continuation remains.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-core --locked action_policy
cargo test -p moa-core --locked events
cargo test -p moa-wire --locked turn
cargo nextest run -p moa-hands --locked --test hands_offline -E 'test(/action|policy/)'
cargo nextest run -p moa-brain --locked --lib -E 'test(/history|action_review/)'
cargo nextest run -p moa-edge --locked -E 'test(/action_review|session_stream/)'
cargo test -p moa-orchestrator --lib --locked action_review
cargo test -p moa-orchestrator --lib --locked \
  pending_worker_reviews_hold_lifecycle_until_ordered_continuations_finish
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test action_policy_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test tool_executor_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test worker_coordination_service_e2e
cargo clippy -p moa-core -p moa-wire -p moa-hands -p moa-brain -p moa-messaging \
  -p moa-session -p moa-skills -p moa-orchestrator \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must remove generation comparison, deliver a callback twice, let
a pending-review worker self-clean, route ExecutionTask to Session, dispatch before the
terminal event is durable, preserve `provider_tool_use_id`, or enable planner/classifier/
tools on continuation. Each owner/generation/count/lifecycle/ID/trigger assertion must
fail before restoration and pass afterward. Also remove Coordinator SSE retargeting or
retarget a Worker fact; the focused stream assertions must fail before restoration.

## M2 — Secure Cloud and Credential Boundaries

Task 2.1 is parallel-safe with disjoint post-M0 work. Task 2.3 follows Task 2.1 because
both change runtime composition and is the next production task. Task 2.2 follows Task 1.3 because the action-review
continuation and security circuit share the durable ToolExecutor result boundary. Task
2.4 follows Task 2.3.

### Task 2.1 — Introduce explicit local/cloud security profiles [P0] [x]

**Depends on:** Task 0.2
**Why:** checked-in production selects no typed fail-closed posture even though the
security contract requires explicit tenant grants for cloud tool use.

**Files:**

- `crates/moa-config/src/security.rs`
- `crates/moa-config/src/lib.rs`
- `crates/moa-config/src/env_overlay/security.rs`
- `crates/moa-config/src/env_overlay/mod.rs`
- `crates/moa-config/src/env_overlay/providers.rs`
- `crates/moa-config/src/env_overlay/tests.rs`
- `crates/moa-config/src/sandbox.rs`
- `crates/moa-core/src/types/tools.rs`
- `crates/moa-security/src/policies.rs`
- `crates/moa-hands/src/core/construction.rs`
- `crates/moa-hands/src/core/registration.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-hands/tests/hands_offline/security_defaults.rs`
- `crates/moa-hands/tests/hands_offline/mcp_router.rs`
- `crates/moa-hands/tests/daytona_live.rs`
- `crates/moa-hands/tests/e2b_live.rs`
- `crates/moa-brain/examples/chat_harness.rs`
- `crates/moa-brain/tests/cache_audit_live.rs`
- `crates/moa-orchestrator/tests/experiment_trial_run_e2e.rs`
- `crates/moa-orchestrator/tests/behavior_lab_simulation_e2e.rs`
- `crates/moa-orchestrator/tests/experiment_agent_loop_e2e.rs`
- `crates/moa-orchestrator/tests/tenant_purge_service_e2e.rs`
- `crates/moa-test-support/src/orchestrator_fixture/process.rs`
- `k8s/base/15-runtime-config.yaml`
- `k8s/base/20-orchestrator-deployment.yaml`
- `k8s/overlays/production/kustomization.yaml`
- new `k8s/overlays/production/patches/runtime-security-profile.yaml`
- new `k8s/overlays/production/patches/orchestrator-security-profile.yaml`
- `k8s/scripts/smoke.sh`
- `docker-compose.yml`
- `.env.example`
- `scripts/run-clean-e2e.sh`
- `docs/08-security.md`
- `docs/23-environment-variables.md`
- `docs/eval/execution-honesty.md`

**Implementation:**

1. Add public serde-snake-case `SecurityProfile::{Local, Cloud}` as the single top-level
   `MoaConfig.security_profile`; `MOA_SECURITY_PROFILE` maps directly and defaults Local.
   Delete `CloudHandsConfig.allow_local_provider` and `MOA_CLOUD_HANDS_ALLOW_LOCAL`
   everywhere. Do not accept the old field/key through an alias.
2. Cloud requires unresolved `permissions.default_effect == Deny`. For an unmatched
   request, combine deployment and intrinsic tool defaults. For a matched persisted rule,
   take the strictest of that rule, intrinsic tool effect, and matching configured
   deny/admin-review overrides; deployment default is not a ceiling on an explicit
   low-risk scoped grant. A rule never makes a filtered/unregistered tool visible.
3. Return a typed safe decision source such as deployment default, tool definition,
   persisted rule, configured review, or configured deny. Logs identify only profile and
   policy/backend owner kind, never inputs, patterns, or secrets.
4. Hard-change `ToolRouter::from_config` to accept its optional rule-store owner during
   construction and update every caller. Cloud requires a real owner and rejects local,
   absent, missing-credential, or policy-incompatible sandbox routes before returning;
   Local may pass no store and use local hands. Do not retain an overload/wrapper for the
   old two-argument `from_config`; `with_rule_store` may remain for `new`/`new_local`
   local and test assembly, but cloud construction cannot use it as a later escape hatch.
5. Render base/local explicitly as Local+Allow+local provider and production as
   Cloud+Deny+E2B. Move the E2B secret reference out of base into production. Extend the
   Kubernetes smoke script to assert both contracts and absence of the deleted key.

**Acceptance:** unmatched Cloud Deny plus tool Allow is denied from deployment default;
same-tenant/same-operation persisted Allow plus tool Allow and no override is allowed from
the rule, while another operation/tenant stays denied; persisted Allow plus intrinsic
AdminReview/Deny remains review/deny; configured review/deny cannot be weakened; filtered
tools stay unavailable. Local works with local hands and no rule owner. Cloud rejects
Allow default, missing owner, local/absent backend, and missing selected credentials before
serving. Local/production renders contain exactly their intended profile/default/provider,
and the deleted field/env key has no residue.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-config --locked security_profile
cargo test -p moa-security --locked policies
cargo nextest run -p moa-hands --locked --test hands_offline -E 'test(security_defaults::)'
cargo test -p moa-orchestrator --locked runtime::deps
kubectl kustomize k8s/overlays/local >/tmp/moa-local.yaml
kubectl kustomize k8s/overlays/production >/tmp/moa-production.yaml
rg '^  MOA_SECURITY_PROFILE: local$|^  MOA_PERMISSIONS_DEFAULT_EFFECT: allow$' /tmp/moa-local.yaml
rg '^  MOA_SECURITY_PROFILE: cloud$|^  MOA_PERMISSIONS_DEFAULT_EFFECT: deny$' /tmp/moa-production.yaml
./k8s/scripts/smoke.sh --validate-manifests
! rg -n 'MOA_CLOUD_HANDS_ALLOW_LOCAL|allow_local_provider' \
  crates scripts k8s docs docker-compose.yml \
  --glob '!docs/engineering-discipline/plans/**'
cargo clippy -p moa-config -p moa-core -p moa-security -p moa-hands \
  -p moa-orchestrator -p moa-test-support \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must weaken Cloud to Allow, omit intrinsic tool effect from a
matched-rule decision, allow Cloud local/no-backend construction, allow Cloud without a
rule owner, and remove one rendered production key. Each focused assertion must fail
before restoration and pass afterward.

### Task 2.2 — Add a typed prompt-injection security circuit [P0] [ ]

**Depends on:** Tasks 0.2 and 1.3
**Why:** generic caps do not reliably stop varied malicious attempts, disable the
attacked capability, or create a security incident trail.

**Files:**

- `crates/moa-core/src/types/security.rs`
- `crates/moa-core/src/types/tools.rs`
- `crates/moa-core/src/types/action_policy.rs`
- `crates/moa-core/src/types/contact.rs`
- `crates/moa-core/src/types/worker/state.rs`
- `crates/moa-core/src/events.rs`
- `crates/moa-wire/src/turn.rs`
- `crates/moa-security/src/injection.rs`
- `crates/moa-security/src/lib.rs`
- `crates/moa-brain/src/harness/tool_dispatch.rs`
- `crates/moa-brain/tests/brain_turn_offline.rs`
- `crates/moa-hands/src/core/dispatch.rs`
- `crates/moa-hands/src/core/output_budget.rs`
- `crates/moa-hands/src/core/recovery.rs`
- `crates/moa-hands/src/core/recovery/tests.rs`
- `crates/moa-hands/src/core/telemetry.rs`
- new tool-output-security coverage under `crates/moa-hands/tests/hands_offline/`
- `crates/moa-hands/tests/hands_offline/local_tools_offline.rs`
- `crates/moa-hands/tests/hands_offline/mcp_router.rs`
- `crates/moa-hands/tests/hands_offline/security_defaults.rs`
- `crates/moa-hands/tests/session_search_db.rs`
- `crates/moa-hands/tests/daytona_live.rs`
- `crates/moa-hands/tests/e2b_live.rs`
- `crates/moa-orchestrator/src/tool_invocation/governed.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- new `crates/moa-orchestrator/src/services/security_events.rs`
- `crates/moa-orchestrator/src/services/action_reviews.rs`
- `crates/moa-orchestrator/src/objects/mod.rs`
- `crates/moa-orchestrator/src/objects/session/state.rs`
- `crates/moa-orchestrator/src/objects/session/mod.rs`
- `crates/moa-orchestrator/src/objects/session/handlers.rs`
- `crates/moa-orchestrator/src/objects/worker/state.rs`
- `crates/moa-orchestrator/src/objects/worker/mod.rs`
- `crates/moa-orchestrator/src/objects/worker/handlers.rs`
- `crates/moa-orchestrator/src/workflows/turn_events.rs`
- `crates/moa-orchestrator/src/workflows/turn_responsiveness.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/mod.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/tools.rs`
- `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- `crates/moa-orchestrator/src/workflows/execution_task.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-execution/src/wire.rs`
- `crates/moa-ocsf/src/classes.rs`
- `crates/moa-ocsf/src/enums.rs`
- `crates/moa-ocsf/src/emit.rs`
- `crates/moa-ocsf/src/lib.rs`
- `crates/moa-ocsf/tests/ocsf_db.rs`
- new `crates/moa-ocsf/tests/ocsf_db/prompt_injection_finding_db.rs`
- `crates/moa-orchestrator/tests/orchestrator_offline/tool_executor.rs`
- `crates/moa-orchestrator/tests/orchestrator_offline/session_vo.rs`
- `crates/moa-orchestrator/tests/tool_executor_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/tool_executor_e2e.rs`
- `crates/moa-orchestrator/tests/action_policy_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/action_policy_flow_e2e.rs`
- `crates/moa-orchestrator/tests/turn_responsiveness_service_e2e.rs`
- `crates/moa-orchestrator/tests/integration/turn_responsiveness_e2e.rs`
- `crates/moa-orchestrator/tests/worker_coordination_service_e2e.rs`
- `crates/moa-orchestrator/tests/execution_run_service_e2e.rs`
- `crates/moa-orchestrator/tests/execution_run_service_e2e/task_lifecycle.rs`
- `crates/moa-orchestrator/tests/session_reply_matrix_service_e2e.rs`
- every exhaustive `Event::ToolResult` consumer surfaced by
  `rg -l 'Event::ToolResult' crates -g '*.rs'`
- `docs/01-architecture-overview.md`
- `docs/02-brain-orchestration.md`
- `docs/05-session-event-log.md`
- `docs/08-security.md`
- `docs/12-restate-architecture.md`
- `docs/operations/ocsf-audit.md`

**Implementation:**

1. Define shared serialized assessment class, detector revision, exact owner, canonical
   capability ID, stage, state, signal, transition, and processed-assessment identity in
   `moa-core::types::security`. Define required `SecuredToolOutput` in
   `moa-core::types::tools`; durable `ToolResult` and reviewed success/error receipts
   carry the required safe output, assessment, and canonical capability ID. Security
   metadata is never optional. `moa-security` owns one pure carrier-aware classifier and
   transition function so core/event types do not depend back on it. The mandatory
   versioned policy has no opt-out config or compatibility mode.
2. Invoke that classifier exactly once at each raw-output source: in `moa-hands`
   immediately after every built-in/Hand/MCP provider return, and in the trusted-file
   branch that bypasses the router. Classify recovery-created error output too. This
   occurs before telemetry, `apply_output_budget`, artifactization, persistence, traces,
   or logging of provider text. Redact matched suspicious spans; for every non-safe class
   clear structured output and artifact references; confirmed/canary/secret output uses
   one fixed safe replacement. Deduplicate identical bytes repeated across content,
   structured values, stdout, stderr, and error carriers so duplication cannot multiply
   the score. Pre-classification diagnostics use stable error-class labels only.
3. Replace every router/executor API returning bare `ToolOutput` or
   `(Option<String>, ToolOutput)` with the required secured envelope. The router returns
   the canonical resolved capability identity, never a caller-supplied identity:
   `builtin:<tool>`, `mcp:<server>:<tool>`, or one logical Hand capability independent
   of fallback provider. Classification must happen inside the ToolExecutor `ctx.run`
   closure before Restate journals its return. Workflows, ActionReviews, the harness,
   and all `Event::ToolResult` consumers consume the envelope without reclassification.
   Reviewed success and error continuations preserve its assessment/capability metadata;
   ActionReviews never derives `safe_output` by truncating raw bytes. Delete the harness
   `SecuredToolOutput`, output use of `inspect_input`/`InputInspection`, every bare/raw
   overload, and every deprecated wrapper.
4. Keep serialized circuit state with generation-fenced owners:
   `Coordinator { turn_id, generation }`, `Worker { worker_id, turn_id, generation }`,
   or `ExecutionTask { run_uid, task_uid, generation }`. Persist the exact active
   workflow projection as `active_turn_id + active_turn_generation` and the logical
   `active_security_owner` separately. Never use the admission allocator
   `action_review_generation` as the active-owner fence: queuing later Session/Worker
   work may advance it while the current turn is still live. Session and Worker VOs own
   per-current-generation capability maps deduplicated by triggering `ToolCallId`; their
   internal atomic apply-assessment handlers return the exact replay-stable transition.
   The capability key is owner plus the router-resolved canonical capability ID. Delayed
   approved/denied/errored ActionReview continuation from Task 1.3 may use a new
   continuation workflow ID but retains and feeds the original logical live owner. State
   resets only for a genuinely new owner generation, never for a new input
   fingerprint, tool argument, fallback Hand provider, or workflow replay.
5. Add `CoordinatorInput { turn_id, generation, input_request_id }` to public and pending
   reply-target enums. Session VO state registers the pending coordinator awakeable and
   owns exact selection, delivery, acknowledgement, replay, ambiguity,
   stale-generation, and cancellation behavior. Coordinator score-3 `NeedsInput` uses
   this target and idles until its reply. Worker `NeedsInput` reuses the existing Worker
   input awakeable/signal machinery with one fixed safe security question. This is a new
   hard contract, not a shim for the existing ExecutionConfirmation/ExecutionInput/
   WorkerInput targets.
   Strengthen WorkerInput ownership with owner turn/generation and waiting workflow ID;
   timeout, worker cancellation, Session child removal, task-tree cancellation, and
   terminal owner outcome must clear the exact Session target/mapping. Preserve delivery
   history so a late reply cannot resolve a replacement awakeable.
6. Use these exact typed assessment classes and additive owner/capability score:
   `Safe = 0`, `SuspiciousInstruction = 1`, `ConfirmedInjection = 2`, `CanaryLeak = 4`,
   and `RestrictedOrSecretOutput = 4`. Suspicious matched spans are replaced immediately;
   ConfirmedInjection, CanaryLeak, and RestrictedOrSecretOutput clear every raw carrier
   regardless of current score. Score 1 emits one warning, score 2 disables the
   capability, score 3 suspends for `NeedsInput`, and score >=4 halts. Only the first
   highest stage reached by one assessment transitions: a 0-to-4 CanaryLeak or
   RestrictedOrSecretOutput emits exactly one Halt, not warning/disable/NeedsInput first.
   Repetition fingerprints remain unrelated.
7. Owner outcomes are exact. Coordinator `NeedsInput` registers the new generation-
   fenced Session coordinator-input target and idles until its reply; coordinator halt
   records the canonical actor+turn
   `TurnFailed`. Worker `NeedsInput` emits one `WorkerSignal { kind: NeedsInput,
   input_audience: User }` and awaits its awakeable; worker halt emits one `Failed` signal
   and terminates that worker turn. ExecutionTask `NeedsInput` returns
   `ExecutionTaskResult::NeedsInput { audience: User }`; halt returns
   `ExecutionTaskResult::Failed { class: Terminal }`. Worker/task circuit facts are
   neutral in shared Session history; their existing signals/task outcomes own suspension
   or termination. Warning/disable facts are neutral for every owner. Disabled
   capabilities cannot dispatch later and classification never broadens authorization.
   ExecutionTask agent turns receive a per-task-turn canary and pass it to every
   capability invocation.
8. Add typed, neutral `Event::PromptInjectionCircuitTransition` with no raw output. The
   owner journals one timestamp before applying the transition, persists the Session
   transition with one replay-stable key shaped exactly as
   `prompt_injection_circuit:v1:<64 lowercase blake3 hex>`, then synchronously calls an internal security-
   event service before applying the owner outcome. Emit an OCSF v1.3 Detection Finding:
   `category_uid=2`, `class_uid=2004`, `activity_id=1`, `type_uid=200401`, required
   `finding_info.uid=transition_key`, fixed safe title/description, and deterministic
   stage severity. Derive the transition digest from canonical JSON containing schema
   version, Session ID, exact owner, canonical capability ID, `ToolCallId`, prior stage,
   and reached stage under domain `moa.prompt-injection-circuit.transition.v1`. Derive
   the event UUID with UUIDv5 from a fixed namespace and the transition key. The existing
   `security_events.id` primary key is sufficient; add no migration and leave retrieval
   idempotency null. On primary-key conflict load the existing row and require the same
   tenant, occurrence timestamp, and canonical JCS payload, verifying its HMAC with the
   stored signing key even after key rotation; return replay conflict on drift. Do not
   reuse retrieval idempotency or generate identity/time during replay.
9. Replace prompt-injection uses of generic `Event::Warning`; persist only safe class,
   detector/policy revision, owner/capability identifiers, transition, and counts. Unsafe
   bytes never enter ToolResult, model context, events, OCSF, errors, traces, or logs.

**Acceptance:** root, worker, execution-agent, and delayed reviewed-tool paths inspect
router/trusted-file/success/error outputs through one classifier. Varied malicious output
with different inputs trips before generic caps; replay/crash produces exactly one
Session transition and one signed OCSF finding, and new input fingerprints cannot reset
the circuit. Capability disable prevents later dispatch; each owner produces the exact
generation-fenced `NeedsInput` or halt effect. Coordinator input registration/delivery,
ambiguity, replay, acknowledgement, cancellation, and stale generation are exact.
Benign repetition, duplicate carriers, and benign suspicious-looking prose stay below
threshold. Hand fallback cannot evade one logical capability key. Raw malicious bytes
are absent from ToolResult, reviewed receipts, model context, Session events, OCSF,
errors, artifacts, telemetry, and traces. Bare executor responses, harness-local
wrappers, output-as-input inspection, raw-output truncation called `safe_output`, and
generic prompt-injection warnings have no residue.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-core --locked events
cargo test -p moa-core --locked types::security
cargo test -p moa-wire --locked pending_user_reply_target
cargo test -p moa-security --locked injection
cargo nextest run -p moa-hands --locked --test hands_offline \
  -E 'test(/tool_output_security|output_budget/)'
cargo nextest run -p moa-brain --locked --features eval-harness \
  --test brain_turn_offline -E 'test(/prompt_injection|tool_output_security/)'
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline \
  -E 'test(/coordinator_input|tool_executor|security_circuit/)'
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test tool_executor_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test turn_responsiveness_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test worker_coordination_service_e2e
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test action_policy_service_e2e -E 'test(/prompt_injection|security_circuit/)'
MOA_RUN_SESSION_REPLY_MATRIX_SERVICE_E2E=1 \
  cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test session_reply_matrix_service_e2e \
  -E 'test(/coordinator_input|security_circuit/)'
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test execution_run_service_e2e -E 'test(/prompt_injection/)'
cargo nextest run -p moa-ocsf --locked --test ocsf_db \
  -E 'test(/prompt_injection/)'
cargo clippy -p moa-core -p moa-security -p moa-brain -p moa-ocsf \
  -p moa-hands -p moa-orchestrator --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
! rg -n 'struct SecuredToolOutput|type SecuredToolOutput|InputInspection|inspect_input' \
  crates/moa-brain/src/harness/tool_dispatch.rs
! rg -n 'Result<Json<ToolOutput>|Result<\(Option<String>, ToolOutput\)>' \
  crates/moa-orchestrator/src/services/tool_executor.rs crates/moa-hands/src/core
! rg -n 'classified as .*signals|tool output for' \
  crates/moa-brain/src/harness/tool_dispatch.rs
! rg -n 'safe_output: output|output\.to_text\(\).*safe_output' \
  crates/moa-orchestrator/src/services/action_reviews.rs
git diff --check
```

Mutation verification must artifactize before classification, retain one raw structured/
artifact carrier, skip the trusted-file branch, return bare/raw output, or restore raw
output in ToolResult/model context/trace. It must also remove owner generation, reset
state on a new input fingerprint, let Hand fallback change capability identity, allow
dispatch after disable, generate a fresh timestamp/OCSF ID, accept a conflicting signed
payload, validate a conflicting row with a newly active rather than stored signing key,
or append unsafe bytes to one durable/model surface. Also emit intermediate transitions
on a 0-to-4 score jump. The malicious-artifact store count, stale review/input,
varied-input circuit, dispatch count, owner-specific transition, jump, deterministic
identity, replay-conflict, and secret-leak assertions must each fail before restoration
and pass afterward.

### Task 2.3 — Add one durable encrypted tenant credential owner [P0] [x]

**Depends on:** Task 2.1
**Why:** MOA-managed connector material is currently process-local or freshly empty in
different service/workflow instances, so restart, replica, and rotation behavior is not
durable enough for either knowledge or tenant MCP connections.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000346__tenant_credential_vault.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- new `crates/moa-auth/providers/src/postgres_credential_vault.rs`
- `crates/moa-auth/providers/src/lib.rs`
- `crates/moa-auth/providers/Cargo.toml`
- `crates/moa-auth/providers/tests/auth_providers_db.rs`
- new `crates/moa-auth/providers/tests/auth_providers_db/tenant_credential_vault_db.rs`
- `crates/moa-auth/providers/tests/auth_providers_db/support/mod.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-core/src/types/mod.rs`
- new `crates/moa-core/src/types/credentials.rs`
- `crates/moa-core/src/types/model.rs`
- `crates/moa-security/src/mcp_proxy.rs`
- `crates/moa-security/src/lib.rs`
- `crates/moa-hands/src/core/construction.rs`
- `crates/moa-hands/src/core/dispatch.rs`
- `crates/moa-hands/tests/hands_offline/mcp_router.rs`
- `crates/moa-messaging/src/delivery.rs`
- `crates/moa-messaging/src/lib.rs`
- `crates/moa-messaging/src/postmark.rs`
- `crates/moa-messaging/src/twilio.rs`
- `crates/moa-messaging/tests/messaging_offline/delivery_offline.rs`
- `crates/moa-messaging/tests/messaging_offline/postmark_offline.rs`
- `crates/moa-messaging/tests/messaging_offline/twilio_offline.rs`
- `crates/moa-messaging/tests/postmark_provider_e2e.rs`
- `crates/moa-messaging/tests/twilio_provider_e2e.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-orchestrator/src/main.rs`
- `crates/moa-orchestrator/src/services/knowledge/mod.rs`
- `crates/moa-orchestrator/src/services/knowledge/link.rs`
- `crates/moa-orchestrator/src/services/knowledge/sync.rs`
- `crates/moa-orchestrator/src/services/contacts.rs`
- `crates/moa-orchestrator/src/workflows/knowledge_sync_ingestion.rs`
- `crates/moa-orchestrator/src/workflows/tenant_purge/repository.rs`
- `crates/moa-orchestrator/tests/knowledge_service/connections.rs`
- `crates/moa-orchestrator/tests/knowledge_service/ingestion.rs`
- `crates/moa-orchestrator/tests/orchestrator_db_memory/tenant_purge_repository_db_memory.rs`
- `crates/moa-knowledge/Cargo.toml`
- `crates/moa-knowledge/src/domain/connection.rs`
- `crates/moa-knowledge/src/domain/provider.rs`
- `crates/moa-knowledge/src/domain/sync.rs`
- `crates/moa-knowledge/src/normalize.rs`
- `crates/moa-knowledge/src/providers/mod.rs`
- `crates/moa-knowledge/src/providers/merge.rs`
- `crates/moa-knowledge/src/providers/nango/mod.rs`
- `crates/moa-knowledge/src/repository/connection.rs`
- `crates/moa-knowledge/src/repository/row_mapping.rs`
- `crates/moa-knowledge/tests/knowledge_offline/provider_merge.rs`
- `crates/moa-knowledge/tests/knowledge_offline/provider_nango.rs`
- compile-migration fixtures under `crates/moa-knowledge/tests/knowledge_db_memory/`
- compile-migration fixtures under `crates/moa-knowledge/tests/knowledge_offline/`
- `crates/moa-knowledge/tests/provider_live.rs`
- `crates/moa-orchestrator/tests/knowledge_service/support.rs`
- `crates/moa-edge/src/routes/auth_accounts.rs`
- `crates/moa-edge/src/routes/knowledge.rs`
- `crates/moa-edge/src/routes/tenant_accounts/application.rs`
- `crates/moa-wire/src/knowledge.rs`
- `crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs`
- `crates/moa-brain/tests/brain_db_memory/tenant_contact_knowledge_retrieval_db_memory.rs`
- `crates/moa-memory/vector/tests/memory_vector_db_memory/vector_sync_outbox_db_memory.rs`
- `crates/xtask/src/wixqa_rag_eval.rs`
- `Cargo.lock`
- `docs/01-architecture-overview.md`
- `docs/06-hands-and-mcp.md`
- `docs/08-security.md`
- `docs/10-technology-stack.md`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Hard-replace `CredentialVault`'s generic `(service, scope)` address and every caller
   with typed credential persistence/reference/context types. Persistence identity carries
   tenant, owning connection, credential kind, and exact version; resolution separately
   carries the acting principal/delegation and requested operation. Owner-principal data
   is authorization metadata, not privacy-subject ownership. Model deployment-owned
   Email/SMS and other operator secrets through a distinct typed deployment-secret source
   if they do not have a tenant connection; never encode either boundary in free-form
   strings or retain an adapter for the old API.
2. Add V000346 as a full forward migration for an encrypted versioned credential table
   and append-only, secret-free operation audit. Register ownership and migration
   discovery, forced RLS, tenant/connection/kind/version checks, one-active-version
   constraints, rotation/revocation state, indexes, and idempotent bootstrap/upgrade
   coverage. Store ciphertext plus KMS metadata only. Do not add an FK that makes the
   auth-provider DB harness depend on the knowledge schema; enforce the connection
   lifecycle through the typed owner and orchestrator transaction/workflow boundary.
3. Implement `PostgresCredentialVault` beside the existing Postgres token vault in
   `moa-auth-providers`, reusing its `ScopedConn`, encryption, and KMS patterns without
   treating the user/OAuth-shaped token table as generic credential storage. Use
   caller-supplied idempotency/operation IDs, canonical request hashes, compare-and-swap
   version checks, and deterministic secret-free audit keys so every operation is
   replay-safe. Every resolve context also carries a replay-stable operation ID and hash;
   its audit insert commits before plaintext is returned. Same ID/hash replays one row;
   same ID with a changed selector or operation is a typed conflict. The provider owns
   its DB pool and never borrows a request transaction. Ordinary roles cannot update or
   delete append-only audit rows; only the narrowly scoped tenant-purge owner can remove
   them through its controlled lifecycle path. Hard-remove the unused public credential
   `list` operation from the trait and Postgres owner: exposing a caller-selected tenant
   enumeration surface is unnecessary and cannot remain as an unaudited authorization
   exception.
4. Construct exactly one shared `Arc<dyn CredentialVault>` in runtime composition and
   inject it through `RuntimeDeps`, endpoint/main assembly, `KnowledgeService`, trusted
   content fetching, and the durable knowledge-sync workflow. Delete fresh empty env
   vaults and install-once/global lookup paths; reconstructed workflows resolve through
   the same durable owner.
5. Generate `connection_uid` before storing credentials and make link/re-link one
   operation-ID-fenced reserve/claim/finalize flow so concurrent or replayed links cannot
   orphan a vault version, attach it to a different connection, or overwrite a newer
   claim. Persist a forced-RLS claim keyed by tenant plus operation with the canonical
   request hash, owner, expected connection reference, exact previous-active reference,
   and exact candidate reference. Use explicit `reserved`, `credential_written`,
   `compensating`, `compensated`, and `finalized` transitions guarded by compare-and-swap.
   Run source selection and initial sync with the claimed candidate before atomically
   finalizing the connection and claim. On any post-write/pre-finalize failure, durably
   enter `compensating` before revoking only that candidate; restore the exact previous
   version only when the candidate is still active and no newer version exists, then mark
   the claim `compensated`. Replay resumes incomplete states, returns a finalized result
   once, and treats compensated operations as terminal without touching a newer claim.
   A persisted queued sync run is not evidence that provider dispatch occurred: persist
   an idempotent trigger boundary and make the owning link operation resume credential
   resolution/provider trigger after a crash between run creation and dispatch. Atomic
   link finalization requires durable evidence that the candidate's initial provider
   trigger completed, never merely `AlreadyRunning`.
   Implement that boundary against each provider's real contract. Nango initial link uses
   naturally idempotent `/sync/start`; its one-off `/sync/trigger` is not idempotent.
   Merge initial link must not call its plan-gated, credit-consuming force-resync endpoint:
   Merge starts initial sync automatically, so replay performs a read-only, category-
   correct sync-status reconciliation and accepts only an unambiguous active/completed
   provider state. Follow Merge's documented readiness rule: completion is proven by
   `status = DONE` or `is_initial_sync = false`; skip disabled models when evaluating
   readiness, while failing closed on relevant failed, paused, or malformed states.
   Preserve the exact Merge category selected by the link operation,
   validate it against `integration.categories`, and use category/versioned endpoints.
   Do not claim support from an undocumented `Idempotency-Key` header.
   Keep
   `credential_ref` opaque in `KnowledgeConnection`, domain/provider merge types, Restate
   state, events, and API payloads. Hand plaintext to the provider only as a non-
   serializable, redacted secret value immediately before the authorized outbound
   request; it must be impossible to serialize, format, clone into a model payload, or
   persist the plaintext accidentally.
6. Authorize tenant connection and requested operation before resolving any secret.
   Caller lifecycle routes use the identity returned by
   `require_authz_with_delegation(ObjectType::Tenant, tenant_id, Relation::Operator)`;
   create stamps that principal as owner, while rotate/revoke/delete require the owner or
   its explicit delegation. Durable knowledge resolution uses a closed typed service-
   actor allowlist for exact tenant/connection-bound operations such as sync listing and
   content fetch, never a general service bypass or caller-shaped string. Do not invent a
   connection OpenFGA object unless this task also updates the authz schema/model.
   Cross-tenant, wrong-owner, wrong-kind, stale-version, revoked, and unauthorized-
   operation resolution all fail with safe typed errors before provider dispatch. Prove
   forced RLS as `moa_app`/`SET ROLE moa_app` for correct, missing, and wrong
   `moa.tenant_id`; owner-URL tests alone are insufficient.
7. Make credentials tenant-lifecycle owned: tenant purge removes every credential version
   and its permitted audit projection with resumable/idempotent semantics. Do not attach
   credential deletion to privacy-subject erasure merely because an owner principal is
   recorded. Audit create/resolve/rotate/revoke/delete without request payloads, plaintext,
   ciphertext, token fragments, or provider error bodies.
8. Remove the old trait methods, environment-vault implementations, call signatures,
   fixtures, and serialized address vocabulary in the same change. Add no aliases,
   deprecated wrappers, dual lookup, or fallback to deployment credentials for a tenant
   connection.

**Acceptance:** a credential created on replica A resolves on replica B and after
workflow reconstruction/restart through the single runtime owner; knowledge link/sync
and trusted content fetch use the same opaque reference. Create/rotate/revoke/delete are
idempotent under replay and concurrent rotation uses CAS; old/revoked versions are
unusable. Authorization plus forced RLS and typed selectors prevent cross-tenant,
cross-connection, wrong-owner/kind/version, and forbidden-operation resolution. Tenant
purge removes credential state without conflating owner metadata with subject erasure.
No Restate state, event, audit row, API/error/log, fixture, debug output, or knowledge row
contains plaintext or ciphertext. V000346 upgrades and bootstraps idempotently. The old
`(service, scope)` methods, address vocabulary, env-vault owners, globals, and fallback
paths no longer compile or appear in production/tests. No public credential-enumeration
operation remains. A crash after durable sync-run creation but before provider dispatch
replays that exact trigger and cannot finalize the link until trigger completion is
durable.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(tenant_credential_vault_v000346_fresh_and_idempotent_db)'
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-auth-providers --locked --test auth_providers_db
cargo nextest run -p moa-security --locked
cargo nextest run -p moa-hands --locked --test hands_offline -E 'test(mcp_router::)'
cargo nextest run -p moa-messaging --locked --test messaging_offline
cargo nextest run -p moa-edge --locked
cargo nextest run -p moa-orchestrator --locked --test knowledge_service
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db_memory \
  -E 'test(tenant_purge_repository)'
cargo nextest run -p moa-knowledge --locked --test knowledge_offline -E 'test(provider_merge::)'
cargo nextest run -p moa-knowledge --locked --test knowledge_db_memory
cargo nextest run -p moa-brain --locked --test brain_db_memory \
  -E 'test(/hybrid_retrieval|tenant_contact_knowledge/)'
cargo nextest run -p moa-memory-vector --locked --test memory_vector_db_memory \
  -E 'test(vector_sync_outbox)'
cargo test -p xtask --locked --features eval-tools wixqa
make test-authz-pentest
cargo clippy -p moa-core -p moa-auth-providers -p moa-security -p moa-hands \
  -p moa-messaging -p moa-knowledge -p moa-wire -p moa-brain -p moa-memory-vector \
  -p moa-edge -p moa-orchestrator -p moa-migrations -p xtask \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
! rg -n 'StoredCredentialMetadata|EnvironmentCredentialVault|EnvironmentDeliveryCredentialVault|credential_service|credential_scope|parse_credential_ref|vault://knowledge' \
  crates --glob '!**/target/**'
! rg -n 'CredentialOperation::List|List tenant credential metadata' crates --glob '!**/target/**'
! rg -U -n 'async fn delete\([^)]*(service|scope)' crates/moa-core/src/traits/mod.rs
git diff --check
```

Mutation verification must bypass tenant/operation authorization, resolve a revoked or
stale version, reuse an idempotency key with different material, serialize the secret
handoff, create a fresh workflow-local vault, and omit tenant-purge deletion. The
corresponding cross-tenant/rotation/conflict/secret-scan/reconstruction/purge assertions
must fail before each restoration and pass afterward. Migration verification uses a
fresh isolated database; do not wipe the long-running Compose database without explicit
approval. Also drop/relax the forced-RLS policy and return a resolved secret before its
audit commit; the application-role RLS and resolve-audit assertions must fail before
restoration and pass afterward. Remove the durable compensation call and prove the
post-write failure test leaves an unclaimed active candidate; restore it and prove crash
replay at every claim transition, failed relink restoration, and concurrent-newer
survival all pass. Inject a crash after sync-run claim and before provider dispatch; replay
must perform the exact idempotent trigger before finalization, and removing that resume
must fail the assertion. Prove missing and wrong tenant RLS context denies both audit and
claim-table access through production DB paths.

### Task 2.4 — Scope MCP credentials to tenant-owned connections [P0] [ ]

**Depends on:** Tasks 2.1 and 2.3
**Why:** one deployment credential can currently serve every tenant invoking an MCP
server, preventing least privilege and safe independent rotation.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000349__tenant_mcp_connection_bindings.sql`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-config/src/sandbox.rs`
- `crates/moa-security/src/mcp_proxy.rs`
- new `crates/moa-hands/src/core/mcp_connections.rs`
- `crates/moa-hands/src/core/mod.rs`
- `crates/moa-hands/src/core/registration.rs`
- `crates/moa-hands/src/core/construction.rs`
- `crates/moa-hands/src/core/dispatch.rs`
- `crates/moa-hands/src/core/policy.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-hands/tests/hands_offline/mcp_router.rs`
- new `crates/moa-hands/tests/hands_db.rs`
- new `crates/moa-hands/tests/hands_db/tenant_mcp_connection_db.rs`
- `crates/moa-edge/tests/direct_read_routes_db/mcp_db.rs`

**Implementation:**

1. Add the missing secret-free binding owner. V000349 persists one forced-RLS active or
   disabled `(tenant, connection, server)` row with an exact serialized Task 2.3
   `CredentialReference` and closed operation allowlist. The unqualified table belongs
   to `moa-hands`, uses the current migration schema, and has a partial unique active
   `(tenant_id, server_name)` index.
2. Hard-cut every MCP server configuration and registered tool to required
   `McpServerCredentialScope::{DeploymentOwned, TenantOwned}`, and every invocation to
   required
   `ToolCredentialScope::{NonMcp, DeploymentOwnedMcp, TenantOwnedMcp}`. Add no
   `Default`, `Option`, serde default, alias, or old constructor.
3. Inject delegated tenant-operator authorization before the first binding read, then
   require exact tenant, connection, server, canonical operation, active status, and
   credential-reference agreement. The real ToolExecutor/ToolRouter path carries the
   replay-stable tool-call operation identity; no test-only dispatch helper is accepted.
4. Resolve the exact Task 2.3 version through the durable vault only inside the trusted
   MCP proxy, shape plaintext headers immediately before `tools/call`, and retain only
   payload-safe opaque metadata in Restate/action-review state, events, logs, traces,
   and model context.
5. Environment credentials remain available only to explicitly deployment-owned
   connectors. Tenant-owned construction rejects them, and every authorization,
   binding, vault, header, egress, or provider failure returns without consulting the
   deployment branch.

**Acceptance:** two tenants sharing a server use only their credentials; cross-tenant,
stale-version, disabled/unknown connection, server/reference drift, missing-delegation,
and forbidden-operation cases fail before dispatch. Updating the binding to version
N+1 affects the next call without restart, serialized dispatch metadata contains no
secret, tenant-owned construction rejects deployment selectors, and no tenant-owned
error can fall back to deployment credentials.

**Verification:**

```bash
cargo test -p moa-security --locked mcp_proxy
cargo nextest run -p moa-hands --locked --test hands_offline
cargo nextest run -p moa-edge --locked --test direct_read_routes_db
make test-authz-pentest
```

## M3 — Safe Enterprise Knowledge

### Task 3.1 — Separate graph occurrence identity from content identity [P0] [x]

**Depends on:** Tasks 0.2 and 2.3
**Why:** equal text in different documents can collapse to one graph node, cite the
wrong source, and let one document's deletion affect another occurrence.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000347__knowledge_graph_occurrences.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-knowledge/src/domain/document.rs`
- `crates/moa-knowledge/src/chunking.rs`
- `crates/moa-knowledge/src/graph_delta.rs`
- `crates/moa-knowledge/src/semantic_graph.rs`
- `crates/moa-knowledge/src/semantic_graph_model.rs`
- `crates/moa-knowledge/src/ingestion/graph_writer.rs`
- `crates/moa-knowledge/src/ingestion/materialization.rs`
- `crates/moa-knowledge/src/ingestion/record.rs`
- `crates/moa-knowledge/src/ingestion/steps.rs`
- `crates/moa-knowledge/src/repository/mod.rs`
- `crates/moa-knowledge/src/repository/document.rs`
- `crates/moa-retrieval/src/retrieval/hydration.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/ingestion_pipeline_db_memory/semantic_graph.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/ingestion_pipeline_db_memory/deletion.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/ingestion_pipeline_db_memory/mod.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/repository_db_memory.rs`
- `crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs`
- `crates/moa-wire/src/knowledge.rs`
- `crates/moa-orchestrator/src/services/knowledge/inspect.rs`
- `crates/moa-orchestrator/tests/knowledge_service/inspection.rs`
- `crates/moa-orchestrator/tests/knowledge_service/ingestion.rs`
- `crates/moa-orchestrator/tests/knowledge_service/support.rs`
- `crates/moa-orchestrator/tests/knowledge_service/trace.rs`
- `docs/04-memory-architecture.md`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Make `graph_node_uid = chunk_uid` the single invariant for every knowledge chunk.
   `chunk_uid` is already deterministic from document version, ordinal, and content seed.
   Hard-change the Rust/domain shape from nullable graph identity and make V000347
   enforce `graph_node_uid NOT NULL` plus equality to `chunk_uid`; do not introduce a
   derivative, alias, nullable compatibility field, or dual identity. Once identity is
   mandatory, keep `object_ingestion_completed_since(...)` as the authoritative durable
   materialization-completion fence; do not replace it with a now-meaningless optional-
   UID presence check.
2. V000347 first creates one occurrence node for every legacy chunk, including nullable
   and shared references, preserving each occurrence's tenant/storage partition and
   active/tombstoned state. Clone occurrence-specific containment, provenance, semantic,
   and evidence edges and rewire chunk/vector references; entity/fact nodes may remain
   shared. Then clone each current embedding beneath every occurrence UID while
   preserving model/version, validity, tenant, and storage partition.
3. Queue external-vector upserts for new occurrence UIDs and deletions for retired shared
   UIDs through `vector_sync_outbox`. Retire content-hash chunk nodes only after all
   references, edges, embeddings, and outbox rows exist. Backfill, clean bootstrap, and
   replay are idempotent and forced RLS remains effective for `moa_app`; no compatibility
   reader or dual-write phase survives activation.
4. Treat embedding reuse as computation caching only when the complete contextual input
   (chunk text, document title, and heading path) plus embedding model/version matches.
   Equal text alone is insufficient. Every occurrence gets its own persisted embedding
   row and vector association keyed by `chunk_uid`, even when computation is reused.
5. Remove hydration's `DISTINCT ON`; one graph UID hydrates exactly one document-version
   occurrence. Every invalidation/deletion path uses persisted `graph_node_uid` for all
   active chunks/versions returned by `active_chunks_for_object`, never the latest version
   alone and never recomputed tenant-plus-content-hash UIDs.
6. Preserve tenant RLS, storage-partition predicates, and connection information barriers.
   This task makes occurrence identity capable of carrying independent provenance and
   ACLs; provider-principal ACL storage/admission remains exclusively Task 3.2/V000348.

**Acceptance:** equal contextual embedding input may reuse computation, but equal text in
two documents produces distinct `chunk_uid == graph_node_uid` nodes, occurrence embeddings,
document/provenance edges, deletion, hydration, and citation occurrences. Deleting one
document invalidates only its occurrences; the other remains retrievable and cites its
own source/version. A new version creates new associations even for unchanged content and
invalidates superseded occurrences; whole-object deletion covers every active version.
V000346-to-V000347 migration handles shared/null graph references, active/tombstoned
chunks, edges, embeddings, and vector outbox work under correct/missing/wrong tenant RLS.
Independent provider ACL enforcement is not claimed until Task 3.2.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000347|V000347|v346.*v347/)'
cargo nextest run -p moa-knowledge --locked --test knowledge_db_memory
cargo nextest run -p moa-brain --locked --test brain_db_memory \
  -E 'test(hybrid_retrieval_db_memory)'
cargo nextest run -p moa-orchestrator --locked --test knowledge_service
cargo clippy -p moa-migrations -p moa-knowledge -p moa-retrieval -p moa-brain \
  -p moa-wire -p moa-orchestrator --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
! rg -F -n 'stable_uid(&format!("chunk:{}:{}"' crates/moa-knowledge/src
! rg -n 'DISTINCT ON \(c\.graph_node_uid\)' crates/moa-retrieval/src/retrieval/hydration.rs
git diff --check
```

Mutation verification must restore tenant-plus-hash graph identity, delete only the
latest version or recompute a hash UID, skip per-occurrence embedding association, and
omit the migration equality constraint, edge clone, or vector outbox backfill. The exact
identity/deletion/hydration/migration assertions must fail before each restoration and
pass afterward. Use fresh isolated databases; do not wipe the Compose database without
explicit approval.

### Task 3.2 — Enforce provider-native source/document ACL admission [P0/P1] [ ]

**Depends on:** Task 3.1
**Why:** permission-bearing connectors can otherwise disclose content between users
inside the same tenant.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000348__knowledge_source_acl.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-core/src/types/memory.rs`
- `crates/moa-db/src/lib.rs`
- `crates/moa-db/tests/scoped_conn_rls_db.rs`
- `crates/moa-knowledge/src/domain/connection.rs`
- `crates/moa-knowledge/src/domain/document.rs`
- `crates/moa-knowledge/src/domain/provider.rs`
- `crates/moa-knowledge/src/providers/mod.rs`
- `crates/moa-knowledge/src/providers/merge.rs`
- `crates/moa-knowledge/src/providers/nango/mod.rs`
- `crates/moa-knowledge/src/ingestion/record.rs`
- `crates/moa-knowledge/src/repository/mod.rs`
- `crates/moa-knowledge/src/repository/document.rs`
- new `crates/moa-knowledge/src/repository/acl.rs`
- `crates/moa-knowledge/src/ingestion/materialization.rs`
- `crates/moa-retrieval/src/retrieval/types.rs`
- `crates/moa-retrieval/src/retrieval/cache.rs`
- `crates/moa-retrieval/src/retrieval/hybrid.rs`
- `crates/moa-retrieval/src/retrieval/legs.rs`
- `crates/moa-retrieval/src/retrieval/admission.rs`
- `crates/moa-retrieval/src/retrieval/graph_seed.rs`
- `crates/moa-retrieval/src/retrieval/hydration.rs`
- `crates/moa-retrieval/src/retrieval/hybrid/tests.rs`
- `crates/moa-brain/src/pipeline/memory.rs`
- `crates/moa-brain/src/pipeline/memory/lineage.rs`
- `crates/moa-brain/tests/brain_db_memory/hybrid_retrieval_db_memory.rs`
- `crates/moa-wire/src/knowledge.rs`
- `crates/moa-orchestrator/src/services/knowledge/mod.rs`
- `crates/moa-orchestrator/src/services/knowledge/inspect.rs`
- `crates/moa-orchestrator/tests/knowledge_service/ingestion.rs`
- `crates/moa-orchestrator/tests/knowledge_service/inspection.rs`
- `crates/moa-knowledge/tests/knowledge_db_memory/contact_groups_db_memory.rs`
- provider/record/retrieval constructor callers under `crates/moa-eval/`,
  `crates/moa-loadtest/`, `crates/moa-orchestrator/tests/`, and `crates/xtask/`
- `docs/01-architecture-overview.md`
- `docs/04-memory-architecture.md`
- `docs/07-context-pipeline.md`
- `docs/08-security.md`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Hard-add typed `ConnectionAclMode::{TenantPublic, ProviderManaged}`, canonical
   provider-principal kinds, allow/deny entries, immutable `ProviderAclSnapshot` with
   canonical revision/hash/provenance, and `Current|Stale|Incomplete` state. No missing
   field defaults or optional legacy shape exists. ProviderManaged admission requires a
   complete current matching revision, at least one matching allow, and no matching deny;
   missing principals/bindings/snapshot, incomplete/stale state, or revision mismatch
   denies. Add provider-declared `ProviderAclCapability::{UniformlyPublic,
   NativeSnapshots}`: only UniformlyPublic adapters can create TenantPublic; permission-
   bearing adapters require ProviderManaged and reject link/sync if they cannot return a
   complete native snapshot. Nango Google Drive and Merge normalize their provider ACLs
   or reject the integration—caller/operator choice cannot downgrade the mode.
2. V000348 adds connection ACL mode, object current ACL state/revision, immutable snapshot
   and entry tables, tenant/connection direct-principal and group bindings, and tenant/
   source ACL epoch. Trigger every snapshot, binding, and current-state change to bump the
   epoch. Enable and force RLS, own the migration, and prove bootstrap/reapply plus
   V000346-to-V000347-to-V000348 upgrade as the non-BYPASSRLS app role with correct/
   missing/wrong tenant and empty/wrong/correct principals.
   Backfill deterministically: known UniformlyPublic provider rows become TenantPublic;
   every existing permission-bearing or ambiguous connection becomes ProviderManaged +
   Incomplete/Stale and remains hidden until resync. No ambiguity reader survives.
3. Resolve a bounded, canonical caller principal set durably from authenticated Session/
   contact identity and verified provider bindings. Never accept principals from request
   JSON or fetch them during retrieval. Tenant role/operator status does not bypass source
   ACL. Extend `RlsContext`/`ScopedConn` with typed ACL admission context containing only
   the canonical principal-set fingerprint and current ACL/policy epoch in cache identity.
   Persist only keyed opaque fingerprints: normalize provider namespace/kind/subject,
   HMAC it with the versioned ACL-key owner backed by deployment KMS, and store fingerprint
   plus key version for direct/group entries and bindings. Never persist raw email/phone/
   provider labels or expose them in logs/traces/cache keys.
4. Acquire, normalize, validate, and atomically replace provider ACL snapshots before the
   content change-token and content-hash skip fences. ACL-only changes bump the epoch and
   change visibility without parse/re-embed. A permission-bearing record announcing ACL
   change without a complete valid snapshot becomes stale/hidden before a typed error.
5. Enforce admission before any ranking or content influence. Pass the bounded opaque
   principal fingerprints as bind parameters to explicit SQL admission predicates in
   lexical and pgvector ranking and every recursive graph hop; tenant RLS remains defense
   in depth and the aggregate principal-set fingerprint is cache identity only, never an
   ACL-entry join key. External-vector candidates receive one
   batched Postgres admission check before RRF or graph seeding. Graph seeds,
   intermediate hops, returned nodes, and every context-window neighbor are admitted.
   Denied text/metadata never enters fusion, hydration, tracing, lineage, preview, or
   citation construction.
6. Put canonical sorted principal-set fingerprint and current ACL/policy epoch in both
   retrieval-result and scoped-runtime cache keys. Snapshot and binding changes bump the
   epoch. Either key a memory runtime by the full ACL context or pass it per request; a
   runtime/store created for one principal set cannot be reused by another.
7. Keep connection information barriers as organization policy and tenant RLS as defense
   in depth. Operator authorization may list control-plane metadata only. Content preview,
   hydrated chunks, and citations require source-principal admission; this task adds no
   silent privileged content relation.
8. Hard-change `RetrievalRequest`, `ProviderRecord`, connection/document/wire types, and
   every brain/eval/loadtest/orchestrator/provider/xtask constructor in one change. Add no
   `Option` fallback, serde default, principal-bearing public request field, dual ACL read,
   deprecated wrapper, or compatibility alias.

**Acceptance:** direct allow and group allow work only for bound principals; explicit
deny wins. Empty/wrong principals and missing/incomplete/stale/revision-mismatched ACLs
deny ProviderManaged content. ACL-only updates change visibility and cache epoch without
parse/re-embed. Pgvector, lexical, external-vector RRF, graph seeds/intermediate hops,
context neighbors, hydration, preview, lineage, and citation all fail closed before
denied content can influence results. Cross-principal warm result/runtime caches never
hit. Raw principals are absent from logs, traces, and cache keys. Operator metadata
listing remains authorized separately and grants no content bypass.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000348|V000348|v347.*v348/)'
cargo nextest run -p moa-db --locked --run-ignored ignored-only \
  --test scoped_conn_rls_db
cargo nextest run -p moa-knowledge --locked --test knowledge_db_memory
cargo nextest run -p moa-knowledge --locked --test knowledge_offline
cargo nextest run -p moa-retrieval --locked
cargo nextest run -p moa-brain --locked --test brain_db_memory \
  -E 'test(/hybrid_retrieval|source_acl/)'
cargo nextest run -p moa-orchestrator --locked --test knowledge_service
make test-authz-pentest
cargo clippy -p moa-core -p moa-db -p moa-migrations -p moa-knowledge \
  -p moa-retrieval -p moa-brain -p moa-wire -p moa-orchestrator \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must remove deny precedence, treat missing ACL as public, preserve
the content skip before ACL ingestion, filter after RRF, admit denied graph seeds/hops or
context neighbors, omit either cache-key dimension or an epoch bump, and drop `FORCE ROW
LEVEL SECURITY`. The exact provider-normalization, ACL-only update, pre-rank, graph,
neighbor, cache, lineage/citation, and app-role tests must fail before each restoration
and pass afterward. Use fresh isolated databases; do not wipe Compose without approval.

### Task 3.3 — Add durable re-embed/rechunk rebuild workflows [P1] [ ]

**Depends on:** Task 3.2
**Scheduling:** wait until Task 4.2 freezes `runtime/endpoint.rs`, service registration,
edge routes, and migration wiring. Task 3.2 may be source-passed while Docker is blocked,
but its inherited DB gate remains part of Task 3.3's assembled runtime validation.
**Why:** embedder upgrades, chunking changes, and corruption recovery otherwise require
bespoke operations without resumable progress, activation, or rollback.

**Files:**

- `crates/moa-core/src/types/memory.rs`
- `crates/moa-wire/src/memory.rs`
- new `crates/moa-migrations/migrations/postgres/V000351__knowledge_rebuild_state.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-memory/vector/src/promotion.rs`
- `crates/moa-memory/vector/src/pgvector_store.rs`
- `crates/moa-memory/vector/src/turbopuffer.rs`
- new vector rebuild/generation repository modules under `crates/moa-memory/vector/src/`
- knowledge rebuild staging/materialization repositories under `crates/moa-knowledge/src/`
- every non-knowledge vector owner needed to reconstruct the authoritative embedding
  input for a whole storage partition
- retrieval generation routing under `crates/moa-retrieval/src/retrieval/`
- `crates/moa-orchestrator/src/services/admin_maintenance.rs`
- `crates/moa-orchestrator/src/services/graph_memory_maint.rs`
- new `crates/moa-orchestrator/src/workflows/knowledge_index_rebuild.rs`
- `crates/moa-orchestrator/src/workflows/mod.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-edge/src/routes/memory.rs`
- `crates/moa-edge/src/mcp/`
- `crates/xtask/src/execution_trace_manifest.rs`
- `crates/moa-orchestrator/tests/memory_service_e2e.rs`
- `crates/moa-orchestrator/tests/knowledge_service.rs`
- `docs/01-architecture-overview.md`
- `docs/04-memory-architecture.md`
- `docs/12-restate-architecture.md`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Add V000351 forced-RLS rebuild-operation, generation, candidate-vector, staging, and
   active-generation state. Persist the owner/fence token, durable checkpoint,
   lifecycle, counts, deterministic estimated cost, request/throttle/retry counters,
   safe errors, cancellation, rollback, and finalization. Enforce one nonterminal
   operation per storage partition and compare-and-swap every transition. The provider
   trait does not expose billed usage, so do not label estimates as actual cost.
2. Make re-embed the first production writer of the existing
   `reembed_state = 'in_progress'` fence and extend it to ordinary writes as well as KNN
   reads. The candidate generation builds while the old generation remains the sole
   production read generation; no candidate/shadow hit may enter retrieval, ranking,
   hydration, lineage, or citations.
3. Re-embed the complete storage partition, not only knowledge chunks. Fact, incident,
   contact/session, entity, and knowledge vectors can coexist under one embedding
   identity. Reconstruct the exact authoritative original embedding input for every
   active vector type and fail closed on missing or unknown provenance; never substitute
   display names or otherwise mix vector spaces.
4. Reuse only the pure top-K overlap validation rule from the existing promotion engine.
   Do not reuse its serving `dual_read`, unconditional state updates, or process-local
   cursor: the current path can serve Turbopuffer hits and is not validation-only.
   Persist generation-specific Turbopuffer namespaces plus one active-generation
   pointer, validate from bounded shadow queries, and atomically activate only a complete
   generation. Retain the prior generation for explicit rollback, then remove retired
   data at finalization.
5. Define rechunk as a second operation on the same generation state machine. Stage
   chunks, graph deltas, embeddings, ACL snapshots, occurrence identity, and provenance;
   fence ordinary materialization; then activate document/chunk/graph/vector/changelog/
   outbox state and the generation pointer in one scoped transaction. If the graph and
   outbox owners cannot join that atomic boundary, block rechunk rather than ship a
   resumable-looking partial workflow.
6. Use Restate workflows for bounded batches, replay-safe checkpoints, cancellation,
   validation, activation, rollback, and finalization. Crash/retry cannot duplicate
   candidates or let one generation overwrite another's fence.
7. Expose tenant-scoped start/status/cancel/rollback/finalize through typed wire DTOs,
   GraphMemoryMaint, authorized edge HTTP, and the operator MCP catalog. Require
   tenant-admin/operator authz before reads and mutations, register the workflow/service
   and execution-trace paths, and add no compatibility route or legacy payload.

**Acceptance:** crash/replay resumes without duplicate candidates or active generations;
failed validation leaves the old generation authoritative; production never serves
shadow results; partition-wide re-embed never mixes models; rechunk activation is one
scoped atomic boundary; activation/rollback are compare-and-swap atomic; concurrent
rebuilds cannot overwrite progress or fences; status reports generation IDs, exact
counts, estimated cost, rate/retry state, and safe errors; after finalization no
production reader understands the retired vector-model/chunk contract.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-memory-vector --locked
cargo nextest run -p moa-knowledge --locked --test knowledge_offline
cargo nextest run -p moa-retrieval --locked
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db_memory \
  -E 'test(/rebuild|reembed|rechunk/)'
cargo nextest run -p moa-orchestrator --locked --test knowledge_service
cargo nextest run -p moa-edge --locked
cargo test -p xtask --locked execution_trace_manifest
cargo clippy -p moa-core -p moa-wire -p moa-memory-vector -p moa-knowledge \
  -p moa-retrieval -p moa-orchestrator -p moa-edge -p xtask \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Runtime verification at the assembled large-task boundary uses a fresh isolated
V000346-to-V000351 database, the ignored Restate memory-service E2E, and the DB-memory
lane. Mutation verification must remove generation CAS, the ordinary-write fence,
partition-wide coverage, authoritative-input reconstruction, validation-only admission,
generation namespace identity, atomic activation, authz, or one rechunk staging member;
the exact replay/fence/model-mixing/shadow-leak/rollback/authz/atomicity assertion must
fail before restoration.

### Migration verification discipline

The plan reserves V000346 through V000357 from the current V000345 maximum. Verify each
forward migration against a fresh isolated database and exercise upgrade plus current
baseline bootstrap. A new migration does not itself justify wiping a database. If an
unreleased migration was already applied and then edited, reset only an explicitly
disposable local Compose database and only with explicit approval for `make dev-wipe`;
never wipe shared/staging/production state or hand-`ALTER` around the migration.

## M4 — Complete Messaging and Operator Workflows

### Task 4.1 — Route bound Slack messages and deliver final answers [P1] [ ]

**Depends on:** Tasks 1.2 and 1.3
**Why:** the documented Slack front door accepts ordinary text but ignores it, then
sends progress without the answer.

**Files:**

- `crates/moa-orchestrator/src/runtime/channel_ingress.rs`
- `crates/moa-orchestrator/src/workflows/progress_delivery.rs`
- `crates/moa-session/src/store/session_channels.rs`
- `crates/moa-messaging/src/action_review.rs`
- `crates/moa-messaging/src/slack/mod.rs`
- `crates/moa-messaging/src/slack/inbound.rs`
- `crates/moa-messaging/src/slack/adapter.rs`
- `crates/moa-messaging/tests/messaging_offline/normalization.rs`
- `crates/moa-orchestrator/tests/session_turn_lifecycle_service_e2e.rs`

**Implementation:**

1. Resolve ordinary Slack messages through an existing channel/session binding. Do not
   create a session implicitly for an unbound conversation.
2. Admit through Session `queue_message` with the Slack event ID as
   `client_message_id`; preserve thread/reply correlation.
3. After the visible response event commits, render the durable `BrainResponse` through
   the adapter. Key delivery by channel plus session event sequence in the existing
   delivery journal/outbox.
4. Keep progress coalescing; replace only the terminal literal status path.

**Acceptance:** a bound message uses the REST-equivalent Session path and receives the
answer; duplicates/replay create no duplicate turn or answer; unbound conversations get
an explicit binding instruction and create no session.

**Verification:**

```bash
cargo nextest run -p moa-messaging --locked --test messaging_offline
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration --run-ignored ignored-only --test session_turn_lifecycle_service_e2e
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline
```

### Task 4.2 — Close offboarding and distributed auth abuse controls [P1] [ ]

**Depends on:** Tasks 1.3 and 2.1
**Scheduling:** Task 1.3 must finish first because both tasks own the edge route/test
harness. Task 3.2 may continue in parallel.
**Why:** SCIM owns the only complete user offboarding cascade, tenant-admin routes have
no equivalent role/disable/delete operation, and the cascade does not revoke browser
sessions. Login/reset requests also have no replica-wide admission control.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000350__tenant_user_lifecycle.sql`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/migration-ownership.toml`
- new `crates/moa-wire/src/user_accounts.rs`
- `crates/moa-wire/src/lib.rs`
- `crates/moa-auth/auth0/src/auth0_provider.rs`
- `crates/moa-auth/providers/src/api_keys.rs`
- `crates/moa-auth/providers/src/postgres_vault.rs`
- `crates/moa-auth/providers/src/user_sessions.rs`
- new `crates/moa-db/src/user_lifecycle.rs`
- `crates/moa-db/src/lib.rs`
- `crates/moa-auth/providers/tests/auth_providers_db.rs`
- new `crates/moa-auth/providers/tests/auth_providers_db/user_sessions_lifecycle_db.rs`
- `crates/moa-orchestrator/src/identity_admin/users.rs`
- `crates/moa-orchestrator/src/identity_admin/groups.rs`
- `crates/moa-orchestrator/src/identity_admin/api_keys.rs`
- `crates/moa-orchestrator/src/identity_admin/agents.rs`
- `crates/moa-orchestrator/src/services/mod.rs`
- new `crates/moa-orchestrator/src/services/user_accounts.rs`
- `crates/moa-orchestrator/src/services/authz_admin.rs`
- `crates/moa-orchestrator/src/services/session_store/handlers.rs`
- `crates/moa-orchestrator/src/services/session_store/inner.rs`
- `crates/moa-edge/src/routes/tenant_accounts/users.rs`
- `crates/moa-edge/src/routes/tenant_accounts/mod.rs`
- `crates/moa-edge/src/tenant_accounts/mod.rs`
- `crates/moa-edge/src/tenant_accounts/application.rs`
- `crates/moa-edge/src/tenant_accounts/repository.rs`
- `crates/moa-edge/src/routes/auth_accounts.rs`
- `crates/moa-edge/src/lib.rs`
- `crates/moa-edge/src/main.rs`
- `crates/moa-edge/src/routes.rs`
- new `crates/moa-edge/src/auth_abuse.rs`
- `crates/moa-edge/src/proxy.rs`
- `crates/moa-edge/Cargo.toml`
- `crates/moa-edge/tests/direct_read_routes_db.rs`
- new `crates/moa-edge/tests/direct_read_routes_db/auth_accounts_db.rs`
- new `crates/moa-edge/tests/direct_read_routes_db/tenant_user_lifecycle_db.rs`
- new `crates/moa-edge/tests/auth_abuse_docker.rs`
- `crates/moa-edge/tests/session_message_attachments_docker.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-orchestrator/src/services/scim/users.rs`
- `crates/moa-orchestrator/tests/orchestrator_db.rs`
- new `crates/moa-orchestrator/tests/orchestrator_db/user_lifecycle_db.rs`
- new `crates/moa-orchestrator/tests/user_accounts_service_e2e.rs`
- `crates/moa-ocsf/src/lib.rs`
- `crates/moa-ocsf/src/emit.rs`
- `crates/moa-ocsf/tests/ocsf_db/emit_matrix_db.rs`
- `crates/xtask/src/execution_trace_manifest.rs`
- `k8s/base/50-edge-deployment.yaml`
- `k8s/overlays/local/patches/edge.yaml`
- new `k8s/overlays/production/patches/edge-auth-security.yaml`
- `k8s/overlays/production/kustomization.yaml`
- `k8s/scripts/smoke.sh`
- `.env.example`
- `docker-compose.yml`

**Implementation:**

1. Add one wire-owned hard-break contract:
   `TenantUserRole::{Admin, Operator}`,
   `UserAccountOperation::{SetRole { role }, Disable, Delete}`,
   `UserAccountMutationRequest { user_id, operation }`, and a typed mutation response
   with exact lifecycle counts, including sealed user OAuth connections. Remove the
   edge-private role type and update every caller; add no alias, re-export, conversion
   shim, optional field, or `serde(default)`.
2. Add V000350 with nullable `users.tenant_role` constrained to `admin|operator`.
   Deterministically backfill once from current outbox desired state (admin wins, then
   operator, else null). After migration, use only this field as product role state;
   signup, direct creation, and invitation acceptance persist it, with no runtime
   fallback to invitations, outbox, or OpenFGA. Remove privileged SCIM group-name role
   materialization so the shared lifecycle owner is the only direct role writer.
3. Make `identity_admin/users.rs` the only transactional lifecycle owner. Under a
   tenant/user serialization fence, authorize before reading the target and perform exact
   role desired-state replacement or disable/delete. Disable/delete must cancel
   nonterminal conversational sessions, revoke API keys and all browser session tokens,
   revoke pending invitations/password-reset tokens and sealed user OAuth connections,
   enqueue inverse OpenFGA tuples, orphan owned agents, remove group memberships through
   the owning path, and write the account-change OCSF event in the same transaction.
   Use canonical session actor columns rather than the legacy string `user_id`, and
   preserve each resource's actual tenant when enqueuing inverse tuples. Delete removes
   the user; disable retains it inactive. Repeated terminal or same-role operations
   return `changed = false` with no duplicate audit/outbox generation. Add one
   transaction-scoped tenant/user revoke-all helper in
   `moa-auth-providers::user_sessions`; remove duplicate edge SQL, the dead
   `oidc_user_groups` compatibility probe, and positive Auth0/API-key cache paths that
   can authorize after offboarding.
4. Serialize every existing-user access grant against lifecycle changes. Creators use
   the exact active tenant/user row `FOR KEY SHARE`, while the lifecycle owner uses
   `FOR UPDATE`; opaque-resource discovery may occur without a lock only to identify
   tenant/user, after which the order is user first and then
   invitation/reset/credential/key/group/agent/resource. Cover invitation acceptance,
   reset issuance and consumption, login/signup/invitation browser-token issuance,
   admin/self password changes, user API-key create/rotate and tenant-role grants,
   operator/agent/internal conversational-session creation, existing Auth0/OIDC
   mappings, Auth0 linked-connection webhooks, sealed token-vault connection writes,
   SCIM group add/replace, agent registration, and `grant_can_act_as`. Multi-user
   operations sort and deduplicate IDs before locks. Lifecycle teardown also deletes
   every current `can_act_as` grant where the offboarded user is the grantee, including
   agents owned by another user. Fresh user-row creation needs no pre-existing lock,
   but subsequent token/resource issuance does.
5. Add a typed `UserAccounts` Restate service. Derive tenant/actor only from verified
   invocation identity and call delegated tenant-admin authz before target reads. SCIM
   keeps its authenticated tenant binding and invokes the same shared owner; edge
   PATCH/DELETE tenant-user routes call the typed Restate service. Register the service
   and execution-trace endpoints. Remove the SCIM-only cascade; do not retain a second
   lifecycle path. Do not edit whole-tenant `tenant_accounts/deletion.rs`.
6. Build `auth_abuse.rs` over `RuntimeCacheStore::try_acquire_bounded_lease`; do not add
   another Redis counter. Enforce both dimensions before lookup/hash/token/delivery:
   login account 10/15m, login client IP 100/15m, reset account 3/hour, reset client IP
   30/hour. Every admitted attempt consumes a fresh full-window lease; success does not
   clear it. HMAC-SHA-256 `auth-abuse:v1` account and IP keys; persist/label only opaque
   digests. Saturation returns `429` plus exact `Retry-After` and performs zero sensitive
   work.
7. Enforce one explicit startup/runtime matrix, with no `auto` or fallback. Cloud requires
   security profile Cloud, Redis backend/URL/startup probe, a valid 32-byte abuse HMAC
   key, and nonempty trusted proxies; Redis failure returns `503` before auth work. Local
   requires security profile Local, explicit memory backend, an ephemeral process key,
   and exactly one checked-in edge replica. Add the runtime-store Redis feature and
   render explicit local/production config and Secret references.
8. Establish one trusted-ingress IP contract using `ConnectInfo<SocketAddr>`. Local and
   untrusted Cloud peers ignore forwarding headers. A trusted immediate proxy may supply
   exactly one canonical first `X-Forwarded-For` address; missing, duplicate, non-UTF-8,
   empty, or malformed trusted forwarding returns `400` before auth work. Production
   ingress strips/replaces client forwarding headers. Replace the existing header-only
   sibling helper so no spoofable path remains.
9. Emit existing OCSF Authentication, Account Change, and Authorization classes for
   login/reset attempts, throttles, reset issuance, role change, disable, and deletion.
   Lifecycle and reset-issuance audits commit with their mutations. Record stable safe
   outcome/reason codes and opaque admission keys; never email, raw IP, password,
   credential hashes, browser/reset tokens, or provider error text.
10. Add `// Pins:` tests through real owners: browser-token tenant isolation/replay;
   SCIM/admin cascade parity, role replacement, exact tuples/counts/rollback/no-op replay;
   Restate identity/authz/delegation/cross-tenant denial; edge route scope and zero-work
   throttling; trusted-ingress cases; two independent Redis guards sharing one isolated
   prefix; OCSF mapping/transaction/redaction; local/production manifest contracts; and
   both race outcomes for every dependent access creator (creator first is torn down,
   lifecycle first creates no resource/outbox/audit), including sorted multi-user locks
   and a bounded deadlock timeout.

**Acceptance:** SCIM and tenant-admin role/disable/delete converge on one transactional
owner and typed hard-break contract. Disable/delete revoke conversational sessions,
browser sessions, API keys, password resets, invitations, sealed user OAuth connections,
OpenFGA tuples, group memberships, and agent ownership exactly once; same-role/repeated
operations emit no duplicate audit or outbox. Product role state has one relational
owner, dependent access cannot race after offboarding, and SCIM groups cannot regrant a
direct tenant role. Offboarding removes user-as-grantee `can_act_as` delegation, not
only user-owned agents. Authz is before target reads, tenant scope never comes from request
JSON, and agent writes require delegation. Account and IP limits run before sensitive
work and hold across two Cloud replicas. Cloud fails startup/runtime closed without
Redis/key/trusted-proxy config; Local explicitly uses one memory-backed replica.
Forwarded IP is trusted only from an immediate configured proxy. No raw
account/IP/secret material enters cache keys, logs, traces, responses, or OCSF. No old
role type, SCIM-only cascade, duplicate revocation SQL, positive offboarding-bypass
cache, dead table probe, compatibility wrapper, `auto`, or backend fallback remains.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-wire --locked user_accounts
cargo nextest run -p moa-auth-providers --locked --test auth_providers_db \
  -E 'test(/user_sessions_lifecycle/)'
cargo test -p moa-edge --locked auth_abuse
cargo nextest run -p moa-edge --locked --test direct_read_routes_db \
  -E 'test(/login|password_reset|trusted_client_ip|tenant_user_lifecycle/)'
MOA_RUN_LIVE_REDIS=1 cargo nextest run -p moa-edge --locked \
  --run-ignored ignored-only --test auth_abuse_docker
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db \
  -E 'test(/user_lifecycle|role_change|disable_user|delete_user|offboarding/)'
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features integration --run-ignored ignored-only \
  --test user_accounts_service_e2e
cargo nextest run -p moa-ocsf --locked --test ocsf_db \
  -E 'test(/authn|login|throttle|reset|role|disable|delete/)'
cargo test -p xtask --locked execution_trace_manifest
kubectl kustomize k8s/overlays/local >/tmp/moa-local.yaml
kubectl kustomize k8s/overlays/production >/tmp/moa-production.yaml
./k8s/scripts/smoke.sh --validate-manifests
make test-authz-pentest
cargo clippy -p moa-wire -p moa-auth-providers -p moa-edge -p moa-orchestrator \
  -p moa-runtime-store -p moa-ocsf -p xtask \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must separately omit browser-session revocation, one inverse
OpenFGA tuple, pre-read/delegated authz, transactional audit, account or IP limiter,
Cloud Redis fail-closed/startup/key/proxy checks, Local explicit-memory/single-replica
checks, trusted-proxy enforcement, redaction, reset zero-work after rejection, and no-op
replay. Each exact lifecycle count, ordering, cross-replica limit, backend matrix,
trusted-ingress, secret absence, transaction, and replay assertion must fail before
restoration and pass afterward.

### Task 4.3 — Add workspace context and unified approval read/notification paths [P1] [ ]

**Depends on:** Tasks 1.3 and 4.2
**Scheduling:** start only after Task 4.2 freezes edge tenant/auth routes, service
registration, and migration wiring. Task 3.3 takes V000351 first; this task takes
V000352.
**Why:** operators need authorized tenant selection and one actionable queue, but
merging underlying state machines would weaken typed ownership.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000352__approval_notification_intents.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-wire/src/`
- `crates/moa-edge/src/routes.rs`
- new workspace-context and approval-inbox handlers under `crates/moa-edge/src/routes/`
- `crates/moa-edge/src/routes/tenant_accounts/mod.rs`
- `crates/moa-edge/src/routes/tenant_accounts/users.rs`
- `crates/moa-edge/src/routes/auth.rs`
- `crates/moa-edge/src/tenant_accounts/application.rs`
- `crates/moa-edge/src/tenant_accounts/repository.rs`
- `crates/moa-auth/authz/src/client.rs`
- `crates/moa-orchestrator/src/action_reviews/app.rs`
- `crates/moa-orchestrator/src/action_reviews/store.rs`
- `crates/moa-orchestrator/src/services/action_reviews.rs`
- `crates/moa-orchestrator/src/authz_challenges/app.rs`
- `crates/moa-orchestrator/src/authz_challenges/store.rs`
- `crates/moa-orchestrator/src/services/authz_challenges.rs`
- new `crates/moa-orchestrator/src/services/approval_inbox.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-messaging/src/action_review.rs`
- `crates/moa-orchestrator/tests/action_policy_service_e2e.rs`
- `crates/moa-orchestrator/tests/orchestrator_db/authz_challenges_db.rs`
- new approval-inbox and notification-intent modules in the existing orchestrator DB
  harness

**Implementation:**

1. Add a direct authorized workspace-inventory route. Reject contact/non-operator
   identities before enumeration, call the existing FGA `list_objects` relation owner,
   parse exact tenant IDs, and read only those tenant rows in stable order.
2. Treat selected workspace/tenant as short-lived untrusted UI context, not a minted
   credential. Authorize the selected tenant before loading it and re-authorize it at
   every consumer; a forged cookie/header grants no authority and does not replace
   Task 4.2's active home-tenant session validation.
3. Add a read-only unified inbox projection over action reviews and builtin authz
   challenges. Preserve both mutation state machines and endpoints. Filter action reviews
   by exact selected tenant; filter challenges by exact selected tenant and deciding
   user. Require selected-tenant Admin authorization before either protected read.
4. Use keyset pagination on `(created_at, kind_rank, id) DESC` with `limit + 1`. Return a
   strict discriminated DTO with kind, normalized display status, typed owner,
   session-history link, and typed decision target:
   `ActionReview { tenant_id, review_id }` or
   `AuthzChallenge { tenant_id, challenge_id }`. Clients dispatch only to the existing
   canonical decision endpoint; add no unified decision route or compatibility shim.
5. Add decision-time action-review expiry validation matching builtin challenge
   behavior. An expired pending review cannot be cleared while waiting for its reaper;
   cover decision-versus-expiry races.
6. Add V000352 forced-RLS durable in-app notification intents. Both state machines and
   reapers enqueue actionable and expired transitions transactionally with unique
   semantic identity
   `(approval_kind, approval_id, event_kind, recipient, channel)` and
   `ON CONFLICT DO NOTHING`. The unified inbox consumes in-app intent state; replay
   cannot duplicate an item.
7. External approval push is deliberately not invented in this task: there is no typed
   operator Email/SMS/Slack destination owner. Capability DTOs separate notification
   from decision and report external channels unconfigured. Never route tenant-admin
   approval data through contact/session destinations; Slack interactive decisions
   remain false. A later external-delivery feature must add explicit operator
   destination ownership and at-least-once delivery semantics.

**Acceptance:** contact/cross-tenant inventory and selected context fail before reads;
forged context grants no authority; equal-timestamp keyset pagination has no duplicates
or skips; each typed decision target reaches only its existing state machine; selected
tenant and deciding-user filters cannot leak another item; expired action reviews cannot
be acted on before the reaper; actionable/expired intent replay deduplicates; external
notification/decision capability is false without an explicit operator destination.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000352|V000352|v351.*v352/)'
cargo nextest run -p moa-edge --locked
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test action_policy_service_e2e
make test-authz-pentest
cargo clippy -p moa-wire -p moa-edge -p moa-authz -p moa-orchestrator \
  -p moa-messaging -p moa-migrations --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must remove the inventory pre-authz, selected-tenant re-authz,
deciding-user filter, one keyset tie-breaker, decision-time expiry guard, transactional
intent enqueue, uniqueness key, or capability/destination check. The exact no-read,
pagination, wrong-owner, expiry-race, replay, and false-capability assertion must fail
before restoration.

## M5 — Privacy-Safe Learning and Measurable Evaluation

Task 5.1 can start after M0 and is independent of the prompt-injection circuit. Task 5.4
also has no logical dependency on the knowledge rebuild, but both Tasks 5.4 and 3.3 edit
`graph_memory_maint.rs`; do not execute those write sets concurrently. Task 5.5 waits for
correct occurrence and ACL admission, not for rebuild or Behavior Lab work.

### Task 5.1 — Require typed sanitized evidence before learning calls [P0/P1] [ ]

**Depends on:** Task 0.2
**Why:** raw messages, tool inputs/results/errors, and assistant content can reach a
learning provider and draft storage before human review.

**Files:**

- new `crates/moa-skills/src/evidence.rs`
- `crates/moa-skills/Cargo.toml`
- `crates/moa-skills/src/lib.rs`
- `crates/moa-skills/src/distiller.rs`
- `crates/moa-skills/src/embeddings.rs`
- `crates/moa-skills/src/improver.rs`
- `crates/moa-skills/src/proposals.rs`
- `crates/moa-skills/src/regression.rs`
- `crates/moa-skills/src/candidates.rs`
- `crates/moa-brain/src/lineage.rs`
- `crates/moa-brain/Cargo.toml`
- `crates/moa-brain/src/turn_learning.rs`
- `crates/moa-brain/src/learning/experience.rs`
- `crates/moa-brain/src/learning/attribution.rs`
- `crates/moa-brain/src/learning/candidates.rs`
- `crates/moa-eval/src/long_conversation/transcript_runner.rs`
- `crates/moa-memory/pii/src/lib.rs`
- `crates/moa-orchestrator/src/turn_driver/learning.rs`
- `crates/moa-orchestrator/src/services/session_store/handlers.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/experience.rs`
- `crates/moa-orchestrator/src/workflows/skill_learning.rs`
- `crates/moa-skills/tests/regression.rs`
- `crates/moa-skills/tests/support/common.rs`
- `crates/moa-skills/tests/distillation_db_memory.rs`
- `crates/moa-skills/tests/improver_db_memory.rs`
- `crates/moa-skills/tests/draft_proposals_db_memory.rs`
- `crates/moa-orchestrator/tests/orchestrator_db/skill_learning_workflow_db.rs`
- `crates/moa-orchestrator/tests/skill_learning_gate_e2e.rs`
- `docs/09-skills-and-learning.md`
- `docs/08-security.md`

**Implementation:**

1. Define `SanitizedLearningEvidence` with private fields and no raw-string constructor,
   `From<String>`, or `Deserialize`. It contains irreversibly redacted structured event
   content; tenant/contact scope; session/segment/experience/event provenance; original
   classification result and detector version; redacted categories; and one constant
   privacy-policy revision. Add an opaque fallible sanitized result primitive to
   `moa-memory-pii`; do not make `moa-skills` depend on `moa-brain`. The orchestrator
   alone constructs evidence with an explicit `Arc<dyn PiiClassifier>`: production uses
   the deterministic heuristic shared with lineage, while direct workflow tests inject
   abstaining, error, and invalid-span classifiers. `moa-skills` never classifies raw text.
2. Hard-replace raw `EventRecord` APIs for distillation, improvement, sibling/resynthesis,
   recurrence suites, regression-suite generation, provider formatting, and task-summary
   embeddings with typed sanitized evidence. No overload, wrapper, deprecated raw path,
   or transcript-shaped candidate payload remains. ID-only model-free mining stays
   separate.
3. Before any provider call or derived write, reject Restricted, Secret, classifier
   error/abstention, incomplete/malformed/overlapping/non-UTF-8 spans, residual sensitivity
   after reclassification, and reserved reversible DLP-token delimiters. PII/PHI proceeds
   only after irreversible redaction. Errors/logs contain stable reason codes, never text
   or classifier source errors.
4. Apply the gate to all distillation, improvement, resynthesis, embedding, experience,
   candidate, draft, and suite persistence paths. Derived rows contain redaction or stable
   provenance IDs; the raw session event log remains the separate source-of-truth owner.
   Hard-change brain experience/candidate call sites and the eval transcript runner to
   carry the sanitized contract; no unchanged public raw signature remains.
5. Extend `skill_learning_gate_e2e` through a proposal produced by the real sanitized
   generation path and assert its sanitized suite survives review. All added tests carry
   `// Pins:` and use isolated tenant/session IDs. Update the skills/security docs with
   irreversible PII/PHI handling, restricted/abstained rejection, provenance, and the
   separation from reversible request-scoped `moa-dlp`.

**Acceptance:** APIs make raw transcript evidence unrepresentable at every automatic
learning/provider boundary. PII across user/queued/tool input/tool result/tool error/
assistant/summary/assessment sources is redacted in captured LLM and embedding requests
and absent from derived rows. Restricted/secret, abstained/error, invalid-span,
residual-sensitive, and DLP-token inputs produce zero provider calls and zero derived
writes. Tenant/contact and exact session/segment/experience/event provenance survive;
sibling/recurrence paths gate each sibling independently; durable errors contain reason
codes only.

**Verification:**

```bash
cargo nextest run -p moa-memory-pii --locked --lib
cargo nextest run -p moa-memory-pii --locked --test memory_pii_offline
cargo nextest run -p moa-brain --locked --lib
cargo nextest run -p moa-eval --locked --lib long_conversation
cargo nextest run -p moa-skills --locked --lib
cargo nextest run -p moa-skills --locked --test regression
cargo nextest run -p moa-skills --locked --test distillation_db_memory
cargo nextest run -p moa-skills --locked --test improver_db_memory
cargo nextest run -p moa-skills --locked --test draft_proposals_db_memory
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db -E 'test(skill_learning)'
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration \
  --run-ignored ignored-only --test skill_learning_gate_e2e
cargo fmt --all
cargo clippy -p moa-memory-pii -p moa-brain -p moa-eval -p moa-skills -p moa-orchestrator \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must remove the restricted/abstention guard, pass one raw field
instead of its redacted value, and build a suite from raw events. The corresponding
zero-provider, request/persistence scan, and suite-row assertions must fail before each
restoration and pass afterward.

### Task 5.2 — Close privacy provenance and simplify candidate status [P1] [ ]

**Depends on:** Tasks 5.1, 2.3, 3.3, and 4.3. Task 4.3 must land V000352 before
this task creates or applies V000353.
**Why:** source-memory deletion while retaining attributable learning is incomplete
erasure; proposed kinds without materializers create a false review contract.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000353__learning_privacy_provenance.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-core/src/types/experience.rs`
- `crates/moa-core/src/types/learning.rs`
- `crates/moa-brain/src/learning/candidates.rs`
- `crates/moa-skills/src/mining.rs`
- `crates/moa-skills/src/proposals.rs`
- `crates/moa-skills/src/rollback.rs`
- `crates/moa-skills/src/recurrence.rs`
- `crates/moa-skills/src/review.rs`
- `crates/moa-orchestrator/src/services/privacy/manifest.rs`
- `crates/moa-orchestrator/src/services/privacy/export.rs`
- `crates/moa-orchestrator/src/services/privacy/erase.rs`
- `crates/moa-orchestrator/src/services/privacy/repository.rs`
- `crates/moa-memory/pii/src/lib.rs`
- `crates/moa-memory/pii/src/legal_hold.rs`
- `crates/moa-memory/pii/src/erasure.rs`
- new PII-owned learning-erasure/decision ledger module
- `crates/moa-session/src/store/learning.rs`
- `crates/moa-session/src/store/experience.rs`
- `crates/moa-session/src/store/session_store.rs`
- new session-owned learning provenance/decision module
- `crates/moa-session/src/queries/columns.rs`
- `crates/moa-session/src/queries/enums.rs`
- `crates/moa-session/src/queries/rows.rs`
- `crates/moa-session/src/analytics.rs`
- `crates/moa-artifacts/src/registry/mod.rs`
- `crates/moa-artifacts/src/registry/revisions.rs`
- new artifact-owned contribution/privacy module
- `crates/moa-skills/src/candidates.rs`
- `crates/moa-orchestrator/src/turn_driver/learning.rs`
- `crates/moa-orchestrator/src/workflows/consolidate.rs`
- `crates/moa-orchestrator/src/workflows/turn_execution/experience.rs`
- `crates/moa-orchestrator/src/services/experiments.rs`
- `crates/moa-orchestrator/src/services/learning_review.rs`
- `crates/moa-orchestrator/src/services/session_store/mod.rs`
- `crates/moa-orchestrator/src/services/session_store/handlers.rs`
- `crates/moa-wire/src/privacy.rs`
- `crates/moa-wire/src/session_store.rs`
- `crates/moa-edge/src/routes/artifacts.rs`
- `crates/moa-edge/src/mcp/artifacts_learning.rs`
- `crates/moa-edge/src/mcp/contract.rs`
- `crates/moa-edge/src/mcp/mod.rs`
- `crates/moa-edge/src/mcp/analytics_sessions.rs`
- `crates/moa-experiments/src/app.rs`
- `crates/moa-eval/src/long_conversation/transcript_runner.rs`
- `crates/moa-analytics-export/src/dims.rs`
- `crates/moa-analytics-export/src/schema.rs`
- `crates/moa-orchestrator/tests/orchestrator_db_memory/privacy_service_db_memory.rs`
- `crates/moa-orchestrator/tests/skill_learning_review_db.rs`
- `crates/moa-session/tests/session_db/postgres_store_db.rs`
- `crates/moa-session/tests/session_db/learning_candidate_planning_audit_db.rs`
- `docs/04-memory-architecture.md`
- `docs/01-architecture-overview.md`
- `docs/08-security.md`
- `docs/09-skills-and-learning.md`

**Implementation:**

1. Add owner-specific normalized tenant-bound provenance. Session owns
   `learning_candidate_source`, `learning_log_source`, and
   `learning_candidate_decision`; artifact registry owns
   `artifact_revision_contribution` and `artifact_suite_contribution`; PII owns
   `privacy_erasure_record_decision` and the erasure-job stage/counters. Use typed one-of
   foreign keys, tenant-equality checks, forced RLS, traversal/dedupe indexes, and
   deferred commit-time source completeness. Do not replace arrays with an unvalidated
   polymorphic `(kind,id)` pair.
2. Close the learning-derived chain:
   `subject/contact -> session/event/task_segment -> experience/attribution -> candidate
   -> learning_log -> artifact revision/file -> generated/accumulated suite
   contribution`. Move attributable suite bytes out of candidate JSON into owner-written
   rows and assemble review input through the artifact owner. Deterministically backfill
   every existing array/payload reference, validate tenant/referenced rows, then drop
   `source_experience_ids`, `source_refs`, and JSON provenance/discriminator/reference
   authority. Fail V000353 on source-less or unclassifiable pre-production rows.
3. Keep `candidate_type` as target-domain taxonomy and add required `proposal_kind`.
   Hard-change the state machine:
   `SkillDraft: Proposed -> Evaluating -> Promoted|Rejected`,
   original promoted draft `-> RolledBack`;
   `SkillRollback: Proposed -> Evaluating -> Promoted|Rejected`;
   both reviewable kinds permit owner-only `Evaluating -> Proposed` claim release after
   a transient failure;
   `MemoryAdvisory: Advisory -> Dismissed`;
   authoring kinds `NeedsAuthoring -> Dismissed`.
   Enforce valid `(proposal_kind,status)` pairs and transitions in the database with
   repository CAS as defense in depth.
4. Backfill informational Proposed/Evaluating rows to Advisory or NeedsAuthoring and
   informational Rejected rows to Dismissed; fail on informational Promoted/RolledBack.
   Validate rollback/draft relationships before classification. Skill suggestions
   without a real draft become SkillAuthoring. Then delete payload-string routing,
   permissive status parsing, and the generic unauthenticated
   `SessionStore/update_learning_candidate_status` RPC/DTO/trait method.
   `accept_skill`, `accept_rollback`, and reject require exact reviewable kinds.
5. Require every automatic producer to commit complete normalized sources. Skill
   evidence writes experience/session/event contributions; mining writes typed event
   refs; rollback links the original promotion plus exact revisions; experiment
   proposals link run/trial/score/session/artifact rows. Update experiment and turn-
   execution writers, not only builders. Delete the source-less tenant consolidation
   learning-log emission instead of inventing tenant-wide provenance.
6. Fence privacy before enumeration. Execute order is auth/hold/dual-control, atomically
   claim operation plus destruction fence, enumerate/snapshot normalized closure,
   reverse-derived learning/artifact erase, then existing vault/graph/digest/lineage
   stages. Contribution inserts check the fence and deferred completeness prevents
   insert-then-forget. This task closes the learning-derived branch; it does not claim
   raw session-event/attachment/blob/archive erasure.
7. Make export enumerate every normalized learning-derived level and historical
   disposition through typed joins, never JSON/array/LIKE search. A hold-block operation
   enumerates IDs read-only, mutates zero protected bytes/state, and writes exactly one
   idempotent `retained_legal_hold` decision per record. Dry run persists a typed planned
   disposition with `applied=false`, never a false deletion. Operation/attempt identity
   prevents a later post-hold request from overwriting history.
8. Without a hold, delete or irreversibly redact attributable fields. Sole-source
   candidate/revision/file/suite bytes are removed. `retained_shared` is permitted only
   when retained bytes are proven independent; non-subtractable shared LLM output
   invalidates the whole serving revision and clears definition/source/file/identity
   metadata. Mutation and its decision commit atomically for database records.
9. Expose the existing transactional rollback owner through
   `POST /v1/learning-candidates/accept-rollback` and an
   `learning_candidate_accept_rollback` MCP tool, including contract/discovery entries.
   Authorize before reads, reject wrong candidate kind and stale rollback, and route to
   `LearningReview/accept_rollback`. Add typed `Dismiss`, `LearningReview/dismiss`,
   `POST /v1/learning-candidates/dismiss`, and `learning_candidate_dismiss`. Dismiss uses
   CAS and permits only `Advisory|NeedsAuthoring -> Dismissed`; every other state returns
   a typed conflict and a successful/replayed dismissal writes exactly one durable audit.
   Do not add a generic promotion switch.

**Acceptance:** export exactly enumerates every closure level with tenant/contact
isolation. Sole-source erasure removes all payload, evaluation, artifact definition/source
text/file, and generated/accumulated-suite bytes; shared-source erasure rebuilds or
invalidates the complete serving revision. A legal hold causes zero protected-data
mutation and one idempotent retained decision per enumerated record; dry runs persist
planned unapplied dispositions rather than false deletions; a failed/replayed job resumes
without duplicate decisions; concurrent learning is fenced before enumeration. Exact
proposal-kind backfill and DB constraints
reject every forbidden pair while preserving Skill draft/rollback promotion. Advisory/
authoring items can only be dismissed through audited CAS, never accepted/promoted;
authored drafts are new linked proposals. Authorized HTTP/MCP dismiss routes appear in
discovery, reach the real handler, reject non-informational/current-state conflicts, and
write exactly one audit under replay. HTTP and MCP rollback reach the real authorized
transactional service and reject wrong-kind/stale requests. No active
learning-derived contribution or source byte survives outside a legal hold, and no
legacy provenance array, payload authority, old-status alias, dual read, generic status
RPC, or generic materializer remains.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000353|V000353|v352.*v353/)'
cargo test -p moa-core --locked candidate
cargo nextest run -p moa-memory-pii --locked
cargo nextest run -p moa-brain --locked --lib learning::candidates
cargo nextest run -p moa-skills --locked
cargo nextest run -p moa-artifacts --locked
cargo nextest run -p moa-experiments --locked
cargo nextest run -p moa-eval --locked --lib long_conversation
cargo nextest run -p moa-analytics-export --locked
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db_memory
cargo nextest run -p moa-orchestrator --locked --test skill_learning_review_db
cargo nextest run -p moa-session --locked --test session_db
cargo nextest run -p moa-edge --locked
make test-authz-pentest
cargo clippy -p moa-core -p moa-brain -p moa-skills -p moa-session \
  -p moa-artifacts -p moa-memory-pii -p moa-orchestrator -p moa-wire -p moa-edge \
  -p moa-experiments -p moa-eval -p moa-analytics-export -p moa-migrations \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
! rg -n 'source_experience_ids|source_refs|update_learning_candidate_status' \
  crates/moa-{core,session,brain,skills,orchestrator,wire,edge,experiments,eval}
git diff --check
```

Mutation verification must allow a forbidden proposal-kind/status or transition pair;
route rollback by payload string; restore the generic status RPC; commit a source-less
candidate/revision/suite; accept/promote an authoring item; skip dismiss authz/CAS/audit
or discovery; omit one closure edge or export level; retain a sole-source byte or falsely
retain a non-subtractable shared revision; move enumeration before the fence; bypass a
contribution fence; mutate data under legal hold; mark dry run applied; duplicate a
replayed decision; or route rollback without authz/kind checks. Each exact
status/provenance/source-completeness/erasure/hold/dry-run/idempotency/fence/rollback
assertion must fail before restoration and pass afterward. Verify V000353 against a
fresh isolated database after V000346-V000352; do not wipe the Compose database without
explicit approval.

### Task 5.3 — Produce complete evaluator-linked Behavior Lab scorecards [P1] [ ]

**Depends on:** Tasks 5.1 and 5.2
**Why:** seeded score rows prove query mechanics, not that trials produced every required
piece of deployment evidence.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000354__experiment_score_provenance.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-core/src/types/experiments.rs`
- `crates/moa-wire/src/experiments.rs`
- `crates/moa-artifacts/src/simulation.rs`
- `crates/moa-experiments/src/model.rs`
- `crates/moa-experiments/src/plan.rs`
- `crates/moa-experiments/src/scores.rs`
- `crates/moa-experiments/src/app.rs`
- new deterministic evaluator registry/evidence modules under `crates/moa-experiments/src/`
- `crates/moa-scoring/src/`
- `crates/moa-lineage/core/src/records.rs`
- `crates/moa-lineage/sink/src/writer/rows.rs`
- `crates/moa-lineage/sink/src/writer/storage.rs`
- `crates/moa-lineage/sink/tests/writer_db.rs`
- `crates/moa-orchestrator/src/lineage.rs`
- `crates/moa-orchestrator/src/services/experiments.rs`
- `crates/moa-orchestrator/src/runtime/endpoint.rs`
- `crates/moa-orchestrator/src/workflows/experiment_trial_run.rs`
- `crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs`
- `crates/moa-orchestrator/tests/behavior_lab_simulation_e2e.rs`
- `crates/moa-orchestrator/tests/experiment_trial_run_e2e.rs`
- `.github/workflows/integration-tests.yml`

**Implementation:**

1. Hard-replace arbitrary `Value` scorecards and
   `score_names + evaluator_metadata` with one shared typed `ExperimentScorecard`.
   Every requirement carries evaluator ID/version, exact output score name/value type,
   validated deterministic config, and Blocking or Informational effect. Reject empty
   required sets, duplicates, unknown evaluator/version, invalid thresholds, and
   stochastic/informational evaluators marked blocking. Update artifact/wire/plan
   generation in the same break; keep no old parser or dual form.
2. Own product evaluators in `moa-experiments`, not the separate regression-eval system
   or orchestrator. The initial deterministic registry covers target completion without
   provider/runtime error, result production, configured token/cost/turn budgets, and
   privacy-safe visible output using the existing deterministic PII classifier version.
   Scenario-quality judging remains nightly/informational.
3. Refactor both target paths to return typed terminal evidence, including the token/
   cost observations currently emitted only as telemetry. One workflow finalizer derives
   deterministic scores, derives `score_id` from score run + evaluator ID/version + score
   name + exact target, uses one replay-stable timestamp, and emits one durable
   `LineageEvent::Eval` batch through an injected score-capable `LineageHandle`. The
   lineage sink remains the only production writer; null/OTLP-only lineage cannot claim
   score completion, and there is no direct-SQL fallback.
4. Evaluate before terminal `Completed`. Journal acceptance is durable enqueue, not SQL
   visibility; journaled Restate steps poll Postgres until every exact score row is
   query-visible, then persist Completed. Append failure or bounded visibility timeout
   persists a safe stable failure code. Provider/runtime failures emit blocking evidence
   before Failed; a completed target with a privacy/policy blocker is Ineligible.
5. Add V000354 typed experiment score provenance: experiment run, pinned plan revision,
   trial, exact target session or execution run, evaluator ID/version, bounded evidence
   references/hash, and score-run linkage. Enforce exact foreign-key/tenant
   relationships and immutable replay. Replace score upsert mutation with byte-identical
   replay acceptance; a same-ID provenance collision fails.
6. Add a raw tenant-scoped exact-row query in `moa-scoring`; keep scorecard completeness
   policy in `moa-experiments`. Eligibility is
   `Incomplete | Eligible | Ineligible | Invalid` and requires exactly one row for every
   typed blocking requirement with exact evaluator/version/value type/plan/trial/target/
   evidence linkage. Wrong or duplicate rows never satisfy the gate.
7. For plan-backed Behavior Lab, trial score runs are authoritative. Compute run/scenario/
   variant scorecards from exact trial rows and delete the separate run-level-score
   fallback/requirement. This is Behavior Lab scorecard eligibility, not an agent
   deployment guard until deployment explicitly consumes it.
8. Add deterministic scripted coverage to default CI. The billed plan-to-trial-to-score
   smoke remains ignored, requires both live flags and credentials, and is
   workflow-dispatch/environment-approved only.

**Acceptance:** removing one required evaluator result blocks eligibility; trial
Completed never precedes exact SQL visibility; journal acceptance is not mistaken for
visibility; scores cannot attach to another score run/plan/trial/session/execution run or
rewrite provenance on replay; evaluator version and evidence hash are identity; provider
and privacy blockers make the scorecard ineligible; informational results cannot block;
OTLP remains the telemetry default but cannot masquerade as the durable product score
store; default CI is deterministic/unbilled and the opted-in live smoke fails clearly on
missing credentials.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-core --locked experiment
cargo nextest run -p moa-artifacts --all-targets --locked
cargo nextest run -p moa-lineage-core --locked
cargo nextest run -p moa-lineage-sink --locked --lib
cargo nextest run -p moa-experiments --locked --lib evaluator
cargo test -p moa-orchestrator --locked --lib experiment --no-run
cargo test -p moa-orchestrator --locked \
  --features integration,provider-overrides \
  --test behavior_lab_simulation_e2e \
  --test experiment_trial_run_e2e --no-run
cargo clippy -p moa-core -p moa-wire -p moa-artifacts -p moa-scoring \
  -p moa-experiments -p moa-lineage-core -p moa-lineage-sink \
  -p moa-orchestrator --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

At the assembled large-task boundary:

```bash
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000354|V000354|v353.*v354/)'
cargo nextest run -p moa-lineage-sink --locked --test writer_db \
  -E 'test(/experiment.*score|score.*provenance|score.*replay/)'
cargo nextest run -p moa-experiments --locked --test experiment_store_db \
  -E 'test(/scorecard|evaluator|eligibility|provenance/)'
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features integration,provider-overrides --run-ignored ignored-only \
  --test behavior_lab_simulation_e2e --test experiment_trial_run_e2e \
  -E 'test(/score|evaluator|eligibility/)'
```

With explicit billing authorization only:

```bash
MOA_RUN_LIVE_E2E=1 MOA_RUN_LIVE_PROVIDER_TESTS=1 \
  cargo nextest run -p moa-orchestrator --locked --test experiment_trial_run_e2e -E 'test(/live/)'
```

Mutation verification must omit a required result or evaluator version from score
identity; use a non-replay-stable timestamp; attach the wrong score run/plan/trial/
session/execution target; mutate provenance on conflict; mark Completed before SQL
visibility; treat journal acceptance as visibility; remove provider/privacy blocking;
allow Informational to block; bypass lineage with direct SQL; let null/OTLP-only lineage
claim completion; or restore the any-row-per-trial gate. Each exact-row, replay-count,
linkage, visibility-ordering, sole-writer, and eligibility assertion must fail before
restoration.

### Task 5.4 — Schedule memory-quality computation and retention [P2] [ ]

**Depends on:** Tasks 0.2 and 3.2
**Concurrency:** do not run with Task 3.3; both own
`crates/moa-orchestrator/src/services/graph_memory_maint.rs`.
**Why:** outcome-weighted quality scoring exists, but has no production schedule,
minimum-evidence gate, bounded tenant/contact processing, scoped app-role writer, or
retention owner. The manual raw-pool path cannot safely become production behavior.

**Files:**

- `Cargo.lock`
- `crates/moa-config/src/lib.rs`
- `crates/moa-config/src/memory.rs`
- `crates/moa-memory/lifecycle/Cargo.toml`
- `crates/moa-memory/lifecycle/src/lib.rs`
- `crates/moa-memory/lifecycle/src/quality.rs`
- `crates/moa-orchestrator/src/services/graph_memory_maint.rs`
- `crates/moa-orchestrator/src/runtime/jobs.rs`
- `crates/xtask/src/compute_memory_quality_scores.rs`
- `crates/moa-retrieval/src/retrieval/enrichment.rs`
- `crates/moa-memory/lifecycle/tests/memory_lifecycle_db_memory/quality_postgres_db_memory.rs`
- `docs/04-memory-architecture.md`

**Implementation:**

1. Add typed `MemoryQualityConfig` under `MemoryConfig`: mode
   `ReportOnly | Apply` (default ReportOnly), positive lookback/minimum weighted
   uses/batch size/retention, and nonnegative late-outcome buffer. Validate before I/O.
   Effective retention is
   `max(lineage_retention_days, lookback_days + late_outcome_buffer_days)`. Add no flat
   legacy fields, aliases, optional compatibility mode, or old fallback defaults.
2. Replace raw-pool mutation with one lifecycle owner shared by Restate and xtask. Use a
   bounded control-plane scope discovery read, then Task 3.2's final app-role
   `ScopedConn` with exact tenant/contact `RlsContext` for every scoring/delete query.
   Bind scope, contact, cursor, cutoff, and limit; never hydrate source text, accept
   request principals, cross tenants, or bypass ACL/RLS.
3. Capture one journaled `run_at`, page scopes by `(tenant_id, contact_id)` and node UIDs
   in stable order, and bound every query by `batch_size`. Preserve rank-tiered
   attribution and Beta(1,1), but write only when weighted uses meet the configured
   minimum; sparse candidates remain exactly neutral `0.5`. Bump changelog only when a
   score actually changes. Replay/rerun cannot double-update or double-bump.
4. Add `GraphMemoryMaint::score_quality` typed request/report DTOs. A manual request may
   narrow to one tenant but cannot supply contacts, cursors, principals, retention
   cutoffs, or apply policy. Each DB batch is a named `ctx.run`; a batch failure stops
   that scope and prevents its retention. Reports contain bounded operational
   counts/timestamps only, never tenant/contact/node/content/principal labels.
5. Reconcile one stable UTC default cron with explicit version and empty payload through
   the existing job owner. Keep xtask as a feature-gated operator entrypoint over the
   same lifecycle/config/cursors; make `--help` print usage and exit successfully.
6. Run retention only after all scoring batches for the exact scope succeed. ReportOnly
   counts without mutation. Apply deletes `retrieval_lineage` only before the captured
   safe horizon in stable bounded batches. Failed/cancelled/partial scopes retain all
   lineage; never delete task segments, query traces, learning/source provenance, current
   windows, or late-outcome-buffer rows.
7. Add bounded-label accepted/dropped enrichment counters by job kind
   (`access | lineage`) and quality metrics for mode, duration, lag, candidates, sparse,
   would-change/applied, prune/deleted/retained, and failures. Preserve queue depth/batch
   size; never label by tenant/contact/node/query/source/principal.
8. Update `docs/04-memory-architecture.md` from deferred scheduling/pruning to the shipped
   cron/manual owner, report-only default, sparse gate, scoped RLS, safe retention,
   late-outcome protection, and bounded metrics.

**Acceptance:** default scheduled/manual runs are report-only and mutate no score,
changelog, or lineage. Apply changes only sufficiently evidenced candidates; sparse
scores stay neutral. App-role `ScopedConn` prevents wrong tenant/contact/empty-scope
reads and writes. Stable cursors/batches plus journaled time make crash replay/rerun
idempotent. Pruning occurs only after a complete successful scope and never shortens the
lookback plus late-outcome horizon. Cron and xtask share one owner/config/math/cursors.
Metrics are bounded and identifier-free. No raw production-pool writer, owner bypass,
compatibility config, dual scorer, fallback retention, or second scheduler remains.

**Verification:**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo test -p moa-config --locked memory::tests
cargo test -p moa-retrieval --locked enrichment
cargo test -p moa-orchestrator --locked services::graph_memory_maint
cargo test -p moa-orchestrator --locked runtime::jobs
MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa \
  cargo nextest run -p moa-memory-lifecycle --locked --profile db-memory \
  --test memory_lifecycle_db_memory -E 'test(/quality/)'
cargo run -p xtask --locked --features eval-tools -- \
  compute-memory-quality-scores --help
cargo clippy -p moa-config -p moa-db -p moa-memory-lifecycle -p moa-retrieval \
  -p moa-orchestrator -p xtask \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

The DB lane must prove it created an isolated database and executed app-role queries; a
missing URL, skipped body, zero-test selector, or owner-only pass is invalid. Mutation
verification must separately remove the sparse gate, let ReportOnly mutate, use raw or
wrong-scope DB access, remove cursor/limit, double-bump replay, shorten the safe horizon,
prune before completion/after failure, alter the cron target/version, split xtask
semantics, and remove accepted/drop metrics. Each exact assertion must fail before
restoration and pass afterward.

### Task 5.5 — Measure and choose one semantic-graph policy [P2] [ ]

**Depends on:** Task 3.2
**Why:** semantic graph data is written while main tenant retrieval disables graph
expansion, so MOA may pay extraction/storage cost without retrieval value.

**Files:**

- `crates/moa-config/src/memory.rs`
- `crates/moa-knowledge/src/ingestion/materialization.rs`
- `crates/moa-retrieval/src/retrieval/`
- `crates/moa-eval/src/memory_eval/runner/quality.rs`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Add one overall semantic-graph policy plus extraction cost, graph contribution,
   quality, and latency telemetry.
2. Run a controlled retrieval evaluation after occurrence identity and source ACLs are
   correct. Ship opt-in SourceGraph only if it meets the declared bar; otherwise disable
   model-backed semantic writes and retain only demonstrated deterministic value.
3. Record the measured decision and remove any permanent write-only semantic path.

**Acceptance:** the selected policy cites quality, latency, and cost measurements;
disabled retrieval does not silently retain model-backed writes; enabled retrieval
preserves occurrence ACL admission before expansion.

**Verification:**

```bash
cargo nextest run -p moa-eval --locked memory_eval
cargo nextest run -p moa-knowledge --locked --test knowledge_db_memory
cargo nextest run -p moa-retrieval --locked
```

## M6 — Production Operations, Scale, and Simplification

Tasks 6.1, 6.3, 6.4, and 6.5 are parallel-safe once their stated prerequisites are
complete and their listed write sets stay disjoint. Task 6.2 follows 6.1. Task 6.6
converges the runtime-owner changes from 2.3, 6.1, 6.4, and 6.5.

### Task 6.1 — Make observability and durable lineage production-correct [P1] [ ]

**Depends on:** Tasks 0.2, 2.1, 3.3, 5.2, and 5.3
**Scheduling:** run after Task 5.3 because both own lineage-writer storage. Task 1.3
documentation must already be landed. Reserve
`V000355__lineage_journal.sql`; Task 6.2 uses V000356 and Task 6.5 uses V000357.
**Why:** OTLP currently exports traces but not runtime metrics; production assumes
invalid/load-balanced Prometheus scrapes; alert rules are undeployed; local lineage
journals cannot survive non-sticky multi-replica Deployments; and detached audit/writer
tasks plus incomplete shutdown lose accepted records during rollout.

**Files:**

- `Cargo.toml`
- `Cargo.lock`
- `.env.example`
- `docker-compose.yml`
- `crates/moa-config/src/lib.rs`
- `crates/moa-config/src/telemetry.rs`
- `crates/moa-config/src/lineage.rs`
- `crates/moa-config/src/env_overlay/mod.rs`
- `crates/moa-config/src/env_overlay/tests.rs`
- `crates/moa-observability/Cargo.toml`
- `crates/moa-observability/src/telemetry.rs`
- `crates/moa-observability/src/runtime_metrics.rs`
- new `crates/moa-migrations/migrations/postgres/V000355__lineage_journal.sql`
- `crates/moa-migrations/migration-ownership.toml`
- `crates/moa-migrations/src/lib.rs`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-lineage/sink/Cargo.toml`
- delete `crates/moa-lineage/sink/src/fjall_journal.rs`
- `crates/moa-lineage/sink/src/lib.rs`
- `crates/moa-lineage/sink/src/error.rs`
- `crates/moa-lineage/sink/src/mpsc_sink.rs`
- `crates/moa-lineage/sink/src/schema.rs`
- `crates/moa-lineage/sink/src/store.rs`
- delete `crates/moa-lineage/sink/src/writer/journal.rs`
- new `crates/moa-lineage/sink/src/writer/acceptance.rs`
- `crates/moa-lineage/sink/src/writer/mod.rs`
- `crates/moa-lineage/sink/src/writer/supervisor.rs`
- `crates/moa-lineage/sink/src/writer/storage.rs`
- `crates/moa-lineage/sink/src/writer/retry.rs`
- `crates/moa-lineage/sink/src/writer/compliance.rs`
- `crates/moa-lineage/sink/src/writer/tests.rs`
- `crates/moa-lineage/sink/tests/writer_db.rs`
- `crates/moa-orchestrator/src/lineage.rs`
- `crates/moa-orchestrator/src/ctx.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/main.rs`
- `crates/moa-orchestrator/src/workflows/tenant_purge/repository.rs`
- `crates/moa-orchestrator/tests/orchestrator_db/lineage_postgres_db.rs`
- `crates/moa-orchestrator/tests/orchestrator_db_memory/tenant_purge_repository_db_memory.rs`
- `crates/moa-ocsf/src/lib.rs`
- `crates/moa-ocsf/src/audit_sink.rs`
- `crates/moa-ocsf/src/emit.rs`
- `crates/moa-ocsf/tests/ocsf_db/background_audit_writer_db.rs`
- `crates/moa-auth/authz/src/lib.rs`
- `crates/moa-auth/authz/src/client.rs`
- `crates/moa-auth/authz/src/require.rs`
- `crates/moa-auth/authz/tests/authz_db/require_audit_db.rs`
- `crates/moa-edge/src/main.rs`
- `crates/moa-edge/src/routes.rs`
- `crates/moa-edge/tests/direct_read_routes_db.rs`
- new `crates/moa-edge/tests/direct_read_routes_db/graceful_shutdown_db.rs`
- `crates/moa-test-support/Cargo.toml`
- `crates/moa-test-support/src/orchestrator_fixture.rs`
- `crates/moa-test-support/src/orchestrator_fixture/process.rs`
- `crates/moa-test-support/src/orchestrator_fixture/otlp_capture.rs`
- `crates/moa-orchestrator/tests/execution_run_service_e2e/observability.rs`
- `k8s/base/20-orchestrator-deployment.yaml`
- `k8s/base/25-orchestrator-service.yaml`
- `k8s/base/26-orchestrator-network-policy.yaml`
- `k8s/base/50-edge-deployment.yaml`
- `k8s/base/55-edge-service.yaml`
- `k8s/base/15-runtime-config.yaml`
- `k8s/overlays/production/kustomization.yaml`
- `k8s/overlays/production/patches/orchestrator-observability.yaml`
- `k8s/overlays/production/patches/edge-observability.yaml`
- delete `k8s/observability/10-alloy-config.yaml`
- new `k8s/observability/config.alloy`
- `k8s/observability/15-alloy-rbac.yaml`
- new `k8s/observability/25-alloy-pvc.yaml`
- `k8s/observability/20-alloy-deployment.yaml`
- `k8s/observability/kustomization.yaml`
- new pinned Restate v1/v1beta1 and PrometheusRule schemas under `k8s/schemas/`
- new `ops/prometheus/alerts/kustomization.yaml`
- all existing `ops/prometheus/alerts/*.yaml`
- `k8s/scripts/smoke.sh`
- new `k8s/scripts/validate-observability.sh`
- `k8s/scripts/observability-smoke.sh`
- `.github/workflows/ci.yml`
- `docs/01-architecture-overview.md`
- `docs/08-security.md`
- `docs/10-technology-stack.md`
- `docs/23-environment-variables.md`

**Implementation:**

1. Add FORCE-RLS `analytics.lineage_journal` with UUID identity, tenant/partition/user
   scope, payload, accepted/available times, attempts, expiring lease pair, stable claim
   index, and purge indexes. Only the internal runtime/background role can access it.
   Register/own/test V355 idempotently.
2. Make Postgres the sole durable lineage acceptance owner. `record_durable_batch`
   inserts the whole batch transactionally and returns only after commit. A bounded local
   channel is wake/ingress optimization, never durability. Replicas poll and claim ordered
   batches with expiring leases and `FOR UPDATE SKIP LOCKED`. Recoverable failures retry;
   permanent payload failures atomically dead-letter/dequeue. Postgres final writes,
   compliance/dead-letter, and dequeue are one transaction. Preserve optional ClickHouse
   at-least-once/dedupe but production explicitly uses Postgres. Delete fjall, lockfiles,
   journal-path config, and every pod-local/dual path.
3. Coordinate delivery with tenant/subject purge by the existing fence or narrow
   tenant/partition advisory lock. Purge pending/leased journal rows and prove no accepted
   lineage reappears after purge across lease/retry/commit races.
4. Make `LineageSinkRuntime`/`WriterHandle` own its task and expose
   Running/Draining/Failed/Stopped, fatal error, pending count, oldest age, and last
   claim/flush. Add max-pending-age config. Readiness fails for task death/fatal state,
   Postgres failure, or over-age eligible backlog; liveness remains process health.
   Shutdown timeout preserves committed journal rows for another replica.
5. Replace OCSF/authz globals with explicit `AuditEmitter` plus owned receiver,
   cancellation, and `JoinHandle`. Queue APIs take the emitter; startup is fallible;
   shutdown closes admission, drains, and joins. `FgaClient`, edge state, and runtime deps
   receive exactly one instance-owned audit runtime. Denied authz remains synchronous
   fail-closed. Remove `OnceLock`, global configure/init, and replacement drain globals.
6. Both binaries handle SIGINT/SIGTERM. Order: readiness false; stop accepts/new Restate
   delivery; drain in-flight/jobs; drain lineage then audit; flush meter then tracer.
   `TelemetryGuard` owns both providers and has an explicit consuming shutdown; Drop is
   best-effort only. Publish distinct live/ready probes and render them correctly.
7. Hard-replace metrics config with
   `MetricsExporter::{Otlp, Prometheus, Disabled}`, default Otlp. Prometheus is explicit
   dev mode with required listen address. Remove/reject old enabled/listen and lineage
   journal-path keys. `MOA_OTLP_ENDPOINT` is a collector base URL; derive `/v1/traces`
   and `/v1/metrics` and reject signal-specific URLs. Wire OTel metrics plus exact
   compatible `metrics-exporter-otel` bridge while preserving histogram boundaries and
   identical trace/metric resource identity.
8. Extend the real OTLP fixture to decode both signals and assert expected trace, runtime
   metric, and identical required service/environment/release/deployment resource
   attributes. Trace-only, mismatched resource, or old endpoint fails.
9. Deploy one pinned Alloy replica with Recreate and a 20Gi RWO PVC/WAL; no latest,
   emptyDir, or second replica. Receive MOA OTLP metrics, remote-write them, remove fake
   MOA 9090 scrape/service/network-policy surfaces, and retain only real scrape targets.
   Make `mimir.rules.kubernetes` the sole selected PrometheusRule synchronizer with
   minimal RBAC, explicit Mimir query/rules URLs, Secret credentials, and converted
   selected alert resources. Replace fjall alerts with Postgres journal/backlog/retry/
   dead-letter/writer/drain alerts.
10. Vendor pinned Restate and PrometheusRule schemas with source/checksum. Manifest
    validation renders all overlays and runs pinned strict kubeconform with no missing-
    schema ignore. Add pinned Alloy config validation and promtool rule checks; CI
    checksum-installs tools and runs both contracts.
11. Make observability smoke opt-in and fail-closed: real marked traffic, Mimir metric/
    resource query, temporary canary rule sync, Postgres final-lineage plus drained
    journal assertion, graceful edge/orchestrator rotation within bounds, persisted
    audit/lineage, and no new drop/fatal/drain-timeout signals. Clean every temporary
    resource on success/failure and never print Secrets.
12. Update source docs to Postgres acceptance/lease/atomic-dequeue/purge, instance-owned
    workers, readiness/shutdown, OTLP default, explicit Prometheus dev mode, one Alloy
    rule owner, and only new environment keys.

**Acceptance:** committed lineage survives pod death and another replica completes it;
final Postgres write/dequeue and permanent dead-letter/dequeue are atomic; retryable
failures preserve rows; purge cannot race lineage back after completion. Dead/failed or
over-age writers fail readiness, not liveness. Audit/lineage workers are instance-owned
with no global/detached path. SIGTERM stops new work before bounded drains and meter/
tracer shutdown; timeout preserves accepted rows. OTLP is default and emits trace+
runtime metrics with matching resources; old config/signal URLs fail. MOA exposes no
fake 9090 endpoint. One exact-tag Alloy replica has persistent WAL and sole rule sync.
All rendered manifests/CRDs/rules/config validate strictly. Live smoke proves Mimir,
rules, lineage drain, pod rotation, audit persistence, and zero new failure signals.

**Verification:**

```bash
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-migrations
cargo nextest run -p moa-migrations --locked
cargo test -p moa-config --locked
cargo nextest run -p moa-observability --locked
cargo nextest run -p moa-lineage-sink --locked --test writer_db
cargo nextest run -p moa-ocsf --locked --test ocsf_db
cargo nextest run -p moa-authz --locked --test authz_db
cargo nextest run -p moa-edge --locked --test direct_read_routes_db
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db_memory
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration --run-ignored ignored-only \
  --test execution_run_service_e2e -E 'test(/observability/)'
./k8s/scripts/smoke.sh --validate-manifests
./k8s/scripts/validate-observability.sh
bash -n k8s/scripts/smoke.sh
bash -n k8s/scripts/validate-observability.sh
bash -n k8s/scripts/observability-smoke.sh
! rg -n 'MOA_METRICS_ENABLED|MOA_METRICS_LISTEN|metrics\.enabled|metrics\.listen|MOA_LINEAGE_JOURNAL_PATH|journal_path' \
  .env.example docker-compose.yml crates docs k8s
! rg -n 'fjall|FjallJournal|configure_security_audit|init_background_audit|static (SINK|AUDIT).*OnceLock' \
  crates/moa-lineage crates/moa-config crates/moa-ocsf crates/moa-auth docs
! rg -n 'grafana/alloy:latest|moa-(edge|orchestrator).*9090|emptyDir:' \
  k8s/observability k8s/base k8s/overlays/production
cargo clippy -p moa-config -p moa-observability -p moa-lineage-sink \
  -p moa-ocsf -p moa-authz -p moa-edge -p moa-orchestrator \
  --all-targets --all-features --locked -- -D warnings
cargo build --workspace --all-features --locked
git diff --check
```

Cluster-mutating smoke remains explicitly gated:

```bash
MOA_RUN_LIVE_OBSERVABILITY_SMOKE=1 ./k8s/scripts/observability-smoke.sh
```

Mutation verification must separately return success before journal commit; delete
before final commit; disable lease expiry/polling then kill claimant; dead-letter a
retryable error; race purge; kill writer while process lives; leave over-age backlog;
drop audit on SIGTERM; flush traces but not metrics; restore old config or signal URL;
disable metrics only; weaken Alloy PVC/replicas/tag; remove rule label/RBAC; corrupt a
custom-resource field; break one rule/Alloy reference; replace live assertions with
prints; and time out drain while deleting accepted rows. Each deterministic assertion or
explicit live smoke must fail before restoration and pass afterward.

### Task 6.2 — Add terminal-session archival and retention [P1/P2] [ ]

**Depends on:** Tasks 1.1 and 6.1
**Why:** append-only session history has no normal lifecycle boundary, causing unbounded
storage, backup, and operational cost.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000356__session_event_archives.sql`
- new `crates/moa-session/src/archive.rs`
- `crates/moa-session/src/store/`
- `crates/moa-orchestrator/src/services/session_store/`
- new `crates/moa-orchestrator/src/workflows/session_retention.rs`
- `crates/moa-orchestrator/tests/orchestrator_db/session_store_db.rs`
- `crates/moa-session/tests/session_db/events_partitioning_db.rs`
- `crates/moa-session/tests/session_db/events_append_only_db.rs`

**Implementation:**

1. Make `moa-session` own terminal-session eligibility, archive, verification, hydrate,
   and purge-after-archive.
2. Use a durable workflow that checks legal holds, tenant policy, checkpoint/replay
   sufficiency, export/erasure state, and archive integrity.
3. Preserve append-only triggers on active history; never delete arbitrary prefixes.
4. Add time subpartitioning only if partition/backup measurements require it.

**Acceptance:** active, held, uncheckpointed, or unverifiably archived sessions cannot
purge; hydrated archives reproduce visible history/integrity; retry/cancel is idempotent
and composes with tenant purge.

**Verification:**

```bash
cargo nextest run -p moa-session --locked --test session_db
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db
MOA_RUN_LIVE_E2E=1 cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration --run-ignored ignored-only --test tenant_purge_service_e2e
```

### Task 6.3 — Make MCP catalogs and large tool loadouts deterministic [P2] [ ]

**Depends on:** Task 2.4
**Why:** one connector error/name collision can block router construction, while tools
after the lexical first 30 can become unavailable for reasons unrelated to the task.

**Files:**

- `crates/moa-hands/src/adapters/mcp/mod.rs`
- `crates/moa-hands/src/core/construction.rs`
- `crates/moa-hands/src/core/registration.rs`
- `crates/moa-brain/src/pipeline/tools.rs`
- `crates/moa-core/src/types/agent.rs`
- `crates/moa-execution/src/capability.rs`
- `crates/moa-hands/tests/hands_offline/mcp_router.rs`
- inline tests in `crates/moa-brain/src/pipeline/tools.rs`

**Implementation:**

1. Store server-qualified stable tool references/schema hashes in the existing capability
   catalog; do not add another catalog.
2. Mark connectors required/optional, retain last-known-good revisions, refresh in the
   background, expose health, and pin a session's selected schema revision.
3. Replace alphabetical truncation with a small control set plus explicit agent/skill
   dependencies and capability priority. Canonicalize ordering after selection.
4. Add lazy discovery/load. Do not add per-turn semantic ranking until a >30-tool
   scenario proves the simpler policy insufficient.

**Acceptance:** an optional outage does not block unrelated tools; required outages fail
with typed health; same inputs/revision yield the same schemas/order; a declared tool
after lexical position 30 remains available.

**Verification:**

```bash
cargo nextest run -p moa-hands --locked --test hands_offline
cargo test -p moa-brain --locked tool_definitions
```

### Task 6.4 — Coordinate provider pacing/cooldown without replacing concurrency [P2] [ ]

**Depends on:** Task 0.2
**Why:** one provider key's pacing, 429 cooldown, and retry budget can be multiplied by
replicas even though concurrency admission is already global.

**Files:**

- `crates/moa-core/src/traits/runtime_cache.rs`
- `crates/moa-runtime-store/src/memory.rs`
- `crates/moa-runtime-store/src/redis.rs`
- `crates/moa-config/src/providers.rs`
- `crates/moa-config/src/env_overlay/providers.rs`
- `crates/moa-providers/src/core/global_concurrency.rs`
- `crates/moa-providers/src/core/pacer.rs`
- `crates/moa-providers/src/core/rate_guard.rs`
- `crates/moa-providers/src/core/retry.rs`
- `crates/moa-providers/src/core/concurrency_factory.rs`
- `crates/moa-providers/tests/providers_offline.rs`

**Implementation:**

1. Add bounded atomic token-bucket/cooldown/retry-budget operations keyed by provider,
   opaque credential/quota identity, model, and rate class.
2. Keep healthy global concurrency and remove its hidden coordination-store global by
   constructor injection.
3. Apply the configured coordination-failure policy to every distributed provider
   control, including existing global-concurrency acquisition and missing-store startup.
   `bounded_degraded` may use the current per-process semaphore with an explicit metric,
   fleet-ceiling warning, and duration; `fail_closed` rejects admission. A deliberate
   local-only mode is config, not an error fallback.
4. Never let store/semaphore errors become unbounded and never key or label by raw secret.

**Acceptance:** two replica simulations share pacing/cooldown/retry state; tests
distinguish locally bounded from globally enforced behavior; missing-store startup and
runtime-store failure follow policy and cannot silently multiply the fleet ceiling or
retry-storm; healthy global concurrency behavior remains.

**Verification:**

```bash
cargo nextest run -p moa-runtime-store --locked
cargo nextest run -p moa-providers --locked --test providers_offline
cargo test -p moa-providers --locked global_concurrency
```

### Task 6.5 — Enforce one canonical effective sandbox profile in every HandProvider [P1/P2] [ ]

**Depends on:** Tasks 0.2, 1.3, 2.1, and 3.2. V000357 remains reserved but cannot
merge/deploy/apply until V000348–V000356 are present in order.
**Why:** CPU, memory, disk, egress, idle, and maximum lifetime are not policy if router
provisioning substitutes defaults, policy layers never reach provisioning, adapters
silently ignore fields, recovered leases carry no policy identity, or hard lifetime has
no destruction owner.

**Files:**

- new `crates/moa-migrations/migrations/postgres/V000357__hand_lease_effective_profile.sql`
- `crates/moa-migrations/tests/run_idempotency_db.rs`
- `crates/moa-core/src/types/hands.rs`
- `crates/moa-core/src/types/agent.rs`
- `crates/moa-core/src/types/guardrails.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-config/src/sandbox.rs`
- `crates/moa-config/src/lib.rs`
- `crates/moa-config/src/env_overlay/mod.rs`
- `crates/moa-config/src/env_overlay/providers.rs`
- `crates/moa-config/src/env_overlay/tests.rs`
- `crates/moa-hands/Cargo.toml`
- new `crates/moa-hands/src/core/profile.rs`
- new `crates/moa-hands/src/core/reaper.rs`
- `crates/moa-hands/src/core/mod.rs`
- `crates/moa-hands/src/core/construction.rs`
- `crates/moa-hands/src/core/registration.rs`
- `crates/moa-hands/src/core/dispatch.rs`
- `crates/moa-hands/src/core/recovery.rs`
- `crates/moa-hands/src/core/recovery/tests.rs`
- `crates/moa-hands/src/core/lifecycle.rs`
- `crates/moa-hands/src/core/leases.rs`
- `crates/moa-hands/src/adapters/local/mod.rs`
- `crates/moa-hands/src/adapters/e2b/mod.rs`
- `crates/moa-hands/src/adapters/e2b/tests.rs`
- `crates/moa-hands/src/adapters/daytona/mod.rs`
- `crates/moa-orchestrator/src/objects/tenant.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/runtime/jobs.rs`
- `crates/moa-orchestrator/src/main.rs`
- `crates/moa-hands/tests/hands_offline/security_defaults.rs`
- `crates/moa-hands/tests/hands_offline/local_tools_offline.rs`
- `crates/moa-hands/tests/docker_hardening_docker.rs`
- `crates/moa-hands/tests/local_tools_docker.rs`
- `crates/moa-hands/tests/local_tools_support/common.rs`
- `crates/moa-hands/tests/local_tools_support/docker.rs`
- `crates/moa-hands/tests/local_tools_support/offline.rs`
- `crates/moa-hands/tests/daytona_live.rs`
- `crates/moa-hands/tests/e2b_live.rs`
- all `HandSpec`, `HandRoute`, `ToolRouter::new`, `HandProvider`, agent-policy, and
  tenant-policy constructors/fixtures found by compile-guided search
- `.env.example`
- `k8s/base/15-runtime-config.yaml`
- `docs/01-architecture-overview.md`
- `docs/06-hands-and-mcp.md`
- `docs/08-security.md`
- `docs/10-technology-stack.md`
- `docs/23-environment-variables.md`

**Implementation:**

1. Hard-replace the partial contract with one required six-dimension
   `effective_profile`: typed bounded-nonzero or explicit `Unbounded` CPU, memory,
   ephemeral disk, idle, and maximum lifetime plus typed
   `DenyAll | AllowList { revision, destinations } | Unrestricted` egress. Reject zero,
   empty revision, malformed destination, idle above hard lifetime, missing fields,
   legacy names, defaults, `Option`, aliases, and numeric sentinels. Canonicalize
   allowlists; an empty intersection becomes DenyAll.
2. Require `SandboxPolicySnapshot { revision, profile }` for deployment config, current
   tenant config, pinned `AgentPolicySnapshot`, and every `HandRoute`. Resolve the
   restrictive intersection: lowest bounded limits, Unbounded identity, DenyAll
   dominance, allowlist intersection, Unrestricted identity. Include all four revisions,
   provider/capability revision, canonical serialization, and stable SHA-256 profile
   hash. Absence is an error, never inferred unrestricted policy.
3. Inject deployment policy into `ToolRouter`; read current tenant policy for every tool
   invocation; require the agent/request snapshots at compile time. Do not indefinitely
   cache tenant policy or reuse an old session snapshot as the current tenant source.
4. Hard-add required, non-default `HandProvider::capabilities()` covering supported
   tiers, resource ranges/granularity, egress modes, idle/hard-lifetime enforcement
   owners, and stable revision. Update every production/test implementor. Reject before
   lease claim/provision when any effective dimension is unsupported; fallback providers
   rerun resolution/capability checks.
5. Translate or reject all six fields per adapter. Local host accepts only enforceable
   dimensions. Local Docker deliberately maps proven CPU/memory/egress and rejects
   allowlist/disk limits without real enforcement. E2B/Daytona serialize only documented,
   fixture-proven fields and reject the rest before create. Lifecycle/reaper enforcement
   may be advertised only when that durable owner is installed.
6. Persist exact profile/hash/source revisions/capability revision plus separate nullable
   `idle_expires_at` and immutable `hard_expires_at` on the generation-fenced lease.
   Unbounded maps to NULL. Renewal may advance idle only up to hard and never changes
   hard/profile/hash/provenance. Recovery/reuse recomputes and exactly matches active
   generation, both deadlines, and profile hash; mismatch fences/destroys/reprovisions.
7. Add an independent durable reaper started by runtime jobs. Postgres claims bounded
   expired/stale/abandoned generations with row locks, generation tokens, and
   `SKIP LOCKED`. Claim before idempotent provider destroy and finalize only the claimed
   generation. Failures stay fenced/retryable with bounded backoff; never reactivate.
   Cloud durable-hand startup fails if the reaper/lease owner is absent.
8. V000357 renames old expiry to idle expiry, adds profile/hash/revisions/capability/hard
   expiry and active/provisioning constraints, enforces idle ≤ hard, replaces the reaper
   index, and marks every legacy active/provisioning row stale and immediately
   destroyable instead of inventing permissive policy. Prove fresh and complete
   V356→V357 upgrade/idempotency/constraints/index/nonreuse.
9. Emit only provider/tier/hash/revisions/egress mode+revision/deadline class/destroy
   reason; never policy payloads, allowlist contents, paths, env, or credentials. Update
   architecture and environment docs to the shipped owner/enforcement contract.

**Acceptance:** `HandSpec` has one required six-dimension profile; stale partial/legacy
JSON fails. Every policy layer and provider capability is explicit and hash-significant.
Typed Unbounded supports deliberate local development without zero/None/missing fields.
Intersection is deterministic/restrictive. Every provider has a required capability
implementation and translates or rejects every field before create. Leases persist and
recompute exact policy identity; any revision/hash/generation change prevents reuse.
Idle renewal cannot extend hard lifetime. Competing replicas destroy hard-expired
sandboxes without new traffic; destroy failures remain fenced/retryable. Migrated legacy
active leases are cleanup work, never reusable unrestricted sandboxes. Cloud fails
without required policy/store/reaper. Docker-selected tests fail clearly if Docker is
unavailable.

**Verification:**

```bash
cargo fmt --all -- --check
cargo test -p moa-core --locked types::hands
cargo nextest run -p moa-config --locked
cargo nextest run -p moa-hands --locked --lib
cargo nextest run -p moa-hands --locked --test hands_offline
docker info
cargo nextest run -p moa-hands --locked --test docker_hardening_docker
cargo nextest run -p moa-hands --locked --run-ignored ignored-only \
  --test local_tools_docker
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline \
  -E 'test(/sandbox_policy|hand_lease_reaper|tool_executor/)'
cargo nextest run -p moa-orchestrator --locked --test orchestrator_db \
  -E 'test(/hand_lease_reaper|tenant_sandbox_policy/)'
cargo run -p xtask --locked -- check-migrations
cargo test -p xtask --locked check_migrations
cargo nextest run -p moa-migrations --locked --run-ignored ignored-only \
  --test run_idempotency_db -E 'test(/v000357/)'
cargo clippy -p moa-core -p moa-config -p moa-hands -p moa-migrations \
  -p moa-orchestrator --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
git diff --check
```

Mutation verification must separately remove each policy layer; choose the largest limit;
deserialize missing/zero as unbounded; union allowlists or weaken DenyAll; omit a source
or capability revision from the hash; advertise/omit an adapter field; create after
rejection; recover changed hash/generation; let idle change/exceed hard expiry; require
future traffic for destruction; remove reaper generation/`SKIP LOCKED`; reactivate after
destroy failure; backfill reusable unrestricted legacy rows; drop migration constraints;
and restore Docker's silent early return. Exact serialization/resolution/adapter/lease/
reaper/startup/migration/Docker assertions must fail before restoration and pass after.

### Task 6.6 — Remove concrete dependency and placeholder debt [P2] [ ]

**Depends on:** Tasks 2.3, 6.1, 6.4, and 6.5
**Why:** correctness-sensitive globals and an eight-trait aggregate hide dependencies;
a permanently registered 501 route advertises a nonexistent contract.

**Files:**

- `crates/moa-orchestrator/src/ctx.rs`
- `crates/moa-orchestrator/src/runtime/deps.rs`
- `crates/moa-orchestrator/src/action_reviews/app.rs`
- `crates/moa-orchestrator/src/services/tool_executor.rs`
- `crates/moa-orchestrator/src/objects/session/mod.rs`
- `crates/moa-orchestrator/src/objects/worker/mod.rs`
- `crates/moa-core/src/traits/mod.rs`
- `crates/moa-edge/src/routes.rs`
- `crates/moa-core/src/error.rs`
- `crates/xtask/src/check_architecture_boundaries.rs`

**Implementation:**

1. Remove `SessionRepository`; inject `SessionEventLookupStore` into ActionReviews,
   `SessionStore` plus lookup into ToolExecutor, and `SessionStore` into Session/Worker.
2. Remove install-once owners already replaced by earlier tasks: provider coordination,
   tenant credentials, OCSF/authz audit, and raw OrchestratorCtx reads. Leave immutable
   caches, semaphores, stemmers, and metric handles.
3. Remove `/v1/security/secret-scanning/github` registration while preserving its regex
   contract test. Delete `MoaError::NotImplemented` only after no construction site
   remains. Preserve feature-disabled Auth0 behavior.
4. Remove old import paths at their callers and reduce architecture allowances; add no
   replacement facade, re-export, deprecated alias, or wrapper trait.

**Acceptance:** consumers expose only used traits; targeted multi-runtime tests do not
depend on global install order; no public route permanently returns Not Implemented;
intentional feature gating remains explicit.

**Verification:**

```bash
rg -n 'SessionRepository|OrchestratorCtx::current\(|NotImplemented' crates
cargo nextest run -p moa-orchestrator --locked --test orchestrator_offline
cargo nextest run -p moa-edge --locked
cargo run -p xtask --locked -- check-architecture-boundaries
```

### Task 6.7 — Resolve optional subsystem decisions with evidence [P2 decision gate] [ ]

**Depends on:** Tasks 5.5 and 6.1
**Why:** undeployed machinery has maintenance cost, but deleting implemented capability
only because production wiring is absent is also speculative.

**Files:**

- `crates/moa-analytics/`
- `crates/moa-analytics-export/`
- `crates/moa-analytics/src/clickhouse_exec.rs`
- `crates/moa-orchestrator/src/main.rs`
- `crates/moa-config/src/clickhouse.rs`
- `k8s/overlays/production/kustomization.yaml`
- `docs/10-technology-stack.md`
- `docs/21-tenant-knowledge-base.md`

**Implementation:**

1. Measure analytics volume, Postgres query latency/cost, compliance retention, and real
   ClickHouse deployment ownership.
2. Choose one: support ClickHouse with owner/SLO, isolate it behind an optional feature
   and CI lane, or remove it and exporter state.
3. Record Task 5.5's semantic graph decision in the knowledge architecture doc.
4. Do not keep dual unowned paths or compatibility shims after a decision.

**Acceptance:** each optional subsystem has owner, supported deployment/test path, and
SLO, or is removed completely; decisions cite measurements and update architecture.

**Verification:**

```bash
cargo metadata --format-version 1 --no-deps --locked
cargo run -p xtask --locked -- check-architecture-boundaries
kubectl kustomize k8s/overlays/production >/tmp/moa-production.yaml
git diff --check
```

## M7 — End-to-End Verification [ ]

### Task 7.1 — Certify the complete program

**Depends on:** every retained implementation task
**Why:** these changes cross durable contracts. Focused green tests are insufficient
unless replay, authorization, persistence, provider, manifest, and workspace boundaries
are revalidated together.

**Execution:**

1. Use the repo `certify` skill to select the deterministic/live matrix for completed
   milestones. Do not run billed/live providers without explicit approval.
2. Run formatting, architecture, focused lanes, clippy, build, manifest validation, and
   diff checks in that order.
3. Mutation-check each substantial new test by briefly breaking its pinned behavior,
   confirming expected failure, restoring, and rerunning.
4. Review migrations, RLS, auth-before-read, inverse OpenFGA outbox, Restate replay,
   generation fencing, safe errors, and secret absence.
5. Record actual commands/duration and explicitly deferred live checks. Compilation alone
   never completes a task.
6. For every replaced contract, run a residual scan for its old identifiers and inspect
   the diff for aliases, deprecated wrappers, re-exports, dual writes/reads, or fallback
   branches. Any residue fails certification even when tests pass.

**Required commands:**

```bash
cargo fmt --all -- --check
cargo run -p xtask --locked -- check-architecture-boundaries
cargo run -p xtask --locked -- check-migrations
! rg -n 'K_PENDING|drain_pending_messages|SessionRepository|MoaError::NotImplemented' crates
make test-fast
make test-db-session
make test-db-memory
make test-authz-pentest
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
kubectl kustomize k8s/overlays/production >/tmp/moa-production.yaml
./k8s/scripts/smoke.sh --validate-manifests
git diff --check
```

Run the deterministic service lane when local Restate/Postgres/OpenFGA is needed:

```bash
MOA_RUN_LIVE_E2E=1 ./scripts/run-clean-e2e.sh --live
```

Expected: every command exits 0, scenario assertions observe intended durable behavior,
rendered manifests pass schema/smoke validation, and no secret or cross-tenant data
appears in state, events, logs, errors, fixtures, or snapshots.

## Explicit Non-Goals

- No backwards compatibility, compatibility shims, deprecated aliases, transitional
  wrappers, re-exported old paths, or simultaneous old/new persistence contracts.
- No rewrite of working admission capacity leases.
- No broad public API rewrite or frontend implementation in this Rust plan.
- No merging of approval/review write state machines.
- No implicit Email/SMS conversational mode.
- No per-turn semantic tool ranking until dependencies/lazy loading prove insufficient.
- No generic candidate materializer, repository facade, internal service mesh, or Restate
  CRUD extraction program.
- No ClickHouse or semantic-graph deletion without the measurement gate.
- No Merkle publisher until the compliance tier is committed, durable lineage is live,
  and external cryptographic review is available.

## Completion Standard

The program is complete only when final certification passes, production manifests
select the intended secure/durable behavior, every failure is pinned by a production-path
test, every replaced contract has zero residual compatibility surface, and architecture/
subsystem docs describe the resulting ownership without presenting temporary exceptions
as permanent architecture.

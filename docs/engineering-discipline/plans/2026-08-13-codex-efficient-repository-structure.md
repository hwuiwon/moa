# Codex-Efficient Repository Structure

## Objective

Reduce the context, duplicated analysis, repeated compilation, and log volume required to work safely across the whole MOA repository. Improve routing and ownership without creating compatibility, network, or facade layers; fix only runtime defects directly exposed by certification.

## Constraints

- `docs/01-architecture-overview.md` and `docs/15-architecture-policy.md` remain the ownership sources of truth.
- Refactors follow durable behavior boundaries, never line count alone.
- Existing public paths and runtime contracts stay unchanged unless direct imports can be updated in the same private module boundary; no compatibility re-exports.
- Restate journal step names, serialization, SQL ordering, fencing, provider I/O ordering, and idempotency keys are preserved.
- Live, billed, credentialed, and 24h/7d checks remain explicitly authorized external gates.
- Parallel workers receive only mapped paths and documents. One integration owner runs broad validation.

## Repository-Wide Design

### Validated subsystem registry

Add `.agents/subsystems.toml` as the single path-routing registry. Cover every workspace crate plus repository operations with grouped entries for:

1. platform core and configuration;
2. execution domain and artifacts;
3. orchestration, edge, sessions, and messaging;
4. hands and sandbox workspaces;
5. connectors, knowledge, and outbound security;
6. providers and model governance;
7. memory, retrieval, and brain context;
8. auth, principals, and contacts;
9. lineage, observability, and analytics;
10. skills, experiments, eval, load test, and test support;
11. migrations and database operations;
12. repository tooling, deployment, and documentation.

Each entry records stable ownership, exact path prefixes, canonical docs, applicable local `AGENTS.md`, deterministic test profiles or Make targets, and structured live prerequisites. Longest-prefix resolution is deterministic; the validator rejects ambiguous prefixes and uncovered workspace members.

Add `xtask check-subsystems` to validate the registry and `xtask plan-subsystem-audit` to turn a base revision or explicit paths into bounded context packets under `target/agent-audits/`. The planner caps reviewers, emits selected paths/docs/tests/live gates, and does not launch agents itself.

### Instruction hierarchy

Extend root `AGENTS.md` with a bounded audit/implementation/certification workflow:

- resolve the subsystem registry before broad reading;
- use a maximum of four read-only discovery agents by default;
- send minimal context and disjoint ownership;
- reconcile evidence before editing;
- use one integration owner for broad Cargo/E2E runs;
- cap command output and persist summaries/artifacts;
- checkpoint completed phases so later work can start in a fresh session;
- never infer authorization for billed/live gates.

Add short local instruction files only where policy differs materially:

- `crates/moa-orchestrator/AGENTS.md`
- `crates/moa-execution/AGENTS.md`
- `crates/moa-hands/AGENTS.md`
- `crates/moa-memory/AGENTS.md`
- `crates/moa-auth/AGENTS.md`
- `crates/moa-connectors/AGENTS.md`

### MOA-wide structural inventory

Add `docs/engineering-discipline/repository-structure-inventory.md`. Rank large and change-central production files as:

- `split-now`: a verified behavior boundary, independent consumers/tests, and a safe atomic write set exist;
- `keep`: size is justified by one cohesive generated/table-driven/state-machine owner;
- `investigate`: size or fan-in is notable, but evidence is insufficient for a safe extraction.

This tranche implements only the three independently verified `split-now` items below. Other entries become explicitly routed follow-up work instead of hidden debt or speculative churn.

Certification exposed three adjacent defects that were repaired in their
existing owners: duplicate same-tenant admission locks, checkpoint replay and
failed-generation liveness, and a burst test that used a test-only 60-second
pseudo-SLO instead of the checked-in 120-second production alert contract.

## Parallel Implementation Tasks

### Task A - Repository-wide routing and bounded workflow

Write set:

- `.agents/subsystems.toml`
- root `AGENTS.md`
- six local `AGENTS.md` files listed above
- `crates/xtask/src/subsystem_map.rs`
- `crates/xtask/src/main.rs`
- `crates/xtask/README.md`
- `docs/engineering-discipline/repository-structure-inventory.md`

Requirements:

- Validate all configured paths, docs, local instructions, owners, profiles/targets, and live-gate structure.
- Unit-test longest-prefix routing, ambiguity rejection, missing-path rejection, reviewer capping, and deterministic packet output.
- Reuse checked-in nextest/Make identifiers; do not duplicate test implementation or create an autonomous Codex launcher.

### Task B - Execution task-attempt behavior modules

Write set:

- `crates/moa-orchestrator/src/workflows/execution_task_attempt.rs`
- `crates/moa-orchestrator/src/workflows/execution_task_attempt/active.rs`
- `crates/moa-orchestrator/src/workflows/execution_task_attempt/{external,yielding,watchdog}.rs`
- new `execution_task_attempt/continuation.rs`
- new `execution_task_attempt/active/{heartbeat,capability,agent}.rs`

Move continuation schema, heartbeat fencing, direct capability execution, and agent execution to their behavior owners. Keep `execute_task_attempt`, exit routing, and genuinely shared helpers in the thin parent. Preserve serialized shapes, model/tool progression, journal names, provider ordering, and sibling visibility. Move existing inline tests with their owners.

### Task C - Pending-terminal compensation coordination

Write set:

- `crates/moa-execution/src/repository/compensation.rs`
- new `crates/moa-execution/src/repository/compensation/pending_terminal.rs`

Move the terminal fence/drain/finalization state machine into a private child module while keeping public `ExecutionRepository` methods and result types at their existing paths. Keep shared compensation attempt primitives in the parent. Preserve SQL transaction order, row locks, capacity accounting, paging, replay, and terminal replacement behavior.

### Task D - Sandbox workspace lifecycle behavior modules

Write set:

- `crates/moa-hands/src/core/sandbox_workspace/lifecycle.rs`
- new `crates/moa-hands/src/core/sandbox_workspace/lifecycle/{management,materialization,commit,execution_release}.rs`

Move management operations, initial materialization/hydration, commit publication, and execution-release recovery into private child modules. Keep shared commit result, lease attachment, and abandoned-checkpoint cleanup in the parent. Preserve operation-ledger order, manifest locking, provider I/O boundaries, commit-before-release, exact receipt fencing, and current public `ToolRouter` methods.

## Dependencies And Execution Order

1. Complete the read-only seam and repository-wide inventory.
2. Run Tasks A-D in parallel because their Rust write sets do not overlap.
3. Reconcile visibility and formatting centrally after all workers stop editing.
4. Run focused tests once per touched crate, then strict Clippy and workspace build once.
5. Run independent read-only review against this plan.

## Verification

Deterministic gates:

```bash
cargo run -p xtask --locked -- check-subsystems
cargo test -p xtask --locked subsystem_map
cargo test -p moa-orchestrator --lib --locked execution_task_attempt
cargo test -p moa-execution --lib --locked repository::compensation
cargo test -p moa-hands --lib --locked sandbox_workspace
cargo clippy -p xtask -p moa-orchestrator -p moa-execution -p moa-hands --all-targets --all-features --locked -- -D warnings
cargo build --workspace --locked
cargo fmt --all --check
git diff --check
```

Run the focused existing DB/service cases selected by the registry when local services are available. Compile ignored live targets if their code moved, but do not set live/billing flags. Report credentialed provider E2E and 24h/7d canaries as not run unless separately authorized.

## Rollback And Review Boundaries

- Each task is independently revertible by write set.
- If an extraction needs public compatibility exports, changes SQL/provider ordering, or expands beyond the listed files, stop and re-plan rather than widening the patch.
- A failing pre-existing test must be reproduced against the base revision before classification; do not weaken it.

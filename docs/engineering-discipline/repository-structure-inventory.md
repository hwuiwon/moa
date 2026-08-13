# Repository Structure Inventory

This inventory routes structural work by verified behavior seams, not line count
alone. `split-now` means an extraction boundary is evidenced and can be planned;
it does not mean the extraction is complete. `investigate` needs a bounded
read-only seam review first. `keep` records a cohesive owner that should not be
split merely because it is large.

## Current tranche

Only these private extractions are in the current implementation tranche:

- `crates/moa-execution/src/repository/compensation.rs`: pending-terminal
  fence, drain, and finalization state machine.
- `crates/moa-hands/src/core/sandbox_workspace/lifecycle.rs`: management,
  materialization, commit, and execution-release behavior.
- `crates/moa-orchestrator/src/workflows/execution_task_attempt/active.rs`:
  heartbeat, direct-capability, and agent-attempt behavior.

These current-tranche extractions are complete and certified by focused
behavior/persistence tests, strict Clippy, the workspace build, and the
deterministic long-horizon service lane. Every other `split-now` row below is a
phased follow-up with a separate future write set.

## Split now

| Path | Verified seam | Status |
|---|---|---|
| `crates/moa-execution/src/repository/compensation.rs` | Pending-terminal coordination is a private transaction/fence state machine distinct from shared compensation-attempt primitives. | Current tranche; certified |
| `crates/moa-orchestrator/src/workflows/execution_task_attempt/active.rs` | Heartbeat fencing, direct capability execution, agent turns, and continuation state are distinct bounded-attempt behaviors. | Current tranche; certified |
| `crates/moa-execution/src/repository/task.rs` | Attempt admission/liveness, checkpoints, external jobs, settlement, and capacity accounting form independently testable repository behavior families. | Phased follow-up |
| `crates/moa-orchestrator/src/services/tool_executor.rs` | External-job adapters, scoped catalog/policy construction, governed dispatch, and callback/recovery behavior have separate consumers and fixtures. | Phased follow-up |
| `crates/moa-core/src/types/execution_planning.rs` | Routing evidence, durable-upgrade transitions, plan/goal contracts, task outcomes, and audit envelopes are stable type families inside one owning module. | Phased follow-up |
| `crates/moa-hands/src/core/sandbox_workspace/lifecycle.rs` | Management, hydration/materialization, commit publication, and execution-release recovery preserve distinct provider-I/O and ledger boundaries. | Current tranche; certified |
| `crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs` | Target preparation, session ownership/resume, observation, usage, and terminal scoring are separable workflow behaviors with durable ordering constraints. | Phased follow-up |
| `crates/moa-retrieval/src/retrieval/legs.rs` | Graph expansion/policy, exact seeds, lexical/vector legs, temporal scoring, and diagnostic assembly are independently testable retrieval stages. | Phased follow-up |
| `crates/moa-eval/src/kernel/stats.rs` | Bootstrap estimation, paired tests, multiple-comparison correction, arm summaries, and deterministic sampling are distinct statistical algorithms. | Phased follow-up |
| `crates/xtask/src/execution_trace_manifest.rs` | Manifest data, discovery, source loading, sender/receiver audits, and diagnostics are separate repository-validation responsibilities. | Phased follow-up |

## Investigate

| Path | Question to resolve before extraction |
|---|---|
| `crates/moa-providers/src/registry.rs` | Determine whether model catalog data, provider construction, capability lookup, and governance policy have independent consumers or intentionally share one exhaustive registry. |
| `crates/moa-memory/ingest/src/slow_path.rs` | Trace transaction, contradiction, extraction, and graph/vector write ordering before proposing a seam; ingestion correctness may require one coordinated owner. |
| `crates/moa-artifacts/src/validation.rs` | Measure which validation families already delegate to child modules and whether another extraction reduces coupling without fragmenting one canonical artifact contract. |

## Keep

| Path | Reason |
|---|---|
| `crates/moa-brain/src/pipeline/memory.rs` | One cohesive context-pipeline memory stage owns retrieval request construction, degradation, rendering, and stage telemetry. |
| `crates/moa-connectors/src/executor.rs` | One constrained HTTP execution boundary intentionally keeps destination admission, credential application, request execution, and durable outcome normalization together. |
| `crates/moa-experiments/src/plan.rs` | One canonical experiment-plan contract and validation state machine benefits from exhaustive, colocated semantics. |

## Review rule

Promoting an `investigate` or `keep` entry to `split-now` requires a read-only
report naming the behavior owner, callers, tests, invariant-preserving move, and
disjoint write set. Completing a `split-now` row requires the focused tests plus
the repository integration owner’s final validation; a file move alone is not
completion.

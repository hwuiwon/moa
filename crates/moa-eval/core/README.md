# moa-eval-core

Production-safe evaluation contracts and scoring helpers: the suite and case
types (`TestSuite`, `TestCase`, `AgentConfig`), the `Evaluator` trait, and the
deterministic built-in evaluators. Deliberately separate from the heavy
`moa-eval` harness so runtime crates such as `moa-skills` and the orchestrator
can depend on evaluation contracts without pulling in the brain or memory
stacks.

## Structure

- `types.rs` — serializable suite and agent configuration types.
- `loader.rs` — file loaders for evaluation suites and agent configs.
- `evaluator.rs` — evaluator traits for scoring agent outputs after a suite
  run.
- `evaluators/` — built-in evaluators (output match, trajectory match, tool
  success, threshold) and scoring helpers.
- `engine.rs` — shared eval run options and result aggregates.
- `results.rs` — result and metrics types produced by evaluation runs.
- `plan.rs` — dry-run planning and provider-independent cost arithmetic.
- `replay.rs` — shared replay scoring primitives such as token F1.
- `conversation_cost.rs` — per-turn and per-conversation tool-call, token, and
  coordination KPIs reconstructed from a session's durable event log.
- `error.rs` — error types for loading and reporting evaluation artifacts.

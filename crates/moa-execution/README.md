# moa-execution

Pure compilation and interpretation for durable MOA execution runs. The
orchestrator's durable workflows call into this crate for deterministic
plan compilation, scheduling, and completion/replan evaluation, keeping that
logic testable outside Restate.

## Modules

- `compiler` — initial-plan compilation and restricted amendment validation.
- `interpreter` — pure execution scheduling.
- `completion` — deterministic completion evaluation.
- `replan` — deterministic replan-stop evaluation.
- `capability` — capability catalogs, estimates, and canonical hashes.
- `bindings` — restricted execution binding resolution.
- `budget` — integer-only run budget accounting.
- `schema` — Draft 2020-12 schema validation.
- `state` — public execution projection and task state.
- `wire` — public execution service and internal durable-workflow wire
  contracts.
- `repository` — scoped PostgreSQL execution-run persistence.
- `error` — crate error contract.

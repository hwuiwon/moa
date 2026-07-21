# moa-experiments

Domain types for MOA behavior-lab experiment runs and scorecard configuration.
The crate intentionally stays below the orchestration layer: it models
experiment definitions and durable records without depending on Restate or
`moa-orchestrator`.

## Structure

- `app.rs` — application boundary for behavior-lab experiment service
  operations.
- `model.rs` — typed experiment definitions and run records.
- `plan.rs` — pure helpers for expanding behavior-lab experiment plans.
- `scores.rs` — experiment-owned score sources, trial joins, summaries, and
  comparisons.
- `store.rs` — storage boundary for durable experiment records.

# xtask

Repository maintenance and eval tooling commands, invoked as
`cargo xtask <command>` (aliased to `cargo run -p xtask -- <command>`).

## Structure

Default commands:

- `audit-paths` — forbid reintroduction of removed code patterns (connector
  and envelope-encryption code) across the workspace.
- `check-architecture-boundaries` — enforce crate dependency and architecture
  rules, including the execution trace manifest audit.
- `check-migrations` — enforce central migration files, ban non-central
  `migrations/` directories, and check table ownership.

## Features

- `eval-tools` — enables the eval and memory-benchmark commands
  (`check-eval-budgets`, `calibrate-external-memory-judge`,
  `compare-eval-reports`, `compute-memory-quality-scores`, `execution-eval`,
  `fetch-memory-benchmark`, `generate-memory-eval-corpus`,
  `record-memory-extractions`, `record-memory-merges`,
  `run-external-memory-eval`, `run-memory-retrieval-eval`, `wixqa-rag-eval`),
  which pull in `moa-eval`, memory, and provider crates. Run them with
  `cargo run -p xtask --features eval-tools -- <command>`.

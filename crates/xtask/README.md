# xtask

Repository maintenance and eval tooling commands, invoked as
`cargo xtask <command>` (aliased to `cargo run -p xtask -- <command>`).

## Structure

Default commands:

- `audit-paths` — forbid reintroduction of removed code patterns (connector
  and envelope-encryption code) across the workspace.
- `check-architecture-boundaries` — enforce crate dependency and architecture
  rules, including the execution trace manifest audit.
- `check-migrations` — enforce the flat canonical `V000001..V00000N` central
  sequence, ban non-central `migrations/` directories, and require exact table
  ownership.

## Features

- `eval-tools` — enables the eval and memory-benchmark commands
  (`check-eval-budgets`, `calibrate-external-memory-judge`,
  `certify-platform-simulator`, `compare-eval-reports`,
  `compute-memory-quality-scores`, `execution-eval`,
  `fetch-memory-benchmark`, `generate-memory-eval-corpus`,
  `record-memory-extractions`, `record-memory-merges`,
  `run-external-memory-eval`, `run-memory-retrieval-eval`, `wixqa-rag-eval`),
  which pull in `moa-eval`, memory, and provider crates. Run them with
  `cargo run -p xtask --features eval-tools -- <command>`.

`certify-platform-simulator` is the operator-only ingestion seam for an
externally produced, canonical `FidelityStudyArtifact`. It requires an admin
database connection capable of assuming `moa_promoter`, and never accepts a
caller-supplied verdict, bounds, authorization, or mandate. The command can only
name the fixed migration-owned platform mandate:

```bash
MOA_DATABASE_ADMIN_URL=postgres://... \
  cargo run -p xtask --features eval-tools -- certify-platform-simulator \
  --artifact path/to/canonical-study.json \
  --mandate-id 00000000-0000-4000-8000-0000000d75f2
```

Before this command can succeed, a reviewed code-and-migration revision must pin
real cohort and external source-manifest digests in its new fixed mandate, and a
separate promoter evidence import must approve the exact canonical study hash
against that source manifest. The initial migration mandate is explicitly
`UNPROVISIONED` and is rejected, so a fabricated aggregate artifact cannot
bootstrap platform release authority. The promoter may append evidence imports
but cannot update or delete the migration-owned mandate.

Failed and inconclusive studies are still recorded for auditability, but the
command exits nonzero. Success means the fixed platform release simulator
revision is certified and resolvable at the current time.

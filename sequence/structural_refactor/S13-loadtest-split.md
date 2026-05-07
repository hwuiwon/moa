# S13 — Split `moa-loadtest/src/lib.rs` (2,195 LOC) and `scenarios/retrieval.rs` (1,303 LOC)

## Scope

Cut the load-test crate's main lib + the largest scenario file. **Mechanical split.** No scenario behavior changes.

## Preconditions

- S01–S12 complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

`moa-loadtest` is a load-test harness; size grew because the harness, scenario runner, metrics collection, and CLI glue all landed in `lib.rs`. The retrieval scenario file is similarly tangled — likely contains the scenario definition, fixture seeding, and assertion logic in one file.

## Files in scope

- `crates/moa-loadtest/src/lib.rs` → split
- `crates/moa-loadtest/src/scenarios/retrieval.rs` → split if >700 LOC after prompt
- Other scenario files only if they exceed 700 LOC

## Files explicitly out of scope

- Other small scenario files (under 700 LOC)
- Any tests
- The runner binary if it lives in `bin/` or similar

## Step-by-step instructions

### Part A — `lib.rs` split

1. Read end-to-end. Sections:
   - `Harness` struct + lifecycle (setup, teardown)
   - Scenario trait / runner
   - Metrics collection (latency histograms, throughput counters)
   - Reporting (summary print, JSON output)
   - CLI glue (or this lives in a separate binary file)

2. Target structure:
   ```
   src/
   ├── lib.rs           — module declarations + public surface re-exports
   ├── harness.rs       — Harness struct + lifecycle
   ├── runner.rs        — scenario trait + per-scenario execution
   ├── metrics.rs       — histograms, counters, accumulators
   ├── reporting.rs     — summary formatting, JSON output
   └── scenarios/       — already a folder
       ├── mod.rs
       ├── retrieval.rs (Part B may split this)
       └── ...
   ```

### Part B — `scenarios/retrieval.rs`

3. Read end-to-end. Likely sections:
   - Scenario struct + `impl Scenario`
   - Fixture seeding (creating test data — many embeddings, many sessions, many memory pages)
   - Per-iteration logic (the request the load-test makes)
   - Assertion / verification logic (sanity checks)

4. Target structure:
   ```
   scenarios/retrieval/
   ├── mod.rs           — Scenario impl + thin glue
   ├── fixtures.rs      — fixture seeding
   ├── iteration.rs     — per-iteration request logic
   └── assertions.rs    — verification
   ```

### All parts

5. Run verification.

6. Document anomalies in `REFACTOR_NOTES.md` under `[S13]`.

## Verification

```bash
cargo check -p moa-loadtest --all-targets
cargo clippy -p moa-loadtest --all-targets -- -D warnings
cargo test -p moa-loadtest --no-run
cargo check --workspace --all-targets

# File sizes
find crates/moa-loadtest/src -name '*.rs' -exec wc -l {} + | awk '$1 > 700 {print "TOO BIG:", $0}'
```

## Acceptance criteria

- [ ] `crates/moa-loadtest/src/lib.rs` is under 300 LOC.
- [ ] `crates/moa-loadtest/src/scenarios/retrieval.rs` no longer exists; replaced by folder (only if it was >700 LOC after lib split).
- [ ] No file in `crates/moa-loadtest/src/` exceeds 700 LOC.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-loadtest/`.

## Notes for the agent

- **Loadtest scenarios are sensitive to ordering.** If a scenario seeds fixtures, runs iterations, and asserts in a specific order, the split must preserve that order. Don't lift assertions to "run after all iterations" — they may run *during* iterations to detect drift.
- **Metrics histograms**: don't change the histogram bucket boundaries during the split. Those are tuning knobs that compare across runs.
- **Time budget**: 1 session.
- **Anti-pattern**: don't introduce a metrics-export trait. The current concrete metrics types are fine.

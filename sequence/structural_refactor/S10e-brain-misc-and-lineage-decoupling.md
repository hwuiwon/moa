# S10e — Split remaining `moa-brain` files >600 LOC + drop direct `moa-lineage-otel` dep

## Scope

Three loose ends in `moa-brain`:
1. Split `tool_stats.rs` (~725 LOC) — telemetry on tool usage
2. Split any other `moa-brain/src/**.rs` file that ended up >600 LOC after S10a–S10d
3. **Remove the direct `moa-lineage-otel` dependency.** Brain should consume lineage via `moa_core::traits::LineageHandle`, not by importing the OTel-specific adapter directly.

## Preconditions

- S10a–S10d complete and merged.
- `cargo check --workspace` is green.
- The audit confirmed `moa-brain` directly imports `moa-lineage-otel`. If that's no longer true at this point (e.g. removed inadvertently in an earlier prompt), Part B is a no-op; verify and skip.

## Why this prompt

Wraps up `moa-brain`. After this prompt, no source file in `moa-brain/src/` should exceed 700 LOC. Also removes the layer-violation in dependency direction: `moa-brain` (cognition) should not know that lineage is delivered via OTel — that's an adapter detail. The whole point of `LineageHandle` in `moa-core` is to be transport-agnostic.

## Files in scope

- `crates/moa-brain/src/tool_stats.rs` → split (or rename if appropriate)
- Any other `moa-brain/src/**.rs` over 600 LOC after prior S10 prompts
- `crates/moa-brain/Cargo.toml` — drop `moa-lineage-otel` from `[dependencies]`
- `crates/moa-brain/src/**/*.rs` — replace `use moa_lineage_otel::...` with `use moa_core::traits::LineageHandle`
- Wherever `moa-brain` constructs / consumes a lineage handle, ensure construction goes through `moa-core` types

## Files explicitly out of scope

- `moa-lineage-otel` itself
- Any other lineage sub-crate
- `moa-brain/tests/`

## Step-by-step instructions

### Part A — `tool_stats.rs`

1. Read end-to-end. Identify sections:
   - Counter / histogram structs (per-tool stats)
   - Aggregation logic (per-session, per-workspace)
   - Reporting / serialization (for memory writes)
   - Time-series helpers (if any)

2. Split into `tool_stats/` folder if the sections are clear:
   ```
   tool_stats/
   ├── mod.rs            — public API
   ├── counters.rs       — per-tool counter primitives
   ├── aggregation.rs    — rolling aggregation
   └── reporting.rs      — serialization for memory writes
   ```

   If `tool_stats.rs` is mostly one coherent unit, leave it; rename to `tool_metrics.rs` only if the current name is misleading (telemetry vs stats vs metrics).

### Part B — Catch-all

3. Run a size audit:
   ```bash
   find crates/moa-brain/src -name '*.rs' -exec wc -l {} + | sort -n | tail -10
   ```
   For any file >600 LOC, decide:
   - Split if it has multiple concerns
   - Leave with a doc comment explaining why if it's a single coherent unit (rare; flag in `REFACTOR_NOTES.md`)

### Part C — Drop `moa-lineage-otel` direct dep

4. Find usages:
   ```bash
   rg "moa_lineage_otel" crates/moa-brain/src/
   ```

5. For each match, replace the import with `moa_core::traits::LineageHandle` (or the relevant lineage trait/type from `moa-core`).

6. **If a method on the OTel adapter is being called that isn't on the trait**, that's a leak. Two options:
   - The method belongs on the trait → add it to `moa-core::traits::LineageHandle` (this expands the trait surface; document)
   - The method is OTel-specific → brain shouldn't be calling it; flag as bug in `REFACTOR_NOTES.md` and refactor brain to use trait-only methods. If brain genuinely needs more capability, add to the trait.

7. Remove `moa-lineage-otel` from `crates/moa-brain/Cargo.toml`.

8. **Verify the lineage handle is constructed elsewhere.** Brain receives a `Box<dyn LineageHandle>` or `Arc<dyn LineageHandle>` from its caller (the orchestrator or runtime). The construction (`OtelLineageHandle::new(...)`) lives outside brain — confirm.

9. If you find construction inside brain, that's the actual layer violation. Move the construction to `moa-runtime` or `moa-cli` (whichever wires up the brain). This may be the bulk of Part C.

### All parts

10. Run verification.

11. Document any expansions to `LineageHandle` trait in `REFACTOR_NOTES.md` under `[S10e]`.

## Verification

```bash
cargo check -p moa-brain --all-targets
cargo clippy -p moa-brain --all-targets -- -D warnings
cargo test -p moa-brain --no-run
cargo check --workspace --all-targets

# Verify dep is gone
grep "moa-lineage-otel" crates/moa-brain/Cargo.toml && echo "FAIL: dep still present" || echo "OK: dep removed"
cargo tree -p moa-brain | grep -i "lineage-otel" && echo "FAIL: transitive dep" || echo "OK: no transitive"

# File-size audit
find crates/moa-brain/src -name '*.rs' -exec wc -l {} + | awk '$1 > 700 {print "TOO BIG:", $0}'
```

## Acceptance criteria

- [ ] No file in `crates/moa-brain/src/` exceeds 700 LOC.
- [ ] `crates/moa-brain/Cargo.toml` does not list `moa-lineage-otel`.
- [ ] `cargo tree -p moa-brain` does not show `moa-lineage-otel` as a direct dep.
- [ ] All previously-OTel-specific calls go through `moa_core::traits::LineageHandle`.
- [ ] If `LineageHandle` trait was expanded, the expansion is documented.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-brain/`. If the trait was expanded in `moa-core`, also `git checkout -- crates/moa-core/src/traits/`.

## Notes for the agent

- **The lineage trait expansion needs care.** Each new method is permanent API. Only add a method if brain genuinely needs it; don't speculatively add.
- **`Box<dyn LineageHandle>` vs `Arc<dyn LineageHandle>`**: depends on whether brain shares the handle across tasks. Read the existing usage; don't change the indirection type unnecessarily.
- **OTel-specific span attributes**: if brain was calling `add_otel_attribute("foo", "bar")`, that's a leak. The trait should expose `add_attribute(key, value)` (transport-agnostic). The OTel adapter translates internally.
- **Transitive deps are different from direct deps.** If `moa-lineage-otel` shows up in `cargo tree` because `moa-runtime` depends on it (and brain depends on runtime), that's fine. The direct dep is what we remove.
- **Don't try to remove other indirect lineage usage.** The brain probably uses `LineageHandle` heavily. That's by design — it should. The fix is making sure brain talks to it through the trait.
- **Time budget**: 1 session. Smaller cleanup, but the dep removal can surface unexpected couplings.

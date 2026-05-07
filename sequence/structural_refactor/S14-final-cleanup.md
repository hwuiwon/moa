# S14 — Workspace-wide final cleanup plan

## Scope

Final low-risk cleanup after S01-S13. This is not another structural split pack.
The goal is to remove obvious leftovers from the refactor, prune unused
manifest entries, replace temporary re-export bridges where safe, and capture a
public API snapshot before moving into the TEST pack.

## Execution Status

Started 2026-05-07.

- Installed `cargo-machete` and `cargo-public-api`.
- Removed unused direct manifest entries found by `cargo machete --with-metadata`.
- Regenerated and verified `workspace-hack` with `cargo-hakari`.
- Replaced temporary wildcard re-exports in `moa-core` and
  `moa-orchestrator::services::session_store` with explicit public lists.
- Removed the `moa-loadtest --scale` alias for `--sessions`.
- Removed the obsolete mirrored config key `general.default_model`; `models.main`
  is now the single model source of truth.
- Gated loadtest's bash mock behavior to tests so production builds do not carry
  a dead-code allowance.

Verification completed:

```bash
cargo fmt --all
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo build --workspace
cargo hakari verify
cargo public-api --package moa-core > /tmp/moa-core-public-api-after.txt
cargo tree --workspace --duplicates > /tmp/moa-tree-duplicates-after.txt
git diff --check
```

`cargo machete --with-metadata` is clean for every non-`workspace-hack` package.
The global run still reports generated dependencies inside
`crates/workspace-hack/Cargo.toml`; those are intentional hakari output and were
not hand-edited.

## Preconditions

- S01-S13 are complete.
- `cargo check --workspace --all-targets` is green.
- The worktree should be made clean except for the S14 cleanup branch changes.
  Current audit note: this checkout has unrelated `graphify-out/` churn and a
  restored `REFACTOR_NOTES.md`; do not mix those changes into S14 unless the
  user explicitly wants graphify artifacts committed.

## Audit Snapshot

Commands run while preparing this plan:

```bash
cargo machete --version
cargo public-api --version
cargo +nightly udeps --workspace --all-targets
cargo hakari verify
cargo tree --workspace --duplicates
cargo xtask audit-paths
rg -n "compat|back-compat|compatibility|deprecated|alias|shim|legacy|temporary|TODO|FIXME|pub use .* as |pub use .*::\*" crates --glob '*.rs' --glob 'Cargo.toml'
find crates -path '*/src/*.rs' -o -path '*/src/**/*.rs' | xargs wc -l | awk '$2 != "total" && $1 > 700 {print}' | sort -n
```

Findings:

- `cargo machete` is not installed.
- `cargo public-api` is not installed.
- `cargo +nightly udeps --workspace --all-targets` completed compilation but
  reported unused dependency candidates. Most `workspace-hack` hits are expected
  false positives from hakari. Potentially actionable candidates to verify with
  `cargo machete` and `rg` before removing:
  - `crates/moa-cli/Cargo.toml`: dev-dependency `expectrl`
  - `crates/moa-lineage/audit/Cargo.toml`: dependencies `arrow`, `parquet`
  - `crates/moa-lineage/cold/Cargo.toml`: dependency `moa-lineage-core`
  - `crates/moa-skills/Cargo.toml`: dev-dependency `tempfile`
- `cargo hakari verify` passed.
- `cargo xtask audit-paths` passed.
- `cargo tree --workspace --duplicates` still reports many existing transitive
  duplicate versions (`reqwest`, `getrandom`, `hashbrown`, `thiserror`, etc.).
  S14 should baseline this output before and after manifest edits, not attempt a
  broad transitive dependency convergence.
- S09/S10e dependency-direction checks are currently clean:
  - `cargo tree -p moa-hands | rg -i "moa-memory-ingest"` finds no normal
    runtime dependency.
  - `cargo tree -p moa-brain | rg -i "moa-lineage-otel"` finds no normal
    dependency.
- Temporary or broad re-export bridges still present:
  - `crates/moa-core/src/config/mod.rs`: `pub use <submodule>::*`
  - `crates/moa-core/src/types/mod.rs`: `pub use <submodule>::*`
  - `crates/moa-core/src/traits/mod.rs`: `pub use embedding::*`
  - `crates/moa-core/src/lib.rs`: `pub use types::*`
  - `crates/moa-orchestrator/src/services/session_store/mod.rs`: `pub use requests::*`
- Compatibility or alias markers to decide:
  - `crates/moa-loadtest/src/main.rs`: `--scale` is an alias for `--sessions`
  - `crates/moa-core/src/config/mod.rs`: test named
    `observability_config_backward_compat`
  - No `moa_memory_vector::Embedder` compatibility alias was found.
- Source files still over 700 LOC after S13:
  - `crates/moa-gateway/src/slack.rs` — 705 LOC
  - `crates/moa-core/src/runtime_metrics.rs` — 707 LOC
  - `crates/moa-gateway/src/discord.rs` — 724 LOC
  - `crates/moa-hands/src/tools/tool_result.rs` — 733 LOC
  - `crates/moa-eval/src/engine.rs` — 742 LOC
  - `crates/moa-gateway/src/renderer.rs` — 757 LOC
  - `crates/moa-memory/ingest/src/slow_path.rs` — 832 LOC
  - `crates/moa-session/src/neon.rs` — 866 LOC
  - `crates/moa-memory/ingest/src/contradiction.rs` — 907 LOC
  - `crates/moa-lineage/sink/src/writer.rs` — 975 LOC

## Cleanup Plan

### 1. Tool bootstrap and baselines

Install missing audit tools if they are not present:

```bash
cargo install cargo-machete --locked
cargo install cargo-public-api --locked
```

Capture baselines before edits:

```bash
cargo check --workspace --all-targets
cargo hakari verify
cargo tree --workspace --duplicates > /tmp/moa-tree-duplicates-before.txt
cargo machete > /tmp/moa-machete-before.txt
cargo public-api --package moa-core > /tmp/moa-core-public-api-before.txt
```

If `cargo public-api` cannot diff against `main` in this checkout, use the
before/after files above and keep them out of the commit.

### 2. Manifest cleanup

Use `cargo machete` as the primary source of truth. Use `cargo udeps` only as a
secondary signal because it flags hakari's `workspace-hack` by design.

For each candidate:

1. Verify with `rg` that the crate is not referenced by source, tests,
   examples, build scripts, or generated macro paths.
2. Remove only the manifest entry.
3. Run the focused package check.
4. Regenerate hakari after all manifest edits.

Initial candidates to check:

- `moa-cli` dev-dependency `expectrl`
- `moa-lineage-audit` dependencies `arrow` and `parquet`
- `moa-lineage-cold` dependency `moa-lineage-core`
- `moa-skills` dev-dependency `tempfile`

Do not remove `workspace-hack` from workspace crates. If `cargo udeps` remains
noisy, add explicit udeps ignore metadata instead of dismantling hakari.

After dependency edits:

```bash
cargo hakari generate
cargo hakari manage-deps --yes
cargo hakari verify
```

### 3. Re-export and compatibility sweep

The S03 prompt intentionally used wildcard `pub use` to preserve API during the
split. S14 is the first safe point to tighten those.

Plan:

1. Replace wildcard re-exports with explicit lists in:
   - `moa-core::config`
   - `moa-core::types`
   - `moa-core::traits`
   - `moa-orchestrator::services::session_store`
2. Decide whether root `moa_core::pub use types::*` is still the intended public
   prelude. If keeping it, document that it is an intentional public API, not a
   compatibility shim.
3. Use `cargo public-api --package moa-core` before and after. If the user wants
   the hard-break behavior requested during S09, API shrinkage is acceptable
   only when it removes temporary bridge exports and all workspace call sites
   compile.
4. Remove the `moa-loadtest --scale` alias if it was introduced only as a
   temporary compatibility bridge. If it is an intentional CLI convenience,
   leave it and document the decision.
5. Rename or revisit `observability_config_backward_compat`; if it is only a
   test name, rename it. If real old-config behavior remains, decide whether the
   hard-break rule should remove it.

### 4. Dead-code and size sweep

S14 should be conservative. It can remove dead private helpers exposed by the
dependency/re-export work, but it should not start another major file-split
campaign.

Actions:

```bash
rg -n "allow\\(dead_code\\)|cfg_attr\\(not\\(test\\), allow\\(dead_code\\)|TODO|FIXME" crates --glob '*.rs'
find crates -path '*/src/*.rs' -o -path '*/src/**/*.rs' | xargs wc -l | awk '$2 != "total" && $1 > 700 {print}' | sort -n
```

Decision rule:

- Remove truly dead private helpers and now-unused imports.
- Leave behavior-sensitive TODOs in place unless they are obviously obsolete
  comments from S01-S13.
- Do not split the >700 LOC files in this prompt unless the change is a tiny
  move-only extraction. Record larger splits as follow-up prompts.

Likely follow-up split candidates, not default S14 edits:

- `moa-lineage/sink/src/writer.rs`
- `moa-memory/ingest/src/contradiction.rs`
- `moa-session/src/neon.rs`
- `moa-memory/ingest/src/slow_path.rs`
- `moa-gateway/src/renderer.rs`

### 5. Verification

Required after edits:

```bash
cargo fmt --all
cargo machete
cargo hakari verify
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo public-api --package moa-core > /tmp/moa-core-public-api-after.txt
cargo tree --workspace --duplicates > /tmp/moa-tree-duplicates-after.txt
diff -u /tmp/moa-tree-duplicates-before.txt /tmp/moa-tree-duplicates-after.txt || true
git diff --check
```

Acceptance notes:

- `cargo machete` should be clean or have documented false positives.
- `cargo public-api` changes should be intentional and documented.
- `cargo tree --duplicates` must not show new duplicate versions caused by S14.
- `workspace-hack` must remain verified after any manifest edit.

## Files In Scope

- Root `Cargo.toml` and crate `Cargo.toml` files for dependency cleanup.
- `crates/workspace-hack/` generated files, only through `cargo hakari`.
- `crates/moa-core/src/{config,types,traits,lib.rs}` for explicit re-exports.
- `crates/moa-orchestrator/src/services/session_store/` for explicit re-exports.
- Small `.rs` edits needed to remove unused imports or dead private helpers.
- `REFACTOR_NOTES.md` for documenting decisions.

## Explicitly Out Of Scope

- New architecture, new crates, new traits, or behavior changes.
- Live/billed tests.
- Broad transitive dependency upgrades.
- Major splits of remaining >700 LOC files unless the user explicitly expands
  S14 into another structural pass.
- `graphify-out/` churn unless the user asks to refresh or commit graphify
  artifacts.

## Rollback Plan

Manifest cleanup is easy to roll back:

```bash
git checkout -- Cargo.toml crates/*/Cargo.toml crates/moa-lineage/*/Cargo.toml crates/moa-memory/*/Cargo.toml Cargo.lock crates/workspace-hack/
```

For source cleanup:

```bash
git checkout -- crates/moa-core/src crates/moa-orchestrator/src/services/session_store crates/
```

Prefer smaller commits inside S14: one commit for dependency cleanup, one for
re-export/API cleanup, and one for dead-code/comment cleanup.

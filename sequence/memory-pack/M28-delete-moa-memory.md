# Step M28 — DELETE entire `moa-memory` crate; final sweep for dead wiki code

_Final cleanup phase. The legacy `moa-memory` crate, all `FileMemoryStore` code, MEMORY.md handlers, _log.md branching/reconciliation, page-level consolidation, FileWiki tsvector index, and the on-disk skill loader are physically removed from the workspace._

## 1 What this step is about

By M27 every consumer (moa-runtime, moa-brain, moa-gateway, moa-orchestrator, moa-cli, moa-desktop) has been migrated to `moa-memory-graph` + `moa-memory-vector` + `moa-memory-pii` + `moa-memory-ingest`. The `moa-memory` crate now contains only dead modules + a few `#[deprecated]` re-export shims. M28 deletes it.

## 2 Files to read

- All M07/M13/M14/M18 cleanup sections (each enumerates files moved or deprecated).
- `Cargo.toml` workspace members list.
- `cargo +nightly udeps --workspace` output.

## 3 Goal

1. `moa-memory` crate directory deleted.
2. `Cargo.toml` workspace.members list updated.
3. All `#[deprecated]` re-export shims removed.
4. `cargo build --workspace` clean.
5. `rg "moa_memory(::|;|/)|moa-memory" -- crates/ Cargo.toml` returns ZERO hits.
6. Any orphaned MEMORY.md / _log.md references in docs deleted.

## 4 Rules

- A pre-flight script asserts no live consumer still imports `moa_memory::*`.
- Migration files that lived in `moa-memory/migrations/` were moved to per-crate dirs in earlier steps; nothing remains here.
- Old skill `.md` files on user disks are NOT touched (orphaned but harmless).

## 5 Tasks

### 5a Pre-flight `xtask audit-moa-memory`

```rust
// xtask/src/main.rs
fn cmd_audit_moa_memory() -> anyhow::Result<()> {
    let output = Command::new("rg")
        .args(["-l", "moa_memory(::|;|/)|use moa_memory|moa-memory", "--", "crates/", "Cargo.toml"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().is_empty() {
        eprintln!("Surviving moa-memory references:\n{}", stdout);
        std::process::exit(1);
    }
    Ok(())
}
```

Run via `cargo xtask audit-moa-memory` in CI.

### 5b Delete the crate directory

```sh
git rm -r crates/moa-memory
```

Run `cargo build --workspace`. Fix any straggler imports (should be none if M01-M27 cleanup was thorough).

### 5c Remove from workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/moa-brain",
    "crates/moa-cli",
    "crates/moa-core",
    "crates/moa-desktop",
    "crates/moa-eval",
    "crates/moa-gateway",
    "crates/moa-hands",
    "crates/moa-loadtest",
    # "crates/moa-memory",   ← REMOVED
    "crates/moa-memory-graph",
    "crates/moa-memory-vector",
    "crates/moa-memory-pii",
    "crates/moa-memory-ingest",
    "crates/moa-orchestrator",
    "crates/moa-orchestrator-local",
    "crates/moa-providers",
    "crates/moa-runtime",
    "crates/moa-security",
    "crates/moa-session",
    "crates/moa-skills",
    "xtask",
]
```

Also drop `moa-memory` from any `[workspace.dependencies]` entries.

### 5d Delete docs

- `docs/04-memory-architecture.md` — obsolete, points at file-wiki design.
- Any `MEMORY.md` template references in skill bootstrap docs.
- `docs/03-state-storage.md` if it described filesystem state.

Replace with a link to `docs/13-graph-memory-architecture.md` (created in M02 documentation pass).

### 5e Delete examples

```sh
rg -l "FileMemoryStore|MEMORY\\.md" examples/
```

Delete every match.

### 5f Update README.md

Replace any "Memory" section that described file-wiki with a short paragraph pointing at the new crates.

### 5g Final dep audit

```sh
cargo +nightly udeps --workspace
cargo tree --workspace | grep -i memory
```

The first should be clean. The second should show only the four new crates and no `moa-memory` (sans dash-prefix).

### 5h CI guardrail

Add to `.github/workflows/ci.yml`:

```yaml
- name: Audit no moa-memory references
  run: cargo xtask audit-moa-memory
```

## 6 Deliverables

- `crates/moa-memory/` directory removed.
- Workspace Cargo.toml updated.
- Docs sweep complete.
- `xtask audit-moa-memory` permanent guardrail.

## 7 Acceptance criteria

1. `test -d crates/moa-memory` returns false.
2. `cargo build --workspace` clean.
3. `cargo test --workspace` green.
4. Zero `rg` hits for the dead module names listed above.
5. `cargo tree --workspace` shows no orphan deps formerly used only by moa-memory (walkdir, etc).
6. `cargo +nightly udeps --workspace` clean.

## 8 Tests

The `xtask audit-moa-memory` script IS the test; CI runs it on every PR.

```sh
cargo build --workspace
cargo test --workspace
cargo xtask audit-moa-memory
cargo +nightly udeps --workspace
```

## 9 Cleanup

This IS the cleanup phase. Confirm:
- No `MEMORY.md` references remain anywhere in source.
- No `_log.md` references remain.
- No `FileMemoryStore`, `FileWiki`, `wiki_branch`, `reconcile_pages` references remain.
- No `moa-memory` directory, Cargo entry, or import.

## 10 What's next

**M29 — Validation: 100-fact ingestion + 10-supersession + retrieval golden test.**

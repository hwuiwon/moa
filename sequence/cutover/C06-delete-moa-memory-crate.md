# Step C06 — Delete the legacy `moa-memory` crate

_Final removal. After C05, `moa-memory` is the only crate in the workspace that fails to build, and no other crate references it. C06 deletes the directory, removes it from the workspace, and adds a CI guardrail that prevents reintroduction._

## 1 What this step is about

This is the lever pull. C01–C05 did the work; C06 is mostly a `git rm`. After this prompt, the legacy file-wiki memory system is gone from the codebase. The repo's only memory subsystem is the graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`).

This is a destructive operation. Make sure C05's acceptance criteria all pass before starting.

## 2 Files to read

- `Cargo.toml` (workspace root) — the `members` and `default-members` lists.
- `Cargo.lock` — will regenerate after the deletion.
- C05 — confirm prerequisites.
- `Dockerfile`, `docker/`, `scripts/`, `.github/workflows/` — anything that might reference `moa-memory` by path.

## 3 Goal

After this step:

- `crates/moa-memory/` does not exist.
- `Cargo.toml` workspace `members` and `default-members` no longer list `crates/moa-memory`.
- `Cargo.lock` regenerated.
- `cargo build --workspace` clean.
- `cargo test --workspace` green.
- A small CI guardrail (xtask or shell script) audits the workspace and fails if `moa-memory` reappears in any form (directory, dep, import).

## 4 Rules

- **C05 acceptance criteria are a precondition.** Confirm before starting.
- **Use `git rm -r`, not `rm -rf`.** Preserves history; the deletion shows up as a coherent commit.
- **One commit.** This whole step is one focused commit titled e.g. `chore(memory): delete legacy moa-memory crate (C06)`.
- **Don't touch `moa-memory-graph` / vector / pii / ingest.** They stay where they are. R01 reorganizes them later.

## 5 Tasks

### 5a Confirm precondition

```sh
# C05 should have left the workspace in this exact state:
cargo build --workspace --exclude moa-memory   # clean
cargo build --workspace 2>&1 | grep "^error" | grep -v "crates/moa-memory/"   # empty

# No external references remain
rg "use moa_memory(::|;|\\s)" crates/ --type rust | grep -v "crates/moa-memory/"   # empty
rg "moa-memory\\s*=" crates/ --type toml | grep -v "moa-memory-"   # empty
```

If any of these fail, **stop**. Loop back to C02/C03/C04/C05 and fix the missed consumer.

### 5b Delete the directory

```sh
git rm -r crates/moa-memory
```

### 5c Update workspace `Cargo.toml`

Open `Cargo.toml` and remove `"crates/moa-memory"` from both `members` and `default-members`. Result:

```toml
[workspace]
resolver = "2"
members = [
    "crates/moa-core",
    "crates/moa-brain",
    "crates/moa-session",
    # "crates/moa-memory",   ← deleted
    "crates/moa-memory-graph",
    "crates/moa-memory-ingest",
    "crates/moa-memory-pii",
    "crates/moa-memory-vector",
    # ... rest unchanged
]
default-members = [
    "crates/moa-core",
    "crates/moa-brain",
    "crates/moa-session",
    # "crates/moa-memory",   ← deleted
    "crates/moa-memory-graph",
    # ... rest unchanged
]
```

### 5d Regenerate `Cargo.lock`

```sh
cargo build --workspace
```

This succeeds (every consumer was migrated in C02–C05) and rewrites `Cargo.lock` to drop `moa-memory`.

```sh
git add Cargo.lock Cargo.toml
```

### 5e Confirm no path leaks

```sh
# Dockerfile, scripts, CI, docs
rg -l "moa-memory[^-]" Dockerfile docker/ scripts/ .github/ ops/ docs/ *.md 2>/dev/null
```

For each match, decide: was this referring to the deleted crate, or to one of the four memory subcrates (`moa-memory-graph` / `vector` / `pii` / `ingest`)? Note the `[^-]` in the grep — it excludes hyphenated subcrate names. If a real reference to the deleted crate remains, update or delete it.

### 5f Add a CI guardrail

Create `xtask/src/audit_legacy_memory.rs` (or a similar location consistent with your existing `xtask`/scripts pattern):

```rust
//! Fails if the legacy `moa-memory` crate has reappeared anywhere in the workspace.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut violations = Vec::new();

    // 1. Directory must not exist.
    if Path::new("crates/moa-memory").exists() {
        violations.push("crates/moa-memory/ exists; legacy crate has reappeared.".to_string());
    }

    // 2. Workspace Cargo.toml must not reference it.
    let toml = std::fs::read_to_string("Cargo.toml").expect("read workspace Cargo.toml");
    if toml.contains("\"crates/moa-memory\"") {
        violations.push("workspace Cargo.toml lists crates/moa-memory.".to_string());
    }

    // 3. No crate Cargo.toml may depend on `moa-memory` (note: `moa-memory-*` subcrates are fine).
    for entry in walkdir::WalkDir::new("crates").max_depth(2) {
        let entry = entry.expect("walk crates");
        if entry.file_name() != "Cargo.toml" { continue; }
        let body = std::fs::read_to_string(entry.path()).expect("read crate Cargo.toml");
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("moa-memory ") || trimmed.starts_with("moa-memory=") {
                violations.push(format!("{} declares a moa-memory dep.", entry.path().display()));
            }
        }
    }

    // 4. No source file may import the legacy module.
    for entry in walkdir::WalkDir::new("crates") {
        let entry = entry.expect("walk crates");
        if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") { continue; }
        let body = std::fs::read_to_string(entry.path()).expect("read .rs");
        if body.contains("use moa_memory::") || body.contains("use moa_memory;") {
            violations.push(format!("{} imports moa_memory.", entry.path().display()));
        }
    }

    if violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        for v in &violations {
            eprintln!("audit_legacy_memory: {v}");
        }
        ExitCode::FAILURE
    }
}
```

(If `xtask` doesn't exist yet, a shell script in `scripts/audit_legacy_memory.sh` works equally well — same checks via `test`, `grep`, `find`.)

Wire the guardrail into CI:

```yaml
# .github/workflows/ci.yml (add a step)
- name: Audit legacy memory removal
  run: cargo run -p xtask --bin audit_legacy_memory
```

### 5g Update `architecture.md`

Open `architecture.md` and:

- Remove any "legacy memory" / "wiki" sections.
- Update the crate-list section to drop `moa-memory`.
- Add a short paragraph in the Memory section: "The graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`) is the only memory subsystem. The legacy file-wiki crate `moa-memory` was removed in C06; see `docs/migrations/moa-memory-inventory.md` for the per-consumer migration record."

### 5h Final verification

```sh
cargo build --workspace
cargo test --workspace
cargo run -p xtask --bin audit_legacy_memory   # exit 0
```

All three pass.

## 6 Deliverables

- `crates/moa-memory/` deleted (`git rm -r`).
- Workspace `Cargo.toml` updated.
- `Cargo.lock` regenerated.
- CI guardrail xtask added and wired into the workflow.
- `architecture.md` updated.

## 7 Acceptance criteria

1. `test ! -d crates/moa-memory`.
2. `rg "moa-memory[^-]" Cargo.toml` returns 0 hits.
3. `cargo build --workspace` clean.
4. `cargo test --workspace` green.
5. The CI guardrail xtask exits 0.
6. The CI guardrail xtask is invoked from `.github/workflows/ci.yml`.
7. `git log --oneline -5` shows the C06 commit titled appropriately.

## 8 Tests

```sh
cargo build --workspace
cargo test --workspace
cargo run -p xtask --bin audit_legacy_memory

# Negative test: temporarily reintroduce a violation, confirm guardrail catches it.
mkdir crates/moa-memory && touch crates/moa-memory/Cargo.toml
cargo run -p xtask --bin audit_legacy_memory   # expect exit 1
rm -rf crates/moa-memory
cargo run -p xtask --bin audit_legacy_memory   # expect exit 0
```

## 9 Cleanup

- Confirm no orphaned `target/debug/.fingerprint/moa-memory-*` artifacts cause a stale build (run `cargo clean` if anything looks weird).
- Delete the inventory doc's "open questions" section if all questions resolved.

## 10 What's next

**R01 — `moa-memory/` folder grouping.** With the legacy crate gone, the four graph-stack crates can collapse under a single `crates/moa-memory/` parent for cleaner organization. Then **R02** (type-location audit) and **M21+** continue.

# Step M28 — Final cleanup + CI guardrail

_Verify R01 has fully eliminated the legacy `moa-memory` shim and the old hyphenated subcrate paths, delete any obsolete documentation pages from the wiki era, and install a permanent CI check that prevents regressions._

## 1 What this step is about

Original M28 was a heavy "delete the `moa-memory` crate" prompt. R01 absorbed most of that work when it moved the four subcrates into `crates/moa-memory/`. M28 is now the residual cleanup: confirm nothing leaked, delete a few orphan docs, install a CI guardrail.

## 2 Files to read

- `Cargo.toml` (workspace root)
- `docs/` directory tree
- `examples/` directory tree
- `README.md`
- Output of `rg "moa-memory-graph|moa-memory-vector|moa-memory-pii|moa-memory-ingest" Cargo.toml docs/ README.md` — should match the new paths only

## 3 Goal

1. Verify R01 left the workspace in the expected shape.
2. Delete obsolete docs from the wiki era.
3. Install `cargo xtask audit-paths` as a permanent CI guardrail.
4. `cargo build --workspace` clean; `cargo +nightly udeps --workspace` clean.

## 4 Rules

- **No code changes.** This is documentation, CI, and audit only.
- **The CI guardrail must be cheap.** Runs in <5s. Just rg + path checks.

## 5 Tasks

### 5a Verify R01 outcome

Run these checks; any non-empty result indicates R01 was incomplete:

```sh
# Old hyphenated subcrate dirs must be gone
test ! -d crates/moa-memory-graph
test ! -d crates/moa-memory-vector
test ! -d crates/moa-memory-pii
test ! -d crates/moa-memory-ingest

# New folder-grouped paths must exist
test -d crates/moa-memory/graph
test -d crates/moa-memory/vector
test -d crates/moa-memory/pii
test -d crates/moa-memory/ingest

# Legacy moa-memory crate must not exist
test ! -f crates/moa-memory/Cargo.toml
test ! -f crates/moa-memory/src/lib.rs

# Workspace.members must reference the new paths
grep -q '"crates/moa-memory/graph"' Cargo.toml
grep -q '"crates/moa-memory/vector"' Cargo.toml
grep -q '"crates/moa-memory/pii"' Cargo.toml
grep -q '"crates/moa-memory/ingest"' Cargo.toml

# No source file imports the old shim
! rg -q "use moa_memory::" crates/ --type rust
```

Any failure here indicates R01 needs to be re-run before M28 proceeds.

### 5b Delete obsolete docs

The original repo (pre-graph migration) likely has docs describing file-wiki memory. Identify and delete them:

```sh
rg -l "MEMORY\.md|FileMemoryStore|wiki_branch|reconcile_pages|FileWiki" docs/
```

For each match:
- If the doc is entirely about the file-wiki (e.g., `docs/04-memory-architecture.md` per the original 04 spec), delete it.
- If the doc has a section about file-wiki within a broader topic, redact the section and add a redirect note pointing at `docs/architecture/decisions/0001-envelope-encryption-deferred.md` and the new memory crate README.

Also check examples:

```sh
rg -l "MEMORY\.md|FileMemoryStore" examples/
```

Delete obsolete examples.

### 5c Update README.md

If the top-level README has a "Memory" section describing the file-wiki, replace with a short paragraph:

```markdown
## Memory

Memory is split across four crates under `crates/moa-memory/`:

- `graph/` — Apache AGE adapter, bi-temporal write protocol
- `vector/` — pgvector / Turbopuffer, Cohere Embed v4
- `pii/` — redaction at ingestion via openai/privacy-filter HTTP service
- `ingest/` — slow-path Restate VO, fast-path API, contradiction detector

See `docs/architecture/type-placement.md` for how types are owned across these
crates and `crates/moa-memory/README.md` for crate-level details.
```

### 5d Install CI guardrail

Create or update `xtask/src/main.rs` with an `audit-paths` subcommand:

```rust
fn cmd_audit_paths() -> anyhow::Result<()> {
    use std::process::Command;

    // 1. Old hyphenated dirs must not exist.
    for old in ["crates/moa-memory-graph", "crates/moa-memory-vector",
                "crates/moa-memory-pii", "crates/moa-memory-ingest"] {
        if std::path::Path::new(old).exists() {
            anyhow::bail!("forbidden directory exists: {}", old);
        }
    }

    // 2. Legacy moa-memory crate must not exist.
    for legacy in ["crates/moa-memory/Cargo.toml", "crates/moa-memory/src/lib.rs"] {
        if std::path::Path::new(legacy).exists() {
            anyhow::bail!("legacy moa-memory crate file exists: {}", legacy);
        }
    }

    // 3. No source uses the old shim path.
    let out = Command::new("rg")
        .args(["-l", "use moa_memory::|moa_memory::vector|moa_memory::embedder|moa_memory::chunking",
               "--", "crates/", "--type", "rust"])
        .output()?;
    if !out.stdout.is_empty() {
        eprintln!("Surviving moa-memory shim references:\n{}", String::from_utf8_lossy(&out.stdout));
        anyhow::bail!("legacy shim imports detected");
    }

    // 4. No connector code (deferred per ADR).
    let out = Command::new("rg")
        .args(["-l", "MockConnector|ConnectorClient|connector_inbox", "--", "crates/", "--type", "rust"])
        .output()?;
    if !out.stdout.is_empty() {
        eprintln!("Forbidden connector references:\n{}", String::from_utf8_lossy(&out.stdout));
        anyhow::bail!("connector code detected (deferred per ADR)");
    }

    // 5. No crypto-shred references in code (deferred per ADR 0001).
    let out = Command::new("rg")
        .args(["-l", "crypto_shred|wrapped_dek|EnvelopeCipher", "--", "crates/", "migrations/", "--type-add",
               "sql:*.sql", "--type", "rust", "--type", "sql"])
        .output()?;
    if !out.stdout.is_empty() {
        eprintln!("Forbidden envelope-encryption references:\n{}", String::from_utf8_lossy(&out.stdout));
        anyhow::bail!("envelope-encryption code detected (deferred per ADR 0001)");
    }

    println!("✅ path audit clean");
    Ok(())
}
```

Wire into `xtask` dispatch and add to CI:

```yaml
# .github/workflows/ci.yml
- name: Path audit
  run: cargo xtask audit-paths
```

### 5e Verify

```sh
cargo build --workspace
cargo test --workspace
cargo +nightly udeps --workspace
cargo xtask audit-paths
```

All four must pass.

## 6 Deliverables

- Obsolete docs deleted from `docs/` and `examples/`.
- README.md memory section updated.
- `xtask audit-paths` subcommand implemented.
- CI workflow updated to run `cargo xtask audit-paths`.

## 7 Acceptance criteria

1. All checks in 5a return success.
2. `rg "MEMORY\.md|FileMemoryStore|wiki_branch|reconcile_pages|FileWiki" docs/ examples/` returns 0 hits.
3. README.md memory section reflects the new structure.
4. `cargo xtask audit-paths` exits 0.
5. CI workflow includes the audit step.
6. `cargo +nightly udeps --workspace` clean.

## 8 Tests

The `xtask audit-paths` subcommand IS the test; CI runs it on every PR. Sanity-check it locally:

```sh
cargo xtask audit-paths
# Optional adversarial test: temporarily create a file matching a forbidden pattern
# and confirm the check fails:
mkdir -p crates/moa-memory-graph
cargo xtask audit-paths   # must exit non-zero
rmdir crates/moa-memory-graph
cargo xtask audit-paths   # back to clean
```

## 9 Cleanup

This step IS the cleanup phase. After it lands, the migration is structurally complete:

- No legacy `moa-memory` crate.
- No old hyphenated subcrate paths.
- No file-wiki docs.
- No connector code.
- No envelope-encryption code.
- CI guardrail in place to prevent regression.

## 10 What's next

**M29 — Validation: 100-fact ingestion + 10-supersession + retrieval golden test.** Then **M30 — Performance gate**. After M30 the migration is done.

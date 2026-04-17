---
name: moa-rust
description: >
  Use this skill for Rust work in MOA. It keeps changes aligned with the repo's
  architecture, async, error handling, documentation, feature flag, and verification rules.
compatibility: Rust 2024 workspace with tokio, tracing, thiserror, cargo, and MOA's docs-driven architecture
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
  - Bash(rg:*)
  - Bash(cargo:*)
  - Bash(git:*)
metadata:
  author: Apollo GraphQL & hwuiwon
  version: "1.0.0"
---

# MOA Rust

Use this skill for any Rust implementation or review in this repo. Apply Apollo's handbook as a decision framework, but let MOA's repo rules win when there is a conflict.

## Load Order

1. Read [references/moa-rust-rules.md](references/moa-rust-rules.md) first.
2. Read the relevant design doc under `docs/` before editing. `docs/01-architecture-overview.md` is the interface source of truth, and `docs/09-skills-and-learning.md` matters for skill work.
3. If `graphify-out/GRAPH_REPORT.md` exists, consult it before broad repo exploration or raw-file search.
4. For deeper Rust guidance, load only the relevant local chapter note from [references/apollo/README.md](references/apollo/README.md).

## Default Stance In This Repo

- Preserve documented traits and crate boundaries. Do not invent new interfaces when `docs/01-architecture-overview.md` already defines one.
- Prefer borrowing over cloning. Use owned inputs only when ownership transfer is part of the API.
- Use `Result`-based APIs for fallible work. In library crates, model errors with `thiserror`; use `anyhow` only in binary entrypoints such as `moa-cli` or `moa-desktop`.
- Keep all I/O async on `tokio`. Avoid introducing blocking filesystem or network work in async paths.
- Use `tracing` for observability. Never add `println!` or `eprintln!` to library code.
- Every public function needs a doc comment. Every module needs a module-level doc comment.
- Avoid `unwrap()` in library code. In tests, `expect()` with a specific failure message is acceptable.
- Optional integrations must stay behind the workspace feature flags: `telegram`, `slack`, `discord`, `cloud`, `temporal`.
- Prefer focused tests close to the changed behavior. Use inline unit tests for local logic and `tests/` directories for integration coverage.

## Rust Review Checklist

When reviewing or writing code, check these points in order:

1. Interface fit: does the change match the documented trait, type, and ownership model?
2. Error shape: does the crate expose precise errors with `thiserror`, and does control flow use `?`, `let-else`, or `if let` instead of panic-oriented shortcuts?
3. Ownership: are there redundant clones, needless borrows, or early allocations?
4. Async correctness: is all I/O async, and are spawned tasks or error types compatible with `Send + Sync` where required?
5. Docs and comments: do module and public API docs exist, and do inline comments explain why instead of narrating the code?
6. Feature boundaries: are optional integrations isolated behind feature gates and not pulled into default builds?
7. Verification: were `cargo fmt --all` and `cargo clippy --all-targets --all-features --locked -- -D warnings` run, plus `cargo build -p moa-desktop` when desktop code changed?

## MOA-Specific Guidance

### Architecture First

Before editing a subsystem, read the matching design doc:

- `docs/02-brain-orchestration.md` for orchestrators, Temporal, or the brain loop
- `docs/03-communication-layer.md` for gateway, approvals, observation, or CLI/desktop communication
- `docs/04-memory-architecture.md` and `docs/05-session-event-log.md` for memory or session persistence
- `docs/06-hands-and-mcp.md` for hands, MCP, and tool routing
- `docs/07-context-pipeline.md` for context processors, skills injection, and cache optimization
- `docs/08-security.md` for sandboxing, credentials, or prompt-injection defenses
- `docs/09-skills-and-learning.md` for skills, distillation, and improvement flows

### Type And API Conventions

- Use `uuid::Uuid` wrapped in MOA newtypes for IDs.
- Use `chrono::DateTime<Utc>` for timestamps.
- Use `PathBuf` for filesystem paths and `String` for logical wiki paths.
- Use `serde_json::Value` for dynamic JSON payloads.
- Keep public APIs intentionally documented and boring. Clarity beats cleverness.

### Performance Posture

- Avoid clones in hot paths and loops.
- Prefer iterators and direct transformations over intermediate `collect()` calls when allocation is not needed.
- Box large enum variants when size imbalance matters.
- Use static dispatch by default. Reach for `dyn Trait` only when runtime heterogeneity or boundary abstraction is the real requirement.

## Verification

- Rust-only changes: run `cargo fmt --all` and `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- Desktop/GPUI changes: also run `cargo build -p moa-desktop`.
- If you cannot run a required check, say so explicitly and explain why.

## Sources

- Apollo handbook notes are bundled locally under `references/apollo/`.
- MOA repo instructions come from `AGENTS.md` and the relevant files under `docs/`.

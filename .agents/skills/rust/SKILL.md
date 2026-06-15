---
name: rust
description: >
  Use this skill for any Rust implementation, review, or refactor in the MOA workspace.
  Triggers include: writing or modifying code in `crates/moa-*`, fixing clippy warnings,
  adding doc comments, choosing between `thiserror` and `anyhow`, deciding async patterns,
  selecting types for IDs/timestamps/JSON payloads, or running `cargo fmt`/`cargo clippy`.
  It applies the repo's conventions for traits, errors, async, observability, feature
  flags, and verification. Do NOT use for `sequence/memory-pack` step implementation
  (use `memory-pack`), repo-wide architecture audits, memory-eval baseline or
  scorecard work, release-time test selection (use `certify`), runtime regression
  diagnosis (use `runtime-forensics`), provider integration (use `provider-integration`),
  or test authoring (use `test-authoring`).
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
  moa-tags: "rust, conventions, code-quality, clippy, async, errors, doc-comments"
  moa-one-liner: "Rust implementation and review workflow for the MOA workspace"
---

# Rust

Use this skill for any Rust implementation or review in the MOA workspace. Apply Apollo's handbook as a decision framework, but let MOA's repo rules win when there is a conflict.

## Boundary

Use this skill for: writing or modifying Rust code in `crates/moa-*`, deciding between borrowing and cloning, choosing error types, picking async patterns, applying clippy fixes, adding or improving doc comments, deciding between static and dynamic dispatch.

Do not use this skill for:

- `sequence/memory-pack` step implementation; use `memory-pack`
- selecting which tests to run before merge or release; use `certify`
- diagnosing a runtime regression or adapter drift; use `runtime-forensics`
- adding a new LLM, embedding, hand, MCP, or platform provider; use `provider-integration`
- authoring or extending tests; use `test-authoring`
- repo-wide architecture or lean-down audits; use a dedicated architecture-audit workflow when available
- memory-retrieval baselines, ranking scorecards, query-rewrite gating validation, or live memory-eval lanes; use `certify` for validation until a dedicated memory-eval workflow exists

## Load Order

1. Read [references/repo-rules.md](references/repo-rules.md) first. It contains the non-negotiable repo conventions.
2. Read the relevant design doc under `docs/` before editing. `docs/01-architecture-overview.md` is the interface source of truth.
3. For deeper Rust guidance, load only the relevant chapter from [references/apollo/README.md](references/apollo/README.md).

## Default Stance

- Preserve documented traits and crate boundaries. Do not invent new interfaces when `docs/01-architecture-overview.md` already defines one.
- Import directly from the owning crate or module. Do not add compatibility shim modules, wrapper functions, or `pub use` re-exports just to preserve old paths; update call sites to the source of truth.
- Prefer borrowing over cloning. Use owned inputs only when ownership transfer is part of the API.
- Use `Result`-based APIs for fallible work. In library crates, model errors with `thiserror`. Use `anyhow` only in binary entrypoints such as `moa-orchestrator-bin`, `moa-edge`, `xtask`, or `moa-desktop`.
- Keep all I/O async on `tokio`. Avoid blocking filesystem or network work in async paths.
- Use `tracing` for observability. Never add `println!` or `eprintln!` to library code.
- Every public function needs a doc comment. Every module needs a module-level doc comment.
- Avoid `unwrap()` in library code. In tests, `expect()` with a specific failure message is acceptable.
- Optional integrations stay behind workspace feature flags: `telegram`, `slack`, `discord`, `cloud`.
- Prefer focused tests close to the changed behavior. Use inline unit tests for local logic and `tests/` directories for integration coverage.

## Review Checklist

When reviewing or writing code, check these in order:

1. Interface fit: does the change match the documented trait, type, and ownership model?
2. Error shape: does the crate expose precise errors with `thiserror`, and does control flow use `?`, `let-else`, or `if let` instead of panic-oriented shortcuts?
3. Ownership: are there redundant clones, needless borrows, or early allocations?
4. Async correctness: is all I/O async, and are spawned tasks or error types `Send + Sync` where required?
5. Docs and comments: do module and public API docs exist, and do inline comments explain why instead of narrating the code?
6. Feature boundaries: are optional integrations isolated behind feature gates and not pulled into default builds?
7. Verification: were `cargo fmt --all` and `cargo clippy --all-targets --all-features --locked -- -D warnings` run?

## Cross-Crate API Preflight

Before changing public structs, trait methods, constructors, config types, or wire payloads:

1. Build a call-site map with `rg` for the type, constructor, trait method, and obvious builders.
2. Check tests and fixtures as well as production crates; MOA often has eval, orchestrator, and memory callers for the same type.
3. Update all direct call sites rather than adding compatibility wrappers.
4. For hot surfaces such as `GraphStore`, retrieval requests, config structs, and provider traits, run a compile-guided pass with focused package checks before broader workspace gates.

## Architecture First

Before editing a subsystem, read the matching design doc:

- `docs/02-brain-orchestration.md` for Restate orchestration or the brain loop
- `docs/03-communication-layer.md` for gateway, approvals, observation, or hosted API message flow
- `docs/04-memory-architecture.md` and `docs/05-session-event-log.md` for memory or session persistence
- `docs/06-hands-and-mcp.md` for hands, MCP, and tool routing
- `docs/07-context-pipeline.md` for context processors, skills injection, and cache optimization
- `docs/08-security.md` for sandboxing, credentials, or prompt-injection defenses
- `docs/09-skills-and-learning.md` for skills, distillation, and improvement flows
- `docs/12-restate-architecture.md` for Restate-specific virtual-object structure

## Type and API Conventions

- Use `uuid::Uuid` wrapped in MOA newtypes (`SessionId`, `UserId`, `WorkspaceId`).
- Use `chrono::DateTime<Utc>` for timestamps.
- Use `PathBuf` for filesystem paths and `String` for logical wiki paths.
- Use `serde_json::Value` for dynamic JSON payloads.
- Keep public APIs intentionally documented and boring. Clarity beats cleverness.

## Performance Posture

- Avoid clones in hot paths and loops.
- Prefer iterators and direct transformations over intermediate `collect()` calls when allocation is not needed.
- Box large enum variants when size imbalance matters.
- Use static dispatch by default. Reach for `dyn Trait` only when runtime heterogeneity is the real requirement.

## Verification

- Rust-only changes: run `cargo fmt --all` and `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- Desktop/GPUI changes: also run `cargo build -p moa-desktop`.
- If you cannot run a required check, say so explicitly and explain why.

## Output Format

When reporting on a Rust change, include:

- `Files changed`: list
- `Behavior landed`: one or two sentences
- `Verification`: which `cargo` commands ran and their outcome
- `Open questions`: anything the change deferred or left ambiguous

# MOA Rust Rules

This reference condenses the repository-specific instructions that matter when applying generic Rust advice inside MOA.

## Non-Negotiable Rules

- `docs/01-architecture-overview.md` is the interface source of truth. Start there before changing public behavior or crate boundaries.
- Every public function must have a doc comment.
- Every module must have a module-level doc comment.
- Use `thiserror` for library error types. Use `anyhow` only in binary entrypoints such as `moa-cli` and `moa-desktop`.
- Use `tracing` for logging. Do not add `println!` or `eprintln!` to library code.
- Use `tokio` for async work. Keep I/O async.
- Do not use `unwrap()` in library code.
- Optional integrations stay behind feature flags: `telegram`, `slack`, `discord`, `cloud`, `temporal`.
- Close-out for Rust work is `cargo fmt --all` and `cargo clippy ... -D warnings`.
- If `graphify-out/GRAPH_REPORT.md` exists, consult it before broad repo exploration.

## Workspace Facts

- The workspace uses `edition = "2024"`.
- Default members exclude `moa-desktop`, so desktop changes need an explicit `cargo build -p moa-desktop`.
- The current workspace dependencies already standardize `tokio`, `serde`, `chrono`, `uuid`, `thiserror`, `anyhow`, and `tracing`.

## Project Conventions

- IDs are `uuid::Uuid` wrapped in MOA newtypes such as `SessionId`, `UserId`, and `WorkspaceId`.
- Timestamps are `chrono::DateTime<Utc>`.
- Filesystem paths use `PathBuf`; logical wiki paths use `String`.
- Dynamic JSON payloads use `serde_json::Value`.
- Tests belong in crate `tests/` directories for integration coverage or inline `#[cfg(test)]` modules for unit coverage.

## Design-Doc Map

- `docs/02-brain-orchestration.md`: orchestrators, Temporal, local brain lifecycle
- `docs/03-communication-layer.md`: gateway, approvals, observation, desktop and CLI message flow
- `docs/04-memory-architecture.md`: memory wiki, indexing, consolidation
- `docs/05-session-event-log.md`: Postgres event schema, event types, compaction
- `docs/06-hands-and-mcp.md`: hands, MCP routing, sandbox providers
- `docs/07-context-pipeline.md`: seven-stage context compilation pipeline
- `docs/08-security.md`: credential vault, sandbox, prompt injection
- `docs/09-skills-and-learning.md`: skill format, distillation, self-improvement
- `docs/10-technology-stack.md`: crate selection, external services, implementation phases

## Practical Implications

- Prefer surgical changes inside the existing crate structure over broad cross-crate refactors.
- When a doc and implementation diverge, either update both deliberately or stop and resolve the contract mismatch before continuing.
- Keep feature-gated integrations isolated so default builds remain lean.
- Preserve the repo's docs-first style: comment the why, not the obvious what.

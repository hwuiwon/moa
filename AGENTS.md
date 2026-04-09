# MOA — Agent Instructions

You are implementing MOA, a cloud-first general-purpose AI agent platform written in Rust.

## Spec location

The full architecture specification is in `docs/`. Read the relevant section before implementing any step.

| File | Covers |
|---|---|
| `docs/00-direction.md` | Product identity and philosophy |
| `docs/01-architecture-overview.md` | System diagram, all trait definitions, workspace layout |
| `docs/02-brain-orchestration.md` | Temporal, Fly.io, LocalOrchestrator, brain loop |
| `docs/03-communication-layer.md` | Gateway, TUI layout, CLI, approvals, observation |
| `docs/04-memory-architecture.md` | File-wiki, FTS5, scoping, consolidation |
| `docs/05-session-event-log.md` | Turso/libSQL schema, event types, compaction |
| `docs/06-hands-and-mcp.md` | HandProvider, Daytona, E2B, MCP, tool routing |
| `docs/07-context-pipeline.md` | 7-stage compilation, cache optimization |
| `docs/08-security.md` | Credential vault, sandbox, prompt injection |
| `docs/09-skills-and-learning.md` | Agent Skills format, distillation |
| `docs/10-technology-stack.md` | Crates, phases, deployment |

## Rules

1. **Use the trait definitions from `docs/01-architecture-overview.md` as the source of truth.** All component interfaces are defined there.
2. **Every public function must have a doc comment.**
3. **Every module must have a module-level doc comment.**
4. **Use `thiserror` for library error types, `anyhow` only in binary crates (`moa-cli`, `moa-tui`).**
5. **Use `tracing` for all logging.** Never `println!` or `eprintln!` in library code.
6. **Use `tokio` as the async runtime.** All I/O must be async.
7. **All tests go in a `tests/` directory within each crate** (integration tests) or inline `#[cfg(test)] mod tests` (unit tests).
8. **Run `cargo clippy` and `cargo fmt` before considering any step complete.**
9. **No `unwrap()` in library code.** Use `?` or explicit error handling.
10. **Feature flags** control optional dependencies: `telegram`, `slack`, `discord`, `cloud`, `temporal`.
11. **If `graphify-out/GRAPH_REPORT.md` exists, consult it before broad repo exploration or raw-file search.**

## Conventions

- IDs: `uuid::Uuid` wrapped in newtypes (`SessionId`, `UserId`, `WorkspaceId`)
- Timestamps: `chrono::DateTime<Utc>`, serialized as ISO 8601
- Config: TOML files via the `config` crate
- JSON: `serde_json::Value` for dynamic payloads
- Paths: `std::path::PathBuf` for filesystem, `String` for logical paths (memory wiki paths)
- Errors: One `Error` enum per crate with `#[derive(thiserror::Error)]`

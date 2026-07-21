# moa-core

Shared MOA types, traits, and error definitions — the base crate every runtime
crate depends on. The crate root deliberately exports only `MoaError`,
`Result`, and `WORKSPACE_ID`; all other APIs are addressed through their
owning modules.

## Modules

- `analytics` — typed analytics read-model DTOs shared by session storage and API surfaces
- `coordination_counters` — per-turn instrumentation for durable virtual-object round-trips
- `diff` — unified diff helpers shared across tool implementations
- `error` — shared error types and failure classification (`MoaError`, `Result`)
- `events` — session event definitions and helpers
- `session_engine` — shared session-lifecycle rules used by orchestrator adapters
- `session_replay` — per-turn session event replay instrumentation utilities
- `shell` — shell parsing helpers used by policy normalization and matching
- `traits` — stable trait interfaces shared across MOA crates
- `transcript` — shared JSONL transcript fixtures for recorded provider-response tests
- `truncation` — text truncation utilities for tool output handling
- `types` — cross-crate DTOs, identifiers, and supporting enums
- `workspace` — workspace-level constants (`WORKSPACE_ID`)

## Rules

- A type belongs in core only if it is shared currency across crates: an ID
  newtype, a type used in trait signatures, or an `Event` payload. See the
  Type Placement policy in `docs/15-architecture-policy.md`.
- No wildcard, prelude, or compatibility re-exports; the crate-root allowlist
  is exactly `MoaError`, `Result`, and `WORKSPACE_ID`.
- Runtime configuration lives in `moa-config` and service wire DTOs in
  `moa-wire`, not here.

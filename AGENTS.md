# MOA — Agent Instructions

You are implementing MOA, a cloud-first general-purpose AI agent platform written in Rust.

## Spec location

The full architecture specification is in `docs/`. Read the relevant section before implementing any step.

| File | Covers |
|---|---|
| `docs/00-direction.md` | Product identity and philosophy |
| `docs/01-architecture-overview.md` | System diagram, all trait definitions, workspace layout |
| `docs/02-brain-orchestration.md` | Restate orchestration, hosted runtime mode, brain loop |
| `docs/03-communication-layer.md` | Gateway/API communication, approvals, observation |
| `docs/04-memory-architecture.md` | Graph memory, privacy filtering, sidecar indexes, retrieval, consolidation |
| `docs/05-session-event-log.md` | Postgres event schema, event types, compaction |
| `docs/06-hands-and-mcp.md` | HandProvider, Daytona, E2B, MCP, tool routing |
| `docs/07-context-pipeline.md` | 7-stage compilation, cache optimization |
| `docs/08-security.md` | Credential vault, sandbox, prompt injection |
| `docs/09-skills-and-learning.md` | Agent Skills format, distillation |
| `docs/10-technology-stack.md` | Crates, phases, deployment |

## Rules

1. **Use the trait definitions from `docs/01-architecture-overview.md` as the source of truth.** All component interfaces are defined there.
2. **Every public function must have a doc comment.**
3. **Every module must have a module-level doc comment.**
4. **Use `thiserror` for library error types.** Use `anyhow` only in binary entrypoints.
5. **Use `tracing` for all logging.** Never `println!` or `eprintln!` in library code.
6. **Use `tokio` as the async runtime.** All I/O must be async.
7. **All tests go in a `tests/` directory within each crate** (integration tests) or inline `#[cfg(test)] mod tests` (unit tests).
8. **Run `cargo clippy` and `cargo fmt` before considering any step complete.**
9. **No `unwrap()` in library code.** Use `?` or explicit error handling.
10. **Feature flags** control optional dependencies: `telegram`, `slack`, `discord`, `cloud`.
11. **If `graphify-out/GRAPH_REPORT.md` exists, consult it before broad repo exploration or raw-file search.**
12. **MOA crates live under `crates/`.** If a prompt references `<name>/...` for a workspace crate, translate it to `crates/<name>/...`.

## Conventions

- IDs: `uuid::Uuid` wrapped in newtypes (`SessionId`, `UserId`, `WorkspaceId`)
- Timestamps: `chrono::DateTime<Utc>`, serialized as ISO 8601
- Config: TOML files via the `config` crate
- JSON: `serde_json::Value` for dynamic payloads
- Paths: `std::path::PathBuf` for filesystem, `String` for logical identifiers, and typed IDs for graph memory nodes where available
- Errors: One `Error` enum per crate with `#[derive(thiserror::Error)]`

## Verification Checklist

- Rust-only changes:
  - `cargo fmt --all`
  - run focused crate/test targets for the changed surface
  - `cargo clippy ... -D warnings`
  - `cargo build --workspace` when public types, shared crates, or workspace wiring changed
  - `git diff --check`

## Local Docker Compose Stack

- Bring the MOA compose stack up only when a task needs local Postgres, Restate, OpenFGA, edge, PII, audit shipper, or loadtest services.
- When the stack is no longer needed, stop it with `docker compose down` before ending work. This preserves volumes and avoids leaving background services running.
- Use `docker compose down -v` or `make dev-wipe` only when an explicit reset is intended; those commands remove local state.
- Check current stack state with `docker compose ps` before assuming services are running or stopped.

## Authorization review rule

Every handler that touches data on behalf of a caller MUST either:

1. Call `moa_authz::require_authz` or `require_authz_with_delegation` with
   the appropriate `(ObjectType, object_id, Relation)`. The check happens
   before any reads of the protected resource. OR
2. Carry a one-line `// SAFETY: ...` justification immediately above the
   handler signature explaining why no check is needed. Acceptable
   justifications: "purely informational; no resource-specific data",
   "health/observability endpoint", "called only from another handler that
   has already checked".

PRs that violate this rule are returned for revision. The reviewer is
explicitly responsible for verifying the justification is correct; this is
not a CI-enforceable rule because it depends on judgment about the data the
handler returns.

Common mistakes:

- Reading the resource first to determine the workspace_id, then checking
  authz. Wrong order: an unauthorized caller has already exfiltrated the
  resource's existence and any read-able fields. Always check first.
- Using `require_authz` for a write that should use
  `require_authz_with_delegation`. If the handler accepts requests from
  agents, use the delegation variant.
- Forgetting to enqueue the inverse outbox tuple on resource deletion. If
  create-handlers enqueue write tuples, delete-handlers must enqueue delete
  tuples in the same transaction. Stale tuples in OpenFGA grant phantom
  access after a resource is deleted.

## Live And Billed Tests

- Live provider tests must never run by default.
- Live/billed tests require both `#[ignore = "..."]` and an explicit opt-in env flag such as `MOA_RUN_LIVE_COHERE_TESTS=1`.
- If the opt-in flag is set but the required credential is missing, the test should fail with a clear message.
- Secrets must not be written to files, git-tracked fixtures, or shell command text. Inject temporary keys via stdin, a local shell prompt, or an existing secret store.

## Testing standards

Tests in this repo are graded against the four criteria below. Any test that fails its criterion should be deleted, not weakened. We do not keep tests "for coverage" — coverage that doesn't catch regressions is a liability.

### A test must exercise a real code path

A test that asserts a mock returned what the mock was configured to return proves the mocking framework works, not the code under test. A test that calls `serde_json::from_str` on a fixture and asserts it parsed proves serde works. A test that constructs a struct with `Default::default()` and reads its fields proves the field literals match.

If you cannot name the production code path the test exercises in one sentence, delete the test.

### A test's assertions must be strong enough to catch a real regression

`assert!(result.is_ok())` is almost never enough. `assert!(events.iter().any(|e| matches!(e, Event::ToolCall { .. })))` is almost never enough. The assertion must pin a specific behavior:

- Counts (exact, not `>= 1`).
- Identities (specific session id, sequence num, content).
- Ordering (exact element-by-element equality where order matters).
- Field values (specific token counts, specific cost cents, specific status transitions).

Before merging, ask: *"if I broke the implementation in a plausible way, would this test fail?"* If the answer is "maybe," the test is too weak.

### A test must not be coupled to implementation details that are free to change

Tests that assert on internal struct field order, private function call counts, or specific `HashMap` iteration order will break on legitimate refactors. Behavior — what the public API does — is what tests pin. Internal layout is the implementation's problem.

If a test fails on a refactor that did not change observable behavior, the test was wrong.

### A test must not duplicate a stronger test elsewhere

The long-conversation eval suite (in `crates/moa-eval/scenarios/long_conversation/`) is the authoritative check for end-to-end behavior. The 12 scenarios cover code-task flows, research, deploys, crash recovery, concurrent writes, learning loop, prompt injection, shell bypass, approval modes, multi-observer parity, compaction, and canary leak prevention.

A unit test that asserts an invariant already checked by an eval scenario is **redundant** — the eval scenario catches the regression downstream and is closer to production behavior. Delete the unit-level duplicate unless it adds something the eval scenario cannot:

- The unit test exercises a code path the eval scenario doesn't reach (rare; document it).
- The unit test runs in milliseconds; the eval scenario takes 30+ seconds (acceptable reason for a fast PR-time sentinel).
- The unit test pins a property the eval scenario cannot easily express (e.g., a specific error variant).

When in doubt, prefer the eval scenario. Unit tests should pin **invariants** and **algorithmic properties**; eval scenarios pin **behaviors**.

### How to add a new test

When authoring a new test, write it in this order:

1. **Name the production scenario or invariant the test pins.** Write it as a one-line comment at the top of the test function. Example: `// Pins: brain harness preserves error events through history compaction.`
2. **Write the assertion first.** Decide what concrete value, count, or sequence the test will pin before writing setup code.
3. **Write the setup that drives the system to produce that assertion-target.** Use real code paths; use mocks only for external boundaries (HTTP, time, RNG).
4. **Verify the test catches what it claims to catch.** Temporarily break the implementation in a plausible way; confirm the test fails with a message that names the regression. Revert.

If step 4 is hard because the test would still pass with a broken implementation, the test is too weak — strengthen the assertion before merging.

### How to remove an existing test

If you find a test that fails any criterion above, delete it in a focused PR with the commit message:

```
test: remove <test_name> in <crate>

Fails AGENTS.md testing criterion <A|B|C|D>: <one-sentence reason>.

The behavior is covered by: <pointer to stronger test, OR "uncovered, intentionally — the assertion was tautological">.
```

Do not weaken or rewrite tests that fail criterion A or D — delete them. Tests that fail B or C may be rewriteable; prefer rewrite over delete for those.

### What "test infrastructure" gets a free pass

Test helpers, fixtures, and contract harnesses in `tests/support/`, `moa-test-support`, and shared modules are not graded against criteria A–D directly — they are graded by whether the tests *that use them* meet the criteria. A helper that exists but is unused should be deleted.

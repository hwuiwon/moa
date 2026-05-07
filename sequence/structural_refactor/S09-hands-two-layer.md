# S09 — Reorganize `moa-hands` into `core/` + `adapters/` and drop the `moa-memory-ingest` runtime dep

## Scope

Two distinct changes in one prompt because they're tightly coupled:
1. **Two-layer split**: Move shared sandbox/router plumbing into `src/core/`. Move per-sandbox implementations (local, daytona, e2b, mcp) into `src/adapters/<sandbox>/`. Break up `router/mod.rs` (~2,120 LOC).
2. **Dependency direction fix**: `moa-hands` currently depends on `moa-memory-ingest` at runtime — that's the data-flow direction inverted. Move shared types to `moa-core` (or extract a tiny `moa-hands-types` if absolutely necessary), and let ingest depend on hands' core types instead.

## Preconditions

- S01–S08 complete and merged.
- `cargo check --workspace` is green.
- The `moa-memory-ingest` dependency direction must be confirmed as "inverted" (i.e. hands does not need to consume ingest at runtime). If S05's audit produced contradicting evidence, escalate before running.

## Why this prompt

`moa-hands` is the second-largest crate (~13,260 LOC) and has the same shape as `moa-providers`: small core (router, registry, telemetry) and many adapters (local exec, Daytona containers, E2B microVMs, MCP servers). The router's `mod.rs` is doing too much despite siblings (`construction.rs`, `normalization.rs`, `policy.rs`, `registration.rs`, `telemetry.rs`) already existing — the giant mod.rs is largely a *move* operation into existing siblings.

The `moa-memory-ingest` dependency is a layer-violation: ingest should consume the *outputs* of tool calls, not the other way around. Fixing it now (rather than later) removes a constraint on every subsequent prompt.

## Files in scope

```
crates/moa-hands/src/
├── lib.rs
├── core/                          — NEW
│   ├── mod.rs
│   ├── registry.rs                — tool registration, was scattered in router/
│   ├── normalization.rs           — was router/normalization.rs (move + expand)
│   ├── policy.rs                  — was router/policy.rs
│   ├── telemetry.rs               — was router/telemetry.rs
│   ├── dispatch.rs                — was a chunk of router/mod.rs
│   └── construction.rs            — was router/construction.rs
├── adapters/                      — NEW
│   ├── mod.rs
│   ├── local/
│   │   ├── mod.rs
│   │   ├── exec.rs
│   │   ├── docker.rs              — if local has a docker sub-mode
│   │   └── allowlist.rs
│   ├── daytona/
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   └── exec.rs
│   ├── e2b/
│   │   ├── mod.rs
│   │   └── client.rs
│   └── mcp/
│       ├── mod.rs
│       ├── client.rs
│       ├── transport.rs
│       └── credential_proxy.rs   — if proxy currently lives in moa-hands
└── tools/                         — keep folder structure as-is
    ├── mod.rs
    ├── bash.rs
    ├── file_read.rs
    ├── file_write.rs
    ├── grep.rs
    ├── str_replace.rs
    └── tool_result.rs
```

## Files explicitly out of scope

- `crates/moa-hands/tests/` — TEST pack
- The `HandProvider` trait (in `moa-core`)
- The `BuiltInTool` trait (in `moa-core`)
- `tools/` folder structure — already correctly organized; do not reorganize

## Step-by-step instructions

### Part A: Two-layer split

1. **Audit `router/mod.rs` (~2,120 LOC)** by section. Likely contents:
   - Tool dispatch (find tool by name, route to adapter)
   - Permission checking
   - Telemetry wrapping
   - Tool-result post-processing (truncation, sanitization)
   - Default tool loadout construction
   - Built-in tool registration

2. **Identify content that belongs in existing siblings.** The siblings (`construction.rs`, `normalization.rs`, `policy.rs`, `registration.rs`, `telemetry.rs`) already exist; the giant mod.rs is dumping that they should own.

3. **Move content from `router/mod.rs` into siblings**:
   - Tool registration code → `registration.rs` (or `core/registry.rs` after step 5)
   - Permission checking → `policy.rs`
   - Tool-result truncation/sanitization → `normalization.rs`
   - Span / metric wrapping → `telemetry.rs`
   - Pure dispatch (find-and-call) → stays in `mod.rs` or moves to a new `dispatch.rs`

4. **`router/mod.rs` after step 3** should be under 400 LOC and contain only:
   - The `ToolRouter` struct
   - `pub fn new`, `pub fn register_tool`, `pub fn execute` (delegating to siblings)
   - The `impl` aggregator (if there's one trait the router implements)

5. **Hoist the now-clean `router/` folder up to `core/`.** Either:
   - **Option A**: Rename `router/` to `core/`, move `mod.rs`/etc. into it. Cleanest but `core/` is more general than just routing.
   - **Option B**: Keep `router/` as a sub-module of `core/`: `core/router/{mod.rs, registry.rs, ...}`.
   
   Recommendation: Option A. The router *is* the core of moa-hands — there's no other "core" to share the namespace with.

6. **Move sandbox implementations to `adapters/`.** For each existing top-level sandbox file (`local.rs`, `daytona.rs`, `e2b.rs`, `mcp.rs` or wherever they live now):
   - Create `adapters/<sandbox>/mod.rs`
   - Move the sandbox struct + `impl HandProvider for ...` block into `mod.rs`
   - If the sandbox file is >700 LOC, split into `client.rs` (HTTP/transport) + `exec.rs` (execution semantics) + `mod.rs` (struct + trait impl)
   - For MCP specifically: split `transport.rs` (stdio/sse/http) from `client.rs` (call_tool, list_tools) from `credential_proxy.rs` if relevant

7. **Update `lib.rs`** to declare `mod core;` and `mod adapters;`, with `pub use` re-exports preserving the previous `moa_hands::*` surface.

### Part B: Drop `moa-memory-ingest` runtime dep

8. **Find the actual usage.** Run:
   ```bash
   rg "moa_memory_ingest" crates/moa-hands/src/
   ```
   Likely findings:
   - A type like `IngestableEvent` or `ToolOutputArtifact` that hands emits and ingest consumes
   - Possibly a function that hands calls to "kick" ingest (hands as producer pushing to a downstream)

9. **Determine the type's natural home.** Three possibilities:
   - **The type belongs in `moa-core`** (it's a domain type both hands and ingest care about). Move it there.
   - **The type belongs in `moa-hands`** (it's about tool outputs, ingest is a consumer). Move it from `moa-memory-ingest` to `moa-hands::core` and update ingest to depend on `moa-hands` instead. **This inverts the dep — ingest depends on hands, not vice versa.**
   - **The type belongs in `moa-memory-ingest`** but hands shouldn't construct it directly. Instead, hands emits a simpler type (`ToolResult`, already in `moa-core`) and ingest's pipeline consumes it. **This deletes the dep entirely.**

   Recommendation: **third option** if feasible. The cleanest dep direction is hands → core, ingest → core, ingest → hands (if needed). Hands should never depend on a memory subsystem.

10. **Remove `moa-memory-ingest` from `crates/moa-hands/Cargo.toml`.**

11. **If the type was moved to `moa-core`**, update `moa-memory-ingest` to import from there.

12. **Run verification.**

13. **Document the dep change** in `REFACTOR_NOTES.md` under `[S09]` — specifically what type moved and why, so future maintainers don't re-introduce the bad dep.

## Verification

```bash
cargo check -p moa-hands --all-targets
cargo clippy -p moa-hands --all-targets -- -D warnings
cargo test -p moa-hands --no-run
cargo check -p moa-memory-ingest --all-targets   # ingest still compiles
cargo check --workspace --all-targets

# Verify no cycles
cargo tree -p moa-hands | grep -i "moa-memory-ingest" && echo "CYCLE STILL PRESENT" || echo "OK: dep removed"
```

The last line is the load-bearing check. If it prints "CYCLE STILL PRESENT", the dep wasn't fully removed.

## Acceptance criteria

- [ ] `crates/moa-hands/src/core/` exists with router-flow plumbing.
- [ ] `crates/moa-hands/src/adapters/{local,daytona,e2b,mcp}/` each has `mod.rs` and per-concern siblings.
- [ ] `router/mod.rs` content has been redistributed; if `core/router/mod.rs` exists, it's <400 LOC.
- [ ] No file in `crates/moa-hands/src/` exceeds 700 LOC.
- [ ] `crates/moa-hands/Cargo.toml` does not list `moa-memory-ingest` as a dep.
- [ ] If a type moved between crates, both old and new locations build.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change *for the structural split* (the dep removal may cause a compile error in `moa-memory-ingest` that you fix in this same prompt).

## Rollback plan

`git checkout -- crates/moa-hands/ crates/moa-memory/ingest/` and `git clean -fd crates/moa-hands/`. The change is contained to two crates.

## Notes for the agent

- **Part A and Part B are independent in their effects.** If Part B turns out to be more complex than expected (e.g. the dep is genuinely needed because hands and ingest share something subtle), do Part A only and document Part B as a follow-up. Better to ship a partial fix than block on a hard call.
- **The `tools/` folder is already correct.** Don't reorganize it. Just leave it. Bash, file_read, file_write, grep, str_replace, tool_result — each in its own file, with a `mod.rs` aggregator. That's the right shape.
- **`HandProvider` trait stays in `moa-core`.** Each adapter's `mod.rs` has `impl HandProvider for LocalProvider { ... }`.
- **MCP credential proxy is special.** It's mentioned in the audit as living in `moa-security`. If `moa-hands` has a copy or wrapper, that's a smell — but likely intentional (security checks happen at the hand boundary). Don't move the proxy in this prompt; just note the boundary.
- **Don't unify the four sandbox adapter shapes** beyond the trait. Each sandbox has its own quirks (Daytona's auto-stop, E2B's microVM lifecycle, MCP's three transports). Forcing them into a uniform shape will leak details.
- **`tools/` items are dispatched by the router**, not by the sandbox. The flow is: brain → router → (tool? bash? file_*? built-in?) → if requires sandbox: route to adapter.
- **Time budget**: 2 sessions for Part A, 0.5 for Part B if it's clean, up to 1.5 if the dep removal is gnarly.
- **Anti-pattern**: do not introduce a `SandboxProvider` enum that exhaustively names all sandboxes. Polymorphism is via `dyn HandProvider`, not a sum type. The factory function in `core/construction.rs` does the matching.

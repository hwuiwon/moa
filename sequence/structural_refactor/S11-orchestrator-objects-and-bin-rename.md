# S11 — Split `moa-orchestrator` oversized files + execute S05 verdict + rename binary

## Scope

Three changes:
1. Split `objects/session.rs` (~1,466 LOC) and `objects/sub_agent.rs` (~1,187 LOC) by handler/state-machine-step.
2. Execute the verdict from `S05-decision.md` regarding `services/session_store.rs`.
3. Rename the binary target from `moa-orchestrator` to `moa-orchestrator-bin` to clarify the bin/lib boundary.

## Preconditions

- S01–S10 complete and merged.
- `cargo check --workspace` is green.
- `struct-pack/S05-decision.md` exists and has a definitive verdict.

## Why this prompt

`moa-orchestrator` is a Restate workflow engine with three giant "object" implementations (sessions, sub-agents, workspaces) and several services. The audit found two object files over 1k LOC and one service file (`session_store.rs`) that may be a duplicate trait. The bin/lib ambiguity (the crate is declared `[[bin]]` but consumed as a library by `moa-orchestrator-local`) is the third hygiene fix.

## Files in scope

- `crates/moa-orchestrator/src/objects/session.rs` → split to `objects/session/`
- `crates/moa-orchestrator/src/objects/sub_agent.rs` → split to `objects/sub_agent/`
- `crates/moa-orchestrator/src/services/session_store.rs` → action depends on S05 verdict
- `crates/moa-orchestrator/Cargo.toml` — possibly add `[[bin]]` with rename
- Possibly downstream `Cargo.toml` files that depend on the `package = "moa-orchestrator"` entry

## Files explicitly out of scope

- `objects/workspace.rs` (if not flagged in audit)
- `services/intent_manager.rs`, `services/llm_gateway.rs`, `services/tool_executor.rs`, `services/workspace_store.rs` (under 1k LOC unless growth happened)
- `workflows/turn.rs`, `workflows/ingestion.rs`, `workflows/intent_discovery.rs` — out of scope unless oversized
- `crates/moa-orchestrator/tests/` — TEST pack

## Step-by-step instructions

### Part A — `objects/session.rs` split

1. Read end-to-end. This is a Restate "virtual object" — likely structured as:
   - State definition (the session's persisted state)
   - Init / construction handler
   - Append-event handler
   - Update-status handler
   - Resume handler
   - Observe handler
   - Cancel handler
   - State-machine transition logic

2. Target structure:
   ```
   objects/session/
   ├── mod.rs           — object registration + state struct
   ├── state.rs         — state struct + serialization
   ├── handlers/
   │   ├── mod.rs
   │   ├── init.rs
   │   ├── append.rs
   │   ├── update_status.rs
   │   ├── resume.rs
   │   ├── observe.rs
   │   └── cancel.rs
   └── transitions.rs   — state-machine transition rules
   ```

3. Move handler bodies into separate files. Each handler typically takes `&mut state` and returns a `Result`. Keep signatures identical.

4. The Restate registration macro / attribute (likely `#[restate::object]` or similar) stays in `mod.rs`. Handler functions are referenced by name; they can live in sibling files as `pub(super) async fn`.

### Part B — `objects/sub_agent.rs` split

5. Same recipe as Part A, applied to sub_agent. Likely handlers:
   - Init (parent session ID, task, tool subset)
   - Run (run-to-completion or until budget exhausted)
   - Result (return summary to parent)
   - Cancel

6. Target structure mirrors `objects/session/`.

### Part C — `services/session_store.rs` per S05 verdict

7. Read `struct-pack/S05-decision.md` carefully. Execute the verdict:
   - **DELETE**: Remove the orchestrator's trait. Migrate every caller to use `moa_core::traits::SessionStore` directly. Delete `services/session_store.rs` if it has no other content.
   - **RENAME**: Rename the trait to `OrchestratorSessionStore` (or `SessionStoreFacade`). Update all internal callers. Add a doc comment explaining why this facade exists.
   - **MERGE**: Move the additional methods to `moa_core::traits::SessionStore`. Then proceed as DELETE for the now-redundant orchestrator trait. (Careful: trait expansion requires updating every implementer in the workspace.)
   - **LEAVE**: Add a doc comment in `services/session_store.rs` explaining the design intent. Note in `REFACTOR_NOTES.md` that this is a known design wart pending a future decision.

### Part D — Binary rename

8. In `crates/moa-orchestrator/Cargo.toml`:
   ```toml
   [[bin]]
   name = "moa-orchestrator-bin"
   path = "src/main.rs"   # or wherever
   ```
   The package name (the `[package] name = "moa-orchestrator"`) **stays**. The library is consumed as `moa-orchestrator`; only the binary target gets renamed.

9. **Update any references to the binary**:
   - `Dockerfile` (if it copies the binary by name)
   - `fly.toml` (if it references the binary)
   - CI configs that build the binary
   - `docker/` and `k8s/` if they reference `moa-orchestrator` binary
   - `package = "moa-..."` aliases in `crates/*/Cargo.toml` that consume the lib don't change (those reference the package, not the binary)

10. **Verify binary builds**:
    ```bash
    cargo build -p moa-orchestrator --bin moa-orchestrator-bin --release
    ```

### All parts

11. Run verification.

12. Document the S05 verdict execution result (which option was taken) in `REFACTOR_NOTES.md` under `[S11]`.

## Verification

```bash
cargo check -p moa-orchestrator --all-targets
cargo clippy -p moa-orchestrator --all-targets -- -D warnings
cargo test -p moa-orchestrator --no-run
cargo check --workspace --all-targets

# Binary builds with the new name
cargo build -p moa-orchestrator --bin moa-orchestrator-bin

# File sizes
find crates/moa-orchestrator/src -name '*.rs' -exec wc -l {} + | awk '$1 > 700 {print "TOO BIG:", $0}'
```

## Acceptance criteria

- [ ] `objects/session.rs` no longer exists; replaced by folder.
- [ ] `objects/sub_agent.rs` no longer exists; replaced by folder.
- [ ] S05 verdict has been executed; `REFACTOR_NOTES.md` documents which option was taken.
- [ ] Binary target is named `moa-orchestrator-bin`; package is still named `moa-orchestrator`.
- [ ] `Dockerfile` / `fly.toml` / k8s manifests updated if they reference the binary name. (If these files are out of scope per pack-level rules, only `Cargo.toml` is updated and the rename is documented in `REFACTOR_NOTES.md`.)
- [ ] No file in `moa-orchestrator/src/` exceeds 700 LOC.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-orchestrator/ Dockerfile fly.toml k8s/` (whatever was touched). The bin rename is the most "outwardly visible" change; if rolling back, also revert any deployment configs that were updated.

## Notes for the agent

- **Restate object handlers have specific signatures.** Don't change them — `Restate` reflects on them at compile time. The handler-per-file split is *moving the function body*, not the signature.
- **State serialization is via Serde.** Don't reorder struct fields (could break compatibility with persisted state). Keep `#[derive]`s exact.
- **The `[[bin]]` rename is mechanical for Cargo** but operationally fragile for deployments. If Dockerfile/fly.toml are out of scope for the pack, **stop and explicitly ask the user before renaming the binary**, because this is an ops-touching change.
- **Alternative for Part D**: if the user prefers no operational change, *defer the binary rename to a follow-up*. Document that the bin/lib ambiguity remains and revisit when ops can coordinate.
- **The S05 verdict execution can be the single biggest part of this prompt.** If verdict is MERGE, that's its own session — split S11 into S11a (objects + bin rename) and S11b (S05 execution).
- **Time budget**: 1.5–2 sessions. If S05 verdict is MERGE, 2.5.
- **Anti-pattern**: don't try to "improve" the Restate workflow logic. State machines have edge-case handling that looks redundant but covers real concurrency races.

# Step 64 — Workspace Cleanup + `enum_dispatch` for Session Backend + Split `router.rs`

_Remove stale TUI dependencies from workspace. Clean up vestigial feature flags. Eliminate SessionDatabase boilerplate. Split the 44KB tool router._

---

## 1. What this step is about

Four related cleanup tasks that don't individually warrant their own prompt but collectively reduce ~300 lines of boilerplate and remove dead dependency edges.

---

## 2. Files to read

- **`Cargo.toml` (root workspace)** — Workspace dependencies and feature flags.
- **`moa-session/src/backend.rs`** — The 9KB `SessionDatabase` enum with mechanical `match` delegation.
- **`moa-session/Cargo.toml`** — Add `enum_dispatch`.
- **`moa-hands/src/router.rs`** — The 44KB tool router file.
- **`moa-hands/src/lib.rs`** — Current re-exports.
- **`src/lib.rs`** (root) — The near-empty placeholder.

---

## 3. Goal

After this step:
1. Stale TUI crate dependencies removed from workspace
2. Vestigial root crate and feature flags cleaned up
3. `SessionDatabase` uses `enum_dispatch` — no more hand-written match arms
4. `moa-hands/src/router.rs` split into focused modules

---

## 4. Tasks

### 4a. Remove stale TUI dependencies from `Cargo.toml` workspace

These were only used by `moa-tui` which has been deleted. Remove from `[workspace.dependencies]`:

```toml
# DELETE these lines:
crossterm = "0.28"
ratatui = "0.29"
tui-textarea = "0.7"
syntect = { version = "5", default-features = false, features = ["default-fancy"] }
nucleo = "0.5"
similar = "2"
```

After removing, run `cargo build --workspace` to confirm nothing else depends on them. If any crate still uses one (e.g., `similar` might be used by `moa-brain` for diff rendering), keep only that one.

### 4b. Clean up root crate

The root `Cargo.toml` has a `[package]` section defining `moa-workspace` as a crate with `src/lib.rs`. This is unnecessary — it's a workspace manifest, not a real crate.

**Option A (recommended):** Remove `[package]`, `[lib]`, `[dependencies]`, and `[features]` sections entirely. Delete `src/lib.rs`. Keep only `[workspace]`, `[workspace.package]`, `[workspace.dependencies]`, `[profile.dev]`, `[profile.release]`.

**Option B (if Option A causes issues):** Keep the `[package]` but remove the empty `[features]` block (all feature flags except `postgres` are vestigial — `telegram`, `slack`, `discord`, `cloud`, `daytona`, `e2b` do nothing).

**Caveat:** The root `src/lib.rs` is used by the Tauri frontend (it's the Vite entry point for the web portion). Check if `vite.config.ts` or `index.html` references it. If so, the file is serving a different purpose and should NOT be deleted. In that case, only clean up the `[features]` block.

### 4c. `enum_dispatch` for `SessionDatabase`

Add `enum_dispatch` to `moa-session/Cargo.toml`:
```toml
enum_dispatch = "0.3"
```

Refactor `moa-session/src/backend.rs`:

```rust
use enum_dispatch::enum_dispatch;

#[enum_dispatch(SessionStore)]
#[derive(Clone)]
pub enum SessionDatabase {
    #[cfg(feature = "turso")]
    Turso(TursoSessionStore),
    #[cfg(feature = "postgres")]
    Postgres(PostgresSessionStore),
}
```

This eliminates the entire hand-written `impl SessionStore for SessionDatabase` block (~150 lines of mechanical delegation). The `#[enum_dispatch]` macro generates all the `match` arms.

**Also do the same for `ApprovalRuleStore`:**
```rust
#[enum_dispatch(ApprovalRuleStore)]
// ... (if enum_dispatch supports multiple traits on one enum — check docs)
```

If `enum_dispatch` doesn't support multiple traits on one enum, keep the `ApprovalRuleStore` impl manual (it's only 3 methods) and use `enum_dispatch` for `SessionStore` only.

The non-trait methods (`wake`, `cloud_sync_enabled`, `sync_now`, `from_config`, `backend`) stay as inherent methods on `SessionDatabase` since they're not part of the trait.

### 4d. Split `moa-hands/src/router.rs` into modules

Replace the 44KB `router.rs` with a `router/` directory:

```
moa-hands/src/router/
├── mod.rs              # ToolRouter struct, execute() dispatch, get_or_provision_hand()
├── normalization.rs    # Tool input normalization, summary generation, shell parsing
├── policy.rs           # Policy evaluation, approval decision logic, rule matching
└── registration.rs     # Tool registration, schema compilation, loadout building
```

Update `moa-hands/src/lib.rs` accordingly.

---

## 5. Deliverables

- [ ] Root `Cargo.toml` — Stale deps removed, features cleaned
- [ ] `src/lib.rs` — Deleted (if not used by Vite) or kept with comment
- [ ] `moa-session/Cargo.toml` — `enum_dispatch` added
- [ ] `moa-session/src/backend.rs` — `enum_dispatch` applied, ~150 lines removed
- [ ] `moa-hands/src/router.rs` — **DELETED** (replaced by directory)
- [ ] `moa-hands/src/router/mod.rs` — Core dispatch logic
- [ ] `moa-hands/src/router/normalization.rs` — Input processing
- [ ] `moa-hands/src/router/policy.rs` — Policy evaluation
- [ ] `moa-hands/src/router/registration.rs` — Tool registration

---

## 6. Acceptance criteria

1. `cargo build --workspace` compiles with zero errors.
2. `cargo test --workspace` passes.
3. No `crossterm`, `ratatui`, `tui-textarea`, `nucleo` in `Cargo.lock` (unless another crate legitimately uses them).
4. `moa-session/src/backend.rs` contains zero hand-written `match self { Turso(s) => s.foo(), Postgres(s) => s.foo() }` blocks for `SessionStore` methods.
5. `moa-hands/src/router.rs` single file does not exist.
6. No file in `moa-hands/src/router/` exceeds 400 lines.
7. Root workspace `Cargo.toml` has no empty feature flags (no `telegram = []`, `slack = []`, etc. that do nothing).

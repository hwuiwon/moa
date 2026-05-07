# S07 — Split `moa-session/src/store.rs` and `queries.rs`

## Scope

Break `crates/moa-session/src/store.rs` (~2,536 LOC) and `crates/moa-session/src/queries.rs` (~829 LOC) into folder-based modules organized by operation. **No SQL changes, no signature changes, no behavior changes.** This is a pure file split.

## Preconditions

- S01–S06 complete and merged.
- `cargo check --workspace` is green.
- `cargo test -p moa-session --no-run` compiles.

## Why this prompt

`store.rs` is the second-largest file in the workspace (2,536 LOC) and bundles the entire `SessionStore` trait impl: append, search, snapshot, segment management, status updates, retention, retry, and serialization. Splitting by operation makes each part editable in isolation and clears the way for the test pack to apply the contract-test pattern (which `tests/shared/mod.rs` already demonstrates) to a wider set of operations.

## Files in scope

- `crates/moa-session/src/store.rs` → deleted
- `crates/moa-session/src/store/mod.rs` → new (impl block aggregator)
- `crates/moa-session/src/store/<operation>.rs` → new files
- `crates/moa-session/src/queries.rs` → split similarly into `crates/moa-session/src/queries/`
- `crates/moa-session/src/lib.rs` → unchanged surface

## Files explicitly out of scope

- `crates/moa-session/tests/` — TEST pack handles
- The `SessionStore` trait itself (lives in `moa-core`)
- SQL migrations — out of scope entirely
- Any `.sql` file — untouched

## Step-by-step instructions

1. **Read `store.rs` end-to-end.** Identify operation groups. Likely:
   - `append.rs` — `append_event`, `append_batch`, idempotency, retries on conflict
   - `read.rs` — `get_event`, `range_read`, cursor pagination
   - `search.rs` — `search_events`, FTS queries
   - `snapshot.rs` — checkpoint creation, snapshot restore
   - `segments.rs` — segment management (cold storage, archival, compaction)
   - `status.rs` — `update_status`, status transitions, lifecycle
   - `retention.rs` — TTL, archival policy, deletion
   - `transaction.rs` — connection pooling, retry, transaction helpers
   - `serde.rs` — payload serialization helpers
   - `mod.rs` — `PostgresSessionStore` struct + `impl SessionStore for PostgresSessionStore`

2. **Read `queries.rs`.** This is likely the SQL string constants and query-builder helpers. Split by domain matching the `store/` folder:
   ```
   queries/
   ├── mod.rs
   ├── append.rs    — INSERT statements
   ├── read.rs      — SELECT statements
   ├── search.rs    — FTS queries
   ├── snapshot.rs  — checkpoint queries
   └── ...
   ```

3. **Use the impl-block-distribution pattern.** Rust allows splitting `impl Foo` across multiple files. Pattern:
   ```rust
   // crates/moa-session/src/store/mod.rs
   mod append;
   mod read;
   mod search;
   // ...
   
   pub struct PostgresSessionStore {
       pool: PgPool,
       config: SessionStoreConfig,
       // ...
   }
   
   impl PostgresSessionStore {
       pub fn new(pool: PgPool, config: SessionStoreConfig) -> Self { /* ... */ }
   }
   
   #[async_trait::async_trait]
   impl moa_core::traits::SessionStore for PostgresSessionStore {
       // Methods are spread across sibling files via `impl PostgresSessionStore` blocks
       // -- BUT trait methods MUST all live in this single impl block.
       //    Each trait method body delegates to a free fn or inherent method in the sibling file.
       
       async fn append_event(&self, ...) -> Result<...> {
           append::append_event_impl(self, ...).await
       }
       
       async fn search_events(&self, ...) -> Result<...> {
           search::search_events_impl(self, ...).await
       }
       
       // ... etc
   }
   ```
   This is a deliberate Rust idiom for splitting big trait impls. The trait impl itself stays in one block (Rust requires this), but each method delegates to a sibling-module `pub(super) fn` or method.

   **Alternative pattern** (simpler when methods don't share much state): use an inherent `impl PostgresSessionStore` block in each sibling file:
   ```rust
   // crates/moa-session/src/store/append.rs
   use super::PostgresSessionStore;
   
   impl PostgresSessionStore {
       pub(super) async fn append_event_impl(&self, ...) -> Result<...> { /* ... */ }
   }
   ```
   The trait impl in `mod.rs` then becomes:
   ```rust
   async fn append_event(&self, ...) -> Result<...> {
       self.append_event_impl(...).await
   }
   ```
   This works when each operation is largely self-contained.

   **Pick the simpler pattern per-operation.** If an operation is 300+ LOC with multiple helpers, use the inherent-impl pattern. If it's a single function, the free-fn pattern is fine.

4. **Move SQL strings to `queries/`.** Each operation file in `store/` imports its queries from the corresponding `queries/` file:
   ```rust
   // crates/moa-session/src/store/append.rs
   use crate::queries::append::{INSERT_EVENT, INSERT_BATCH};
   ```
   This separates "what SQL we run" from "how we orchestrate the run."

5. **Watch for shared helpers.** Functions like `map_db_error`, `parse_payload`, `build_event_record` are used by multiple operations. Keep them in `store/mod.rs` or `store/helpers.rs` as `pub(super)` items.

6. **Connection pool / transaction wrappers** stay in `store/mod.rs` or move to `store/transaction.rs`. They're owned by the struct, not by an operation.

7. **`lib.rs` re-exports**: the path `moa_session::PostgresSessionStore` (or whatever was previously exported) must continue to work. If `lib.rs` had `pub use store::PostgresSessionStore;`, that line stays.

8. **Run verification.**

9. **Document any SQL or logic that resisted clean splitting** in `REFACTOR_NOTES.md` under `[S07]` — for example, a query that composes parts from two domains and required a shared module.

## Verification

```bash
cargo check -p moa-session --all-targets
cargo clippy -p moa-session --all-targets -- -D warnings
cargo test -p moa-session --no-run
cargo check --workspace --all-targets   # downstream still compiles

# Specifically verify the FTS5 / Postgres tests still link:
cargo test -p moa-session --no-run --test postgres_store
```

If `cargo test --no-run` fails with "missing function" errors, the split missed a `pub(super)` annotation or moved a helper without updating its callers.

## Acceptance criteria

- [ ] `crates/moa-session/src/store.rs` no longer exists; replaced by `crates/moa-session/src/store/`.
- [ ] `crates/moa-session/src/queries.rs` no longer exists; replaced by `crates/moa-session/src/queries/`.
- [ ] No file in either folder exceeds 700 LOC.
- [ ] `impl SessionStore for PostgresSessionStore` block is in exactly one place (Rust requires this).
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change.
- [ ] No SQL string was modified (verify with `git diff -- '*.rs' | grep -E '^[+-]' | grep -i -E 'INSERT|SELECT|UPDATE|DELETE'` — should show only line-position changes, not content changes).

## Rollback plan

`git checkout -- crates/moa-session/src/` and `git clean -fd crates/moa-session/src/store crates/moa-session/src/queries`.

## Notes for the agent

- **The `tests/shared/mod.rs` contract test is gold.** Do not modify it. It generic-over-`SessionStore`s a comprehensive test suite — that's the pattern other crates should adopt later. Just make sure it still compiles.
- **Don't merge `store/` and `queries/` into one folder.** Separation of "orchestration" from "SQL strings" is intentional and worth preserving.
- **Watch for compile errors about visibility.** When a helper moves from `pub(crate)` to `pub(super)`, callers in *sibling* modules can still see it; callers in *parent* modules cannot. Rare but happens.
- **`sqlx::query!` macros stay where the query string is.** If a `query!` macro pulls a string from `queries/`, that's fine — sqlx evaluates at compile time regardless of where the literal came from.
- **`async_trait` impl block can only exist once.** This is the most likely "trip" — don't try to split the `impl SessionStore` across multiple files. Use the delegation pattern.
- **Time budget**: ~1.5 sessions for a careful split. The query-string move is mostly mechanical; the `impl` distribution is where attention is needed.
- **Anti-pattern**: don't add caching, retry policy changes, or "while we're here" improvements. Pure structural move.

# S01 — Hoist common dependencies to `[workspace.dependencies]`

## Scope

Move dependencies that are used in 2+ workspace crates with matching versions up to `[workspace.dependencies]` in the root `Cargo.toml`. Replace per-crate version specifiers with `.workspace = true` references. **No code changes.** No version bumps. No new dependencies.

## Preconditions

- On a clean working tree at `main` (or wherever you've branched from `main`).
- `cargo check --workspace` is currently green.

## Why this prompt

The workspace currently has 15 crates declaring `sqlx` with overlapping but non-identical feature sets, and at least 12 other dependencies (`blake3`, `sha2`, `secrecy`, `moka`, `shell-words`, `tempfile`, `axum`, `toml`, `arrow`, `parquet`, `object_store`, `globset`, `base64`, `regex`, `hex`, `ed25519-dalek`) repeated across multiple crates with matching versions. Every version bump touches multiple files. Hoisting reduces coordination cost during the rest of the refactor.

## Files in scope

- Root `Cargo.toml`
- Every `crates/*/Cargo.toml` and `crates/moa-memory/*/Cargo.toml` and `crates/moa-lineage/*/Cargo.toml`

## Files explicitly out of scope

- Any `.rs` file. Zero source changes in this prompt.
- `Cargo.lock` will be regenerated automatically; do not hand-edit it.

## Step-by-step instructions

1. **Inventory phase.** Run `cargo metadata --format-version 1 --no-deps | jq '...'` (or grep through `Cargo.toml` files) and produce a table of dependencies used in ≥2 crates. For each, note: name, version specifier(s) found, feature set(s) found, and the list of crates that use it.

2. **Identify the hoist set.** A dependency is hoistable when:
   - It appears in ≥2 workspace crates (including `[dev-dependencies]` counts toward the 2)
   - All occurrences specify the same version (or compatible semver — pick the highest)
   - Feature sets are reconcilable (see step 4)
   
   Skip any dependency where feature sets diverge in a way that can't be unified without behavior changes.

3. **Add a `[workspace.dependencies]` section** to the root `Cargo.toml`. For each hoistable dep, add:
   ```toml
   <name> = { version = "X.Y", default-features = <bool>, features = [...] }
   ```
   The feature set should be the **union** of all features used across crates *only if* every crate is fine receiving the union (default-features off, features additive). If a crate intentionally opts *out* of a default feature, do NOT hoist that dep — flag it in `REFACTOR_NOTES.md` and move on.

4. **`sqlx` deserves special handling.** The 15 crates using `sqlx` have differing feature sets. Strategy:
   - Hoist a base `sqlx = { version = "...", default-features = false, features = ["runtime-tokio-rustls", "macros"] }` (or whatever the common minimum is — read the actual files first).
   - Each crate then declares `sqlx = { workspace = true, features = ["..."] }` adding *only* the features it uniquely needs.
   - This is the documented pattern in Cargo's workspace inheritance docs. Verify `cargo check` passes for every crate after the conversion.

5. **Convert each crate's `Cargo.toml`** to use `.workspace = true`:
   ```toml
   # before
   blake3 = "1.5"
   # after
   blake3 = { workspace = true }
   ```
   For deps with crate-specific features:
   ```toml
   sqlx = { workspace = true, features = ["postgres", "json"] }
   ```

6. **Verify.** Run the verification block below. Fix any issues. Do not proceed if anything fails.

7. **Document.** In `REFACTOR_NOTES.md`, add an entry under `[S01]` for any dep that was *not* hoisted, with the reason (feature divergence, version mismatch, etc.).

## Verification

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo tree --workspace --duplicates  # should not show new duplicates
```

The `cargo tree --duplicates` line is the key one — if hoisting introduces a duplicate version of any crate that wasn't there before, the hoist for that dep is wrong; back it out.

## Acceptance criteria

- [ ] Root `Cargo.toml` has a populated `[workspace.dependencies]` section with all hoistable deps.
- [ ] Every workspace crate uses `.workspace = true` for hoisted deps.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo tree --workspace --duplicates` shows no *new* duplicates compared to before this prompt.
- [ ] Any non-hoistable deps are documented in `REFACTOR_NOTES.md`.

## Rollback plan

If anything breaks: `git checkout -- .` and `git clean -fd`. The change is self-contained in `Cargo.toml` files.

## Notes for the agent

- **Do not change versions.** Even if a dep has a newer version available, this is not the prompt for that.
- **Do not add new deps.** If a crate is missing a dep it should have, that's a separate prompt.
- **`Cargo.lock` will change** because the structural representation of deps has changed. That's expected. Do not commit any changes that are NOT in `Cargo.toml` files or `Cargo.lock`.
- **Watch for `default-features = false`.** If a crate disables defaults, the hoisted version must also disable them, and re-enabling crates must explicitly opt in. Easy to get wrong.
- **`tokio` is special.** It's used everywhere with overlapping feature sets that mostly union cleanly. Hoist it. `tokio = { workspace = true, features = ["rt-multi-thread", "macros", ...] }` per-crate.
- **Workspace member list itself is not in scope.** Don't add or remove members from `[workspace] members = [...]`.
- **Stop at ~25 hoisted deps.** If the diff gets larger than that, you're hoisting too aggressively. The point is correlation reduction, not maximizing the workspace.dependencies block.

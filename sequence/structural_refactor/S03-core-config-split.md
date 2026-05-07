# S03 — Split `moa-core/src/config.rs` into a feature-folder

## Scope

Break the 2,336-LOC `moa-core/src/config.rs` into a `moa-core/src/config/` folder with one file per logical sub-domain. Maintain bit-for-bit API compatibility via `pub use` re-exports from `mod.rs`. **No type changes, no behavior changes.**

## Preconditions

- S01 + S02 complete and merged.
- `cargo check --workspace` is green.
- This is the first prompt to touch source code; expect everything downstream to depend on its output.

## Why this prompt

`moa-core/src/config.rs` is the single largest file in `moa-core` and almost certainly bundles configuration for: providers, sandboxing, telemetry, sessions, memory, lineage, security, eval, and CLI defaults. A 2.3k-LOC config file is a coordination tax on every crate that imports it — every PR touches it, every code review has to scan it. Splitting by sub-domain unblocks parallel work in later prompts.

## Files in scope

- `crates/moa-core/src/config.rs` → deleted
- `crates/moa-core/src/config/mod.rs` → new (re-exports + top-level `Config` struct)
- `crates/moa-core/src/config/<sub-domain>.rs` → new files (see step 3)
- `crates/moa-core/src/lib.rs` → unchanged surface (`pub mod config;` already there, presumably)

## Files explicitly out of scope

- Any other file in `moa-core/`. Don't refactor `types/`, `traits/`, `error.rs`, `telemetry.rs` in this prompt.
- Any other crate. Their imports of `moa_core::config::...` must continue to resolve unchanged.
- `moa_core::config::Config` (the top-level type) must keep the same name and field layout. No renames, no field reorderings.

## Step-by-step instructions

1. **Read `config.rs` end-to-end** before making any change. Build a mental map of:
   - Top-level types (likely `Config`, `MoaConfig`, or similar)
   - Sub-config types (`ProviderConfig`, `SessionConfig`, `MemoryConfig`, etc.)
   - `impl` blocks
   - `Default` impls
   - Validation functions
   - Loading functions (TOML parsing, env-var reading)
   - Any free functions

2. **Identify natural sub-domains.** The expected partitions (verify against actual content):
   - `providers.rs` — LLM provider config (Anthropic, Gemini, OpenAI, embedding settings)
   - `session.rs` — Session storage (Postgres URL, replay, retention)
   - `memory.rs` — Memory subsystem (vector, graph, ingest, PII, paths)
   - `lineage.rs` — Lineage / OTel / audit / cold-storage settings
   - `sandbox.rs` — Hand provider config (local/Daytona/E2B/MCP)
   - `security.rs` — Vault, credential proxy, approval rules, prompt-injection settings
   - `gateway.rs` — Telegram/Slack/Discord settings, if config lives in core
   - `telemetry.rs` — Tracing / metrics / log config
   - `eval.rs` — Evaluator settings if present in core
   - `defaults.rs` — Default values, ENV overrides, validation rules
   - `loader.rs` — TOML/YAML parsing, env-var merging, file discovery
   - `mod.rs` — Top-level `Config` struct + `Config::load()` + re-exports

   **Adjust this list to match actual content.** The file may have fewer or more sub-domains.

3. **Create the folder structure**:
   ```
   crates/moa-core/src/config/
   ├── mod.rs
   ├── providers.rs
   ├── session.rs
   ├── memory.rs
   ├── lineage.rs
   ├── sandbox.rs
   ├── security.rs
   ├── telemetry.rs
   ├── defaults.rs
   └── loader.rs
   ```

4. **Move types verbatim.** For each sub-config type:
   - Cut the type definition + its `impl` blocks + its `Default` impl from the old file
   - Paste into the appropriate new sub-file
   - Add `use super::*;` at the top of each sub-file (then narrow to specific imports as you go)
   - Mark types `pub` (they were already pub at module level)
   - Add `#[derive(Debug, Clone, Serialize, Deserialize, Default)]` only if it was already there

5. **`mod.rs` should contain**:
   ```rust
   //! Configuration for moa, organized by sub-domain.
   
   pub mod providers;
   pub mod session;
   pub mod memory;
   pub mod lineage;
   pub mod sandbox;
   pub mod security;
   pub mod telemetry;
   pub mod defaults;
   pub mod loader;
   
   // Re-export to preserve `moa_core::config::Foo` paths.
   pub use providers::*;
   pub use session::*;
   pub use memory::*;
   pub use lineage::*;
   pub use sandbox::*;
   pub use security::*;
   pub use telemetry::*;
   pub use defaults::*;
   pub use loader::*;
   
   // Top-level Config struct stays here (it composes sub-configs).
   pub struct Config { /* ... */ }
   
   impl Config {
       pub fn load() -> Result<Self, ConfigError> { /* ... */ }
       // etc.
   }
   ```

6. **The wildcard `pub use` is intentional and temporary**. It preserves `moa_core::config::Foo` for every existing call site. After all downstream prompts have stabilized, S14 will optionally tighten these to explicit re-exports.

7. **Move `impl` blocks to the file containing the type.** If `impl ProviderConfig { ... }` is in the loader section, it moves to `providers.rs`. If a `Default` impl is on `ProviderConfig`, it moves with the type.

8. **Loader logic stays in `loader.rs`**. The `Config::load`, `Config::from_toml`, env-var merging, file discovery — all in `loader.rs`. The top-level `impl Config` block can split: methods that *load* live in `loader.rs` (via `impl Config` block in that file), methods that *operate on* a loaded Config (validators, accessors) stay in `mod.rs`.

9. **Verify nothing in `moa-core/src/config.rs` survives.** Delete the old file. The folder + `mod.rs` is the new home.

10. **Run the full verification block**.

11. **If `cargo check --workspace` reveals broken downstream imports**, the fix is *additional re-exports in `mod.rs`*, not modifications to downstream crates. The contract is "downstream code keeps working."

12. **Document anything that resists clean splitting** in `REFACTOR_NOTES.md` under `[S03]` — for example, a sub-config that's tightly coupled to two domains and required a new trait or helper.

## Verification

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo public-api --package moa-core --diff-against main  # if cargo-public-api is available
```

The `cargo-public-api` step is optional but worth running. **The diff should be empty** — this prompt does not change the public API of `moa-core`.

## Acceptance criteria

- [ ] `crates/moa-core/src/config.rs` no longer exists.
- [ ] `crates/moa-core/src/config/mod.rs` exists and re-exports all sub-modules with `pub use`.
- [ ] No file in `crates/moa-core/src/config/` exceeds 600 LOC (smaller is fine; the goal is sub-domain isolation, not file-size minimization).
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change to compile.
- [ ] (If available) `cargo public-api --package moa-core` shows zero changes.

## Rollback plan

`git checkout -- crates/moa-core/src/config.rs crates/moa-core/src/config/` and `git clean -fd crates/moa-core/src/config/`. The change is contained to one folder.

## Notes for the agent

- **Do not change any type's name, field name, or visibility.** This is a move-and-paste exercise.
- **Do not add validation or "improve" the error types.** Note them in `REFACTOR_NOTES.md` if obvious bugs surface, but don't fix.
- **Do not remove any `pub` re-export.** Even if you think nothing uses it. The point of S03 is API-preserving.
- **The `pub use foo::*;` pattern is fine for now.** Yes, it's wildcard. Yes, S14 will tighten. Don't pre-optimize.
- **If a type is generic or has complex bounds**, copy the full signature. Don't simplify.
- **Imports inside the new sub-files**: the agent should use `rust-analyzer` (or `cargo check` errors) to drive specific imports rather than relying on `use super::*;` long-term. Tighten as the file stabilizes.
- **Time budget**: this prompt should fit in one session. If it's running long because the sub-domain split is unclear, stop and consult the user — it's better to leave half the splits in place and document the rest in `REFACTOR_NOTES.md` than to force a split that doesn't fit.
- **Anti-pattern**: do NOT introduce `pub(crate)` to "tighten visibility" in this prompt. Visibility is a separate audit; mixing it with the move makes review harder.

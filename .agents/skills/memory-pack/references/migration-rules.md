# Migration Rules

How to land memory-pack steps without leaving compatibility shims, deprecated aliases, or duplicated SQL boilerplate.

## Hard Break by Default

- Memory-pack steps prefer hard breaks over compatibility shims unless the prompt explicitly asks for backwards compatibility.
- Delete obsolete wiki, vector, or tool paths when the step says cleanup. Do not leave deprecated aliases that just forward to the new path.
- Do not introduce tuple-variant compatibility, JSON parsing shims for old shapes, or `#[deprecated]` markers as a substitute for actually removing the old code.
- If an old test enforces the old shape and the step removes the old shape, delete the test. Do not weaken the assertion to be permissive.

## SQL Helper Conventions

- Prefer compact SQL helpers and templates over duplicated policy blocks. If three migrations would write the same `CREATE POLICY` boilerplate, the third is the signal to extract a helper.
- Helpers belong in a clearly-named module (e.g. `migrations/policy_helpers.rs`), not scattered across migration files.
- Migration files should read top-to-bottom as the intent of the change. If they read as a wall of `CREATE POLICY` repetition, refactor.

## Idempotency

- Migrations must be idempotent. Run `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`, `DROP TRIGGER IF EXISTS` before recreating, and `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` where supported.
- For Postgres versions that do not support `IF NOT EXISTS` on a specific DDL, wrap the operation in a `DO $$ BEGIN ... EXCEPTION WHEN duplicate_object THEN NULL; END $$` block.
- Migrations that depend on a prior step's data must still be idempotent on a clean database. Use `INSERT ... ON CONFLICT DO NOTHING` for seed data.

## Tool Naming

- Tool names use underscores, not dotted names. `memory_remember` is correct; `memory.remember` is not.
- This applies in tool catalogs, MCP server registrations, and any string identifier that names a tool.

## Live and Billed Behavior

- For live or billed providers introduced by a memory-pack step, add permanent ignored tests with explicit env opt-in flags. Do not treat live tests as optional or transient.
- The opt-in flag pattern is `MOA_RUN_LIVE_<PROVIDER>_TESTS=1`. The `certify` skill's test matrix is the source of truth for the flag conventions.

## Verification Before Handoff

Before reporting the step done:

- `cargo fmt --all`
- focused tests for the changed crate
- `cargo clippy -p <crate> --all-targets --all-features --locked -- -D warnings`
- `cargo build --workspace` when public APIs or shared crates changed
- `git diff --check`

If broader release validation is needed, hand off to `certify` and name the surfaces that changed.

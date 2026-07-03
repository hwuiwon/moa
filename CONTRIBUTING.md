# Contributing

For test quality rules, read `AGENTS.md`'s `Testing standards` section before adding, weakening, or deleting tests.

## CodeGraph CLI

This repo uses the CodeGraph code knowledge graph CLI for local code navigation.
The generated index lives in `.codegraph/` and is intentionally untracked.

- `./scripts/codegraph` runs the repo-pinned package version without requiring a
  global install.
- `make codegraph` initializes or refreshes the local `.codegraph/` index.
- Use `./scripts/codegraph explore "<question>"` when MCP tools are unavailable.

Agent-level MCP wiring is managed by CodeGraph's installer. Run
`./scripts/codegraph install` if you want CodeGraph available in your local
agent configuration.

## How Tests Are Organized

Keep integration tests grouped by behavior, not by the implementation type that happens to own the code today. A file named after a broad component tends to grow into a kitchen-sink suite and makes local iteration harder.

Restate-backed integration tests should use `moa-test-support`'s
`OrchestratorTestFixture`. Keep behavior assertions in behavior-named test
files, and keep stack setup inside shared fixture helpers.

Use `OrchestratorTestFixture::shared().await?.isolated().await` for tests that
only need unique session/tenant IDs. Use `serialized()` only for tests that
mutate shared orchestrator state, such as cron configuration or provider
fixture replacement.

Avoid adding a new catch-all file such as `local_orchestrator.rs` or `skills.rs`. If a new test does not clearly fit an existing behavior file, create a new behavior-named file with a short module-level doc comment.

Offline, `_db`, and `_db_memory` behavior files compile into one harness
binary per crate per lane (for example
`crates/moa-orchestrator/tests/orchestrator_offline.rs` declares
`#[path = "orchestrator_offline/session_vo.rs"] mod session_vo;`). Every file
directly under `tests/` links as its own binary, and binary count dominates
link and nextest-listing time — so put new behavior files under the harness
directory and add a `mod` line to the harness instead of creating a new root
file. Run one behavior file with
`cargo test -p <crate> --test <harness> <module_name>`. The
`_service_e2e`/`_provider_e2e`/`_live`/`_eval` lanes and binaries pinned by
name in `.config/nextest.toml` or scripts stay standalone.

For the inner loop, `make test-affected` runs only the tests of crates
affected by your change set (merge base with `main` plus uncommitted files);
`make test-fast` runs the full deterministic lane before a PR.

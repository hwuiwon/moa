# Contributing

For test quality rules, read `AGENTS.md`'s `Testing standards` section before adding, weakening, or deleting tests.

## How Tests Are Organized

Keep integration tests grouped by behavior, not by the implementation type that happens to own the code today. A file named after a broad component tends to grow into a kitchen-sink suite and makes local iteration harder.

Restate-backed integration tests should use `moa-test-support`'s
`OrchestratorTestFixture`. Keep behavior assertions in behavior-named test
files, and keep stack setup inside shared fixture helpers.

Use `OrchestratorTestFixture::shared().await?.isolated().await` for tests that
only need unique session/workspace IDs. Use `serialized()` only for tests that
mutate shared orchestrator state, such as cron configuration or provider
fixture replacement.

Avoid adding a new catch-all file such as `local_orchestrator.rs` or `skills.rs`. If a new test does not clearly fit an existing behavior file, create a new behavior-named file with a short module-level doc comment.

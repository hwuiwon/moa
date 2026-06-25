# Contributing

For test quality rules, read `AGENTS.md`'s `Testing standards` section before adding, weakening, or deleting tests.

## Graphify CLI

This repo uses the `graphify` knowledge-graph CLI (see `AGENTS.md`'s `Graphify`
section and the `.claude` hooks). The version is pinned in
`.agents/skills/graphify/.graphify_version`, and everything resolves to that one
version via [`uv`](https://docs.astral.sh/uv/) — no pyenv/venv setup required.

- **Agents / `.claude` hooks** invoke `./scripts/graphify`, a wrapper that runs
  the pinned version with `uvx`. This needs only `uv` installed; there is no
  separate install step, and it never depends on whatever `graphify` you may
  have globally.
- **To type `graphify` directly** in your own shell, optionally install the
  pinned version onto your `PATH`:

  ```sh
  make graphify
  ```

  Re-run it if the pinned version changes. The wrapper above stays pinned
  automatically regardless.

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

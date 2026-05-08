# Contributing

For test quality rules, read `AGENTS.md`'s `Testing standards` section before adding, weakening, or deleting tests.

## How Tests Are Organized

Keep integration tests grouped by behavior, not by the implementation type that happens to own the code today. A file named after a broad component tends to grow into a kitchen-sink suite and makes local iteration harder.

The local orchestrator tests are the reference layout:

- `crates/moa-orchestrator-local/tests/lifecycle.rs` covers session creation, completion, listing, and lifecycle cleanup.
- `crates/moa-orchestrator-local/tests/signals.rs` covers queued messages and cancellation signals.
- `crates/moa-orchestrator-local/tests/approval.rs` covers approval request, allow, deny, and approval-adjacent queue behavior.
- `crates/moa-orchestrator-local/tests/recovery.rs` covers resume, replay, crash-like recovery, and failed-provider behavior.
- `crates/moa-orchestrator-local/tests/observe.rs` covers runtime and persisted-event observation.
- `crates/moa-orchestrator-local/tests/bootstrap.rs` covers constructor, model-routing, workspace bootstrap, local workspace detection, and session-limit behaviors.

When Local and Restate runtimes should satisfy the same behavior, put the assertion in a shared harness instead of duplicating it. The orchestrator contract helper at `crates/moa-orchestrator-local/tests/support/orchestrator_contract.rs` is the pattern to follow: expose the smallest harness trait needed by the assertion, then keep runtime-specific setup in the behavior-named test file.

Avoid adding a new catch-all file such as `local_orchestrator.rs` or `skills.rs`. If a new test does not clearly fit an existing behavior file, create a new behavior-named file with a short module-level doc comment.

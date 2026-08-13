# Orchestrator Instructions

Read `docs/02-brain-orchestration.md`, `docs/05-session-event-log.md`, and
`docs/12-restate-architecture.md`. Keep this crate at the Restate transport,
authorization, workflow, and composition boundary; domain decisions and SQL
belong in their owning services or repositories. Preserve journal step names,
serialized state, replay behavior, and authorization-before-read ordering.

Use `fast-pr`, `db-session`, or `db-memory` for focused checks. Deterministic
service and recovery profiles require their named repository fixture or E2E
harness, but not live authorization. Set `MOA_RUN_LIVE_E2E=1` only for an
explicitly live target; provider credentials do not belong in deterministic
lanes.

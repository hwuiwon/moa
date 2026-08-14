# Execution Instructions

Read the execution ownership section of `docs/01-architecture-overview.md` and
`docs/15-architecture-policy.md`. Keep compiler, interpreter, and scheduler
transitions pure; repositories own SQL and fencing, while Restate remains in
`moa-orchestrator`. Preserve SQL lock/order, idempotency keys, generation
fences, accounting, and public `ExecutionRepository` paths.

Use `fast-pr` for pure logic and `db-session` for repository behavior. The
deterministic `execution-eval-pr` service lane runs through the clean E2E
harness without live authorization; set live flags only for explicitly live or
provider-backed targets.

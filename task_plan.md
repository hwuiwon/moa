# Action Policy Auto Mode Planning

## Goal

Create a concrete implementation plan for replacing blocking end-user tool approvals with a simple action-policy auto mode: execute valid workflow or skill tool steps by default, deny policy violations, and route exceptional review to workspace admins without blocking the conversation.

## Current Status

- Phase 1: Initialize planning files - complete
- Phase 2: Map current approval, tool policy, workflow, and event surfaces - complete
- Phase 3: Decide target architecture and breaking-change boundaries - complete
- Phase 4: Write executable implementation plan document - complete
- Phase 5: Self-review plan and report to user - complete

## Constraints

- Do not implement code changes in this turn.
- Prefer breaking changes over compatibility shims when they simplify long-term architecture.
- Keep the production architecture simple and inside the existing MOA modular monolith.
- Remove end-user blocking approval semantics from the normal tool-execution path.
- Treat workspace-admin review as an asynchronous enterprise control, not a user chat prompt.
- Enterprise actions should flow through typed tool/function/MCP calls rather than broad shell behavior.

## Decisions

- User wants auto-mode semantics: execute unless policy denies or routes to admin review.
- No backwards compatibility or compatibility shim is required.
- User-facing autonomy tiers are not desired; internal action classes are acceptable if they keep policy simple.
- The implementation plan uses `ActionReviews` for tool/action review and splits builtin async-authz rows into `AuthzChallenges`.
- Admin-cleared review execution must use a fresh tool-call id so the original pending-review tool result does not trip non-idempotent replay protection.

## Plan Artifact

- `docs/engineering-discipline/plans/2026-06-18-action-policy-auto-mode.md`

## Errors Encountered

| Error | Attempt | Resolution |
|---|---|---|
| `rg` reported missing top-level `src` and `tests` paths | Initial repo-wide approval search | Continue with `crates/` and `docs/`, which are the relevant MOA roots |
| `rg` reported missing top-level `config` path | Config/migration approval search | Continue with existing crate config and migration paths |
| Tried `crates/moa-orchestrator/src/objects/session.rs` and `sub_agent.rs` | Initial object lookup | Actual paths are under `crates/moa-orchestrator/src/objects/session/` and `crates/moa-orchestrator/src/objects/sub_agent/` |
| Tried `crates/moa-hands/src/router.rs` | Initial router lookup | Actual router modules are under `crates/moa-hands/src/core/` |

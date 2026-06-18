# Progress: Action Policy Auto Mode Planning

## 2026-06-18

- Read the `planning-with-files` skill and ran its session catchup script.
- Read the relevant `graphify` skill sections and confirmed this task should use graphify first because the repo has an existing graph for codebase architecture questions.
- Created persistent planning files in the MOA repo root.
- Ran `graphify query` for the action-policy redesign. It identified approval service, turn execution, sub-agent execution, policy, session status, and tests as the relevant implementation surfaces.
- Ran an initial approval/policy text search. The search included two missing top-level paths, but still produced useful crate and docs hits; this was recorded as a search-shape issue, not a code issue.
- Read the relevant architecture docs for runtime services, brain orchestration, communication approvals, hands/MCP tool routing, security approval semantics, and event-log groups. The docs currently encode the blocking approval model and must be updated as part of the breaking change.
- Read core approval, session, sub-agent, wire, event, tool, runtime-event, and session-engine types. Blocking approval is embedded in serialized events, lifecycle statuses, replay helpers, and tool policy metadata, so the plan should make a clean breaking type replacement.
- Read security policy, config, session-store approval rules, migration snippets, `WorkspaceStore`, `Approvals`, and approval reaper code. The clean boundary is to rename/replace the policy service and store types, while being careful not to conflate privacy approval-token flows with tool/action review.
- Read root and sub-agent turn execution approval gates. Both workflows block on awakeables and must be changed together.
- Located the actual session/sub-agent object modules after an incorrect initial path guess.
- Read session and sub-agent VO trait, handler, and state modules. The plan must delete pending approval VO state/methods and event-based pending awakeable reconstruction.
- Read `moa-hands` policy, dispatch, registration, normalization, and router struct files. The router already has the right policy/evaluation boundary but its names/defaults are approval-centric.
- Read `ToolExecutor`, public edge approval routes, and orchestrator startup registration. Policy is checked before `ToolExecutor`, while edge/startup expose `Approvals` as a named product surface that must be renamed for action review.
- Mapped the main tests that currently pin approval behavior across orchestrator E2E, behavior lab, experiment service, tool executor descriptors, hands diff previews, shell matching, and session blob claim checks.
- Checked migration numbering and workspace package names for focused verification. A new forward migration after `V000301__ocsf_baseline.sql` is the clean path.
- Checked public tool descriptors, runtime metrics, analytics status parsing, experiment status models, and artifact workflow statuses for remaining `waiting_approval` surfaces.
- Checked OpenFGA relation names and existing workspace-admin authorization helpers. The action-review service can use `Workspace:Admin`.
- Checked skill/workflow context surfaces. Current tool calls do not carry strict skill-step origin, so the plan should add optional origin fields in the action envelope without overbuilding manifest validation in the first pass.
- Created executable plan at `docs/engineering-discipline/plans/2026-06-18-action-policy-auto-mode.md`.
- Self-reviewed the plan for placeholder terms and ambiguity; tightened admin-cleared execution to use a fresh tool-call id and removed optional implementation branches.
- Began executing with `run-plan`. Task 1's core-only implementation passed focused core checks, but the information-isolated validator failed on full-workspace build because downstream crates intentionally had not yet migrated from the removed core approval API.
- Amended the plan with a `Run-Plan Execution Boundary`: detailed Tasks 1-10 are now ordered checklist sections inside one executable end-to-end task, with full workspace and E2E verification at that boundary instead of after the intentionally breaking core-only slice.
- After the executable task hit the validator retry limit, added `Executable Task B: Clean Up Residual Tool-Approval Language` to target remaining docs/comments/helper names such as `single_approval_field`, `has_approval_unsafe_shell_syntax`, and `approval matching` in tool/action surfaces.

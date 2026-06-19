# Findings: Action Policy Auto Mode

## Initial Scope

The requested change is an architecture-level redesign of MOA's approval path. The implementation plan must be based on the current repo files, not on preserving old approval names or compatibility behavior.

## User Decisions Captured

- Default behavior should execute tool actions unless policy denies.
- A valid workflow or skill step should be able to perform its declared action, including higher-impact actions, when workspace policy authorizes that capability.
- Review, when required, belongs to workspace admins rather than the end user in conversation.
- Conversation should continue while an admin-reviewed action is pending.
- Enterprise actions should primarily be represented as defined function calls and MCP tools.
- There is no need to expose configurable autonomy tiers if auto mode plus policy escalation covers the product behavior.

## Open Architecture Questions To Resolve In Plan

- Which current approval types should be deleted versus renamed into workspace action review.
- Which crate should own the action envelope and policy decision types.
- How turn execution should represent pending admin review without `WaitingApproval`.
- How tests should prove "execute by default" and "admin review does not block conversation".

## Graphify And Initial Search Findings

- `graphify query` identified the approval and policy hot spots as `crates/moa-orchestrator/src/services/approvals.rs`, `crates/moa-orchestrator/src/workflows/turn_execution.rs`, `crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs`, `crates/moa-security/src/policies.rs`, session/sub-agent status types, and approval-flow integration tests.
- Current approval terms are spread through docs, core types, edge routes, session store approval-rule persistence, orchestrator services, experiment-trial status mapping, and messaging approval cards.
- Initial `rg` included non-existent top-level `src` and `tests` paths. The useful results came from `crates/` and `docs/`; subsequent searches should target those roots only.

## Architecture Doc Findings

- `docs/01-architecture-overview.md` lists `Approvals` as a cloud service and `TurnExecution` / `SubAgentTurnExecution` as workflows. The plan must update the service name and workflow behavior together.
- `docs/02-brain-orchestration.md` currently says risky tool calls emit `ApprovalRequested`, store an awakeable, block the invocation, and resume through an approval handler. This is the exact behavior to replace.
- `docs/03-communication-layer.md` currently describes user-rendered approval cards, allow-once, always-allow, and deny decisions. The plan should replace this with workspace-admin action-review observation.
- `docs/06-hands-and-mcp.md` already says tool descriptors carry risk level, approval default, and idempotency behavior. This is the natural place to move toward action envelopes and policy decisions.
- `docs/08-security.md` says prompt filtering is not a complete boundary and approval rows are durable product state. The new architecture should preserve action audit durability while removing awakeable-based conversation blocking.
- `docs/05-session-event-log.md` lists `ApprovalRequested` / `ApprovalDecided` as major session events. The event model must be renamed or replaced for action review.

## Core Type Findings

- `crates/moa-core/src/types/approval.rs` owns `ApprovalDecision`, `ApprovalRequest`, `ApprovalPrompt`, `PolicyAction::RequireApproval`, and `ApprovalRule`. These names encode end-user approval and should be replaced with action-policy and workspace action-review types.
- `crates/moa-core/src/types/session.rs` has both `SessionStatus::WaitingApproval` and `TurnOutcome::WaitingApproval`, plus a `SessionSignal::ApprovalDecided`. The plan should remove approval waiting from normal session lifecycle.
- `crates/moa-core/src/types/sub_agent.rs` has `SubAgentState::WaitingApproval`; sub-agents need the same removal.
- `crates/moa-core/src/wire.rs` defines `SetSessionPendingApprovalInput` and `ClearSessionPendingApprovalInput`, which are only needed for awakeable-based blocking turns.
- `crates/moa-core/src/events.rs` serializes `ApprovalRequested` / `ApprovalDecided` events with `awakeable_id`, prompts, and user decisions. Since compatibility is not required, the plan should replace these with action-policy/action-review events instead of reusing approval names.
- `crates/moa-core/src/events/tool_approval.rs` reconstructs blocked or resolved approval state from the session log. This helper should be deleted or replaced by action-review query helpers that do not cause session reprocessing by themselves.
- `crates/moa-core/src/session_engine.rs` currently treats pending and resolved tool approvals as requiring processing. That behavior should go away for admin review because a pending admin review should not force the conversation into a blocked processing state.
- `crates/moa-core/src/types/tools.rs` attaches `PolicyAction` directly to tool definitions and uses `write_tool_policy` to default write tools to `RequireApproval`. The new plan should make tool descriptors declare action metadata and default auto-execution policy, not approval defaults.
- `crates/moa-core/src/types/runtime_events.rs` has `ToolCardStatus::WaitingApproval` and `RuntimeEvent::ApprovalRequested`; these should become pending-admin-review or policy-denied events if UI streaming still needs them.

## Policy, Store, And Service Findings

- `crates/moa-security/src/policies.rs` already owns the policy engine boundary, but names it `ToolPolicies`, returns `PolicyAction`, and stores `ApprovalRule` through `ApprovalRuleStore`.
- `ToolPolicies::default()` currently leaves bash/write tools at `PolicyAction::RequireApproval`; tests assert this. These tests need to flip to auto-allow unless an action rule denies or escalates.
- `crates/moa-core/src/config/security.rs` exposes `default_posture`, `auto_approve`, and `always_deny`. The plan should replace approval-oriented config with simple action-policy config, likely keeping `always_deny` and adding admin-review rules rather than auto-approve lists.
- `crates/moa-session/src/store/approval.rs` and `crates/moa-session/src/queries/rows.rs` persist `approval_rules`. Since compatibility is not required, this can be renamed to action policy rules in a forward migration plus Rust store rename.
- `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql` creates `approval_rules`; changing an old migration would be risky for applied databases. The implementation should add a forward migration that creates/replaces the new table and leaves old applied migrations untouched.
- `crates/moa-migrations/migrations/postgres/V000101__auth_baseline.sql` creates `builtin_pending_approvals` for async authz approval rows with awakeables. This is distinct from tool approval but shares the `Approvals` service.
- `crates/moa-orchestrator/src/services/workspace_store.rs` currently calls `ToolRouter::prepare_invocation`, returns `PreparedToolApproval`, and only includes an approval prompt when `PolicyAction::RequireApproval`. This is the right boundary to rename into action evaluation.
- `crates/moa-orchestrator/src/services/approvals.rs` merges builtin async-authz rows and event-backed tool approvals under `Approvals/list_mine` and `Approvals/decide`. The plan should split tool/action review from builtin async authz or rename the whole service to workspace action review if builtin rows remain in scope.
- `crates/moa-orchestrator/src/services/approvals_reaper.rs` resolves expired builtin approval awakeables. This belongs to async-authz compatibility, not to the new tool auto-mode path.
- Search note: a second `rg` included missing top-level `config`; future searches should target crate paths only.

## Workflow And VO Findings

- `crates/moa-orchestrator/src/workflows/turn_execution.rs` checks `PolicyAction::Deny`, then blocks on `PolicyAction::RequireApproval` via `handle_approval_gate`. That function creates a Restate awakeable, writes `ApprovalRequested`, sets `SessionStatus::WaitingApproval`, waits, writes `ApprovalDecided`, and optionally stores an always-allow rule.
- `crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs` duplicates the same approval gate for child agents and additionally records denied tool results into child-local history.
- `cleanup_pending_approval_after_cancel` exists in both root and sub-agent workflows only because approval waits can be active. Removing blocking review should remove these cleanup paths or reduce them to normal cancellation only.
- `crates/moa-orchestrator/src/objects/session/mod.rs` exposes shared `approve`, `set_pending_approval`, and `clear_pending_approval` methods. With no compatibility requirement, these should be deleted rather than kept as no-op surfaces.
- `crates/moa-orchestrator/src/objects/session/handlers.rs` resolves approval awakeables and can reconstruct a pending awakeable by scanning `ApprovalRequested` / `ApprovalDecided` events. This recovery path should disappear with the old events.
- `crates/moa-orchestrator/src/objects/session/state.rs` stores `pending_approval` and maps `TurnOutcome::WaitingApproval` to `SessionStatus::WaitingApproval`. These should be removed.
- `crates/moa-orchestrator/src/objects/sub_agent/mod.rs`, `handlers.rs`, and `state.rs` mirror the approval methods/state and should be changed in the same implementation task.

## ToolRouter Findings

- `crates/moa-hands/src/core/policy.rs` prepares tool invocations by loading the tool definition, normalizing input, listing approval rules, running `ToolPolicies::check`, building approval prompt fields/diffs, and exposing `PreparedToolInvocation`.
- The same file provides `store_approval_rule`; this should become action policy rule storage or disappear if the new model is workspace-admin configured rather than user-created always-allow rules.
- `crates/moa-hands/src/core/dispatch.rs` has direct execution paths that return permission denied for `PolicyAction::RequireApproval`. These paths should understand the new policy decision enum consistently with workflow execution.
- `crates/moa-hands/src/core/registration.rs` sets bash, MCP tools, and execute tools to `PolicyAction::RequireApproval`, while file writes use `write_tool_policy`, which also defaults to `RequireApproval`. These defaults need to change for auto mode.
- `crates/moa-hands/src/core/normalization.rs` builds approval summaries, patterns, fields, and diffs. The useful normalization/diff pieces can remain but should be renamed for action review instead of approval UI.
- Path note: `ToolRouter` lives under `crates/moa-hands/src/core/`, not `crates/moa-hands/src/router.rs`.

## Tool Executor, Edge, And Startup Findings

- `crates/moa-orchestrator/src/services/tool_executor.rs` executes through `ToolRouter::execute_authorized_with_recovery`, so policy is already expected to be checked before the executor call. This avoids double policy evaluation in the turn workflow.
- `ToolExecutor` still screens active canary tokens before backend execution. That should remain as an execution-boundary guardrail even in auto mode.
- `crates/moa-edge/src/routes.rs` exposes `/v1/approvals` and `/v1/approvals/{id}/decision`, translating them to `/Approvals/list_mine` and `/Approvals/decide`. The plan should replace these with workspace-admin action-review routes and tests.
- `crates/moa-orchestrator/src/main.rs` imports, binds, and expects the `Approvals` service name. Service registration and expected-service checks must change if the service is renamed.
- `start_approval_reaper_if_configured` starts the builtin async-authz approval reaper only for `AsyncAuthzKind::Builtin`; this can remain if builtin async-authz stays separate from tool/action review.

## Test Findings

- `crates/moa-orchestrator/tests/integration/approval_flow_e2e.rs` currently pins allow-once awakeable resolution and cancellation while waiting for approval. This should be replaced by auto-execute and async admin-review tests.
- `crates/moa-orchestrator/tests/behavior_lab_simulation_e2e.rs` has a transaction-dispute scenario that stops at `waiting_approval`, counts `ApprovalRequested`, and asserts no successful bash result. The scenario fixture includes `approval_behavior: stop_on_approval_wait`; this must be changed to the new action-review behavior.
- `crates/moa-orchestrator/tests/experiment_service.rs` has source-string tests that assert experiment runs do not auto-approve tools and that `ExperimentTrialStatus::WaitingApproval` persists. These tests need to be rewritten for auto mode and admin review.
- `crates/moa-orchestrator/tests/tool_executor.rs` asserts write tool descriptors require approval. The descriptor shape should change or the assertion should move to action class/admin-review policy metadata.
- `crates/moa-hands/tests/local_tools_db.rs` has approval prompt tests for remembered workspace root and surgical diffs. These should become action-review preview/envelope tests and keep the useful diff assertions.
- `crates/moa-security/tests/shell_chaining_does_not_match_simple_pattern.rs` tests shell approval matching. The same safety rule is still useful for action policy matching, but names/messages should be changed.
- `crates/moa-session/src/blob.rs` tests claim-checking approval diffs inside `ApprovalRequested`. This should be ported to the new action-review requested event if file diff previews remain.
- Privacy approval-token tests and services should not be touched unless the implementation explicitly broadens from tool/action review to privacy workflows.

## Verification And Migration Findings

- Current Postgres migration files are `V000001__session_baseline.sql`, `V000101__auth_baseline.sql`, `V000201__orchestrator_baseline.sql`, and `V000301__ocsf_baseline.sql`. The plan should add a new forward migration such as `V000302__action_policy_auto_mode.sql`; it should not edit existing applied migrations.
- The primary crates for focused verification are `moa-core`, `moa-security`, `moa-hands`, `moa-session`, `moa-edge`, and `moa-orchestrator`.
- Relevant integration entrypoints include `crates/moa-orchestrator/tests/integration_service_e2e.rs`, which currently includes `integration/approval_flow_e2e.rs`.
- `waiting_approval` is present in DB check constraints for `moa.artifact_run`, `moa.artifact_node_run`, `moa.experiment_run`, and `moa.experiment_trial`. The forward migration must update those constraints if Rust statuses remove or rename the value.

## Additional WaitingApproval Surfaces

- `crates/moa-core/src/wire.rs` exposes `ToolDescriptor.requires_approval`; the plan should replace it with action metadata such as side-effect class and review policy.
- `crates/moa-core/src/runtime_metrics.rs` records approval-wait histograms and experiment approval-wait counters. These should be removed or renamed to admin-review metrics.
- `crates/moa-core/src/analytics.rs` parses persisted `waiting_approval` session status. If `SessionStatus::WaitingApproval` is removed, analytics parsing must be updated with the migration/compat stance.
- `crates/moa-experiments/src/model.rs` includes `ExperimentRunStatus::WaitingApproval`, `ExperimentTrialStatus::WaitingApproval`, and `ExperimentTrialStopReason::ApprovalWait`; experiments need a new pending-review concept only if admin-reviewed actions should pause a trial row.
- `crates/moa-artifacts/src/registry.rs` includes `ArtifactRunStatus::WaitingApproval` and `ArtifactNodeRunStatus::WaitingApproval`. These are workflow-artifact gates and should be renamed to `PendingReview` if retained.

## Authz Findings

- `Relation::Admin` already exists in `crates/moa-auth/authz-schema/src/tuple.rs`.
- Existing workspace admin checks use `require_authz_with_delegation` with `ObjectType::Workspace` and `Relation::Admin`, for example in `LineageAdmin` and `AdminMaintenance`. The action-review service should use the same pattern.

## Skill And Workflow Origin Findings

- The context pipeline selects skills and stores selected skill names/files in working-context metadata, but `ToolInvocation` does not currently carry a skill id or workflow step id.
- `ActionEnvelope` should include optional `origin_kind`, `origin_id`, and `origin_step_id` fields now so workflow/skill capability checks have a stable place to land. The first implementation can populate the turn/workflow id and leave strict skill-step matching for persisted action-policy rules or a follow-up manifest validator.

## Plan Decisions

- The executable plan uses direct SQL in `ActionReviews` for `workspace_action_reviews`; `moa-session` remains responsible for session events and action policy rule CRUD.
- The executable plan renames the mixed `Approvals` service into two domains: `ActionReviews` for tool/action admin review and `AuthzChallenges` for builtin async-authz challenge rows.
- Admin-review pending tool results use the original tool-call id for LLM protocol continuity. Later admin-cleared execution uses a fresh tool-call id linked by the review event.

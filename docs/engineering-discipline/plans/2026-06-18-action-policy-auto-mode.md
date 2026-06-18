# Action Policy Auto Mode Implementation Plan

> **Worker note:** Execute this plan task-by-task using the run-plan skill or subagents. Each step uses checkbox (`- [ ]`) syntax for progress tracking.

**Goal:** Replace blocking end-user tool approvals with action-policy auto mode: execute valid tool steps by default, deny policy violations, and route exceptional workspace-admin review without blocking the conversation.

**Architecture:** Keep the change inside the existing modular monolith. `moa-security` owns action-policy evaluation, `moa-hands::ToolRouter` owns action-envelope construction and tool normalization, `TurnExecution` / `SubAgentTurnExecution` apply decisions before calling `ToolExecutor`, and a new `ActionReviews` service owns workspace-admin review queueing and decisions. Remove approval compatibility shims and delete the `WaitingApproval` session/sub-agent lifecycle path for normal tool execution.

**Tech Stack:** Rust, Restate services/workflows, Postgres/sqlx migrations through `crates/moa-migrations`, OpenFGA authz through `moa-authz`, MOA core event log and tool router.

**Work Scope:**
- **In scope:** Tool/action policy model, action envelope, action review events, workspace-admin review service/routes, policy rule persistence, turn/sub-agent nonblocking admin-review behavior, public tool descriptor replacement, tests/docs/migrations for the new model.
- **Out of scope:** Privacy signed approval tokens, Auth0 CIBA naming, and the low-level `AsyncAuthzProvider` trait. Those are authentication challenge flows, not tool execution approvals. Local import/comment cleanup is allowed when the compiler points at code touched by this plan.

**Verification Strategy:**
- **Level:** integration plus focused crate tests and build/lint
- **Command:**

```bash
cargo fmt --all
cargo test -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts --locked
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo clippy -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts -p moa-orchestrator --all-targets --locked -- -D warnings
cargo build --workspace --locked
make e2e-clean
git diff --check
```

- **What it validates:** The renamed core model compiles across shared crates, policy defaults execute by default, denied/admin-reviewed actions are represented without `WaitingApproval`, public routes translate to the new review service, and the deterministic clean E2E lane exercises the Restate/Postgres/OpenFGA orchestration path.

---

## Run-Plan Execution Boundary

This plan has one executable implementation task for the `run-plan` worker/validator loop. The detailed sections named Task 1 through Task 10 below are an ordered implementation checklist inside that one task, not separate validator boundaries. Task 11 is the final end-to-end verification gate for the same executable task.

Reason: the core type replacement is intentionally breaking. Running the full workspace regression gate after only the core-only slice leaves downstream crates uncompilable until the security, session-store, hands, orchestrator, edge, experiment, artifact, test, and docs slices are migrated. The full workspace and E2E verification belongs at the end-to-end boundary.

### Executable Task A: Implement Action Policy Auto Mode End-To-End

**Dependencies:** None

**Files:** All files listed in Detailed Tasks 1 through 10 below, plus compiler-directed call sites for the renamed core/security/session/hands/orchestrator APIs.

**Acceptance Criteria:**
- [ ] The core model exports action-policy/review types and no longer exports tool approval types.
- [ ] `SessionStatus`, `TurnOutcome`, and `SubAgentState` no longer expose `WaitingApproval`.
- [ ] Tool execution policy defaults to auto-mode `Allow`; policy rules and config can return `Allow`, `Deny`, or `AdminReview`.
- [ ] `ToolRouter` prepares an `ActionEnvelope` and `ActionReviewPreview` for policy-checked invocations.
- [ ] Admin-review actions create workspace action review rows/events, return a pending-review tool result, and do not block root or sub-agent workflows.
- [ ] Workspace admins can list/decide action reviews; cleared reviews execute the stored request with a fresh tool-call id, while denied reviews do not execute.
- [ ] Builtin async-authz challenges remain separate from tool/action reviews.
- [ ] Experiments and artifacts no longer use `waiting_approval` / `approval_wait` as experiment terminal state; artifact workflow pending state uses `pending_review`.
- [ ] E2E coverage proves auto-mode execution, pending admin review without blocking, admin clear execution, and non-admin denial.
- [ ] Docs describe auto mode and workspace-admin action review, with stale blocking end-user tool approval flow removed.
- [ ] Global stale-name searches leave only privacy `approval_token`, Auth0 CIBA protocol wording, and builtin async-authz `builtin_pending_approvals` references.

**Test Commands:**

```bash
cargo fmt --all
cargo test -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts --locked
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo test -p moa-orchestrator --test integration_service_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo test -p moa-orchestrator --test behavior_lab_simulation_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo clippy -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts -p moa-orchestrator --all-targets --locked -- -D warnings
cargo build --workspace --locked
make e2e-clean
git diff --check
rg -n --glob '!crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql' --glob '!crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql' --glob '!docs/engineering-discipline/plans/**' "ApprovalRequested|ApprovalDecided|ApprovalPrompt|ApprovalRequest|ApprovalRule|ApprovalDecision|PolicyAction|RequireApproval|WaitingApproval|waiting_approval|approval_wait|requires_approval|/v1/approvals|Approvals" crates docs scripts .env.example
```

Expected: every command exits 0, except the final `rg` may return only allowed privacy `approval_token`, Auth0 CIBA protocol wording, or builtin async-authz provider references that are not connected to tool execution. The final stale-name search excludes historical applied migration text, the forward cleanup migration that must mention old values to remove them, and this plan document.

### Executable Task B: Clean Up Residual Tool-Approval Language

**Dependencies:** Executable Task A

**Files:**
- Modify: `docs/06-hands-and-mcp.md`
- Modify: `crates/moa-hands/src/core/normalization.rs`
- Modify: `crates/moa-core/src/shell.rs`
- Modify: `crates/moa-hands/src/core/dispatch.rs`
- Modify: `crates/moa-hands/src/tools/str_replace.rs`
- Modify: `crates/moa-orchestrator/src/turn/util.rs`
- Modify compiler-directed call sites for renamed helpers only.

**Acceptance Criteria:**
- [ ] `docs/06-hands-and-mcp.md` describes the action envelope, action policy decision order, and workspace-admin action review behavior.
- [ ] `docs/06-hands-and-mcp.md` no longer describes tool routing as applying approval rules, approval defaults, or parsed command approval matching.
- [ ] Tool/action code no longer uses residual names or comments such as `single_approval_field`, `has_approval_unsafe_shell_syntax`, approval previews, or execution after approval.
- [ ] The broader action-policy implementation still passes the focused compile/test gates affected by these names.

**Test Commands:**

```bash
cargo fmt --all
cargo test -p moa-core -p moa-hands -p moa-orchestrator --test tool_executor --locked
cargo clippy -p moa-core -p moa-hands -p moa-orchestrator --all-targets --locked -- -D warnings
git diff --check
rg -n --glob '!crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql' --glob '!crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql' --glob '!docs/engineering-discipline/plans/**' "approval rules|approval default|approval matching|approval previews|after approval|single_approval_field|has_approval_unsafe_shell_syntax" docs/06-hands-and-mcp.md crates/moa-hands/src crates/moa-core/src/shell.rs crates/moa-orchestrator/src/turn
```

Expected: every command exits 0, and the final `rg` returns no residual tool/action approval-language hits.

### Executable Task C: Clean Up Session Event-Log Approval Rule Wording

**Dependencies:** Executable Task B

**Files:**
- Modify: `docs/05-session-event-log.md`

**Acceptance Criteria:**
- [ ] `docs/05-session-event-log.md` lists action policy rules, not approval rules, in the Postgres session storage overview.
- [ ] The final broad stale-language scan leaves only allowed async-authz/Auth0 CIBA approval-domain references.

**Test Commands:**

```bash
cargo fmt --all
git diff --check
rg -n --glob '!crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql' --glob '!crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql' --glob '!docs/engineering-discipline/plans/**' "ApprovalRequested|ApprovalDecided|ApprovalPrompt|ApprovalRequest|ApprovalRule|ApprovalDecision|PolicyAction|RequireApproval|WaitingApproval|waiting_approval|approval_wait|requires_approval|/v1/approvals|Approvals|approval rules|approval default|approval matching|approval previews|after approval|single_approval_field|has_approval_unsafe_shell_syntax" crates docs scripts .env.example
```

Expected: formatting/diff checks pass. The final `rg` may return only allowed async-authz/Auth0 CIBA approval-domain references, not tool/action-review stale language.

---

## Target Design

Use these names consistently. Do not keep approval aliases.

Core types:

- `ActionClass`: `Read`, `LocalWrite`, `CommandExecution`, `ExternalWrite`, `DataExport`, `Destructive`, `PermissionChange`, `Deployment`, `MoneyMovement`.
- `ActionPolicyEffect`: `Allow`, `Deny`, `AdminReview`.
- `ActionPolicyDecision`: effect plus optional reason and optional matched rule id.
- `ActionPolicyRule`: workspace/global rule matched by tool and normalized input pattern.
- `ActionEnvelope`: durable policy-facing description of one tool invocation.
- `ActionReviewPreview`: human-readable fields and file diffs for workspace admins.
- `ActionReviewDecision`: `Cleared` or `Denied { reason }`.
- `ActionReviewStatus`: `Pending`, `Cleared`, `Denied`, `Expired`.

Runtime behavior:

- `Allow`: execute through `ToolExecutor`.
- `Deny`: append `ToolError` or an error `ToolResult`, then continue the turn.
- `AdminReview`: persist a workspace action review, append `ActionReviewRequested`, append an error `ToolResult` that says the action is pending workspace-admin review, then continue the turn. Do not set session or sub-agent state to waiting.
- Admin `Cleared`: `ActionReviews/decide` marks the row cleared, creates a fresh execution `ToolCallId`, and calls `ToolExecutor/execute` with the stored tool request rewritten to use that fresh id and `provider_tool_use_id: None`. The original tool call keeps its pending-review tool result for LLM protocol continuity. The stored request must have `active_canary: None`; canary leakage is screened before the review is stored.
- Admin `Denied`: `ActionReviews/decide` marks the row denied and appends `ActionReviewDecided`. It must not execute the tool.

Policy behavior:

- Default effect is `Allow`.
- Workspace/global rules can return `Allow`, `Deny`, or `AdminReview`.
- Config can always deny or require admin review by tool name.
- No user-facing autonomy tiers. `ActionClass` is internal policy/audit metadata.

---

### Task 1: Replace Core Approval Types With Action Policy Types

**Dependencies:** None

**Files:**
- Create: `crates/moa-core/src/types/action_policy.rs`
- Delete: `crates/moa-core/src/types/approval.rs`
- Delete: `crates/moa-core/src/events/tool_approval.rs`
- Modify: `crates/moa-core/src/types/mod.rs`
- Modify: `crates/moa-core/src/types/session.rs`
- Modify: `crates/moa-core/src/types/sub_agent.rs`
- Modify: `crates/moa-core/src/types/tools.rs`
- Modify: `crates/moa-core/src/types/runtime_events.rs`
- Modify: `crates/moa-core/src/types/events_stream.rs`
- Modify: `crates/moa-core/src/events.rs`
- Modify: `crates/moa-core/src/session_engine.rs`
- Modify: `crates/moa-core/src/wire.rs`
- Modify: `crates/moa-core/src/runtime_metrics.rs`
- Modify: `crates/moa-core/src/analytics.rs`

**Acceptance Criteria:**
- [ ] `moa-core` exports action-policy/review types and no longer exports tool approval types.
- [ ] `SessionStatus`, `TurnOutcome`, and `SubAgentState` no longer have `WaitingApproval`.
- [ ] Session events use `ActionReviewRequested` and `ActionReviewDecided`, with no `awakeable_id`.
- [ ] `ToolDescriptor` no longer has `requires_approval`; it exposes `action_class` and `risk_level`.
- [ ] `session_requires_processing` no longer treats pending reviews as work that blocks or resumes a turn.

- [ ] **Step 1: Create `action_policy.rs` with the new shared model.**

Define the types listed in the Target Design. Reuse the existing `RiskLevel` enum by moving it into this file with updated comments. `ActionEnvelope` must include:

```text
review_id: uuid::Uuid
workspace_id: WorkspaceId
user_id: UserId
session_id: Option<SessionId>
sub_agent_id: Option<SubAgentId>
tool_call_id: ToolCallId
tool_name: String
normalized_input: String
input_summary: String
risk_level: RiskLevel
action_class: ActionClass
origin_kind: Option<String>
origin_id: Option<String>
origin_step_id: Option<String>
idempotency_key: Option<String>
created_at: DateTime<Utc>
```

`ActionReviewPreview` must contain `fields: Vec<ActionReviewField>` and `file_diffs: Vec<ActionReviewFileDiff>`, using the old approval field/diff shapes under new names.

- [ ] **Step 2: Update `types/mod.rs` exports and remove approval exports.**

Run:

```bash
rg -n "ApprovalDecision|ApprovalPrompt|ApprovalRequest|ApprovalRule|PolicyAction|PolicyScope" crates/moa-core/src
```

Expected: remaining hits are only in files currently being edited. Replace them with the new action names.

- [ ] **Step 3: Remove waiting approval from session and sub-agent lifecycle.**

In `types/session.rs`, remove `SessionStatus::WaitingApproval`, `TurnOutcome::WaitingApproval`, and `SessionSignal::ApprovalDecided`.

In `types/sub_agent.rs`, remove `SubAgentState::WaitingApproval` and delete `SetSubAgentPendingApprovalInput` / `ClearSubAgentPendingApprovalInput`.

In `wire.rs`, delete `SetSessionPendingApprovalInput` and `ClearSessionPendingApprovalInput`.

- [ ] **Step 4: Replace event schema.**

In `events.rs`, replace `ApprovalRequested` / `ApprovalDecided` with:

```text
ActionReviewRequested { review_id, envelope, preview }
ActionReviewDecided { review_id, decision, decided_by, decided_at }
```

Update `Event::event_type`, `Event::type_name`, event round-trip tests, and `events_stream.rs`.

- [ ] **Step 5: Delete approval replay helpers.**

Remove `events/tool_approval.rs` and its `pub use`. In `session_engine.rs`, remove calls to `find_pending_tool_approval` and `find_resolved_pending_tool_approval`.

- [ ] **Step 6: Replace public tool descriptor fields.**

In `types/tools.rs`, replace `ToolPolicySpec.default_action` with:

```text
default_effect: ActionPolicyEffect
action_class: ActionClass
```

Update `read_tool_policy`, `write_tool_policy`, and `ToolDefinition` helpers. Remove `ToolDefinition::requires_approval`.

In `wire.rs`, replace `ToolDescriptor.requires_approval` with `risk_level: RiskLevel` and `action_class: ActionClass`.

- [ ] **Step 7: Rename metrics and analytics status parsing.**

Remove `record_approval_wait` and `record_experiment_approval_wait`. Add `record_action_review_requested(effect, action_class)` and `record_action_review_decision(status, action_class)`, and call them from the `ActionReviews` service.

Remove `"waiting_approval"` parsing from `analytics.rs` because the session status is no longer valid.

- [ ] **Step 8: Verify core type cleanup.**

Run:

```bash
rg -n "ApprovalRequested|ApprovalDecided|WaitingApproval|RequireApproval|ApprovalPrompt|ApprovalRule|requires_approval" crates/moa-core/src
cargo test -p moa-core --locked
```

Expected: `rg` returns no hits except privacy `approval_token` wire fields, and `cargo test -p moa-core --locked` passes.

---

### Task 2: Add Action Policy And Review Storage

**Dependencies:** Task 1

**Files:**
- Create: `crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql`
- Create: `crates/moa-session/src/store/action_policy.rs`
- Modify: `crates/moa-session/src/store/mod.rs`
- Modify: `crates/moa-session/src/queries/rows.rs`
- Modify: `crates/moa-session/src/queries/enums.rs`
- Modify: `crates/moa-session/tests/postgres_store_db.rs`
- Modify: `crates/moa-session/tests/shared/mod.rs`
- Modify: `crates/moa-test-support/src/postgres/contracts/mod.rs`
- Rename or replace: `crates/moa-test-support/src/postgres/contracts/approval.rs`

**Acceptance Criteria:**
- [ ] New policy rules persist under `action_policy_rules`, not `approval_rules`.
- [ ] Workspace action reviews persist under `workspace_action_reviews`.
- [ ] Old applied migration files remain unchanged.
- [ ] Store contract tests cover action policy rule CRUD.

- [ ] **Step 1: Add the forward migration.**

Create `V000302__action_policy_auto_mode.sql` with:

```sql
DROP TABLE IF EXISTS approval_rules;

CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('global', 'workspace')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(workspace_id, tool, pattern)
);

CREATE INDEX IF NOT EXISTS idx_action_policy_rules_scope
    ON action_policy_rules(workspace_id, scope, user_id);

CREATE TABLE IF NOT EXISTS workspace_action_reviews (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    session_id UUID,
    sub_agent_id TEXT,
    tool_call_id UUID NOT NULL,
    tool_name TEXT NOT NULL,
    action_class TEXT NOT NULL,
    risk_level TEXT NOT NULL,
    input_summary TEXT NOT NULL,
    normalized_input TEXT NOT NULL,
    envelope JSONB NOT NULL,
    preview JSONB NOT NULL,
    tool_request JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'cleared', 'denied', 'expired')),
    requested_by TEXT NOT NULL,
    decided_by TEXT,
    deny_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    decided_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_pending
    ON workspace_action_reviews(workspace_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_session
    ON workspace_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
```

Also update status constraints:

```sql
UPDATE moa.artifact_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.artifact_node_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.experiment_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.experiment_trial
SET status = 'running', stop_reason = NULL
WHERE status = 'waiting_approval' OR stop_reason = 'approval_wait';
```

Drop and recreate the affected check constraints without `waiting_approval` / `approval_wait`. Keep exact existing constraint names from `V000001__session_baseline.sql`.

- [ ] **Step 2: Replace approval rule store code.**

Rename the Rust storage surface to action policy:

```text
ApprovalRuleStore -> ActionPolicyRuleStore
ApprovalRule -> ActionPolicyRule
approval_rule_from_row -> action_policy_rule_from_row
list_approval_rules -> list_action_policy_rules
upsert_approval_rule -> upsert_action_policy_rule
delete_approval_rule -> delete_action_policy_rule
```

Do not keep deprecated methods.

- [ ] **Step 3: Keep action-review row SQL in the orchestrator service.**

Do not add review-row helpers to `moa-session` in this pass. `ActionReviews` will use direct SQL against `OrchestratorCtx::graph_pool` because review rows are product workflow state owned by the orchestrator service, while `moa-session` remains the session event/rule store.

- [ ] **Step 4: Update store contract tests.**

Rename the test support contract to `action_policy.rs`. The test must insert an `ActionPolicyRule` with `effect: ActionPolicyEffect::AdminReview`, list it, then delete it.

- [ ] **Step 5: Verify migration/store cleanup.**

Run:

```bash
rg -n "ApprovalRuleStore|ApprovalRule|upsert_approval_rule|list_approval_rules|delete_approval_rule" crates/moa-session crates/moa-test-support
rg -n "approval_rules" crates/moa-session crates/moa-test-support
rg -n "approval_rules" crates/moa-migrations/migrations/postgres --glob '!V000001__session_baseline.sql' --glob '!V000302__action_policy_auto_mode.sql'
cargo test -p moa-session --locked
```

Expected: `rg` returns no hits except `builtin_pending_approvals` and comments that clearly refer to async authz, and the session tests pass. Historical baseline migration declarations and the forward migration's old-table cleanup statement are intentionally excluded from this stale-name check.

---

### Task 3: Replace The Security Policy Engine With Auto Mode Decisions

**Dependencies:** Tasks 1 and 2

**Files:**
- Modify: `crates/moa-security/src/policies.rs`
- Modify: `crates/moa-security/src/lib.rs`
- Modify: `crates/moa-core/src/config/security.rs`
- Modify: `crates/moa-core/src/config/env_overlay.rs`
- Rename or replace: `crates/moa-security/tests/shell_chaining_does_not_match_simple_pattern.rs`

**Acceptance Criteria:**
- [ ] Policy default is allow.
- [ ] Persisted rules can allow, deny, or route to admin review.
- [ ] Config supports `always_deny` and `admin_review` tool lists.
- [ ] No `auto_approve` or `RequireApproval` concept remains in policy code.

- [ ] **Step 1: Rename policy engine types.**

Use:

```text
ToolPolicies -> ActionPolicies
PolicyCheck -> ActionPolicyCheck
ToolPolicyContext -> ActionPolicyContext
ApprovalRuleStore -> ActionPolicyRuleStore
parse_and_match_bash -> parse_and_match_command
```

Do not keep re-export aliases.

- [ ] **Step 2: Implement decision ordering.**

In `ActionPolicies::check`, use this exact order:

1. Matching action policy rule.
2. Config `always_deny` tool-name match.
3. Config `admin_review` tool-name match.
4. Tool default effect.
5. Fallback `Allow`.

The returned `ActionPolicyCheck` must include `effect`, optional `reason`, and optional matched rule.

- [ ] **Step 3: Replace permission config.**

In `PermissionsConfig`, use:

```text
default_effect: ActionPolicyEffect
admin_review: Vec<String>
always_deny: Vec<String>
```

Default:

```text
default_effect = Allow
admin_review = []
always_deny = []
```

In `env_overlay.rs`, replace `MOA_PERMISSIONS_DEFAULT_POSTURE` and `MOA_PERMISSIONS_AUTO_APPROVE` with:

```text
MOA_PERMISSIONS_DEFAULT_EFFECT
MOA_PERMISSIONS_ADMIN_REVIEW
MOA_PERMISSIONS_ALWAYS_DENY
```

- [ ] **Step 4: Rename shell matching tests.**

Keep the behavior that command-chain and shell-evaluation syntax cannot satisfy a simple persisted allow/admin-review pattern. Update test names and assertion messages from approval matching to action-policy matching.

- [ ] **Step 5: Verify policy behavior.**

Run:

```bash
cargo test -p moa-security --locked
rg -n "auto_approve|RequireApproval|approval matching|ApprovalRuleStore|ToolPolicies" crates/moa-security crates/moa-core/src/config
```

Expected: tests pass and `rg` returns no stale policy names.

---

### Task 4: Convert ToolRouter To Action Envelope Preparation

**Dependencies:** Tasks 1-3

**Files:**
- Modify: `crates/moa-hands/src/core/mod.rs`
- Modify: `crates/moa-hands/src/core/construction.rs`
- Modify: `crates/moa-hands/src/core/policy.rs`
- Modify: `crates/moa-hands/src/core/dispatch.rs`
- Modify: `crates/moa-hands/src/core/registration.rs`
- Modify: `crates/moa-hands/src/core/normalization.rs`
- Modify: `crates/moa-hands/src/core/telemetry.rs`
- Modify: `crates/moa-hands/tests/local_tools_db.rs`
- Modify: `crates/moa-orchestrator/tests/tool_executor.rs`
- Modify: `crates/moa-orchestrator/tests/integration/tool_executor_e2e.rs`

**Acceptance Criteria:**
- [ ] `ToolRouter` prepares an `ActionEnvelope` and `ActionReviewPreview` for every policy-checked invocation.
- [ ] Write, command, execute, and MCP tools default to `Allow`, with high-risk/action-class metadata.
- [ ] Direct router dispatch returns permission denied only for `Deny` or `AdminReview`, never with "requires approval" wording.
- [ ] Tool descriptors expose `action_class` and `risk_level`.

- [ ] **Step 1: Rename `PreparedToolInvocation` to `PreparedActionInvocation`.**

Expose these action-based methods:

```text
policy() -> &ActionPolicyCheck
policy_input() -> &ToolPolicyInput
envelope(review_id, session, tool_id, sub_agent_id, origin) -> ActionEnvelope
review_preview() -> ActionReviewPreview
input_summary() -> &str
```

Rename `approval_prompt` to `review_preview` and `approval_pattern_for` to `action_pattern_for`.

- [ ] **Step 2: Update tool policy specs.**

In `registration.rs`:

- `execute_tool_policy` uses `default_effect: Allow`, `action_class: CommandExecution`, `risk_level: High`.
- MCP tools use `default_effect: Allow`, `action_class: ExternalWrite`, `risk_level: High` for all discovered tools in this pass.
- `write_tool_policy` uses `default_effect: Allow`, `action_class: LocalWrite`, `risk_level: Medium`.
- `read_tool_policy` uses `default_effect: Allow`, `action_class: Read`, `risk_level: Low`.

- [ ] **Step 3: Update router dispatch semantics.**

In `dispatch.rs`, match `ActionPolicyEffect`:

- `Allow`: execute.
- `Deny`: return `MoaError::PermissionDenied("tool <name> denied by action policy: <reason>")`.
- `AdminReview`: return `MoaError::PermissionDenied("tool <name> requires workspace admin review: <summary>")`.

The workflow path will handle admin review before calling `ToolExecutor`; this direct dispatch path is for callers that did not implement review handling.

- [ ] **Step 4: Update tests.**

Rename the local-tools tests:

```text
approval_prompt_uses_remembered_workspace_root_for_commands -> action_review_preview_uses_remembered_workspace_root_for_commands
approval_prompt_str_replace_diff_is_surgical -> action_review_preview_str_replace_diff_is_surgical
```

Update `tool_executor.rs` descriptor assertions to check `action_class` and `risk_level`, not `requires_approval`.

- [ ] **Step 5: Verify hands/router cleanup.**

Run:

```bash
cargo test -p moa-hands --locked
cargo test -p moa-orchestrator --test tool_executor --locked
rg -n "approval_prompt|approval_pattern|requires_approval|RequireApproval" crates/moa-hands crates/moa-orchestrator/tests/tool_executor.rs
```

Expected: tests pass and stale approval-router names are gone.

---

### Task 5: Add Workspace Admin Action Review Service And Routes

**Dependencies:** Tasks 1-4

**Files:**
- Create: `crates/moa-orchestrator/src/services/action_reviews.rs`
- Create: `crates/moa-orchestrator/src/services/authz_challenges.rs`
- Create: `crates/moa-orchestrator/src/services/authz_challenges_reaper.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-edge/src/routes.rs`
- Delete: `crates/moa-orchestrator/src/services/approvals.rs`
- Delete: `crates/moa-orchestrator/src/services/approvals_reaper.rs`

**Acceptance Criteria:**
- [ ] Tool/action review Restate service name is `ActionReviews`.
- [ ] Builtin async-authz challenge Restate service name is `AuthzChallenges`.
- [ ] Public routes are workspace-scoped and require workspace admin for listing/deciding.
- [ ] Admin `cleared` decisions execute the stored tool request through `ToolExecutor`.
- [ ] Admin `denied` decisions do not execute the tool.
- [ ] The service does not resolve Restate awakeables.

- [ ] **Step 1: Implement service DTOs.**

Create:

```text
ActionReviewSummary
RequestActionReview
ListActionReviewsRequest
DecideActionReviewRequest
```

`RequestActionReview` must contain `ActionEnvelope`, `ActionReviewPreview`, and a `ToolCallRequest`.

Before persisting, set `tool_request.active_canary = None`. Canary leakage is already screened before review storage and should not be written to DB.

- [ ] **Step 2: Implement `ActionReviews` service.**

Methods:

```text
request(request: Json<RequestActionReview>) -> Result<Json<ActionReviewSummary>, HandlerError>
list_pending(request: Json<ListActionReviewsRequest>) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError>
decide(request: Json<DecideActionReviewRequest>) -> Result<(), HandlerError>
```

Authz:

- `request`: internal workflow call. Add a one-line `// SAFETY:` comment above the handler explaining that the owning session/workflow already checked participant authz before tool execution.
- `list_pending`: require `Workspace:Admin`.
- `decide`: require `Workspace:Admin`.

Decision behavior:

- Lock row `FOR UPDATE`.
- Reject non-pending rows with 409.
- For `Cleared`, update status to `cleared`, append `ActionReviewDecided`, create a fresh `ToolCallId`, rewrite the stored `ToolCallRequest` to use that fresh id and `provider_tool_use_id: None`, then call `ToolExecutor/execute`.
- For `Denied`, update status to `denied`, append `ActionReviewDecided`, and do not call `ToolExecutor`.

- [ ] **Step 3: Split builtin async-authz challenges from action reviews.**

Move the builtin row listing/decision logic from `approvals.rs` into `authz_challenges.rs`. Do not carry over event-backed tool approval listing or routing to `Session/approve` / `SubAgent/approve`.

Rename `approvals_reaper.rs` to `authz_challenges_reaper.rs` and keep its builtin async-authz timeout behavior. Keep the database table name `builtin_pending_approvals` because it belongs to the async-authz provider and is outside the tool/action review model.

- [ ] **Step 4: Update service registration.**

In `main.rs`, replace imports/binding/expected service names:

```text
Approvals -> ActionReviews and AuthzChallenges
ApprovalsImpl -> ActionReviewsImpl and AuthzChallengesImpl
ApprovalReaper -> AuthzChallengeReaper
```

Expected service names must include `ActionReviews` and `AuthzChallenges`, not `Approvals`.

- [ ] **Step 5: Replace edge routes.**

In `routes.rs`, remove `/v1/approvals` translation. Add:

```text
GET  /v1/workspaces/{workspace_id}/action-reviews
POST /v1/workspaces/{workspace_id}/action-reviews/{review_id}/decision
GET  /v1/authz-challenges
POST /v1/authz-challenges/{challenge_id}/decision
```

Forward to:

```text
/ActionReviews/list_pending
/ActionReviews/decide
/AuthzChallenges/list_mine
/AuthzChallenges/decide
```

Decision body uses:

```json
{"decision":"cleared","reason":null}
{"decision":"denied","reason":"..."}
```

Update route tests accordingly.

- [ ] **Step 6: Verify service/edge routing.**

Run:

```bash
cargo test -p moa-edge --locked
cargo test -p moa-orchestrator --lib --locked action_reviews
rg -n "Approvals|/v1/approvals|approvals_list_mine|approvals_decide|Session/approve|SubAgent/approve" crates/moa-edge/src crates/moa-orchestrator/src/services crates/moa-orchestrator/src/main.rs
```

Expected: tests pass and old mixed approval service names are gone. `builtin_pending_approvals` remains in authz challenge code and migrations only.

---

### Task 6: Make Root And Sub-Agent Tool Execution Nonblocking

**Dependencies:** Tasks 1-5

**Files:**
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/mod.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/handlers.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/state.rs`
- Modify: `crates/moa-orchestrator/src/objects/sub_agent/mod.rs`
- Modify: `crates/moa-orchestrator/src/objects/sub_agent/handlers.rs`
- Modify: `crates/moa-orchestrator/src/objects/sub_agent/state.rs`
- Modify: `crates/moa-orchestrator/src/turn/mod.rs`
- Delete: `crates/moa-orchestrator/src/turn/approval.rs`
- Delete: `crates/moa-orchestrator/src/workflows/approval_wait.rs`

**Acceptance Criteria:**
- [ ] Tool actions execute by default when policy allows.
- [ ] Admin-review actions create review rows/events and return a tool result to the model without blocking the workflow.
- [ ] Root session and sub-agent VOs no longer expose `approve`, `set_pending_approval`, or `clear_pending_approval`.
- [ ] No workflow sets `SessionStatus::WaitingApproval` or `SubAgentState::WaitingApproval`.

- [ ] **Step 1: Replace root `handle_approval_gate`.**

Delete `PendingApprovalState`, `ApprovalOutcome`, `handle_approval_gate`, and `cleanup_pending_approval_after_cancel`.

In `handle_tool_call`, after policy evaluation:

- `Allow`: call `ToolExecutor/execute` as today.
- `Deny`: append `ToolError` and return.
- `AdminReview`: build `ToolCallRequest`, call `ActionReviews/request`, append an error `ToolResult` with text:

```text
Action is pending workspace admin review: <tool_name>: <input_summary>
```

Then return `Ok(())` so the turn loop continues.

- [ ] **Step 2: Replace sub-agent approval gate.**

Mirror the root behavior in `sub_agent_turn_execution.rs`.

For admin review, append the parent-session `ActionReviewRequested` event, append the non-executed tool result, and record the child-local tool output through the renamed denied/non-executed tool helper so the sub-agent history remains coherent.

- [ ] **Step 3: Delete VO approval methods and state.**

Remove from session VO:

```text
approve
set_pending_approval
clear_pending_approval
K_PENDING_APPROVAL
pending_approval
pending_approval_awakeable
approval_event_range
```

Remove equivalent sub-agent methods/state.

- [ ] **Step 4: Remove waiting outcome branches.**

Remove `CoreTurnOutcome::WaitingApproval` handling from root and sub-agent workflow result mapping.

Update tests in `objects/session/state.rs` and `objects/sub_agent/state.rs` that construct waiting-approval state.

- [ ] **Step 5: Verify no blocking approval path remains.**

Run:

```bash
rg -n "K_PENDING_APPROVAL|pending_approval|set_pending_approval|clear_pending_approval|approve\\(|ApprovalOutcome|handle_approval_gate|approval_wait|WaitingApproval" crates/moa-orchestrator/src crates/moa-core/src
cargo test -p moa-orchestrator --test session_vo --locked
cargo test -p moa-orchestrator --test sub_agent_delegation --locked
```

Expected: `rg` has no tool-approval/waiting hits, privacy/authz approval-token hits are not part of this check, and tests pass.

---

### Task 7: Update Experiments, Artifacts, Metrics, And Analytics For No Waiting Approval

**Dependencies:** Tasks 1, 2, and 6

**Files:**
- Modify: `crates/moa-experiments/src/model.rs`
- Modify: `crates/moa-experiments/src/store.rs`
- Modify: `crates/moa-experiments/tests/model.rs`
- Modify: `crates/moa-artifacts/src/registry.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run/status.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_run/plan_expansion.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run/status.rs`
- Modify: `crates/moa-orchestrator/src/workflows/experiment_trial_run/target_execution.rs`
- Modify: `crates/moa-orchestrator/tests/experiment_service.rs`
- Modify: `crates/moa-orchestrator/tests/experiment_agent_loop_e2e.rs`

**Acceptance Criteria:**
- [ ] Experiment and artifact Rust models no longer expose `WaitingApproval`.
- [ ] `approval_wait` stop reason is removed.
- [ ] Trials do not become terminal because a session has a pending admin review.
- [ ] Source-string tests no longer assert that experiments avoid auto-approval.

- [ ] **Step 1: Remove experiment waiting states.**

Delete:

```text
ExperimentRunStatus::WaitingApproval
ExperimentTrialStatus::WaitingApproval
ExperimentTrialStopReason::ApprovalWait
```

Update `from_db`, `as_str`, and tests.

- [ ] **Step 2: Remove artifact waiting states or rename to pending review.**

Rename artifact workflow pending states:

```text
ArtifactRunStatus::WaitingApproval -> PendingReview
ArtifactNodeRunStatus::WaitingApproval -> PendingReview
```

Use database value `pending_review`. Update `run_status_from_str`, `as_str`, and check constraints. Experiments must not expose pending review as a terminal or stop status; only artifact workflow state keeps `PendingReview`.

- [ ] **Step 3: Update experiment status mappings.**

In target execution:

- `SessionStatus::Paused` remains non-terminal. The simulator stop condition owns trial completion.
- `SessionStatus::Completed` maps to target terminal.
- Pending admin review does not map to a session status.

In plan expansion, remove waiting-approval slot occupancy. Dispatch slots are occupied by `Dispatched` and `Running` only.

- [ ] **Step 4: Update tests and behavior expectations.**

In `experiment_service.rs`, replace "must not auto-approve" assertions with checks that experiment workflows use normal `Session/queue_message` and action policy.

In `experiment_agent_loop_e2e.rs`, remove accepted `"waiting_approval"` terminal statuses.

- [ ] **Step 5: Verify experiment/artifact cleanup.**

Run:

```bash
cargo test -p moa-experiments --locked
cargo test -p moa-artifacts --locked
cargo test -p moa-orchestrator --test experiment_service --locked
rg -n "WaitingApproval|waiting_approval|ApprovalWait|approval_wait" crates/moa-experiments crates/moa-artifacts crates/moa-orchestrator/src/workflows/experiment_* crates/moa-orchestrator/tests/experiment_*
```

Expected: tests pass and `rg` returns no experiment/artifact waiting-approval hits.

---

### Task 8: Replace Approval E2E With Auto Mode And Admin Review E2E

**Dependencies:** Tasks 1-7

**Files:**
- Rename or replace: `crates/moa-orchestrator/tests/integration/approval_flow_e2e.rs`
- Modify: `crates/moa-orchestrator/tests/integration_service_e2e.rs`
- Modify: `crates/moa-orchestrator/tests/behavior_lab_simulation_e2e.rs`
- Modify: scripted fixtures inside `behavior_lab_simulation_e2e.rs`

**Acceptance Criteria:**
- [ ] E2E proves a formerly approval-gated bash action executes by default.
- [ ] E2E proves an admin-review policy creates a pending review without blocking session progress.
- [ ] E2E proves workspace-admin clear executes the stored tool request.
- [ ] E2E proves non-admin callers cannot list/decide workspace action reviews.

- [ ] **Step 1: Replace approval flow integration tests.**

Rename the module to `action_policy_flow_e2e.rs` and update the include in `integration_service_e2e.rs`.

Implement ignored tests:

```text
action_policy_auto_mode_executes_shell_without_user_approval
admin_review_policy_records_pending_review_and_turn_continues
workspace_admin_clear_executes_stored_review_action
workspace_member_cannot_decide_action_review
```

- [ ] **Step 2: Update scripted provider fixtures.**

For auto mode, keep a scripted tool call to `bash` and assert successful `ToolResult` without `ActionReviewRequested`.

For admin review, configure `MOA_PERMISSIONS_ADMIN_REVIEW=bash` for the spawned orchestrator. Assert:

```text
Event::ActionReviewRequested exists
no SessionStatus::WaitingApproval exists
final session status becomes Paused or Completed
the first tool result tells the model the action is pending workspace admin review
```

- [ ] **Step 3: Update behavior-lab transaction dispute scenario.**

Replace fixture text:

```text
approval_behavior: stop_on_approval_wait
```

with:

```text
action_review_behavior: continue_after_pending_review
```

Update success/failure criteria so the target either executes the valid action under auto mode or records a pending workspace-admin review under admin-review policy. Do not assert `waiting_approval`.

- [ ] **Step 4: Verify E2E test names compile.**

Run:

```bash
cargo test -p moa-orchestrator --test integration_service_e2e --features provider-overrides,integration,skill-learning --locked --no-run
cargo test -p moa-orchestrator --test behavior_lab_simulation_e2e --features provider-overrides,integration,skill-learning --locked --no-run
```

Expected: both test binaries compile.

---

### Task 9: Update Documentation Source Of Truth

**Dependencies:** Tasks 1-8

**Files:**
- Modify: `docs/01-architecture-overview.md`
- Modify: `docs/02-brain-orchestration.md`
- Modify: `docs/03-communication-layer.md`
- Modify: `docs/05-session-event-log.md`
- Modify: `docs/06-hands-and-mcp.md`
- Modify: `docs/08-security.md`
- Modify: `docs/12-restate-architecture.md`
- Delete or rewrite: `docs/operations/builtin-approvals.md`
- Modify: `docs/eval/live-behavior-experiments.md`

**Acceptance Criteria:**
- [ ] Docs describe auto mode and workspace-admin action review.
- [ ] Docs no longer describe risky tool calls blocking on end-user approval.
- [ ] Event-log docs list `ActionReviewRequested` and `ActionReviewDecided`.
- [ ] Tool-routing docs explain action envelope and policy decision order.

- [ ] **Step 1: Update architecture overview and Restate service list.**

Replace `Approvals` with `ActionReviews` in service lists. Keep async-authz provider language separate.

- [ ] **Step 2: Rewrite brain orchestration approval section.**

Replace the old awakeable flow with:

```text
Tool call
  -> build ActionEnvelope
  -> evaluate ActionPolicies
  -> Allow: execute ToolExecutor
  -> Deny: record ToolError and continue
  -> AdminReview: persist workspace action review, return pending-review tool result, continue
```

- [ ] **Step 3: Rewrite communication-layer review section.**

Describe workspace-admin review cards/routes and explicitly say conversation clients do not resolve end-user approval gates.

- [ ] **Step 4: Update security docs.**

Preserve prompt-injection/canary guidance. State that input guardrails are advisory and action guardrails execute at the tool boundary.

- [ ] **Step 5: Verify docs have no stale tool approval flow.**

Run:

```bash
rg -n --glob '!docs/engineering-discipline/plans/**' "ApprovalRequested|ApprovalDecided|WaitingApproval|waiting_approval|RequireApproval|end-user approval|allow once|always allow|/v1/approvals" docs
```

Expected: no stale tool-approval flow references outside the planning artifact. Privacy/authz approval-token references remain only when clearly scoped to privacy/authentication.

---

### Task 10: Remove Stale Approval Names Across The Workspace

**Dependencies:** Tasks 1-9

**Files:**
- Modify all compile-hit files found by the commands below.
- Do not rename privacy `approval_token`; split any accidental tool/action-review coupling without renaming the token field.
- Do not rename Auth0 CIBA approval protocol terms; split any accidental tool/action-review coupling without renaming protocol terms.

**Acceptance Criteria:**
- [ ] No stale tool approval symbols remain.
- [ ] No `WaitingApproval` status remains outside a deliberate `PendingReview` replacement.
- [ ] No public route or service named `Approvals` remains for tool/action execution.

- [ ] **Step 1: Run global stale-name searches.**

Run:

```bash
rg -n --glob '!crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql' --glob '!crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql' --glob '!docs/engineering-discipline/plans/**' "ApprovalRequested|ApprovalDecided|ApprovalPrompt|ApprovalRequest|ApprovalRule|ApprovalDecision|PolicyAction|RequireApproval|WaitingApproval|waiting_approval|approval_wait|requires_approval|/v1/approvals|Approvals" crates docs scripts .env.example
```

- [ ] **Step 2: Classify each hit before editing.**

Allowed remaining domains:

```text
approval_token in privacy/DSAR flows
Auth0 CIBA protocol approval wording
builtin_pending_approvals in authz challenge, authz provider, and migration code
```

Everything else must be renamed, removed, or rewritten.

- [ ] **Step 3: Run formatting.**

Run:

```bash
cargo fmt --all
```

Expected: formatting completes without changes outside the implementation scope.

---

### Task 11 (Final): End-to-End Verification

**Dependencies:** All preceding tasks

**Files:** None for implementation; read-only verification plus formatting.

- [ ] **Step 1: Run the full verification strategy.**

Run:

```bash
cargo fmt --all
cargo test -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts --locked
cargo test -p moa-orchestrator --test tool_executor --locked
cargo test -p moa-orchestrator --test experiment_service --locked
cargo clippy -p moa-core -p moa-security -p moa-hands -p moa-session -p moa-edge -p moa-experiments -p moa-artifacts -p moa-orchestrator --all-targets --locked -- -D warnings
cargo build --workspace --locked
make e2e-clean
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 2: Verify success criteria manually with searches.**

Run:

```bash
rg -n --glob '!crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql' --glob '!crates/moa-migrations/migrations/postgres/V000302__action_policy_auto_mode.sql' --glob '!docs/engineering-discipline/plans/**' "ApprovalRequested|ApprovalDecided|ApprovalPrompt|ApprovalRequest|ApprovalRule|ApprovalDecision|PolicyAction|RequireApproval|WaitingApproval|waiting_approval|approval_wait|requires_approval|/v1/approvals|Approvals" crates docs scripts .env.example
```

Expected: no stale tool/action approval hits outside historical migration text, the forward cleanup migration, and the planning artifact. Any remaining hits must be privacy approval-token, Auth0 CIBA, or builtin async-authz provider language and must not be connected to tool execution.

- [ ] **Step 3: Verify architecture behavior from events.**

From the new E2E output or local event inspection, confirm:

```text
auto-mode allowed action has ToolCall and successful ToolResult with no ActionReviewRequested
admin-review action has ToolCall, ActionReviewRequested, pending-review ToolResult, and no session WaitingApproval status
admin-cleared review later records ActionReviewDecided plus a fresh ToolCall and ToolResult from ToolExecutor
admin-denied review records ActionReviewDecided and no ToolResult execution
```

Expected: all four behavior checks are true.

---

## Self-Review Checklist

- [ ] Every user requirement maps to a task.
- [ ] No backwards compatibility shims are requested.
- [ ] No task asks workers to edit already-applied migration files.
- [ ] Core, policy, router, workflow, service, edge, test, and docs surfaces are all covered.
- [ ] Parallelism is safe only where tasks do not touch the same files; otherwise dependencies are explicit.
- [ ] Verification includes focused tests, clippy, workspace build, clean E2E, and stale-name searches.

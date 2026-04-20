# R08 — `SubAgent` Virtual Object

## Purpose

Ship the `SubAgent` VO: a conversational sub-agent that mirrors `Session` but is dispatched from a parent session (or another sub-agent) rather than from the gateway. This prompt implements dispatch, the turn loop (reusing most of `Session::run_turn`), fork-bomb prevention, budget inheritance, result delivery via awakeable, and sub-agent approval routing through the parent.

End state: a root session can dispatch a sub-agent with a task and tool subset, the sub-agent runs its own conversational loop, returns results to the parent, and approval requests from the sub-agent reach the user via the parent session's UI.

## Prerequisites

- R01–R07 complete. Root sessions, brain loop, and approvals all working.
- `moa-brain` context pipeline is parameterizable by agent identity (so sub-agents can have different tool sets and system prompts).

## Read before starting

- `docs/12-restate-architecture.md` — "Sub-agent dispatch" section
- `docs/02-brain-orchestration.md` — existing multi-brain orchestration model
- R05 (`Session` VO lifecycle) and R06 (`run_turn` brain loop) — `SubAgent` mirrors both
- R07 — approval pattern to extend

## Steps

### 1. Define SubAgent types

`moa-core/src/types.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubAgentMessage {
    InitialTask {
        task: String,
        tool_subset: Vec<String>,
        budget_tokens: u64,
        parent_session: SessionId,
        parent_sub_agent: Option<SubAgentId>,
        depth: u32,
        result_awakeable_id: String,
    },
    FollowUp {
        text: String,
    },
    ChildResult {
        sub_agent_id: SubAgentId,
        result: SubAgentResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub sub_agent_id: SubAgentId,
    pub success: bool,
    pub output: String,
    pub tokens_used: u64,
    pub tools_invoked: u32,
    pub error: Option<String>,
}

pub type SubAgentId = String;  // "{parent_session}-{uuid}"

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentStatus {
    pub state: SubAgentState,
    pub depth: u32,
    pub tokens_used: u64,
    pub budget_remaining: u64,
    pub active_children: Vec<SubAgentId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentState {
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}
```

### 2. Define the SubAgent VO trait

`moa-orchestrator/src/objects/sub_agent.rs`:

```rust
use restate_sdk::prelude::*;
use moa_core::types::*;

#[restate_sdk::object]
pub trait SubAgent {
    async fn post_message(
        ctx: ObjectContext<'_>,
        msg: SubAgentMessage,
    ) -> Result<(), HandlerError>;

    #[shared]
    async fn status(
        ctx: SharedObjectContext<'_>,
    ) -> Result<SubAgentStatus, HandlerError>;

    async fn cancel(
        ctx: ObjectContext<'_>,
        reason: String,
    ) -> Result<(), HandlerError>;

    async fn approve(
        ctx: ObjectContext<'_>,
        decision: ApprovalDecision,
    ) -> Result<(), HandlerError>;

    async fn run_turn(
        ctx: ObjectContext<'_>,
    ) -> Result<TurnOutcome, HandlerError>;

    async fn destroy(
        ctx: ObjectContext<'_>,
    ) -> Result<(), HandlerError>;
}
```

### 3. State shape

```rust
// All Session VO keys, plus:
const K_PARENT_SESSION: &str = "parent_session";
const K_PARENT_SUB_AGENT: &str = "parent_sub_agent";  // Option
const K_DEPTH: &str = "depth";
const K_BUDGET_REMAINING: &str = "budget_remaining";
const K_TOKENS_USED: &str = "tokens_used";
const K_RESULT_AWAKEABLE_ID: &str = "result_awakeable_id";
const K_TASK: &str = "task";
const K_TOOL_SUBSET: &str = "tool_subset";
```

### 4. Implement `post_message` with InitialTask vs FollowUp dispatch

```rust
async fn post_message(
    ctx: ObjectContext<'_>,
    msg: SubAgentMessage,
) -> Result<(), HandlerError> {
    match msg {
        SubAgentMessage::InitialTask {
            task, tool_subset, budget_tokens,
            parent_session, parent_sub_agent, depth, result_awakeable_id,
        } => {
            // Bootstrap: set all state from the task payload.
            ctx.set(K_TASK, &task);
            ctx.set(K_TOOL_SUBSET, &tool_subset);
            ctx.set(K_BUDGET_REMAINING, budget_tokens);
            ctx.set(K_TOKENS_USED, 0u64);
            ctx.set(K_PARENT_SESSION, parent_session);
            if let Some(parent_sa) = parent_sub_agent {
                ctx.set(K_PARENT_SUB_AGENT, parent_sa);
            }
            ctx.set(K_DEPTH, depth);
            ctx.set(K_RESULT_AWAKEABLE_ID, &result_awakeable_id);
            ctx.set(K_STATUS, SubAgentState::Running);

            // Queue initial pending "user" message (the task).
            ctx.set(K_PENDING, vec![UserMessage {
                text: task,
                attachments: vec![],
                sent_at: chrono::Utc::now(),
                platform_msg_id: None,
            }]);
        }
        SubAgentMessage::FollowUp { text } => {
            // Parent is sending a follow-up question.
            let mut pending = ctx.get::<Vec<UserMessage>>(K_PENDING).await?.unwrap_or_default();
            pending.push(UserMessage {
                text,
                attachments: vec![],
                sent_at: chrono::Utc::now(),
                platform_msg_id: None,
            });
            ctx.set(K_PENDING, pending);
        }
        SubAgentMessage::ChildResult { sub_agent_id, result } => {
            // A grandchild sub-agent reported back; feed as synthetic tool result.
            // This message is sent when the grandchild resolves its awakeable,
            // which this sub-agent was awaiting in its run_turn.
            // Implementation: store in a child_results queue; run_turn picks up.
            let mut results = ctx.get::<Vec<SubAgentResult>>("child_results").await?.unwrap_or_default();
            results.push(result);
            ctx.set("child_results", results);
        }
    }

    // Drive the turn loop until done.
    loop {
        if let Some(_mode) = ctx.get::<CancelMode>(K_CANCEL_FLAG).await? {
            ctx.set(K_STATUS, SubAgentState::Cancelled);
            break;
        }

        let outcome = ctx.object_client::<SubAgentClient>(ctx.key())
            .run_turn()
            .call()
            .await?;

        match outcome {
            TurnOutcome::Continue => continue,
            TurnOutcome::Idle => {
                ctx.set(K_STATUS, SubAgentState::Completed);
                break;
            }
            TurnOutcome::WaitingApproval => {
                ctx.set(K_STATUS, SubAgentState::WaitingApproval);
                break;
            }
            TurnOutcome::Cancelled => {
                ctx.set(K_STATUS, SubAgentState::Cancelled);
                break;
            }
        }
    }

    // If terminal, resolve the parent's awakeable with our result.
    let terminal_state = ctx.get::<SubAgentState>(K_STATUS).await?;
    if matches!(terminal_state, Some(SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled)) {
        let awakeable_id: String = ctx.get(K_RESULT_AWAKEABLE_ID).await?
            .ok_or_else(|| HandlerError::from("result_awakeable_id missing"))?;
        let result = build_sub_agent_result(&ctx).await?;

        ctx.run("resolve_parent_awakeable", || async {
            resolve_awakeable_via_admin(&awakeable_id, &result).await
        })
        .await?;
    }

    Ok(())
}
```

### 5. Implement `run_turn` (delegating to shared brain logic)

Extract the turn-body logic from `Session::run_turn` (R06) into a reusable function that takes a `TurnRunner` trait, then have both VOs call it:

```rust
// moa-orchestrator/src/turn_runner.rs
pub trait TurnRunnerContext {
    fn key(&self) -> &str;
    fn id(&self) -> uuid::Uuid;
    fn meta(&self) -> TurnMeta;
    fn get_pending(&self) -> impl Future<Output = Vec<UserMessage>>;
    // ... etc.
}

pub async fn execute_turn<C: TurnRunnerContext>(ctx: &C, services: &ServiceClients)
    -> Result<TurnOutcome, HandlerError>
{
    // Full turn body. Reused by both Session::run_turn and SubAgent::run_turn.
}
```

Too much refactor at once is risky. Alternative: **duplicate the ~50 lines of turn body in `SubAgent::run_turn`** for R08, then consolidate in a follow-up. Shipping working code beats premature DRY.

Key differences in `SubAgent::run_turn` vs `Session::run_turn`:

- Tools filtered by `K_TOOL_SUBSET` before passing to LLM
- Budget check before each LLM call; reject if `tokens_used >= budget_remaining`
- Depth/fan-out/loop-detection check before dispatching any grandchild SubAgent
- Approval requests include `sub_agent_id` in the event (routed to parent user via gateway)
- System prompt includes "you are a specialist sub-agent" framing

```rust
async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
    let depth: u32 = ctx.get(K_DEPTH).await?.unwrap_or(0);
    let budget: u64 = ctx.get(K_BUDGET_REMAINING).await?.unwrap_or(0);
    let tokens_used: u64 = ctx.get(K_TOKENS_USED).await?.unwrap_or(0);

    // Budget check.
    if tokens_used >= budget {
        tracing::warn!(sub_agent = %ctx.key(), "budget exhausted, terminating");
        return Ok(TurnOutcome::Idle);
    }

    // Fork-bomb prevention: depth check.
    if depth > 3 {
        return Err(HandlerError::from("sub-agent depth exceeds maximum (3)"));
    }

    // ... build context (scoped to tool subset), call LLM, handle tool calls ...
    // (near-verbatim from Session::run_turn with the scoping above)
}
```

### 6. Dispatch helper for parent

Used by `Session::run_turn` and `SubAgent::run_turn` when the LLM calls a `dispatch_sub_agent` tool:

```rust
// moa-orchestrator/src/sub_agent_dispatch.rs
pub async fn dispatch_sub_agent(
    ctx: &ObjectContext<'_>,
    parent_session: SessionId,
    parent_sub_agent: Option<SubAgentId>,
    current_depth: u32,
    task: String,
    tool_subset: Vec<String>,
    budget_tokens: u64,
) -> Result<SubAgentResult, HandlerError> {
    // Fork-bomb checks at dispatch site.
    if current_depth >= 3 {
        return Err(HandlerError::from("sub-agent depth limit reached"));
    }

    // Fan-out check: count active children.
    let children: Vec<SubAgentId> = ctx.get(K_CHILDREN).await?.unwrap_or_default();
    if children.len() >= 4 {
        return Err(HandlerError::from("sub-agent fan-out limit reached (4)"));
    }

    // Loop detection: hash (task, tool_subset) against active children's tasks.
    let task_hash = hash_task(&task, &tool_subset);
    for existing in &children {
        let existing_hash: Option<String> = ctx.service_client::<SubAgentClient>(existing.clone())
            .task_hash()
            .call()
            .await
            .ok();
        if existing_hash.as_deref() == Some(&task_hash) {
            return Err(HandlerError::from("duplicate sub-agent task detected (loop prevention)"));
        }
    }

    // Allocate sub-agent id.
    let sub_id = format!("{}-{}", ctx.key(), ctx.rand_uuid());

    // Register as child.
    let mut updated_children = children;
    updated_children.push(sub_id.clone());
    ctx.set(K_CHILDREN, updated_children);

    // Pre-register result awakeable.
    let (result_awakeable_id, result_future) = ctx.awakeable::<SubAgentResult>();

    // Dispatch.
    ctx.object_client::<SubAgentClient>(sub_id.clone())
        .post_message(SubAgentMessage::InitialTask {
            task,
            tool_subset,
            budget_tokens,
            parent_session,
            parent_sub_agent,
            depth: current_depth + 1,
            result_awakeable_id,
        })
        .send();  // fire-and-forget; the sub-agent will resolve the awakeable

    // Deduct from parent budget.
    let parent_budget: u64 = ctx.get(K_BUDGET_REMAINING).await?.unwrap_or(u64::MAX);
    ctx.set(K_BUDGET_REMAINING, parent_budget.saturating_sub(budget_tokens));

    // Durable wait for result.
    let result = result_future.await?;

    // Remove from children list.
    let mut still_children: Vec<SubAgentId> = ctx.get(K_CHILDREN).await?.unwrap_or_default();
    still_children.retain(|c| c != &sub_id);
    ctx.set(K_CHILDREN, still_children);

    Ok(result)
}
```

The LLM invokes sub-agent dispatch via a tool call like `dispatch_sub_agent` with `task` and `tools` args. Register this as a synthetic tool that the handler catches before calling `ToolExecutor` (because its "execution" is `dispatch_sub_agent`, not a generic tool invocation).

### 7. Sub-agent approval routing

In `SubAgent::run_turn`, when an approval is needed, persist the awakeable in the **session** event log (not just sub-agent state) with `sub_agent_id` set, so the gateway's pending-approvals query surfaces it under the parent user's UI:

```rust
// In SubAgent::run_turn approval block:
let parent_session: SessionId = ctx.get(K_PARENT_SESSION).await?.unwrap();
ctx.service_client::<SessionStoreClient>()
    .append_event(parent_session, SessionEvent::ApprovalRequested {
        tool_call: serde_json::to_value(&tool_call)?,
        awakeable_id: awakeable_id.clone(),
        // NEW FIELD: sub_agent_id indicates this approval is for a nested agent.
        // Extend SessionEvent::ApprovalRequested to include Option<SubAgentId>.
    })
    .call()
    .await?;
```

Gateway fetches pending approvals for the root session and includes any `sub_agent_id`-tagged ones. When the user decides, gateway calls `SubAgent/approve` (not `Session/approve`) with the appropriate VO key.

### 8. Unit tests

`moa-orchestrator/tests/sub_agent.rs`:

- `initial_task_seeds_state` — post InitialTask, assert all state keys populated
- `follow_up_queues_message` — post FollowUp, assert `K_PENDING` grows
- `depth_exceeds_limit_errors` — dispatch from depth=3, assert error
- `fan_out_exceeds_limit_errors` — dispatch 5th child from same parent, assert error
- `loop_detection` — dispatch identical task twice, assert second rejected
- `budget_deducted_on_dispatch` — parent budget decreases by child's allocation
- `result_awakeable_resolves_parent` — sub-agent completes, parent's awaiting handler wakes with result
- `sub_agent_approval_routes_to_parent_event_log` — SubAgent approval request appears in parent session event log

### 9. Integration test

`moa-orchestrator/tests/integration/sub_agent_e2e.rs`:

- Create root session, post message that triggers sub-agent dispatch.
- Assert SubAgent VO created with expected state.
- Assert sub-agent runs, completes, resolves parent's awakeable.
- Assert root session receives result, incorporates into next LLM turn, emits final response.
- Verify child-rooted event trail: events in Postgres carry `sub_agent_id` where appropriate.

## Files to create or modify

- `moa-core/src/types.rs` — add SubAgent message types, `SubAgentResult`, state types; extend `SessionEvent::ApprovalRequested` with optional `sub_agent_id`
- `moa-orchestrator/src/objects/sub_agent.rs` — new
- `moa-orchestrator/src/objects/mod.rs` — add `pub mod sub_agent;`
- `moa-orchestrator/src/sub_agent_dispatch.rs` — dispatch helper
- `moa-orchestrator/src/objects/session.rs` — import dispatch helper, hook into the `dispatch_sub_agent` synthetic tool path
- `moa-orchestrator/src/main.rs` — wire SubAgent VO into endpoint
- `moa-gateway/src/routes/approvals.rs` — route to `Session/approve` or `SubAgent/approve` based on `sub_agent_id` presence
- `moa-orchestrator/tests/sub_agent.rs` — unit tests
- `moa-orchestrator/tests/integration/sub_agent_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] Unit tests pass.
- [ ] Integration test: a root session dispatches a sub-agent, receives a result, and continues correctly.
- [ ] Fork-bomb limits enforced: 4th sibling dispatch fails, depth-4 dispatch fails.
- [ ] Sub-agent approval appears in parent session's pending approvals; gateway routes approve call to the correct VO.
- [ ] Budget inheritance: total tokens across parent + all descendants <= original parent budget.
- [ ] Pod restart mid-sub-agent: sub-agent resumes from journal, parent continues awaiting.
- [ ] `restate kv get SubAgent/<sub_agent_id>/depth` returns expected value.

## Notes

- **Result delivery uses pre-registered awakeable**, not a direct VO call back to parent. This is cleaner: the parent's turn handler blocks on `awakeable.await` and resumes when the sub-agent resolves it. No separate `Session::receive_sub_agent_result` handler needed.
- **Loop detection via task hash** is a first-order protection. A determined loop (same subagent-chain using slightly different task strings) would bypass it. For Phase 2, add a global cycle check (have we seen this subagent_id ancestry pattern before?). Defer.
- **Cancellation propagation**: parent `cancel` should cascade to children. Implementation: on parent cancel, iterate `K_CHILDREN`, call `SubAgent::cancel` on each. Add to `Session::cancel` handler.
- **Shared turn logic**: resist the urge to abstract this in R08. After R09 ships, consolidate `Session::run_turn` and `SubAgent::run_turn` into a shared helper in `turn_runner.rs`. The duplication now is intentional.

## What R09 expects

- SubAgent VO works, including dispatch, results, and approvals.
- Fork-bomb limits enforced.
- The pattern for dispatching async work from a VO to another VO with awakeable-based result delivery is established. R09 uses it for scheduled consolidation.

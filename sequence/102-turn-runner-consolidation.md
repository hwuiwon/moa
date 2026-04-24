# 102 — TurnRunner Loop Consolidation

## Purpose

Extract the duplicated turn-loop logic from `Session::post_message`, `Session::run_turn`, and `SubAgent::post_message` into a single `TurnRunner::run_until_idle` method. Both VOs currently have near-identical loop structures: increment counter, check max turns, call `run_once`, match outcome, break or continue. This consolidation eliminates the duplication, ensures consistent turn-budget enforcement, and adds sub-agent telemetry that currently only exists on sessions.

End state: `Session::run_turn` and `SubAgent::post_message` each call `runner.run_until_idle(&mut ctx, MAX_TURNS)` — a single method that owns the loop, cancel checks, outcome application, and per-turn telemetry. The loop code in each VO shrinks to <10 lines.

## Prerequisites

- R06 (`Session::run_turn`), R08 (`SubAgent` VO) complete.
- `AgentAdapter` trait and `TurnRunner<A>` working in `moa-orchestrator/src/turn/`.

## Read before starting

```
cat moa-orchestrator/src/turn/runner.rs
cat moa-orchestrator/src/turn/adapter.rs
cat moa-orchestrator/src/turn/mod.rs
cat moa-orchestrator/src/objects/session.rs
cat moa-orchestrator/src/objects/sub_agent.rs
cat moa-orchestrator/src/observability.rs
```

## Architecture

### What's duplicated today

Both `Session::run_turn` and `SubAgent::post_message` contain this pattern:

```rust
let mut turns = 0;
loop {
    turns += 1;
    if turns > MAX_TURNS_PER_POST { /* error handling */ break; }
    // check cancel flag
    let outcome = runner.run_once(&mut ctx).await?;
    // apply outcome to state
    match outcome {
        Continue => continue,
        Idle | WaitingApproval | Cancelled => break,
    }
}
```

Session has rich telemetry (TurnLatencyCounters, TurnReplayCounters, span creation). SubAgent has none. The loop logic, cancel-flag checking, max-turns enforcement, and outcome application should be shared.

### Target: `TurnRunner::run_until_idle`

```rust
impl<A: AgentAdapter> TurnRunner<A> {
    /// Runs consecutive turns until the agent becomes idle, blocked, or cancelled.
    /// Returns the terminal outcome.
    pub async fn run_until_idle(
        &self,
        ctx: &mut ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<TurnOutcome, HandlerError> {
        for turn_number in 1..=max_turns {
            // 1. Check adapter-level cancel
            if self.adapter.is_cancelled(ctx).await? {
                self.adapter.apply_outcome(ctx, TurnOutcome::Cancelled).await?;
                return Ok(TurnOutcome::Cancelled);
            }

            // 2. Create per-turn telemetry span
            let meta = self.adapter.session_meta(ctx).await.ok();
            let span = self.create_turn_span(meta.as_ref(), turn_number);

            // 3. Run one turn with instrumentation
            let outcome = self.run_once(ctx)
                .instrument(span.clone())
                .await?;

            // 4. Apply outcome to adapter state
            self.adapter.apply_outcome(ctx, outcome).await?;

            // 5. Emit turn telemetry
            self.emit_turn_metrics(&span, turn_number, &outcome);

            // 6. Continue or break
            match outcome {
                TurnOutcome::Continue => continue,
                terminal => return Ok(terminal),
            }
        }

        // Max turns exceeded — emit error event, return Idle
        self.adapter.emit_turn_budget_exceeded(ctx, max_turns).await?;
        self.adapter.apply_outcome(ctx, TurnOutcome::Idle).await?;
        Ok(TurnOutcome::Idle)
    }
}
```

## Steps

### 1. Add `apply_outcome` to `AgentAdapter` trait

In `moa-orchestrator/src/turn/adapter.rs`, add:

```rust
/// Applies a turn outcome to the agent's durable lifecycle state.
async fn apply_outcome(
    &self,
    ctx: &ObjectContext<'_>,
    outcome: TurnOutcome,
) -> Result<(), HandlerError>;

/// Emits a structured error event when max turns is exceeded.
async fn emit_turn_budget_exceeded(
    &self,
    ctx: &ObjectContext<'_>,
    max_turns: usize,
) -> Result<(), HandlerError>;
```

Implement for `SessionTurnAdapter`: loads `SessionVoState`, calls `apply_turn_outcome`, persists, syncs status to Postgres via `SessionStoreClient`.

Implement for `SubAgentTurnAdapter`: loads `SubAgentVoState`, calls `apply_turn_outcome`, persists. For `emit_turn_budget_exceeded`, set state to `Failed` and emit an error event to the parent session store.

### 2. Move telemetry span creation into `TurnRunner`

Move `session_turn_span` from `Session::run_turn` into a `TurnRunner::create_turn_span` method. Make it work for both sessions (with full metadata) and sub-agents (with synthetic metadata from the adapter). The span should include:
- `moa.turn.number`
- `moa.session.id`
- `moa.sub_agent.id` (if present, from `adapter.sub_agent_id()`)
- `moa.model`

Move `emit_turn_latency_summary` and `emit_turn_replay_summary` into `TurnRunner::emit_turn_metrics`.

### 3. Implement `run_until_idle` on `TurnRunner`

As described in the architecture section above. This is the core of the prompt. The method must:
- Check cancellation at the top of each iteration (before the LLM call)
- Create a per-turn span for telemetry
- Call `run_once` instrumented with the span
- Apply the outcome to adapter state
- Emit metrics
- Honor the max_turns cap
- Return the terminal outcome

### 4. Simplify `Session::run_turn`

Replace the ~60-line loop in `Session::run_turn` with:

```rust
async fn run_turn(&self, mut ctx: ObjectContext<'_>) -> Result<Json<TurnOutcome>, HandlerError> {
    annotate_restate_handler_span("Session", "run_turn");
    let runner = TurnRunner::new(SessionTurnAdapter);
    let outcome = runner.run_until_idle(&mut ctx, MAX_TURNS_PER_POST).await?;
    Ok(Json::from(outcome))
}
```

### 5. Simplify `SubAgent::post_message`

Replace the loop in `SubAgent::post_message` (after the message handling section) with:

```rust
// After handling InitialTask/FollowUp/ChildResult and persisting state:
let runner = TurnRunner::new(SubAgentTurnAdapter);
runner.run_until_idle(&mut ctx, MAX_TURNS_PER_POST).await?;
maybe_resolve_parent_awakeable(&ctx).await?;
```

### 6. Verify `TurnReplayCounters` and `TurnLatencyCounters` scoping

Currently, Session wraps each turn in `scope_turn_replay_counters` and `scope_turn_latency_counters`. Move this scoping into `run_until_idle` so sub-agents get it too. Use `tokio::task_local!` or the existing mechanism.

### 7. Tests

- Unit: `run_until_idle` stops at `TurnOutcome::Idle`
- Unit: `run_until_idle` stops at `TurnOutcome::WaitingApproval`
- Unit: `run_until_idle` stops at `TurnOutcome::Cancelled` (cancel flag set)
- Unit: `run_until_idle` enforces max_turns cap
- Unit: `emit_turn_budget_exceeded` called when max_turns reached
- Unit: both Session and SubAgent produce turn telemetry spans after consolidation
- Regression: all existing Session tests pass unchanged
- Regression: all existing SubAgent tests pass unchanged

## Files to create or modify

- `moa-orchestrator/src/turn/adapter.rs` — add `apply_outcome`, `emit_turn_budget_exceeded`
- `moa-orchestrator/src/turn/runner.rs` — add `run_until_idle`, move telemetry helpers
- `moa-orchestrator/src/objects/session.rs` — simplify `run_turn`, implement new adapter methods
- `moa-orchestrator/src/objects/sub_agent.rs` — simplify `post_message`, implement new adapter methods
- `moa-orchestrator/src/observability.rs` — make span helpers reusable from `TurnRunner`

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] Session behavior identical to before (all existing tests pass).
- [ ] SubAgent behavior identical to before (all existing tests pass).
- [ ] SubAgent turns now emit telemetry spans (previously missing).
- [ ] Max-turns error events emitted for both Session and SubAgent.
- [ ] The turn-loop code in `Session::run_turn` is <10 lines.
- [ ] The turn-loop code in `SubAgent::post_message` is <10 lines.
- [ ] No duplicated turn-loop logic remains across the two VOs.

## Notes

- **This is a pure refactor — no behavioral changes.** All existing tests must pass without modification. If a test breaks, the consolidation introduced a behavioral change that needs to be reverted.
- **SubAgent telemetry is the bonus feature.** Today, sub-agent turns have no spans or latency counters. After this prompt, they get the same instrumentation as sessions, for free.
- **Do NOT change `run_once`** — it stays exactly as-is. `run_until_idle` is a wrapper around it.
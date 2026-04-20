# R05 — `Session` Virtual Object: State and Lifecycle Handlers

## Purpose

Ship the `Session` Virtual Object with its state shape and lifecycle handlers: `post_message`, `status`, `cancel`, `destroy`. The brain loop itself (`run_turn`) is **not** in this prompt — it lands in R06. Keeping state and lifecycle separate from the turn logic is deliberate: this prompt establishes the VO shell, verifies state persistence across restarts, and confirms the single-writer semantics before adding the harder brain loop.

End state: a VO keyed by `session_id` that accepts user messages, queues them in state, updates session metadata, supports graceful cancellation, and can be destroyed. `run_turn` is stubbed to return `TurnOutcome::Idle` immediately.

## Prerequisites

- R01–R04 complete.
- `SessionStore`, `LLMGateway`, `ToolExecutor` services all registered and working.
- Local `restate-server` running.

## Read before starting

- `docs/12-restate-architecture.md` — "Virtual Object" and "Handler signatures" sections, especially the `Session` VO state table
- `docs/05-session-event-log.md` — understand what events get emitted when
- `moa-core/src/types.rs` — `UserMessage`, `SessionStatus`, `CancelMode`, `ApprovalDecision`

## Steps

### 1. Define supporting types (if not already in `moa-core`)

```rust
// moa-core/src/types.rs (add if missing)
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub text: String,
    pub attachments: Vec<String>, // paths or URIs
    pub sent_at: DateTime<Utc>,
    pub platform_msg_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    Soft,   // finish current tool, then stop
    Hard,   // abort immediately
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalDecision {
    AllowOnce,
    AlwaysAllow { pattern: String },
    Deny { reason: Option<String> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TurnOutcome {
    Continue,
    Idle,
    WaitingApproval,
    Cancelled,
}
```

### 2. Define the Session VO trait

`moa-orchestrator/src/objects/session.rs` (create new `objects/` module):

```rust
use restate_sdk::prelude::*;
use moa_core::types::*;

#[restate_sdk::object]
pub trait Session {
    /// Append a user message and drive turns until idle or blocked.
    async fn post_message(
        ctx: ObjectContext<'_>,
        msg: UserMessage,
    ) -> Result<(), HandlerError>;

    /// Resolve an outstanding approval awakeable. (Implementation lands in R07.)
    async fn approve(
        ctx: ObjectContext<'_>,
        decision: ApprovalDecision,
    ) -> Result<(), HandlerError>;

    /// Soft or hard cancel.
    async fn cancel(
        ctx: ObjectContext<'_>,
        mode: CancelMode,
    ) -> Result<(), HandlerError>;

    /// Read-only status query; runs concurrently with mutations.
    #[shared]
    async fn status(
        ctx: SharedObjectContext<'_>,
    ) -> Result<SessionStatus, HandlerError>;

    /// Internal: run one brain turn. Stubbed in R05, implemented in R06.
    async fn run_turn(
        ctx: ObjectContext<'_>,
    ) -> Result<TurnOutcome, HandlerError>;

    /// Clear VO state after session completion + grace period.
    async fn destroy(
        ctx: ObjectContext<'_>,
    ) -> Result<(), HandlerError>;
}
```

### 3. State keys as constants

```rust
// moa-orchestrator/src/objects/session.rs
const K_META: &str = "meta";
const K_STATUS: &str = "status";
const K_PENDING: &str = "pending";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_CHILDREN: &str = "children";
const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
const K_CANCEL_FLAG: &str = "cancel_flag";
```

### 4. Implement `post_message`

```rust
pub struct SessionImpl;

impl Session for SessionImpl {
    async fn post_message(
        ctx: ObjectContext<'_>,
        msg: UserMessage,
    ) -> Result<(), HandlerError> {
        let session_id: uuid::Uuid = ctx.key().parse()
            .map_err(|e| HandlerError::from(format!("invalid session key: {}", e)))?;

        // Ensure session metadata exists. If this is the first message, bootstrap.
        let meta = ctx.get::<SessionMeta>(K_META).await?;
        if meta.is_none() {
            return Err(HandlerError::from(
                "Session metadata missing. Create session via SessionStore first."
            ));
        }

        // Queue the message. Since we're the single writer, no concurrency concern.
        let mut pending = ctx.get::<Vec<UserMessage>>(K_PENDING).await?.unwrap_or_default();
        pending.push(msg.clone());
        ctx.set(K_PENDING, pending);

        // Persist to Postgres event log.
        ctx.service_client::<SessionStoreClient>()
            .append_event(session_id, SessionEvent::UserMessage {
                text: msg.text.clone(),
                attachments: msg.attachments.clone(),
            })
            .call()
            .await?;

        // Update status to running.
        ctx.set(K_STATUS, SessionStatus::Running);
        ctx.service_client::<SessionStoreClient>()
            .update_status(session_id, SessionStatus::Running)
            .call()
            .await?;

        // Drive turns. Self-call via object_client; each turn is its own invocation.
        loop {
            // Check cancellation before each turn.
            if let Some(mode) = ctx.get::<CancelMode>(K_CANCEL_FLAG).await? {
                tracing::info!(?mode, "cancelling session");
                ctx.set(K_STATUS, SessionStatus::Cancelled);
                ctx.clear(K_CANCEL_FLAG);
                break;
            }

            let outcome = ctx
                .object_client::<SessionClient>(ctx.key())
                .run_turn()
                .call()
                .await?;

            match outcome {
                TurnOutcome::Continue => continue,
                TurnOutcome::Idle => {
                    ctx.set(K_STATUS, SessionStatus::Idle);
                    break;
                }
                TurnOutcome::WaitingApproval => {
                    ctx.set(K_STATUS, SessionStatus::WaitingApproval);
                    break;
                }
                TurnOutcome::Cancelled => {
                    ctx.set(K_STATUS, SessionStatus::Cancelled);
                    break;
                }
            }
        }

        // Final status update to Postgres.
        let final_status = ctx.get::<SessionStatus>(K_STATUS).await?.unwrap_or(SessionStatus::Idle);
        ctx.service_client::<SessionStoreClient>()
            .update_status(session_id, final_status)
            .call()
            .await?;

        Ok(())
    }

    async fn run_turn(_ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
        // STUB. R06 replaces this with the actual brain loop.
        // For R05, just drain pending messages and return Idle.
        Ok(TurnOutcome::Idle)
    }

    async fn status(
        ctx: SharedObjectContext<'_>,
    ) -> Result<SessionStatus, HandlerError> {
        Ok(ctx.get::<SessionStatus>(K_STATUS).await?.unwrap_or(SessionStatus::Created))
    }

    async fn cancel(
        ctx: ObjectContext<'_>,
        mode: CancelMode,
    ) -> Result<(), HandlerError> {
        ctx.set(K_CANCEL_FLAG, mode);
        tracing::info!(?mode, key = %ctx.key(), "cancel flag set");
        Ok(())
    }

    async fn approve(
        _ctx: ObjectContext<'_>,
        _decision: ApprovalDecision,
    ) -> Result<(), HandlerError> {
        // STUB. R07 implements awakeable resolution.
        Err(HandlerError::from("approve handler not yet implemented (lands in R07)"))
    }

    async fn destroy(
        ctx: ObjectContext<'_>,
    ) -> Result<(), HandlerError> {
        // Clear all state keys. Restate persists nothing after this invocation.
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }
}
```

### 5. Bootstrap helper

VO state (`K_META`) must be populated *before* the first `post_message`. The caller — the gateway — does this via a helper on `SessionStore`. Extend `SessionStore`:

```rust
// services/session_store.rs — add new handler
async fn init_session_vo(
    ctx: Context<'_>,
    session_id: SessionId,
    meta: SessionMeta,
) -> Result<(), HandlerError> {
    // This handler is called by the gateway right after SessionStore::create_session
    // to populate the VO with its initial state.
    ctx.object_client::<SessionClient>(session_id.to_string())
        .set_meta(meta)
        .call()
        .await?;
    Ok(())
}
```

Add `set_meta` to the Session VO trait (admin-only, not user-facing):

```rust
// Session VO — add:
async fn set_meta(
    ctx: ObjectContext<'_>,
    meta: SessionMeta,
) -> Result<(), HandlerError>;

// Impl:
async fn set_meta(ctx: ObjectContext<'_>, meta: SessionMeta) -> Result<(), HandlerError> {
    ctx.set(K_META, meta);
    ctx.set(K_STATUS, SessionStatus::Created);
    Ok(())
}
```

Alternative: have `post_message` auto-create metadata from a bootstrap payload on first call. The explicit `set_meta` is cleaner and keeps responsibility at the gateway.

### 6. Wire into main

```rust
// main.rs
HttpServer::new(
    Endpoint::builder()
        .bind(services::health::HealthImpl.serve())
        .bind(services::session_store::SessionStoreImpl { pool: pool.clone() }.serve())
        .bind(services::llm_gateway::LLMGatewayImpl { providers: providers.clone() }.serve())
        .bind(services::tool_executor::ToolExecutorImpl { router: router.clone() }.serve())
        .bind(objects::session::SessionImpl.serve())
        .build(),
)
.listen_and_serve(...)
.await
```

### 7. Unit tests

`moa-orchestrator/tests/session_vo.rs`:

- `post_message_without_meta_errors` — without prior `set_meta`, `post_message` returns error
- `post_message_queues_in_state` — after call, `K_PENDING` contains the message
- `post_message_updates_status_to_running` — status transitions Created → Running → Idle
- `status_shared_handler_does_not_block` — concurrent `status()` calls succeed during an in-flight `post_message`
- `cancel_sets_flag` — after `cancel`, `K_CANCEL_FLAG` is set; next turn returns Cancelled
- `destroy_clears_state` — after `destroy`, all state keys are empty

Use `restate-test-server` for in-process VO testing.

### 8. Integration test

`moa-orchestrator/tests/integration/session_vo_e2e.rs`:

- Create a session via `SessionStore/create_session`.
- Call `SessionStore/init_session_vo` with the meta.
- Call `Session/post_message` on `session_id=<uuid>` with a UserMessage.
- Assert returns `Ok(())`.
- Call `Session/status` — assert returns `Idle`.
- Verify `UserMessage` event appeared in Postgres.
- Kill and restart `moa-orchestrator`, call `Session/status` again — assert VO state survived.
- Call `Session/cancel(Soft)`, then `Session/post_message`, assert status becomes `Cancelled`.
- Call `Session/destroy`, then `Session/status` — assert `Created` (empty state).

## Files to create or modify

- `moa-orchestrator/src/objects/mod.rs` — new module
- `moa-orchestrator/src/objects/session.rs` — new
- `moa-orchestrator/src/services/session_store.rs` — add `init_session_vo` handler
- `moa-orchestrator/src/main.rs` — wire the VO
- `moa-core/src/types.rs` — add `UserMessage`, `CancelMode`, `ApprovalDecision`, `TurnOutcome` if missing
- `moa-orchestrator/tests/session_vo.rs` — unit tests
- `moa-orchestrator/tests/integration/session_vo_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` succeeds.
- [ ] `cargo test -p moa-orchestrator session_vo` passes all unit tests.
- [ ] Integration test passes end-to-end against local `restate-server`.
- [ ] `restate kv get Session/<session_id>/meta` returns the stored meta.
- [ ] `restate invocation list` shows `Session/post_message` invocation completed.
- [ ] Pod restart mid-invocation: the VO state survives, the invocation replays and completes.
- [ ] Concurrent `post_message` and `status` calls: `status` returns promptly even while `post_message` is in-flight (single-writer queues mutations, shared reads do not block).
- [ ] `run_turn` stub returns `Idle`; no actual LLM calls happen yet.

## Notes

- **Why `run_turn` is stubbed**: the actual turn loop depends on context compilation (`moa-brain`) and approval flow (R07). Landing state + lifecycle separately lets you prove the VO shell works before complicating it.
- **Self-call cost**: each `post_message` → `run_turn` invocation costs ~1 extra journal entry vs inlining. Fixed overhead, worth the debuggability. See the decision in `docs/12-restate-architecture.md`.
- **`clear_all()` semantics**: Restate clears all state keys for this object instance. Subsequent calls see empty state. The VO "exists" whenever any state key exists; `destroy` effectively frees the VO.
- **Sub-agent children field unused in R05**: `K_CHILDREN` is declared but not populated until R08. Leaving it in the constants list keeps the state shape stable.
- **Gateway integration not in R05**: how the external gateway invokes `post_message` is covered in R10/R12. For R05, use `restate` CLI to hand-drive the VO.

## What R06 expects

- `Session` VO registered and callable.
- State shape stable across restarts.
- `run_turn` stub in place, ready to be replaced.
- `post_message` drives the turn loop correctly (even though each turn is trivially Idle).

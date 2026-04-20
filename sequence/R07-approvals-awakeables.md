# R07 — Approvals via Awakeables

## Purpose

Wire the approval flow end-to-end using Restate awakeables. Replace the R06 stub that auto-denies approval-required tools. When `run_turn` encounters such a tool, it creates an awakeable, persists the id to the event log, and suspends. The gateway fetches the id, renders an approval card to the user, and calls the Restate admin API to resolve the awakeable with the user's decision. The suspended turn resumes at the `.await` point.

End state: a session that invokes `bash` (approval-required) pauses at the approval point, a gateway-side callback resolves it, and the turn proceeds based on the decision. Timeout handling returns an auto-deny with explanation. Sub-agent approvals route through the parent.

## Prerequisites

- R01–R06 complete. Brain loop works end-to-end.
- `moa-gateway` crate exists with HTTP endpoints for approval buttons.
- Restate admin API accessible (default `http://localhost:9070`).

## Read before starting

- `docs/12-restate-architecture.md` — "Approvals via awakeables" section
- `docs/02-brain-orchestration.md` — existing approval semantics
- Restate docs on awakeables: https://docs.restate.dev/concepts/durable_building_blocks#awakeables
- `moa-gateway/src/` — existing approval button handlers and callback wiring

## Steps

### 1. Define approval policy

Move the hardcoded `requires_approval` check into a real policy:

```rust
// moa-core/src/approval.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPolicy {
    /// Tools that always require approval
    pub always_approve: Vec<String>,
    /// Tools that never require approval (allowlist)
    pub allowlist: Vec<String>,
    /// Always-allow patterns from prior user decisions
    pub always_allow_patterns: Vec<AlwaysAllowPattern>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlwaysAllowPattern {
    pub tool_name: String,
    pub pattern: String,  // e.g., "cargo test*" for bash
    pub approved_by: uuid::Uuid,
    pub approved_at: chrono::DateTime<chrono::Utc>,
}

impl ApprovalPolicy {
    pub fn requires_approval(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        if self.allowlist.contains(&tool_name.to_string()) {
            return false;
        }
        if self.always_approve.contains(&tool_name.to_string()) {
            // Check always_allow patterns
            for pattern in &self.always_allow_patterns {
                if pattern.tool_name == tool_name && matches_pattern(&pattern.pattern, input) {
                    return false;
                }
            }
            return true;
        }
        // Default: NonIdempotent tools require approval
        false
    }
}
```

Policy is loaded from workspace settings (Postgres) or a per-session override. For R07, fetch once per turn via a Service call; caching is a later optimization.

### 2. Update `run_turn` approval handling

Replace the auto-deny block from R06:

```rust
// In run_turn, per tool_call:
let policy = ctx.service_client::<WorkspaceStoreClient>()
    .get_approval_policy(meta.workspace_id)
    .call()
    .await?;

if policy.requires_approval(&tool_call.name, &tool_call.input) {
    // Create awakeable.
    let (awakeable_id, awakeable) = ctx.awakeable::<ApprovalDecision>();

    // Persist: approval request must be discoverable by the gateway.
    ctx.set(K_PENDING_APPROVAL, awakeable_id.clone());
    ctx.service_client::<SessionStoreClient>()
        .append_event(session_id, SessionEvent::ApprovalRequested {
            tool_call: serde_json::to_value(&tool_call)?,
            awakeable_id: awakeable_id.clone(),
        })
        .call()
        .await?;

    // Race the awakeable against a timeout.
    let decision = tokio::select! {
        decision = awakeable => decision?,
        _ = ctx.sleep(std::time::Duration::from_secs(30 * 60)) => {
            ApprovalDecision::Deny {
                reason: Some("Auto-denied: no decision within 30 minutes".to_string())
            }
        }
    };

    ctx.clear(K_PENDING_APPROVAL);

    // Log decision.
    ctx.service_client::<SessionStoreClient>()
        .append_event(session_id, SessionEvent::ApprovalDecided {
            awakeable_id,
            decision: serde_json::to_value(&decision)?,
        })
        .call()
        .await?;

    match decision {
        ApprovalDecision::Deny { reason } => {
            // Feed denial back to LLM via a synthetic ToolResult; skip actual execution.
            ctx.service_client::<SessionStoreClient>()
                .append_event(session_id, SessionEvent::ToolResult {
                    tool_id: tool_call.id,
                    output: format!(
                        "User denied approval. Reason: {}",
                        reason.unwrap_or_else(|| "no reason given".to_string())
                    ),
                    success: false,
                    duration_ms: 0,
                })
                .call()
                .await?;
            continue;  // skip this tool, proceed to next or next turn
        }
        ApprovalDecision::AllowOnce => { /* fall through to execute */ }
        ApprovalDecision::AlwaysAllow { pattern } => {
            // Persist the always-allow pattern to workspace policy.
            ctx.service_client::<WorkspaceStoreClient>()
                .add_always_allow(meta.workspace_id, AlwaysAllowPattern {
                    tool_name: tool_call.name.clone(),
                    pattern,
                    approved_by: meta.user_id,
                    approved_at: chrono::Utc::now(),
                })
                .call()
                .await?;
            /* fall through to execute */
        }
    }
}

// Execute the tool (unchanged from R06).
let _tool_output = ctx.service_client::<ToolExecutorClient>()
    .execute(tool_req)
    .call()
    .await?;
```

### 3. Implement `Session::approve`

The `approve` handler on the Session VO is what the gateway calls after fetching the awakeable id from the event log. But note: **`approve` doesn't resolve the awakeable directly**. The Restate admin API does that. The handler exists as a convenience: it wraps the admin call and validates authorization.

Two patterns are valid:

**Pattern A (direct admin call from gateway)**: Gateway calls `POST /restate/v1/awakeables/{id}/resolve` with the decision payload. Session VO never sees the approval — it just wakes up. Simpler, but the gateway must have admin credentials.

**Pattern B (via Session::approve)**: Gateway calls `Session/approve(decision)`; handler validates the decision is for the pending approval, then calls the admin API internally. More auditable.

Choose Pattern B. Implement:

```rust
async fn approve(
    ctx: ObjectContext<'_>,
    decision: ApprovalDecision,
) -> Result<(), HandlerError> {
    let awakeable_id = ctx.get::<String>(K_PENDING_APPROVAL).await?
        .ok_or_else(|| HandlerError::from("no pending approval for this session"))?;

    // Resolve via Restate admin API. This is the side effect; wrap in ctx.run.
    ctx.run("resolve_awakeable", || async {
        let admin_url = std::env::var("RESTATE_ADMIN_URL")
            .unwrap_or_else(|_| "http://localhost:9070".to_string());
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/awakeables/{}/resolve", admin_url, awakeable_id))
            .json(&decision)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(format!("awakeable resolution failed: {}", resp.status()).into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;

    Ok(())
}
```

Alternatively, if `restate-sdk` provides a client for admin operations, use that instead of raw `reqwest`.

### 4. Gateway integration

`moa-gateway/src/routes/approvals.rs`:

```rust
// Existing route: POST /sessions/:session_id/approvals/:awakeable_id
// Body: ApprovalDecision

pub async fn handle_approval(
    Path((session_id, awakeable_id)): Path<(Uuid, String)>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(decision): Json<ApprovalDecision>,
) -> Result<impl IntoResponse, ApiError> {
    // Verify the user owns the session.
    let session = db.get_session(session_id).await?;
    if session.user_id != user.id {
        return Err(ApiError::Forbidden);
    }

    // Verify the awakeable is the current pending one.
    let pending_events = db.get_events_by_type(session_id, "ApprovalRequested").await?;
    let matching = pending_events.iter().find(|e| matches_awakeable(e, &awakeable_id));
    if matching.is_none() {
        return Err(ApiError::NotFound);
    }

    // Call Session::approve on the VO.
    restate_client.object(&format!("Session/{}", session_id))
        .invoke("approve", &decision)
        .await?;

    Ok(StatusCode::OK)
}
```

Remove any old signal-sending code paths from the retired workflow engine.

### 5. Timeout handling

The `tokio::select!` in step 2 races the awakeable against a 30-minute sleep. The sleep is durable — if the pod dies during the wait, Restate resumes the timer.

For longer/shorter per-tenant timeouts, fetch from policy:

```rust
let timeout = policy.approval_timeout.unwrap_or(Duration::from_secs(30 * 60));
let decision = tokio::select! {
    d = awakeable => d?,
    _ = ctx.sleep(timeout) => ApprovalDecision::Deny {
        reason: Some(format!("Auto-denied: no decision within {} minutes", timeout.as_secs() / 60))
    }
};
```

### 6. Sub-agent approval routing

When a SubAgent turn requests approval, its awakeable lives in its own VO state. But the user's UI is attached to the root session, not the sub-agent. The flow:

1. SubAgent VO creates awakeable, persists to its own `K_PENDING_APPROVAL`.
2. SubAgent emits an `ApprovalRequested` event into the session event log with `sub_agent_id` and awakeable id.
3. Gateway fetches pending approvals across the whole session tree (Postgres query by `session_id` root + any `sub_agent_id` descendant).
4. User decides; gateway calls `SubAgent/approve` with the decision.
5. SubAgent resolves its own awakeable.

Implementation note: for R07, scope to the parent Session VO only. Sub-agent approvals land in R08.

### 7. Unit tests

`moa-orchestrator/tests/session_approvals.rs`:

- `approval_request_creates_awakeable_and_persists_event` — mock policy returns "approve required", assert awakeable id stored in state and in event log
- `allow_once_proceeds_to_execute` — resolve awakeable with AllowOnce, assert ToolExecutor called
- `deny_skips_execution_and_emits_synthetic_result` — resolve with Deny, assert ToolExecutor not called, ToolResult event with `success: false`
- `always_allow_persists_pattern` — resolve with AlwaysAllow, assert WorkspaceStore updated
- `timeout_auto_denies` — do not resolve, advance time past 30 min, assert Deny with timeout reason
- `approve_without_pending_errors` — call approve when no awakeable pending, assert error

### 8. Integration test

`moa-orchestrator/tests/integration/approval_flow_e2e.rs`:

- Post message asking to run `bash echo hello` (approval-required).
- Assert session status becomes `WaitingApproval` within 5 seconds.
- Fetch pending approvals from event log.
- Call `Session/approve(AllowOnce)`.
- Assert session transitions Running → Idle.
- Verify ToolResult event has the echo output.
- Retry test with `Deny`; assert synthetic ToolResult with denial reason.
- Retry test with timeout (wait 30 minutes — use a 30-second override in test mode); assert auto-deny.

## Files to create or modify

- `moa-core/src/approval.rs` — new, `ApprovalPolicy`
- `moa-orchestrator/src/objects/session.rs` — implement `approve`, expand `run_turn` approval block
- `moa-orchestrator/src/services/workspace_store.rs` — new service (stub if Workspace service doesn't yet exist; R09 fills it out)
- `moa-gateway/src/routes/approvals.rs` — wire to Session/approve
- `moa-gateway/src/` — remove obsolete signal code paths
- `moa-orchestrator/tests/session_approvals.rs` — unit tests
- `moa-orchestrator/tests/integration/approval_flow_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build` across workspace succeeds.
- [ ] All unit tests pass.
- [ ] Integration test passes end-to-end: approval flow round-trips, decisions honored.
- [ ] `restate kv get Session/<session_id>/pending_approval` returns the awakeable id while waiting.
- [ ] Pod restart during approval wait: the wait survives, user decision resolves correctly after restart.
- [ ] Timeout auto-deny works with a real wall-clock wait (use a short override for CI).
- [ ] Gateway code has zero references to obsolete signal plumbing.

## Notes

- **Awakeable id format**: opaque string returned by Restate. Do not assume any structure. Always fetch from the handler via `ctx.awakeable()`, never construct.
- **Gateway admin credentials**: the gateway needs permission to call Session/approve via Restate's ingress. Configure a service account with handler-invoke permissions in R10.
- **Never leak awakeable ids to end users**: they're passed via Postgres events (internal), never exposed in client-facing APIs. The gateway authenticates the user, then fetches the awakeable id server-side.
- **Idempotency of approve**: if gateway retries the `Session/approve` call (network glitch), the second call sees `K_PENDING_APPROVAL` cleared and errors. Safe. Make the gateway tolerate this error gracefully (approval already processed).
- **Re-opening an approval**: not supported. If the user wants to change their mind, they must cancel the session and start over. Simpler semantics.

## What R08 expects

- Approvals work end-to-end for root sessions.
- Gateway integration complete.
- Timeout + auto-deny pattern established (reusable for SubAgent).
- SubAgent approval routing pattern noted but not yet implemented — R08 implements.

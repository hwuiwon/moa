//! Generation-guarded, bottom-up Worker self-cleanup.

use super::*;

impl WorkerImpl {
    // SAFETY: internal generation-guarded self-call scheduled by this Worker VO's own
    // terminal-delivery path. It reads only this child's own VO state and writes only to
    // its own state (clear) plus the parent fan-out removal handler, which is itself an
    // established internal VO→VO write (register_child/remove_child/complete_child) on the
    // child's own parent. No caller-owned data is read back to a caller.
    pub(super) async fn cleanup(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<CleanupRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "cleanup");
        let req = req.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let has_non_terminal_child = state.children.iter().any(|child| child.terminal.is_none());
        let decision = decide_cleanup(
            req.generation == state.cleanup_generation,
            crate::delegation::is_terminal_worker_state(state.current_status())
                && !state.action_review_holds_lifecycle(),
            has_non_terminal_child,
            state.notification_delivered,
        );

        match decision {
            CleanupDecision::Skip => {
                tracing::debug!(
                    key = %ctx.key(),
                    req_generation = req.generation,
                    cleanup_generation = state.cleanup_generation,
                    "worker cleanup skipped (stale, revived, or report not durable)"
                );
            }
            CleanupDecision::Defer => {
                // Bottom-up teardown: this child still has non-terminal children, so
                // reschedule (same generation) and let them self-clean first. A revive of
                // this child bumps the generation and supersedes the rescheduled tick.
                let grace_ms = self.session_limits.worker_cleanup_grace_ms;
                if grace_ms > 0 {
                    let now = durable_utc_now(&ctx).await?;
                    schedule_cleanup_self_call(&ctx, state.cleanup_generation, now, grace_ms);
                }
                tracing::debug!(
                    key = %ctx.key(),
                    "worker cleanup deferred: non-terminal children remain"
                );
            }
            CleanupDecision::Proceed => {
                if !release_and_clear_worker(&ctx, &state).await? {
                    let attempts = state.cleanup_release_attempts.saturating_add(1);
                    let grace_ms = self.session_limits.worker_cleanup_grace_ms;
                    if attempts >= MAX_CLEANUP_RELEASE_ATTEMPTS || grace_ms == 0 {
                        // Bound the retry loop: a persistently-failing release (e.g. a provider
                        // permanently absent from the router registry) must not reschedule
                        // forever and pin the VO. Force-clear; the hand lease may leak and is
                        // reclaimed at session teardown.
                        tracing::error!(
                            key = %ctx.key(),
                            attempts,
                            grace_ms,
                            "worker hand release still incomplete after retry cap; force-clearing VO state (possible hand-lease leak until session teardown)"
                        );
                        ctx.clear_all();
                    } else {
                        state.cleanup_release_attempts = attempts;
                        state.persist(&ctx);
                        reschedule_cleanup(
                            &ctx,
                            state.cleanup_generation,
                            self.session_limits.worker_cleanup_grace_ms,
                        )
                        .await?;
                        tracing::warn!(
                            key = %ctx.key(),
                            cleanup_generation = state.cleanup_generation,
                            attempts,
                            "worker cleanup release incomplete; rescheduled cleanup"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// Decision of whether a fired `cleanup` tick should clear, wait, or be ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CleanupDecision {
    /// Stale generation, revived (non-terminal), or report not yet durable: drop the
    /// tick without clearing or rescheduling.
    Skip,
    /// Terminal but not a leaf yet (non-terminal children remain): reschedule so
    /// teardown stays bottom-up.
    Defer,
    /// Terminal leaf with a durable report: release fan-out and clear VO state.
    Proceed,
}

/// Pure cleanup guard ordering: stale/revive → terminal → bottom-up → durable report.
///
/// Kept free of `ctx` so the guard order is unit-testable without a Restate runtime.
pub(super) fn decide_cleanup(
    generation_matches: bool,
    is_terminal: bool,
    has_non_terminal_child: bool,
    notification_delivered: bool,
) -> CleanupDecision {
    // Stale/revive guard: a superseded generation or a child that was revived back to a
    // non-terminal state must not be torn down.
    if !generation_matches || !is_terminal {
        return CleanupDecision::Skip;
    }
    // Bottom-up: defer until this child's own children are terminal.
    if has_non_terminal_child {
        return CleanupDecision::Defer;
    }
    // Durable-report guard: only clear once the terminal result is recorded on the
    // parent (the same flag delivery set). Never reached in practice because cleanup is
    // scheduled only after delivery, but it fails safe by not clearing.
    if !notification_delivered {
        return CleanupDecision::Skip;
    }
    CleanupDecision::Proceed
}

/// Releases a terminal leaf child's fan-out registration and clears its VO state.
///
/// Resource release is awaited before the VO is cleared. If hand cleanup is incomplete,
/// the caller reschedules cleanup and leaves the worker state intact. The parent fan-out
/// removal is still dispatched detached after release succeeds; `clear_all` cannot fail.
pub(super) async fn release_and_clear_worker(
    ctx: &ObjectContext<'_>,
    state: &WorkerVoState,
) -> Result<bool, HandlerError> {
    let worker_id = ctx.key().to_string();

    // Resource release (hand leases / sandbox): each worker owns its own sandbox keyed
    // by `(parent_session, worker_id)` (Sandbox Increments 1-2 re-keyed hands by scope),
    // so releasing here frees exactly this child's hand without over-releasing the parent's
    // or siblings' sandboxes. The Worker VO holds no `ToolRouter`, so the release is
    // delegated to the ToolExecutor service that owns the router and awaited before
    // clearing state.
    if let Some(request) = release_worker_hands_request(state.parent_session, &worker_id)
        && let Err(error) = crate::restate_identity::replay_safe_request(
            ctx.service_client::<ToolExecutorClient>()
                .release_worker_hands(Json::from(request)),
        )
        .call()
        .await
    {
        tracing::warn!(
            key = %worker_id,
            error = ?error,
            "worker hand release incomplete"
        );
        return Ok(false);
    }

    // Remove from the root parent fan-out via the existing removal handler (detached).
    if let Some(parent_session) = state.parent_session {
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(parent_session.to_string())
                .remove_child(worker_id.clone()),
        )
        .send();
    }

    // Clear all VO state (reuse `destroy` semantics). The parent keeps the cached
    // terminal result and the durable event log, so nothing is lost.
    ctx.clear_all();
    tracing::info!(
        key = %worker_id,
        "worker self-cleaned after terminal report"
    );
    Ok(true)
}

/// Retracts at the owning Session every reply target the cleared requests advertised.
///
/// The single retraction seam for the child side: every path that drops an in-flight
/// `request_input` registration (wait timeout, cancellation, terminal turn outcome, an
/// answered round-trip) routes through here, so an advertised target can never outlive
/// the awakeable behind it. Dispatched detached — the Session may be the very caller
/// waiting on this handler, and a synchronous call back into its single-writer queue
/// would deadlock. Retraction is idempotent, so at-least-once delivery is safe.
pub(super) fn retract_session_input_targets(
    ctx: &ObjectContext<'_>,
    parent_session: SessionId,
    cleared: Vec<WorkerInputTarget>,
) {
    if cleared.is_empty() {
        return;
    }
    let worker_id = ctx.key().to_string();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .clear_worker_input_targets(Json::from(ClearWorkerInputTargetsInput {
                worker_id,
                cleared,
            })),
    )
    .send();
}

pub(super) async fn reschedule_cleanup(
    ctx: &ObjectContext<'_>,
    generation: u64,
    grace_ms: u64,
) -> Result<(), HandlerError> {
    if grace_ms > 0 {
        let now = durable_utc_now(ctx).await?;
        schedule_cleanup_self_call(ctx, generation, now, grace_ms);
    }
    Ok(())
}

/// Builds the hand-release request for a finishing worker, when it has an owning session.
///
/// Returns `None` when the child has no `parent_session`, since nothing was provisioned
/// under a session scope and there is nothing to release. The request is keyed by the
/// owning session id (where the child's hands were provisioned: a worker tool call runs
/// with `session_id = parent_session`) plus the child's own id, matching the
/// `(session_id, worker_id)` hand scope used by `ToolRouter::reclaim_hands`.
pub(super) fn release_worker_hands_request(
    parent_session: Option<SessionId>,
    worker_id: &str,
) -> Option<ReleaseWorkerHandsRequest> {
    parent_session.map(|session_id| ReleaseWorkerHandsRequest {
        session_id,
        worker_id: worker_id.to_string(),
    })
}

/// Issues one generation-guarded delayed self-call to `Worker/cleanup`.
pub(super) fn schedule_cleanup_self_call(
    ctx: &ObjectContext<'_>,
    generation: u64,
    now: DateTime<Utc>,
    grace_ms: u64,
) {
    let delay = std::time::Duration::from_millis(grace_ms);
    let scheduled_for_millis =
        (now + chrono::Duration::milliseconds(grace_ms as i64)).timestamp_millis();
    schedule_generation_guarded_self_call(
        ctx,
        WORKER_OBJECT_NAME,
        CLEANUP_HANDLER,
        generation,
        scheduled_for_millis,
        Json::from(CleanupRequest { generation }),
        delay,
    );
}

/// Registered Restate object name for the Worker VO, used for the untyped self-call.
const WORKER_OBJECT_NAME: &str = "Worker";
/// Handler name of the self-cleanup tick on the Worker VO.
const CLEANUP_HANDLER: &str = "cleanup";

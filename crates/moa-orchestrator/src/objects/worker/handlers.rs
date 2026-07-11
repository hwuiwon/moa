//! Restate handlers for the Worker VO.

use super::state::{ClaimedHistoryEntry, MAX_CLEANUP_RELEASE_ATTEMPTS, WorkerHistoryEntry};
use super::*;
use crate::objects::session::SessionClient;
use crate::services::tool_executor::{ReleaseWorkerHandsRequest, ToolExecutorClient};
use crate::workflows::worker_turn_execution::WorkerTurnExecutionClient;
use moa_core::wire::turn::{RunWorkerTurnRequest, TurnOutcomeKind};
use moa_security::{canary_system_message, new_canary_token};

impl Worker for WorkerImpl {
    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<WorkerMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "post_message");
        let message = msg.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        match &message {
            // ProvideInput answers an in-flight `request_input` round-trip: resolve the
            // matching awakeable to unblock the parked child turn. It never starts a turn,
            // enqueues a message, or touches the cleanup generation. A missing pending
            // entry (already resolved, timed out, or unknown id) is an idempotent no-op.
            WorkerMessage::ProvideInput {
                input_request_id,
                text,
            } => {
                if let Some(awakeable_id) = state.take_input_awakeable(input_request_id) {
                    ctx.resolve_awakeable(&awakeable_id, text.clone());
                    state.persist(&ctx);
                    tracing::info!(
                        key = %ctx.key(),
                        input_request_id = %input_request_id,
                        "resolved worker input request awakeable"
                    );
                } else {
                    tracing::debug!(
                        key = %ctx.key(),
                        input_request_id = %input_request_id,
                        "ignored ProvideInput for unknown or already-resolved input request"
                    );
                }
                return Ok(());
            }
            WorkerMessage::InitialTask(_) => {
                state
                    .initialize(&message)
                    .map_err(moa_error_to_handler_error)?;
            }
            WorkerMessage::FollowUp { text } => {
                // Reject a follow-up to a child whose VO state was cleared by
                // self-cleanup: there is nothing to revive and re-bootstrapping would
                // resurrect a completed child. A still-initialized terminal child (within
                // the grace window) is revived by `enqueue_follow_up` as before.
                if !state.accepts_follow_up() {
                    return Err(TerminalError::new(
                        "worker already completed; its state was cleaned up and it cannot accept follow-ups",
                    )
                    .into());
                }
                state
                    .enqueue_follow_up(text.clone())
                    .map_err(moa_error_to_handler_error)?;
            }
        }
        // Accepting any message supersedes a pending self-cleanup scheduled during the
        // grace window, so a message arriving mid-grace revives the child instead of
        // letting the delayed `cleanup` tick clear it.
        state.bump_cleanup_generation();
        let turn_id = if state.active_turn_id.is_none() {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            Some(turn_id)
        } else {
            None
        };
        let max_turns = state.max_turns;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist(&ctx);

        if let Some(turn_id) = turn_id {
            start_worker_turn_execution(&ctx, turn_id, max_turns, trusted_sandbox_manifest);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkerStatus>, HandlerError> {
        annotate_restate_handler_span("Worker", "status");
        Ok(Json::from(WorkerVoState::load_status_view(&ctx).await?))
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: informational fan-in read; mirrors `status` which exposes the same
    // VO projection without additional authz (the calling coordinator is already
    // authorized for the owning session before it fans in).
    async fn progress_summary(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkerProgressSummary>, HandlerError> {
        annotate_restate_handler_span("Worker", "progress_summary");
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
            .name("worker_progress_summary_now")
            .await?
            .into_inner();
        let stale_threshold_ms = self.session_limits.worker_heartbeat_stale_ms;
        Ok(Json::from(
            WorkerVoState::load_progress_summary(
                &ctx,
                ctx.key().to_string(),
                now,
                stale_threshold_ms,
            )
            .await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, at))]
    // SAFETY: internal telemetry-plane write invoked only by the child's own turn
    // workflow at the progress cadence; updates VO state only and appends no event.
    async fn record_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        at: Json<DateTime<Utc>>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_heartbeat");
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        state.last_heartbeat_at = Some(at.into_inner());
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn result(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Option<WorkerResult>>, HandlerError> {
        annotate_restate_handler_span("Worker", "result");
        let result = WorkerVoState::load_terminal_result(&ctx, ctx.key().to_string()).await?;
        Ok(Json::from(result))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "cancel");
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let active_turn_id = state.active_turn_id.clone();
        state.cancel_reason = Some(reason.clone());
        state.status = Some(WorkerState::Cancelled);
        let children = state
            .children
            .iter()
            .filter(|child| child.terminal.is_none())
            .cloned()
            .collect::<Vec<_>>();
        state.persist(&ctx);

        if let Some(turn_id) = active_turn_id {
            ctx.workflow_client::<WorkerTurnExecutionClient>(turn_id)
                .request_cancel(Json::from(reason.clone()))
                .send();
        }
        for child in children {
            ctx.object_client::<WorkerClient>(child.id)
                .cancel(reason.clone())
                .send();
        }
        tracing::info!(key = %ctx.key(), %reason, "worker cancel requested");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn prepare_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<WorkerTurnPreparation>, HandlerError> {
        annotate_restate_handler_span("Worker", "prepare_turn");
        Ok(Json::from(
            prepare_turn_inner(
                &mut ctx,
                &self.providers,
                &self.tool_schemas,
                &self.session_store,
            )
            .await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, response))]
    async fn record_response(
        &self,
        ctx: ObjectContext<'_>,
        response: Json<WorkerTurnResponseRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_response");
        let record = response.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale worker response"
            );
            return Ok(());
        }
        let response = record.response;
        let token_usage = response.token_usage();
        let token_cost = (token_usage.total_input_tokens() + token_usage.output_tokens) as u64;
        state.record_token_usage(token_cost);
        let parent_session = state.parent_session;
        state.last_turn_summary = summarize_response_text(&response);
        let mut appended = Vec::new();
        apply_response_to_history(&mut appended, &response);
        state
            .history
            .extend(appended.into_iter().map(WorkerHistoryEntry::inline));
        claim_check_worker_history(&ctx, &mut state, &self.session_store).await?;
        state.persist(&ctx);

        if let Some(parent_session) = parent_session
            && token_cost > 0
        {
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id: parent_session,
                    token_cost,
                }))
                .send();
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_tool_result(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_tool_result");
        record_tool_result_inner(
            &ctx,
            record.into_inner(),
            ToolRecordKind::Executed,
            &self.session_store,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_denied_tool(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_denied_tool");
        record_tool_result_inner(
            &ctx,
            record.into_inner(),
            ToolRecordKind::Denied,
            &self.session_store,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn apply_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<WorkerTurnOutcomeRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "apply_turn_outcome");
        let record = outcome.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale worker turn outcome"
            );
            return Ok(());
        }
        let outcome = record.outcome;
        if !matches!(
            (state.current_status(), outcome),
            (WorkerState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn attach_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachWorkerResultWaiterInput>,
    ) -> Result<Json<AttachWorkerResultWaiterOutput>, HandlerError> {
        annotate_restate_handler_span("Worker", "attach_result_waiter");
        let input = input.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if let Some(terminal) = state.terminal_result(ctx.key().to_string()) {
            return Ok(Json::from(AttachWorkerResultWaiterOutput {
                terminal: Some(terminal),
            }));
        }
        if state.add_result_waiter(input.awakeable_id) {
            state.persist(&ctx);
        }
        Ok(Json::from(AttachWorkerResultWaiterOutput {
            terminal: None,
        }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn remove_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveWorkerResultWaiterInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "remove_result_waiter");
        let input = input.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if state.remove_result_waiter(&input.awakeable_id) {
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: internal control-plane write invoked only by this child's own turn
    // workflow when the child model calls `request_input`. It records the awakeable id
    // backing the round-trip on the child's own VO state and reads no caller-owned data.
    async fn register_input_request(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<WorkerPendingInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "register_input_request");
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if state.register_input_request(input.into_inner()) {
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input_request_id))]
    // SAFETY: internal control-plane write invoked only by this child's own turn
    // workflow when its `request_input` wait times out. It clears the child's own pending
    // input mapping and reads no caller-owned data.
    async fn clear_input_request(
        &self,
        ctx: ObjectContext<'_>,
        input_request_id: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "clear_input_request");
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if state
            .take_input_awakeable(&input_request_id.into_inner())
            .is_some()
        {
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<moa_core::wire::turn::TurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let matches_active = state.clear_active_turn(&outcome.turn_id);
        if matches_active {
            if matches!(outcome.kind, TurnOutcomeKind::Failed) {
                state.status = Some(WorkerState::Failed);
                state.last_turn_summary = Some(outcome.message.clone());
            }
            state.last_outcome = Some(outcome.clone());
        }

        let should_restart = matches_active
            && !state.pending.is_empty()
            && !matches!(
                state.current_status(),
                WorkerState::Failed | WorkerState::Cancelled
            );
        let next_turn_id = if should_restart {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            Some(turn_id)
        } else {
            None
        };
        let max_turns = state.max_turns;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist(&ctx);

        if let Some(turn_id) = next_turn_id {
            start_worker_turn_execution(&ctx, turn_id, max_turns, trusted_sandbox_manifest);
            return Ok(());
        }
        maybe_resolve_parent_awakeable(&ctx, &self.session_limits).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "worker VO state cleared");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-call scheduled by this Worker VO's own
    // terminal-delivery path. It reads only this child's own VO state and writes only to
    // its own state (clear) plus the parent fan-out removal handler, which is itself an
    // established internal VO→VO write (register_child/remove_child/complete_child) on the
    // child's own parent. No caller-owned data is read back to a caller.
    async fn cleanup(
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
            crate::delegation::is_terminal_worker_state(state.current_status()),
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
enum CleanupDecision {
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
fn decide_cleanup(
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
async fn release_and_clear_worker(
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
        && let Err(error) = ctx
            .service_client::<ToolExecutorClient>()
            .release_worker_hands(Json::from(request))
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
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .remove_child(worker_id.clone())
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

async fn reschedule_cleanup(
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
fn release_worker_hands_request(
    parent_session: Option<SessionId>,
    worker_id: &str,
) -> Option<ReleaseWorkerHandsRequest> {
    parent_session.map(|session_id| ReleaseWorkerHandsRequest {
        session_id,
        worker_id: worker_id.to_string(),
    })
}

/// Issues one generation-guarded delayed self-call to `Worker/cleanup`.
fn schedule_cleanup_self_call(
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

fn generate_turn_id(ctx: &mut ObjectContext<'_>) -> String {
    let key = ctx.key().to_string();
    let id = ctx.rand_uuid();
    format!("{key}-turn-{id}")
}

async fn prepare_turn_inner(
    ctx: &mut ObjectContext<'_>,
    providers: &ProviderRegistry,
    tool_schemas: &[serde_json::Value],
    session_store: &Arc<dyn SessionRepository>,
) -> Result<WorkerTurnPreparation, HandlerError> {
    let mut state = Tracked::<WorkerVoState>::load(ctx).await?;
    if state.cancel_reason.is_some() {
        state.apply_turn_outcome(TurnOutcome::Cancelled);
        state.persist(ctx);
        return Ok(WorkerTurnPreparation::Outcome {
            outcome: TurnOutcome::Cancelled,
        });
    }
    if state.depth > MAX_WORKER_DEPTH {
        return Err(TerminalError::new(format!(
            "worker depth exceeds maximum ({MAX_WORKER_DEPTH})"
        ))
        .into());
    }
    state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;

    let pending = std::mem::take(&mut state.pending);
    for message in &pending {
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::user(
                render_user_message(message),
            )));
    }

    if state.budget_exhausted() {
        state.complete_after_budget_exhausted();
        state.persist(ctx);
        return Ok(WorkerTurnPreparation::Outcome {
            outcome: TurnOutcome::Idle,
        });
    }

    let parent_session = state
        .parent_session
        .ok_or_else(|| TerminalError::new("worker parent session missing"))?;
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("worker tenant_id missing"))?;
    let user_id = state
        .user_id
        .clone()
        .ok_or_else(|| TerminalError::new("worker user_id missing"))?;
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("worker model missing"))?;

    let mut request = build_completion_request(&state, providers, tool_schemas)?;
    extend_request_with_history(
        &*ctx,
        parent_session,
        &state.history,
        &mut request.messages,
        session_store,
    )
    .await?;
    let active_canary = if request.tools.is_empty() {
        None
    } else {
        let canary = new_canary_token();
        request
            .messages
            .push(ContextMessage::system(canary_system_message(&canary)));
        Some(canary)
    };
    request.metadata.insert(
        "_moa.session_id".to_string(),
        json!(parent_session.to_string()),
    );
    request
        .metadata
        .insert("_moa.tenant_id".to_string(), json!(tenant_id.to_string()));
    request
        .metadata
        .insert("_moa.contact_id".to_string(), json!(user_id.to_string()));
    request
        .metadata
        .insert("_moa.model".to_string(), json!(model.as_str()));
    request
        .metadata
        .insert("_moa.worker_id".to_string(), json!(ctx.key().to_string()));
    let session_meta = synthetic_session_meta(&state)?;
    state.persist(ctx);

    Ok(WorkerTurnPreparation::Request {
        request: Box::new(request),
        active_canary,
        session_meta: Box::new(session_meta),
        parent_session,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolRecordKind {
    Executed,
    Denied,
}

impl ToolRecordKind {
    fn counts_invocation(self) -> bool {
        matches!(self, Self::Executed)
    }
}

async fn record_tool_result_inner(
    ctx: &ObjectContext<'_>,
    record: WorkerToolRecord,
    kind: ToolRecordKind,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<(), HandlerError> {
    let mut state = Tracked::<WorkerVoState>::load(ctx).await?;
    if let Some(turn_id) = record.turn_id.as_deref()
        && !state.active_turn_matches(turn_id)
    {
        tracing::warn!(
            key = %ctx.key(),
            record_turn_id = %turn_id,
            active_turn_id = ?state.active_turn_id,
            "ignored stale worker tool result"
        );
        return Ok(());
    }
    state
        .history
        .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
            record
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| record.tool_id.0.to_string()),
            record.output.to_text(),
            Some(record.output.content.clone()),
        )));
    if kind.counts_invocation() {
        state.tools_invoked = state.tools_invoked.saturating_add(1);
    }
    claim_check_worker_history(ctx, &mut state, session_store).await?;
    state.persist(ctx);
    Ok(())
}

/// Offloads aged-out, over-threshold inline history entries to content-addressed blobs.
///
/// Runs after any append to `state.history`. The pure candidate selection keeps the
/// most-recent inline tail resident (no hydration on the hot path) and only offloads older
/// entries whose serialized body crosses the threshold. Each body is stored via
/// `store_text_artifact` inside a journaled `ctx.run`: the blob store is content-addressed,
/// so the recorded blob id is a deterministic function of the body and is reused verbatim on
/// replay. A worker without an owning session has no blob namespace, so its history stays
/// inline.
async fn claim_check_worker_history(
    ctx: &ObjectContext<'_>,
    state: &mut WorkerVoState,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<(), HandlerError> {
    let Some(session_id) = state.parent_session else {
        return Ok(());
    };
    for (idx, body) in state.history_entries_to_claim_check()? {
        let store = session_store.clone();
        let claim = ctx
            .run(|| async move {
                store
                    .store_text_artifact(session_id, &body)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name(format!("worker_history_claim_check_{idx}"))
            .await?
            .into_inner();
        state.claim_history_entry(idx, claim);
    }
    Ok(())
}

/// Appends the worker's buffered history to `out`, hydrating any claim-checked entries.
///
/// Inline entries are cloned directly; a `Claimed` entry's full body is read back from its
/// content-addressed blob and decoded into the original `ContextMessage`. The read is
/// journaled (rather than a bare content-addressed read) so the compiled turn request is
/// byte-identical on replay without re-touching the blob store.
async fn extend_request_with_history(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    history: &[WorkerHistoryEntry],
    out: &mut Vec<ContextMessage>,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<(), HandlerError> {
    for (idx, entry) in history.iter().enumerate() {
        match entry {
            WorkerHistoryEntry::Inline(message) => out.push(message.clone()),
            WorkerHistoryEntry::Claimed(claimed) => {
                out.push(
                    hydrate_claimed_history_entry(ctx, session_id, idx, claimed, session_store)
                        .await?,
                );
            }
        }
    }
    Ok(())
}

/// Reads one claim-checked history entry's full body back from its blob and decodes it.
async fn hydrate_claimed_history_entry(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    idx: usize,
    claimed: &ClaimedHistoryEntry,
    session_store: &Arc<dyn SessionRepository>,
) -> Result<ContextMessage, HandlerError> {
    let claim_check = ClaimCheck {
        blob_id: claimed.blob_id.clone(),
        size: claimed.size,
        preview: claimed.preview.clone(),
    };
    let store = session_store.clone();
    let body = ctx
        .run(|| async move {
            store
                .load_text_artifact(session_id, &claim_check)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name(format!("worker_history_hydrate_{idx}"))
        .await?
        .into_inner();
    serde_json::from_str(&body).map_err(|error| {
        HandlerError::from(TerminalError::new(format!(
            "failed to decode claimed worker history entry {}: {error}",
            claimed.blob_id
        )))
    })
}

fn start_worker_turn_execution(
    ctx: &ObjectContext<'_>,
    turn_id: String,
    max_turns: Option<u32>,
    trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
) {
    ctx.workflow_client::<WorkerTurnExecutionClient>(turn_id.clone())
        .run(Json::from(RunWorkerTurnRequest {
            worker_id: ctx.key().to_string(),
            turn_id,
            max_turns,
            trusted_sandbox_manifest,
        }))
        .send();
}

async fn maybe_resolve_parent_awakeable(
    ctx: &ObjectContext<'_>,
    session_limits: &SessionLimitsConfig,
) -> Result<(), HandlerError> {
    let mut state = Tracked::<WorkerVoState>::load(ctx).await?;
    let Some(terminal) = state.terminal_result(ctx.key().to_string()) else {
        return Ok(());
    };

    let delivered =
        deliver_terminal_notification_once(ctx, &mut state, terminal.clone(), session_limits)
            .await?;
    let waiters = state.take_result_waiters();
    let waiter_payload = if waiters.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&terminal).map_err(|error| {
            TerminalError::new(format!(
                "failed to serialize worker terminal result: {error}"
            ))
        })?)
    };
    for waiter in waiters {
        if let Some(payload) = waiter_payload.as_ref() {
            ctx.resolve_awakeable(&waiter, payload.clone());
        }
    }

    if delivered || waiter_payload.is_some() {
        state.persist(ctx);
    }
    Ok(())
}

async fn deliver_terminal_notification_once(
    ctx: &ObjectContext<'_>,
    state: &mut WorkerVoState,
    terminal: WorkerTerminalResult,
    session_limits: &SessionLimitsConfig,
) -> Result<bool, HandlerError> {
    if state.notification_delivered {
        return Ok(false);
    }

    let Some(parent_session) = state.parent_session else {
        return Ok(false);
    };
    let status = state.current_status();
    if !crate::delegation::is_terminal_worker_state(status) {
        return Ok(false);
    }

    let result = terminal.result.clone();
    // Captured before the events move `result.worker_id`, for the additive idle-wake.
    let wake_worker_id = result.worker_id.clone();
    let wake_summary = result
        .error
        .clone()
        .unwrap_or_else(|| result.output.clone());
    cache_parent_terminal_result(ctx, state, terminal).await?;
    persist_parent_session_event(
        ctx,
        parent_session,
        Event::WorkerStatusChanged {
            worker_id: result.worker_id.clone(),
            from: None,
            to: status,
            summary: state.last_turn_summary.clone(),
        },
    )
    .await?;
    persist_parent_session_event(
        ctx,
        parent_session,
        Event::WorkerNotificationDelivered {
            worker_id: result.worker_id,
            state: status,
            summary: result
                .error
                .clone()
                .unwrap_or_else(|| result.output.clone()),
        },
    )
    .await?;

    // Terminal idle-wake (additive control-plane wake; does NOT alter the three existing
    // channels or the `notification_delivered` guard). Lets a finished-while-idle child
    // wake its coordinator. The wake is idempotent via the terminal signal id's dedupe
    // key and non-fatal. `record_child_signal` performs the idle gate (active-turn
    // check), so a busy coordinator is never auto-resumed; a Failed terminal is
    // resume-eligible, a Completed/Cancelled terminal records as a non-resuming Finding.
    emit_terminal_idle_wake(ctx, parent_session, &wake_worker_id, status, wake_summary).await?;

    state.notification_delivered = true;

    // Report-then-self-clean: now that the result is durable on the parent (cache +
    // event log) and the idle-wake fired, schedule a generation-guarded delayed
    // self-cleanup. A follow-up arriving during the grace window bumps
    // `cleanup_generation` (in `post_message`), making this pending tick stale so the
    // child is revived instead of cleaned. The caller persists `state` after this
    // returns `true`, so the bumped generation is durable before the tick fires.
    let grace_ms = session_limits.worker_cleanup_grace_ms;
    if grace_ms > 0 {
        state.bump_cleanup_generation();
        let now = durable_utc_now(ctx).await?;
        schedule_cleanup_self_call(ctx, state.cleanup_generation, now, grace_ms);
    }

    Ok(true)
}

/// Sends the additive terminal idle-wake control-plane signal to the owning coordinator.
///
/// The signal id and timestamp are journaled via `ctx.run()` so the wake is replay-safe
/// and idempotent (the coordinator dedupes on `worker_signal:{signal_id}`). It is
/// dispatched detached (`.send()`) and never fails terminal delivery; the coordinator's
/// `record_child_signal` applies the idle gate, so this only ever wakes an *idle*
/// parent. A Failed terminal maps to a resume-eligible `Failed` signal; a successful or
/// cancelled terminal maps to an informational `Finding` that records the wake without
/// arming a resume (honoring "never resume on plain success").
async fn emit_terminal_idle_wake(
    ctx: &ObjectContext<'_>,
    parent_session: SessionId,
    worker_id: &str,
    status: WorkerState,
    summary: String,
) -> Result<(), HandlerError> {
    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("worker_terminal_wake_signal_id")
        .await?
        .into_inner();
    let created_at = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("worker_terminal_wake_at")
        .await?
        .into_inner();
    let (kind, severity) = if matches!(status, WorkerState::Failed) {
        (ChildSignalKind::Failed, SignalSeverity::Critical)
    } else {
        (ChildSignalKind::Finding, SignalSeverity::Info)
    };
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(WorkerSignal {
            signal_id,
            worker_id: worker_id.to_string(),
            parent_session,
            kind,
            severity,
            summary,
            payload: serde_json::Value::Null,
            created_at,
            resume_policy: ParentResumePolicy::IfIdle,
            input_request_id: None,
            input_audience: None,
        }))
        .send();
    Ok(())
}

async fn cache_parent_terminal_result(
    ctx: &ObjectContext<'_>,
    state: &WorkerVoState,
    terminal: WorkerTerminalResult,
) -> Result<(), HandlerError> {
    let input = MarkWorkerChildTerminalInput {
        worker_id: terminal.result.worker_id.clone(),
        terminal,
    };
    if let Some(parent_session) = state.parent_session {
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .mark_child_terminal(Json::from(input))
            .call()
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CleanupDecision, decide_cleanup, release_worker_hands_request};
    use moa_core::types::identifiers::SessionId;

    #[test]
    fn cleanup_release_request_targets_owning_session_and_child() {
        // Pins: a finishing worker's hand release is keyed by its OWNING session id
        // (where its hands were provisioned) and its own id, so cleanup frees exactly its
        // own scope; a child with no owning session issues no release.
        let session_id = SessionId::new();
        let request = release_worker_hands_request(Some(session_id), "sub-7")
            .expect("a child with a parent session releases its scoped hands");
        assert_eq!(request.session_id, session_id);
        assert_eq!(request.worker_id, "sub-7");
        assert!(
            release_worker_hands_request(None, "sub-7").is_none(),
            "a child with no owning session issues no hand release"
        );
    }

    #[test]
    fn cleanup_skips_on_stale_generation() {
        // Pins: a fired cleanup whose generation no longer matches (the child was revived
        // or rescheduled during the grace window) is a no-op, never tearing down.
        assert_eq!(
            decide_cleanup(false, true, false, true),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_skips_when_revived_to_non_terminal() {
        // Pins: a child that a follow-up revived back to Running is not terminal, so
        // cleanup must skip even when the generation still matches.
        assert_eq!(
            decide_cleanup(true, false, false, true),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_defers_while_non_terminal_child_exists() {
        // Pins: teardown is bottom-up; a terminal parent with a still-running child
        // reschedules rather than clearing.
        assert_eq!(
            decide_cleanup(true, true, true, true),
            CleanupDecision::Defer
        );
    }

    #[test]
    fn cleanup_skips_when_report_not_durable() {
        // Pins: the durable-report guard — cleanup never clears a terminal leaf whose
        // result was not yet recorded on the parent.
        assert_eq!(
            decide_cleanup(true, true, false, false),
            CleanupDecision::Skip
        );
    }

    #[test]
    fn cleanup_proceeds_on_durable_terminal_leaf() {
        // Pins: a terminal leaf with a durable report and a live generation is released.
        assert_eq!(
            decide_cleanup(true, true, false, true),
            CleanupDecision::Proceed
        );
    }
}

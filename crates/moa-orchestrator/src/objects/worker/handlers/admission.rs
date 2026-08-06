//! Worker admission, status, heartbeat, result, and cancellation handlers.

use super::*;

impl WorkerImpl {
    pub(super) async fn post_message(
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
                let reply = serde_json::Value::String(text.clone());
                let parent_session = state.parent_session;
                let (acknowledgement, applied) =
                    state.apply_input_reply(input_request_id, &reply)?;
                if let Some(applied) = applied {
                    ctx.resolve_awakeable(&applied.awakeable_id, text.clone());
                    state.persist(&ctx);
                    // An answered round-trip is no longer user-addressable, so the
                    // advertised target is retracted on the same path as every other clear.
                    if let Some(parent_session) = parent_session {
                        retract_session_input_targets(&ctx, parent_session, vec![applied.target()]);
                    }
                    tracing::info!(
                        key = %ctx.key(),
                        input_request_id = %input_request_id,
                        "resolved worker input request awakeable"
                    );
                } else if acknowledgement == UserReplyDeliveryAck::Conflict {
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
        // New parent instructions supersede every action review this worker raised
        // under an older generation, so a late approval cannot preempt them.
        let generation = state.advance_generation();
        let turn_id = if state.active_turn_id.is_none() {
            let turn_id = generate_turn_id(&mut ctx);
            let _started = state.start_workflow_turn(turn_id.clone());
            activate_worker_security_owner(&mut state, ctx.key(), &turn_id, generation);
            Some(turn_id)
        } else {
            None
        };
        let max_turns = state.max_turns;
        let identity = state
            .identity
            .clone()
            .ok_or_else(|| TerminalError::new("worker is missing its admitted caller identity"))?;
        let parent_session = required_parent_session(&state)?;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist(&ctx);

        if let Some(turn_id) = turn_id {
            start_worker_turn_execution(
                &ctx,
                WorkerTurnDispatch {
                    turn_id,
                    identity,
                    parent_session,
                    generation,
                    max_turns,
                    trusted_sandbox_manifest,
                    action_review: None,
                },
            );
        }
        Ok(())
    }

    pub(super) async fn provide_input(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<WorkerProvideInputRequest>,
    ) -> Result<Json<UserReplyDeliveryAck>, HandlerError> {
        annotate_restate_handler_span("Worker", "provide_input");
        let input = input.into_inner();
        require_identity(&ctx)?;
        self.authz
            .authorize_object_session_participant(&ctx, input.parent_session)
            .await?;
        let text = input
            .input
            .as_str()
            .ok_or_else(|| TerminalError::new_with_code(422, "worker input must be a string"))?
            .to_string();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        state.ensure_parent_session_scope(input.parent_session)?;
        // No target retraction here: the caller IS the owning Session, which clears the
        // target it advertised from this handler's acknowledgement. Calling back into
        // its single-writer queue while it waits on this call would deadlock.
        let (acknowledgement, applied) =
            state.apply_user_input_reply(&input.target, &input.input)?;
        if let Some(applied) = applied {
            ctx.resolve_awakeable(&applied.awakeable_id, text);
            state.persist(&ctx);
        }
        Ok(Json::from(acknowledgement))
    }

    pub(super) async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkerStatus>, HandlerError> {
        annotate_restate_handler_span("Worker", "status");
        Ok(Json::from(WorkerVoState::load_status_view(&ctx).await?))
    }

    // SAFETY: informational fan-in read; mirrors `status` which exposes the same
    // VO projection without additional authz (the calling coordinator is already
    // authorized for the owning session before it fans in).
    pub(super) async fn progress_summary(
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

    // SAFETY: internal telemetry-plane write invoked only by the child's own turn
    // workflow at the progress cadence; updates VO state only and appends no event.
    pub(super) async fn record_heartbeat(
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

    pub(super) async fn result(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Option<WorkerResult>>, HandlerError> {
        annotate_restate_handler_span("Worker", "result");
        let result = WorkerVoState::load_terminal_result(&ctx, ctx.key().to_string()).await?;
        Ok(Json::from(result))
    }

    pub(super) async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: String,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "cancel");
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let active_turn_id = state.active_turn_id.clone();
        let parent_session = state.parent_session;
        state.cancel_reason = Some(reason.clone());
        // An active turn owns the terminal transition. Keeping the worker
        // nonterminal until that workflow has fenced and joined its children
        // prevents status readers and parent notification from outrunning
        // provider cancellation. An idle worker has no workflow left to report
        // the outcome, so it can become terminal here.
        if active_turn_id.is_none() {
            state.status = Some(WorkerState::Cancelled);
        }
        // Nothing will resolve this child's awakeables once it is cancelled, so every
        // in-flight round-trip is dropped and its advertised reply target retracted.
        let cleared_inputs = state.clear_all_input_requests();
        // A cancelled worker runs no continuation: the tree it belongs to is being
        // torn down, so every held review is released instead of resumed.
        let discarded_reviews = state.discard_action_reviews();
        if discarded_reviews > 0 {
            tracing::info!(
                key = %ctx.key(),
                discarded_reviews,
                "released held action reviews on worker cancellation"
            );
        }
        let children = state
            .children
            .iter()
            .filter(|child| child.terminal.is_none())
            .cloned()
            .collect::<Vec<_>>();
        state.persist(&ctx);

        if let Some(parent_session) = parent_session {
            retract_session_input_targets(
                &ctx,
                parent_session,
                cleared_inputs
                    .iter()
                    .map(WorkerPendingInput::target)
                    .collect(),
            );
        }
        if let Some(turn_id) = active_turn_id {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<WorkerTurnExecutionClient>(turn_id)
                    .request_cancel(Json::from(reason.clone())),
            )
            .call()
            .await?;
        }
        for child in children {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(child.id)
                    .cancel(reason.clone()),
            )
            .call()
            .await?;
        }
        tracing::info!(key = %ctx.key(), %reason, "worker cancel requested");
        Ok(())
    }
}

pub(super) fn generate_turn_id(ctx: &mut ObjectContext<'_>) -> String {
    let key = ctx.key().to_string();
    let id = ctx.rand_uuid();
    format!("{key}-turn-{id}")
}

/// Returns the worker's owning session, or a terminal error when it has none.
///
/// Every worker turn request carries its parent session so a turn that fails
/// before preparing its first iteration can still append the parent-session
/// facts. A worker without a recorded parent cannot be dispatched, and the
/// missing value is never inferred.
pub(super) fn required_parent_session(state: &WorkerVoState) -> Result<SessionId, HandlerError> {
    state.parent_session.ok_or_else(|| {
        TerminalError::new("worker is missing its owning parent session and cannot run a turn")
            .into()
    })
}

/// Everything one worker turn dispatch needs from the VO.
pub(super) struct WorkerTurnDispatch {
    /// Stable workflow key for the turn.
    pub(super) turn_id: String,
    /// Exact identity inherited from the root turn.
    pub(super) identity: moa_core::traits::Identity,
    /// Owning session.
    pub(super) parent_session: SessionId,
    /// Worker generation admitting the turn.
    pub(super) generation: u64,
    /// Optional per-turn iteration cap.
    pub(super) max_turns: Option<u32>,
    /// Trusted sandbox manifest inherited from the root turn.
    pub(super) trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    /// Resolved review this turn continues, when it is a continuation turn.
    pub(super) action_review: Option<ActionReviewContinuation>,
}

pub(super) fn start_worker_turn_execution(ctx: &ObjectContext<'_>, dispatch: WorkerTurnDispatch) {
    crate::restate_identity::replay_safe_request(
        ctx.workflow_client::<WorkerTurnExecutionClient>(dispatch.turn_id.clone())
            .run(Json::from(RunWorkerTurnRequest {
                worker_id: ctx.key().to_string(),
                turn_id: dispatch.turn_id,
                identity: dispatch.identity,
                parent_session: dispatch.parent_session,
                generation: dispatch.generation,
                max_turns: dispatch.max_turns,
                trusted_sandbox_manifest: dispatch.trusted_sandbox_manifest,
                action_review: dispatch.action_review,
            })),
    )
    .send();
}

/// Installs the worker circuit owner as part of admitting a turn.
pub(super) fn activate_worker_security_owner(
    state: &mut WorkerVoState,
    worker_id: &str,
    turn_id: &str,
    generation: u64,
) {
    state
        .security_circuit
        .adopt_owner(&moa_core::types::security::SecurityCircuitOwner::Worker {
            worker_id: worker_id.to_string(),
            turn_id: turn_id.to_string(),
            generation,
        });
}

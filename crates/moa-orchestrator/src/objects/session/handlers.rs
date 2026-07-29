//! Restate handlers for the Session VO.

use super::execution_runs::{
    accept_execution_input_required, accept_execution_progress, accept_execution_run_started,
    accept_execution_terminal, admit_execution_template, dispatch_execution_run,
};
use super::state::signal_kind_is_resume_eligible;
use super::*;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::tool_executor::{ReleaseSessionHandsRequest, ToolExecutorClient};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    // SAFETY: internal SessionStore initialization only; mirrors persisted session metadata into VO hot state.
    async fn set_meta(
        &self,
        ctx: ObjectContext<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "set_meta");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        state.set_meta(meta.into_inner());
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, scope))]
    async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        scope: Json<CancelScope>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "cancel");
        let session_id = parse_session_key(ctx.key())?;
        let identity = require_session_participant(&ctx, session_id).await?;
        let scope = scope.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let meta = state
            .ensure_initialized()
            .map_err(moa_error_to_handler_error)?
            .clone();
        let children = state.children.clone();
        let active_execution_run_uids = state
            .active_execution_runs
            .iter()
            .map(|run| run.run_uid)
            .collect::<Vec<_>>();
        // A cancelled task tree can answer nothing, so every reply target its children
        // advertised is retracted here rather than waiting on each child's own clear:
        // the cascade below is detached, and until it lands the next plain user message
        // would be delivered to a round-trip that is already being torn down.
        if scope.cancels_task_tree() {
            for child in &children {
                state.clear_worker_input_targets_for_worker(&child.id);
            }
        }
        state.persist(&ctx);

        let mut pending_state = load_pending_state(&ctx).await?;
        let active_turn_id = pending_state.active_turn_id.clone();
        // Remember the scope against the turn it cancels. `record_turn_outcome`
        // needs it to decide the queue's disposition, and it is what releases the
        // admission fence below. Without an active turn there is no callback to
        // wait for, so there is nothing to fence and nothing to remember.
        if let Some(turn_id) = active_turn_id.clone() {
            pending_state.pending_cancellation = Some(PendingCancellation { turn_id, scope });
        }
        // A whole-task-tree cancellation discards every already-accepted queued
        // message. Each was acknowledged to its sender, so each gets one durable
        // rejection fact, in queue order, before the queue is drained here rather
        // than at some later callback.
        if scope.cancels_task_tree() {
            let mut admissions = load_message_admissions(&ctx).await?;
            let now = durable_utc_now(&ctx).await?;
            let rejected = reject_queued_messages(
                &ctx,
                session_id,
                &mut pending_state,
                &mut admissions,
                active_turn_id.as_deref(),
                now,
            )
            .await?;
            if rejected > 0 {
                persist_message_admissions(&ctx, &admissions, 0);
                tracing::info!(
                    key = %ctx.key(),
                    rejected,
                    "rejected and drained queued messages for a cancelled task tree"
                );
            }
        }
        persist_pending_state(&ctx, &pending_state);

        // Both scopes cancel the active coordinator turn.
        if let Some(turn_id) = active_turn_id {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<TurnExecutionClient>(turn_id)
                    .request_cancel(Json::from("session cancel requested".to_string())),
            )
            .send();
        }
        // Only `TaskTree` cascades to the registered children; `CoordinatorOnly` leaves them running.
        if scope.cancels_task_tree() {
            for child in children {
                crate::restate_identity::replay_safe_request(
                    ctx.object_client::<WorkerClient>(child.id)
                        .cancel("parent session cancelled".to_string()),
                )
                .send();
            }
            for run_uid in active_execution_run_uids {
                let call = ctx.service_client::<ExecutionClient>().cancel(Json::from(
                    moa_execution::wire::ExecutionCancelRequest {
                        run: moa_execution::wire::ExecutionRunRequest {
                            tenant_id: meta.tenant_id,
                            contact_id: meta.contact.as_ref().map(|contact| contact.contact_id),
                            session_id,
                            run_uid,
                        },
                        reason: "parent session cancelled".to_string(),
                    },
                ));
                match with_identity_headers(call, &identity)
                    .call()
                    .await?
                    .into_inner()
                {
                    moa_execution::wire::ExecutionMutationResponse::Applied { .. }
                    | moa_execution::wire::ExecutionMutationResponse::Replayed { .. }
                    | moa_execution::wire::ExecutionMutationResponse::Conflict { .. }
                    | moa_execution::wire::ExecutionMutationResponse::NotFound => {}
                }
            }
        }
        tracing::info!(scope = ?scope, key = %ctx.key(), "session cancel requested");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "status");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        Ok(Json::from(SessionVoState::load_status(&ctx).await?))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start_turn(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "start_turn");
        Ok(Json::from(
            start_turn_inner(
                &mut ctx,
                request.into_inner(),
                &self.session_limits,
                &self.turn_admission,
            )
            .await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<ExecutionTurnOutcome>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let session_id = parse_session_key(ctx.key())?;

        // The admission that started this turn is resolved by the turn's terminal
        // disposition, whether or not the turn is still the active one. Its guarantee
        // window opens here, not when admission returned, so a caller retrying while
        // the turn was still running always saw the original response.
        let mut admissions = load_message_admissions(&ctx).await?;
        let terminal_at = durable_utc_now(&ctx).await?;
        let (resolved_admission, evicted) =
            admissions.mark_terminal_for_turn(&outcome.turn_id, terminal_at);
        if resolved_admission {
            persist_message_admissions(&ctx, &admissions, evicted);
        }

        // A stale or replayed callback for a turn that is no longer active is a
        // complete no-op: it must not overwrite the terminal outcome or summary a
        // newer turn already published, and it must not touch a newer active turn.
        // Only its own waiters resolve, because those are keyed by the turn it
        // actually belongs to. This runs before any validation so a duplicate
        // delivery can never be turned into a retryable error.
        if pending_state.active_turn_id.as_deref() != Some(outcome.turn_id.as_str()) {
            let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
            if !turn_waiters.is_empty() {
                resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
                persist_pending_state(&ctx, &pending_state);
            }
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                active_turn_id = ?pending_state.active_turn_id,
                "ignored turn outcome for a turn that is no longer active"
            );
            return Ok(());
        }

        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        if let ExecutionTurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind
            && !state
                .active_execution_runs
                .iter()
                .any(|marker| marker.run_uid == execution_run_uid)
        {
            return Err(TerminalError::new_with_code(
                409,
                "accepted turn outcome requires a matching active execution run marker",
            )
            .into());
        }

        let dispatch_next = pending_state.dispatches_next_after(&outcome);
        // The cancellation fence is released only now, by the outcome it was waiting
        // for, so nothing can be admitted while the cancelled turn is still winding
        // down.
        let cleared_cancellation = pending_state
            .pending_cancellation
            .take_if(|cancellation| cancellation.turn_id == outcome.turn_id)
            .is_some();
        if cleared_cancellation {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared cancellation fence on its matching turn outcome"
            );
        }

        pending_state.active_turn_id = None;
        pending_state.last_outcome = Some(outcome.clone());
        let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
        state.last_turn_summary = Some(outcome.message.clone());

        // When the completing turn was the guarded coordinator resume turn, clear the
        // pending-resume marker and drain exactly its dispatch-time unread snapshot
        // (signals that arrived mid-turn stay queued for the next resume). Only the
        // active turn reaches here, and every branch below persists `state`.
        if state.clear_resume_on_outcome(&outcome.turn_id) {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared pending parent resume and drained dispatch-time signal snapshot"
            );
        }
        // A failed or cancelled origin turn ends the work the review belonged to, so
        // its continuation is stale: resuming into a turn that just died would answer
        // for work the session already gave up on.
        let continuation_eligible = matches!(
            outcome.kind,
            ExecutionTurnOutcomeKind::Completed | ExecutionTurnOutcomeKind::Accepted { .. }
        );
        if !continuation_eligible {
            let discarded = pending_state.action_reviews.discard_all();
            if discarded > 0 {
                tracing::info!(
                    key = %ctx.key(),
                    turn_id = %outcome.turn_id,
                    discarded,
                    "released action reviews held by a turn that did not complete"
                );
            }
        }

        // A resolved same-generation continuation runs before ordinary FIFO: it is
        // the tail of work the session already told the user it was doing. It is only
        // eligible while it is still current — a newer admission advanced the
        // generation and already discarded it.
        if continuation_eligible
            && let Some(queued) = pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        {
            pending_state.active_turn_id = Some(queued.turn_id.clone());
            activate_coordinator_security_owner(&mut state, &queued.turn_id, queued.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
            let identity = state.owning_identity.clone().ok_or_else(|| {
                TerminalError::new("session has no owning identity for an action review resume")
            })?;
            let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
            resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    queued.turn_id,
                    identity,
                    contact,
                    queued.generation,
                    queued.continuation,
                ),
            );
            return Ok(());
        }

        if dispatch_next && let Some(next) = pending_state.pending_messages.pop_front() {
            let next_turn_id = generate_turn_id(&mut ctx);
            pending_state.active_turn_id = Some(next_turn_id.clone());
            activate_coordinator_security_owner(&mut state, &next_turn_id, next.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
            // The dequeued message's admission now owns a running turn. Its recorded
            // response still says `queued`, which is exactly what its caller was told.
            if admissions.mark_running(&next.client_message_id, &next_turn_id) {
                persist_message_admissions(&ctx, &admissions, 0);
            }
            let drained = state.drain_unread_child_signals();
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
            resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
            if drained > 0 {
                tracing::debug!(
                    key = %ctx.key(),
                    turn_id = %next_turn_id,
                    drained,
                    "drained queued child signals into next coordinator turn"
                );
            }
            dispatch_turn_execution(
                &ctx,
                RunTurnRequest {
                    session_id: ctx.key().to_string(),
                    turn_id: next_turn_id,
                    identity: next.identity,
                    contact: next.contact,
                    generation: next.generation,
                    user_message: next.user_message,
                    attachments: next.attachments,
                    model: next.model,
                    max_turns: next.max_turns,
                    trigger: TurnTrigger::UserMessage,
                    child_signal_id: None,
                    execution_template: next.execution_template,
                    action_review: None,
                },
            );
            return Ok(());
        }

        {
            let now = durable_utc_now(&ctx).await?;
            let resumed = if matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed) {
                dispatch_queued_parent_resume_if_idle(
                    &mut ctx,
                    &mut pending_state,
                    &mut state,
                    session_id,
                    now,
                    &self.session_limits,
                )
                .await?
            } else {
                false
            };
            if !resumed {
                match outcome.kind {
                    ExecutionTurnOutcomeKind::Completed => {
                        // Compute the branch before the `&mut self` call so the
                        // immutable read does not overlap the mutable borrow
                        // (the `Tracked` deref forgoes two-phase borrows).
                        let completed_status = if !state.active_execution_runs.is_empty() {
                            SessionStatus::Running
                        } else {
                            SessionStatus::Paused
                        };
                        state.set_status(completed_status, now)
                    }
                    ExecutionTurnOutcomeKind::Cancelled => {
                        state.set_status(SessionStatus::Cancelled, now)
                    }
                    ExecutionTurnOutcomeKind::Failed => {
                        state.set_status(SessionStatus::Failed, now)
                    }
                    ExecutionTurnOutcomeKind::Accepted { .. } => {
                        state.apply_accepted_execution_turn(now)
                    }
                }
            }
            state.persist(&ctx);
            sync_status(&ctx, session_id, &state).await?;
        }
        if pending_state.active_turn_id.is_none() {
            let tenant_id = state
                .ensure_initialized()
                .map_err(moa_error_to_handler_error)?
                .tenant_id;
            self.turn_admission
                .release(&ctx, session_id, tenant_id)
                .await?;
        }
        persist_pending_state(&ctx, &pending_state);
        resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, registration))]
    // SAFETY: internal control-plane write from `ActionReviews/request`, which runs
    // only after this session admitted the caller and its own coordinator turn issued
    // the reviewed tool call. It records the review id on this session's own VO state
    // and returns no caller-owned data.
    async fn register_action_review(
        &self,
        ctx: ObjectContext<'_>,
        registration: Json<moa_core::types::action_policy::ActionReviewRegistration>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "register_action_review");
        let registration = registration.into_inner();
        let turn_id = registration
            .owner
            .turn_id()
            .ok_or_else(|| {
                TerminalError::new("session action review registration requires an owning turn")
            })?
            .to_string();
        // The generation comes from the owner that issued the tool call, not from
        // whatever the session happens to be on now. Reading "now" would let a user
        // message admitted between the tool call and this registration re-stamp a stale
        // review as current, and the fence would then resume superseded work.
        let generation = registration.owner.generation().ok_or_else(|| {
            TerminalError::new("session action review registration requires an owner generation")
        })?;
        let mut pending_state = load_pending_state(&ctx).await?;
        if generation < pending_state.turn_generation {
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation,
                current_generation = pending_state.turn_generation,
                "skipped registering an already-superseded session action review"
            );
            return Ok(());
        }
        if pending_state
            .action_reviews
            .register(registration.review_id, turn_id, generation)
        {
            persist_pending_state(&ctx, &pending_state);
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation,
                "registered pending action review on session"
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, receipt))]
    // SAFETY: internal control-plane write from `ActionReviews/decide`, which
    // authorizes the deciding tenant admin before resolving. It writes only this
    // session's own VO state and event log.
    async fn action_review_resolved(
        &self,
        mut ctx: ObjectContext<'_>,
        receipt: Json<moa_core::types::action_policy::ActionReviewReceipt>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "action_review_resolved");
        let receipt = receipt.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut pending_state = load_pending_state(&ctx).await?;
        // Unknown or already-resolved review: a duplicated callback changes nothing.
        let Some(registered) = pending_state.action_reviews.resolve(receipt.review_id) else {
            tracing::debug!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                "ignored resolution for an unknown or already-resolved session action review"
            );
            return Ok(());
        };
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let cancelling = pending_state.task_tree_cancellation_fenced();
        let terminal_session = matches!(
            state.status,
            Some(SessionStatus::Cancelled) | Some(SessionStatus::Failed)
        );
        if registered.generation != pending_state.turn_generation || cancelling || terminal_session
        {
            persist_pending_state(&ctx, &pending_state);
            tracing::info!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                registered_generation = registered.generation,
                current_generation = pending_state.turn_generation,
                cancelling,
                terminal_session,
                "dropped superseded or cancelled session action review continuation"
            );
            return Ok(());
        }

        let Some(identity) = state.owning_identity.clone() else {
            persist_pending_state(&ctx, &pending_state);
            tracing::warn!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                "session action review resolved but no owning identity is recorded"
            );
            return Ok(());
        };
        let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
        // Minted before any scheduling decision so the durable continuation fact names
        // the exact turn that will run it, even when it has to wait behind the origin.
        let continuation_turn_id = generate_turn_id(&mut ctx);
        let entry = QueuedActionReviewContinuation {
            continuation: moa_core::types::action_policy::ActionReviewContinuation { receipt },
            turn_id: continuation_turn_id,
            generation: registered.generation,
            ordinal: registered.ordinal,
        };
        let fact = entry.clone();
        if !pending_state.action_reviews.enqueue(entry) {
            persist_pending_state(&ctx, &pending_state);
            return Ok(());
        }
        let dispatch = if pending_state.active_turn_id.is_some() {
            None
        } else {
            pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        };
        if let Some(dispatch) = dispatch.as_ref() {
            pending_state.active_turn_id = Some(dispatch.turn_id.clone());
            activate_coordinator_security_owner(&mut state, &dispatch.turn_id, dispatch.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
        }
        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        sync_status(&ctx, session_id, &state).await?;

        append_session_event_deduped(
            &ctx,
            session_id,
            Event::ActionReviewContinuationRequested {
                review_id: fact.continuation.receipt.review_id,
                turn_id: fact.turn_id.clone(),
                receipt: fact.continuation.receipt.clone(),
            },
            moa_core::types::action_policy::action_review_continuation_dedupe_key(
                fact.continuation.receipt.review_id,
            ),
        )
        .await?;

        if let Some(dispatch) = dispatch {
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    dispatch.turn_id,
                    identity,
                    contact,
                    dispatch.generation,
                    dispatch.continuation,
                ),
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, release))]
    // SAFETY: internal control-plane release from the action-review reaper or
    // security circuit. It removes only the matching review registration owned by
    // this session and does not create a continuation for that review.
    async fn release_action_review(
        &self,
        ctx: ObjectContext<'_>,
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "release_action_review");
        let release = release.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        if !matches!(
            &release.owner,
            moa_core::types::action_policy::ActionReviewOwner::Coordinator {
                session_id: owner_session,
                ..
            } if *owner_session == session_id
        ) {
            return Err(TerminalError::new(
                "action review release does not belong to this session coordinator",
            )
            .into());
        }
        let mut pending_state = load_pending_state(&ctx).await?;
        if pending_state
            .action_reviews
            .resolve(release.review_id)
            .is_none()
        {
            return Ok(());
        }
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let identity = state.owning_identity.clone();
        let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
        let can_dispatch = release.resume_queued
            && pending_state.active_turn_id.is_none()
            && !pending_state.task_tree_cancellation_fenced()
            && !matches!(
                state.status,
                Some(SessionStatus::Cancelled) | Some(SessionStatus::Failed)
            )
            && identity.is_some();
        let dispatch = if can_dispatch {
            pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        } else {
            None
        };
        if let Some(dispatch) = dispatch.as_ref() {
            pending_state.active_turn_id = Some(dispatch.turn_id.clone());
            activate_coordinator_security_owner(&mut state, &dispatch.turn_id, dispatch.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
        }
        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        sync_status(&ctx, session_id, &state).await?;
        if let (Some(dispatch), Some(identity)) = (dispatch, identity) {
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    dispatch.turn_id,
                    identity,
                    contact,
                    dispatch.generation,
                    dispatch.continuation,
                ),
            );
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, delivery))]
    // SAFETY: internal TurnExecution delivery after Execution/start has committed the run.
    async fn execution_run_started(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<ExecutionRunStartedDelivery>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "execution_run_started");
        let identity = require_identity(&ctx)?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let delivery = delivery.into_inner();
        let run_uid = delivery.started.run_uid;
        let originating_user_sequence_num = delivery.started.originating_user_sequence_num;
        tracing_opentelemetry::OpenTelemetrySpanExt::set_attribute(
            &tracing::Span::current(),
            "moa.execution.run_uid",
            run_uid.to_string(),
        );
        accept_execution_run_started(&ctx, &mut state, delivery.started, delivery.approved_budget)
            .await?;
        let terminal_replay = state
            .execution_synthesis_marker(run_uid, originating_user_sequence_num)
            .is_some();
        if !terminal_replay {
            state.apply_accepted_execution_turn(durable_utc_now(&ctx).await?);
        }
        state.persist(&ctx);
        if !terminal_replay {
            let session_id = parse_session_key(ctx.key())?;
            sync_status(&ctx, session_id, &state).await?;
            dispatch_execution_run(&ctx, &state, run_uid, identity)?;
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn admit_execution_template(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<moa_execution::wire::ExecutionTemplateAdmissionRequest>,
    ) -> Result<Json<moa_execution::wire::ExecutionTemplateAdmissionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "admit_execution_template");
        let request = request.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        if request.session_id != session_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution-template admission request targets a different Session",
            )
            .into());
        }
        let identity = require_session_participant(&ctx, session_id).await?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let response = admit_execution_template(
            &mut ctx,
            &mut state,
            self.session_store_backend.clone(),
            &self.admission_pool,
            self.config.as_ref(),
            &identity,
            request,
        )
        .await?;
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, progress))]
    // SAFETY: internal ExecutionRun delivery after execution persistence committed the aggregate.
    async fn execution_progress(
        &self,
        ctx: ObjectContext<'_>,
        progress: Json<ExecutionProgress>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "execution_progress");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        accept_execution_progress(
            &ctx,
            &mut state,
            progress.into_inner(),
            self.session_limits.progress_interval_ms,
        )
        .await?;
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal registration by this session's own running turn; it stores an
    // awakeable id and a pending reply target and reads no caller-owned data back.
    async fn register_coordinator_input(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<RegisterCoordinatorInputRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "register_coordinator_input");
        let request = request.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;

        state.register_coordinator_input(CoordinatorPendingInput {
            turn_id: request.turn_id.clone(),
            generation: request.generation,
            input_request_id: request.input_request_id.clone(),
            awakeable_id: request.awakeable_id,
            waiting_workflow_id: request.waiting_workflow_id,
        });
        // Advertising the pending target is what lets an unaddressed plain reply be
        // routed here instead of starting an ordinary turn behind the blocked one.
        state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
            turn_id: request.turn_id,
            generation: request.generation,
            input_request_id: request.input_request_id.clone(),
        });
        append_session_event_deduped(
            &ctx,
            session_id,
            Event::Warning {
                message: request.question,
            },
            format!("coordinator_input_request:{}", request.input_request_id),
        )
        .await?;
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal workflow delivery of an assessment the router already produced;
    // it reads no caller-owned data back and returns only closed-vocabulary state.
    async fn apply_security_assessment(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ApplySecurityAssessmentRequest>,
    ) -> Result<Json<ApplySecurityAssessmentResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "apply_security_assessment");
        let request = request.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let transition = moa_security::apply_owner_assessment(
            &mut state.security_circuit,
            moa_security::CircuitTarget {
                session_id,
                owner: &request.owner,
                capability: &request.capability,
                tool_call_id: request.tool_call_id,
            },
            &request.assessment,
        );
        let transition = match transition {
            Ok(transition) => transition,
            Err(error)
                if request.allow_superseded_owner_noop
                    && matches!(
                        (error.active.as_ref(), &error.received),
                        (
                            Some(
                                moa_core::types::security::SecurityCircuitOwner::Coordinator {
                                    generation: active_generation,
                                    ..
                                }
                            ),
                            moa_core::types::security::SecurityCircuitOwner::Coordinator {
                                generation: received_generation,
                                ..
                            }
                        ) if active_generation > received_generation
                    ) =>
            {
                tracing::info!(
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_generation = error.received.generation(),
                    "discarded superseded reviewed session security assessment"
                );
                return Ok(Json::from(ApplySecurityAssessmentResponse {
                    transition: None,
                    stage: moa_core::types::security::SecurityCircuitStage::Clear,
                }));
            }
            Err(error) => {
                tracing::warn!(
                    active_owner_kind = error.active.as_ref().map(|owner| owner.kind()),
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_kind = error.received.kind(),
                    received_owner_generation = error.received.generation(),
                    "rejected stale session security assessment"
                );
                return Err(TerminalError::new_with_code(
                    409,
                    "security assessment owner is no longer active",
                )
                .into());
            }
        };
        let stage = state
            .security_circuit
            .stage(&request.owner, &request.capability);
        state.persist(&ctx);
        Ok(Json::from(ApplySecurityAssessmentResponse {
            transition,
            stage,
        }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: internal ExecutionRun delivery after a task persisted exact user-audience input.
    async fn execution_input_required(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ExecutionInputRequired>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "execution_input_required");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        accept_execution_input_required(&ctx, &mut state, input.into_inner()).await?;
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, delivery))]
    // SAFETY: internal ExecutionRun delivery after the terminal run and task projection are durable.
    async fn execution_terminal(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<moa_execution::wire::ExecutionTerminalDelivery>,
    ) -> Result<Json<ExecutionSynthesisDispatch>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "execution_terminal");
        let delivery = delivery.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let run_uid = delivery.summary.run_uid;
        let origin = delivery.summary.originating_user_sequence_num;
        if let Some(existing) = state.execution_synthesis_marker(run_uid, origin) {
            return Ok(Json::from(ExecutionSynthesisDispatch {
                run_uid,
                originating_user_sequence_num: origin,
                turn_id: existing.turn_id.clone(),
            }));
        }

        let requested = accept_execution_terminal(&ctx, &state, delivery).await?;
        let meta = state
            .ensure_initialized()
            .map_err(moa_error_to_handler_error)?
            .clone();
        let identity = state.owning_identity.clone().ok_or_else(|| {
            TerminalError::new_with_code(
                409,
                "execution synthesis requires the session owning identity",
            )
        })?;
        let session_id = parse_session_key(ctx.key())?;
        let evidence_call = ctx
            .service_client::<ExecutionClient>()
            .synthesis_evidence(Json::from(
                moa_execution::wire::ExecutionSynthesisEvidenceRequest {
                    run: moa_execution::wire::ExecutionRunRequest {
                        tenant_id: meta.tenant_id,
                        contact_id: meta.contact.as_ref().map(|contact| contact.contact_id),
                        session_id,
                        run_uid,
                    },
                    originating_user_sequence_num: origin,
                },
            ));
        let evidence = with_identity_headers(evidence_call, &identity)
            .call()
            .await?
            .into_inner();
        let evidence_json = serde_json::to_string(&evidence).map_err(|error| {
            TerminalError::new(format!(
                "failed to encode execution synthesis evidence: {error}"
            ))
        })?;
        let instruction = format!(
            "Synthesize the final user response for execution run {run_uid} from the durable \
             <execution_synthesis> event and this internally authorized evidence: \
             <execution_run_evidence>{evidence_json}</execution_run_evidence>. Do not start a new \
             execution run and do not reproduce raw task-table outputs."
        );

        let mut pending_state = load_pending_state(&ctx).await?;
        self.turn_admission
            .acquire(
                &ctx,
                session_id,
                meta.tenant_id,
                "turn_admission_execution_synthesis",
            )
            .await?;
        pending_state.active_turn_id = Some(requested.turn_id.clone());
        activate_coordinator_security_owner(
            &mut state,
            &requested.turn_id,
            pending_state.turn_generation,
        );
        arm_turn_admission_heartbeat(&ctx, &mut pending_state, &self.turn_admission);
        let now = durable_utc_now(&ctx).await?;
        state.set_status(SessionStatus::Running, now);
        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        sync_status(&ctx, session_id, &state).await?;

        dispatch_turn_execution(
            &ctx,
            RunTurnRequest {
                session_id: ctx.key().to_string(),
                turn_id: requested.turn_id.clone(),
                identity,
                contact: meta.contact,
                // A system-triggered resume continues the work the current
                // generation already admitted; it is not a new user admission.
                generation: pending_state.turn_generation,
                user_message: instruction,
                attachments: Vec::new(),
                model: None,
                max_turns: None,
                trigger: TurnTrigger::ExecutionSynthesis,
                child_signal_id: None,
                execution_template: None,
                action_review: None,
            },
        );
        let marker = ExecutionSynthesisDedupe {
            run_uid,
            originating_user_sequence_num: origin,
            turn_id: requested.turn_id.clone(),
        };
        state
            .record_execution_synthesis_dispatch(marker)
            .map_err(moa_error_to_handler_error)?;
        state.persist(&ctx);
        Ok(Json::from(ExecutionSynthesisDispatch {
            run_uid,
            originating_user_sequence_num: origin,
            turn_id: requested.turn_id,
        }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn has been admitted by Session.
    async fn attach_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachSessionTurnWaiterInput>,
    ) -> Result<Json<AttachSessionTurnWaiterOutput>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "attach_turn_waiter");
        let input = input.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        if let Some(outcome) = pending_state
            .last_outcome
            .as_ref()
            .filter(|outcome| outcome.turn_id == input.turn_id)
            .cloned()
        {
            return Ok(Json::from(AttachSessionTurnWaiterOutput {
                outcome: Some(outcome),
            }));
        }
        if !pending_state.turn_waiters.iter().any(|waiter| {
            waiter.turn_id == input.turn_id && waiter.awakeable_id == input.awakeable_id
        }) {
            pending_state.turn_waiters.push(SessionTurnWaiter {
                turn_id: input.turn_id,
                awakeable_id: input.awakeable_id,
            });
            persist_pending_state(&ctx, &pending_state);
        }
        Ok(Json::from(AttachSessionTurnWaiterOutput { outcome: None }))
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn wait deadline expires.
    async fn remove_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveSessionTurnWaiterInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "remove_turn_waiter");
        let input = input.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let before = pending_state.turn_waiters.len();
        pending_state.turn_waiters.retain(|waiter| {
            waiter.turn_id != input.turn_id || waiter.awakeable_id != input.awakeable_id
        });
        if pending_state.turn_waiters.len() != before {
            persist_pending_state(&ctx, &pending_state);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: Json<String>,
    ) -> Result<Json<CancelResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "request_cancel");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let pending_state = load_pending_state(&ctx).await?;
        let Some(turn_id) = pending_state.active_turn_id else {
            return Ok(Json::from(CancelResponse {
                cancelled: false,
                reason: "no active turn".to_string(),
            }));
        };

        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
                .request_cancel(reason),
        )
        .send();

        Ok(Json::from(CancelResponse {
            cancelled: true,
            reason: format!("cancel forwarded to turn {turn_id}"),
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn queue_message(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<QueueMessageRequest>,
    ) -> Result<Json<QueueMessageResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "queue_message");
        let response = start_turn_inner(
            &mut ctx,
            StartTurnRequest::from(request.into_inner()),
            &self.session_limits,
            &self.turn_admission,
        )
        .await?;
        Ok(Json::from(QueueMessageResponse::from(response)))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn snapshot(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionSnapshot>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "snapshot");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let pending_state = load_pending_state(&ctx).await?;
        let active_execution_runs = SessionVoState::load_active_execution_runs(&ctx).await?;
        Ok(Json::from(SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
            active_execution_run_uids: active_execution_runs
                .into_iter()
                .map(|marker| marker.run_uid)
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn progress(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "progress");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let event_range = request.into_inner().normalized_event_range();
        let pending_state = load_pending_state(&ctx).await?;
        let children = SessionVoState::load_children(&ctx).await?;
        let active_execution_runs = SessionVoState::load_active_execution_runs(&ctx).await?;
        let active_execution_progress =
            SessionVoState::project_active_execution_progress(&active_execution_runs);
        let active_turn_id = pending_state.active_turn_id.clone();
        let snapshot = SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
            active_execution_run_uids: active_execution_runs
                .iter()
                .map(|marker| marker.run_uid)
                .collect(),
        };
        let events =
            load_progress_events(&ctx, session_id, event_range, &self.session_store).await?;
        let active_turn_progress = if let Some(turn_id) = active_turn_id {
            active_turn_progress_or_none(
                &turn_id,
                crate::restate_identity::replay_safe_request(
                    ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
                        .progress(),
                )
                .call()
                .await,
            )
        } else {
            None
        };
        let child_progress = collect_child_progress(&ctx, &children).await;

        Ok(Json::from(SessionProgress {
            snapshot,
            active_turn_progress,
            active_execution_progress,
            events,
            child_progress,
        }))
    }

    #[tracing::instrument(skip(self, ctx, child))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn register_child(
        &self,
        ctx: ObjectContext<'_>,
        child: Json<WorkerChildRef>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "register_child");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let child = child.into_inner();
        let worker_id = child.id.clone();
        if state.register_child(child) {
            // Active edge: a child just became active, so ensure one narration tick is
            // outstanding (single-outstanding guard prevents overlapping schedules).
            narration::ensure_narration_tick_scheduled(&ctx, &mut state, &self.session_limits)
                .await?;
            // Active edge: schedule one single-outstanding per-child heartbeat-liveness
            // watchdog so a stuck child is detected without polling across sessions.
            liveness::ensure_child_liveness_scheduled(
                &ctx,
                &mut state,
                &worker_id,
                &self.session_limits,
            )
            .await?;
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn remove_child(
        &self,
        ctx: ObjectContext<'_>,
        worker_id: String,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "remove_child");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        if state.remove_child(&worker_id) {
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: internal retraction from this session's own child; it only removes reply
    // targets this session advertised for that child and reads no caller-owned data.
    async fn clear_worker_input_targets(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ClearWorkerInputTargetsInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "clear_worker_input_targets");
        let input = input.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let mut retracted = 0usize;
        for target in &input.cleared {
            if state.clear_worker_input_target(&input.worker_id, target) {
                retracted += 1;
            }
        }
        if retracted > 0 {
            tracing::debug!(
                key = %ctx.key(),
                worker_id = %input.worker_id,
                retracted,
                "retracted worker input reply targets the child cleared"
            );
        }
        // Always persist: retracting a target also drops the paired unread signal, and
        // that removal must survive even when no advertised target remained.
        state.persist(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from Worker terminal delivery after parent dispatch authz has already checked.
    async fn mark_child_terminal(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<MarkWorkerChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "mark_child_terminal");
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let input = input.into_inner();
        let worker_id = input.worker_id.clone();
        if state.mark_child_terminal(input) {
            claim_check_child_output(
                &ctx,
                &mut state,
                session_id,
                &worker_id,
                &self.session_store,
            )
            .await?;
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn consume_child_result(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ConsumeWorkerChildResultInput>,
    ) -> Result<Json<ConsumeWorkerChildResultOutput>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "consume_child_result");
        let session_id = parse_session_key(ctx.key())?;
        let input = input.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let mut terminal = state.consume_child_terminal(&input.worker_id);
        let blob = state.take_child_terminal_blob(&input.worker_id);
        if terminal.is_some() || blob.is_some() {
            // Return full-fidelity output: if the cached output was claim-checked, hydrate the
            // full body from its blob so the coordinator never receives a truncated preview.
            if let (Some(terminal), Some(claim_check)) = (terminal.as_mut(), blob) {
                hydrate_child_terminal_output(
                    &ctx,
                    session_id,
                    terminal,
                    claim_check,
                    &self.session_store,
                )
                .await?;
            }
            state.persist(&ctx);
        }
        Ok(Json::from(ConsumeWorkerChildResultOutput { terminal }))
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn child_refs(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<WorkerChildRef>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "child_refs");
        Ok(Json::from(SessionVoState::load_children(&ctx).await?))
    }

    #[tracing::instrument(skip(self, ctx, signal))]
    // SAFETY: internal child→parent control-plane write. The signaling Worker VO is
    // part of this session's task tree — it was reserved/spawned under the owning
    // session's participant authz, exactly like register_child/mark_child_terminal. The
    // handler only appends idempotently to this session's own event log and updates the
    // session's compact VO state; it reads no caller-owned data back to the caller. This
    // mirrors the established internal VO→VO write pattern on Session and adds no broad
    // authz bypass.
    async fn record_child_signal(
        &self,
        mut ctx: ObjectContext<'_>,
        signal: Json<WorkerSignal>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "record_child_signal");
        let signal = signal.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        if signal.parent_session != session_id {
            return Err(TerminalError::new_with_code(
                403,
                format!(
                    "worker signal targets session {} but was delivered to {}",
                    signal.parent_session, session_id
                ),
            )
            .into());
        }
        if !state.owns_signal_worker(&signal) {
            // Not a hard authorization failure: the `parent_session` already matched (checked
            // above), so this is a signal for a worker that is no longer a registered child —
            // typically one that raced its own `remove_child`/self-clean. Ignore it as a no-op
            // (a non-retryable 403 would permanently drop a legitimate late signal); it injects
            // nothing into the session either way.
            tracing::debug!(
                key = %ctx.key(),
                worker_id = %signal.worker_id,
                "ignoring child signal for a worker no longer registered on this session"
            );
            return Ok(());
        }

        // Idempotent append: a retried delivery with the same signal_id is a no-op at the
        // event log (dedupe table, Task 2), so it never double-records the signal.
        append_session_event_deduped(
            &ctx,
            session_id,
            Event::WorkerSignalReceived {
                signal_id: signal.signal_id,
                worker_id: signal.worker_id.clone(),
                kind: signal.kind,
                severity: signal.severity,
                summary: signal.summary.clone(),
                // Carry the awakeable id and audience onto the durable event so any later
                // coordinator turn rendered from history (not only the guarded resume turn)
                // can answer a `NeedsInput` request via `provide_worker_input`.
                input_request_id: signal
                    .input_request
                    .as_ref()
                    .map(|request| request.input_request_id.clone()),
                input_audience: signal
                    .input_request
                    .as_ref()
                    .map(|request| request.audience),
            },
            format!("worker_signal:{}", signal.signal_id),
        )
        .await?;

        // The resume gate needs the active-turn cursor, which lives in pending state.
        let active_turn_id = load_pending_state(&ctx).await?.active_turn_id;

        // Push compact signal CONTENT (dedup by signal_id, cap + action-required keep).
        let inserted = state.push_unread_child_signal(UnreadChildSignal {
            signal_id: signal.signal_id,
            worker_id: signal.worker_id.clone(),
            kind: signal.kind,
            summary: signal.summary.clone(),
            input_request: signal.input_request.clone(),
        });
        if signal.kind == ChildSignalKind::NeedsInput
            && let Some(request) = signal.input_request.as_ref()
            && request.audience == InputAudience::User
        {
            // Advertised with the raising turn and generation, so the reply the user
            // sends can only ever resolve that exact round-trip.
            state.upsert_pending_user_reply_target(PendingUserReplyTarget::WorkerInput {
                worker_id: signal.worker_id.clone(),
                turn_id: request.turn_id.clone(),
                generation: request.generation,
                input_request_id: request.input_request_id.clone(),
            });
        }

        // Idempotence: a retried delivery of an already-armed signal must not start a
        // second resume turn. `maybe_arm_parent_resume` also re-blocks once the dispatched
        // turn is active, but this short-circuits even before the turn becomes active.
        let already_pending = state.pending_parent_resume_signal == Some(signal.signal_id);
        let limits = &self.session_limits;
        let now = durable_utc_now(&ctx).await?;
        let armed = !already_pending
            && state.maybe_arm_parent_resume(
                &signal,
                active_turn_id.as_deref(),
                now,
                limits.worker_resume_max_per_window,
                limits.worker_resume_window_ms,
            );

        if armed {
            // Run the resume turn as the session's recorded owning actor so it passes
            // `require_session_participant` legitimately. With no owning identity we cannot
            // authorize a turn, so we undo the arm and skip dispatch — never a bypass.
            if let Some(identity) = state.owning_identity.clone() {
                let tenant_id = state
                    .ensure_initialized()
                    .map_err(moa_error_to_handler_error)?
                    .tenant_id;
                self.turn_admission
                    .acquire(&ctx, session_id, tenant_id, "turn_admission_child_resume")
                    .await?;
                let turn_id = generate_turn_id(&mut ctx);
                let instruction = build_resume_instruction(&signal, &state.unread_child_signals);
                // Durable, idempotent control record that seeds the resume turn's prompt
                // (the brain renders this event's `reason` instead of a fake user message).
                append_session_event_deduped(
                    &ctx,
                    session_id,
                    Event::WorkerParentResumeRequested {
                        signal_id: signal.signal_id,
                        worker_id: signal.worker_id.clone(),
                        turn_id: turn_id.clone(),
                        reason: instruction.clone(),
                    },
                    format!("parent_resume:{}", signal.signal_id),
                )
                .await?;
                // Mirror `start_turn_inner` bookkeeping so a concurrent/queued message sees
                // an active turn and no second root turn can start.
                let mut pending_state = load_pending_state(&ctx).await?;
                pending_state.active_turn_id = Some(turn_id.clone());
                activate_coordinator_security_owner(
                    &mut state,
                    &turn_id,
                    pending_state.turn_generation,
                );
                arm_turn_admission_heartbeat(&ctx, &mut pending_state, &self.turn_admission);
                state.set_status(SessionStatus::Running, now);
                state.record_resume_dispatch(turn_id.clone(), now, limits.worker_resume_window_ms);
                let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
                state.persist(&ctx);
                persist_pending_state(&ctx, &pending_state);
                sync_status(&ctx, session_id, &state).await?;
                dispatch_turn_execution(
                    &ctx,
                    RunTurnRequest {
                        session_id: ctx.key().to_string(),
                        turn_id: turn_id.clone(),
                        identity,
                        contact,
                        generation: pending_state.turn_generation,
                        user_message: instruction,
                        attachments: Vec::new(),
                        model: None,
                        max_turns: None,
                        trigger: TurnTrigger::ChildSignal,
                        child_signal_id: Some(signal.signal_id),
                        execution_template: None,
                        action_review: None,
                    },
                );
                tracing::info!(
                    key = %ctx.key(),
                    signal_id = %signal.signal_id,
                    kind = ?signal.kind,
                    turn_id = %turn_id,
                    "dispatched guarded parent resume turn"
                );
                return Ok(());
            }
            state.pending_parent_resume_signal = None;
            tracing::warn!(
                key = %ctx.key(),
                signal_id = %signal.signal_id,
                "armed parent resume but no owning identity is recorded; skipping dispatch"
            );
        } else if state.resume_budget_exhausted_for_signal(
            &signal,
            active_turn_id.as_deref(),
            now,
            limits.worker_resume_max_per_window,
            limits.worker_resume_window_ms,
        ) {
            append_session_event_deduped(
                &ctx,
                session_id,
                Event::Warning {
                    message: format!(
                        "Worker resume budget exhausted; queued signal {} from worker {} for a later turn.",
                        signal.signal_id, signal.worker_id
                    ),
                },
                format!("parent_resume_budget_exhausted:{}", signal.signal_id),
            )
            .await?;
        } else if already_pending {
            tracing::debug!(
                key = %ctx.key(),
                signal_id = %signal.signal_id,
                "duplicate child signal for an already-pending resume; no second dispatch"
            );
        }

        state.persist(&ctx);
        tracing::debug!(
            key = %ctx.key(),
            signal_id = %signal.signal_id,
            kind = ?signal.kind,
            inserted,
            armed,
            "recorded child control-plane signal"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "destroy");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        // Reclaim any coordinator/orphan hands still leased under this session before the
        // VO state is cleared. The Session VO holds no `ToolRouter`, so this is dispatched
        // detached (fire-and-forget) to the ToolExecutor service that owns the router. It is
        // non-fatal; without this caller durable leases reclaim only via their 1-hour TTL.
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<ToolExecutorClient>()
                .release_session_hands(Json::from(ReleaseSessionHandsRequest { session_id })),
        )
        .send();
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-tick scheduled by this Session VO; it
    // reads only its own VO state and the bounded child/turn fan-in, and forwards the
    // session's own owning-actor identity to the detached narration job, which re-checks
    // Session participant authz on its gated progress read.
    async fn narration_tick(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<NarrationTickRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "narration_tick");
        narration::run_narration_tick(&ctx, req.into_inner().generation, &self.session_limits).await
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-call; it only renews the shared
    // admission lease for this Session while a coordinator turn remains active.
    async fn turn_admission_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<TurnAdmissionHeartbeatRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "turn_admission_heartbeat");
        let req = req.into_inner();
        let pending_state = load_pending_state(&ctx).await?;
        if pending_state.active_turn_id.is_none()
            || pending_state.admission_heartbeat_generation != req.generation
        {
            return Ok(());
        }
        let session_id = parse_session_key(ctx.key())?;
        let state = SessionVoState::load_from(&ctx).await?;
        let tenant_id = state
            .ensure_initialized()
            .map_err(moa_error_to_handler_error)?
            .tenant_id;
        self.turn_admission
            .acquire(&ctx, session_id, tenant_id, "turn_admission_heartbeat")
            .await?;
        schedule_turn_admission_heartbeat(&ctx, req.generation, &self.turn_admission);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-tick scheduled by this Session VO for its
    // own active children. It reads only its own VO state plus the child's compact
    // progress summary (the same informational fan-in `progress` already performs), and
    // any stale signal it raises is recorded through `record_child_signal`, which carries
    // the established internal child→parent control-plane authz justification.
    async fn check_child_liveness(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<CheckChildLivenessRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "check_child_liveness");
        liveness::run_child_liveness_check(&ctx, req.into_inner(), &self.session_limits).await
    }
}

fn take_turn_waiters(state: &mut SessionPendingState, turn_id: &str) -> Vec<SessionTurnWaiter> {
    let mut matching = Vec::new();
    state.turn_waiters.retain(|waiter| {
        if waiter.turn_id == turn_id {
            matching.push(waiter.clone());
            false
        } else {
            true
        }
    });
    matching
}

fn resolve_turn_waiters(
    ctx: &ObjectContext<'_>,
    waiters: Vec<SessionTurnWaiter>,
    outcome: &ExecutionTurnOutcome,
) -> Result<(), HandlerError> {
    if waiters.is_empty() {
        return Ok(());
    }
    let payload = serde_json::to_string(outcome).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize turn outcome for waiter: {error}"
        ))
    })?;
    for waiter in waiters {
        ctx.resolve_awakeable(&waiter.awakeable_id, payload.clone());
    }
    Ok(())
}

fn active_turn_progress_or_none(
    turn_id: &str,
    progress: Result<Json<TurnProgress>, TerminalError>,
) -> Option<TurnProgress> {
    match progress {
        Ok(progress) => Some(progress.into_inner()),
        Err(error) => {
            tracing::warn!(
                turn_id = %turn_id,
                error = %error,
                "active turn progress unavailable; returning durable session history"
            );
            None
        }
    }
}

/// Builds the bounded, on-demand child-progress fan-in for `Session/progress`.
///
/// Terminal children are synthesized from cached parent refs without a live call;
/// non-terminal children are read via `Worker::progress_summary`, capped by the
/// existing `MAX_WORKER_FAN_OUT` so the fan-in never walks an unbounded tree.
/// A child whose summary read fails is omitted rather than failing the whole poll.
async fn collect_child_progress(
    ctx: &SharedObjectContext<'_>,
    children: &[WorkerChildRef],
) -> Vec<WorkerProgressSummary> {
    let plan = plan_child_progress_fan_in(children, MAX_WORKER_FAN_OUT);
    let mut summaries: Vec<Option<WorkerProgressSummary>> = (0..plan.len()).map(|_| None).collect();
    let mut fetch_plan_slots = Vec::new();
    let mut inflight = DurableFuturesUnordered::new();

    for (plan_slot, item) in plan.into_iter().enumerate() {
        match item {
            ChildProgressFetch::Ready(summary) => summaries[plan_slot] = Some(summary),
            ChildProgressFetch::Fetch(child_id) => {
                fetch_plan_slots.push((plan_slot, child_id.clone()));
                inflight.push(
                    crate::restate_identity::replay_safe_request(
                        ctx.object_client::<WorkerClient>(child_id)
                            .progress_summary(),
                    )
                    .call(),
                );
            }
        }
    }

    loop {
        match inflight.next().await {
            Ok(Some((fetch_slot, result))) => {
                let (plan_slot, child_id) = &fetch_plan_slots[fetch_slot];
                match result {
                    Ok(summary) => summaries[*plan_slot] = Some(summary.into_inner()),
                    Err(error) => tracing::warn!(
                        child_id = %child_id,
                        error = %error,
                        "child progress summary unavailable; omitting from fan-in"
                    ),
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "child progress fan-in interrupted; omitting unfinished summaries"
                );
                break;
            }
        }
    }

    child_progress_in_plan_order(summaries)
}

/// Delivers one user reply to the exact target the reply matrix resolved.
///
/// The target is decided by [`resolve_message_routing`] before any mutation, so this
/// function never chooses a target itself and can never deliver to a second one.
async fn forward_user_input_reply(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    identity: &moa_core::traits::Identity,
    target: &PendingUserReplyTarget,
    text: &str,
) -> Result<(), HandlerError> {
    // The run this reply addresses is scoped by the session's own tenant and contact, so
    // they are read from admitted session metadata rather than re-passed by the caller.
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let tenant_id = meta.tenant_id;
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    let acknowledgement = match target {
        PendingUserReplyTarget::ExecutionConfirmation {
            run_uid,
            expected_plan_hash,
            approved_budget,
        } => {
            let call = ctx.service_client::<ExecutionClient>().confirm(Json::from(
                moa_execution::wire::ExecutionConfirmRequest {
                    run: moa_execution::wire::ExecutionRunRequest {
                        tenant_id,
                        contact_id,
                        session_id,
                        run_uid: *run_uid,
                    },
                    expected_plan_hash: moa_execution::capability::ExecutionHash::from_bytes(
                        *expected_plan_hash,
                    ),
                    approved_budget: approved_budget.clone(),
                },
            ));
            execution_mutation_ack(
                with_identity_headers(call, identity)
                    .call()
                    .await?
                    .into_inner(),
            )
        }
        PendingUserReplyTarget::ExecutionInput {
            run_uid,
            task_id,
            generation,
        } => {
            let call = ctx
                .service_client::<ExecutionClient>()
                .deliver_input(Json::from(moa_execution::wire::ExecutionInputRequest {
                    tenant_id,
                    contact_id,
                    session_id: Some(session_id),
                    run_uid: *run_uid,
                    task_id: moa_execution::state::ExecutionTaskId::from_uuid(*task_id),
                    expected_generation: *generation,
                    audience: moa_artifacts::execution_plan::InputAudience::User,
                    input: serde_json::Value::String(text.to_string()),
                }));
            execution_mutation_ack(
                with_identity_headers(call, identity)
                    .call()
                    .await?
                    .into_inner(),
            )
        }
        PendingUserReplyTarget::WorkerInput {
            worker_id,
            turn_id,
            generation,
            input_request_id,
        } => {
            let call = ctx
                .object_client::<WorkerClient>(worker_id.clone())
                .provide_input(Json::from(worker_provide_input_request(
                    session_id,
                    WorkerInputTarget {
                        turn_id: turn_id.clone(),
                        generation: *generation,
                        input_request_id: input_request_id.clone(),
                    },
                    text,
                )));
            let acknowledgement = with_identity_headers(call, identity)
                .call()
                .await?
                .into_inner();
            if matches!(
                acknowledgement,
                UserReplyDeliveryAck::Applied | UserReplyDeliveryAck::Replayed
            ) {
                append_session_event_deduped(
                    ctx,
                    session_id,
                    Event::WorkerMessageSent {
                        worker_id: worker_id.clone(),
                        input_request_id: Some(input_request_id.clone()),
                        text: text.to_string(),
                    },
                    format!("worker_input_reply:{input_request_id}"),
                )
                .await?;
                state.clear_unread_worker_input(worker_id, input_request_id);
            }
            acknowledgement
        }
        PendingUserReplyTarget::CoordinatorInput {
            turn_id,
            generation,
            input_request_id,
        } => {
            // Delivery is a VO-local awakeable resolve, not a cross-object call:
            // the blocked turn is parked on an awakeable this same object
            // registered. The fence is checked inside `take_*`, so a reply naming
            // a superseded generation resolves nothing.
            match state.take_coordinator_input_awakeable(turn_id, *generation, input_request_id) {
                Some(awakeable_id) => {
                    ctx.resolve_awakeable(&awakeable_id, text.to_string());
                    UserReplyDeliveryAck::Applied
                }
                None if state.coordinator_input_already_delivered(input_request_id) => {
                    // A late duplicate is a replay, never a second resolve: the
                    // awakeable is gone, and a newer request could otherwise be
                    // unblocked by an answer meant for the previous one.
                    UserReplyDeliveryAck::Replayed
                }
                None => UserReplyDeliveryAck::Conflict,
            }
        }
    };
    state.apply_pending_user_reply_ack(target, acknowledgement);
    Ok(())
}

fn worker_provide_input_request(
    parent_session: SessionId,
    target: WorkerInputTarget,
    text: &str,
) -> WorkerProvideInputRequest {
    WorkerProvideInputRequest {
        parent_session,
        target,
        input: serde_json::Value::String(text.to_string()),
    }
}

fn execution_mutation_ack(
    response: moa_execution::wire::ExecutionMutationResponse,
) -> UserReplyDeliveryAck {
    match response {
        moa_execution::wire::ExecutionMutationResponse::Applied { .. } => {
            UserReplyDeliveryAck::Applied
        }
        moa_execution::wire::ExecutionMutationResponse::Replayed { .. } => {
            UserReplyDeliveryAck::Replayed
        }
        moa_execution::wire::ExecutionMutationResponse::Conflict { .. }
        | moa_execution::wire::ExecutionMutationResponse::NotFound => {
            UserReplyDeliveryAck::Conflict
        }
    }
}

/// Offloads a just-marked terminal child's large output to a content-addressed blob.
///
/// A no-op unless the child's output exceeds the claim-check threshold. The full body is
/// stored via a journaled `ctx.run` (content-addressed, so the recorded blob id is
/// deterministic and reused on replay) and the inline `children` copy is compacted to a
/// preview.
async fn claim_check_child_output(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    worker_id: &str,
    session_store: &Arc<dyn SessionStore>,
) -> Result<(), HandlerError> {
    let Some(full_output) = state.large_child_terminal_output(worker_id) else {
        return Ok(());
    };
    let store = session_store.clone();
    let claim = ctx
        .run(|| async move {
            store
                .store_text_artifact(session_id, &full_output)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name(format!("child_terminal_output_claim_check_{worker_id}"))
        .await?
        .into_inner();
    state.compact_child_terminal_output(worker_id, claim);
    Ok(())
}

/// Hydrates a consumed terminal child's full output back into `terminal` from its blob.
async fn hydrate_child_terminal_output(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    terminal: &mut WorkerTerminalResult,
    claim_check: ClaimCheck,
    session_store: &Arc<dyn SessionStore>,
) -> Result<(), HandlerError> {
    let name = format!(
        "child_terminal_output_hydrate_{}",
        terminal.result.worker_id
    );
    let store = session_store.clone();
    let body = ctx
        .run(|| async move {
            store
                .load_text_artifact(session_id, &claim_check)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name(name)
        .await?
        .into_inner();
    terminal.result.output = body;
    Ok(())
}

/// Rejects and drains every already-accepted queued message, in FIFO order.
///
/// Returns how many messages were discarded. Each rejection is appended under a
/// key derived from the cancelled turn (or this invocation when the session was
/// idle) plus the message's queue position, so a retried `cancel` re-derives the
/// same keys and records each rejection exactly once.
///
/// Rejection is a terminal disposition for the admission that queued the message: its
/// recorded response still replays for a retry inside the guarantee window, but the entry
/// can now age out. Leaving it unresolved would pin one admission per rejected message
/// for the life of the session.
async fn reject_queued_messages(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    pending_state: &mut SessionPendingState,
    admissions: &mut SessionMessageAdmissions,
    cancelled_turn_id: Option<&str>,
    now: DateTime<Utc>,
) -> Result<usize, HandlerError> {
    let fence_key = cancelled_turn_id
        .map(str::to_string)
        .unwrap_or_else(|| ctx.invocation_id().to_string());
    let rejected = std::mem::take(&mut pending_state.pending_messages);
    let count = rejected.len();
    for (queue_index, message) in rejected.into_iter().enumerate() {
        append_session_event_deduped(
            ctx,
            session_id,
            Event::QueuedMessageRejected {
                queued_at: message.queued_at,
                queue_index: queue_index as u64,
                rejection: QueuedMessageRejection::TaskTreeCancelled,
            },
            format!("queued_message_rejected:{fence_key}:{queue_index}"),
        )
        .await?;
        admissions.mark_terminal_for_message(&message.client_message_id, now);
    }
    Ok(count)
}

async fn dispatch_queued_parent_resume_if_idle(
    ctx: &mut ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    state: &mut SessionVoState,
    session_id: SessionId,
    now: DateTime<Utc>,
    limits: &SessionLimitsConfig,
) -> Result<bool, HandlerError> {
    if pending_state.active_turn_id.is_some() {
        return Ok(false);
    }
    let Some(unread) = state
        .unread_child_signals
        .iter()
        .find(|signal| signal_kind_is_resume_eligible(signal.kind))
        .cloned()
    else {
        return Ok(false);
    };
    let signal = unread_to_resume_signal(session_id, &unread, now);
    if !state.maybe_arm_parent_resume(
        &signal,
        None,
        now,
        limits.worker_resume_max_per_window,
        limits.worker_resume_window_ms,
    ) {
        if state.resume_budget_exhausted_for_signal(
            &signal,
            None,
            now,
            limits.worker_resume_max_per_window,
            limits.worker_resume_window_ms,
        ) {
            append_session_event_deduped(
                ctx,
                session_id,
                Event::Warning {
                    message: format!(
                        "Worker resume budget exhausted; queued signal {} from worker {} for a later turn.",
                        signal.signal_id, signal.worker_id
                    ),
                },
                format!("parent_resume_budget_exhausted:{}", signal.signal_id),
            )
            .await?;
        }
        return Ok(false);
    }

    let Some(identity) = state.owning_identity.clone() else {
        state.pending_parent_resume_signal = None;
        tracing::warn!(
            key = %ctx.key(),
            signal_id = %signal.signal_id,
            "queued child signal could resume parent but no owning identity is recorded"
        );
        return Ok(false);
    };

    let turn_id = generate_turn_id(ctx);
    let instruction = build_resume_instruction(&signal, &state.unread_child_signals);
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerParentResumeRequested {
            signal_id: signal.signal_id,
            worker_id: signal.worker_id.clone(),
            turn_id: turn_id.clone(),
            reason: instruction.clone(),
        },
        format!("parent_resume:{}", signal.signal_id),
    )
    .await?;
    pending_state.active_turn_id = Some(turn_id.clone());
    activate_coordinator_security_owner(state, &turn_id, pending_state.turn_generation);
    state.set_status(SessionStatus::Running, now);
    state.record_resume_dispatch(turn_id.clone(), now, limits.worker_resume_window_ms);
    let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
    state.persist_into(ctx);
    persist_pending_state(ctx, pending_state);
    sync_status(ctx, session_id, state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            generation: pending_state.turn_generation,
            user_message: instruction,
            attachments: Vec::new(),
            model: None,
            max_turns: None,
            trigger: TurnTrigger::ChildSignal,
            child_signal_id: Some(signal.signal_id),
            execution_template: None,
            action_review: None,
        },
    );
    tracing::info!(
        key = %ctx.key(),
        signal_id = %signal.signal_id,
        kind = ?signal.kind,
        turn_id = %turn_id,
        "dispatched queued child signal after coordinator turn completed"
    );
    Ok(true)
}

fn unread_to_resume_signal(
    session_id: SessionId,
    unread: &UnreadChildSignal,
    now: DateTime<Utc>,
) -> WorkerSignal {
    WorkerSignal {
        signal_id: unread.signal_id,
        worker_id: unread.worker_id.clone(),
        parent_session: session_id,
        kind: unread.kind,
        severity: match unread.kind {
            ChildSignalKind::Failed => SignalSeverity::Critical,
            ChildSignalKind::Finding => SignalSeverity::Info,
            ChildSignalKind::Blocked
            | ChildSignalKind::NeedsInput
            | ChildSignalKind::HeartbeatStale => SignalSeverity::Warning,
        },
        summary: unread.summary.clone(),
        payload: serde_json::Value::Null,
        created_at: now,
        resume_policy: ParentResumePolicy::IfIdle,
        input_request: unread.input_request.clone(),
    }
}

/// Builds the system-generated coordinator instruction for a guarded resume turn.
///
/// Folds the triggering signal plus the session's current unread child-signal summaries
/// into one prompt so the coordinator can decide to ask for edits, provide input, wait,
/// or produce the final response. Carried as the `reason` of `WorkerParentResumeRequested`
/// and as the resume turn's `user_message`; the brain renders it as a system directive,
/// never a fake user message.
fn build_resume_instruction(signal: &WorkerSignal, unread: &[UnreadChildSignal]) -> String {
    use std::fmt::Write;
    let mut text = format!(
        "Worker {} reported {:?}: {}\n",
        signal.worker_id, signal.kind, signal.summary
    );
    if !unread.is_empty() {
        text.push_str("\nUnread worker signals:\n");
        for entry in unread {
            let _ = writeln!(
                text,
                "- {} [{:?}]: {}",
                entry.worker_id, entry.kind, entry.summary
            );
        }
    }
    text.push_str(
        "\nDecide whether to ask for edits, provide input, wait, or produce the final response.",
    );
    text
}

async fn load_progress_events(
    ctx: &SharedObjectContext<'_>,
    session_id: SessionId,
    range: EventRange,
    session_store: &Arc<dyn SessionStore>,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = session_store.clone();
    Ok(ctx
        .run(move || {
            let store = store.clone();
            async move {
                store
                    .get_events(session_id, range)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            }
        })
        .name("session_progress_load_events")
        .await?
        .into_inner())
}

async fn start_turn_inner(
    ctx: &mut ObjectContext<'_>,
    request: StartTurnRequest,
    session_limits: &SessionLimitsConfig,
    turn_admission: &admission::TurnAdmission,
) -> Result<StartTurnResponse, HandlerError> {
    // Continue the caller's trace (edge or Slack ingress) so the turn this
    // schedules dispatches TurnExecution under the same end-to-end trace.
    crate::ctx::adopt_incoming_trace_parent(ctx);
    let session_id = parse_session_key(ctx.key())?;
    let identity = require_session_participant(ctx, session_id).await?;
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let contact = admitted_contact_for_turn(request.contact.clone(), meta)?;
    let tenant_id = meta.tenant_id;

    // The fence is consulted before every Session side effect, including the reply
    // delivery, the queue mutation, the shared admission lease, and the turn dispatch
    // below. A retry that reaches here after its original response was lost must
    // receive that response back, never a second admission.
    let client_message_id = request.client_message_id.clone();
    let request_hash = request.canonical_request_hash(contact.as_ref());
    let mut admissions = load_message_admissions(ctx).await?;
    match admissions.lookup(&client_message_id, request_hash) {
        AdmissionLookup::Replay(response) => {
            record_admission_decision("replayed");
            tracing::info!(
                key = %ctx.key(),
                client_message_id = %client_message_id,
                "replayed an already-admitted session message without a second side effect"
            );
            return Ok(response);
        }
        AdmissionLookup::Conflict { admitted } => {
            record_admission_decision("conflict");
            return Err(TerminalError::new_with_code(
                409,
                format!(
                    "client message id {client_message_id} was already admitted for a different \
                     request (admitted request hash {})",
                    admitted.to_hex()
                ),
            )
            .into());
        }
        AdmissionLookup::Fresh => {}
    }

    let mut pending_state = load_pending_state(ctx).await?;
    // A cancelled task tree is being torn down. Admitting work into it — even work
    // that raced the cancellation — would start a turn whose children and execution
    // runs are already being cancelled, so the caller gets a typed refusal until the
    // cancelled turn reports its outcome.
    if pending_state.task_tree_cancellation_fenced() {
        record_admission_decision("rejected_task_tree_cancelling");
        return Err(TerminalError::new_with_code(
            409,
            "session task-tree cancellation is in progress and cannot admit new turns",
        )
        .into());
    }

    // The routing decision is made before any mutation, so every refusal below leaves
    // no queue entry, no reply delivery, no turn, and no recorded admission.
    let routing = resolve_message_routing(
        &state.pending_user_reply_targets,
        request.reply_to.as_ref(),
        !request.attachments.is_empty(),
    );
    let now = durable_utc_now(ctx).await?;
    match routing {
        MessageRouting::AmbiguousImplicitReply { targets } => {
            record_admission_decision("rejected_ambiguous_reply");
            return Err(TerminalError::new_with_code(
                409,
                format!(
                    "session is waiting on {targets} user replies; resend with reply_to naming \
                     exactly one of them"
                ),
            )
            .into());
        }
        MessageRouting::StaleReplyTarget => {
            record_admission_decision("rejected_stale_reply_target");
            return Err(TerminalError::new_with_code(
                409,
                "reply target does not match any request this session is waiting on",
            )
            .into());
        }
        MessageRouting::ReplyWithAttachments => {
            record_admission_decision("rejected_reply_with_attachments");
            return Err(TerminalError::new_with_code(
                400,
                "a reply to a pending request cannot carry attachments",
            )
            .into());
        }
        MessageRouting::Reply(target) => {
            forward_user_input_reply(
                ctx,
                &mut state,
                session_id,
                &identity,
                &target,
                &request.user_message,
            )
            .await?;
            let response = StartTurnResponse {
                turn_id: None,
                queued: false,
                stream_cursor: request.stream_cursor,
            };
            // A delivered reply has no later callback: the target either applied it or
            // definitively did not, so this admission is terminal at admission time.
            let evicted = admissions.record(
                client_message_id,
                request_hash,
                response.clone(),
                MessageAdmissionState::Terminal {
                    at: now,
                    ordinal: 0,
                },
                now,
            );
            state.persist_into(ctx);
            persist_message_admissions(ctx, &admissions, evicted);
            sync_status(ctx, session_id, &state).await?;
            record_admission_decision("admitted_reply");
            return Ok(response);
        }
        MessageRouting::OrdinaryTurn => {}
    }

    if let Some(active_turn_id) = pending_state.active_turn_id.clone() {
        if pending_message_queue_is_full(
            pending_state.pending_messages.len(),
            session_limits.max_pending_messages,
        ) {
            record_admission_decision("rejected_queue_full");
            return Err(TerminalError::new_with_code(
                429,
                format!(
                    "session pending message queue is full; retry_after_ms={}",
                    session_limits.turn_admission_retry_after_ms
                ),
            )
            .into());
        }
        let generation = pending_state.advance_turn_generation();
        let queue_index = pending_state.pending_messages.len();
        append_session_event_deduped(
            ctx,
            session_id,
            Event::QueuedMessage {
                text: request.user_message.clone(),
                attachments: request.attachments.clone(),
                queued_at: now,
            },
            format!("queued_message:{active_turn_id}:{queue_index}:{client_message_id}"),
        )
        .await?;
        pending_state.pending_messages.push_back(PendingMessage {
            client_message_id: client_message_id.clone(),
            generation,
            queued_at: now,
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            execution_template: request.execution_template,
        });
        let response = StartTurnResponse {
            turn_id: None,
            queued: true,
            stream_cursor: request.stream_cursor,
        };
        let evicted = admissions.record(
            client_message_id,
            request_hash,
            response.clone(),
            MessageAdmissionState::Queued,
            now,
        );
        persist_pending_state(ctx, &pending_state);
        persist_message_admissions(ctx, &admissions, evicted);
        record_admission_decision("admitted_queued");
        return Ok(response);
    }

    turn_admission
        .acquire(ctx, session_id, tenant_id, "turn_admission_start")
        .await?;
    let generation = pending_state.advance_turn_generation();
    let turn_id = generate_turn_id(ctx);
    pending_state.active_turn_id = Some(turn_id.clone());
    activate_coordinator_security_owner(&mut state, &turn_id, generation);
    arm_turn_admission_heartbeat(ctx, &mut pending_state, turn_admission);
    state.set_status(SessionStatus::Running, now);
    // Capture the session's owning-actor identity from the first verified turn
    // participant so the self-originated narration read can be authorized later.
    if state.owning_identity.is_none() {
        state.owning_identity = Some(identity.clone());
    }
    // Active edge: a coordinator turn is starting, so ensure a narration tick is
    // outstanding (single-outstanding guard prevents overlapping schedules).
    narration::ensure_narration_tick_scheduled(ctx, &mut state, session_limits).await?;
    let drained = state.drain_unread_child_signals();
    let response = StartTurnResponse {
        turn_id: Some(turn_id.clone()),
        queued: false,
        stream_cursor: request.stream_cursor,
    };
    let evicted = admissions.record(
        client_message_id,
        request_hash,
        response.clone(),
        MessageAdmissionState::Running {
            turn_id: turn_id.clone(),
        },
        now,
    );
    state.persist_into(ctx);
    persist_pending_state(ctx, &pending_state);
    persist_message_admissions(ctx, &admissions, evicted);
    sync_status(ctx, session_id, &state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            generation,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            trigger: TurnTrigger::UserMessage,
            child_signal_id: None,
            execution_template: request.execution_template,
            action_review: None,
        },
    );
    if drained > 0 {
        tracing::debug!(
            key = %ctx.key(),
            turn_id = %turn_id,
            drained,
            "drained queued child signals into coordinator turn"
        );
    }
    record_admission_decision("admitted_turn");

    Ok(response)
}

fn pending_message_queue_is_full(pending: usize, limit: u32) -> bool {
    pending >= limit as usize
}

/// Builds the coordinator continuation turn request for one resolved review.
///
/// The turn carries the typed receipt and the origin generation — never a fake
/// user message, an execution template, or an attachment — so it can only run the
/// bounded no-tools `Respond` path the continuation matrix allows.
fn action_review_run_request(
    session_id: String,
    turn_id: String,
    identity: moa_core::traits::Identity,
    contact: Option<ContactRef>,
    generation: u64,
    continuation: moa_core::types::action_policy::ActionReviewContinuation,
) -> RunTurnRequest {
    let user_message = continuation.receipt.system_directive();
    RunTurnRequest {
        session_id,
        turn_id,
        identity,
        contact,
        generation,
        user_message,
        attachments: Vec::new(),
        model: None,
        // Exactly one bounded model call: the continuation reports a settled result,
        // it does not reopen the task.
        max_turns: Some(1),
        trigger: TurnTrigger::ActionReview,
        child_signal_id: None,
        execution_template: None,
        action_review: Some(continuation),
    }
}

fn admitted_contact_for_turn(
    requested: Option<ContactRef>,
    meta: &SessionMeta,
) -> Result<Option<ContactRef>, HandlerError> {
    let Some(requested) = requested else {
        return Ok(meta.contact.clone());
    };
    if meta.contact.as_ref() == Some(&requested) {
        Ok(meta.contact.clone())
    } else {
        Err(TerminalError::new_with_code(403, "turn contact override is not allowed").into())
    }
}

async fn require_session_participant(
    ctx: &impl RequestHeaders,
    session_id: SessionId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

/// Installs the coordinator circuit owner as part of admitting a turn.
fn activate_coordinator_security_owner(state: &mut SessionVoState, turn_id: &str, generation: u64) {
    state.security_circuit.adopt_owner(
        &moa_core::types::security::SecurityCircuitOwner::Coordinator {
            turn_id: turn_id.to_string(),
            generation,
        },
    );
}

#[cfg(test)]
mod tests {
    use moa_core::{
        types::channel::Channel, types::contact::ContactId, types::contact::ContactRef,
        types::contact::ContactVerificationState, types::identifiers::ModelId,
        types::identifiers::SessionId, types::identifiers::TenantId,
        types::security::SecurityCircuitOwner, types::session::SessionMeta,
    };
    use restate_sdk::prelude::TerminalError;

    use super::{
        SessionVoState, WorkerInputTarget, activate_coordinator_security_owner,
        active_turn_progress_or_none, admitted_contact_for_turn, pending_message_queue_is_full,
        worker_provide_input_request,
    };

    #[test]
    fn coordinator_turn_admission_installs_the_security_owner() {
        // Pins: the owner fence exists before any classified tool output or
        // delayed action-review assessment can reach the Session VO.
        let mut state = SessionVoState::default();

        activate_coordinator_security_owner(&mut state, "turn-7", 7);

        assert_eq!(
            state.security_circuit.owner,
            Some(SecurityCircuitOwner::Coordinator {
                turn_id: "turn-7".to_string(),
                generation: 7,
            })
        );
    }

    #[test]
    fn pending_message_queue_rejects_exactly_at_the_configured_bound() {
        // Pins: active sessions accept only the declared number of queued
        // messages; the next message is rejected instead of growing state.
        assert!(!pending_message_queue_is_full(7, 8));
        assert!(pending_message_queue_is_full(8, 8));
        assert!(pending_message_queue_is_full(9, 8));
    }

    #[test]
    fn session_worker_reply_payload_carries_exact_parent_session_and_string() {
        // Pins: Session plain-reply routing sends the exact owning Session scope, the full
        // owner fence of the advertised target, and keeps the canonical Value::String
        // payload expected by Worker replay hashing.
        let parent_session = SessionId::new();
        let target = WorkerInputTarget {
            turn_id: "worker-turn-9".to_string(),
            generation: 5,
            input_request_id: "request-9".to_string(),
        };
        let request =
            worker_provide_input_request(parent_session, target.clone(), "the exact answer");

        assert_eq!(request.parent_session, parent_session);
        assert_eq!(request.target, target);
        assert_eq!(
            request.input,
            serde_json::Value::String("the exact answer".to_string())
        );
    }

    #[test]
    fn session_progress_active_turn_failure_returns_none() {
        // Pins: Session/progress still returns snapshot and durable history when the active turn workflow is unavailable.
        let progress = active_turn_progress_or_none(
            "turn-1",
            Err(TerminalError::new("turn progress unavailable")),
        );

        assert_eq!(progress, None);
    }

    #[test]
    fn admitted_contact_for_turn_rejects_per_message_contact_override() {
        // Pins: contact context for turns comes from persisted SessionMeta, not caller payloads.
        let tenant_id = TenantId::new();
        let session_contact = contact(ContactId::new(), tenant_id);
        let requested_contact = contact(ContactId::new(), tenant_id);
        let meta = session_meta(session_contact.clone());

        let error = admitted_contact_for_turn(Some(requested_contact), &meta)
            .expect_err("mismatched contact override should fail");

        assert!(
            format!("{error:?}").contains("turn contact override is not allowed"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            admitted_contact_for_turn(Some(session_contact.clone()), &meta)
                .expect("matching snapshot should be admitted"),
            Some(session_contact)
        );
        assert_eq!(
            admitted_contact_for_turn(None, &meta).expect("missing contact should use session"),
            meta.contact
        );
    }

    fn session_meta(contact: ContactRef) -> SessionMeta {
        SessionMeta {
            tenant_id: contact.tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: Some(contact),
            ..SessionMeta::default()
        }
    }

    fn contact(contact_id: ContactId, tenant_id: TenantId) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Unverified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }
}

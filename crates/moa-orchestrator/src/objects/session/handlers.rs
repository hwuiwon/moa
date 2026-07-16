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

    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        mut ctx: ObjectContext<'_>,
        msg: Json<UserMessage>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Session", "post_message");
        let msg = msg.into_inner();
        start_turn_inner(
            &mut ctx,
            StartTurnRequest {
                user_message: msg.text,
                attachments: msg.attachments,
                model: None,
                contact: None,
                max_turns: None,
                execution_template: None,
            },
            &self.session_limits,
        )
        .await?;
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
        state.set_cancel_flag(scope);
        let children = state.children.clone();
        let active_execution_run_uids = state
            .active_execution_runs
            .iter()
            .map(|run| run.run_uid)
            .collect::<Vec<_>>();
        state.persist(&ctx);
        // Both scopes cancel the active coordinator turn.
        if let Some(turn_id) = load_pending_state(&ctx).await?.active_turn_id {
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
            start_turn_inner(&mut ctx, request.into_inner(), &self.session_limits).await?,
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
        let matches_active =
            pending_state.active_turn_id.as_deref() == Some(outcome.turn_id.as_str());
        let session_id = parse_session_key(ctx.key())?;
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

        if matches_active {
            pending_state.active_turn_id = None;
        }
        pending_state.last_outcome = Some(outcome.clone());
        let turn_waiters = take_turn_waiters(&mut pending_state, &outcome.turn_id);
        state.last_turn_summary = Some(outcome.message.clone());

        // When the completing turn was the guarded coordinator resume turn, clear the
        // pending-resume marker and drain exactly its dispatch-time unread snapshot
        // (signals that arrived mid-turn stay queued for the next resume). Only the
        // active turn can match, and every `matches_active` branch below persists `state`.
        if matches_active && state.clear_resume_on_outcome(&outcome.turn_id) {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared pending parent resume and drained dispatch-time signal snapshot"
            );
        }
        if matches_active
            && matches!(
                outcome.kind,
                ExecutionTurnOutcomeKind::Completed | ExecutionTurnOutcomeKind::Accepted { .. }
            )
            && let Some(next) = pending_state.pending_messages.pop_front()
        {
            let next_turn_id = generate_turn_id(&mut ctx);
            pending_state.active_turn_id = Some(next_turn_id.clone());
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
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
                    user_message: next.user_message,
                    attachments: next.attachments,
                    model: next.model,
                    max_turns: next.max_turns,
                    trigger: TurnTrigger::UserMessage,
                    child_signal_id: None,
                    execution_template: next.execution_template,
                },
            );
            return Ok(());
        }

        if matches_active {
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
        persist_pending_state(&ctx, &pending_state);
        resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
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
            dispatch_execution_run(&ctx, &state, run_uid)?;
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
        pending_state.active_turn_id = Some(requested.turn_id.clone());
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
                user_message: instruction,
                attachments: Vec::new(),
                model: None,
                max_turns: None,
                trigger: TurnTrigger::ExecutionSynthesis,
                child_signal_id: None,
                execution_template: None,
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
        let request = request.into_inner();
        let response = start_turn_inner(
            &mut ctx,
            StartTurnRequest {
                user_message: request.user_message,
                attachments: request.attachments,
                model: request.model,
                contact: request.contact,
                max_turns: request.max_turns,
                execution_template: request.execution_template,
            },
            &self.session_limits,
        )
        .await?;
        Ok(Json::from(QueueMessageResponse {
            queued: response.queued,
            started_turn_id: response.turn_id,
        }))
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
                input_request_id: signal.input_request_id.clone(),
                input_audience: signal.input_audience,
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
            input_request_id: signal.input_request_id.clone(),
            input_audience: signal.input_audience,
        });
        if signal.kind == ChildSignalKind::NeedsInput
            && signal.input_audience == Some(InputAudience::User)
            && let Some(input_request_id) = signal.input_request_id.clone()
        {
            state.upsert_pending_user_reply_target(PendingUserReplyTarget::WorkerInput {
                worker_id: signal.worker_id.clone(),
                input_request_id,
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
                        user_message: instruction,
                        attachments: Vec::new(),
                        model: None,
                        max_turns: None,
                        trigger: TurnTrigger::ChildSignal,
                        child_signal_id: Some(signal.signal_id),
                        execution_template: None,
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

async fn forward_user_input_reply(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
    identity: &moa_core::traits::Identity,
    text: &str,
) -> Result<bool, HandlerError> {
    let Some(target) = state.exact_pending_user_reply_target() else {
        return Ok(false);
    };
    let acknowledgement = match &target {
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
            input_request_id,
        } => {
            let call = ctx
                .object_client::<WorkerClient>(worker_id.clone())
                .provide_input(Json::from(worker_provide_input_request(
                    session_id,
                    input_request_id,
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
    };
    state.apply_pending_user_reply_ack(&target, acknowledgement);
    Ok(true)
}

fn worker_provide_input_request(
    parent_session: SessionId,
    input_request_id: &str,
    text: &str,
) -> WorkerProvideInputRequest {
    WorkerProvideInputRequest {
        parent_session,
        input_request_id: input_request_id.to_string(),
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
    session_store: &Arc<dyn SessionRepository>,
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
    session_store: &Arc<dyn SessionRepository>,
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
            user_message: instruction,
            attachments: Vec::new(),
            model: None,
            max_turns: None,
            trigger: TurnTrigger::ChildSignal,
            child_signal_id: Some(signal.signal_id),
            execution_template: None,
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
        input_request_id: unread.input_request_id.clone(),
        input_audience: unread.input_audience,
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
    session_store: &Arc<dyn SessionRepository>,
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
    let contact = admitted_contact_for_turn(request.contact, meta)?;
    let tenant_id = meta.tenant_id;
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    let mut pending_state = load_pending_state(ctx).await?;

    if request.attachments.is_empty()
        && forward_user_input_reply(
            ctx,
            &mut state,
            session_id,
            tenant_id,
            contact_id,
            &identity,
            &request.user_message,
        )
        .await?
    {
        state.persist_into(ctx);
        sync_status(ctx, session_id, &state).await?;
        return Ok(StartTurnResponse {
            turn_id: None,
            queued: false,
        });
    }

    if let Some(active_turn_id) = pending_state.active_turn_id.as_deref() {
        let queued_at = durable_utc_now(ctx).await?;
        let queue_index = pending_state.pending_messages.len();
        append_session_event_deduped(
            ctx,
            session_id,
            Event::QueuedMessage {
                text: request.user_message.clone(),
                attachments: request.attachments.clone(),
                queued_at,
            },
            format!(
                "queued_message:{active_turn_id}:{queue_index}:{}",
                queued_at.timestamp_micros()
            ),
        )
        .await?;
        pending_state.pending_messages.push_back(PendingMessage {
            queued_at,
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            execution_template: request.execution_template,
        });
        persist_pending_state(ctx, &pending_state);
        return Ok(StartTurnResponse {
            turn_id: None,
            queued: true,
        });
    }

    let turn_id = generate_turn_id(ctx);
    pending_state.active_turn_id = Some(turn_id.clone());
    let now = durable_utc_now(ctx).await?;
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
    state.persist_into(ctx);
    persist_pending_state(ctx, &pending_state);
    sync_status(ctx, session_id, &state).await?;
    dispatch_turn_execution(
        ctx,
        RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id: turn_id.clone(),
            identity,
            contact,
            user_message: request.user_message,
            attachments: request.attachments,
            model: request.model,
            max_turns: request.max_turns,
            trigger: TurnTrigger::UserMessage,
            child_signal_id: None,
            execution_template: request.execution_template,
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

    Ok(StartTurnResponse {
        turn_id: Some(turn_id),
        queued: false,
    })
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

#[cfg(test)]
mod tests {
    use moa_core::{
        types::channel::Channel, types::contact::ContactId, types::contact::ContactRef,
        types::contact::ContactVerificationState, types::identifiers::ModelId,
        types::identifiers::SessionId, types::identifiers::TenantId, types::session::SessionMeta,
    };
    use restate_sdk::prelude::TerminalError;

    use super::{
        active_turn_progress_or_none, admitted_contact_for_turn, worker_provide_input_request,
    };

    #[test]
    fn session_worker_reply_payload_carries_exact_parent_session_and_string() {
        // Pins: Session plain-reply routing sends the exact owning Session scope and keeps the
        // canonical Value::String payload expected by Worker replay hashing.
        let parent_session = SessionId::new();
        let request = worker_provide_input_request(parent_session, "request-9", "the exact answer");

        assert_eq!(request.parent_session, parent_session);
        assert_eq!(request.input_request_id, "request-9");
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

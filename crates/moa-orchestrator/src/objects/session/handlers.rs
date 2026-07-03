//! Restate handlers for the Session VO.

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
            },
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
        annotate_restate_handler_span("Session", "cancel");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let scope = scope.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        state.set_cancel_flag(scope);
        let children = state.children.clone();
        state.persist(&ctx);
        // Both scopes cancel the active coordinator turn.
        if let Some(turn_id) = load_pending_state(&ctx).await?.active_turn_id {
            ctx.workflow_client::<TurnExecutionClient>(turn_id)
                .request_cancel(Json::from("session cancel requested".to_string()))
                .send();
        }
        // Only `TaskTree` cascades to the registered children; `CoordinatorOnly` leaves them running.
        if scope.cancels_task_tree() {
            for child in children {
                ctx.object_client::<WorkerClient>(child.id)
                    .cancel("parent session cancelled".to_string())
                    .send();
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
        annotate_restate_handler_span("Session", "start_turn");
        Ok(Json::from(
            start_turn_inner(&mut ctx, request.into_inner()).await?,
        ))
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<ExecutionTurnOutcome>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "record_turn_outcome");
        let outcome = outcome.into_inner();
        let mut pending_state = load_pending_state(&ctx).await?;
        let matches_active =
            pending_state.active_turn_id.as_deref() == Some(outcome.turn_id.as_str());
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;

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
        if matches_active && state.clear_auto_delegation_on_synthesis_outcome(&outcome.turn_id) {
            tracing::debug!(
                key = %ctx.key(),
                turn_id = %outcome.turn_id,
                "cleared completed auto-delegation synthesis run"
            );
        }

        if matches_active
            && matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed)
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
                },
            );
            return Ok(());
        }

        if matches_active {
            let now = durable_utc_now(&ctx).await?;
            let resumed = if matches!(outcome.kind, ExecutionTurnOutcomeKind::Completed) {
                dispatch_auto_delegation_synthesis_if_idle(
                    &mut ctx,
                    &mut pending_state,
                    &mut state,
                    session_id,
                    now,
                )
                .await?
                    || dispatch_queued_parent_resume_if_idle(
                        &mut ctx,
                        &mut pending_state,
                        &mut state,
                        session_id,
                        now,
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
                        let completed_status = if state.auto_delegation_waiting_for_workers() {
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
                }
            }
            state.persist(&ctx);
            sync_status(&ctx, session_id, &state).await?;
        }
        persist_pending_state(&ctx, &pending_state);
        resolve_turn_waiters(&ctx, turn_waiters, &outcome)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn has been admitted by Session.
    async fn attach_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachSessionTurnWaiterInput>,
    ) -> Result<Json<AttachSessionTurnWaiterOutput>, HandlerError> {
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

        ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
            .request_cancel(reason)
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
            },
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
        annotate_restate_handler_span("Session", "snapshot");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let pending_state = load_pending_state(&ctx).await?;
        Ok(Json::from(SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
        }))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn progress(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        annotate_restate_handler_span("Session", "progress");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        let event_range = request.into_inner().normalized_event_range();
        let pending_state = load_pending_state(&ctx).await?;
        let children = SessionVoState::load_children(&ctx).await?;
        let active_turn_id = pending_state.active_turn_id.clone();
        let snapshot = SessionSnapshot {
            session_id: ctx.key().to_string(),
            active_turn_id: pending_state.active_turn_id,
            pending_message_count: pending_state.pending_messages.len() as u64,
            last_outcome: pending_state.last_outcome,
        };
        let events = load_progress_events(&ctx, session_id, event_range).await?;
        let active_turn_progress = if let Some(turn_id) = active_turn_id {
            active_turn_progress_or_none(
                &turn_id,
                ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
                    .progress()
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
        annotate_restate_handler_span("Session", "register_child");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let child = child.into_inner();
        let worker_id = child.id.clone();
        if state.register_child(child) {
            // Active edge: a child just became active, so ensure one narration tick is
            // outstanding (single-outstanding guard prevents overlapping schedules).
            narration::ensure_narration_tick_scheduled(&ctx, &mut state).await?;
            // Active edge: schedule one single-outstanding per-child heartbeat-liveness
            // watchdog so a stuck child is detected without polling across sessions.
            liveness::ensure_child_liveness_scheduled(&ctx, &mut state, &worker_id).await?;
            state.persist(&ctx);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn register_auto_delegation_run(
        &self,
        mut ctx: ObjectContext<'_>,
        input: Json<RegisterAutoDelegationRunInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "register_auto_delegation_run");
        let session_id = parse_session_key(ctx.key())?;
        let input = input.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let mut pending_state = load_pending_state(&ctx).await?;
        let changed = state.register_auto_delegation_run(input.user_sequence_num, input.worker_ids);
        maybe_complete_auto_delegation_run(&mut ctx, &mut pending_state, &mut state, session_id)
            .await?;
        if changed || state.auto_delegation_run.is_some() {
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn poll_auto_delegation_fan_in(
        &self,
        mut ctx: ObjectContext<'_>,
        input: Json<PollAutoDelegationFanInInput>,
    ) -> Result<Json<AutoDelegationFanInStatus>, HandlerError> {
        annotate_restate_handler_span("Session", "poll_auto_delegation_fan_in");
        let session_id = parse_session_key(ctx.key())?;
        let input = input.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let mut pending_state = load_pending_state(&ctx).await?;

        // Only the run for the polling root turn's own user sequence is fanned in here.
        let matches = state
            .auto_delegation_run
            .as_ref()
            .is_some_and(|run| run.user_sequence_num == input.user_sequence_num);
        if !matches {
            return Ok(Json::from(AutoDelegationFanInStatus::NotRunning));
        }

        // Fan-in wait bound exceeded for a stuck worker: fail out any worker still not terminal
        // so the run can complete and the coordinator synthesizes from the workers that did
        // finish, instead of hanging the whole session until the turn/session timeout.
        if input.force_complete {
            let failed = state.fail_pending_auto_delegation_workers(
                "worker did not report a terminal result before the fan-in wait bound",
            );
            if failed > 0 {
                tracing::warn!(
                    key = %ctx.key(),
                    user_sequence_num = input.user_sequence_num,
                    failed,
                    "auto-delegation fan-in bound exceeded; failed out stuck worker(s) to unblock synthesis"
                );
            }
        }

        // Emit the durable bundle from run-owned snapshots if the run is complete; idempotent
        // if the Session-VO terminal path already emitted it. Does not dispatch synthesis
        // because the root turn is the active turn.
        maybe_complete_auto_delegation_run(&mut ctx, &mut pending_state, &mut state, session_id)
            .await?;

        let status = if state
            .auto_delegation_run
            .as_ref()
            .is_some_and(|run| run.bundle_emitted)
        {
            // The active root turn owns synthesis: claim it so `record_turn_outcome` clears the
            // run instead of dispatching a duplicate synthesis turn. Idempotent once claimed.
            state.record_auto_delegation_synthesis_dispatch(input.root_turn_id);
            AutoDelegationFanInStatus::Ready
        } else if let Some(worker_id) = state.pending_auto_delegation_worker() {
            AutoDelegationFanInStatus::Pending { worker_id }
        } else {
            AutoDelegationFanInStatus::NotRunning
        };

        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        Ok(Json::from(status))
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn remove_child(
        &self,
        ctx: ObjectContext<'_>,
        worker_id: String,
    ) -> Result<(), HandlerError> {
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
        mut ctx: ObjectContext<'_>,
        input: Json<MarkWorkerChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "mark_child_terminal");
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let mut pending_state = load_pending_state(&ctx).await?;
        let input = input.into_inner();
        let worker_id = input.worker_id.clone();
        if state.mark_child_terminal(input) {
            claim_check_child_output(&ctx, &mut state, session_id, &worker_id).await?;
            maybe_complete_auto_delegation_run(
                &mut ctx,
                &mut pending_state,
                &mut state,
                session_id,
            )
            .await?;
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
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
                hydrate_child_terminal_output(&ctx, session_id, terminal, claim_check).await?;
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

        // Never-terminal recovery: a stuck worker (HeartbeatStale) that belongs to an un-emitted
        // auto-delegation run is failed out here and the run is completed. The root turn's inline
        // fan-in bound only force-completes while that turn is alive; once it has ended (e.g. this
        // stale signal itself dispatches a resume, or the turn hit its cap) this is the only path
        // that emits the bundle, so a never-terminal worker cannot leave the session waiting
        // indefinitely. Runs BEFORE the active-turn reload so the resume gate below observes any
        // synthesis turn this dispatches and does not start a duplicate.
        if signal.kind == ChildSignalKind::HeartbeatStale
            && state.fail_auto_delegation_worker(
                &signal.worker_id,
                "worker heartbeat went stale and it never reported a terminal result",
            )
        {
            let mut pending_state = load_pending_state(&ctx).await?;
            maybe_complete_auto_delegation_run(
                &mut ctx,
                &mut pending_state,
                &mut state,
                session_id,
            )
            .await?;
            state.persist(&ctx);
            persist_pending_state(&ctx, &pending_state);
            sync_status(&ctx, session_id, &state).await?;
        }

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

        // Idempotence: a retried delivery of an already-armed signal must not start a
        // second resume turn. `maybe_arm_parent_resume` also re-blocks once the dispatched
        // turn is active, but this short-circuits even before the turn becomes active.
        let already_pending = state.pending_parent_resume_signal == Some(signal.signal_id);
        let config = OrchestratorCtx::current_config();
        let limits = &config.session_limits;
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
        annotate_restate_handler_span("Session", "destroy");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&ctx, session_id).await?;
        // Reclaim any coordinator/orphan hands still leased under this session before the
        // VO state is cleared. The Session VO holds no `ToolRouter`, so this is dispatched
        // detached (fire-and-forget) to the ToolExecutor service that owns the router. It is
        // non-fatal; without this caller durable leases reclaim only via their 1-hour TTL.
        ctx.service_client::<ToolExecutorClient>()
            .release_session_hands(Json::from(ReleaseSessionHandsRequest { session_id }))
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
        annotate_restate_handler_span("Session", "narration_tick");
        narration::run_narration_tick(&ctx, req.into_inner().generation).await
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
        annotate_restate_handler_span("Session", "check_child_liveness");
        liveness::run_child_liveness_check(&ctx, req.into_inner()).await
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
    let mut summaries = Vec::new();
    for item in plan_child_progress_fan_in(children, MAX_WORKER_FAN_OUT) {
        match item {
            ChildProgressFetch::Ready(summary) => summaries.push(summary),
            ChildProgressFetch::Fetch(child_id) => {
                match ctx
                    .object_client::<WorkerClient>(child_id.clone())
                    .progress_summary()
                    .call()
                    .await
                {
                    Ok(summary) => summaries.push(summary.into_inner()),
                    Err(error) => tracing::warn!(
                        child_id = %child_id,
                        error = %error,
                        "child progress summary unavailable; omitting from fan-in"
                    ),
                }
            }
        }
    }
    summaries
}

async fn forward_user_input_reply(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    text: &str,
) -> Result<Option<UnreadChildSignal>, HandlerError> {
    // Only auto-forward the message as a `ProvideInput` reply when EXACTLY ONE worker is
    // awaiting user input. With zero pending questions there is nothing to answer, and with more
    // than one the target is ambiguous — in both cases the message must start a normal
    // coordinator turn rather than being silently swallowed into an unrelated worker.
    let signal = {
        let mut awaiting = state.unread_child_signals.iter().filter(|signal| {
            signal.kind == ChildSignalKind::NeedsInput
                && signal.input_audience == Some(InputAudience::User)
                && signal.input_request_id.is_some()
        });
        let Some(signal) = awaiting.next() else {
            return Ok(None);
        };
        if awaiting.next().is_some() {
            return Ok(None);
        }
        signal.clone()
    };
    let Some(input_request_id) = signal.input_request_id.clone() else {
        return Ok(None);
    };

    ctx.object_client::<WorkerClient>(signal.worker_id.clone())
        .post_message(Json::from(WorkerMessage::ProvideInput {
            input_request_id: input_request_id.clone(),
            text: text.to_string(),
        }))
        .call()
        .await?;
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerMessageSent {
            worker_id: signal.worker_id.clone(),
            input_request_id: Some(input_request_id.clone()),
            text: text.to_string(),
        },
        format!("worker_input_reply:{input_request_id}"),
    )
    .await?;
    state.clear_unread_child_signal(signal.signal_id);
    tracing::info!(
        session_id = %session_id,
        worker_id = %signal.worker_id,
        input_request_id = %input_request_id,
        "forwarded user reply to worker input request"
    );
    Ok(Some(signal))
}

/// Offloads a just-marked terminal child's large output to a content-addressed blob.
///
/// A no-op unless the child's output exceeds the claim-check threshold. The full body is
/// stored via a journaled `ctx.run` (content-addressed, so the recorded blob id is
/// deterministic and reused on replay) and the inline `children` copy is compacted to a
/// preview. The separate auto-delegation run copy keeps the full output for the bundle event.
async fn claim_check_child_output(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    session_id: SessionId,
    worker_id: &str,
) -> Result<(), HandlerError> {
    let Some(full_output) = state.large_child_terminal_output(worker_id) else {
        return Ok(());
    };
    let store = OrchestratorCtx::current_session_store();
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
) -> Result<(), HandlerError> {
    let name = format!(
        "child_terminal_output_hydrate_{}",
        terminal.result.worker_id
    );
    let store = OrchestratorCtx::current_session_store();
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

async fn maybe_complete_auto_delegation_run(
    ctx: &mut ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    state: &mut SessionVoState,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let Some((user_sequence_num, results)) = state.ready_auto_delegation_bundle() else {
        return Ok(());
    };
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerResultBundle {
            user_sequence_num,
            results,
        },
        format!("auto_delegation_fan_in:{user_sequence_num}"),
    )
    .await?;
    state.mark_auto_delegation_bundle_emitted(user_sequence_num);
    if pending_state.active_turn_id.is_none() {
        let now = durable_utc_now(ctx).await?;
        dispatch_auto_delegation_synthesis_if_idle(ctx, pending_state, state, session_id, now)
            .await?;
    }
    Ok(())
}

async fn dispatch_auto_delegation_synthesis_if_idle(
    ctx: &mut ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    state: &mut SessionVoState,
    session_id: SessionId,
    now: DateTime<Utc>,
) -> Result<bool, HandlerError> {
    if pending_state.active_turn_id.is_some() {
        return Ok(false);
    }
    let Some(user_sequence_num) = state.pending_auto_delegation_synthesis_sequence() else {
        return Ok(false);
    };
    let Some(identity) = state.owning_identity.clone() else {
        tracing::warn!(
            key = %ctx.key(),
            user_sequence_num,
            "auto-delegation bundle is ready but no owning identity is recorded"
        );
        return Ok(false);
    };

    let turn_id = generate_turn_id(ctx);
    let instruction = build_auto_delegation_synthesis_instruction(user_sequence_num);
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerResultSynthesisRequested {
            user_sequence_num,
            turn_id: turn_id.clone(),
            reason: instruction.clone(),
        },
        format!("worker_result_synthesis:{user_sequence_num}"),
    )
    .await?;
    pending_state.active_turn_id = Some(turn_id.clone());
    state.set_status(SessionStatus::Running, now);
    state.record_auto_delegation_synthesis_dispatch(turn_id.clone());
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
            trigger: TurnTrigger::WorkerResults,
            child_signal_id: None,
        },
    );
    tracing::info!(
        key = %ctx.key(),
        user_sequence_num,
        turn_id = %turn_id,
        "dispatched auto-delegation result synthesis turn"
    );
    Ok(true)
}

fn build_auto_delegation_synthesis_instruction(user_sequence_num: u64) -> String {
    format!(
        "Auto-delegated worker results are complete for user message sequence {user_sequence_num}. \
         Produce the final answer from the <worker_result_bundle> in the session history. \
         Do not call list_workers or wait_worker for these workers."
    )
}

async fn dispatch_queued_parent_resume_if_idle(
    ctx: &mut ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    state: &mut SessionVoState,
    session_id: SessionId,
    now: DateTime<Utc>,
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
    let config = OrchestratorCtx::current_config();
    let limits = &config.session_limits;
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
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
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
) -> Result<StartTurnResponse, HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let identity = require_session_participant(ctx, session_id).await?;
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;
    let contact = admitted_contact_for_turn(request.contact, meta)?;
    let mut pending_state = load_pending_state(ctx).await?;

    if request.attachments.is_empty()
        && forward_user_input_reply(ctx, &mut state, session_id, &request.user_message)
            .await?
            .is_some()
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
    narration::ensure_narration_tick_scheduled(ctx, &mut state).await?;
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
        Channel, ContactId, ContactRef, ContactVerificationState, ModelId, SessionMeta, TenantId,
    };
    use restate_sdk::prelude::TerminalError;

    use super::{active_turn_progress_or_none, admitted_contact_for_turn};

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

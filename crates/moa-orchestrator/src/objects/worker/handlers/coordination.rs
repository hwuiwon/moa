//! Worker waiter, input, terminal-delivery, and action-review handlers.

use super::*;

impl WorkerImpl {
    pub(super) async fn attach_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachWorkerResultWaiterInput>,
    ) -> Result<Json<AttachWorkerResultWaiterOutput>, HandlerError> {
        annotate_restate_handler_span("Worker", "attach_result_waiter");
        let input = input.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        // A worker holding an unresolved review is not finished, so a parent that waits
        // on it must be parked rather than handed a result that does not yet include
        // the reviewed action's outcome.
        if !state.action_review_holds_lifecycle()
            && let Some(terminal) = state.terminal_result(ctx.key().to_string())
        {
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

    pub(super) async fn remove_result_waiter(
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

    // SAFETY: internal control-plane write invoked only by this child's own turn
    // workflow when the child model calls `request_input`. It records the awakeable id
    // backing the round-trip on the child's own VO state and reads no caller-owned data.
    pub(super) async fn register_input_request(
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

    // SAFETY: internal control-plane write invoked only by this child's own turn
    // workflow when its `request_input` wait times out. It clears the child's own pending
    // input mapping and reads no caller-owned data.
    pub(super) async fn clear_input_request(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<WorkerClearInputRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "clear_input_request");
        let request = request.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let parent_session = state.parent_session;
        let Some(cleared) =
            state.clear_input_request_for_workflow(&request.target, &request.waiting_workflow_id)
        else {
            return Ok(());
        };
        state.persist(&ctx);
        if let Some(parent_session) = parent_session {
            retract_session_input_targets(&ctx, parent_session, vec![cleared.target()]);
        }
        Ok(())
    }

    pub(super) async fn record_turn_outcome(
        &self,
        mut ctx: ObjectContext<'_>,
        outcome: Json<moa_wire::turn::TurnOutcome>,
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

        // A turn that reported its outcome is no longer parked on anything it
        // registered, so its own round-trips die with it; a terminal worker's
        // remaining registrations die too. Both retract their advertised targets.
        let mut cleared_inputs = if matches_active {
            state.clear_input_requests_for_turn(&outcome.turn_id)
        } else {
            Vec::new()
        };

        let terminal_status = matches!(
            state.current_status(),
            WorkerState::Failed | WorkerState::Cancelled
        );
        // A failed or cancelled worker runs no continuation, and its held reviews must
        // be released here — otherwise the lifecycle gate below would keep a dead
        // worker nonterminal forever and its parent would never get a report. Its
        // remaining input round-trips are equally dead and give up their targets too.
        if terminal_status {
            cleared_inputs.extend(state.clear_all_input_requests());
            let discarded = state.discard_action_reviews();
            if discarded > 0 {
                tracing::info!(
                    key = %ctx.key(),
                    turn_id = %outcome.turn_id,
                    discarded,
                    "released action reviews held by a worker that did not complete"
                );
            }
        }
        let should_restart = matches_active && !state.pending.is_empty() && !terminal_status;
        // A queued continuation runs before ordinary buffered work only when the
        // worker is otherwise idle; a pending parent message is newer instruction and
        // already advanced the generation, so it wins.
        let continuation = if matches_active && !should_restart && !terminal_status {
            state.take_action_review_continuation()
        } else {
            None
        };
        // A continuation keeps the turn id minted when its review resolved, because
        // that is the id the durable continuation fact already named.
        let next_turn = match (&continuation, should_restart) {
            (Some(queued), _) => Some((queued.turn_id.clone(), Some(queued.continuation.clone()))),
            (None, true) => Some((generate_turn_id(&mut ctx), None)),
            (None, false) => None,
        };
        let generation = state.generation;
        if let Some((turn_id, _)) = next_turn.as_ref() {
            let _started = state.start_workflow_turn(turn_id.clone());
            activate_worker_security_owner(&mut state, ctx.key(), turn_id, generation);
        }
        let max_turns = state.max_turns;
        let identity = state
            .identity
            .clone()
            .ok_or_else(|| TerminalError::new("worker is missing its admitted caller identity"))?;
        let parent_session = required_parent_session(&state)?;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist(&ctx);

        retract_session_input_targets(
            &ctx,
            parent_session,
            cleared_inputs
                .iter()
                .map(WorkerPendingInput::target)
                .collect(),
        );

        if let Some((turn_id, action_review)) = next_turn {
            start_worker_turn_execution(
                &ctx,
                WorkerTurnDispatch {
                    turn_id,
                    identity,
                    parent_session,
                    generation,
                    max_turns,
                    trusted_sandbox_manifest,
                    action_review,
                },
            );
            return Ok(());
        }
        maybe_resolve_parent_awakeable(&ctx, &self.session_limits).await
    }

    // SAFETY: internal control-plane write from `ActionReviews/request`, which runs
    // only after the owning session admitted the caller and the worker's own turn
    // issued the reviewed tool call. It records the review id on this worker's own VO
    // state and returns no caller-owned data.
    pub(super) async fn register_action_review(
        &self,
        ctx: ObjectContext<'_>,
        registration: Json<ActionReviewRegistration>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "register_action_review");
        let registration = registration.into_inner();
        let turn_id = registration
            .owner
            .turn_id()
            .ok_or_else(|| {
                TerminalError::new("worker action review registration requires an owning turn")
            })?
            .to_string();
        // The generation comes from the worker turn that issued the tool call, not from
        // whatever this worker happens to be on now: a follow-up admitted between the
        // tool call and this registration must not re-stamp a stale review as current.
        let generation = registration.owner.generation().ok_or_else(|| {
            TerminalError::new("worker action review registration requires an owner generation")
        })?;
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if generation < state.generation {
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation,
                current_generation = state.generation,
                "skipped registering an already-superseded worker action review"
            );
            return Ok(());
        }
        if state.register_action_review(registration.review_id, turn_id, generation) {
            state.persist(&ctx);
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation = state.generation,
                "registered pending action review on worker"
            );
        }
        Ok(())
    }

    // SAFETY: internal control-plane write from `ActionReviews/decide`, which
    // authorizes the deciding tenant admin before resolving. It writes only this
    // worker's own state plus its own parent-session event log.
    pub(super) async fn action_review_resolved(
        &self,
        mut ctx: ObjectContext<'_>,
        receipt: Json<ActionReviewReceipt>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "action_review_resolved");
        let receipt = receipt.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        // Unknown or already-resolved review: a duplicated callback changes nothing.
        let Some(registered) = state.resolve_action_review(receipt.review_id) else {
            tracing::debug!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                "ignored resolution for an unknown or already-resolved worker action review"
            );
            return Ok(());
        };
        let superseded = registered.generation != state.generation;
        let terminal_status = matches!(
            state.current_status(),
            WorkerState::Failed | WorkerState::Cancelled
        );
        if superseded || terminal_status {
            // Releasing the lifecycle is the point: the review was holding this
            // worker's terminal report open, and it no longer may.
            state.persist(&ctx);
            tracing::info!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                registered_generation = registered.generation,
                current_generation = state.generation,
                superseded,
                terminal_status,
                "released worker action review without a continuation"
            );
            return maybe_resolve_parent_awakeable(&ctx, &self.session_limits).await;
        }

        let parent_session = required_parent_session(&state)?;
        let directive = receipt.system_directive();
        // The continuation turn id is minted now, before any scheduling decision, so
        // the durable continuation fact names the exact turn that will run it even
        // when the worker is busy and the turn starts later.
        let continuation_turn_id = generate_turn_id(&mut ctx);
        let entry = QueuedActionReviewContinuation {
            continuation: ActionReviewContinuation { receipt },
            turn_id: continuation_turn_id,
            generation: registered.generation,
            ordinal: registered.ordinal,
        };
        let fact = entry.clone();
        if !state.queue_action_review_continuation(entry) {
            state.persist(&ctx);
            return Ok(());
        }
        // The worker's own history is VO-local, so the directive is folded in here;
        // the parent-session fact appended below is what operators and the session
        // stream observe.
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::system(
                directive,
            )));
        let dispatch = if state.active_turn_id.is_some() {
            None
        } else {
            state.take_action_review_continuation()
        };
        let generation = state.generation;
        if let Some(dispatch) = dispatch.as_ref() {
            let _started = state.start_workflow_turn(dispatch.turn_id.clone());
            activate_worker_security_owner(&mut state, ctx.key(), &dispatch.turn_id, generation);
        }
        let identity = state
            .identity
            .clone()
            .ok_or_else(|| TerminalError::new("worker is missing its admitted caller identity"))?;
        let max_turns = state.max_turns;
        let trusted_sandbox_manifest = state.trusted_sandbox_manifest.clone();
        state.persist(&ctx);

        append_action_review_continuation_fact(
            &ctx,
            parent_session,
            &fact.continuation,
            &fact.turn_id,
        )
        .await?;

        if let Some(dispatch) = dispatch {
            start_worker_turn_execution(
                &ctx,
                WorkerTurnDispatch {
                    turn_id: dispatch.turn_id,
                    identity,
                    parent_session,
                    generation,
                    max_turns,
                    trusted_sandbox_manifest,
                    action_review: Some(dispatch.continuation),
                },
            );
        }
        Ok(())
    }

    // SAFETY: internal control-plane release from the action-review reaper or
    // security circuit. It removes only the matching review registration owned by
    // this worker and does not create a continuation for that review.
    pub(super) async fn release_action_review(
        &self,
        ctx: ObjectContext<'_>,
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "release_action_review");
        let release = release.into_inner();
        if !matches!(
            &release.owner,
            moa_core::types::action_policy::ActionReviewOwner::Worker { worker_id, .. }
                if worker_id.as_str() == ctx.key()
        ) {
            return Err(
                TerminalError::new("action review release does not belong to this worker").into(),
            );
        }
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if state.resolve_action_review(release.review_id).is_none() {
            return Ok(());
        }
        let terminal_status = matches!(
            state.current_status(),
            WorkerState::Failed | WorkerState::Cancelled
        );
        let dispatch =
            if release.resume_queued && state.active_turn_id.is_none() && !terminal_status {
                state.take_action_review_continuation()
            } else {
                None
            };
        let generation = state.generation;
        if let Some(dispatch) = dispatch.as_ref() {
            let _started = state.start_workflow_turn(dispatch.turn_id.clone());
            activate_worker_security_owner(&mut state, ctx.key(), &dispatch.turn_id, generation);
        }
        let dispatch_context = if dispatch.is_some() {
            Some((
                required_parent_session(&state)?,
                generation,
                state.identity.clone().ok_or_else(|| {
                    TerminalError::new("worker is missing its admitted caller identity")
                })?,
                state.max_turns,
                state.trusted_sandbox_manifest.clone(),
            ))
        } else {
            None
        };
        state.persist(&ctx);
        if let (
            Some(dispatch),
            Some((parent_session, generation, identity, max_turns, trusted_sandbox_manifest)),
        ) = (dispatch, dispatch_context)
        {
            start_worker_turn_execution(
                &ctx,
                WorkerTurnDispatch {
                    turn_id: dispatch.turn_id,
                    identity,
                    parent_session,
                    generation,
                    max_turns,
                    trusted_sandbox_manifest,
                    action_review: Some(dispatch.continuation),
                },
            );
            Ok(())
        } else {
            maybe_resolve_parent_awakeable(&ctx, &self.session_limits).await
        }
    }

    pub(super) async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "worker VO state cleared");
        Ok(())
    }
}

/// Appends the deduped continuation fact to the owner's session event log.
///
/// One review yields exactly one continuation fact: the dedupe key makes a
/// replayed or duplicated resolution callback append nothing.
pub(super) async fn append_action_review_continuation_fact(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    continuation: &ActionReviewContinuation,
    turn_id: &str,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: Event::ActionReviewContinuationRequested {
                    review_id: continuation.receipt.review_id,
                    turn_id: turn_id.to_string(),
                    receipt: continuation.receipt.clone(),
                },
                dedupe_key: Some(action_review_continuation_dedupe_key(
                    continuation.receipt.review_id,
                )),
            })),
    )
    .call()
    .await?;
    Ok(())
}

pub(super) async fn maybe_resolve_parent_awakeable(
    ctx: &ObjectContext<'_>,
    session_limits: &SessionLimitsConfig,
) -> Result<(), HandlerError> {
    let mut state = Tracked::<WorkerVoState>::load(ctx).await?;
    // A worker whose own action is still awaiting (or has just been granted) a
    // tenant-admin decision is NOT finished, even though its model loop returned.
    // Resolving parent waiters, emitting the terminal report, or scheduling cleanup
    // here would strand the approved action's answer and destroy the local history
    // the continuation turn needs.
    if state.action_review_holds_lifecycle() {
        tracing::info!(
            key = %ctx.key(),
            generation = state.generation,
            "worker stays nonterminal while a current-generation action review is unresolved"
        );
        return Ok(());
    }
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

pub(super) async fn deliver_terminal_notification_once(
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
pub(super) async fn emit_terminal_idle_wake(
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
    crate::restate_identity::replay_safe_request(
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
                input_request: None,
            })),
    )
    .send();
    Ok(())
}

pub(super) async fn cache_parent_terminal_result(
    ctx: &ObjectContext<'_>,
    state: &WorkerVoState,
    terminal: WorkerTerminalResult,
) -> Result<(), HandlerError> {
    let input = MarkWorkerChildTerminalInput {
        worker_id: terminal.result.worker_id.clone(),
        terminal,
    };
    if let Some(parent_session) = state.parent_session {
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionClient>(parent_session.to_string())
                .mark_child_terminal(Json::from(input)),
        )
        .call()
        .await?;
    }
    Ok(())
}

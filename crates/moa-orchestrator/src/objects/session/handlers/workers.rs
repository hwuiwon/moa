//! Workers handlers for the Session virtual object.

use super::*;

impl SessionImpl {
    pub(super) async fn handle_register_child(
        &self,
        ctx: ObjectContext<'_>,
        child: Json<WorkerChildRef>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "register_child");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let child = child.into_inner();
        if state.register_child(child) {
            let mut fan_in = WorkerFanInState::load(&ctx).await?;
            fan_in.register_child(&state.children);
            fan_in.persist(&ctx);
            state.persist(&ctx);
        }
        Ok(())
    }

    pub(super) async fn handle_remove_child(
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

    pub(super) async fn handle_clear_worker_input_targets(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ClearWorkerInputTargetsInput>,
    ) -> Result<(), HandlerError> {
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

    pub(super) async fn handle_record_worker_child_terminal(
        &self,
        mut ctx: ObjectContext<'_>,
        input: Json<RecordWorkerChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "record_worker_child_terminal");
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let input = input.into_inner();
        validate_worker_terminal_delivery(&input)?;
        let worker_id = input.worker_id.clone();
        if !state.owns_child(&worker_id) {
            return Err(TerminalError::new(format!(
                "worker terminal delivery names unregistered child {worker_id}"
            ))
            .into());
        }
        let mut fan_in = WorkerFanInState::load(&ctx).await?;
        let record = fan_in.record_terminal(&mut state, &input);
        let WorkerTerminalRecord::Accepted { settled } = record else {
            record_worker_terminal_delivery(WorkerTerminalDeliveryResult::Duplicate);
            tracing::debug!(
                key = %ctx.key(),
                worker_id = %worker_id,
                generation = input.generation,
                "ignored duplicate worker terminal delivery"
            );
            return Ok(());
        };
        record_worker_terminal_delivery(WorkerTerminalDeliveryResult::Accepted);
        if let Some(settled) = settled {
            let kind = match settled {
                WorkerState::Completed => WorkerFanInSettledKind::Completed,
                WorkerState::Cancelled => WorkerFanInSettledKind::Cancelled,
                WorkerState::Uninitialized | WorkerState::Running | WorkerState::Failed => {
                    return Err(TerminalError::new(
                        "worker fan-in settled with a non-success terminal state",
                    )
                    .into());
                }
            };
            record_worker_fan_in_settled(kind);
        }
        claim_check_child_output(
            &ctx,
            &mut state,
            session_id,
            &worker_id,
            &self.session_store,
        )
        .await?;
        fan_in.persist(&ctx);

        append_worker_terminal_events(&ctx, session_id, &input).await?;
        let parent_idle = load_pending_state(&ctx).await?.active_turn_id.is_none();
        let signal = worker_terminal_signal(
            session_id,
            &input,
            settled,
            parent_idle,
            state.children.len(),
        );
        if let Some(signal) = signal {
            self.record_owned_child_signal(&mut ctx, session_id, &mut state, signal)
                .await?;
        } else {
            state.persist(&ctx);
        }
        Ok(())
    }

    pub(super) async fn handle_consume_child_result(
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

    pub(super) async fn handle_child_refs(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<WorkerChildRef>>, HandlerError> {
        annotate_restate_handler_span("Session", "child_refs");
        Ok(Json::from(SessionVoState::load_children(&ctx).await?))
    }

    pub(super) async fn handle_record_child_signal(
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

        self.record_owned_child_signal(&mut ctx, session_id, &mut state, signal)
            .await
    }

    /// Records one already-validated child signal and owns its guarded resume decision.
    async fn record_owned_child_signal(
        &self,
        ctx: &mut ObjectContext<'_>,
        session_id: SessionId,
        state: &mut Tracked<SessionVoState>,
        signal: WorkerSignal,
    ) -> Result<(), HandlerError> {
        // Idempotent append: a retried delivery with the same signal_id is a no-op at the
        // event log (dedupe table, Task 2), so it never double-records the signal.
        append_session_event_deduped(
            ctx,
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
        let active_turn_id = load_pending_state(ctx).await?.active_turn_id;

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
        let now = durable_utc_now(ctx).await?;
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
                    .acquire(ctx, session_id, tenant_id, "turn_admission_child_resume")
                    .await?;
                let turn_id = generate_turn_id(ctx);
                let instruction = build_resume_instruction(&signal, &state.unread_child_signals);
                // Durable, idempotent control record that seeds the resume turn's prompt
                // (the brain renders this event's `reason` instead of a fake user message).
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
                // Mirror `start_turn_inner` bookkeeping so a concurrent/queued message sees
                // an active turn and no second root turn can start.
                let mut pending_state = load_pending_state(ctx).await?;
                pending_state.active_turn_id = Some(turn_id.clone());
                activate_coordinator_security_owner(state, &turn_id, pending_state.turn_generation);
                arm_turn_admission_heartbeat(ctx, &mut pending_state, &self.turn_admission);
                state.set_status(SessionStatus::Running, now);
                state.record_resume_dispatch(turn_id.clone(), now, limits.worker_resume_window_ms);
                let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
                state.persist(ctx);
                persist_pending_state(ctx, &pending_state);
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
                        resource_budget: ResourceBudget::UNBOUNDED,
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
        } else if already_pending {
            tracing::debug!(
                key = %ctx.key(),
                signal_id = %signal.signal_id,
                "duplicate child signal for an already-pending resume; no second dispatch"
            );
        }

        state.persist(ctx);
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
}

fn validate_worker_terminal_delivery(
    input: &RecordWorkerChildTerminalInput,
) -> Result<(), HandlerError> {
    if input.generation == 0 {
        return Err(TerminalError::new(
            "worker terminal delivery requires a non-zero admission generation",
        )
        .into());
    }
    if input.terminal.result.worker_id != input.worker_id {
        return Err(TerminalError::new(
            "worker terminal delivery result does not match its worker id",
        )
        .into());
    }
    if !matches!(
        input.terminal.state,
        WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
    ) {
        return Err(TerminalError::new(
            "worker terminal delivery carries a non-terminal lifecycle state",
        )
        .into());
    }
    Ok(())
}

async fn append_worker_terminal_events(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    input: &RecordWorkerChildTerminalInput,
) -> Result<(), HandlerError> {
    let summary = input
        .terminal
        .result
        .error
        .clone()
        .unwrap_or_else(|| input.terminal.result.output.clone());
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerStatusChanged {
            worker_id: input.worker_id.clone(),
            from: None,
            to: input.terminal.state,
            summary: Some(summary.clone()),
        },
        format!("worker_terminal_status:{}", input.signal_id),
    )
    .await?;
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerNotificationDelivered {
            worker_id: input.worker_id.clone(),
            state: input.terminal.state,
            summary,
        },
        format!("worker_terminal_notification:{}", input.signal_id),
    )
    .await
}

fn worker_terminal_signal(
    parent_session: SessionId,
    input: &RecordWorkerChildTerminalInput,
    settled: Option<WorkerState>,
    parent_idle: bool,
    child_count: usize,
) -> Option<WorkerSignal> {
    let (kind, severity, summary) = if input.terminal.state == WorkerState::Failed {
        let summary = input
            .terminal
            .result
            .error
            .as_deref()
            .filter(|error| !error.trim().is_empty())
            .unwrap_or(&input.terminal.result.output);
        (
            ChildSignalKind::Failed,
            SignalSeverity::Critical,
            compact_terminal_signal_summary(summary, "worker turn failed"),
        )
    } else if settled.is_some() && parent_idle {
        (
            ChildSignalKind::FanInSettled,
            SignalSeverity::Info,
            format!("All {child_count} registered workers have settled."),
        )
    } else {
        return None;
    };
    Some(WorkerSignal {
        signal_id: input.signal_id,
        worker_id: input.worker_id.clone(),
        parent_session,
        kind,
        severity,
        summary,
        payload: serde_json::Value::Null,
        created_at: input.created_at,
        resume_policy: ParentResumePolicy::IfIdle,
        input_request: None,
    })
}

fn compact_terminal_signal_summary(message: &str, fallback: &str) -> String {
    const MAX_CHARS: usize = 200;
    let first_line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(fallback);
    if first_line.chars().count() > MAX_CHARS {
        let truncated: String = first_line.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        first_line.to_string()
    }
}

/// Offloads a just-marked terminal child's large output to a content-addressed blob.
///
/// A no-op unless the child's output exceeds the claim-check threshold. The full body is
/// stored via a journaled `ctx.run` (content-addressed, so the recorded blob id is
/// deterministic and reused on replay) and the inline `children` copy is compacted to a
/// preview.
pub(super) async fn claim_check_child_output(
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
pub(super) async fn hydrate_child_terminal_output(
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

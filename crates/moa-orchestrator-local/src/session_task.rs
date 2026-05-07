//! Background session task loop for local sessions.

use crate::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_session_task(
    context: SessionTaskContext,
    mut signal_rx: mpsc::Receiver<SessionSignal>,
    event_tx: broadcast::Sender<EventRecord>,
    runtime_tx: broadcast::Sender<RuntimeEvent>,
    status: Arc<RwLock<SessionStatus>>,
    mut turn_requested: bool,
    mut queued_messages: Vec<BufferedUserMessage>,
    cancel_token: CancellationToken,
    hard_cancel_token: CancellationToken,
) -> Result<()> {
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &context.config,
        context.session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: context.graph_pool.clone(),
            compaction_llm_provider: Some(
                context.model_router.provider_for(ModelTask::Summarization),
            ),
            query_rewrite_llm_provider: Some(
                context.model_router.provider_for(ModelTask::Summarization),
            ),
            discovered_workspace_instructions: context.discovered_workspace_instructions.clone(),
            tool_schemas: context.tool_router.tool_schemas(),
            lineage: context.lineage.clone(),
        },
    );
    let max_turns = context.config.session_limits.max_turns;
    let loop_detection_threshold = context.config.session_limits.loop_detection_threshold;
    let mut turn_count = 0u32;
    let mut loop_detector = LoopDetector::new(loop_detection_threshold);
    loop {
        if !turn_requested {
            match signal_rx.recv().await {
                Some(SessionSignal::QueueMessage(message)) => {
                    accept_user_message(
                        &context.session_store,
                        &event_tx,
                        context.session_id,
                        message.clone(),
                        false,
                    )
                    .await?;
                    if let Some(signal_id) = resolve_matching_pending_signal(
                        &context.session_store,
                        context.session_id,
                        message.clone(),
                    )
                    .await?
                    {
                        best_effort_resolve_pending_signal(
                            &context.session_store,
                            context.session_id,
                            signal_id,
                        )
                        .await?;
                    } else {
                        tracing::warn!(
                            session_id = %context.session_id,
                            text = %message.text,
                            "live queue message did not have a matching durable pending signal"
                        );
                    }
                    update_status(
                        &context.session_store,
                        &event_tx,
                        &status,
                        context.session_id,
                        SessionStatus::Running,
                    )
                    .await?;
                    turn_requested = true;
                }
                Some(SessionSignal::SoftCancel) | Some(SessionSignal::HardCancel) => {
                    update_status(
                        &context.session_store,
                        &event_tx,
                        &status,
                        context.session_id,
                        SessionStatus::Cancelled,
                    )
                    .await?;
                    let _ = runtime_tx.send(RuntimeEvent::Notice(
                        "Cancelled current generation.".to_string(),
                    ));
                    if let Err(err) = runtime_tx.send(RuntimeEvent::TurnCompleted) {
                        tracing::warn!(
                            ?err,
                            "runtime receiver dropped while sending TurnCompleted (cancel)"
                        );
                    }
                    return Ok(());
                }
                Some(SessionSignal::ApprovalDecided {
                    request_id,
                    decision,
                }) => {
                    append_event(
                        &context.session_store,
                        &event_tx,
                        context.session_id,
                        Event::ApprovalDecided {
                            request_id,
                            sub_agent_id: None,
                            decision,
                            decided_by: "orchestrator".to_string(),
                            decided_at: Utc::now(),
                        },
                    )
                    .await?;
                    turn_requested = true;
                }
                None => return Ok(()),
            }
            continue;
        }
        let turn_counters = Arc::new(TurnReplayCounters::default());
        let turn_counters_scope = turn_counters.clone();
        let turn_directive = scope_turn_replay_counters(turn_counters.clone(), async {
            let session = context
                .session_store
                .get_session(context.session_id)
                .await?;
            let events = context
                .session_store
                .get_events(
                    context.session_id,
                    EventRange::recent(TURN_EVENT_TAIL_LIMIT),
                )
                .await?;
            let turn_number = turn_count as i64 + 1;
            let turn_root_span = session_turn_span(
                &session,
                last_user_message_text(&events),
                turn_number,
                context.config.observability.environment.as_deref(),
            );

            let turn_latency_counters = Arc::new(TurnLatencyCounters::new(turn_root_span.clone()));
            let turn_latency_scope = turn_latency_counters.clone();
            let turn_started = std::time::Instant::now();
            let turn_outcome = scope_turn_latency_counters(turn_latency_counters, async {
                let turn_outcome: Result<TurnDirective> = async {
                    if max_turns > 0 && turn_count >= max_turns {
                        pause_active_session(
                            &context,
                            &event_tx,
                            &runtime_tx,
                            &status,
                            context.session_id,
                            &mut queued_messages,
                            turn_limit_pause_message(turn_count, &events),
                        )
                        .await?;
                        return Ok(TurnDirective::FinishOk);
                    }
                    if !session_requires_processing(&session, &events)
                        && !queued_messages.is_empty()
                        && flush_next_queued_message(
                            &context.session_store,
                            &event_tx,
                            context.session_id,
                            &mut queued_messages,
                        )
                        .await?
                    {
                        turn_requested = true;
                        return Ok(TurnDirective::ContinueLoop);
                    }

                    turn_requested = false;
                    let mut soft_cancel_requested = false;
                    let turn_start_sequence_num =
                        events.last().map(|record| record.sequence_num).unwrap_or(0);
                    let turn_result = run_streamed_turn_with_signals_stepwise_and_lineage(
                        context.session_id,
                        context.session_store.clone(),
                        context.model_router.provider_for(ModelTask::MainLoop),
                        &pipeline,
                        Some(context.tool_router.clone()),
                        &runtime_tx,
                        Some(&event_tx),
                        &mut signal_rx,
                        &mut turn_requested,
                        &mut queued_messages,
                        &mut soft_cancel_requested,
                        Some(cancel_token.clone()),
                        Some(hard_cancel_token.clone()),
                        context.lineage.clone(),
                    )
                    .await;

                    match turn_result {
                        Ok(StreamedTurnResult::Complete) => {
                            if record_turn_boundary(
                                &context,
                                &event_tx,
                                &runtime_tx,
                                &status,
                                context.session_id,
                                &mut queued_messages,
                                turn_start_sequence_num,
                                &mut turn_count,
                                &mut loop_detector,
                                loop_detection_threshold,
                            )
                            .await?
                            {
                                return Ok(TurnDirective::FinishOk);
                            }
                            if flush_next_queued_message(
                                &context.session_store,
                                &event_tx,
                                context.session_id,
                                &mut queued_messages,
                            )
                            .await?
                            {
                                turn_requested = true;
                            }
                            if turn_requested {
                                return Ok(TurnDirective::ContinueLoop);
                            }

                            let persist_span = event_persist_span(0);
                            let persist_started = std::time::Instant::now();
                            async {
                                refresh_workspace_tool_stats(
                                    &context.session_store,
                                    context.session_id,
                                )
                                .await;
                                context
                                    .tool_router
                                    .destroy_session_hands(&context.session_id)
                                    .await;
                                update_status(
                                    &context.session_store,
                                    &event_tx,
                                    &status,
                                    context.session_id,
                                    SessionStatus::Completed,
                                )
                                .await?;
                                Result::<()>::Ok(())
                            }
                            .instrument(persist_span)
                            .await?;
                            record_turn_event_persist_duration(persist_started.elapsed(), 0);
                            if let Err(err) = runtime_tx.send(RuntimeEvent::TurnCompleted) {
                                tracing::warn!(?err, "runtime receiver dropped while sending TurnCompleted (completed)");
                            }
                            Ok(TurnDirective::ContinueLoop)
                        }
                        Ok(StreamedTurnResult::Continue) => {
                            if record_turn_boundary(
                                &context,
                                &event_tx,
                                &runtime_tx,
                                &status,
                                context.session_id,
                                &mut queued_messages,
                                turn_start_sequence_num,
                                &mut turn_count,
                                &mut loop_detector,
                                loop_detection_threshold,
                            )
                            .await?
                            {
                                return Ok(TurnDirective::FinishOk);
                            }
                            turn_requested = true;
                            Ok(TurnDirective::ContinueLoop)
                        }
                        Ok(StreamedTurnResult::NeedsApproval(_)) => Ok(TurnDirective::ContinueLoop),
                        Ok(StreamedTurnResult::Cancelled) => {
                            flush_queued_messages(
                                &context.session_store,
                                &event_tx,
                                context.session_id,
                                &mut queued_messages,
                            )
                            .await?;
                            let persist_span = event_persist_span(0);
                            let persist_started = std::time::Instant::now();
                            async {
                                refresh_workspace_tool_stats(
                                    &context.session_store,
                                    context.session_id,
                                )
                                .await;
                                context
                                    .tool_router
                                    .destroy_session_hands(&context.session_id)
                                    .await;
                                update_status(
                                    &context.session_store,
                                    &event_tx,
                                    &status,
                                    context.session_id,
                                    SessionStatus::Cancelled,
                                )
                                .await?;
                                Result::<()>::Ok(())
                            }
                            .instrument(persist_span)
                            .await?;
                            record_turn_event_persist_duration(persist_started.elapsed(), 0);
                            if let Err(err) = runtime_tx.send(RuntimeEvent::TurnCompleted) {
                                tracing::warn!(?err, "runtime receiver dropped while sending TurnCompleted (cancelled)");
                            }
                            Ok(TurnDirective::FinishOk)
                        }
                        Err(error) => {
                            let budget_exhausted = matches!(error, MoaError::BudgetExhausted(_));
                            if !budget_exhausted {
                                append_event(
                                    &context.session_store,
                                    &event_tx,
                                    context.session_id,
                                    Event::Error {
                                        message: error.to_string(),
                                        recoverable: false,
                                    },
                                )
                                .await?;
                            }
                            flush_queued_messages(
                                &context.session_store,
                                &event_tx,
                                context.session_id,
                                &mut queued_messages,
                            )
                            .await?;
                            let persist_span = event_persist_span(0);
                            let persist_started = std::time::Instant::now();
                            async {
                                refresh_workspace_tool_stats(
                                    &context.session_store,
                                    context.session_id,
                                )
                                .await;
                                context
                                    .tool_router
                                    .destroy_session_hands(&context.session_id)
                                    .await;
                                update_status(
                                    &context.session_store,
                                    &event_tx,
                                    &status,
                                    context.session_id,
                                    SessionStatus::Failed,
                                )
                                .await?;
                                Result::<()>::Ok(())
                            }
                            .instrument(persist_span)
                            .await?;
                            record_turn_event_persist_duration(persist_started.elapsed(), 0);
                            if !budget_exhausted
                                && let Err(err) = runtime_tx.send(RuntimeEvent::Error(error.to_string()))
                            {
                                tracing::warn!(?err, "runtime receiver dropped while sending Error");
                            }
                            if let Err(err) = runtime_tx.send(RuntimeEvent::TurnCompleted) {
                                tracing::warn!(?err, "runtime receiver dropped while sending TurnCompleted (error)");
                            }
                            Ok(TurnDirective::FinishErr(error))
                        }
                    }
                }
                .instrument(turn_root_span.clone())
                .await;
                let turn_latency_snapshot = turn_latency_scope.snapshot();
                record_turn_latency(turn_started.elapsed());
                emit_turn_latency_summary(&turn_root_span, turn_number, &turn_latency_snapshot);
                turn_outcome
            })
            .await;

            let turn_snapshot = turn_counters_scope.snapshot();
            emit_turn_replay_summary(&turn_root_span, turn_number, &turn_snapshot);
            turn_outcome
        })
        .await?;

        match turn_directive {
            TurnDirective::ContinueLoop => continue,
            TurnDirective::FinishOk => return Ok(()),
            TurnDirective::FinishErr(error) => return Err(error),
        }
    }
}

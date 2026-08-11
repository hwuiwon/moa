//! Progress handlers for the Session virtual object.

use super::*;

impl SessionImpl {
    pub(super) async fn handle_progress(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        annotate_restate_handler_span("Session", "progress");
        let session_id = parse_session_key(ctx.key())?;
        require_shared_session_participant(&self.authz, &ctx, session_id).await?;
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

    pub(super) async fn handle_turn_admission_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<TurnAdmissionHeartbeatRequest>,
    ) -> Result<(), HandlerError> {
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
}
pub(super) fn active_turn_progress_or_none(
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
pub(super) async fn collect_child_progress(
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

pub(super) async fn load_progress_events(
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

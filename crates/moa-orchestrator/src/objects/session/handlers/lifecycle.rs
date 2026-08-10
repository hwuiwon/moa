//! Lifecycle handlers for the Session virtual object.

use super::*;

impl SessionImpl {
    pub(super) async fn handle_set_meta(
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

    pub(super) async fn handle_cancel(
        &self,
        ctx: ObjectContext<'_>,
        scope: Json<CancelScope>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "cancel");
        let session_id = parse_session_key(ctx.key())?;
        let identity = require_session_participant(&self.authz, &ctx, session_id).await?;
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
                persist_message_admissions(&ctx, &admissions);
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
            .call()
            .await?;
        }
        // Only `TaskTree` cascades to the registered children; `CoordinatorOnly` leaves them running.
        if scope.cancels_task_tree() {
            for child in children {
                crate::restate_identity::replay_safe_request(
                    ctx.object_client::<WorkerClient>(child.id)
                        .cancel("parent session cancelled".to_string()),
                )
                .call()
                .await?;
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

    pub(super) async fn handle_status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        annotate_restate_handler_span("Session", "status");
        let session_id = parse_session_key(ctx.key())?;
        require_shared_session_participant(&self.authz, &ctx, session_id).await?;
        Ok(Json::from(SessionVoState::load_status(&ctx).await?))
    }

    pub(super) async fn handle_request_cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: Json<String>,
    ) -> Result<Json<CancelResponse>, HandlerError> {
        annotate_restate_handler_span("Session", "request_cancel");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&self.authz, &ctx, session_id).await?;
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
        .call()
        .await?;

        Ok(Json::from(CancelResponse {
            cancelled: true,
            reason: format!("cancel forwarded to turn {turn_id}"),
        }))
    }

    pub(super) async fn handle_snapshot(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionSnapshot>, HandlerError> {
        annotate_restate_handler_span("Session", "snapshot");
        let session_id = parse_session_key(ctx.key())?;
        require_shared_session_participant(&self.authz, &ctx, session_id).await?;
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

    pub(super) async fn handle_destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "destroy");
        let session_id = parse_session_key(ctx.key())?;
        require_session_participant(&self.authz, &ctx, session_id).await?;
        let state = Tracked::<SessionVoState>::load(&ctx).await?;
        // Reclaim any coordinator/orphan hands still leased under this session before the
        // VO state is cleared. The Session VO holds no `ToolRouter`, so this is dispatched
        // detached (fire-and-forget) to the ToolExecutor service that owns the router. It is
        // non-fatal; without this caller durable leases reclaim only via their 1-hour TTL.
        if let Some(meta) = state.meta.as_ref() {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .release_session_hands(Json::from(ReleaseSessionHandsRequest {
                        tenant_id: meta.tenant_id,
                        session_id,
                    })),
            )
            .send();
        }
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }
}

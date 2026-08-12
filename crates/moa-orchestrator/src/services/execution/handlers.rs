//! Restate execution service handlers and durable mutation operations.

use super::capability_catalog::list_capabilities_inner;
use super::planning_context::{PlanningContextInput, planning_context_inner};
use super::start::start_inner;
use super::support::*;
use super::*;

impl Execution for ExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn planning_context(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionPlanningContextRequest>,
    ) -> Result<Json<ExecutionPlanningContextResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "planning_context");
        let request = request.into_inner();
        let identity = self
            .authz
            .authorize_session_participant(&ctx, request.session_id)
            .await?;
        let owner_user_id = moa_core::types::identifiers::UserId::new(
            identity
                .acting_on_behalf_of
                .unwrap_or(identity.id)
                .to_string(),
        );
        let store = self.session_store.clone();
        let session_id = request.session_id;
        let sequence = request.originating_user_sequence_num;
        let (parent, event_record) = ctx
            .run(|| async move {
                let parent = store
                    .get_session(session_id)
                    .await
                    .map_err(crate::workflows::errors::moa_error_to_handler_error)?;
                let record = store
                    .get_events(
                        session_id,
                        EventRange {
                            from_seq: Some(sequence),
                            to_seq: Some(sequence),
                            event_types: None,
                            limit: Some(1),
                        },
                    )
                    .await
                    .map_err(crate::workflows::errors::moa_error_to_handler_error)?
                    .pop();
                Ok::<_, HandlerError>(Json::from((parent, record)))
            })
            .name("execution_planning_context_origin")
            .await?
            .into_inner();
        let parent_contact_id = parent.contact.as_ref().map(|contact| contact.contact_id);
        if parent.tenant_id != request.tenant_id || parent_contact_id != request.contact_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning context scope does not match the authorized parent session",
            )
            .into());
        }
        let Some(event_record) = event_record else {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning context origin event does not exist",
            )
            .into());
        };
        if event_record.sequence_num != request.originating_user_sequence_num
            || !matches!(&event_record.event, Event::UserMessage { .. })
        {
            return Err(TerminalError::new_with_code(
                409,
                "execution planning requires an exact persisted user-message origin",
            )
            .into());
        }
        let pool = self.pool.clone();
        let connector_catalog = self.connector_catalog.clone();
        let catalog_identity = identity.clone();
        let config = self.config.clone();
        let planning_admitted_at = event_record.timestamp;
        Ok(ctx
            .run(|| async move {
                let scoped_catalog = connector_catalog
                    .for_session(&catalog_identity, &parent)
                    .await
                    .map_err(scoped_catalog_error)?;
                planning_context_inner(PlanningContextInput {
                    pool,
                    registrations: scoped_catalog.snapshot().capability_registrations(),
                    config,
                    parent,
                    owner_user_id,
                    originating_event: event_record.event,
                    planning_admitted_at,
                    request,
                })
                .await
                .map(Json::from)
            })
            .name("execution_planning_context")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionStartRequest>,
    ) -> Result<Json<ExecutionStartResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "start");
        let request = request.into_inner();
        let identity = self
            .authz
            .authorize_session_participant(&ctx, request.session_id)
            .await?;
        let store = self.session_store.clone();
        let session_id = request.session_id;
        let sequence = request.originating_user_sequence_num;
        let (parent, origin) = ctx
            .run(|| async move {
                let parent = store
                    .get_session(session_id)
                    .await
                    .map_err(crate::workflows::errors::moa_error_to_handler_error)?;
                let origin = store
                    .get_events(
                        session_id,
                        EventRange {
                            from_seq: Some(sequence),
                            to_seq: Some(sequence),
                            event_types: None,
                            limit: Some(1),
                        },
                    )
                    .await
                    .map_err(crate::workflows::errors::moa_error_to_handler_error)?
                    .pop();
                Ok::<_, HandlerError>(Json::from((parent, origin)))
            })
            .name("execution_start_parent_session")
            .await?
            .into_inner();
        let parent_contact_id = parent.contact.as_ref().map(|contact| contact.contact_id);
        if parent.tenant_id != request.tenant_id || parent_contact_id != request.contact_id {
            return Err(TerminalError::new_with_code(
                409,
                "execution scope does not match the authorized parent session",
            )
            .into());
        }
        let objective = match origin.map(|record| record.event) {
            Some(Event::UserMessage { text, .. }) => text,
            _ => {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution start requires an exact persisted user-message origin",
                )
                .into());
            }
        };
        let pool = self.pool.clone();
        let config = self.config.clone();
        let admitted_identity = identity.clone();
        let response = ctx
            .run(|| async move {
                start_inner(pool, config, request, objective, admitted_identity)
                    .await
                    .map(Json::from)
            })
            .name("execution_start")
            .await?
            .into_inner();
        tracing::Span::current()
            .set_attribute("moa.execution.run_uid", response.run.run_uid.to_string());
        with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .execution_run_started(Json::from(execution_run_started_delivery(&response))),
            &identity,
        )
        .send();
        if !response.confirmation_required {
            // A newly admitted run always starts at controller generation zero. The durable
            // RunActivation outbox remains authoritative; this stable kick only reduces latency.
            kick_execution_dispatcher(&ctx, response.run.run_uid, 0, "start").await?;
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn confirm(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionConfirmRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "confirm");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let run_request = request.run.clone();
        let pool = self.pool.clone();
        let accepted = ctx
            .run(|| async move { confirm_inner(pool, request).await.map(Json::from) })
            .name("execution_confirm")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, run_request.run_uid, wake_epoch, "confirm").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunRequest>,
    ) -> Result<Json<ExecutionStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "status");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.session_id)
            .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { status_inner(pool, request).await.map(Json::from) })
            .name("execution_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn synthesis_evidence(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionSynthesisEvidenceRequest>,
    ) -> Result<Json<ExecutionSynthesisEvidence>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "synthesis_evidence");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                synthesis_evidence_inner(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_synthesis_evidence")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_runs(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunListRequest>,
    ) -> Result<Json<ExecutionRunListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_runs");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_runs_inner(pool, request).await.map(Json::from) })
            .name("execution_list_runs")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_tasks(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionTaskListRequest>,
    ) -> Result<Json<ExecutionTaskListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_tasks");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_tasks_inner(pool, request).await.map(Json::from) })
            .name("execution_list_tasks")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionCancelRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "cancel");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let run_request = request.run.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move { cancel_inner(pool, config, request).await.map(Json::from) })
            .name("execution_cancel")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, run_request.run_uid, wake_epoch, "cancel").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn pause(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunControlRequest>,
    ) -> Result<Json<ExecutionRunControlResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "pause");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let response = ctx
            .run(|| async move { pause_inner(pool, config, request).await.map(Json::from) })
            .name("execution_pause")
            .await?
            .into_inner();
        kick_control_dispatcher(&ctx, run_uid, "pause", &response).await?;
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn resume(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionRunControlRequest>,
    ) -> Result<Json<ExecutionRunControlResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "resume");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let response = ctx
            .run(|| async move { resume_inner(pool, config, request).await.map(Json::from) })
            .name("execution_resume")
            .await?
            .into_inner();
        kick_control_dispatcher(&ctx, run_uid, "resume", &response).await?;
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deliver_input(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionInputRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "deliver_input");
        let request = request.into_inner();
        match request.audience {
            moa_artifacts::execution_plan::InputAudience::User => {
                let session_id = request.session_id.ok_or_else(|| {
                    TerminalError::new_with_code(400, "user input requires session_id")
                })?;
                self.authz
                    .authorize_session_participant(&ctx, session_id)
                    .await?;
            }
            moa_artifacts::execution_plan::InputAudience::TenantAdmin
            | moa_artifacts::execution_plan::InputAudience::ExternalSystem => {
                self.authz
                    .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
                    .await?;
            }
        }
        let task_request = request.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                deliver_input_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_deliver_input")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, task_request.run_uid, wake_epoch, "input").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide_review(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionReviewDecisionRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "decide_review");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let task_request = request.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                decide_review_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_decide_review")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, task_request.run_uid, wake_epoch, "review").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deliver_signal(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionSignalRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "deliver_signal");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let task_request = request.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                deliver_signal_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_deliver_signal")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, task_request.run_uid, wake_epoch, "signal").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn apply_amendment(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "apply_amendment");
        let request = request.into_inner();
        self.authz
            .authorize_session_participant(&ctx, request.run.session_id)
            .await?;
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                apply_amendment_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_apply_amendment")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            pause_execution_mutation_handoff_for_test().await;
            kick_execution_dispatcher(&ctx, run_uid, wake_epoch, "amendment").await?;
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_capabilities(
        &self,
        ctx: Context<'_>,
        request: Json<CapabilitiesListRequest>,
    ) -> Result<Json<CapabilitiesListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "list_capabilities");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;

        let pool = self.pool.clone();
        let connector_catalog = self.connector_catalog.clone();
        Ok(ctx
            .run(|| async move {
                // This tenant-wide listing has no authoritative agent revision or
                // connector binding. It therefore exposes only the immutable
                // deployment catalog instead of inventing tenant connector authority.
                let registrations = connector_catalog
                    .deployment_catalog()
                    .map_err(scoped_catalog_error)?
                    .snapshot()
                    .capability_registrations();
                list_capabilities_inner(pool, registrations, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_list_capabilities")
            .await?)
    }
}

pub(super) async fn confirm_inner(
    pool: sqlx::PgPool,
    request: ExecutionConfirmRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(run) = repository
        .load_run_for_session(scope, request.run.run_uid, request.run.session_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&run, &request.run)?;
    let outcome = repository
        .confirm_run(
            scope,
            run.run_uid,
            &request.expected_plan_hash,
            request.approved_budget,
        )
        .await
        .map_err(execution_error)?;
    Ok(match outcome {
        ConfirmationOutcome::Confirmed(run) => applied_mutation(&run),
        ConfirmationOutcome::AlreadyConfirmed(run) => replayed_mutation(&run),
        ConfirmationOutcome::NotFound => not_found_mutation(),
        ConfirmationOutcome::Conflict(reason) => conflict_mutation(match reason {
            ConfirmationConflict::PlanHashMismatch => ExecutionConflictReason::PlanHashMismatch,
            ConfirmationConflict::BudgetMismatch => ExecutionConflictReason::BudgetMismatch,
            ConfirmationConflict::InvalidStatus => ExecutionConflictReason::InvalidStatus,
        }),
    })
}

pub(super) async fn status_inner(
    pool: sqlx::PgPool,
    request: ExecutionRunRequest,
) -> Result<ExecutionStatusResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let run = repository
        .load_run_for_session(scope, request.run_uid, request.session_id)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request)?;
    Ok(ExecutionStatusResponse {
        run: run_summary(&run),
        waiting: run.waiting_reasons.clone(),
        output: run.output.clone(),
        gaps: run.terminal_gaps.clone(),
    })
}

pub(super) async fn synthesis_evidence_inner(
    pool: sqlx::PgPool,
    request: ExecutionSynthesisEvidenceRequest,
) -> Result<ExecutionSynthesisEvidence, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let run = repository
        .load_run_for_session(scope, request.run.run_uid, request.run.session_id)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request.run)?;
    if run.originating_user_sequence_num != request.originating_user_sequence_num {
        return Err(TerminalError::new_with_code(
            409,
            "execution synthesis origin does not match the durable run",
        )
        .into());
    }
    if !run.status.is_terminal() {
        return Err(TerminalError::new_with_code(
            409,
            "execution synthesis evidence is available only for terminal runs",
        )
        .into());
    }
    Ok(ExecutionSynthesisEvidence {
        goal: run.goal,
        completion_check_results: run.completion_check_results,
    })
}

pub(super) async fn list_runs_inner(
    pool: sqlx::PgPool,
    request: ExecutionRunListRequest,
) -> Result<ExecutionRunListResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor::<ExecutionRunCursor>)
        .transpose()
        .map_err(execution_error)?;
    if let Some(cursor) = cursor {
        let boundary = repository
            .load_run(scope, cursor.run_uid)
            .await
            .map_err(execution_error)?;
        if boundary.as_ref().map(|run| run.created_at) != Some(cursor.created_at) {
            return Err(invalid_execution_request(
                "run cursor does not belong to the requested scope",
            ));
        }
    }
    let page = repository
        .list_runs(
            scope,
            ExecutionRunPageRequest {
                limit: request.limit.unwrap_or_default(),
                cursor: cursor.map(|cursor| moa_execution::repository::ExecutionRunCursor {
                    created_at: cursor.created_at,
                    run_uid: cursor.run_uid,
                }),
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionRunListResponse {
        runs: page.runs.iter().map(run_summary).collect(),
        next_cursor: page
            .next_cursor
            .map(|cursor| ExecutionRunCursor {
                created_at: cursor.created_at,
                run_uid: cursor.run_uid,
            })
            .map(|cursor| encode_cursor(&cursor))
            .transpose()
            .map_err(execution_error)?,
    })
}

pub(super) async fn list_tasks_inner(
    pool: sqlx::PgPool,
    request: ExecutionTaskListRequest,
) -> Result<ExecutionTaskListResponse, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let run = repository
        .load_run_for_session(scope, request.run.run_uid, request.run.session_id)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "execution run not found"))?;
    verify_run_request(&run, &request.run)?;
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor::<ExecutionTaskCursor>)
        .transpose()
        .map_err(execution_error)?;
    if let Some(cursor) = cursor.as_ref() {
        let boundary = repository
            .load_task(scope, run.run_uid, cursor.task_id)
            .await
            .map_err(execution_error)?;
        if !boundary
            .as_ref()
            .is_some_and(|task| task.node_id == cursor.node_id && task.item_key == cursor.item_key)
        {
            return Err(invalid_execution_request(
                "task cursor does not belong to the requested run",
            ));
        }
    }
    let page = repository
        .list_tasks(
            scope,
            run.run_uid,
            ExecutionTaskPageRequest {
                limit: request.limit.unwrap_or_default(),
                cursor: cursor.map(|cursor| moa_execution::repository::ExecutionTaskCursor {
                    node_id: cursor.node_id,
                    item_key: cursor.item_key,
                    task_id: cursor.task_id,
                }),
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionTaskListResponse {
        tasks: page.tasks.iter().map(task_projection).collect(),
        next_cursor: page
            .next_cursor
            .map(|cursor| ExecutionTaskCursor {
                node_id: cursor.node_id,
                item_key: cursor.item_key,
                task_id: cursor.task_id,
            })
            .map(|cursor| encode_cursor(&cursor))
            .transpose()
            .map_err(execution_error)?,
    })
}

pub(super) async fn cancel_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionCancelRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(cancellation) = repository
        .load_cancellation_projection_for_session(
            scope,
            request.run.run_uid,
            request.run.session_id,
        )
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&cancellation.run, &request.run)?;
    if cancellation.run.status == ExecutionRunStatus::Cancelled {
        return Ok(replayed_mutation(&cancellation.run));
    }
    if let Some(pending) = &cancellation.run.pending_terminal {
        return Ok(if pending.status == ExecutionRunStatus::Cancelled {
            replayed_mutation(&cancellation.run)
        } else {
            conflict_mutation(ExecutionConflictReason::AlreadyTerminal)
        });
    }
    let completed_node_ids = cancellation
        .completed_node_ids
        .into_iter()
        .collect::<BTreeSet<_>>();
    let terminal_evidence = cancellation_terminal_evidence_from_completed_nodes(
        &cancellation.run.goal,
        &cancellation.run.active_plan,
        &completed_node_ids,
    )
    .map_err(execution_error)?;
    let pending_terminal = PendingExecutionTerminal {
        status: ExecutionRunStatus::Cancelled,
        reason: ExecutionTerminalReason::Cancelled,
        terminal_evidence,
        output: None,
        completion_check_results: Vec::new(),
        terminal_gaps: Vec::new(),
        cancellation_reason: Some(request.reason),
    };
    Ok(
        match repository
            .fence_completion_terminal_and_enqueue_settlement(
                &config,
                scope,
                cancellation.run.run_uid,
                cancellation.run.controller_generation,
                cancellation.run.wake_epoch,
                pending_terminal,
                chrono::Utc::now(),
                u32::try_from(
                    config
                        .maximum_activation_steps
                        .min(config.max_in_flight_tasks)
                        .min(1_000),
                )
                .map_err(|_| invalid_execution_request("terminal page limit exceeds u32"))?,
            )
            .await
            .map_err(execution_error)?
        {
            PendingTerminalAdvanceOutcome::Applied(commit) => applied_mutation(&commit.run),
            PendingTerminalAdvanceOutcome::Replayed(commit) => replayed_mutation(&commit.run),
            PendingTerminalAdvanceOutcome::NotFound => not_found_mutation(),
            PendingTerminalAdvanceOutcome::Conflict => {
                conflict_mutation(ExecutionConflictReason::AlreadyTerminal)
            }
        },
    )
}

pub(super) async fn pause_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionRunControlRequest,
) -> Result<ExecutionRunControlResponse, HandlerError> {
    run_control_inner(pool, config, request, true).await
}

pub(super) async fn resume_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionRunControlRequest,
) -> Result<ExecutionRunControlResponse, HandlerError> {
    run_control_inner(pool, config, request, false).await
}

async fn run_control_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionRunControlRequest,
    pause: bool,
) -> Result<ExecutionRunControlResponse, HandlerError> {
    if request.expected_controller_generation == 0 {
        return Err(invalid_execution_request(
            "expected_controller_generation must be positive",
        ));
    }
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(run) = repository
        .load_run_for_session(scope, request.run.run_uid, request.run.session_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(ExecutionRunControlResponse::NotFound);
    };
    verify_run_request(&run, &request.run)?;
    let outcome = if pause {
        repository
            .pause_run(
                scope,
                &config,
                request.run.run_uid,
                request.expected_controller_generation,
            )
            .await
    } else {
        repository
            .resume_run(
                scope,
                &config,
                request.run.run_uid,
                request.expected_controller_generation,
            )
            .await
    }
    .map_err(execution_error)?;
    Ok(match outcome {
        TransitionOutcome::RunApplied(run) => ExecutionRunControlResponse::Applied {
            run: run_summary(&run),
            controller_generation: run.controller_generation,
            wake_epoch: run.wake_epoch,
        },
        TransitionOutcome::RunAlreadyApplied(run) => ExecutionRunControlResponse::Replayed {
            run: run_summary(&run),
            controller_generation: run.controller_generation,
            wake_epoch: run.wake_epoch,
        },
        TransitionOutcome::NotFound => ExecutionRunControlResponse::NotFound,
        TransitionOutcome::Rejected(TransitionRejection::GenerationMismatch) => {
            ExecutionRunControlResponse::Conflict {
                reason: ExecutionConflictReason::GenerationMismatch,
            }
        }
        TransitionOutcome::Rejected(_) => ExecutionRunControlResponse::Conflict {
            reason: ExecutionConflictReason::InvalidStatus,
        },
        TransitionOutcome::Applied(_) | TransitionOutcome::AlreadyApplied(_) => {
            ExecutionRunControlResponse::Conflict {
                reason: ExecutionConflictReason::InvalidStatus,
            }
        }
    })
}

async fn kick_control_dispatcher(
    ctx: &Context<'_>,
    run_uid: uuid::Uuid,
    action: &str,
    response: &ExecutionRunControlResponse,
) -> Result<(), HandlerError> {
    let generation = match response {
        ExecutionRunControlResponse::Applied {
            controller_generation,
            ..
        }
        | ExecutionRunControlResponse::Replayed {
            controller_generation,
            ..
        } => *controller_generation,
        ExecutionRunControlResponse::Conflict { .. } | ExecutionRunControlResponse::NotFound => {
            return Ok(());
        }
    };
    kick_execution_dispatcher(ctx, run_uid, generation, action).await
}

async fn kick_execution_dispatcher(
    ctx: &Context<'_>,
    run_uid: uuid::Uuid,
    durable_fence: u64,
    action: &str,
) -> Result<(), HandlerError> {
    use crate::services::execution_dispatcher::{
        DispatchExecutionsRequest, ExecutionDispatcherClient,
    };
    let handle = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionDispatcherClient>()
            .dispatch(Json::from(DispatchExecutionsRequest::default()))
            .idempotency_key(format!("{run_uid}:{durable_fence}:{action}")),
    )
    .send();
    let _invocation_id = handle.invocation_id().await?;
    Ok(())
}

pub(super) async fn deliver_input_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionInputRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let run = match request.session_id {
        Some(session_id) => {
            repository
                .load_run_for_session(scope, request.run_uid, session_id)
                .await
        }
        None => repository.load_run(scope, request.run_uid).await,
    }
    .map_err(execution_error)?;
    let Some(run) = run else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id
        || run.contact_id != request.contact_id
        || request
            .session_id
            .is_some_and(|session_id| run.session_id != session_id)
    {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    let persisted_audience = persisted_input_audience(
        task.generation,
        task.current_outcome.as_ref(),
        &task.outcome_audit,
        request.expected_generation,
    );
    if persisted_audience.as_ref() != Some(&request.audience) {
        return Ok(conflict_mutation(ExecutionConflictReason::AudienceMismatch));
    }
    let transition = repository
        .resume_task_with_input(
            scope,
            &config,
            run.run_uid,
            task.task_id,
            request.expected_generation,
            request.input,
        )
        .await
        .map_err(execution_error)?;
    mutation_from_transition(&repository, scope, run.run_uid, transition).await
}

pub(super) async fn decide_review_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionReviewDecisionRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id || run.contact_id != request.contact_id {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    let _wait_policy = match &task.kind {
        moa_execution::state::LogicalTaskKind::Review {
            prompt: _,
            wait_policy,
        } => wait_policy,
        _ => return Ok(conflict_mutation(ExecutionConflictReason::InvalidStatus)),
    };
    let result = match request.decision {
        ExecutionReviewDecision::Approved { payload } => ExecutionTaskResult::Completed {
            output: payload,
            citations: Vec::new(),
        },
        ExecutionReviewDecision::Rejected { reason } => ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::AuthorizationDenied,
            message: reason,
        },
    };
    external_wait_mutation(
        &repository,
        &config,
        scope,
        &run,
        &task,
        request.expected_generation,
        result,
    )
    .await
}

pub(super) async fn deliver_signal_inner(
    pool: sqlx::PgPool,
    config: moa_config::ExecutionConfig,
    request: ExecutionSignalRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    if run.tenant_id != request.tenant_id || run.contact_id != request.contact_id {
        return Ok(conflict_mutation(ExecutionConflictReason::ScopeMismatch));
    }
    let Some(task) = repository
        .load_task(scope, run.run_uid, request.task_id)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    let signal_matches = match &task.kind {
        moa_execution::state::LogicalTaskKind::WaitSignal {
            signal_name,
            wait_policy: _wait_policy,
        } => signal_name == &request.signal_name,
        _ => false,
    };
    if !signal_matches {
        return Ok(conflict_mutation(ExecutionConflictReason::SignalMismatch));
    }
    external_wait_mutation(
        &repository,
        &config,
        scope,
        &run,
        &task,
        request.expected_generation,
        ExecutionTaskResult::Completed {
            output: request.payload,
            citations: Vec::new(),
        },
    )
    .await
}

pub(super) async fn external_wait_mutation(
    repository: &ExecutionRepository,
    config: &moa_config::ExecutionConfig,
    scope: ExecutionScope,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    generation: u64,
    result: ExecutionTaskResult,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    if task.generation == generation
        && matches!(
            task.status,
            ExecutionTaskStatus::Running
                | ExecutionTaskStatus::WaitingReview
                | ExecutionTaskStatus::WaitingSignal
        )
        && let ExecutionTaskResult::Completed { output, .. } = &result
    {
        validate_external_wait_payload(&run.active_plan.definition, &task.node_id, output)?;
    }
    let write = repository
        .complete_external_wait(
            scope,
            config,
            run.run_uid,
            task.task_id,
            generation,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: zero_usage(),
                result,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(mutation_from_task_write(write))
}

pub(super) async fn apply_amendment_inner(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionAmendmentRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let amendment_digest = amendment_hash(&request.amendment).map_err(execution_error)?;
    match repository
        .recover_amendment_handoff(
            scope,
            request.run.run_uid,
            request.run.session_id,
            request.expected_plan_revision,
            &amendment_digest,
        )
        .await
        .map_err(execution_error)?
    {
        AmendmentReplayOutcome::Replayed(commit) => {
            return Ok(
                replayed_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
            );
        }
        AmendmentReplayOutcome::NotFound => return Ok(not_found_mutation()),
        AmendmentReplayOutcome::Conflict => {
            return Ok(conflict_mutation(
                ExecutionConflictReason::PlanRevisionMismatch,
            ));
        }
        AmendmentReplayOutcome::NotApplied => {}
    }
    let snapshot = match repository
        .load_amendment_projection_for_session(
            scope,
            &config,
            AmendmentProjectionRequest {
                run_uid: request.run.run_uid,
                session_id: request.run.session_id,
                expected_plan_revision: request.expected_plan_revision,
            },
        )
        .await
        .map_err(execution_error)?
    {
        AmendmentProjectionOutcome::Ready(snapshot) => *snapshot,
        AmendmentProjectionOutcome::NotFound => return Ok(not_found_mutation()),
        AmendmentProjectionOutcome::Conflict => {
            return Ok(conflict_mutation(
                ExecutionConflictReason::PlanRevisionMismatch,
            ));
        }
    };
    verify_run_request(&snapshot.run, &request.run)?;
    if snapshot.run.plan_revision != request.expected_plan_revision {
        return Ok(conflict_mutation(
            ExecutionConflictReason::PlanRevisionMismatch,
        ));
    }
    let remaining_budget = snapshot
        .budget_ledger
        .remaining_limit()
        .map_err(execution_error)?;
    let waiting_tasks = &snapshot.projection.replan_tasks;
    let [waiting_task] = waiting_tasks.as_slice() else {
        return Ok(conflict_mutation(ExecutionConflictReason::InvalidStatus));
    };
    let proposed_amendment_fingerprint =
        amendment_operations_fingerprint(&request.amendment).map_err(execution_error)?;
    let loop_evaluation = replan_loop_evaluation_request(
        &snapshot,
        proposed_amendment_fingerprint,
        request.amendment.clone(),
        config.clone(),
        waiting_task,
    )
    .map_err(execution_error)?;
    // Detect loop identity and structural no-progress before compiler rejection can hide the
    // typed stop. Valid candidates are still evaluated through the complete precedence below.
    let prevalidation_loop_decision = evaluate_replan_loop_stop(loop_evaluation.clone());
    let now = chrono::Utc::now();
    let validated = validate_amendment(ValidateAmendmentRequest {
        goal: snapshot.run.goal.clone(),
        active_plan: snapshot.run.active_plan.clone(),
        amendment: request.amendment.clone(),
        projection: snapshot.projection.clone(),
        catalog: snapshot.run.catalog.clone(),
        authorization: snapshot.run.authorization.clone(),
        remaining_budget: remaining_budget.clone(),
        config: config.clone(),
        now,
    });
    if let Some(remaining_estimate) = validated.remaining_estimate
        && let Some(reason) =
            evaluate_replan_resource_stop(now, &remaining_budget, remaining_estimate)
    {
        return finalize_service_replan_stop(
            &repository,
            scope,
            &config,
            ServiceReplanStopRequest {
                snapshot: &snapshot,
                waiting_task,
                amendment_digest,
                reason,
                detail: Some(&request.amendment.reason),
            },
        )
        .await;
    }
    let Some(active_plan) = validated.plan else {
        if let ReplanDecision::Stop { reason } = prevalidation_loop_decision {
            return finalize_service_replan_stop(
                &repository,
                scope,
                &config,
                ServiceReplanStopRequest {
                    snapshot: &snapshot,
                    waiting_task,
                    amendment_digest,
                    reason,
                    detail: Some(&request.amendment.reason),
                },
            )
            .await;
        }
        return Err(invalid_execution_request(format!(
            "amendment validation failed: {:?}",
            validated.report.issues
        )));
    };
    let proposed_estimate = validated.remaining_estimate.ok_or_else(|| {
        invalid_execution_request("validated amendment is missing its remaining-work estimate")
    })?;
    let requirement_mapping = request
        .amendment
        .operations
        .iter()
        .filter_map(|operation| match operation {
            PlanAmendmentOperation::AddNode { node }
            | PlanAmendmentOperation::ReplacePendingNode { node, .. } => {
                Some((node.id.clone(), node.requirement_ids.clone()))
            }
            PlanAmendmentOperation::RemovePendingNode { .. } => None,
        })
        .collect();
    if let ReplanDecision::Stop { reason } = evaluate_replan_stop(replan_evaluation_request(
        &snapshot,
        &active_plan,
        proposed_estimate,
        remaining_budget,
        loop_evaluation,
        now,
    )) {
        return finalize_service_replan_stop(
            &repository,
            scope,
            &config,
            ServiceReplanStopRequest {
                snapshot: &snapshot,
                waiting_task,
                amendment_digest,
                reason,
                detail: Some(&request.amendment.reason),
            },
        )
        .await;
    }
    let write = repository
        .append_amendment(
            scope,
            &config,
            snapshot.run.run_uid,
            request.expected_plan_revision,
            ValidatedAmendment {
                amendment: request.amendment,
                amendment_hash: amendment_digest,
                active_plan,
                requirement_mapping,
                superseded_task_id: waiting_task.task_id,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(match write {
        AmendmentWrite::Applied(commit) => {
            applied_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
        }
        AmendmentWrite::Replayed(commit) => {
            replayed_mutation(&commit.run).with_task_ids_to_release(commit.task_ids_to_release)
        }
        AmendmentWrite::NotFound => not_found_mutation(),
        AmendmentWrite::Conflict => {
            conflict_mutation(ExecutionConflictReason::PlanRevisionMismatch)
        }
    })
}

struct ServiceReplanStopRequest<'a> {
    snapshot: &'a ExecutionAmendmentSnapshot,
    waiting_task: &'a ExecutionTaskProjection,
    amendment_digest: ExecutionHash,
    reason: moa_execution::ReplanStopReason,
    detail: Option<&'a str>,
}

async fn finalize_service_replan_stop(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &ExecutionConfig,
    request: ServiceReplanStopRequest<'_>,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let write = repository
        .request_replan_stop(
            scope,
            config,
            NewExecutionReplanStopIntent {
                run_uid: request.snapshot.run.run_uid,
                session_id: request.snapshot.run.session_id,
                base_plan_revision: request.snapshot.run.plan_revision,
                origin_task_id: request.waiting_task.task_id,
                task_generation: request.waiting_task.generation,
                amendment_hash: request.amendment_digest,
                stop_reason: request.reason,
                detail: request.detail.map(str::to_string),
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(match write {
        ReplanStopIntentWriteOutcome::Applied(run) => applied_mutation(&run),
        ReplanStopIntentWriteOutcome::Replayed(run) => replayed_mutation(&run),
        ReplanStopIntentWriteOutcome::NotFound => not_found_mutation(),
        ReplanStopIntentWriteOutcome::Conflict => {
            conflict_mutation(ExecutionConflictReason::PlanRevisionMismatch)
        }
    })
}

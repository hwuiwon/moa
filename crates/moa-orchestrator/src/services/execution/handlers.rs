//! Restate execution service handlers and durable mutation operations.

use super::capability_catalog::list_capabilities_inner;
use super::planning_context::planning_context_inner;
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
        Ok(ctx
            .run(|| async move {
                let scoped_catalog = connector_catalog
                    .for_session(&catalog_identity, &parent)
                    .await
                    .map_err(scoped_catalog_error)?;
                planning_context_inner(
                    pool,
                    scoped_catalog.snapshot().capability_registrations(),
                    config,
                    parent,
                    owner_user_id,
                    event_record.event,
                    request,
                )
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
        let response = ctx
            .run(|| async move {
                start_inner(pool, config, request, objective)
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
            send_run_wake(
                &ctx,
                run_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::Confirmed,
            );
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
        let accepted = ctx
            .run(|| async move { cancel_inner(pool, request).await.map(Json::from) })
            .name("execution_cancel")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            cancel_completion_owner_from_service(
                &ctx,
                LLMCompletionOwner::execution_run(run_request.run_uid.to_string()),
            )
            .await?;
            send_run_wake(
                &ctx,
                run_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::Cancelled,
            );
        }
        Ok(Json::from(accepted.into_response()))
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
        let accepted = ctx
            .run(|| async move { deliver_input_inner(pool, request).await.map(Json::from) })
            .name("execution_deliver_input")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            if accepted
                .task_ids_to_release()
                .contains(&task_request.task_id)
            {
                crate::restate_identity::replay_safe_request(
                    ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                        .cancel(Json::from(
                            "execution input redispatch reached a terminal admission outcome"
                                .to_string(),
                        )),
                )
                .send();
            } else {
                crate::restate_identity::replay_safe_request(
                    ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                        .input_delivered(Json::from(task_request.clone())),
                )
                .send();
            }
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::InputDelivered,
            );
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
        let accepted = ctx
            .run(|| async move { decide_review_inner(pool, request).await.map(Json::from) })
            .name("execution_decide_review")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                    .review_decided(Json::from(task_request.clone())),
            )
            .send();
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::ReviewDecided,
            );
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
        let accepted = ctx
            .run(|| async move { deliver_signal_inner(pool, request).await.map(Json::from) })
            .name("execution_deliver_signal")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_request.task_id.to_string())
                    .signal_delivered(Json::from(task_request.clone())),
            )
            .send();
            send_run_wake(
                &ctx,
                task_request.run_uid,
                wake_epoch,
                ExecutionRunWakeReason::SignalDelivered,
            );
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
            send_run_wake(
                &ctx,
                run_uid,
                wake_epoch,
                ExecutionRunWakeReason::AmendmentAccepted,
            );
        }
        for task_id in accepted.task_ids_to_release() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                    .cancel(Json::from(
                        "execution task superseded or stopped by amendment".to_string(),
                    )),
            )
            .send();
        }
        Ok(Json::from(accepted.into_response()))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only by the keyed ExecutionRun workflow; the request carries no authority and apply_amendment_inner reloads and revision-fences all persisted scope.
    async fn apply_planned_amendment(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionAmendmentRequest>,
    ) -> Result<Json<ExecutionMutationResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Execution", "apply_planned_amendment");
        let request = request.into_inner();
        let run_uid = request.run.run_uid;
        let pool = self.pool.clone();
        let config = self.config.clone();
        let accepted = ctx
            .run(|| async move {
                apply_amendment_inner(pool, config, request)
                    .await
                    .map(Json::from)
            })
            .name("execution_apply_planned_amendment")
            .await?
            .into_inner();
        if let Some(wake_epoch) = accepted.wake_epoch() {
            send_run_wake(
                &ctx,
                run_uid,
                wake_epoch,
                ExecutionRunWakeReason::AmendmentAccepted,
            );
        }
        for task_id in accepted.task_ids_to_release() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExecutionTaskClient>(task_id.to_string())
                    .cancel(Json::from(
                        "execution task superseded or stopped by amendment".to_string(),
                    )),
            )
            .send();
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

pub(super) async fn start_inner(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionStartRequest,
    originating_objective: String,
) -> Result<ExecutionStartResponse, HandlerError> {
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let repository = ExecutionRepository::new(pool);
    let planning_context = repository
        .load_planning_context(scope, request.planning_context_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| {
            TerminalError::new_with_code(409, "execution planning context does not exist")
        })?;
    let expected_context_hash = request
        .planning_context_hash
        .parse::<ExecutionHash>()
        .map_err(execution_error)?;
    let snapshot = &planning_context.snapshot;
    if planning_context.planning_context_hash != expected_context_hash
        || snapshot.tenant_id != request.tenant_id
        || snapshot.contact_id != request.contact_id
        || snapshot.session_id != request.session_id
        || snapshot.originating_user_sequence_num != request.originating_user_sequence_num
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution planning context hash or origin scope mismatch",
        )
        .into());
    }
    if request.compiled.goal.objective.as_bytes() != originating_objective.as_bytes() {
        return Err(invalid_execution_request(
            "compiled execution objective must equal the persisted user message",
        ));
    }
    let validation = compile(CompileExecutionRequest {
        goal: request.compiled.goal.clone(),
        plan: request.compiled.plan.definition.clone(),
        run_input: request.run_input.clone(),
        catalog: snapshot.catalog.clone(),
        authorization: snapshot.authorization.clone(),
        approved_budget: snapshot.budget.clone(),
        config: config.clone(),
        now: Utc::now(),
    });
    if validation.compiled.as_ref() != Some(&request.compiled) {
        return Err(invalid_execution_request(
            "compiled execution does not match deterministic server validation",
        ));
    }
    if request.compiled.plan.plan_hash
        != plan_hash(&request.compiled.plan.definition).map_err(execution_error)?
        || request.compiled.plan.catalog_hash != snapshot.catalog.catalog_hash
    {
        return Err(invalid_execution_request(
            "compiled plan hashes do not match the supplied immutable snapshots",
        ));
    }
    estimate_fits_limit(request.compiled.plan.estimate, &snapshot.budget)
        .map_err(execution_error)?;
    validate_start_source_provenance(
        &request.source_provenance,
        &request.compiled.plan.plan_hash.to_string(),
        &snapshot.execution_templates,
    )
    .map_err(|error| invalid_execution_request(error.to_string()))?;
    let existing = if let Some(key) = request.idempotency_key.as_deref() {
        repository
            .load_run_by_idempotency_key(scope, request.tenant_id, request.contact_id, key)
            .await
            .map_err(execution_error)?
    } else {
        None
    };
    if let Some(run) = existing {
        verify_run_scope(
            &run,
            request.tenant_id,
            request.contact_id,
            request.session_id,
        )?;
        verify_start_replay(&run, &request, snapshot)?;
        let confirmation_required = run.status == ExecutionRunStatus::AwaitingConfirmation;
        return Ok(ExecutionStartResponse {
            active_plan_hash: run.active_plan_hash,
            estimate: run.active_plan.estimate,
            run: run_summary(&run),
            created: false,
            confirmation_required,
        });
    }
    let confirmation_required =
        request.compiled.plan.estimate.cost_microusd > config.unattended_max_cost_microusd;
    let status = if confirmation_required {
        ExecutionRunStatus::AwaitingConfirmation
    } else {
        ExecutionRunStatus::Queued
    };
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: request.tenant_id,
                contact_id: request.contact_id,
                session_id: request.session_id,
                originating_user_sequence_num: request.originating_user_sequence_num,
                planning_context_uid: request.planning_context_uid,
                planning_context_hash: expected_context_hash,
                owner_user_id: snapshot.owner_user_id.clone(),
                goal: request.compiled.goal,
                plan: request.compiled.plan,
                catalog: snapshot.catalog.clone(),
                authorization: snapshot.authorization.clone(),
                pinned_instruction_skills: snapshot.pinned_instruction_skills.clone(),
                source_provenance: request.source_provenance,
                input: request.run_input,
                status,
                approved_budget: snapshot.budget.clone(),
                idempotency_key: request.idempotency_key,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionStartResponse {
        active_plan_hash: run.active_plan_hash,
        estimate: run.active_plan.estimate,
        run: run_summary(&run),
        created: true,
        confirmation_required,
    })
}

pub(super) fn validate_start_source_provenance(
    provenance: &ExecutionSourceProvenance,
    committed_plan_hash: &str,
    execution_templates: &[PinnedExecutionTemplate],
) -> Result<(), ExecutionPlanningContractError> {
    provenance.validate(committed_plan_hash)?;
    let (skill_template_ref, skill_template_revision_uid) = match provenance {
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        }
        | ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (skill_template_ref, skill_template_revision_uid),
        ExecutionSourceProvenance::GeneratedPlan { .. } => return Ok(()),
    };
    let parsed = skill_template_ref.parse::<ArtifactRef>().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    let canonical = parsed.canonical_string().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    if canonical != *skill_template_ref
        || !execution_templates
            .iter()
            .any(|template| template.skill_ref == parsed)
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message:
                "must equal one canonical template reference in the persisted planning context"
                    .to_string(),
        });
    }
    if execution_templates
        .iter()
        .filter(|template| {
            template.skill_ref == parsed && template.revision_uid == *skill_template_revision_uid
        })
        .count()
        != 1
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_revision_uid".to_string(),
            message: "must equal one exact template revision in the persisted planning context"
                .to_string(),
        });
    }
    Ok(())
}

pub(super) async fn confirm_inner(
    pool: sqlx::PgPool,
    request: ExecutionConfirmRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(run) = repository
        .load_run(scope, request.run.run_uid)
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
        .load_run(scope, request.run_uid)
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
        .load_run(scope, request.run.run_uid)
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
        .load_run(scope, request.run.run_uid)
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
    request: ExecutionCancelRequest,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.run.tenant_id, request.run.contact_id);
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&snapshot.run, &request.run)?;
    if snapshot.run.status == ExecutionRunStatus::Cancelled {
        return Ok(replayed_mutation(&snapshot.run));
    }
    if let Some(pending) = &snapshot.run.pending_terminal {
        return Ok(if pending.status == ExecutionRunStatus::Cancelled {
            replayed_mutation(&snapshot.run)
        } else {
            conflict_mutation(ExecutionConflictReason::AlreadyTerminal)
        });
    }
    let terminal_evidence = cancellation_terminal_evidence(
        &snapshot.run.goal,
        &snapshot.run.active_plan,
        &snapshot.projection,
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
            .fence_run_for_terminal(
                scope,
                snapshot.run.run_uid,
                snapshot.run.plan_revision,
                snapshot.run.wake_epoch,
                pending_terminal,
            )
            .await
            .map_err(execution_error)?
        {
            TerminalFenceOutcome::Applied(commit) => applied_mutation(&commit.run),
            TerminalFenceOutcome::Replayed(commit) => replayed_mutation(&commit.run),
            TerminalFenceOutcome::NotFound => not_found_mutation(),
            TerminalFenceOutcome::Conflict => {
                conflict_mutation(ExecutionConflictReason::AlreadyTerminal)
            }
        },
    )
}

pub(super) async fn deliver_input_inner(
    pool: sqlx::PgPool,
    request: ExecutionInputRequest,
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
    if !matches!(
        task.kind,
        moa_execution::state::LogicalTaskKind::Review { .. }
    ) {
        return Ok(conflict_mutation(ExecutionConflictReason::InvalidStatus));
    }
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
    let signal_matches = matches!(
        &task.kind,
        moa_execution::state::LogicalTaskKind::WaitSignal { signal_name }
            if signal_name == &request.signal_name
    );
    if !signal_matches {
        return Ok(conflict_mutation(ExecutionConflictReason::SignalMismatch));
    }
    external_wait_mutation(
        &repository,
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
    scope: ExecutionScope,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    generation: u64,
    result: ExecutionTaskResult,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    if task.generation == generation
        && task.status == ExecutionTaskStatus::Running
        && let ExecutionTaskResult::Completed { output, .. } = &result
    {
        validate_external_wait_payload(&run.active_plan.definition, &task.node_id, output)?;
    }
    let write = repository
        .complete_external_wait(
            scope,
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
    let Some(snapshot) = repository
        .load_scheduling_snapshot(scope, request.run.run_uid)
        .await
        .map_err(execution_error)?
    else {
        return Ok(not_found_mutation());
    };
    verify_run_request(&snapshot.run, &request.run)?;
    let amendment_digest = amendment_hash(&request.amendment).map_err(execution_error)?;
    match repository
        .recover_amendment_handoff(
            scope,
            snapshot.run.run_uid,
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
    if snapshot.run.plan_revision != request.expected_plan_revision {
        return Ok(conflict_mutation(
            ExecutionConflictReason::PlanRevisionMismatch,
        ));
    }
    let remaining_budget = snapshot
        .budget_ledger
        .remaining_limit()
        .map_err(execution_error)?;
    let waiting_tasks = snapshot
        .projection
        .tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .collect::<Vec<_>>();
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
        catalog: snapshot.catalog.clone(),
        authorization: snapshot.authorization.clone(),
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
            &snapshot,
            waiting_task,
            amendment_digest,
            reason,
            Some(&request.amendment.reason),
        )
        .await;
    }
    let Some(active_plan) = validated.plan else {
        if let ReplanDecision::Stop { reason } = prevalidation_loop_decision {
            return finalize_service_replan_stop(
                &repository,
                scope,
                &snapshot,
                waiting_task,
                amendment_digest,
                reason,
                Some(&request.amendment.reason),
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
            &snapshot,
            waiting_task,
            amendment_digest,
            reason,
            Some(&request.amendment.reason),
        )
        .await;
    }
    let write = repository
        .append_amendment(
            scope,
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

pub(super) async fn finalize_service_replan_stop(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    waiting_task: &ExecutionTaskProjection,
    amendment_digest: ExecutionHash,
    reason: moa_execution::ReplanStopReason,
    detail: Option<&str>,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    let mut evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: snapshot.run.goal.clone(),
        plan: snapshot.run.active_plan.clone(),
        run_input: snapshot.run.input.clone(),
        projection: snapshot.projection.clone(),
        terminal_output: snapshot.run.output.clone(),
        budget_ledger: snapshot.budget_ledger.clone(),
        now: chrono::Utc::now(),
    })
    .map_err(execution_error)?;
    evaluation.status = replan_stop_status(
        snapshot.run.output.is_some(),
        evaluation.satisfied_requirement_ids.len(),
    );
    let stop_gaps = replan_stop_gaps(reason, detail);
    evaluation.gaps.extend(stop_gaps.iter().cloned());
    evaluation.gaps.sort();
    evaluation.gaps.dedup();
    let terminal = terminal_projection_from_evaluation(
        &evaluation,
        snapshot.run.output.clone(),
        None,
        None,
        None,
    )
    .map_err(execution_error)?;
    let terminal_status = moa_execution::state::run_status_from_terminal_projection(&terminal);
    let terminal_evidence = terminal_evidence_from_evaluation(
        ExecutionTerminalCause::ReplanStop { reason },
        &evaluation,
    )
    .map_err(execution_error)?;
    let terminal_reason =
        execution_terminal_reason(&terminal_evidence.cause, &terminal, &evaluation)
            .map_err(execution_error)?;
    let pending_terminal = PendingExecutionTerminal {
        status: terminal_status,
        reason: terminal_reason,
        terminal_evidence,
        output: snapshot.run.output.clone(),
        completion_check_results: evaluation
            .checks
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                invalid_execution_request(format!(
                    "serialize replan-stop completion checks: {error}"
                ))
            })?,
        terminal_gaps: evaluation.gaps,
        cancellation_reason: None,
    };
    Ok(
        match repository
            .fence_replan_stop(
                scope,
                snapshot.run.run_uid,
                snapshot.run.plan_revision,
                snapshot.run.wake_epoch,
                pending_terminal,
                ReplanStopReceipt {
                    task_id: waiting_task.task_id,
                    task_generation: waiting_task.generation,
                    base_plan_revision: snapshot.run.plan_revision,
                    amendment_hash: amendment_digest,
                },
            )
            .await
            .map_err(execution_error)?
        {
            TerminalFenceOutcome::Applied(commit) => applied_mutation(&commit.run),
            TerminalFenceOutcome::Replayed(commit) => replayed_mutation(&commit.run),
            TerminalFenceOutcome::Conflict => {
                conflict_mutation(ExecutionConflictReason::PlanRevisionMismatch)
            }
            TerminalFenceOutcome::NotFound => not_found_mutation(),
        },
    )
}

#[cfg(test)]
/// Applies an amendment through the production inner boundary for library regressions.
pub(crate) async fn apply_amendment_for_test(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionAmendmentRequest,
) -> Result<ExecutionMutationResponse, HandlerError> {
    apply_amendment_inner(pool, config, request)
        .await
        .map(ExecutionMutationAccepted::into_response)
}

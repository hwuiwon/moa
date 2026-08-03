//! Execution Bridge handlers for the Session virtual object.

use super::*;

impl SessionImpl {
    pub(super) async fn handle_execution_run_started(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<ExecutionRunStartedDelivery>,
    ) -> Result<(), HandlerError> {
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

    pub(super) async fn handle_admit_execution_template(
        &self,
        mut ctx: ObjectContext<'_>,
        request: Json<moa_execution::wire::ExecutionTemplateAdmissionRequest>,
    ) -> Result<Json<moa_execution::wire::ExecutionTemplateAdmissionResponse>, HandlerError> {
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
        let identity = require_session_participant(&self.authz, &ctx, session_id).await?;
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

    pub(super) async fn handle_execution_progress(
        &self,
        ctx: ObjectContext<'_>,
        progress: Json<ExecutionProgress>,
    ) -> Result<(), HandlerError> {
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

    pub(super) async fn handle_execution_input_required(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ExecutionInputRequired>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "execution_input_required");
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        accept_execution_input_required(&ctx, &mut state, input.into_inner()).await?;
        state.persist(&ctx);
        Ok(())
    }

    pub(super) async fn handle_execution_terminal(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<moa_execution::wire::ExecutionTerminalDelivery>,
    ) -> Result<Json<ExecutionSynthesisDispatch>, HandlerError> {
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
                resource_budget: ResourceBudget::UNBOUNDED,
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
}

//! Durable coordinator human-input registration and admission parking.

use super::*;

impl SessionImpl {
    pub(in crate::objects::session::handlers) async fn handle_register_coordinator_input(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<RegisterCoordinatorInputRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "register_coordinator_input");
        let request = request.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;

        // Delivery history is the terminal fence for this request identity. A
        // replay after the awakeable was resolved must not advertise the target
        // again or emit a new question for work that has already continued.
        if state.coordinator_input_already_delivered(&request.input_request_id) {
            return Ok(());
        }
        if state.security_circuit.owner.as_ref()
            != Some(
                &moa_core::types::security::SecurityCircuitOwner::Coordinator {
                    turn_id: request.turn_id.clone(),
                    generation: request.generation,
                },
            )
        {
            return Err(TerminalError::new_with_code(
                409,
                "coordinator input registration owner is no longer active",
            )
            .into());
        }
        let mut pending_state = load_pending_state(&ctx).await?;
        let expected_park = ParkedCoordinatorAdmission {
            turn_id: request.turn_id.clone(),
            generation: request.generation,
        };
        if pending_state.active_turn_id.as_deref() != Some(request.turn_id.as_str())
            || pending_state
                .turn_admission_parked
                .as_ref()
                .is_some_and(|parked| parked != &expected_park)
        {
            return Err(TerminalError::new_with_code(
                409,
                "coordinator input registration does not match the active turn admission",
            )
            .into());
        }

        let registered = state.register_coordinator_input(CoordinatorPendingInput {
            turn_id: request.turn_id.clone(),
            generation: request.generation,
            input_request_id: request.input_request_id.clone(),
            awakeable_id: request.awakeable_id.clone(),
            waiting_workflow_id: request.waiting_workflow_id.clone(),
        });
        if registered {
            state.upsert_pending_user_reply_target(PendingUserReplyTarget::CoordinatorInput {
                turn_id: request.turn_id.clone(),
                generation: request.generation,
                input_request_id: request.input_request_id.clone(),
            });
            append_session_event_deduped(
                &ctx,
                session_id,
                Event::Warning {
                    message: request.question,
                },
                format!("coordinator_input_request:{}", request.input_request_id),
            )
            .await?;
            state.persist(&ctx);
        }

        // Persist the awakeable, reply target, and park fence before releasing the
        // shared admission lease. Duplicate registration repeats the idempotent
        // release, closing the crash window between those two durable systems.
        if pending_state.park_turn_admission(&request.turn_id, request.generation) {
            persist_pending_state(&ctx, &pending_state);
        }
        let tenant_id = state
            .ensure_initialized()
            .map_err(moa_error_to_handler_error)?
            .tenant_id;
        self.turn_admission
            .release(&ctx, session_id, tenant_id)
            .await?;
        Ok(())
    }

    pub(in crate::objects::session::handlers) async fn handle_clear_coordinator_input(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ClearCoordinatorInputRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "clear_coordinator_input");
        let request = request.into_inner();
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        if state.clear_coordinator_input(
            &request.turn_id,
            request.generation,
            &request.input_request_id,
            &request.waiting_workflow_id,
        ) {
            state.persist(&ctx);
        }
        Ok(())
    }
}

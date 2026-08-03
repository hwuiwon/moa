//! Reviews handlers for the Session virtual object.

use super::*;

impl SessionImpl {
    pub(super) async fn handle_register_action_review(
        &self,
        ctx: ObjectContext<'_>,
        registration: Json<moa_core::types::action_policy::ActionReviewRegistration>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "register_action_review");
        let registration = registration.into_inner();
        let turn_id = registration
            .owner
            .turn_id()
            .ok_or_else(|| {
                TerminalError::new("session action review registration requires an owning turn")
            })?
            .to_string();
        // The generation comes from the owner that issued the tool call, not from
        // whatever the session happens to be on now. Reading "now" would let a user
        // message admitted between the tool call and this registration re-stamp a stale
        // review as current, and the fence would then resume superseded work.
        let generation = registration.owner.generation().ok_or_else(|| {
            TerminalError::new("session action review registration requires an owner generation")
        })?;
        let mut pending_state = load_pending_state(&ctx).await?;
        if generation < pending_state.turn_generation {
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation,
                current_generation = pending_state.turn_generation,
                "skipped registering an already-superseded session action review"
            );
            return Ok(());
        }
        if pending_state
            .action_reviews
            .register(registration.review_id, turn_id, generation)
        {
            persist_pending_state(&ctx, &pending_state);
            tracing::info!(
                key = %ctx.key(),
                review_id = %registration.review_id,
                generation,
                "registered pending action review on session"
            );
        }
        Ok(())
    }

    pub(super) async fn handle_action_review_resolved(
        &self,
        mut ctx: ObjectContext<'_>,
        receipt: Json<moa_core::types::action_policy::ActionReviewReceipt>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "action_review_resolved");
        let receipt = receipt.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        let mut pending_state = load_pending_state(&ctx).await?;
        // Unknown or already-resolved review: a duplicated callback changes nothing.
        let Some(registered) = pending_state.action_reviews.resolve(receipt.review_id) else {
            tracing::debug!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                "ignored resolution for an unknown or already-resolved session action review"
            );
            return Ok(());
        };
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let cancelling = pending_state.task_tree_cancellation_fenced();
        let terminal_session = matches!(
            state.status,
            Some(SessionStatus::Cancelled) | Some(SessionStatus::Failed)
        );
        if registered.generation != pending_state.turn_generation || cancelling || terminal_session
        {
            persist_pending_state(&ctx, &pending_state);
            tracing::info!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                registered_generation = registered.generation,
                current_generation = pending_state.turn_generation,
                cancelling,
                terminal_session,
                "dropped superseded or cancelled session action review continuation"
            );
            return Ok(());
        }

        let Some(identity) = state.owning_identity.clone() else {
            persist_pending_state(&ctx, &pending_state);
            tracing::warn!(
                key = %ctx.key(),
                review_id = %receipt.review_id,
                "session action review resolved but no owning identity is recorded"
            );
            return Ok(());
        };
        let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
        // Minted before any scheduling decision so the durable continuation fact names
        // the exact turn that will run it, even when it has to wait behind the origin.
        let continuation_turn_id = generate_turn_id(&mut ctx);
        let entry = QueuedActionReviewContinuation {
            continuation: moa_core::types::action_policy::ActionReviewContinuation { receipt },
            turn_id: continuation_turn_id,
            generation: registered.generation,
            ordinal: registered.ordinal,
        };
        let fact = entry.clone();
        if !pending_state.action_reviews.enqueue(entry) {
            persist_pending_state(&ctx, &pending_state);
            return Ok(());
        }
        let dispatch = if pending_state.active_turn_id.is_some() {
            None
        } else {
            pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        };
        if let Some(dispatch) = dispatch.as_ref() {
            pending_state.active_turn_id = Some(dispatch.turn_id.clone());
            activate_coordinator_security_owner(&mut state, &dispatch.turn_id, dispatch.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
        }
        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        sync_status(&ctx, session_id, &state).await?;

        append_session_event_deduped(
            &ctx,
            session_id,
            Event::ActionReviewContinuationRequested {
                review_id: fact.continuation.receipt.review_id,
                turn_id: fact.turn_id.clone(),
                receipt: fact.continuation.receipt.clone(),
            },
            moa_core::types::action_policy::action_review_continuation_dedupe_key(
                fact.continuation.receipt.review_id,
            ),
        )
        .await?;

        if let Some(dispatch) = dispatch {
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    dispatch.turn_id,
                    identity,
                    contact,
                    dispatch.generation,
                    dispatch.continuation,
                ),
            );
        }
        Ok(())
    }

    pub(super) async fn handle_release_action_review(
        &self,
        ctx: ObjectContext<'_>,
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "release_action_review");
        let release = release.into_inner();
        let session_id = parse_session_key(ctx.key())?;
        if !matches!(
            &release.owner,
            moa_core::types::action_policy::ActionReviewOwner::Coordinator {
                session_id: owner_session,
                ..
            } if *owner_session == session_id
        ) {
            return Err(TerminalError::new(
                "action review release does not belong to this session coordinator",
            )
            .into());
        }
        let mut pending_state = load_pending_state(&ctx).await?;
        if pending_state
            .action_reviews
            .resolve(release.review_id)
            .is_none()
        {
            return Ok(());
        }
        let mut state = Tracked::<SessionVoState>::load(&ctx).await?;
        let identity = state.owning_identity.clone();
        let contact = state.meta.as_ref().and_then(|meta| meta.contact.clone());
        let can_dispatch = release.resume_queued
            && pending_state.active_turn_id.is_none()
            && !pending_state.task_tree_cancellation_fenced()
            && !matches!(
                state.status,
                Some(SessionStatus::Cancelled) | Some(SessionStatus::Failed)
            )
            && identity.is_some();
        let dispatch = if can_dispatch {
            pending_state
                .action_reviews
                .take_next(pending_state.turn_generation)
        } else {
            None
        };
        if let Some(dispatch) = dispatch.as_ref() {
            pending_state.active_turn_id = Some(dispatch.turn_id.clone());
            activate_coordinator_security_owner(&mut state, &dispatch.turn_id, dispatch.generation);
            let now = durable_utc_now(&ctx).await?;
            state.set_status(SessionStatus::Running, now);
        }
        state.persist(&ctx);
        persist_pending_state(&ctx, &pending_state);
        sync_status(&ctx, session_id, &state).await?;
        if let (Some(dispatch), Some(identity)) = (dispatch, identity) {
            dispatch_turn_execution(
                &ctx,
                action_review_run_request(
                    ctx.key().to_string(),
                    dispatch.turn_id,
                    identity,
                    contact,
                    dispatch.generation,
                    dispatch.continuation,
                ),
            );
        }
        Ok(())
    }
}

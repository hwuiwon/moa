//! Worker VO state loading and changed-key persistence.

use super::*;

impl VoState for WorkerVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            status: reader.get_json(K_STATUS).await?,
            parent_session: reader.get_json(K_PARENT_SESSION).await?,
            depth: reader.get_json(K_DEPTH).await?.unwrap_or_default(),
            budget_remaining: reader
                .get_json(K_BUDGET_REMAINING)
                .await?
                .unwrap_or_default(),
            tokens_used: reader.get_json(K_TOKENS_USED).await?.unwrap_or_default(),
            task: reader.get_json(K_TASK).await?,
            identity: reader.get_json(K_IDENTITY).await?,
            tool_subset: reader.get_json(K_TOOL_SUBSET).await?.unwrap_or_default(),
            tenant_id: reader.get_json(K_TENANT_ID).await?,
            user_id: reader.get_json(K_USER_ID).await?,
            model: reader.get_json(K_MODEL).await?,
            max_turns: reader.get_json(K_MAX_TURNS).await?,
            trusted_sandbox_manifest: reader.get_json(K_TRUSTED_SANDBOX_MANIFEST).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            history: reader.get_json(K_HISTORY).await?.unwrap_or_default(),
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            tools_invoked: reader.get_json(K_TOOLS_INVOKED).await?.unwrap_or_default(),
            cancel_reason: reader.get_json(K_CANCEL_REASON).await?,
            active_turn_id: reader.get_json(K_ACTIVE_TURN_ID).await?,
            security_circuit: reader
                .get_json(K_SECURITY_CIRCUIT)
                .await?
                .unwrap_or_default(),
            last_outcome: reader.get_json(K_LAST_OUTCOME).await?,
            notification_delivered: reader
                .get_json(K_NOTIFICATION_DELIVERED)
                .await?
                .unwrap_or_default(),
            result_waiters: reader.get_json(K_RESULT_WAITERS).await?.unwrap_or_default(),
            last_heartbeat_at: reader.get_json(K_LAST_HEARTBEAT_AT).await?,
            liveness_generation: reader
                .get_json(K_LIVENESS_GENERATION)
                .await?
                .unwrap_or_default(),
            liveness_outstanding: reader
                .get_json(K_LIVENESS_OUTSTANDING)
                .await?
                .unwrap_or_default(),
            cleanup_generation: reader
                .get_json(K_CLEANUP_GENERATION)
                .await?
                .unwrap_or_default(),
            cleanup_release_attempts: reader
                .get_json(K_CLEANUP_RELEASE_ATTEMPTS)
                .await?
                .unwrap_or_default(),
            pending_input_requests: reader
                .get_json(K_PENDING_INPUT_REQUESTS)
                .await?
                .unwrap_or_default(),
            input_delivery_history: reader
                .get_json(K_INPUT_DELIVERY_HISTORY)
                .await?
                .unwrap_or_default(),
            generation: reader.get_json(K_GENERATION).await?.unwrap_or_default(),
            action_reviews: reader.get_json(K_ACTION_REVIEWS).await?.unwrap_or_default(),
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_opt(ctx, K_PARENT_SESSION, self.parent_session.as_ref());
        set_or_clear_scalar(ctx, K_DEPTH, self.depth, 0);
        set_or_clear_scalar(ctx, K_BUDGET_REMAINING, self.budget_remaining, 0);
        set_or_clear_scalar(ctx, K_TOKENS_USED, self.tokens_used, 0);
        set_or_clear_opt(ctx, K_TASK, self.task.as_ref());
        set_or_clear_opt(ctx, K_IDENTITY, self.identity.as_ref());
        set_or_clear_vec(ctx, K_TOOL_SUBSET, &self.tool_subset);
        set_or_clear_opt(ctx, K_TENANT_ID, self.tenant_id.as_ref());
        set_or_clear_opt(ctx, K_USER_ID, self.user_id.as_ref());
        set_or_clear_opt(ctx, K_MODEL, self.model.as_ref());
        set_or_clear_opt(ctx, K_MAX_TURNS, self.max_turns.as_ref());
        set_or_clear_opt(
            ctx,
            K_TRUSTED_SANDBOX_MANIFEST,
            self.trusted_sandbox_manifest.as_ref(),
        );
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_vec(ctx, K_HISTORY, &self.history);
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_scalar(ctx, K_TOOLS_INVOKED, self.tools_invoked, 0);
        set_or_clear_opt(ctx, K_CANCEL_REASON, self.cancel_reason.as_ref());
        set_or_clear_opt(ctx, K_ACTIVE_TURN_ID, self.active_turn_id.as_ref());
        set_or_clear_opt(
            ctx,
            K_SECURITY_CIRCUIT,
            (self.security_circuit != SecurityCircuitState::default())
                .then_some(&self.security_circuit),
        );
        set_or_clear_opt(ctx, K_LAST_OUTCOME, self.last_outcome.as_ref());
        set_or_clear_scalar(
            ctx,
            K_NOTIFICATION_DELIVERED,
            self.notification_delivered,
            false,
        );
        set_or_clear_vec(ctx, K_RESULT_WAITERS, &self.result_waiters);
        set_or_clear_opt(ctx, K_LAST_HEARTBEAT_AT, self.last_heartbeat_at.as_ref());
        set_or_clear_scalar(ctx, K_LIVENESS_GENERATION, self.liveness_generation, 0);
        set_or_clear_scalar(
            ctx,
            K_LIVENESS_OUTSTANDING,
            self.liveness_outstanding,
            false,
        );
        set_or_clear_scalar(ctx, K_CLEANUP_GENERATION, self.cleanup_generation, 0);
        set_or_clear_scalar(
            ctx,
            K_CLEANUP_RELEASE_ATTEMPTS,
            self.cleanup_release_attempts,
            0,
        );
        set_or_clear_vec(ctx, K_PENDING_INPUT_REQUESTS, &self.pending_input_requests);
        set_or_clear_vec(ctx, K_INPUT_DELIVERY_HISTORY, &self.input_delivery_history);
        set_or_clear_scalar(ctx, K_GENERATION, self.generation, 0);
        set_or_clear_scalar(
            ctx,
            K_ACTION_REVIEWS,
            self.action_reviews.clone(),
            ActionReviewSchedule::default(),
        );
    }

    fn persist_changes(&self, ctx: &ObjectContext<'_>, baseline: &Self) {
        set_changed_opt(
            ctx,
            K_STATUS,
            self.status.as_ref(),
            baseline.status.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_PARENT_SESSION,
            self.parent_session.as_ref(),
            baseline.parent_session.as_ref(),
        );
        set_changed_scalar(ctx, K_DEPTH, self.depth, &baseline.depth, 0);
        set_changed_scalar(
            ctx,
            K_BUDGET_REMAINING,
            self.budget_remaining,
            &baseline.budget_remaining,
            0,
        );
        set_changed_scalar(
            ctx,
            K_TOKENS_USED,
            self.tokens_used,
            &baseline.tokens_used,
            0,
        );
        set_changed_opt(ctx, K_TASK, self.task.as_ref(), baseline.task.as_ref());
        set_changed_opt(
            ctx,
            K_IDENTITY,
            self.identity.as_ref(),
            baseline.identity.as_ref(),
        );
        set_changed_vec(ctx, K_TOOL_SUBSET, &self.tool_subset, &baseline.tool_subset);
        set_changed_opt(
            ctx,
            K_TENANT_ID,
            self.tenant_id.as_ref(),
            baseline.tenant_id.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_USER_ID,
            self.user_id.as_ref(),
            baseline.user_id.as_ref(),
        );
        set_changed_opt(ctx, K_MODEL, self.model.as_ref(), baseline.model.as_ref());
        set_changed_opt(
            ctx,
            K_MAX_TURNS,
            self.max_turns.as_ref(),
            baseline.max_turns.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_TRUSTED_SANDBOX_MANIFEST,
            self.trusted_sandbox_manifest.as_ref(),
            baseline.trusted_sandbox_manifest.as_ref(),
        );
        set_changed_vec(ctx, K_PENDING, &self.pending, &baseline.pending);
        set_changed_vec(ctx, K_HISTORY, &self.history, &baseline.history);
        set_changed_vec(ctx, K_CHILDREN, &self.children, &baseline.children);
        set_changed_opt(
            ctx,
            K_LAST_TURN_SUMMARY,
            self.last_turn_summary.as_ref(),
            baseline.last_turn_summary.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_TOOLS_INVOKED,
            self.tools_invoked,
            &baseline.tools_invoked,
            0,
        );
        set_changed_opt(
            ctx,
            K_CANCEL_REASON,
            self.cancel_reason.as_ref(),
            baseline.cancel_reason.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_ACTIVE_TURN_ID,
            self.active_turn_id.as_ref(),
            baseline.active_turn_id.as_ref(),
        );
        if self.security_circuit != baseline.security_circuit {
            set_or_clear_opt(
                ctx,
                K_SECURITY_CIRCUIT,
                (self.security_circuit != SecurityCircuitState::default())
                    .then_some(&self.security_circuit),
            );
        }
        set_changed_opt(
            ctx,
            K_LAST_OUTCOME,
            self.last_outcome.as_ref(),
            baseline.last_outcome.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_NOTIFICATION_DELIVERED,
            self.notification_delivered,
            &baseline.notification_delivered,
            false,
        );
        set_changed_vec(
            ctx,
            K_RESULT_WAITERS,
            &self.result_waiters,
            &baseline.result_waiters,
        );
        set_changed_opt(
            ctx,
            K_LAST_HEARTBEAT_AT,
            self.last_heartbeat_at.as_ref(),
            baseline.last_heartbeat_at.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_LIVENESS_GENERATION,
            self.liveness_generation,
            &baseline.liveness_generation,
            0,
        );
        set_changed_scalar(
            ctx,
            K_LIVENESS_OUTSTANDING,
            self.liveness_outstanding,
            &baseline.liveness_outstanding,
            false,
        );
        set_changed_scalar(
            ctx,
            K_CLEANUP_GENERATION,
            self.cleanup_generation,
            &baseline.cleanup_generation,
            0,
        );
        set_changed_scalar(
            ctx,
            K_CLEANUP_RELEASE_ATTEMPTS,
            self.cleanup_release_attempts,
            &baseline.cleanup_release_attempts,
            0,
        );
        set_changed_vec(
            ctx,
            K_PENDING_INPUT_REQUESTS,
            &self.pending_input_requests,
            &baseline.pending_input_requests,
        );
        set_changed_vec(
            ctx,
            K_INPUT_DELIVERY_HISTORY,
            &self.input_delivery_history,
            &baseline.input_delivery_history,
        );
        set_changed_scalar(ctx, K_GENERATION, self.generation, &baseline.generation, 0);
        set_changed_scalar(
            ctx,
            K_ACTION_REVIEWS,
            self.action_reviews.clone(),
            &baseline.action_reviews,
            ActionReviewSchedule::default(),
        );
    }
}

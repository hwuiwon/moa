//! Restate key loading and delta persistence for Session state.

use super::*;

impl VoState for SessionVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            meta: reader.get_json(K_META).await?,
            status: reader.get_json(K_STATUS).await?,
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            current_segment: reader.get_json(K_CURRENT_SEGMENT).await?,
            narration_tick_generation: reader
                .get_json(K_NARRATION_TICK_GENERATION)
                .await?
                .unwrap_or_default(),
            narration_tick_outstanding: reader
                .get_json(K_NARRATION_TICK_OUTSTANDING)
                .await?
                .unwrap_or_default(),
            narration_seq: reader.get_json(K_NARRATION_SEQ).await?.unwrap_or_default(),
            last_narrated_marker: reader.get_json(K_LAST_NARRATED_MARKER).await?,
            last_narration_at: reader.get_json(K_LAST_NARRATION_AT).await?,
            narration_window_start: reader.get_json(K_NARRATION_WINDOW_START).await?,
            narration_window_count: reader
                .get_json(K_NARRATION_WINDOW_COUNT)
                .await?
                .unwrap_or_default(),
            owning_identity: reader.get_json(K_OWNING_IDENTITY).await?,
            unread_child_signals: reader
                .get_json(K_UNREAD_CHILD_SIGNALS)
                .await?
                .unwrap_or_default(),
            pending_parent_resume_signal: reader.get_json(K_PENDING_PARENT_RESUME_SIGNAL).await?,
            resume_budget: reader.get_json(K_RESUME_BUDGET).await?.unwrap_or_default(),
            resume_turn: reader.get_json(K_RESUME_TURN).await?,
            child_liveness_generation: reader
                .get_json(K_CHILD_LIVENESS_GENERATION)
                .await?
                .unwrap_or_default(),
            child_liveness: reader.get_json(K_CHILD_LIVENESS).await?.unwrap_or_default(),
            child_terminal_blobs: reader
                .get_json(K_CHILD_TERMINAL_BLOBS)
                .await?
                .unwrap_or_default(),
            active_execution_runs: reader
                .get_json(K_ACTIVE_EXECUTION_RUNS)
                .await?
                .unwrap_or_default(),
            pending_user_reply_targets: reader
                .get_json(K_PENDING_USER_REPLY_TARGETS)
                .await?
                .unwrap_or_default(),
            execution_synthesis_dedupe: reader
                .get_json(K_EXECUTION_SYNTHESIS_DEDUPE)
                .await?
                .unwrap_or_default(),
            pending_coordinator_inputs: reader
                .get_json(K_PENDING_COORDINATOR_INPUTS)
                .await?
                .unwrap_or_default(),
            coordinator_input_history: reader
                .get_json(K_COORDINATOR_INPUT_HISTORY)
                .await?
                .unwrap_or_default(),
            security_circuit: reader
                .get_json(K_SECURITY_CIRCUIT)
                .await?
                .unwrap_or_default(),
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_META, self.meta.as_ref());
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_opt(ctx, K_CURRENT_SEGMENT, self.current_segment.as_ref());
        set_or_clear_scalar(
            ctx,
            K_NARRATION_TICK_GENERATION,
            self.narration_tick_generation,
            0,
        );
        set_or_clear_scalar(
            ctx,
            K_NARRATION_TICK_OUTSTANDING,
            self.narration_tick_outstanding,
            false,
        );
        set_or_clear_scalar(ctx, K_NARRATION_SEQ, self.narration_seq, 0);
        set_or_clear_opt(
            ctx,
            K_LAST_NARRATED_MARKER,
            self.last_narrated_marker.as_ref(),
        );
        set_or_clear_opt(ctx, K_LAST_NARRATION_AT, self.last_narration_at.as_ref());
        set_or_clear_opt(
            ctx,
            K_NARRATION_WINDOW_START,
            self.narration_window_start.as_ref(),
        );
        set_or_clear_scalar(
            ctx,
            K_NARRATION_WINDOW_COUNT,
            self.narration_window_count,
            0,
        );
        set_or_clear_opt(ctx, K_OWNING_IDENTITY, self.owning_identity.as_ref());
        set_or_clear_vec(ctx, K_UNREAD_CHILD_SIGNALS, &self.unread_child_signals);
        set_or_clear_opt(
            ctx,
            K_PENDING_PARENT_RESUME_SIGNAL,
            self.pending_parent_resume_signal.as_ref(),
        );
        set_or_clear_scalar(
            ctx,
            K_RESUME_BUDGET,
            self.resume_budget.clone(),
            ResumeBudget::default(),
        );
        set_or_clear_opt(ctx, K_RESUME_TURN, self.resume_turn.as_ref());
        set_or_clear_scalar(
            ctx,
            K_CHILD_LIVENESS_GENERATION,
            self.child_liveness_generation,
            0,
        );
        set_or_clear_vec(ctx, K_CHILD_LIVENESS, &self.child_liveness);
        set_or_clear_vec(ctx, K_CHILD_TERMINAL_BLOBS, &self.child_terminal_blobs);
        set_or_clear_vec(ctx, K_ACTIVE_EXECUTION_RUNS, &self.active_execution_runs);
        set_or_clear_vec(
            ctx,
            K_PENDING_USER_REPLY_TARGETS,
            &self.pending_user_reply_targets,
        );
        set_or_clear_vec(
            ctx,
            K_EXECUTION_SYNTHESIS_DEDUPE,
            &self.execution_synthesis_dedupe,
        );
        set_or_clear_vec(
            ctx,
            K_PENDING_COORDINATOR_INPUTS,
            &self.pending_coordinator_inputs,
        );
        set_or_clear_vec(
            ctx,
            K_COORDINATOR_INPUT_HISTORY,
            &self.coordinator_input_history,
        );
        set_or_clear_opt(
            ctx,
            K_SECURITY_CIRCUIT,
            (self.security_circuit != SecurityCircuitState::default())
                .then_some(&self.security_circuit),
        );
    }

    fn persist_changes(&self, ctx: &ObjectContext<'_>, baseline: &Self) {
        set_changed_opt(ctx, K_META, self.meta.as_ref(), baseline.meta.as_ref());
        set_changed_opt(
            ctx,
            K_STATUS,
            self.status.as_ref(),
            baseline.status.as_ref(),
        );
        set_changed_vec(ctx, K_CHILDREN, &self.children, &baseline.children);
        set_changed_opt(
            ctx,
            K_LAST_TURN_SUMMARY,
            self.last_turn_summary.as_ref(),
            baseline.last_turn_summary.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_CURRENT_SEGMENT,
            self.current_segment.as_ref(),
            baseline.current_segment.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_NARRATION_TICK_GENERATION,
            self.narration_tick_generation,
            &baseline.narration_tick_generation,
            0,
        );
        set_changed_scalar(
            ctx,
            K_NARRATION_TICK_OUTSTANDING,
            self.narration_tick_outstanding,
            &baseline.narration_tick_outstanding,
            false,
        );
        set_changed_scalar(
            ctx,
            K_NARRATION_SEQ,
            self.narration_seq,
            &baseline.narration_seq,
            0,
        );
        set_changed_opt(
            ctx,
            K_LAST_NARRATED_MARKER,
            self.last_narrated_marker.as_ref(),
            baseline.last_narrated_marker.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_LAST_NARRATION_AT,
            self.last_narration_at.as_ref(),
            baseline.last_narration_at.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_NARRATION_WINDOW_START,
            self.narration_window_start.as_ref(),
            baseline.narration_window_start.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_NARRATION_WINDOW_COUNT,
            self.narration_window_count,
            &baseline.narration_window_count,
            0,
        );
        set_changed_opt(
            ctx,
            K_OWNING_IDENTITY,
            self.owning_identity.as_ref(),
            baseline.owning_identity.as_ref(),
        );
        set_changed_vec(
            ctx,
            K_UNREAD_CHILD_SIGNALS,
            &self.unread_child_signals,
            &baseline.unread_child_signals,
        );
        set_changed_opt(
            ctx,
            K_PENDING_PARENT_RESUME_SIGNAL,
            self.pending_parent_resume_signal.as_ref(),
            baseline.pending_parent_resume_signal.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_RESUME_BUDGET,
            self.resume_budget.clone(),
            &baseline.resume_budget,
            ResumeBudget::default(),
        );
        set_changed_opt(
            ctx,
            K_RESUME_TURN,
            self.resume_turn.as_ref(),
            baseline.resume_turn.as_ref(),
        );
        set_changed_scalar(
            ctx,
            K_CHILD_LIVENESS_GENERATION,
            self.child_liveness_generation,
            &baseline.child_liveness_generation,
            0,
        );
        set_changed_vec(
            ctx,
            K_CHILD_LIVENESS,
            &self.child_liveness,
            &baseline.child_liveness,
        );
        set_changed_vec(
            ctx,
            K_CHILD_TERMINAL_BLOBS,
            &self.child_terminal_blobs,
            &baseline.child_terminal_blobs,
        );
        set_changed_vec(
            ctx,
            K_ACTIVE_EXECUTION_RUNS,
            &self.active_execution_runs,
            &baseline.active_execution_runs,
        );
        set_changed_vec(
            ctx,
            K_PENDING_USER_REPLY_TARGETS,
            &self.pending_user_reply_targets,
            &baseline.pending_user_reply_targets,
        );
        set_changed_vec(
            ctx,
            K_EXECUTION_SYNTHESIS_DEDUPE,
            &self.execution_synthesis_dedupe,
            &baseline.execution_synthesis_dedupe,
        );
        set_changed_vec(
            ctx,
            K_PENDING_COORDINATOR_INPUTS,
            &self.pending_coordinator_inputs,
            &baseline.pending_coordinator_inputs,
        );
        set_changed_vec(
            ctx,
            K_COORDINATOR_INPUT_HISTORY,
            &self.coordinator_input_history,
            &baseline.coordinator_input_history,
        );
        if self.security_circuit != baseline.security_circuit {
            set_or_clear_opt(
                ctx,
                K_SECURITY_CIRCUIT,
                (self.security_circuit != SecurityCircuitState::default())
                    .then_some(&self.security_circuit),
            );
        }
    }
}

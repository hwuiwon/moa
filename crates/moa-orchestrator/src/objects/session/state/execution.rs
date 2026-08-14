//! Session execution state behavior.

use super::*;

impl SessionVoState {
    /// Loads minimal active execution-run markers for shared snapshot and progress reads.
    pub(in crate::objects::session) async fn load_active_execution_runs<R: VoReader>(
        reader: &R,
    ) -> Result<Vec<ActiveExecutionRunState>, HandlerError> {
        Ok(reader
            .get_json(K_ACTIVE_EXECUTION_RUNS)
            .await?
            .unwrap_or_default())
    }

    /// Returns the last published aggregate progress for every active execution run.
    #[must_use]
    pub fn project_active_execution_progress(
        active_execution_runs: &[ActiveExecutionRunState],
    ) -> Vec<moa_core::events::ExecutionProgress> {
        active_execution_runs
            .iter()
            .filter_map(|run| run.progress.clone())
            .collect()
    }

    /// Applies aggregate progress only when both cadence and exact tuple delta gates pass.
    pub fn apply_execution_progress(
        &mut self,
        progress: moa_core::events::ExecutionProgress,
        now: DateTime<Utc>,
        progress_interval_ms: u64,
    ) -> MoaResult<bool> {
        let Some(run) = self
            .active_execution_runs
            .iter_mut()
            .find(|run| run.run_uid == progress.run_uid)
        else {
            return Err(MoaError::ValidationError(format!(
                "execution progress references inactive run {}",
                progress.run_uid
            )));
        };
        if run.originating_user_sequence_num != progress.originating_user_sequence_num {
            return Err(MoaError::ValidationError(
                "execution progress origin conflicts with admitted run".to_string(),
            ));
        }

        let signature = ExecutionProgressSignature::from(&progress);
        let changed = run.last_progress_signature.as_ref() != Some(&signature);
        let immediate_transition =
            progress_transition_requires_immediate_publication(run.progress.as_ref(), &progress);
        let cadence_due = run.last_progress_at.is_none_or(|last| {
            let elapsed_ms = now.signed_duration_since(last).num_milliseconds();
            elapsed_ms >= i64::try_from(progress_interval_ms).unwrap_or(i64::MAX)
        });
        run.progress = Some(progress);
        if !(changed && (immediate_transition || cadence_due)) {
            return Ok(false);
        }

        run.last_progress_signature = Some(signature);
        run.last_progress_at = Some(now);
        Ok(true)
    }

    /// Returns a prior stable synthesis dispatch marker for the same run and origin.
    #[must_use]
    pub fn execution_synthesis_marker(
        &self,
        run_uid: uuid::Uuid,
        originating_user_sequence_num: u64,
    ) -> Option<&ExecutionSynthesisDedupe> {
        self.execution_synthesis_dedupe.iter().find(|marker| {
            marker.run_uid == run_uid
                && marker.originating_user_sequence_num == originating_user_sequence_num
        })
    }

    /// Commits synthesis dedupe and clears active run state after durable dispatch.
    pub fn record_execution_synthesis_dispatch(
        &mut self,
        marker: ExecutionSynthesisDedupe,
    ) -> MoaResult<()> {
        if let Some(existing) =
            self.execution_synthesis_marker(marker.run_uid, marker.originating_user_sequence_num)
        {
            if existing != &marker {
                return Err(MoaError::ValidationError(
                    "execution synthesis replay conflicts with stable turn".to_string(),
                ));
            }
            return Ok(());
        }
        let Some(run) = self
            .active_execution_runs
            .iter()
            .find(|run| run.run_uid == marker.run_uid)
        else {
            return Err(MoaError::ValidationError(
                "execution synthesis dispatch references inactive run".to_string(),
            ));
        };
        if run.originating_user_sequence_num != marker.originating_user_sequence_num {
            return Err(MoaError::ValidationError(
                "execution synthesis dispatch origin conflicts with admitted run".to_string(),
            ));
        }

        self.execution_synthesis_dedupe.push(marker.clone());
        self.execution_synthesis_dedupe
            .sort_by_key(|entry| (entry.run_uid, entry.originating_user_sequence_num));
        self.active_execution_runs
            .retain(|run| run.run_uid != marker.run_uid);
        self.pending_user_reply_targets
            .retain(|target| !pending_reply_belongs_to_run(target, marker.run_uid));
        Ok(())
    }
}

fn progress_transition_requires_immediate_publication(
    previous: Option<&moa_core::events::ExecutionProgress>,
    next: &moa_core::events::ExecutionProgress,
) -> bool {
    previous.is_none_or(|previous| {
        previous.plan_revision != next.plan_revision
            || previous.status != next.status
            || previous.phase != next.phase
            || previous.waiting_since != next.waiting_since
            || previous.next_wake_at != next.next_wake_at
            || previous.external_job_uid != next.external_job_uid
            || previous.parked_tasks != next.parked_tasks
            || previous.blocker_audience != next.blocker_audience
    })
}

fn pending_reply_belongs_to_run(target: &PendingUserReplyTarget, run_uid: uuid::Uuid) -> bool {
    matches!(
        target,
        PendingUserReplyTarget::ExecutionConfirmation {
            run_uid: target_run_uid,
            ..
        } | PendingUserReplyTarget::ExecutionInput {
            run_uid: target_run_uid,
            ..
        } if *target_run_uid == run_uid
    )
}

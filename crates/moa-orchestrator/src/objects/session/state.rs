//! Durable Session VO state projection.

use super::*;
use moa_core::traits::Identity;
use moa_core::{
    types::identifiers::AgentSignalId, types::session::TurnOutcome,
    types::worker::state::ChildSignalKind, types::worker::state::ParentResumePolicy,
    types::worker::state::UnreadChildSignal, types::worker::state::WorkerSignal,
};

/// Cycle-safe core pending target instantiated with the artifact-owned execution budget.
pub type PendingUserReplyTarget =
    moa_wire::turn::PendingUserReplyTarget<moa_artifacts::execution_plan::ExecutionBudgetLimit>;

pub(super) const K_META: &str = "meta";
pub(super) const K_STATUS: &str = "status";
pub(super) const K_PENDING: &str = "pending";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_CANCEL_FLAG: &str = "cancel_flag";
pub(super) const K_CURRENT_SEGMENT: &str = "current_segment";
pub(super) const K_NARRATION_TICK_GENERATION: &str = "narration_tick_generation";
pub(super) const K_NARRATION_TICK_OUTSTANDING: &str = "narration_tick_outstanding";
pub(super) const K_NARRATION_SEQ: &str = "narration_seq";
pub(super) const K_LAST_NARRATED_MARKER: &str = "last_narrated_marker";
pub(super) const K_LAST_NARRATION_AT: &str = "last_narration_at";
pub(super) const K_NARRATION_WINDOW_START: &str = "narration_window_start";
pub(super) const K_NARRATION_WINDOW_COUNT: &str = "narration_window_count";
pub(super) const K_OWNING_IDENTITY: &str = "owning_identity";
pub(super) const K_UNREAD_CHILD_SIGNALS: &str = "unread_child_signals";
pub(super) const K_PENDING_PARENT_RESUME_SIGNAL: &str = "pending_parent_resume_signal";
pub(super) const K_RESUME_BUDGET: &str = "resume_budget";
pub(super) const K_RESUME_TURN: &str = "resume_turn";
pub(super) const K_CHILD_LIVENESS_GENERATION: &str = "child_liveness_generation";
pub(super) const K_CHILD_LIVENESS: &str = "child_liveness";
pub(super) const K_CHILD_TERMINAL_BLOBS: &str = "child_terminal_blobs";
pub(super) const K_ACTIVE_EXECUTION_RUNS: &str = "active_execution_runs";
pub(super) const K_PENDING_USER_REPLY_TARGETS: &str = "pending_user_reply_targets";
pub(super) const K_EXECUTION_SYNTHESIS_DEDUPE: &str = "execution_synthesis_dedupe";

/// Byte threshold above which a terminal child's output is offloaded from `K_CHILDREN` to a
/// content-addressed claim-check blob, leaving a compact preview inline in VO state.
///
/// The `children` registry is re-read on every Session VO load (progress polls, fan-in,
/// terminal marking), so a large worker output embedded inline is deserialized repeatedly.
/// 12 KiB keeps ordinary results inline while offloading the large ones.
pub(super) const CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES: usize = 12 * 1024;

/// Maximum characters retained inline as the preview of a claim-checked child output.
const CHILD_OUTPUT_PREVIEW_CHARS: usize = 512;

/// Reference to a terminal child's full output offloaded to a claim-check blob.
///
/// Held on the Session VO so `consume_child_result` can hydrate the full output for the
/// coordinator while the `children` registry keeps only a compact preview.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildTerminalOutputRef {
    /// Worker whose terminal output was offloaded.
    pub worker_id: WorkerId,
    /// Content-addressed blob holding the full output.
    pub claim_check: ClaimCheck,
}

/// Maximum unread child→parent control-plane signals retained on the coordinator VO.
///
/// Kept small so the control-plane projection never bloats parent state. When the cap
/// is exceeded, action-required kinds (`NeedsInput`/`Blocked`) are preferentially kept
/// over informational `Finding`s during eviction.
pub(super) const MAX_UNREAD_CHILD_SIGNALS: usize = 32;

/// Per-session guarded-resume budget: a rolling window start and the resume count
/// dispatched within it.
///
/// Persisted with the Session VO. The rolling-window cap and length are sourced from
/// `MoaConfig` session limits (`worker_resume_max_per_window` /
/// `worker_resume_window_ms`); the budget is checked by the resume-eligibility gate
/// and consumed only on an actual dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeBudget {
    /// Start of the current rolling resume window, if one has opened.
    pub window_start: Option<DateTime<Utc>>,
    /// Number of guarded resumes dispatched in the current window.
    pub count: u32,
}

impl ResumeBudget {
    /// Returns whether another guarded resume may be dispatched at `now`.
    ///
    /// An elapsed (or never-opened) window resets the accounting, so the cap only binds
    /// within one rolling `window_ms`. `max == 0` disables resume entirely.
    #[must_use]
    pub fn allows(&self, now: DateTime<Utc>, window_ms: u64, max: u32) -> bool {
        if max == 0 {
            return false;
        }
        match self.window_start {
            Some(start)
                if now.signed_duration_since(start)
                    < chrono::Duration::milliseconds(window_ms as i64) =>
            {
                self.count < max
            }
            // Fresh or elapsed window: the next dispatch opens a new window.
            _ => true,
        }
    }

    /// Records one dispatched resume at `now`, resetting the window when it has elapsed.
    pub fn consume(&mut self, now: DateTime<Utc>, window_ms: u64) {
        match self.window_start {
            Some(start)
                if now.signed_duration_since(start)
                    < chrono::Duration::milliseconds(window_ms as i64) =>
            {
                self.count = self.count.saturating_add(1);
            }
            _ => {
                self.window_start = Some(now);
                self.count = 1;
            }
        }
    }
}

/// Dispatch-time context for an in-flight guarded coordinator resume turn.
///
/// Records which turn was dispatched for a resume and the snapshot of unread signal ids
/// folded into its instruction, so [`SessionVoState::clear_resume_on_outcome`] consumes
/// exactly that snapshot when the turn completes (signals that arrive mid-turn stay
/// queued for the next resume).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResumeTurnContext {
    /// Turn id dispatched for the guarded resume.
    pub turn_id: String,
    /// Unread signal ids consumed by this resume turn at dispatch time.
    pub consumed_signal_ids: Vec<AgentSignalId>,
}

/// Per-active-child liveness-watchdog scheduling state held on the Session VO.
///
/// One entry exists while a per-child `check_child_liveness` delayed self-call is
/// outstanding. `generation` is drawn from the session-wide monotonic
/// [`SessionVoState::child_liveness_generation`] counter so a tick scheduled by a
/// superseded arming is recognized as stale and ignored when it fires.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildLivenessState {
    /// Child worker this watchdog entry tracks.
    pub worker_id: WorkerId,
    /// Scheduling generation of the currently outstanding liveness check.
    pub generation: u64,
}

/// Exact aggregate tuple used for execution-progress delta gating.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionProgressSignature {
    /// Active immutable plan revision.
    pub plan_revision: u64,
    /// Exhaustively mapped durable run status.
    pub status: String,
    /// Materialized logical task count.
    pub total: u64,
    /// Successfully completed logical task count.
    pub completed: u64,
    /// Failed logical task count.
    pub failed: u64,
    /// Cancelled logical task count.
    pub cancelled: u64,
}

impl From<&moa_core::events::ExecutionProgress> for ExecutionProgressSignature {
    fn from(progress: &moa_core::events::ExecutionProgress) -> Self {
        Self {
            plan_revision: progress.plan_revision,
            status: progress.status.clone(),
            total: progress.total,
            completed: progress.completed,
            failed: progress.failed,
            cancelled: progress.cancelled,
        }
    }
}

/// Compact active execution state retained by the Session VO.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActiveExecutionRunState {
    /// Durable execution-run identifier.
    pub run_uid: uuid::Uuid,
    /// Exact persisted user message that originated the run.
    pub originating_user_sequence_num: u64,
    /// Last aggregate progress published by the Session VO.
    pub progress: Option<moa_core::events::ExecutionProgress>,
    /// Exact aggregate tuple corresponding to the last progress publication.
    pub last_progress_signature: Option<ExecutionProgressSignature>,
    /// Durable time of the last progress publication.
    pub last_progress_at: Option<DateTime<Utc>>,
}

/// Permanent semantic replay projection for one external execution-template admission.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionTemplateAdmissionReplayState {
    /// Stable operation identity reserved before any Session mutation.
    pub operation_uid: uuid::Uuid,
    /// Canonical fingerprint of the complete first admitted request.
    pub request_fingerprint: String,
    /// Exact persisted objective event sequence, when committed.
    pub originating_user_sequence_num: Option<u64>,
    /// Exact execution run created by Task 7, when committed.
    pub execution_run_uid: Option<uuid::Uuid>,
}

/// Next durable boundary for one semantically replayed external admission.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ExecutionTemplateAdmissionResume {
    /// No objective sequence has been committed yet.
    AppendObjective,
    /// The objective is committed and Task 7 must create or replay the run.
    StartExecution {
        /// Exact persisted objective event sequence.
        originating_user_sequence_num: u64,
    },
    /// The operation is complete and must return this exact first response.
    Complete(moa_execution::wire::ExecutionTemplateAdmissionResponse),
}

impl ExecutionTemplateAdmissionReplayState {
    /// Validates semantic replay and returns the first incomplete durable boundary.
    pub fn resume(
        &self,
        expected_request_fingerprint: &str,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> moa_core::error::Result<ExecutionTemplateAdmissionResume> {
        if self.request_fingerprint != expected_request_fingerprint {
            return Err(moa_core::error::MoaError::ValidationError(
                "execution-template admission idempotency key conflicts with the first request"
                    .to_string(),
            ));
        }
        match (self.originating_user_sequence_num, self.execution_run_uid) {
            (None, None) => Ok(ExecutionTemplateAdmissionResume::AppendObjective),
            (Some(originating_user_sequence_num), None) => {
                Ok(ExecutionTemplateAdmissionResume::StartExecution {
                    originating_user_sequence_num,
                })
            }
            (Some(originating_user_sequence_num), Some(execution_run_uid)) => {
                Ok(ExecutionTemplateAdmissionResume::Complete(
                    moa_execution::wire::ExecutionTemplateAdmissionResponse {
                        session_id,
                        originating_user_sequence_num,
                        execution_run_uid,
                    },
                ))
            }
            (None, Some(_)) => Err(moa_core::error::MoaError::StorageError(
                "execution-template admission recorded a run without its user origin".to_string(),
            )),
        }
    }
}

/// Compact marker proving one stable synthesis turn was durably dispatched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionSynthesisDedupe {
    /// Durable execution-run identifier.
    pub run_uid: uuid::Uuid,
    /// Exact persisted user message that originated the run.
    pub originating_user_sequence_num: u64,
    /// Stable keyed synthesis turn identifier.
    pub turn_id: String,
}

/// Serializable projection of the Session VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionVoState {
    /// Persisted session metadata mirror.
    pub meta: Option<SessionMeta>,
    /// Current lifecycle status held in Restate state.
    pub status: Option<SessionStatus>,
    /// Buffered user messages waiting for the next `TurnExecution` workflow.
    pub pending: Vec<UserMessage>,
    /// Placeholder for worker children introduced in R08.
    pub children: Vec<WorkerChildRef>,
    /// Human-readable stub summary of the last drained turn.
    pub last_turn_summary: Option<String>,
    /// Requested cancellation scope, recorded at the most recent cancel request.
    pub cancel_flag: Option<CancelScope>,
    /// Active task segment, when one has been created for the session.
    pub current_segment: Option<ActiveSegment>,
    /// Current progress-narration scheduling generation. Bumped on each active-edge
    /// (re)start so a delayed tick scheduled by a superseded generation is ignored.
    pub narration_tick_generation: u64,
    /// Whether a narration tick is scheduled and not yet stopped. Guarantees a single
    /// outstanding tick so `register_child`/turn-start edges cannot fan out overlapping ticks.
    pub narration_tick_outstanding: bool,
    /// Monotonic narration sequence used to build the `narration:{session}:{seq}` dedupe key.
    pub narration_seq: u64,
    /// Change cursor (semantic marker) of the most recently narrated active sources.
    pub last_narrated_marker: Option<String>,
    /// Journaled instant of the most recent narration dispatch, for the interval gate.
    pub last_narration_at: Option<DateTime<Utc>>,
    /// Rolling narration window start, for the per-window cost cap.
    pub narration_window_start: Option<DateTime<Utc>>,
    /// Narrations dispatched in the current rolling window.
    pub narration_window_count: u32,
    /// Owning participant identity captured for self-originated narration reads. Sourced
    /// from the first verified turn participant, falling back to session metadata.
    pub owning_identity: Option<Identity>,
    /// Recent unread child→parent control-plane signals, capped to a small window.
    ///
    /// Stores signal CONTENT (kind/summary/input request) so a Task-6 resume/drain turn
    /// can compile it into the coordinator prompt without re-reading the event log.
    /// Eviction prefers to keep action-required kinds (`NeedsInput`/`Blocked`).
    pub unread_child_signals: Vec<UnreadChildSignal>,
    /// Signal armed for a guarded coordinator auto-resume, when one is pending.
    ///
    /// Set by the resume-eligibility gate (decision only). The actual resume-turn
    /// dispatch and clearing on completion are wired in Task 6.
    pub pending_parent_resume_signal: Option<AgentSignalId>,
    /// Per-session guarded-resume budget, consumed on each guarded-resume dispatch.
    pub resume_budget: ResumeBudget,
    /// In-flight guarded coordinator resume turn and its dispatch-time unread snapshot,
    /// drained on `record_turn_outcome` when that turn completes.
    pub resume_turn: Option<ResumeTurnContext>,
    /// Session-wide monotonic counter minting one liveness-check generation per arming.
    ///
    /// Monotonic so a re-armed (or re-registered) child never reuses a prior
    /// generation, making any stray in-flight tick from before a clear/re-arm
    /// recognizable as stale.
    pub child_liveness_generation: u64,
    /// Per-child outstanding liveness-watchdog checks (single-outstanding per child).
    pub child_liveness: Vec<ChildLivenessState>,
    /// Claim-check references for terminal child outputs offloaded from `children`.
    ///
    /// One entry per terminal child whose output exceeded
    /// [`CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES`]; the inline `children` copy keeps only a
    /// preview and `consume_child_result` hydrates the full body from the blob.
    pub child_terminal_blobs: Vec<ChildTerminalOutputRef>,
    /// Compact origin and last-published progress for active detached runs.
    pub active_execution_runs: Vec<ActiveExecutionRunState>,
    /// Exact user-addressed execution and worker reply targets.
    pub pending_user_reply_targets: Vec<PendingUserReplyTarget>,
    /// Stable terminal synthesis dispatch markers retained for replay dedupe.
    pub execution_synthesis_dedupe: Vec<ExecutionSynthesisDedupe>,
}

impl SessionVoState {
    /// Initializes the projection from persisted session metadata.
    pub fn set_meta(&mut self, meta: SessionMeta) {
        self.status = Some(meta.status.clone());
        self.meta = Some(meta);
    }

    /// Returns the current lifecycle status, defaulting to `Created` when state is empty.
    pub fn current_status(&self) -> SessionStatus {
        self.status.clone().unwrap_or(SessionStatus::Created)
    }

    /// Loads only the lifecycle status key for hot read-only status polls, so the
    /// handler skips deserializing children, pending, and narration state.
    pub(super) async fn load_status<R: VoReader>(
        reader: &R,
    ) -> Result<SessionStatus, HandlerError> {
        Ok(reader
            .get_json(K_STATUS)
            .await?
            .unwrap_or(SessionStatus::Created))
    }

    /// Loads only the child-refs key for hot read-only child polls.
    pub(super) async fn load_children<R: VoReader>(
        reader: &R,
    ) -> Result<Vec<WorkerChildRef>, HandlerError> {
        Ok(reader.get_json(K_CHILDREN).await?.unwrap_or_default())
    }

    /// Loads minimal active execution-run markers for shared snapshot and progress reads.
    pub(super) async fn load_active_execution_runs<R: VoReader>(
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
        let cadence_due = run.last_progress_at.is_none_or(|last| {
            let elapsed_ms = now.signed_duration_since(last).num_milliseconds();
            elapsed_ms >= i64::try_from(progress_interval_ms).unwrap_or(i64::MAX)
        });
        if !(changed && cadence_due) {
            return Ok(false);
        }

        run.progress = Some(progress);
        run.last_progress_signature = Some(signature);
        run.last_progress_at = Some(now);
        Ok(true)
    }

    /// Inserts or updates one exact pending user reply target.
    pub fn upsert_pending_user_reply_target(&mut self, target: PendingUserReplyTarget) -> bool {
        if self
            .pending_user_reply_targets
            .iter()
            .any(|entry| entry == &target)
        {
            return false;
        }
        if let Some(existing) = self
            .pending_user_reply_targets
            .iter_mut()
            .find(|existing| pending_reply_identity_matches(existing, &target))
        {
            *existing = target;
            return true;
        }
        self.pending_user_reply_targets.push(target);
        true
    }

    /// Returns the only user-addressed target, or `None` when zero or ambiguous.
    #[must_use]
    pub fn exact_pending_user_reply_target(&self) -> Option<PendingUserReplyTarget> {
        match self.pending_user_reply_targets.as_slice() {
            [target] => Some(target.clone()),
            _ => None,
        }
    }

    /// Clears an exact pending target only after an applied or replayed delivery.
    pub fn clear_pending_user_reply_target(&mut self, target: &PendingUserReplyTarget) -> bool {
        let before = self.pending_user_reply_targets.len();
        self.pending_user_reply_targets
            .retain(|existing| existing != target);
        before != self.pending_user_reply_targets.len()
    }

    /// Applies a typed delivery acknowledgement to one exact pending reply target.
    pub fn apply_pending_user_reply_ack(
        &mut self,
        target: &PendingUserReplyTarget,
        acknowledgement: UserReplyDeliveryAck,
    ) -> bool {
        if matches!(
            acknowledgement,
            UserReplyDeliveryAck::Applied | UserReplyDeliveryAck::Replayed
        ) {
            return self.clear_pending_user_reply_target(target);
        }
        false
    }

    /// Clears the unread user-input signal paired with one successfully delivered worker reply.
    pub fn clear_unread_worker_input(
        &mut self,
        worker_id: &WorkerId,
        input_request_id: &str,
    ) -> bool {
        let before = self.unread_child_signals.len();
        self.unread_child_signals.retain(|signal| {
            !(signal.worker_id == *worker_id
                && signal.input_request_id.as_deref() == Some(input_request_id))
        });
        before != self.unread_child_signals.len()
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

    /// Ensures that session metadata has been initialized before mutations proceed.
    pub fn ensure_initialized(&self) -> MoaResult<&SessionMeta> {
        self.meta.as_ref().ok_or_else(|| {
            MoaError::ValidationError(
                "Session metadata missing. Initialize the VO via SessionStore/init_session_vo first."
                    .to_string(),
            )
        })
    }

    /// Queues one user message and transitions the session into `Running`.
    pub fn enqueue_message(&mut self, msg: UserMessage, now: DateTime<Utc>) -> MoaResult<()> {
        self.ensure_initialized()?;
        self.pending.push(msg);
        self.set_status(SessionStatus::Running, now);
        Ok(())
    }

    /// Applies a turn outcome to the lifecycle state.
    ///
    /// In the existing MOA status model, an idle turn parks the session in `Paused`.
    pub fn apply_turn_outcome(
        &mut self,
        outcome: TurnOutcome,
        now: DateTime<Utc>,
    ) -> SessionStatus {
        let next_status = match outcome {
            TurnOutcome::Continue => SessionStatus::Running,
            TurnOutcome::Idle => SessionStatus::Paused,
            TurnOutcome::Cancelled => SessionStatus::Cancelled,
        };
        self.set_status(next_status.clone(), now);
        next_status
    }

    /// Keeps the owning session active after a detached execution run is admitted.
    pub fn apply_accepted_execution_turn(&mut self, now: DateTime<Utc>) {
        self.last_turn_summary = Some("Execution accepted.".to_string());
        self.set_status(SessionStatus::Running, now);
    }

    /// Records the requested cancellation scope.
    pub fn set_cancel_flag(&mut self, scope: CancelScope) {
        self.cancel_flag = Some(scope);
    }

    /// Consumes the current cancellation scope, if any.
    pub fn take_cancel_flag(&mut self) -> Option<CancelScope> {
        self.cancel_flag.take()
    }

    /// Drains buffered user messages and records a short stub summary.
    pub fn drain_pending_messages(&mut self) -> usize {
        let drained = self.pending.len();
        self.pending.clear();
        self.last_turn_summary = if drained == 0 {
            None
        } else if drained == 1 {
            Some("drained 1 queued message".to_string())
        } else {
            Some(format!("drained {drained} queued messages"))
        };
        drained
    }

    /// Clears the in-memory projection back to an empty VO.
    pub fn destroy(&mut self) {
        *self = Self::default();
    }

    /// Replaces the active task segment.
    pub fn set_current_segment(&mut self, segment: ActiveSegment) {
        self.current_segment = Some(segment);
    }

    /// Records a tool usage on the active task segment.
    pub fn record_segment_tool_use(&mut self, tool_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.tools_used.iter().any(|tool| tool == tool_name) {
            segment.tools_used.push(tool_name.to_string());
        }
    }

    /// Records that the model engaged a skill on the active task segment.
    pub fn record_segment_skill_use(&mut self, skill_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.skills_used.iter().any(|skill| skill == skill_name) {
            segment.skills_used.push(skill_name.to_string());
        }
    }

    /// Records one completed model turn on the active task segment.
    pub fn record_segment_turn_usage(&mut self, token_cost: u64) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        segment.turn_count = segment.turn_count.saturating_add(1);
        segment.token_cost = segment.token_cost.saturating_add(token_cost);
    }

    /// Adds a root-owned child worker reference if it is not already registered.
    pub fn register_child(&mut self, child: WorkerChildRef) -> bool {
        if self.children.iter().any(|existing| existing.id == child.id) {
            return false;
        }
        self.children.push(child);
        true
    }

    /// Caches a terminal child result until the parent consumes it.
    pub fn mark_child_terminal(&mut self, input: MarkWorkerChildTerminalInput) -> bool {
        let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.id == input.worker_id)
        else {
            return false;
        };
        if child.terminal.is_some() {
            return false;
        }
        child.terminal = Some(input.terminal);
        true
    }

    /// Removes and returns a cached terminal child result.
    pub fn consume_child_terminal(&mut self, worker_id: &str) -> Option<WorkerTerminalResult> {
        let index = self
            .children
            .iter()
            .position(|child| child.id == worker_id && child.terminal.is_some())?;
        self.children.remove(index).terminal
    }

    /// Removes a root-owned child worker reference by id.
    pub fn remove_child(&mut self, worker_id: &str) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id != worker_id);
        // Drop any outstanding liveness watchdog for the now-removed child.
        self.clear_child_liveness(worker_id);
        // Drop any claim-check reference for the now-removed child's output; the blob is
        // reclaimed at session teardown.
        self.remove_child_terminal_blob(worker_id);
        self.children.len() != before
    }

    /// Returns the full output of a terminal child when it exceeds the claim-check
    /// threshold and is still stored inline, so the handler can offload it to a blob.
    #[must_use]
    pub fn large_child_terminal_output(&self, worker_id: &str) -> Option<String> {
        let child = self.children.iter().find(|child| child.id == worker_id)?;
        let terminal = child.terminal.as_ref()?;
        (terminal.result.output.len() >= CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES)
            .then(|| terminal.result.output.clone())
    }

    /// Replaces a terminal child's inline output with a preview after its full body was
    /// offloaded to `claim_check`, recording the reference for later hydration.
    pub fn compact_child_terminal_output(&mut self, worker_id: &str, claim_check: ClaimCheck) {
        {
            let Some(child) = self.children.iter_mut().find(|child| child.id == worker_id) else {
                return;
            };
            let Some(terminal) = child.terminal.as_mut() else {
                return;
            };
            let preview = child_output_preview(&terminal.result.output);
            terminal.result.output = preview;
        }
        // One reference per worker: replace any stale entry so a revived/re-marked child
        // cannot accumulate duplicates.
        self.child_terminal_blobs
            .retain(|reference| reference.worker_id != worker_id);
        self.child_terminal_blobs.push(ChildTerminalOutputRef {
            worker_id: worker_id.to_string(),
            claim_check,
        });
    }

    /// Removes and returns a terminal child's output claim-check reference, if any, so the
    /// consuming handler can hydrate the full body.
    pub fn take_child_terminal_blob(&mut self, worker_id: &str) -> Option<ClaimCheck> {
        let index = self
            .child_terminal_blobs
            .iter()
            .position(|reference| reference.worker_id == worker_id)?;
        Some(self.child_terminal_blobs.remove(index).claim_check)
    }

    /// Drops a terminal child's output claim-check reference without returning it.
    fn remove_child_terminal_blob(&mut self, worker_id: &str) {
        self.child_terminal_blobs
            .retain(|reference| reference.worker_id != worker_id);
    }

    /// Returns whether the session currently owns the child worker id.
    #[must_use]
    pub fn owns_child(&self, worker_id: &str) -> bool {
        self.children.iter().any(|child| child.id == worker_id)
    }

    /// Returns whether a child signal belongs to this session's worker tree.
    #[must_use]
    pub fn owns_signal_worker(&self, signal: &WorkerSignal) -> bool {
        self.owns_child(&signal.worker_id)
    }

    /// Pushes one unread child→parent control-plane signal onto the recent window.
    ///
    /// Deduplicates by `signal_id` (a retried delivery is a no-op) and caps the window
    /// to [`MAX_UNREAD_CHILD_SIGNALS`]. When evicting, an action-required signal
    /// (`NeedsInput`/`Blocked`) is preferentially kept over informational kinds: the
    /// oldest non-action-required entry is dropped first, falling back to the oldest
    /// entry only when every retained signal is action-required. Returns whether a new
    /// entry was inserted.
    pub fn push_unread_child_signal(&mut self, signal: UnreadChildSignal) -> bool {
        if self
            .unread_child_signals
            .iter()
            .any(|existing| existing.signal_id == signal.signal_id)
        {
            return false;
        }
        self.unread_child_signals.push(signal);
        while self.unread_child_signals.len() > MAX_UNREAD_CHILD_SIGNALS {
            let victim = self
                .unread_child_signals
                .iter()
                .position(|existing| !signal_kind_is_action_required(existing.kind))
                .unwrap_or(0);
            self.unread_child_signals.remove(victim);
        }
        true
    }

    /// Clears one unread child signal by id.
    pub fn clear_unread_child_signal(&mut self, signal_id: AgentSignalId) -> bool {
        let before = self.unread_child_signals.len();
        self.unread_child_signals
            .retain(|signal| signal.signal_id != signal_id);
        if self.pending_parent_resume_signal == Some(signal_id) {
            self.pending_parent_resume_signal = None;
        }
        self.unread_child_signals.len() != before
    }

    /// Drains all queued child signals when a coordinator turn is admitted.
    ///
    /// The durable event log still carries those signals into the turn's compiled history;
    /// this only clears the compact VO projection so answered/seen signals do not fill the
    /// bounded unread window after an active turn has had a chance to observe them.
    pub fn drain_unread_child_signals(&mut self) -> usize {
        let drained = self.unread_child_signals.len();
        self.unread_child_signals.clear();
        self.pending_parent_resume_signal = None;
        drained
    }

    /// Computes the guarded parent-resume decision for one recorded signal and arms it.
    ///
    /// Sets [`Self::pending_parent_resume_signal`] and returns `true` only when the
    /// signal opts into idle-wake (`resume_policy == IfIdle`), its kind is
    /// resume-eligible, the coordinator has no active root turn, and the rolling
    /// per-window resume budget (cap `max_per_window`, length `window_ms`) allows another
    /// resume at `now`. The budget is consumed separately, only on an actual dispatch
    /// ([`Self::record_resume_dispatch`]), so a retried delivery does not double-count.
    pub fn maybe_arm_parent_resume(
        &mut self,
        signal: &WorkerSignal,
        active_turn_id: Option<&str>,
        now: DateTime<Utc>,
        max_per_window: u32,
        window_ms: u64,
    ) -> bool {
        let eligible = matches!(signal.resume_policy, ParentResumePolicy::IfIdle)
            && signal_kind_is_resume_eligible(signal.kind)
            && active_turn_id.is_none()
            && self.resume_budget.allows(now, window_ms, max_per_window);
        if eligible {
            self.pending_parent_resume_signal = Some(signal.signal_id);
        }
        eligible
    }

    /// Returns whether the only reason this signal cannot arm a resume is budget.
    #[must_use]
    pub fn resume_budget_exhausted_for_signal(
        &self,
        signal: &WorkerSignal,
        active_turn_id: Option<&str>,
        now: DateTime<Utc>,
        max_per_window: u32,
        window_ms: u64,
    ) -> bool {
        matches!(signal.resume_policy, ParentResumePolicy::IfIdle)
            && signal_kind_is_resume_eligible(signal.kind)
            && active_turn_id.is_none()
            && !self.resume_budget.allows(now, window_ms, max_per_window)
    }

    /// Records a dispatched guarded-resume turn: consumes one unit of resume budget and
    /// snapshots the current unread signal ids consumed by the turn.
    ///
    /// The snapshot is exactly the set of unread signals folded into the resume turn's
    /// instruction; [`Self::clear_resume_on_outcome`] removes only this set on completion
    /// so signals that arrive mid-turn remain queued for the next resume.
    pub fn record_resume_dispatch(&mut self, turn_id: String, now: DateTime<Utc>, window_ms: u64) {
        self.resume_budget.consume(now, window_ms);
        self.resume_turn = Some(ResumeTurnContext {
            turn_id,
            consumed_signal_ids: self
                .unread_child_signals
                .iter()
                .map(|signal| signal.signal_id)
                .collect(),
        });
    }

    /// Clears resume bookkeeping when the completing turn was the guarded-resume turn.
    ///
    /// Drains exactly the dispatch-time unread snapshot (leaving mid-turn arrivals
    /// queued) and clears `pending_parent_resume_signal`. Returns whether the completing
    /// turn matched the in-flight resume turn.
    pub fn clear_resume_on_outcome(&mut self, completed_turn_id: &str) -> bool {
        let Some(resume_turn) = self.resume_turn.as_ref() else {
            return false;
        };
        if resume_turn.turn_id != completed_turn_id {
            return false;
        }
        let consumed = self.resume_turn.take().map(|turn| turn.consumed_signal_ids);
        if let Some(consumed) = consumed {
            self.unread_child_signals
                .retain(|signal| !consumed.contains(&signal.signal_id));
        }
        self.pending_parent_resume_signal = None;
        true
    }

    /// Arms a single-outstanding liveness check for one active child.
    ///
    /// Returns the new monotonic generation to schedule with when a check is newly
    /// armed, or `None` when one is already outstanding for the child (so overlapping
    /// active edges cannot fan out multiple checks). The generation is drawn from the
    /// session-wide monotonic counter so it never collides with a superseded arming.
    pub fn arm_child_liveness(&mut self, worker_id: &str) -> Option<u64> {
        if self
            .child_liveness
            .iter()
            .any(|entry| entry.worker_id == worker_id)
        {
            return None;
        }
        self.child_liveness_generation = self.child_liveness_generation.wrapping_add(1);
        let generation = self.child_liveness_generation;
        self.child_liveness.push(ChildLivenessState {
            worker_id: worker_id.to_string(),
            generation,
        });
        Some(generation)
    }

    /// Returns whether a fired liveness check still owns scheduling for its child.
    ///
    /// A check is live only when an entry for the child is outstanding and its
    /// generation matches; a superseded or cleared check no-ops.
    #[must_use]
    pub fn liveness_generation_matches(&self, worker_id: &str, generation: u64) -> bool {
        self.child_liveness
            .iter()
            .any(|entry| entry.worker_id == worker_id && entry.generation == generation)
    }

    /// Clears the outstanding liveness check for one child (terminal/stale/removed).
    ///
    /// Removing the entry is safe because re-arming draws a fresh generation from the
    /// monotonic counter, so any stray in-flight tick can never match the re-armed child.
    pub fn clear_child_liveness(&mut self, worker_id: &str) {
        self.child_liveness
            .retain(|entry| entry.worker_id != worker_id);
    }

    pub(super) fn set_status(&mut self, status: SessionStatus, now: DateTime<Utc>) {
        self.status = Some(status.clone());
        if let Some(meta) = self.meta.as_mut() {
            meta.status = status.clone();
            meta.updated_at = now;
            if matches!(
                status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            ) && meta.completed_at.is_none()
            {
                meta.completed_at = Some(now);
            }
        }
    }
}

fn pending_reply_identity_matches(
    left: &PendingUserReplyTarget,
    right: &PendingUserReplyTarget,
) -> bool {
    match (left, right) {
        (
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid: left_run_uid,
                ..
            },
            PendingUserReplyTarget::ExecutionConfirmation {
                run_uid: right_run_uid,
                ..
            },
        ) => left_run_uid == right_run_uid,
        (
            PendingUserReplyTarget::ExecutionInput {
                run_uid: left_run_uid,
                task_id: left_task_id,
                ..
            },
            PendingUserReplyTarget::ExecutionInput {
                run_uid: right_run_uid,
                task_id: right_task_id,
                ..
            },
        ) => left_run_uid == right_run_uid && left_task_id == right_task_id,
        (
            PendingUserReplyTarget::WorkerInput {
                worker_id: left_worker_id,
                input_request_id: left_request_id,
            },
            PendingUserReplyTarget::WorkerInput {
                worker_id: right_worker_id,
                input_request_id: right_request_id,
            },
        ) => left_worker_id == right_worker_id && left_request_id == right_request_id,
        _ => false,
    }
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

/// Whether a signal kind must be preserved over informational kinds during unread-cap
/// eviction. Action-required kinds block the child until the coordinator responds.
#[must_use]
fn signal_kind_is_action_required(kind: ChildSignalKind) -> bool {
    matches!(kind, ChildSignalKind::NeedsInput | ChildSignalKind::Blocked)
}

/// Whether a signal kind is eligible to wake an idle coordinator (resume-eligible).
///
/// Conservative by design: only blocking/attention-or-failure kinds qualify; plain
/// `Finding`s never trigger a resume.
#[must_use]
/// Truncated, human-readable preview retained inline for a claim-checked child output.
fn child_output_preview(output: &str) -> String {
    output.chars().take(CHILD_OUTPUT_PREVIEW_CHARS).collect()
}

pub(super) fn signal_kind_is_resume_eligible(kind: ChildSignalKind) -> bool {
    matches!(
        kind,
        ChildSignalKind::Blocked
            | ChildSignalKind::NeedsInput
            | ChildSignalKind::Failed
            | ChildSignalKind::HeartbeatStale
    )
}

impl VoState for SessionVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            meta: reader.get_json(K_META).await?,
            status: reader.get_json(K_STATUS).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            cancel_flag: reader.get_json(K_CANCEL_FLAG).await?,
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
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_META, self.meta.as_ref());
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_opt(ctx, K_CANCEL_FLAG, self.cancel_flag.as_ref());
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
    }

    fn persist_changes(&self, ctx: &ObjectContext<'_>, baseline: &Self) {
        set_changed_opt(ctx, K_META, self.meta.as_ref(), baseline.meta.as_ref());
        set_changed_opt(
            ctx,
            K_STATUS,
            self.status.as_ref(),
            baseline.status.as_ref(),
        );
        set_changed_vec(ctx, K_PENDING, &self.pending, &baseline.pending);
        set_changed_vec(ctx, K_CHILDREN, &self.children, &baseline.children);
        set_changed_opt(
            ctx,
            K_LAST_TURN_SUMMARY,
            self.last_turn_summary.as_ref(),
            baseline.last_turn_summary.as_ref(),
        );
        set_changed_opt(
            ctx,
            K_CANCEL_FLAG,
            self.cancel_flag.as_ref(),
            baseline.cancel_flag.as_ref(),
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
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        types::channel::Attachment, types::channel::Channel, types::identifiers::ModelId,
    };

    use super::{CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES, SessionVoState};
    use moa_core::{
        types::events_stream::ClaimCheck, types::session::TurnOutcome,
        types::worker::commands::MarkWorkerChildTerminalInput,
    };

    fn test_message(text: &str) -> moa_core::types::session::UserMessage {
        moa_core::types::session::UserMessage {
            text: text.to_string(),
            attachments: vec![Attachment {
                id: None,
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                sha256: None,
                url: None,
                path: None,
                size_bytes: Some(3),
            }],
        }
    }

    fn test_meta() -> moa_core::types::session::SessionMeta {
        moa_core::types::session::SessionMeta {
            tenant_id: moa_core::types::identifiers::TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("test-model"),
            ..moa_core::types::session::SessionMeta::default()
        }
    }

    fn worker_terminal(
        worker_id: &str,
        output: &str,
    ) -> moa_core::types::worker::state::WorkerTerminalResult {
        moa_core::types::worker::state::WorkerTerminalResult {
            state: moa_core::types::worker::state::WorkerState::Completed,
            result: moa_core::types::worker::state::WorkerResult {
                worker_id: worker_id.to_string(),
                success: true,
                output: output.to_string(),
                tokens_used: 17,
                tools_invoked: 1,
                error: None,
            },
        }
    }

    fn pending_child(id: &str) -> moa_core::types::worker::state::WorkerChildRef {
        moa_core::types::worker::state::WorkerChildRef {
            id: id.to_string(),
            task_hash: format!("hash-{id}"),
            budget_tokens: 128,
            terminal: None,
        }
    }

    #[test]
    fn session_vo_requires_meta_before_enqueue() {
        let mut state = SessionVoState::default();
        let error = state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect_err("enqueue should fail without metadata");

        assert!(error.to_string().contains("Session metadata missing"));
    }

    #[test]
    fn session_vo_queues_messages_and_transitions_to_running() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect("enqueue should succeed");

        assert_eq!(state.pending.len(), 1);
        assert_eq!(
            state.current_status(),
            moa_core::types::session::SessionStatus::Running
        );
    }

    #[test]
    fn session_vo_idle_turn_maps_to_paused_status() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        let status = state.apply_turn_outcome(TurnOutcome::Idle, Utc::now());

        assert_eq!(status, moa_core::types::session::SessionStatus::Paused);
        assert_eq!(
            state.current_status(),
            moa_core::types::session::SessionStatus::Paused
        );
    }

    #[test]
    fn session_vo_cancel_flag_round_trips() {
        let mut state = SessionVoState::default();
        state.set_cancel_flag(moa_core::types::session::CancelScope::CoordinatorOnly);

        assert_eq!(
            state.take_cancel_flag(),
            Some(moa_core::types::session::CancelScope::CoordinatorOnly)
        );
        assert_eq!(state.take_cancel_flag(), None);
    }

    #[test]
    fn session_vo_destroy_clears_projection() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"), Utc::now())
            .expect("enqueue should succeed");
        state
            .children
            .push(moa_core::types::worker::state::WorkerChildRef {
                id: "child-1".to_string(),
                task_hash: "hash-1".to_string(),
                budget_tokens: 0,
                terminal: None,
            });
        state.last_turn_summary = Some("summary".to_string());
        state.set_cancel_flag(moa_core::types::session::CancelScope::TaskTree);
        state.destroy();

        assert_eq!(state, SessionVoState::default());
    }

    #[test]
    fn session_child_registry_is_idempotent_by_child_id() {
        // Pins: root delegation registration preserves one active child ref per id.
        let mut state = SessionVoState::default();
        let child = moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        };

        assert!(state.register_child(child.clone()));
        assert!(!state.register_child(child));
        assert_eq!(state.children.len(), 1);
        assert!(state.owns_child("child-1"));
    }

    #[test]
    fn session_child_registry_remove_is_exact() {
        // Pins: root delegation cleanup removes only the requested active child ref.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        state.register_child(moa_core::types::worker::state::WorkerChildRef {
            id: "child-2".to_string(),
            task_hash: "hash-2".to_string(),
            budget_tokens: 256,
            terminal: None,
        });

        assert!(state.remove_child("child-1"));
        assert!(!state.remove_child("missing"));
        assert_eq!(
            state.children,
            vec![moa_core::types::worker::state::WorkerChildRef {
                id: "child-2".to_string(),
                task_hash: "hash-2".to_string(),
                budget_tokens: 256,
                terminal: None,
            }]
        );
    }

    #[test]
    fn session_child_terminal_result_is_consumed_once() {
        // Pins: root wait consumes a cached terminal child result exactly once.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let terminal = moa_core::types::worker::state::WorkerTerminalResult {
            state: moa_core::types::worker::state::WorkerState::Completed,
            result: moa_core::types::worker::state::WorkerResult {
                worker_id: "child-1".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 17,
                tools_invoked: 2,
                error: None,
            },
        };

        assert!(state.mark_child_terminal(
            moa_core::types::worker::commands::MarkWorkerChildTerminalInput {
                worker_id: "child-1".to_string(),
                terminal: terminal.clone(),
            }
        ));
        assert!(!state.mark_child_terminal(
            moa_core::types::worker::commands::MarkWorkerChildTerminalInput {
                worker_id: "child-1".to_string(),
                terminal: terminal.clone(),
            }
        ));
        assert_eq!(state.consume_child_terminal("child-1"), Some(terminal));
        assert_eq!(state.consume_child_terminal("child-1"), None);
        assert!(!state.owns_child("child-1"));
    }

    #[test]
    fn session_owns_only_registered_child_signals() {
        // Pins: workers are root-session-owned only; signal acceptance is the root
        // session child registry, not a nested worker tree.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::types::worker::state::WorkerChildRef {
            id: "child".to_string(),
            task_hash: "hash".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let root_signal = resume_signal(
            moa_core::types::worker::state::ChildSignalKind::Blocked,
            moa_core::types::worker::state::ParentResumePolicy::IfIdle,
        );
        let mut missing_signal = root_signal.clone();
        missing_signal.worker_id = "missing".to_string();

        assert!(state.owns_signal_worker(&root_signal));
        assert!(!state.owns_signal_worker(&missing_signal));
    }

    fn unread_entry(
        signal_id: moa_core::types::identifiers::AgentSignalId,
        kind: moa_core::types::worker::state::ChildSignalKind,
    ) -> moa_core::types::worker::state::UnreadChildSignal {
        moa_core::types::worker::state::UnreadChildSignal {
            signal_id,
            worker_id: "child".to_string(),
            kind,
            summary: "summary".to_string(),
            input_request_id: None,
            input_audience: None,
        }
    }

    fn resume_signal(
        kind: moa_core::types::worker::state::ChildSignalKind,
        resume_policy: moa_core::types::worker::state::ParentResumePolicy,
    ) -> moa_core::types::worker::state::WorkerSignal {
        moa_core::types::worker::state::WorkerSignal {
            signal_id: moa_core::types::identifiers::AgentSignalId::new(),
            worker_id: "child".to_string(),
            parent_session: moa_core::types::identifiers::SessionId::new(),
            kind,
            severity: moa_core::types::worker::state::SignalSeverity::Warning,
            summary: "needs attention".to_string(),
            payload: serde_json::Value::Null,
            created_at: Utc::now(),
            resume_policy,
            input_request_id: None,
            input_audience: None,
        }
    }

    #[test]
    fn unread_child_signal_push_is_idempotent_by_signal_id() {
        // Pins: a retried child-signal delivery records exactly one unread entry.
        let mut state = SessionVoState::default();
        let signal_id = moa_core::types::identifiers::AgentSignalId::new();
        let entry = unread_entry(
            signal_id,
            moa_core::types::worker::state::ChildSignalKind::Finding,
        );

        assert!(state.push_unread_child_signal(entry.clone()));
        assert!(!state.push_unread_child_signal(entry));
        assert_eq!(state.unread_child_signals.len(), 1);
    }

    #[test]
    fn unread_child_signal_cap_evicts_findings_before_action_required() {
        // Pins: when the unread window overflows, NeedsInput/Blocked are preserved while
        // informational Findings are evicted first.
        use moa_core::types::worker::state::ChildSignalKind;
        let mut state = SessionVoState::default();

        let blocked_id = moa_core::types::identifiers::AgentSignalId::new();
        assert!(state.push_unread_child_signal(unread_entry(blocked_id, ChildSignalKind::Blocked)));
        let needs_input_id = moa_core::types::identifiers::AgentSignalId::new();
        assert!(
            state.push_unread_child_signal(unread_entry(
                needs_input_id,
                ChildSignalKind::NeedsInput,
            ))
        );
        for _ in 0..super::MAX_UNREAD_CHILD_SIGNALS + 5 {
            state.push_unread_child_signal(unread_entry(
                moa_core::types::identifiers::AgentSignalId::new(),
                ChildSignalKind::Finding,
            ));
        }

        assert_eq!(
            state.unread_child_signals.len(),
            super::MAX_UNREAD_CHILD_SIGNALS
        );
        assert!(
            state
                .unread_child_signals
                .iter()
                .any(|signal| signal.signal_id == blocked_id),
            "Blocked signal must be preserved over evicted Findings"
        );
        assert!(
            state
                .unread_child_signals
                .iter()
                .any(|signal| signal.signal_id == needs_input_id),
            "NeedsInput signal must be preserved over evicted Findings"
        );
    }

    const TEST_RESUME_MAX: u32 = 6;
    const TEST_RESUME_WINDOW_MS: u64 = 600_000;

    #[test]
    fn resume_gate_arms_only_when_idle_eligible_and_under_budget() {
        // Pins: the resume-eligibility gate arms a pending resume only for an idle
        // coordinator on a resume-eligible IfIdle signal under budget, and never
        // dispatches a turn (it only mutates VO state).
        use moa_core::{
            types::worker::state::ChildSignalKind, types::worker::state::ParentResumePolicy,
        };
        let now = Utc::now();

        let mut idle = SessionVoState::default();
        let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);
        assert!(idle.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(idle.pending_parent_resume_signal, Some(signal.signal_id));

        let mut busy = SessionVoState::default();
        assert!(!busy.maybe_arm_parent_resume(
            &signal,
            Some("turn-1"),
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(busy.pending_parent_resume_signal, None);

        let mut finding = SessionVoState::default();
        let finding_signal = resume_signal(ChildSignalKind::Finding, ParentResumePolicy::IfIdle);
        assert!(!finding.maybe_arm_parent_resume(
            &finding_signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(finding.pending_parent_resume_signal, None);

        let mut never = SessionVoState::default();
        let never_signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::Never);
        assert!(!never.maybe_arm_parent_resume(
            &never_signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(never.pending_parent_resume_signal, None);

        let mut exhausted = SessionVoState::default();
        exhausted.resume_budget.window_start = Some(now);
        exhausted.resume_budget.count = TEST_RESUME_MAX;
        assert!(!exhausted.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(exhausted.pending_parent_resume_signal, None);
    }

    #[test]
    fn resume_gate_does_not_rearm_once_a_resume_turn_is_active() {
        // Pins: after a resume is dispatched (turn active), a repeated delivery of the
        // same signal does not arm a second resume — the active-turn gate blocks it.
        use moa_core::{
            types::worker::state::ChildSignalKind, types::worker::state::ParentResumePolicy,
        };
        let now = Utc::now();
        let signal = resume_signal(ChildSignalKind::Blocked, ParentResumePolicy::IfIdle);

        let mut state = SessionVoState::default();
        assert!(state.maybe_arm_parent_resume(
            &signal,
            None,
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);

        // The dispatched resume turn is now active; a retried signal cannot re-arm.
        assert!(!state.maybe_arm_parent_resume(
            &signal,
            Some("resume-turn"),
            now,
            TEST_RESUME_MAX,
            TEST_RESUME_WINDOW_MS
        ));
        assert_eq!(state.pending_parent_resume_signal, Some(signal.signal_id));
        assert_eq!(state.resume_budget.count, 1);
    }

    #[test]
    fn resume_budget_window_resets_after_elapsed_window() {
        // Pins: the rolling resume budget caps within a window but reopens once the
        // window elapses, and a zero cap disables resume entirely.
        let base = Utc::now();
        let mut budget = super::ResumeBudget::default();
        for _ in 0..TEST_RESUME_MAX {
            assert!(budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
            budget.consume(base, TEST_RESUME_WINDOW_MS);
        }
        // Cap reached inside the window.
        assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
        // After the window elapses the cap reopens.
        let later = base + chrono::Duration::milliseconds(TEST_RESUME_WINDOW_MS as i64 + 1);
        assert!(budget.allows(later, TEST_RESUME_WINDOW_MS, TEST_RESUME_MAX));
        // A zero cap disables resume regardless of window state.
        assert!(!budget.allows(base, TEST_RESUME_WINDOW_MS, 0));
    }

    #[test]
    fn child_liveness_is_single_outstanding_with_monotonic_generations() {
        // Pins: arming a child's liveness check is single-outstanding (a second arm while
        // one is outstanding is a no-op), generations are monotonic so a re-armed child
        // never reuses a prior generation, and a fired check only matches the live
        // generation of an outstanding entry.
        let mut state = SessionVoState::default();

        let first = state
            .arm_child_liveness("child-1")
            .expect("first arm schedules a check");
        // Single-outstanding: a second arm while one is outstanding does not reschedule.
        assert_eq!(state.arm_child_liveness("child-1"), None);
        // The live generation matches; a superseded/older generation does not.
        assert!(state.liveness_generation_matches("child-1", first));
        assert!(!state.liveness_generation_matches("child-1", first.wrapping_sub(1)));
        assert!(!state.liveness_generation_matches("missing", first));

        // A distinct active child gets its own, strictly newer generation.
        let other = state
            .arm_child_liveness("child-2")
            .expect("second child arms independently");
        assert_ne!(first, other);

        // Clearing (terminal/stale/removed) stops scheduling; a stray tick no longer matches.
        state.clear_child_liveness("child-1");
        assert!(!state.liveness_generation_matches("child-1", first));

        // Re-arming after a clear draws a fresh, strictly newer generation, so any stray
        // in-flight tick carrying `first` can never match the re-armed child.
        let rearmed = state
            .arm_child_liveness("child-1")
            .expect("re-arm after clear schedules a new check");
        assert_ne!(first, rearmed);
        assert!(rearmed > other);
        assert!(!state.liveness_generation_matches("child-1", first));
        assert!(state.liveness_generation_matches("child-1", rearmed));
    }

    #[test]
    fn remove_child_clears_outstanding_liveness_check() {
        // Pins: removing a child (e.g. on self-clean) drops its outstanding liveness
        // watchdog so a later fired check recognizes it as superseded.
        let mut state = SessionVoState::default();
        state.register_child(moa_core::types::worker::state::WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
            budget_tokens: 128,
            terminal: None,
        });
        let generation = state
            .arm_child_liveness("child-1")
            .expect("active child arms a liveness check");
        assert!(state.liveness_generation_matches("child-1", generation));

        assert!(state.remove_child("child-1"));
        assert!(!state.liveness_generation_matches("child-1", generation));
    }

    #[test]
    fn clear_resume_on_outcome_drains_only_dispatch_snapshot() {
        // Pins: completing the resume turn drains exactly the dispatch-time unread
        // snapshot and clears the pending signal, leaving mid-turn arrivals queued.
        use moa_core::types::worker::state::ChildSignalKind;
        let now = Utc::now();
        let mut state = SessionVoState::default();
        let snap_a = moa_core::types::identifiers::AgentSignalId::new();
        let snap_b = moa_core::types::identifiers::AgentSignalId::new();
        state.push_unread_child_signal(unread_entry(snap_a, ChildSignalKind::Blocked));
        state.push_unread_child_signal(unread_entry(snap_b, ChildSignalKind::NeedsInput));
        state.pending_parent_resume_signal = Some(snap_a);

        state.record_resume_dispatch("resume-turn".to_string(), now, TEST_RESUME_WINDOW_MS);
        assert_eq!(state.resume_budget.count, 1);

        // A signal that arrives mid-turn must NOT be drained on outcome.
        let mid_turn = moa_core::types::identifiers::AgentSignalId::new();
        state.push_unread_child_signal(unread_entry(mid_turn, ChildSignalKind::Finding));

        // A non-matching turn id is a no-op.
        assert!(!state.clear_resume_on_outcome("other-turn"));
        assert!(state.resume_turn.is_some());

        assert!(state.clear_resume_on_outcome("resume-turn"));
        assert_eq!(state.pending_parent_resume_signal, None);
        assert!(state.resume_turn.is_none());
        let remaining: Vec<_> = state
            .unread_child_signals
            .iter()
            .map(|signal| signal.signal_id)
            .collect();
        assert_eq!(remaining, vec![mid_turn]);
    }

    #[test]
    fn child_terminal_output_offload_round_trip() {
        // Pins: a terminal child whose output exceeds the threshold is reported for offload,
        // compacted to a preview in place, and its claim-check reference is retrievable exactly
        // once for hydration; a small output stays inline with no reference.
        let mut state = SessionVoState::default();
        state.register_child(pending_child("worker-1"));
        let big = "y".repeat(CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES + 10);
        assert!(state.mark_child_terminal(MarkWorkerChildTerminalInput {
            worker_id: "worker-1".to_string(),
            terminal: worker_terminal("worker-1", &big),
        }));
        // Over-threshold output is surfaced verbatim for the handler to store to a blob.
        assert_eq!(
            state.large_child_terminal_output("worker-1"),
            Some(big.clone())
        );

        let claim = ClaimCheck {
            blob_id: "blob-1".to_string(),
            size: big.len(),
            preview: "unused".to_string(),
        };
        state.compact_child_terminal_output("worker-1", claim.clone());
        // The inline copy is now a preview, so it no longer flags as large.
        assert_eq!(state.large_child_terminal_output("worker-1"), None);
        // The reference hydrates exactly once.
        assert_eq!(state.take_child_terminal_blob("worker-1"), Some(claim));
        assert_eq!(state.take_child_terminal_blob("worker-1"), None);

        // A small output is never offloaded.
        let mut small = SessionVoState::default();
        small.register_child(pending_child("worker-2"));
        small.mark_child_terminal(MarkWorkerChildTerminalInput {
            worker_id: "worker-2".to_string(),
            terminal: worker_terminal("worker-2", "short output"),
        });
        assert_eq!(small.large_child_terminal_output("worker-2"), None);
        assert_eq!(small.take_child_terminal_blob("worker-2"), None);
    }

    #[test]
    fn remove_child_drops_claim_check_reference() {
        // Pins: removing a child (worker self-cleanup) also drops its output claim-check
        // reference so evicted children never leak references in VO state.
        let mut state = SessionVoState::default();
        state.register_child(pending_child("worker-1"));
        let big = "q".repeat(CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES + 1);
        state.mark_child_terminal(MarkWorkerChildTerminalInput {
            worker_id: "worker-1".to_string(),
            terminal: worker_terminal("worker-1", &big),
        });
        state.compact_child_terminal_output(
            "worker-1",
            ClaimCheck {
                blob_id: "b".to_string(),
                size: big.len(),
                preview: "p".to_string(),
            },
        );

        assert!(state.remove_child("worker-1"));
        assert_eq!(state.take_child_terminal_blob("worker-1"), None);
    }
}

//! Durable Session VO state projection.

use super::*;
use moa_core::traits::Identity;
use moa_core::{
    types::identifiers::AgentSignalId, types::security::SecurityCircuitState,
    types::worker::signals::ChildSignalKind, types::worker::signals::ParentResumePolicy,
    types::worker::signals::UnreadChildSignal, types::worker::signals::WorkerSignal,
    types::worker::state::WorkerInputTarget,
};

mod execution;
mod inputs;
mod lifecycle;
mod persistence;
pub(super) mod resume;
mod segments;
mod workers;

/// One in-flight coordinator input request and the awakeable its turn is parked on.
///
/// `waiting_turn_id` and `generation` are the ownership fence: a reply may only
/// resolve the awakeable of the exact turn generation that raised the request, so
/// a superseded turn's stale awakeable can never be resolved by a later reply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorPendingInput {
    /// Coordinator turn that raised the request.
    pub turn_id: String,
    /// Session turn generation that admitted the owning turn.
    pub generation: u64,
    /// Exact request identifier.
    pub input_request_id: String,
    /// Restate awakeable the blocked coordinator turn is parked on.
    pub awakeable_id: String,
    /// Workflow invocation that is waiting on `awakeable_id`.
    ///
    /// Recorded so cancellation and timeout can clear the *exact*
    /// target rather than every request that happens to share a turn id. Two
    /// invocations of one logical turn (an original and its retry) can both hold
    /// registrations; clearing by turn alone would drop the live one.
    pub waiting_workflow_id: String,
}

/// Cycle-safe core pending target instantiated with the artifact-owned execution budget.
pub type PendingUserReplyTarget =
    moa_wire::turn::PendingUserReplyTarget<moa_artifacts::execution_plan::ExecutionBudgetLimit>;

pub(super) const K_META: &str = "meta";
pub(super) const K_STATUS: &str = "status";
pub(super) const K_CHILDREN: &str = "children";
pub(super) const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
pub(super) const K_CURRENT_SEGMENT: &str = "current_segment";
pub(super) const K_OWNING_IDENTITY: &str = "owning_identity";
pub(super) const K_UNREAD_CHILD_SIGNALS: &str = "unread_child_signals";
pub(super) const K_PENDING_PARENT_RESUME_SIGNAL: &str = "pending_parent_resume_signal";
pub(super) const K_RESUME_BUDGET: &str = "resume_budget";
pub(super) const K_RESUME_TURN: &str = "resume_turn";
pub(super) const K_CHILD_TERMINAL_BLOBS: &str = "child_terminal_blobs";
pub(super) const K_ACTIVE_EXECUTION_RUNS: &str = "active_execution_runs";
pub(super) const K_PENDING_USER_REPLY_TARGETS: &str = "pending_user_reply_targets";
pub(super) const K_EXECUTION_SYNTHESIS_DEDUPE: &str = "execution_synthesis_dedupe";
pub(super) const K_SECURITY_CIRCUIT: &str = "security_circuit";
pub(super) const K_PENDING_COORDINATOR_INPUTS: &str = "pending_coordinator_inputs";
pub(super) const K_COORDINATOR_INPUT_HISTORY: &str = "coordinator_input_history";

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

/// Exact public projection signature used for execution-progress delta gating.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionProgressSignature {
    /// Active immutable plan revision.
    pub plan_revision: u64,
    /// Exhaustively mapped durable run status.
    pub status: String,
    /// Public active, parked, or pause-state distinction.
    pub phase: moa_core::events::ExecutionProgressPhase,
    /// Time at which the current storage-only wait began.
    pub waiting_since: Option<DateTime<Utc>>,
    /// Earliest durable controller reactivation time.
    pub next_wake_at: Option<DateTime<Utc>>,
    /// Latest durable scheduler progress time.
    pub last_progress_at: DateTime<Utc>,
    /// Exact provider job when this projection is task-qualified.
    pub external_job_uid: Option<uuid::Uuid>,
    /// Exact ready logical task count.
    pub ready_tasks: u64,
    /// Exact active task-attempt count.
    pub active_tasks: u64,
    /// Exact durably parked logical task count.
    pub parked_tasks: u64,
    /// Highest-priority audience expected to resolve the current blocker.
    pub blocker_audience: Option<moa_core::events::ExecutionBlockerAudience>,
    /// Exact unconsumed and unreserved execution budget.
    pub remaining_budget: moa_core::events::ExecutionRemainingBudget,
    /// Cumulative spend against the goal-requirement denominator.
    ///
    /// Reconciling a task's actuals can raise consumed spend while every other signature
    /// field holds, which is exactly the run that is spending without advancing. Gating on
    /// the projection without this field would suppress that publication.
    #[serde(default)]
    pub economics: Option<moa_core::events::ExecutionProgressEconomics>,
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
            phase: progress.phase,
            waiting_since: progress.waiting_since,
            next_wake_at: progress.next_wake_at,
            last_progress_at: progress.last_progress_at,
            external_job_uid: progress.external_job_uid,
            ready_tasks: progress.ready_tasks,
            active_tasks: progress.active_tasks,
            parked_tasks: progress.parked_tasks,
            blocker_audience: progress.blocker_audience,
            remaining_budget: progress.remaining_budget.clone(),
            economics: progress.economics.clone(),
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
    /// Exact public projection signature corresponding to the last progress publication.
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
    /// Placeholder for worker children introduced in R08.
    pub children: Vec<WorkerChildRef>,
    /// Human-readable stub summary of the last drained turn.
    pub last_turn_summary: Option<String>,
    /// Active task segment, when one has been created for the session.
    pub current_segment: Option<ActiveSegment>,
    /// Owning participant identity captured from the first verified turn.
    /// Used by authenticated resume, review, and execution flows.
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
    /// In-flight coordinator input requests, one awakeable per request id.
    pub pending_coordinator_inputs: Vec<CoordinatorPendingInput>,
    /// Request ids whose reply was already delivered.
    ///
    /// Kept after the awakeable is taken so a late duplicate reply is recognized
    /// as a replay instead of resolving a *replacement* awakeable that a newer
    /// request happens to have registered under the same id.
    pub coordinator_input_history: Vec<String>,
    /// Prompt-injection circuit for the session's current coordinator generation.
    ///
    /// Deliberately separate from the admission allocator `turn_generation`:
    /// queuing later work advances that counter while the current turn is still
    /// live, so using it as the owner fence would reset a tripped circuit
    /// mid-attack. The owner stored inside the circuit is the fence.
    pub security_circuit: SecurityCircuitState,
}

#[cfg(test)]
mod tests;

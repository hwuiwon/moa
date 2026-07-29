//! Session-store service wire DTOs.

use chrono::{DateTime, Utc};
use moa_core::events::Event;
use moa_core::types::agent::{AgentContext, AgentSessionSelection};
use moa_core::{
    types::events_stream::EventRange,
    types::experience::{
        ExperienceAttribution, ExperienceRecord, LearningCandidate, LearningCandidateStatus,
    },
    types::identifiers::{SegmentId, SessionId, TenantId},
    types::segment_assessment::SegmentAssessment,
    types::segments::{SegmentCompletion, TaskSegment},
    types::session::{SessionFilter, SessionMeta},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Request payload for `SessionStore/append_event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendEventRequest {
    /// Session receiving the event.
    pub session_id: SessionId,
    /// Event payload to append to the durable log.
    pub event: Event,
    /// Optional idempotency key. When set, a retried append with the same
    /// `(session_id, dedupe_key)` returns the first persisted sequence number
    /// without inserting a second event (see `session_event_dedupe`); when unset,
    /// every append inserts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

/// Request payload for `SessionStore/get_events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetEventsRequest {
    /// Session whose event log should be read.
    pub session_id: SessionId,
    /// Range and filter options for the event query.
    pub range: EventRange,
}

/// Request payload for `SessionStore/create_agent_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAgentSessionRequest {
    /// Base session metadata to persist after agent resolution.
    pub meta: SessionMeta,
    /// Installed deployment or exact revision to pin onto the session.
    pub agent: AgentSessionSelection,
}

/// Response payload returned by `SessionStore/create_agent_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAgentSessionResponse {
    /// Created durable session identifier.
    pub session_id: SessionId,
    /// Exact agent context pinned to the session.
    pub agent_context: AgentContext,
}

/// Request payload for `SessionStore/update_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatusRequest {
    /// Session whose lifecycle state should be updated.
    pub session_id: SessionId,
    /// New session lifecycle state.
    pub status: moa_core::types::session::SessionStatus,
}

/// Request payload for `SessionStore/search_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEventsRequest {
    /// Full-text search query.
    pub query: String,
    /// Additional event-search scoping and limits.
    pub filter: moa_core::types::events_stream::EventFilter,
}

/// Request payload for `SessionStore/init_session_vo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitSessionVoRequest {
    /// Session object key that should be initialized.
    pub session_id: SessionId,
    /// Session metadata mirrored into Restate object state.
    pub meta: SessionMeta,
}

/// Request payload for `SessionStore/create_segment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSegmentRequest {
    /// Segment metadata to persist.
    pub segment: TaskSegment,
}

/// Request payload for `SessionStore/complete_segment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteSegmentRequest {
    /// Segment identifier to complete.
    pub segment_id: SegmentId,
    /// Completion counters and end timestamp.
    pub update: SegmentCompletion,
}

/// Request payload for `SessionStore/update_segment_assessment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateSegmentAssessmentRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Full assessment outcome and evidence.
    pub assessment: SegmentAssessment,
}

/// Request payload for `SessionStore/get_segment_baseline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSegmentBaselineRequest {
    /// Tenant identifier.
    pub tenant_id: TenantId,
}

/// Request payload for `SessionStore/list_skill_resolution_rates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSkillResolutionRatesRequest {
    /// Tenant identifier.
    pub tenant_id: TenantId,
}

/// Request payload for `SessionStore/list_task_strategy_success_rates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListTaskStrategySuccessRatesRequest {
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Task fingerprint hash to aggregate against.
    pub task_fingerprint: String,
}

/// Request payload for `SessionStore/append_experience_record`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendExperienceRecordRequest {
    /// Experience record to append or idempotently refresh.
    pub experience: ExperienceRecord,
}

/// Request payload for `SessionStore/append_experience_attributions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendExperienceAttributionsRequest {
    /// Attribution records to append or idempotently refresh.
    #[serde(default)]
    pub attributions: Vec<ExperienceAttribution>,
}

/// Request payload for `SessionStore/list_experience_records`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListExperienceRecordsRequest {
    /// Session whose experience records should be listed.
    pub session_id: SessionId,
}

/// Request payload for `SessionStore/list_experience_attributions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListExperienceAttributionsRequest {
    /// Experience whose attribution records should be listed.
    pub experience_id: Uuid,
}

/// Request payload for `SessionStore/append_learning_candidate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendLearningCandidateRequest {
    /// Candidate to append or idempotently refresh.
    pub candidate: LearningCandidate,
}

/// Request payload for `SessionStore/get_learning_candidate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetLearningCandidateRequest {
    /// Tenant that owns the candidate.
    pub tenant_id: TenantId,
    /// Candidate identifier to load.
    pub candidate_id: Uuid,
}

/// Request payload for `SessionStore/list_learning_candidates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListLearningCandidatesRequest {
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Optional candidate status filter.
    pub status: Option<LearningCandidateStatus>,
    /// Maximum rows to return.
    pub limit: usize,
}

/// Review action requested for one learning candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCandidateReviewAction {
    /// Accept the candidate and promote it through the relevant review path.
    Accept,
    /// Reject the candidate while preserving its draft artifacts for audit.
    Reject,
    /// Close an informational item that no code can apply.
    ///
    /// The only decision available for an advisory or authoring candidate.
    /// Deliberately a distinct action rather than a flavor of reject: rejecting
    /// means a reviewer declined a proposal that could have been accepted, and
    /// there is no such proposal here. Nothing is ever promoted by a dismissal,
    /// which is why there is no generic promotion switch anywhere on this enum.
    Dismiss,
}

/// Request payload for reviewing one learning candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateReviewRequest {
    /// Tenant that owns the candidate.
    pub tenant_id: TenantId,
    /// Candidate identifier to review.
    pub candidate_id: Uuid,
    /// Review decision to apply.
    pub action: LearningCandidateReviewAction,
    /// Subject identifier for the human or service reviewer.
    pub reviewer_subject: String,
    /// Optional human-readable review reason.
    pub reason: Option<String>,
}

/// Response payload returned after reviewing one learning candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateReviewResponse {
    /// Candidate whose status was updated.
    pub candidate_id: Uuid,
    /// Candidate status after the review action.
    pub status: LearningCandidateStatus,
    /// Artifact that the candidate refers to, when applicable.
    pub artifact_uid: Option<Uuid>,
    /// Draft artifact revision that the candidate refers to, when applicable.
    pub draft_artifact_revision_uid: Option<Uuid>,
    /// Published artifact revision created by acceptance, when applicable.
    pub published_artifact_revision_uid: Option<Uuid>,
}

/// Request payload for recording active-segment tool usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentToolUseRequest {
    /// Session whose active segment receives the tool usage.
    pub session_id: SessionId,
    /// Tool name to record.
    pub tool_name: String,
}

/// Request payload for recording active-segment skill activation (injection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentSkillActivationRequest {
    /// Session whose active segment receives the skill activation.
    pub session_id: SessionId,
    /// Skill name to record.
    pub skill_name: String,
}

/// Request payload for recording that the model engaged a skill on the active segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentSkillUseRequest {
    /// Session whose active segment receives the skill use.
    pub session_id: SessionId,
    /// Skill name the model engaged.
    pub skill_name: String,
}

/// Request payload for recording active-segment turn usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentTurnUsageRequest {
    /// Session whose active segment receives the turn usage.
    pub session_id: SessionId,
    /// Token cost to add for the turn.
    pub token_cost: u64,
}

/// Request payload for `SessionStore/list_sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionsRequest {
    /// Session summary filter.
    pub filter: SessionFilter,
}

/// Request payload for `SessionStore/tenant_cost_since`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantCostSinceRequest {
    /// Tenant whose spend should be aggregated.
    pub tenant_id: TenantId,
    /// Inclusive lower-bound timestamp for the spend query.
    pub since: DateTime<Utc>,
}

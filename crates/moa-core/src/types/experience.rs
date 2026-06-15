//! Experience-learning DTOs derived from assessed task segments.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{SegmentEvidence, SegmentId, SegmentOutcome, SessionId, TenantId, UserId, WorkspaceId};

/// Stable task grouping key used for task-conditioned learning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFingerprint {
    /// Stable hash over the normalized task summary and deterministic task facets.
    pub hash: String,
    /// Human-readable normalized summary used to derive the hash.
    pub normalized_summary: String,
    /// Extraction policy that produced this fingerprint.
    pub policy_version: String,
}

/// Deterministic task facets used for grouping similar work without a fixed taxonomy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFacetSet {
    /// Broad domain inferred from the task, such as `rust`, `auth`, or `docs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Primary action, such as `debug`, `implement`, `review`, or `document`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Artifact class being changed or produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_kind: Option<String>,
    /// Language, framework, or platform named by the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_or_framework: Option<String>,
    /// Verification pattern implied or observed for the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_style: Option<String>,
    /// Risk class inferred for promotion and policy learning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_class: Option<String>,
    /// Tool names observed in the task segment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_pattern: Vec<String>,
    /// Skill names activated in the task segment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_pattern: Vec<String>,
}

/// Resource touched by an experience episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperienceResource {
    /// Resource type, such as `file`, `memory`, `tool`, or `url`.
    pub resource_type: String,
    /// Stable resource identifier when available.
    pub id: String,
    /// Optional human-readable resource label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Immutable learning episode derived from one assessed task segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceRecord {
    /// Stable experience identifier.
    pub id: Uuid,
    /// Assessed task segment this experience was derived from.
    pub segment_id: SegmentId,
    /// Session that owns the segment.
    pub session_id: SessionId,
    /// Tenant scope for task-conditioned learning.
    pub tenant_id: TenantId,
    /// Workspace scope for data isolation.
    pub workspace_id: WorkspaceId,
    /// User scope for user-personal learning evidence.
    pub user_id: UserId,
    /// Best-effort task summary.
    pub task_summary: Option<String>,
    /// Stable grouping fingerprint for similar tasks.
    pub task_fingerprint: TaskFingerprint,
    /// Deterministic task facets.
    pub task_facets: TaskFacetSet,
    /// High-level actions inferred from segment events.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Resources touched by the segment when available.
    #[serde(default)]
    pub resources: Vec<ExperienceResource>,
    /// Assessed outcome for the segment.
    pub outcome: SegmentOutcome,
    /// Confidence in the assessed outcome.
    pub confidence: f64,
    /// Evidence copied from the segment assessment.
    #[serde(default)]
    pub evidence: Vec<SegmentEvidence>,
    /// Tool names used by the segment.
    #[serde(default)]
    pub tools_used: Vec<String>,
    /// Skill names activated by the segment.
    #[serde(default)]
    pub skills_activated: Vec<String>,
    /// Number of turns attributed to the segment.
    pub turn_count: u32,
    /// Token cost attributed to the segment.
    pub token_cost: u64,
    /// Duration in milliseconds when the segment had a closed boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Segment-assessor policy version used for the assessment.
    pub assessment_policy_version: String,
    /// Experience-extraction policy version used for this record.
    pub extraction_policy_version: String,
    /// Time the experience record was created.
    pub created_at: DateTime<Utc>,
}

/// Subject type assigned to an attribution or strategy-rate row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionSubjectType {
    /// A skill package was part of the segment.
    Skill,
    /// A tool was part of the segment.
    Tool,
    /// A memory retrieval or memory write was part of the segment.
    Memory,
    /// A prompt or policy instruction influenced the segment.
    Policy,
    /// Verification behavior influenced the segment.
    Verification,
}

impl AttributionSubjectType {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Tool => "tool",
            Self::Memory => "memory",
            Self::Policy => "policy",
            Self::Verification => "verification",
        }
    }
}

/// Directional effect assigned during experience attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionEffect {
    /// The subject appears to have helped the outcome.
    Helpful,
    /// The subject appears neutral or inconclusive.
    Neutral,
    /// The subject appears to have hurt the outcome.
    Harmful,
    /// The subject has mixed evidence.
    Mixed,
}

impl AttributionEffect {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Helpful => "helpful",
            Self::Neutral => "neutral",
            Self::Harmful => "harmful",
            Self::Mixed => "mixed",
        }
    }
}

/// Attribution explaining why a strategy component helped or hurt an experience.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceAttribution {
    /// Stable attribution identifier.
    pub id: Uuid,
    /// Experience this attribution explains.
    pub experience_id: Uuid,
    /// Tenant scope for aggregation.
    pub tenant_id: TenantId,
    /// Workspace scope for data isolation.
    pub workspace_id: WorkspaceId,
    /// Optional user scope for user-personal learning.
    pub user_id: Option<UserId>,
    /// Subject type being attributed.
    pub subject_type: AttributionSubjectType,
    /// Stable subject identifier, such as a skill or tool name.
    pub subject_id: String,
    /// Directional effect attributed to the subject.
    pub effect: AttributionEffect,
    /// Confidence in this attribution.
    pub confidence: f64,
    /// Concise evidence summaries that justify the attribution.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Time the attribution was created.
    pub created_at: DateTime<Utc>,
}

/// Learning-candidate target type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCandidateType {
    /// Candidate proposes creating or changing a skill package.
    Skill,
    /// Candidate proposes writing or updating memory.
    Memory,
    /// Candidate proposes changing a runtime or tool policy.
    Policy,
    /// Candidate proposes adding eval coverage.
    Eval,
    /// Candidate proposes changing prompt instructions.
    Prompt,
}

impl LearningCandidateType {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Memory => "memory",
            Self::Policy => "policy",
            Self::Eval => "eval",
            Self::Prompt => "prompt",
        }
    }
}

/// Durable status for a proposed learning mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCandidateStatus {
    /// Candidate was proposed but not evaluated.
    Proposed,
    /// Candidate is currently being evaluated.
    Evaluating,
    /// Candidate was promoted to active learned state.
    Promoted,
    /// Candidate was rejected.
    Rejected,
    /// Candidate was rolled back after promotion.
    RolledBack,
}

impl LearningCandidateStatus {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Evaluating => "evaluating",
            Self::Promoted => "promoted",
            Self::Rejected => "rejected",
            Self::RolledBack => "rolled_back",
        }
    }
}

/// Risk assigned to a candidate promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningRiskClass {
    /// Low blast-radius candidate.
    Low,
    /// Medium blast-radius candidate.
    Medium,
    /// High blast-radius candidate.
    High,
}

impl LearningRiskClass {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Proposed mutation to skill, memory, policy, prompt, or eval state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidate {
    /// Stable candidate identifier.
    pub id: Uuid,
    /// Tenant scope for the candidate.
    pub tenant_id: TenantId,
    /// Workspace scope for the candidate.
    pub workspace_id: WorkspaceId,
    /// Optional user scope for user-personal candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<UserId>,
    /// Candidate target type.
    pub candidate_type: LearningCandidateType,
    /// Current promotion status.
    pub status: LearningCandidateStatus,
    /// Optional target identifier when mutating existing learned state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Optional human-readable target label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    /// Task fingerprint the candidate is expected to help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_fingerprint: Option<TaskFingerprint>,
    /// Task facets the candidate is expected to help.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_facets: Option<TaskFacetSet>,
    /// Candidate payload, such as generated markdown or a memory proposal.
    pub payload: Value,
    /// Evaluation output attached during promotion review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_payload: Option<Value>,
    /// Source experiences that motivated this candidate.
    #[serde(default)]
    pub source_experience_ids: Vec<Uuid>,
    /// Confidence in the candidate proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Promotion risk class.
    pub risk_class: LearningRiskClass,
    /// Requirements that must pass before promotion.
    #[serde(default)]
    pub promotion_requirements: Vec<String>,
    /// Last status-transition reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// Optional batch ID for grouped rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<Uuid>,
    /// Candidate creation time.
    pub created_at: DateTime<Utc>,
    /// Last candidate update time.
    pub updated_at: DateTime<Utc>,
}

/// Explicit candidate status transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidateStatusUpdate {
    /// Candidate to update.
    pub candidate_id: Uuid,
    /// New durable status.
    pub status: LearningCandidateStatus,
    /// Optional transition reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    /// Optional evaluation payload attached to the status update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_payload: Option<Value>,
    /// Time the status transition was recorded.
    pub updated_at: DateTime<Utc>,
}

/// Task-conditioned strategy success aggregate used by ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStrategySuccessRate {
    /// Tenant scope for the aggregate.
    pub tenant_id: TenantId,
    /// Task fingerprint hash for the aggregate.
    pub task_fingerprint: String,
    /// Subject type being scored.
    pub subject_type: AttributionSubjectType,
    /// Subject identifier, such as a skill name.
    pub subject_id: String,
    /// Number of attributed experiences.
    pub uses: u64,
    /// Outcome-weighted success rate in `[0.0, 1.0]`.
    pub success_rate: f64,
    /// Mean assessment confidence for matching experiences.
    pub avg_confidence: f64,
    /// Average token cost for matching experiences.
    pub avg_token_cost: f64,
    /// Average turn count for matching experiences.
    pub avg_turn_count: f64,
}

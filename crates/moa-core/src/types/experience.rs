//! Experience-learning DTOs derived from assessed task segments.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    contact::ContactId, identifiers::SegmentId, identifiers::SessionId, identifiers::TenantId,
    identifiers::UserId, segment_assessment::SegmentEvidence, segment_assessment::SegmentOutcome,
};

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
    /// Skill names injected into the segment's turn manifest (candidates offered to the model).
    #[serde(default)]
    pub skills_activated: Vec<String>,
    /// Skill names the model actually engaged during the segment (subset of `skills_activated`).
    #[serde(default)]
    pub skills_used: Vec<String>,
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }
}

/// Directional effect assigned during experience attribution.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }
}

/// Distinguishes normal outcome attributions from weak negative-relevance markers.
///
/// A skill injected into a turn manifest but never engaged by the model is not
/// evidence that the skill helped or hurt the outcome. Such rows are recorded as
/// [`AttributionKind::UnusedInjection`] so ranking can exclude them from success
/// rates while still surfacing that the skill was offered and ignored.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AttributionKind {
    /// A normal outcome-mapped attribution for a subject the segment engaged.
    #[default]
    Standard,
    /// A skill that was injected into the turn manifest but never engaged by the model.
    UnusedInjection,
}

impl AttributionKind {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
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
    /// Optional user scope for user-personal learning.
    pub user_id: Option<UserId>,
    /// Subject type being attributed.
    pub subject_type: AttributionSubjectType,
    /// Stable subject identifier, such as a skill or tool name.
    pub subject_id: String,
    /// Directional effect attributed to the subject.
    pub effect: AttributionEffect,
    /// Whether this is a normal attribution or an unused-injection marker.
    #[serde(default)]
    pub kind: AttributionKind,
    /// Confidence in this attribution.
    pub confidence: f64,
    /// Concise evidence summaries that justify the attribution.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Time the attribution was created.
    pub created_at: DateTime<Utc>,
}

/// Learning-candidate target type.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }
}

/// Durable status for a proposed learning mutation.
///
/// Which statuses a candidate may hold is decided by its
/// [`LearningProposalKind`], not by its [`LearningCandidateType`]. A reviewable
/// proposal moves through the promotion lifecycle; an informational item lives
/// on the terminal-only advisory or authoring lifecycle and can never be
/// promoted, because no code exists that could materialize it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
    /// Informational memory observation offered for reading, never promotion.
    Advisory,
    /// Informational item that describes work a human would have to author.
    NeedsAuthoring,
    /// Informational item a reviewer closed without acting on it.
    Dismissed,
}

impl LearningCandidateStatus {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Review contract a learning candidate offers, independent of its target domain.
///
/// [`LearningCandidateType`] answers "what does this candidate want to change";
/// this answers "what can a reviewer actually do with it". Those were previously
/// the same field, which is how memory, policy, prompt, and eval suggestions
/// came to be written as `Proposed` and displayed beside skill drafts even
/// though nothing in the system could promote them. A reviewer could press
/// accept on a policy suggestion and get a success response for a change that
/// never happened.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LearningProposalKind {
    /// A generated skill draft with a real draft revision and an accept path.
    SkillDraft,
    /// A proposal to archive a regressed activated revision.
    SkillRollback,
    /// A memory observation surfaced for reading only.
    MemoryAdvisory,
    /// A skill suggestion with no draft behind it; authoring work, not a proposal.
    SkillAuthoring,
    /// An observed policy pattern a human would have to author.
    PolicyAuthoring,
    /// An observed prompt change a human would have to author.
    PromptAuthoring,
    /// An observed eval gap a human would have to author.
    EvalAuthoring,
}

impl LearningProposalKind {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Returns true when a reviewer can accept this kind into serving state.
    ///
    /// Only the two kinds with a transactional materializer qualify. Everything
    /// else can be read and dismissed, and nothing more.
    #[must_use]
    pub fn is_reviewable(self) -> bool {
        matches!(self, Self::SkillDraft | Self::SkillRollback)
    }

    /// Returns the only status a freshly written candidate of this kind may hold.
    #[must_use]
    pub fn initial_status(self) -> LearningCandidateStatus {
        match self {
            Self::SkillDraft | Self::SkillRollback => LearningCandidateStatus::Proposed,
            Self::MemoryAdvisory => LearningCandidateStatus::Advisory,
            Self::SkillAuthoring
            | Self::PolicyAuthoring
            | Self::PromptAuthoring
            | Self::EvalAuthoring => LearningCandidateStatus::NeedsAuthoring,
        }
    }

    /// Returns true when this kind admits `status` at all.
    #[must_use]
    pub fn permits_status(self, status: LearningCandidateStatus) -> bool {
        use LearningCandidateStatus as Status;
        match self {
            Self::SkillDraft => matches!(
                status,
                Status::Proposed
                    | Status::Evaluating
                    | Status::Promoted
                    | Status::Rejected
                    | Status::RolledBack
            ),
            Self::SkillRollback => matches!(
                status,
                Status::Proposed | Status::Evaluating | Status::Promoted | Status::Rejected
            ),
            Self::MemoryAdvisory => matches!(status, Status::Advisory | Status::Dismissed),
            Self::SkillAuthoring
            | Self::PolicyAuthoring
            | Self::PromptAuthoring
            | Self::EvalAuthoring => {
                matches!(status, Status::NeedsAuthoring | Status::Dismissed)
            }
        }
    }

    /// Returns true when this kind admits the exact `from -> to` transition.
    ///
    /// The database enforces the same table through a trigger; this exists so a
    /// caller can refuse before issuing a write it knows will be rejected, and
    /// so the rule is testable without a database.
    #[must_use]
    pub fn permits_transition(
        self,
        from: LearningCandidateStatus,
        to: LearningCandidateStatus,
    ) -> bool {
        use LearningCandidateStatus as Status;
        if from == to {
            return self.permits_status(from);
        }
        match self {
            Self::SkillDraft => matches!(
                (from, to),
                (Status::Proposed, Status::Evaluating)
                    | (Status::Evaluating, Status::Promoted)
                    | (Status::Evaluating, Status::Rejected)
                    | (Status::Evaluating, Status::Proposed)
                    | (Status::Promoted, Status::RolledBack)
            ),
            Self::SkillRollback => matches!(
                (from, to),
                (Status::Proposed, Status::Evaluating)
                    | (Status::Evaluating, Status::Promoted)
                    | (Status::Evaluating, Status::Rejected)
                    | (Status::Evaluating, Status::Proposed)
            ),
            Self::MemoryAdvisory => matches!((from, to), (Status::Advisory, Status::Dismissed)),
            Self::SkillAuthoring
            | Self::PolicyAuthoring
            | Self::PromptAuthoring
            | Self::EvalAuthoring => {
                matches!((from, to), (Status::NeedsAuthoring, Status::Dismissed))
            }
        }
    }
}

/// One typed provenance reference standing behind a learning candidate.
///
/// Deliberately an enum rather than a `(kind, uuid)` pair: a pair is the
/// `UUID[]` column this type replaces with an extra field, since neither the
/// database nor the compiler can tell which table the uuid belongs to. Each
/// variant maps to exactly one nullable column with a real composite foreign key
/// that carries the partition, so a cross-tenant source is rejected by the
/// constraint rather than by a check somebody has to remember to write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningCandidateSourceRef {
    /// An experience record derived from an assessed task segment.
    Experience {
        /// Experience the candidate was derived from.
        experience_id: Uuid,
    },
    /// One attribution explaining why a subject helped or hurt an experience.
    Attribution {
        /// Attribution the candidate was derived from.
        attribution_id: Uuid,
    },
    /// A whole session.
    Session {
        /// Session the candidate was derived from.
        session_id: SessionId,
    },
    /// One event inside a session. Events are partitioned by session, so both
    /// halves of the key are required to name one.
    Event {
        /// Event the candidate was derived from.
        event_id: Uuid,
        /// Session that owns the event.
        session_id: SessionId,
    },
    /// One assessed task segment.
    TaskSegment {
        /// Segment the candidate was derived from.
        segment_id: SegmentId,
    },
    /// The contact whose data the candidate is derived from.
    Contact {
        /// Contact the candidate is attributable to.
        contact_id: ContactId,
    },
    /// The promotion a rollback proposal reverses.
    PromotionCandidate {
        /// Candidate whose promotion this proposal would undo.
        candidate_id: Uuid,
    },
    /// An artifact revision the candidate was derived from or targets.
    ArtifactRevision {
        /// Revision the candidate references.
        revision_uid: Uuid,
    },
    /// An experiment run that produced the evidence.
    ExperimentRun {
        /// Run the candidate was derived from.
        run_uid: Uuid,
    },
    /// One trial inside an experiment run.
    ExperimentTrial {
        /// Trial the candidate was derived from.
        trial_uid: Uuid,
    },
    /// The score run that graded the evidence.
    ScoreRun {
        /// Score run the candidate was derived from.
        run_id: Uuid,
    },
}

impl LearningCandidateSourceRef {
    /// Returns the stable `source_kind` discriminator persisted alongside the reference.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Experience { .. } => "experience",
            Self::Attribution { .. } => "attribution",
            Self::Session { .. } => "session",
            Self::Event { .. } => "event",
            Self::TaskSegment { .. } => "task_segment",
            Self::Contact { .. } => "contact",
            Self::PromotionCandidate { .. } => "promotion_candidate",
            Self::ArtifactRevision { .. } => "artifact_revision",
            Self::ExperimentRun { .. } => "experiment_run",
            Self::ExperimentTrial { .. } => "experiment_trial",
            Self::ScoreRun { .. } => "score_run",
        }
    }
}

/// One normalized provenance row linking a learning candidate to one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningCandidateSource {
    /// Stable source-row identifier.
    pub id: Uuid,
    /// Candidate this source stands behind.
    pub candidate_id: Uuid,
    /// The typed reference itself.
    pub reference: LearningCandidateSourceRef,
}

impl LearningCandidateSource {
    /// Builds one source row with a fresh identifier.
    #[must_use]
    pub fn new(candidate_id: Uuid, reference: LearningCandidateSourceRef) -> Self {
        Self {
            id: Uuid::now_v7(),
            candidate_id,
            reference,
        }
    }
}

/// Review action recorded against one learning candidate.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LearningReviewDecision {
    /// A skill draft was accepted and its artifact activated.
    AcceptedSkill,
    /// A rollback proposal was accepted and its revision archived.
    AcceptedRollback,
    /// A reviewable proposal was rejected.
    Rejected,
    /// An informational item was closed without action.
    Dismissed,
}

impl LearningReviewDecision {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Durable historical disposition of one learning-candidate review.
///
/// The candidate's `status` column says where it is now; this says what was
/// decided and by whom. Export needs the second question answered, and a
/// mutable column cannot answer it after the fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidateDecisionRecord {
    /// Stable decision identifier.
    pub id: Uuid,
    /// Candidate the decision applies to.
    pub candidate_id: Uuid,
    /// Tenant scope for the decision.
    pub tenant_id: TenantId,
    /// Action the reviewer took.
    pub decision: LearningReviewDecision,
    /// Status the candidate held before the decision.
    pub from_status: LearningCandidateStatus,
    /// Status the candidate holds after the decision.
    pub to_status: LearningCandidateStatus,
    /// Reviewer identity when one was supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_subject: Option<String>,
    /// Free-text reason recorded with the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Canonical review-action digest for deterministic terminal replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<Vec<u8>>,
    /// Exact response returned by the terminal action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
    /// Time the decision was recorded.
    pub decided_at: DateTime<Utc>,
}

/// Risk assigned to a candidate promotion.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }
}

/// Proposed mutation to skill, memory, policy, prompt, or eval state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidate {
    /// Stable candidate identifier.
    pub id: Uuid,
    /// Tenant scope for the candidate.
    pub tenant_id: TenantId,
    /// Optional user scope for user-personal candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<UserId>,
    /// Candidate target type.
    pub candidate_type: LearningCandidateType,
    /// Review contract this candidate offers.
    pub proposal_kind: LearningProposalKind,
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
    /// Typed sources this candidate was derived from.
    ///
    /// Carried on the candidate itself rather than filed afterwards so a
    /// producer cannot write a candidate and forget its provenance: the store
    /// commits both in one transaction, and a deferred database constraint
    /// refuses the commit if this is empty. That closes the
    /// insert-then-forget shape a separate "add sources" call would leave open.
    #[serde(default)]
    pub sources: Vec<LearningCandidateSourceRef>,
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
    /// Mean attribution-effect score over the same non-unused rows as
    /// [`Self::success_rate`], mapping Helpful=1.0, Mixed=0.5, Neutral=0.5,
    /// Harmful=0.0. Diverges from `success_rate` when a used skill's engaging
    /// tool call failed (effect downgraded) or the outcome was `Unknown`, so it
    /// carries signal beyond the raw outcome. Defaults to the 0.5 neutral prior
    /// when no non-unused rows exist for the subject.
    pub effect_score: f64,
    /// Count of `unused_injection` attribution rows for this subject under the
    /// fingerprint: skills injected into the manifest but never engaged. High
    /// values relative to [`Self::uses`] are weak negative-relevance evidence.
    pub unused_injections: u64,
}

/// Redacted read-model projection of one learning candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningCandidateSummary {
    /// Stable candidate identifier.
    pub id: Uuid,
    /// Tenant scope for the candidate.
    pub tenant_id: TenantId,
    /// Optional contact scope for contact-local candidates.
    pub contact_id: Option<ContactId>,
    /// Candidate target type.
    pub candidate_type: LearningCandidateType,
    /// Review contract this candidate offers.
    pub proposal_kind: LearningProposalKind,
    /// Current promotion status.
    pub status: LearningCandidateStatus,
    /// Optional target identifier when mutating existing learned state.
    pub target_id: Option<String>,
    /// Optional human-readable target label.
    pub target_label: Option<String>,
    /// Task fingerprint hash the candidate is expected to help.
    pub task_fingerprint: Option<String>,
    /// Confidence in the candidate proposal.
    pub confidence: Option<f64>,
    /// Promotion risk class.
    pub risk_class: LearningRiskClass,
    /// Short, redacted preview of the candidate payload.
    pub payload_preview: String,
    /// Candidate creation time.
    pub created_at: DateTime<Utc>,
    /// Last candidate update time.
    pub updated_at: DateTime<Utc>,
}

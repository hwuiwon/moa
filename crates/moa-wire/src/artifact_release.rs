//! Artifact release-control service wire DTOs.
//!
//! Two absences are deliberate. A submit request carries no release policy: the
//! gate is resolved server-side, so a candidate submitter cannot name or weaken
//! its own gate. An activation request carries no verdict: it names an
//! attestation, and the attestation is what a deterministic decision produced
//! under a stronger authorization relation.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A draft dependency the submitter pins for the evaluation overlay.
///
/// Only listed artifacts resolve to a draft during evaluation; everything else
/// keeps resolving through the serving pointer, so an overlay widens resolution by
/// exactly what the submitter enumerated and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasePinnedDependency {
    /// Artifact whose resolution the overlay overrides.
    pub artifact_uid: Uuid,
    /// Exact revision the overlay resolves that artifact to.
    pub revision_uid: Uuid,
}

/// Request payload for submitting a candidate revision for release evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSubmitRequest {
    /// Tenant owning the candidate.
    pub tenant_id: TenantId,
    /// Candidate revision to evaluate.
    pub revision_uid: Uuid,
    /// Installation the candidate would deploy into, for agent subjects only.
    #[serde(default)]
    pub installation_uid: Option<Uuid>,
    /// Draft dependencies to pin for the evaluation-only overlay.
    #[serde(default)]
    pub pinned_draft_dependencies: Vec<ReleasePinnedDependency>,
}

/// Response payload describing the stored release candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSubmitResponse {
    /// Tenant owning the candidate.
    pub tenant_id: TenantId,
    /// Candidate revision.
    pub revision_uid: Uuid,
    /// Activation target class.
    pub activation_target: String,
    /// Candidate lifecycle state after submission.
    pub state: String,
    /// Coalescing slot the candidate occupies.
    pub slot: String,
    /// Hex-encoded digest of the exact evaluation subject.
    pub subject_digest: String,
    /// Server-resolved release policy that will gate this candidate.
    pub policy_uid: Uuid,
    /// Exact policy revision that is part of the subject digest.
    pub policy_revision: i32,
    /// Whether this submission took the artifact's active run slot.
    pub dispatched: bool,
    /// Candidate displaced from the pending slot, if any.
    pub displaced_pending_revision_uid: Option<Uuid>,
    /// Monotonic submission generation every result for this attempt is fenced by.
    pub generation: i64,
    /// Durable dispatch record written in the submission transaction.
    ///
    /// Present exactly when `dispatched` is true. A submission that landed in the
    /// pending slot gets no dispatch record; the decision that frees the active
    /// slot creates one for it.
    pub outbox_uid: Option<Uuid>,
    /// Deterministic dispatch key over revision, generation, and subject digest.
    pub dispatch_idempotency_key: Option<String>,
    /// Dispatch records abandoned because this submission superseded their subject.
    #[serde(default)]
    pub abandoned_outbox_uids: Vec<Uuid>,
}

/// Request payload for moving a type-owned serving pointer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseActivateRequest {
    /// Tenant owning the pointer.
    pub tenant_id: TenantId,
    /// Candidate revision to activate.
    pub revision_uid: Uuid,
    /// Attestation to consume.
    pub attestation_uid: Uuid,
    /// Installation to deploy into, for agent subjects only.
    #[serde(default)]
    pub installation_uid: Option<Uuid>,
    /// Operator-supplied reason recorded on the activation audit row.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response payload describing a completed activation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseActivateResponse {
    /// Audit row recording the decision.
    pub audit_uid: Uuid,
    /// Revision now serving.
    pub activated_revision_uid: Uuid,
    /// Revision that was serving before, if any.
    pub previous_revision_uid: Option<Uuid>,
    /// New serving pointer version.
    pub pointer_version: i64,
    /// Candidates superseded by this activation.
    pub superseded_revision_uids: Vec<Uuid>,
    /// Deployment row written for an agent activation.
    pub deployment_uid: Option<Uuid>,
}

/// Request payload for reading the release-attempt review surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptListRequest {
    /// Tenant whose attempts to read.
    pub tenant_id: TenantId,
    /// Maximum attempts to return, newest first.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// One release attempt on the artifact-release review surface.
///
/// Deliberately reports the hidden cohort *epoch* and nothing about its contents.
/// A tenant that could read the cohort could iterate against it, which is exactly
/// what rotation and the attempt budget exist to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptEntry {
    /// Attempt row identifier.
    pub attempt_uid: Uuid,
    /// Dispatch record the attempt belongs to.
    pub outbox_uid: Uuid,
    /// Candidate revision under evaluation.
    pub revision_uid: Uuid,
    /// Artifact whose serving pointer the candidate would move.
    pub artifact_uid: Uuid,
    /// Submission generation the attempt is fenced by.
    pub generation: i64,
    /// Hex-encoded subject digest the attempt ran.
    pub subject_digest: String,
    /// Activation target class.
    pub activation_target: String,
    /// Paired experiment run the candidate arm executed in.
    pub candidate_run_uid: Option<Uuid>,
    /// Paired experiment run the baseline arm executed in.
    pub baseline_run_uid: Option<Uuid>,
    /// Hidden cohort epoch the attempt faced.
    pub cohort_epoch: Option<i32>,
    /// Deterministic verdict, when one was recorded.
    pub verdict: Option<String>,
    /// Attestation minted by a passing verdict.
    pub attestation_uid: Option<Uuid>,
    /// Whether a superseded result was refused for this attempt.
    pub fenced_out: bool,
    /// Why the attempt was fenced out.
    pub fence_reason: Option<String>,
    /// Review state recorded on the artifact-release surface.
    pub review_state: String,
    /// Who recorded the review.
    pub reviewed_by: Option<String>,
    /// When the review was recorded.
    pub reviewed_at: Option<DateTime<Utc>>,
    /// Reviewer note.
    pub review_note: Option<String>,
    /// When the attempt was created.
    pub created_at: DateTime<Utc>,
}

/// Response payload listing release attempts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptListResponse {
    /// Tenant the attempts belong to.
    pub tenant_id: TenantId,
    /// Attempts, newest first.
    pub attempts: Vec<ReleaseAttemptEntry>,
}

/// Request payload for recording attestation review against one attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptReviewRequest {
    /// Tenant owning the attempt.
    pub tenant_id: TenantId,
    /// Attempt to review.
    pub attempt_uid: Uuid,
    /// Review outcome: `acknowledged` or `disputed`.
    pub review_state: String,
    /// Reviewer note.
    #[serde(default)]
    pub note: Option<String>,
}

/// Response payload describing the reviewed attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAttemptReviewResponse {
    /// The attempt after the review was recorded.
    pub attempt: ReleaseAttemptEntry,
}

//! Application helpers for reviewing generated skill learning candidates.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactFile, ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_core::{
    ActionRuleScope, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningEntry, MoaError, StoragePartitionId, TenantId,
};
use moa_db::ScopedConn;
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::future::Future;

use crate::util::{artifact_scope_context, tenant_artifact_scope};
use thiserror::Error;
use uuid::Uuid;

/// Store operations required by skill candidate review.
pub trait LearningReviewStore: Send + Sync {
    /// Loads one candidate visible in a tenant review scope.
    fn get_learning_candidate<'a>(
        &'a self,
        tenant_id: &'a TenantId,
        candidate_id: Uuid,
    ) -> impl Future<Output = std::result::Result<Option<LearningCandidate>, MoaError>> + Send + 'a;

    /// Applies a candidate status update only when the current status matches.
    fn update_learning_candidate_status_from<'a>(
        &'a self,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> impl Future<Output = std::result::Result<bool, MoaError>> + Send + 'a;

    /// Applies a candidate status update in the caller's open transaction.
    fn update_learning_candidate_status_from_in_tx<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> impl Future<Output = std::result::Result<bool, MoaError>> + Send + 'a;

    /// Appends one promoted learning-log entry in the caller's open transaction.
    fn append_learning_in_tx<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        entry: &'a LearningEntry,
    ) -> impl Future<Output = std::result::Result<(), MoaError>> + Send + 'a;
}

/// Request metadata supplied by the authenticated review service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReviewRequest {
    /// Tenant that owns the candidate.
    pub tenant_id: TenantId,
    /// Candidate selected for review.
    pub candidate_id: Uuid,
    /// Review action requested by the endpoint.
    pub action: SkillReviewAction,
    /// FGA subject of the authenticated reviewer.
    pub reviewer_subject: String,
    /// Optional human review reason.
    pub reason: Option<String>,
}

/// Review action label attached to candidate evaluation payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillReviewAction {
    /// Reviewer accepted a skill draft for promotion gates.
    Accept,
    /// Reviewer rejected a candidate.
    Reject,
}

/// Skill candidate and draft artifact state prepared for acceptance.
#[derive(Debug, Clone)]
pub struct PreparedSkillAcceptance {
    /// Tenant scope used for artifact publication and regression checks.
    pub scope: ActionRuleScope,
    /// Candidate being accepted.
    pub candidate: LearningCandidate,
    /// Draft skill artifact revision to publish.
    pub draft: StoredArtifactRevision,
    /// Draft skill artifact revision identifier.
    pub draft_artifact_revision_uid: Uuid,
    /// Draft package files used for regression checks.
    pub draft_files: Vec<ArtifactFile>,
    /// Validation report proving the draft is publishable.
    pub publish_report: ValidationReport,
}

/// Held-in and held-out acceptance checks recorded when a candidate is promoted.
///
/// Acceptance requires both checks to pass. `held_in_pass` records that the
/// draft validated as publishable and its own generated suite (derived from the
/// proposal's source session) parsed and executed. `held_out_pass` records the
/// result on material the candidate was *not* derived from: the previous
/// promoted revision's suite (each revision carries its own) plus sibling
/// suites accumulated from deduped recurring sessions. When no held-out
/// material exists yet — the first revision of a novel task — the description
/// says so explicitly instead of implying a split. The descriptions must state
/// what actually ran; they persist on the candidate's evaluation payload as the
/// audit record of the gate. Promotion errors unless both booleans are true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceChecks {
    /// Whether the held-in check (the evidence probes/tests now pass) succeeded.
    pub held_in_pass: bool,
    /// Human-readable description of the held-in check.
    pub held_in_description: String,
    /// Whether the held-out check (regression suite + golden no-regression) succeeded.
    pub held_out_pass: bool,
    /// Human-readable description of the held-out check.
    pub held_out_description: String,
}

impl AcceptanceChecks {
    /// Returns the first failing check's description, or `None` when both checks passed.
    #[must_use]
    pub fn failing_check(&self) -> Option<&str> {
        if !self.held_in_pass {
            Some(self.held_in_description.as_str())
        } else if !self.held_out_pass {
            Some(self.held_out_description.as_str())
        } else {
            None
        }
    }

    fn to_payload(&self) -> Value {
        json!({
            "held_in_pass": self.held_in_pass,
            "held_in_description": self.held_in_description,
            "held_out_pass": self.held_out_pass,
            "held_out_description": self.held_out_description,
        })
    }
}

/// Errors acceptance when either the held-in or held-out check failed.
fn ensure_acceptance_checks(checks: &AcceptanceChecks) -> Result<()> {
    if let Some(failing) = checks.failing_check() {
        return Err(bad_request(format!(
            "acceptance blocked: held-in/held-out check failed: {failing}"
        )));
    }
    Ok(())
}

/// Result of applying a learning-candidate review decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReviewOutcome {
    /// Candidate that was reviewed.
    pub candidate_id: Uuid,
    /// Final candidate status.
    pub status: LearningCandidateStatus,
    /// Artifact row linked to the candidate, when any.
    pub artifact_uid: Option<Uuid>,
    /// Draft artifact revision linked to the candidate, when any.
    pub draft_artifact_revision_uid: Option<Uuid>,
    /// Published artifact revision created by acceptance, when any.
    pub published_artifact_revision_uid: Option<Uuid>,
}

/// Error returned by skill candidate review helpers.
#[derive(Debug, Error)]
pub enum SkillReviewError {
    /// Caller supplied an invalid review request or candidate payload.
    #[error("{0}")]
    BadRequest(String),
    /// Requested review state was not found.
    #[error("{0}")]
    NotFound(String),
    /// Candidate status changed before the review could be applied.
    #[error("{0}")]
    Conflict(String),
    /// Shared MOA infrastructure failed.
    #[error(transparent)]
    Moa(#[from] MoaError),
}

/// Convenience result type for skill review operations.
pub type Result<T> = std::result::Result<T, SkillReviewError>;

/// Loads one candidate after the caller has authorized tenant review access.
pub async fn get_learning_candidate_for_review(
    store: &(impl LearningReviewStore + ?Sized),
    tenant_id: &TenantId,
    candidate_id: Uuid,
) -> Result<LearningCandidate> {
    load_candidate(store, tenant_id, candidate_id).await
}

/// Claims and validates a proposed skill candidate before review-time regression execution.
pub async fn prepare_skill_acceptance(
    store: &(impl LearningReviewStore + ?Sized),
    pool: sqlx::PgPool,
    request: &SkillReviewRequest,
) -> Result<PreparedSkillAcceptance> {
    let candidate = load_candidate(store, &request.tenant_id, request.candidate_id).await?;
    ensure_skill_candidate(&candidate)?;
    ensure_proposed_candidate(&candidate)?;
    let draft_revision_uid = payload_uuid(&candidate.payload, "draft_artifact_revision_uid")?;

    let scope = tenant_artifact_scope(candidate.tenant_id);
    let artifact_registry = ArtifactRegistry::new(pool.clone());
    let draft = artifact_registry
        .load_revision(&scope, draft_revision_uid)
        .await?
        .ok_or_else(|| {
            SkillReviewError::NotFound("draft artifact revision not found".to_string())
        })?;
    ensure_tenant_skill_draft(&draft, candidate.tenant_id)?;

    let mut document = draft.document.clone();
    document.reference_resolutions = ArtifactResolver::new(ArtifactRegistry::new(pool.clone()))
        .resolve_document(&scope, &document)
        .await?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        return Err(bad_request(
            "skill draft artifact revision is not publishable",
        ));
    }

    let draft_files = artifact_registry
        .load_files(&scope, draft_revision_uid)
        .await?;
    claim_candidate_for_acceptance(store, candidate.id).await?;

    Ok(PreparedSkillAcceptance {
        scope,
        candidate,
        draft,
        draft_artifact_revision_uid: draft_revision_uid,
        draft_files,
        publish_report: report,
    })
}

/// Rejects a claimed skill candidate after a review-time promotion gate fails.
pub async fn reject_claimed_skill_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    request: &SkillReviewRequest,
    prepared: &PreparedSkillAcceptance,
    regression_report: Value,
    rejection_reason: Option<String>,
) -> Result<SkillReviewOutcome> {
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &prepared.candidate,
        artifact_uid: Some(prepared.draft.artifact_uid),
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        published_artifact_revision_uid: None,
        regression_report: Some(regression_report),
    });
    finish_claimed_candidate_review(
        store,
        LearningCandidateStatusUpdate {
            candidate_id: prepared.candidate.id,
            status: LearningCandidateStatus::Rejected,
            status_reason: Some(
                rejection_reason
                    .unwrap_or_else(|| "skill regression rejected the proposed draft".to_string()),
            ),
            evaluation_payload: Some(evaluation_payload),
            updated_at: Utc::now(),
        },
    )
    .await?;

    Ok(SkillReviewOutcome {
        candidate_id: prepared.candidate.id,
        status: LearningCandidateStatus::Rejected,
        artifact_uid: Some(prepared.draft.artifact_uid),
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        published_artifact_revision_uid: None,
    })
}

/// Publishes a claimed skill candidate and records promoted learning.
pub async fn promote_claimed_skill_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    pool: sqlx::PgPool,
    request: &SkillReviewRequest,
    prepared: PreparedSkillAcceptance,
    regression_report: Value,
    acceptance_checks: AcceptanceChecks,
) -> Result<SkillReviewOutcome> {
    // Reject before mutating any surface: promotion cannot publish unless both the held-in and
    // held-out checks passed.
    ensure_acceptance_checks(&acceptance_checks)?;
    let scope_context = artifact_scope_context(&prepared.scope);
    let mut conn = ScopedConn::begin(&pool, &scope_context).await?;
    let published = ArtifactRegistry::publish_revision_in_tx(
        conn.as_mut(),
        prepared.draft_artifact_revision_uid,
        &prepared.publish_report,
    )
    .await?;
    let artifact_uid = Some(published.artifact_uid);
    let mut evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &prepared.candidate,
        artifact_uid,
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        published_artifact_revision_uid: Some(published.revision_uid),
        regression_report: Some(regression_report),
    });
    evaluation_payload["acceptance_checks"] = acceptance_checks.to_payload();

    finish_claimed_candidate_review_in_tx(
        store,
        conn.as_mut(),
        LearningCandidateStatusUpdate {
            candidate_id: prepared.candidate.id,
            status: LearningCandidateStatus::Promoted,
            status_reason: Some(
                request
                    .reason
                    .clone()
                    .unwrap_or_else(|| "accepted by reviewer".to_string()),
            ),
            evaluation_payload: Some(evaluation_payload.clone()),
            updated_at: Utc::now(),
        },
    )
    .await?;
    let learning_entry =
        accepted_learning_entry(&prepared.candidate, request, &published, evaluation_payload)?;
    store
        .append_learning_in_tx(conn.as_mut(), &learning_entry)
        .await?;
    conn.commit().await?;

    Ok(SkillReviewOutcome {
        candidate_id: prepared.candidate.id,
        status: LearningCandidateStatus::Promoted,
        artifact_uid,
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        published_artifact_revision_uid: Some(published.revision_uid),
    })
}

/// Releases a claimed candidate back to `Proposed` after an operational failure.
///
/// Gate errors that are not properties of the candidate — an unavailable
/// provider, eval infrastructure failure — must not strand the candidate in
/// `Evaluating`: acceptance requires `Proposed`, so an unreleased claim would
/// make the accept permanently unretryable.
pub async fn release_claimed_skill_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    candidate_id: Uuid,
    reason: &str,
) -> Result<()> {
    finish_claimed_candidate_review(
        store,
        LearningCandidateStatusUpdate {
            candidate_id,
            status: LearningCandidateStatus::Proposed,
            status_reason: Some(format!(
                "promotion gate failed operationally; claim released for retry: {reason}"
            )),
            evaluation_payload: None,
            updated_at: Utc::now(),
        },
    )
    .await
}

/// Rejects a proposed learning candidate while preserving linked draft artifacts.
pub async fn reject_learning_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    request: &SkillReviewRequest,
) -> Result<SkillReviewOutcome> {
    let candidate = load_candidate(store, &request.tenant_id, request.candidate_id).await?;
    ensure_proposed_candidate(&candidate)?;
    let artifact_uid = optional_payload_uuid(&candidate.payload, "artifact_uid")?;
    let draft_artifact_revision_uid =
        optional_payload_uuid(&candidate.payload, "draft_artifact_revision_uid")?;
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &candidate,
        artifact_uid,
        draft_artifact_revision_uid,
        published_artifact_revision_uid: None,
        regression_report: None,
    });

    finish_proposed_candidate_review(
        store,
        LearningCandidateStatusUpdate {
            candidate_id: candidate.id,
            status: LearningCandidateStatus::Rejected,
            status_reason: Some(
                request
                    .reason
                    .clone()
                    .unwrap_or_else(|| "rejected by reviewer".to_string()),
            ),
            evaluation_payload: Some(evaluation_payload),
            updated_at: Utc::now(),
        },
    )
    .await?;

    Ok(SkillReviewOutcome {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Rejected,
        artifact_uid,
        draft_artifact_revision_uid,
        published_artifact_revision_uid: None,
    })
}

async fn load_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    tenant_id: &TenantId,
    candidate_id: Uuid,
) -> Result<LearningCandidate> {
    store
        .get_learning_candidate(tenant_id, candidate_id)
        .await?
        .ok_or_else(|| SkillReviewError::NotFound("learning candidate not found".to_string()))
}

fn ensure_skill_candidate(candidate: &LearningCandidate) -> Result<()> {
    if candidate.candidate_type != LearningCandidateType::Skill {
        return Err(bad_request(
            "only skill learning candidates can be accepted by this endpoint",
        ));
    }
    Ok(())
}

fn ensure_proposed_candidate(candidate: &LearningCandidate) -> Result<()> {
    if candidate.status != LearningCandidateStatus::Proposed {
        return Err(bad_request(
            "learning candidate must be proposed before review",
        ));
    }
    Ok(())
}

fn ensure_tenant_skill_draft(revision: &StoredArtifactRevision, tenant_id: TenantId) -> Result<()> {
    if revision.kind != ArtifactKind::Skill {
        return Err(bad_request(
            "draft artifact revision must be a skill artifact",
        ));
    }
    if revision.status != ArtifactStatus::Draft {
        return Err(bad_request("skill artifact revision must still be a draft"));
    }
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    if revision.storage_partition_id.as_ref().map(|id| id.as_str())
        != Some(storage_partition_id.as_str())
        || revision.user_id.is_some()
    {
        return Err(bad_request(
            "skill draft artifact revision must belong to the requested tenant scope",
        ));
    }
    Ok(())
}

async fn claim_candidate_for_acceptance(
    store: &(impl LearningReviewStore + ?Sized),
    candidate_id: Uuid,
) -> Result<()> {
    finish_candidate_review_from(
        store,
        LearningCandidateStatusUpdate {
            candidate_id,
            status: LearningCandidateStatus::Evaluating,
            status_reason: Some("review accepted; running promotion gates".to_string()),
            evaluation_payload: None,
            updated_at: Utc::now(),
        },
        LearningCandidateStatus::Proposed,
    )
    .await
}

async fn finish_proposed_candidate_review(
    store: &(impl LearningReviewStore + ?Sized),
    update: LearningCandidateStatusUpdate,
) -> Result<()> {
    finish_candidate_review_from(store, update, LearningCandidateStatus::Proposed).await
}

async fn finish_claimed_candidate_review(
    store: &(impl LearningReviewStore + ?Sized),
    update: LearningCandidateStatusUpdate,
) -> Result<()> {
    finish_candidate_review_from(store, update, LearningCandidateStatus::Evaluating).await
}

async fn finish_claimed_candidate_review_in_tx(
    store: &(impl LearningReviewStore + ?Sized),
    conn: &mut PgConnection,
    update: LearningCandidateStatusUpdate,
) -> Result<()> {
    finish_candidate_review_from_in_tx(store, conn, update, LearningCandidateStatus::Evaluating)
        .await
}

async fn finish_candidate_review_from(
    store: &(impl LearningReviewStore + ?Sized),
    update: LearningCandidateStatusUpdate,
    expected_status: LearningCandidateStatus,
) -> Result<()> {
    let changed = store
        .update_learning_candidate_status_from(&update, expected_status)
        .await?;
    if changed {
        return Ok(());
    }
    Err(SkillReviewError::Conflict(
        "learning candidate changed status before review could be applied".to_string(),
    ))
}

async fn finish_candidate_review_from_in_tx(
    store: &(impl LearningReviewStore + ?Sized),
    conn: &mut PgConnection,
    update: LearningCandidateStatusUpdate,
    expected_status: LearningCandidateStatus,
) -> Result<()> {
    let changed = store
        .update_learning_candidate_status_from_in_tx(conn, &update, expected_status)
        .await?;
    if changed {
        return Ok(());
    }
    Err(SkillReviewError::Conflict(
        "learning candidate changed status before review could be applied".to_string(),
    ))
}

fn payload_uuid(payload: &Value, key: &str) -> Result<Uuid> {
    let value = payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request(format!("candidate payload missing {key}")))?;
    Uuid::parse_str(value)
        .map_err(|error| bad_request(format!("candidate payload {key} is invalid: {error}")))
}

fn optional_payload_uuid(payload: &Value, key: &str) -> Result<Option<Uuid>> {
    let Some(value) = payload.get(key).and_then(Value::as_str) else {
        return Ok(None);
    };
    Uuid::parse_str(value)
        .map(Some)
        .map_err(|error| bad_request(format!("candidate payload {key} is invalid: {error}")))
}

struct ReviewEvaluationPayload<'a> {
    request: &'a SkillReviewRequest,
    candidate: &'a LearningCandidate,
    artifact_uid: Option<Uuid>,
    draft_artifact_revision_uid: Option<Uuid>,
    published_artifact_revision_uid: Option<Uuid>,
    regression_report: Option<Value>,
}

fn review_evaluation_payload(input: ReviewEvaluationPayload<'_>) -> Value {
    let regression_execution = input
        .regression_report
        .as_ref()
        .and_then(|report| report.get("regression_execution"))
        .cloned();
    json!({
        "reviewer_subject": input.request.reviewer_subject.clone(),
        "action": input.request.action.as_str(),
        "reason": input.request.reason.clone(),
        "candidate_id": input.candidate.id,
        "artifact_uid": input.artifact_uid,
        "draft_artifact_revision_uid": input.draft_artifact_revision_uid,
        "published_artifact_revision_uid": input.published_artifact_revision_uid,
        "regression_execution": regression_execution,
        "regression_report": input.regression_report,
    })
}

fn accepted_learning_entry(
    candidate: &LearningCandidate,
    request: &SkillReviewRequest,
    published: &StoredArtifactRevision,
    evaluation_payload: Value,
) -> Result<LearningEntry> {
    let learning_type = accepted_learning_type(candidate)?;
    Ok(LearningEntry {
        id: Uuid::now_v7(),
        tenant_id: candidate.tenant_id,
        learning_type,
        target_id: target_id(candidate, published),
        target_label: target_label(candidate),
        payload: json!({
            "candidate_id": candidate.id,
            "reviewer_subject": request.reviewer_subject,
            "reason": request.reason,
            "artifact_uid": published.artifact_uid,
            "published_artifact_revision_uid": published.revision_uid,
            "review": evaluation_payload,
        }),
        confidence: candidate.confidence,
        source_refs: source_refs(candidate),
        actor: format!("review:{}", request.reviewer_subject),
        valid_from: Utc::now(),
        valid_to: None,
        batch_id: candidate.batch_id,
        version: 1,
    })
}

fn accepted_learning_type(candidate: &LearningCandidate) -> Result<String> {
    match candidate.payload.get("operation").and_then(Value::as_str) {
        Some("skill_created") => Ok("skill_created".to_string()),
        Some("skill_improved") => Ok("skill_improved".to_string()),
        Some(other) => Err(bad_request(format!(
            "unsupported skill proposal operation `{other}`"
        ))),
        None => Err(bad_request("candidate payload missing operation")),
    }
}

fn target_id(candidate: &LearningCandidate, published: &StoredArtifactRevision) -> String {
    candidate
        .target_id
        .clone()
        .or_else(|| {
            candidate
                .payload
                .get("artifact_path")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| format!("artifact_revision:{}", published.revision_uid))
}

fn target_label(candidate: &LearningCandidate) -> Option<String> {
    candidate.target_label.clone().or_else(|| {
        candidate
            .payload
            .get("artifact_name")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn source_refs(candidate: &LearningCandidate) -> Vec<Uuid> {
    let mut refs = Vec::with_capacity(candidate.source_experience_ids.len() + 2);
    refs.push(candidate.id);
    if let Some(session_id) = candidate
        .payload
        .get("source_session_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
    {
        refs.push(session_id);
    }
    refs.extend(candidate.source_experience_ids.iter().copied());
    refs
}

impl SkillReviewAction {
    /// Returns the stable payload label for this action.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
}

fn bad_request(message: impl Into<String>) -> SkillReviewError {
    SkillReviewError::BadRequest(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::LearningRiskClass;
    use std::sync::Mutex;

    /// In-memory [`LearningReviewStore`] that records the status updates it receives.
    ///
    /// Only the pool-free review paths (`reject_learning_candidate`,
    /// `get_learning_candidate_for_review`) are exercised hermetically; the in-transaction
    /// methods belong to the pool-backed promote path and must never be reached here.
    #[derive(Default)]
    struct RecordingReviewStore {
        candidate: Option<LearningCandidate>,
        status_change_applies: bool,
        recorded_update: Mutex<Option<(LearningCandidateStatusUpdate, LearningCandidateStatus)>>,
    }

    impl LearningReviewStore for RecordingReviewStore {
        fn get_learning_candidate<'a>(
            &'a self,
            _tenant_id: &'a TenantId,
            _candidate_id: Uuid,
        ) -> impl Future<Output = std::result::Result<Option<LearningCandidate>, MoaError>> + Send + 'a
        {
            let candidate = self.candidate.clone();
            async move { Ok(candidate) }
        }

        fn update_learning_candidate_status_from<'a>(
            &'a self,
            update: &'a LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> impl Future<Output = std::result::Result<bool, MoaError>> + Send + 'a {
            let applies = self.status_change_applies;
            *self
                .recorded_update
                .lock()
                .expect("record candidate status update") = Some((update.clone(), expected_status));
            async move { Ok(applies) }
        }

        async fn update_learning_candidate_status_from_in_tx(
            &self,
            _conn: &mut PgConnection,
            _update: &LearningCandidateStatusUpdate,
            _expected_status: LearningCandidateStatus,
        ) -> std::result::Result<bool, MoaError> {
            unreachable!("in-tx status update is only used by the pool-backed promote path")
        }

        async fn append_learning_in_tx(
            &self,
            _conn: &mut PgConnection,
            _entry: &LearningEntry,
        ) -> std::result::Result<(), MoaError> {
            unreachable!("append_learning_in_tx is only used by the pool-backed promote path")
        }
    }

    fn proposed_skill_candidate(payload: Value) -> LearningCandidate {
        LearningCandidate {
            id: Uuid::from_u128(42),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: None,
            task_fingerprint: None,
            task_facets: None,
            payload,
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Medium,
            promotion_requirements: Vec::new(),
            status_reason: None,
            batch_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn reject_request(reason: Option<&str>) -> SkillReviewRequest {
        SkillReviewRequest {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            candidate_id: Uuid::from_u128(42),
            action: SkillReviewAction::Reject,
            reviewer_subject: "user:reviewer".to_string(),
            reason: reason.map(ToString::to_string),
        }
    }

    #[tokio::test]
    async fn reject_learning_candidate_marks_candidate_rejected_and_preserves_draft() {
        // Pins: rejecting a proposed candidate compare-and-sets Proposed -> Rejected and keeps
        // the linked draft/artifact references so the draft is preserved, not deleted.
        let draft_uid = Uuid::from_u128(77);
        let artifact_uid = Uuid::from_u128(88);
        let store = RecordingReviewStore {
            candidate: Some(proposed_skill_candidate(json!({
                "draft_artifact_revision_uid": draft_uid.to_string(),
                "artifact_uid": artifact_uid.to_string(),
            }))),
            status_change_applies: true,
            recorded_update: Mutex::new(None),
        };
        let request = reject_request(Some("not reusable"));

        let outcome = reject_learning_candidate(&store, &request)
            .await
            .expect("reject succeeds");

        assert_eq!(outcome.status, LearningCandidateStatus::Rejected);
        assert_eq!(outcome.candidate_id, Uuid::from_u128(42));
        assert_eq!(outcome.draft_artifact_revision_uid, Some(draft_uid));
        assert_eq!(outcome.artifact_uid, Some(artifact_uid));
        assert_eq!(outcome.published_artifact_revision_uid, None);

        let recorded = store
            .recorded_update
            .lock()
            .expect("lock recorded update")
            .clone()
            .expect("a status update was applied");
        assert_eq!(
            recorded.1,
            LearningCandidateStatus::Proposed,
            "reject must compare-and-set against the Proposed status"
        );
        assert_eq!(recorded.0.status, LearningCandidateStatus::Rejected);
        assert_eq!(recorded.0.status_reason.as_deref(), Some("not reusable"));
    }

    #[tokio::test]
    async fn reject_learning_candidate_conflicts_when_status_changed() {
        // Pins: a compare-and-set miss (another writer moved the candidate first) surfaces as a
        // Conflict instead of silently succeeding.
        let store = RecordingReviewStore {
            candidate: Some(proposed_skill_candidate(json!({}))),
            status_change_applies: false,
            recorded_update: Mutex::new(None),
        };
        let request = reject_request(None);

        let error = reject_learning_candidate(&store, &request)
            .await
            .expect_err("expected-status mismatch must conflict");

        assert!(
            matches!(error, SkillReviewError::Conflict(_)),
            "expected Conflict, got {error:?}"
        );
    }

    #[tokio::test]
    async fn reject_learning_candidate_rejects_non_proposed_candidate() {
        // Pins: a candidate that already left the Proposed state cannot be rejected, and the guard
        // fires before any status write is attempted.
        let mut candidate = proposed_skill_candidate(json!({}));
        candidate.status = LearningCandidateStatus::Promoted;
        let store = RecordingReviewStore {
            candidate: Some(candidate),
            status_change_applies: true,
            recorded_update: Mutex::new(None),
        };
        let request = reject_request(None);

        let error = reject_learning_candidate(&store, &request)
            .await
            .expect_err("a promoted candidate cannot be rejected");

        assert!(
            matches!(error, SkillReviewError::BadRequest(_)),
            "expected BadRequest, got {error:?}"
        );
        assert!(
            store
                .recorded_update
                .lock()
                .expect("lock recorded update")
                .is_none(),
            "the proposed-state guard must reject before any status write"
        );
    }

    #[tokio::test]
    async fn get_learning_candidate_for_review_missing_candidate_is_not_found() {
        // Pins: a missing candidate maps to NotFound rather than a panic or empty success.
        let store = RecordingReviewStore::default();
        let tenant = TenantId::from(Uuid::from_u128(1));

        let error = get_learning_candidate_for_review(&store, &tenant, Uuid::from_u128(42))
            .await
            .expect_err("a missing candidate is not found");

        assert!(
            matches!(error, SkillReviewError::NotFound(_)),
            "expected NotFound, got {error:?}"
        );
    }

    fn checks(held_in: bool, held_out: bool) -> AcceptanceChecks {
        AcceptanceChecks {
            held_in_pass: held_in,
            held_in_description: "evidence probes now pass".to_string(),
            held_out_pass: held_out,
            held_out_description: "regression suite + golden no-regression".to_string(),
        }
    }

    #[test]
    fn acceptance_requires_both_held_in_and_held_out_checks() {
        // Pins: promotion is blocked unless both checks pass, and the surfaced error names the
        // specific failing check (the reason a rejection later logs). Only both-true accepts.
        let held_in_failed = ensure_acceptance_checks(&checks(false, true))
            .expect_err("a failed held-in check blocks acceptance");
        assert!(
            matches!(held_in_failed, SkillReviewError::BadRequest(message) if message.contains("evidence probes now pass")),
            "the held-in description must appear in the failing-check error"
        );

        let held_out_failed = ensure_acceptance_checks(&checks(true, false))
            .expect_err("a failed held-out check blocks acceptance");
        assert!(
            matches!(held_out_failed, SkillReviewError::BadRequest(message) if message.contains("regression suite")),
            "the held-out description must appear in the failing-check error"
        );

        ensure_acceptance_checks(&checks(true, true)).expect("both checks passing must accept");
    }

    #[test]
    fn acceptance_checks_payload_records_both_booleans_and_descriptions() {
        // Pins: the recorded acceptance payload keeps both booleans and descriptions so a promoted
        // candidate carries an auditable held-in/held-out record.
        let payload = checks(true, false).to_payload();
        assert_eq!(payload["held_in_pass"], json!(true));
        assert_eq!(payload["held_out_pass"], json!(false));
        assert_eq!(
            payload["held_in_description"],
            json!("evidence probes now pass")
        );
        assert_eq!(
            payload["held_out_description"],
            json!("regression suite + golden no-regression")
        );
    }

    #[test]
    fn accepted_learning_type_accepts_known_skill_operations() {
        // Pins: promotion appends a learning-log entry for both create and improve operations.
        for (operation, expected) in [
            ("skill_created", "skill_created"),
            ("skill_improved", "skill_improved"),
        ] {
            let candidate = proposed_skill_candidate(json!({ "operation": operation }));
            assert_eq!(
                accepted_learning_type(&candidate).expect("known operation is accepted"),
                expected
            );
        }
    }

    #[test]
    fn accepted_learning_type_rejects_unknown_operations() {
        // Pins: review promotion only appends known skill learning-log operations.
        let candidate = LearningCandidate {
            id: Uuid::from_u128(1),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: None,
            task_fingerprint: None,
            task_facets: None,
            payload: json!({"operation": "unknown_operation"}),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Medium,
            promotion_requirements: Vec::new(),
            status_reason: None,
            batch_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert!(
            accepted_learning_type(&candidate).is_err(),
            "skill review must not append unsupported learning-log operation types"
        );
    }
}

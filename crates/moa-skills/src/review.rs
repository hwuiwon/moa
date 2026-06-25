//! Application helpers for reviewing generated skill learning candidates.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactFile, ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_core::{
    ActionRuleScope, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningEntry, MoaError, TenantId,
};
use moa_db::ScopedConn;
use moa_memory_types::ScopeContext;
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;
use uuid::Uuid;

/// Boxed store operation future used to keep transaction-borrow lifetimes explicit.
pub type LearningReviewStoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = std::result::Result<T, MoaError>> + Send + 'a>>;

/// Store operations required by skill candidate review.
pub trait LearningReviewStore: Send + Sync {
    /// Loads one candidate visible in a tenant review scope.
    fn get_learning_candidate<'a>(
        &'a self,
        tenant_id: &'a TenantId,
        candidate_id: Uuid,
    ) -> LearningReviewStoreFuture<'a, Option<LearningCandidate>>;

    /// Applies a candidate status update only when the current status matches.
    fn update_learning_candidate_status_from<'a>(
        &'a self,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> LearningReviewStoreFuture<'a, bool>;

    /// Applies a candidate status update in the caller's open transaction.
    fn update_learning_candidate_status_from_in_tx<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> LearningReviewStoreFuture<'a, bool>;

    /// Appends one promoted learning-log entry.
    fn append_learning<'a>(&'a self, entry: &'a LearningEntry)
    -> LearningReviewStoreFuture<'a, ()>;

    /// Appends one promoted learning-log entry in the caller's open transaction.
    fn append_learning_in_tx<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        entry: &'a LearningEntry,
    ) -> LearningReviewStoreFuture<'a, ()>;
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
) -> Result<SkillReviewOutcome> {
    let scope_context = artifact_scope_context(&prepared.scope)?;
    let mut conn = ScopedConn::begin(&pool, &scope_context).await?;
    let published = ArtifactRegistry::publish_revision_in_tx(
        conn.as_mut(),
        prepared.draft_artifact_revision_uid,
        &prepared.publish_report,
    )
    .await?;
    let artifact_uid = Some(published.artifact_uid);
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &prepared.candidate,
        artifact_uid,
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        published_artifact_revision_uid: Some(published.revision_uid),
        regression_report: Some(regression_report),
    });

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

fn tenant_artifact_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

fn artifact_scope_context(scope: &ActionRuleScope) -> Result<ScopeContext> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => Ok(ScopeContext::tenant(*tenant_id)),
    }
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
    let storage_partition_id = tenant_id.to_string();
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
            payload: json!({"operation": "workflow_improved"}),
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

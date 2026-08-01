//! Application helpers for reviewing generated skill learning candidates.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactFile, ArtifactRegistry, CandidateSubjectInputs, RecordDecision, ReleaseRepository,
    StoredArtifactRevision, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationOutcome, ActivationRequest, ActivationTarget, AgentRuntimeSubject,
    DeterministicVerdict, Digest32, EvaluationPlanSubject, EvidenceAdapter, ExpectedServing,
    TenantScope,
};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_core::{
    error::MoaError, types::action_policy::ActionRuleScope, types::experience::LearningCandidate,
    types::experience::LearningCandidateSourceRef, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateStatusUpdate, types::experience::LearningProposalKind,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::learning::LearningEntry, types::learning::LearningLogSourceRef,
    types::memory::RlsContext,
};
use moa_db::ScopedConn;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::PgConnection;
use std::collections::BTreeMap;
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

    /// Records a durable review decision in the caller's open transaction.
    fn record_learning_candidate_decision_in_tx<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        decision: &'a moa_core::types::experience::LearningCandidateDecisionRecord,
    ) -> impl Future<Output = std::result::Result<bool, MoaError>> + Send + 'a;
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
    /// Validation report proving the draft is activatable.
    pub validation_report: ValidationReport,
    /// Serving-pointer generation the regression run was compared against.
    pub expected_serving: ExpectedServing,
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
    pub(crate) fn failing_check(&self) -> Option<&str> {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillReviewOutcome {
    /// Candidate that was reviewed.
    pub candidate_id: Uuid,
    /// Final candidate status.
    pub status: LearningCandidateStatus,
    /// Artifact row linked to the candidate, when any.
    pub artifact_uid: Option<Uuid>,
    /// Draft artifact revision linked to the candidate, when any.
    pub draft_artifact_revision_uid: Option<Uuid>,
    /// Artifact revision activated by acceptance, when any.
    pub activated_artifact_revision_uid: Option<Uuid>,
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

/// Inputs captured while a proposed candidate is claimed for rejection.
#[derive(Debug)]
struct PreparedSkillRejection {
    candidate: LearningCandidate,
    artifact_uid: Option<Uuid>,
    draft_artifact_revision_uid: Option<Uuid>,
    evaluation_payload: Value,
}

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
    ensure_skill_draft_proposal(&candidate)?;
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
    let report = validate_for_status(&document, ArtifactStatus::Ready);
    if !report.is_ok() {
        return Err(bad_request(
            "skill draft artifact revision is not activatable",
        ));
    }

    let draft_files = artifact_registry
        .load_files(&scope, draft_revision_uid)
        .await?;
    let release_scope = TenantScope::new(candidate.tenant_id);
    let activation_target = ActivationTarget::SkillVisibility {
        artifact_uid: draft.artifact_uid,
    };
    let expected_serving = ReleaseRepository::new(pool)
        .expected_serving(&release_scope, &activation_target)
        .await
        .map_err(release_error)?;
    claim_candidate_for_acceptance(store, candidate.id).await?;

    Ok(PreparedSkillAcceptance {
        scope,
        candidate,
        draft,
        draft_artifact_revision_uid: draft_revision_uid,
        draft_files,
        validation_report: report,
        expected_serving,
    })
}

/// Rejects a claimed skill candidate after a review-time promotion gate fails.
pub async fn reject_claimed_skill_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    pool: sqlx::PgPool,
    request: &SkillReviewRequest,
    prepared: &PreparedSkillAcceptance,
    regression_report: Value,
    rejection_reason: Option<String>,
    request_digest: &[u8],
) -> Result<SkillReviewOutcome> {
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &prepared.candidate,
        artifact_uid: Some(prepared.draft.artifact_uid),
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        activated_artifact_revision_uid: None,
        regression_report: Some(regression_report),
    });
    let outcome = SkillReviewOutcome {
        candidate_id: prepared.candidate.id,
        status: LearningCandidateStatus::Rejected,
        artifact_uid: Some(prepared.draft.artifact_uid),
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        activated_artifact_revision_uid: None,
    };
    let mut conn = ScopedConn::begin(&pool, &artifact_scope_context(&prepared.scope)).await?;
    finish_claimed_candidate_review_in_tx(
        store,
        conn.as_mut(),
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
    record_terminal_decision_in_tx(
        store,
        conn.as_mut(),
        request,
        request_digest,
        moa_core::types::experience::LearningReviewDecision::Rejected,
        LearningCandidateStatus::Evaluating,
        &outcome,
    )
    .await?;
    conn.commit().await?;

    Ok(outcome)
}

/// Moves the serving pointer to a promoted distilled skill revision.
///
/// The learning review has already produced deterministic evidence: the draft
/// validated as activatable, its generated suite ran, and the held-out suite ran.
/// That result is adapted into a release decision here -- it does not replace the
/// release contract. Everything happens in the caller's transaction, so a promoted
/// skill cannot end up serving with no learning record or vice versa.
struct ActivatedSkillRevision {
    revision: StoredArtifactRevision,
    activation: ActivationOutcome,
}

async fn activate_promoted_skill_revision(
    conn: &mut PgConnection,
    prepared: &PreparedSkillAcceptance,
    request: &SkillReviewRequest,
    regression_report: &Value,
) -> Result<ActivatedSkillRevision> {
    let now = Utc::now();
    let scope = TenantScope::from_action_rule_scope(&prepared.scope)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    let activation_target = ActivationTarget::SkillVisibility {
        artifact_uid: prepared.draft.artifact_uid,
    };
    let decided_by = request.reviewer_subject.clone();
    // The regression run is this candidate's eligibility step, so its resolved
    // report is recorded on the revision before submission. Without it the release
    // repository would refuse the candidate for having unresolved references, which
    // is the correct refusal for a candidate nothing validated.
    ArtifactRegistry::record_validation_report_in_tx(
        conn,
        prepared.draft_artifact_revision_uid,
        &prepared.validation_report,
    )
    .await?;
    let release_policy =
        ReleaseRepository::resolve_policy_in_tx(conn, &scope, activation_target.class())
            .await
            .map_err(release_error)?;
    let submission = ReleaseRepository::submit_candidate_in_tx(
        conn,
        &SubmitCandidate {
            scope,
            activation_target,
            candidate_revision_uid: prepared.draft_artifact_revision_uid,
            subject_inputs: learning_subject_inputs(prepared, regression_report)?,
            submitted_by: decided_by.clone(),
        },
        now,
    )
    .await
    .map_err(release_error)?;
    let candidate = submission.candidate;
    let decision = ReleaseRepository::record_decision_in_tx(
        conn,
        &RecordDecision {
            scope,
            candidate_revision_uid: prepared.draft_artifact_revision_uid,
            subject_digest: candidate.subject_digest,
            verdict: DeterministicVerdict::Pass,
            run_uid: prepared.candidate.id,
            trial_uids: vec![prepared.candidate.id],
            evidence_ids: learning_evidence_ids(prepared),
            gate_results: BTreeMap::from([
                ("held_in_suite".to_string(), "pass".to_string()),
                ("held_out_suite".to_string(), "pass".to_string()),
            ]),
            blocking_assertions: release_policy.blocking_assertions,
            evidence_adapter: EvidenceAdapter::SkillLearningRegression,
            decided_by: decided_by.clone(),
        },
        now,
    )
    .await
    .map_err(release_error)?;
    let attestation = decision.attestation.ok_or_else(|| {
        MoaError::ValidationError(
            "skill regression decision minted no activation attestation".to_string(),
        )
    })?;
    let observed_serving =
        ReleaseRepository::expected_serving_in_tx(conn, &scope, &activation_target)
            .await
            .map_err(release_error)?;
    if observed_serving != prepared.expected_serving {
        return Err(SkillReviewError::Conflict(format!(
            "serving skill changed after regression evaluation: expected {:?}, found {:?}",
            prepared.expected_serving, observed_serving
        )));
    }
    let activation = ReleaseRepository::activate_in_tx(
        conn,
        &ActivationRequest {
            scope,
            activation_target,
            candidate_revision_uid: prepared.draft_artifact_revision_uid,
            candidate_revision_hash: candidate.candidate_revision_hash,
            attestation_uid: attestation.attestation_uid,
            expected_serving: prepared.expected_serving,
            agent_revision_lock: None,
            actor: decided_by,
            reason: request.reason.clone(),
        },
        now,
    )
    .await
    .map_err(release_error)?;
    let revision = load_activated_revision(conn, prepared.draft_artifact_revision_uid).await?;
    Ok(ActivatedSkillRevision {
        revision,
        activation,
    })
}

/// Builds the release subject inputs a distilled skill promotion can prove.
fn learning_subject_inputs(
    prepared: &PreparedSkillAcceptance,
    regression_report: &Value,
) -> Result<CandidateSubjectInputs> {
    // Hashed from the serialized report bytes rather than canonical JSON: a
    // regression report carries measured rates, and the canonical form forbids
    // floating point. `serde_json::Map` is ordered, so the bytes are deterministic.
    let report_hash = Digest32(
        Sha256::digest(
            serde_json::to_vec(regression_report)
                .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        )
        .into(),
    );
    let package_hash = Digest32(Sha256::digest(&prepared.draft.canonical_hash).into());
    Ok(CandidateSubjectInputs {
        dependency_lock_hash: package_hash,
        agent_runtime: AgentRuntimeSubject {
            prompt_hash: package_hash,
            model: SKILL_LEARNING_MODEL_BINDING.to_string(),
            provider: SKILL_LEARNING_PROVIDER_BINDING.to_string(),
            runtime_policy_hash: package_hash,
        },
        tool_policy_hash: package_hash,
        tool_bearing: false,
        tool_catalog: None,
        plan: EvaluationPlanSubject {
            plan_hash: report_hash,
            scenario_dataset_hash: report_hash,
            seed_hash: report_hash,
            evaluator_versions: BTreeMap::from([(
                SKILL_LEARNING_EVALUATOR.to_string(),
                SKILL_LEARNING_EVALUATOR_VERSION.to_string(),
            )]),
        },
        simulator: None,
    })
}

/// Returns the sanitized learning evidence identifiers backing the decision.
fn learning_evidence_ids(prepared: &PreparedSkillAcceptance) -> Vec<Uuid> {
    let mut evidence_ids = vec![prepared.candidate.id];
    evidence_ids.extend(prepared.candidate.sources.iter().filter_map(|source| {
        match source {
            LearningCandidateSourceRef::Experience { experience_id } => Some(*experience_id),
            LearningCandidateSourceRef::Attribution { attribution_id } => Some(*attribution_id),
            LearningCandidateSourceRef::Event { event_id, .. } => Some(*event_id),
            LearningCandidateSourceRef::ArtifactRevision { revision_uid } => Some(*revision_uid),
            LearningCandidateSourceRef::ExperimentRun { run_uid } => Some(*run_uid),
            LearningCandidateSourceRef::ExperimentTrial { trial_uid } => Some(*trial_uid),
            LearningCandidateSourceRef::ScoreRun { run_id } => Some(*run_id),
            // Sessions, segments, contacts, and promotion candidates are not
            // evidence rows a release decision can cite.
            LearningCandidateSourceRef::Session { .. }
            | LearningCandidateSourceRef::TaskSegment { .. }
            | LearningCandidateSourceRef::Contact { .. }
            | LearningCandidateSourceRef::PromotionCandidate { .. } => None,
        }
    }));
    evidence_ids.dedup();
    evidence_ids
}

async fn load_activated_revision(
    conn: &mut PgConnection,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    Ok(ArtifactRegistry::load_revision_in_tx(conn, revision_uid).await?)
}

fn release_error(error: moa_artifacts::Error) -> MoaError {
    MoaError::ValidationError(error.to_string())
}

/// Evaluator identity recorded for the skill-learning evidence adapter.
const SKILL_LEARNING_EVALUATOR: &str = "skill_learning.regression_suite";
/// Version of that evaluator, part of every subject digest it produces.
const SKILL_LEARNING_EVALUATOR_VERSION: &str = "1.0.0";
/// Model binding recorded for a distilled-skill subject.
const SKILL_LEARNING_MODEL_BINDING: &str = "skill-learning-distillation";
/// Provider binding recorded for a distilled-skill subject.
const SKILL_LEARNING_PROVIDER_BINDING: &str = "moa-internal";

/// Publishes a claimed skill candidate and records promoted learning.
pub async fn promote_claimed_skill_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    pool: sqlx::PgPool,
    request: &SkillReviewRequest,
    prepared: PreparedSkillAcceptance,
    regression_report: Value,
    acceptance_checks: AcceptanceChecks,
    request_digest: &[u8],
) -> Result<SkillReviewOutcome> {
    // Reject before mutating any surface: promotion cannot publish unless both the held-in and
    // held-out checks passed.
    ensure_acceptance_checks(&acceptance_checks)?;
    let scope_context = artifact_scope_context(&prepared.scope);
    let mut conn = ScopedConn::begin(&pool, &scope_context).await?;
    // A distilled skill reaches serving through the same release decision
    // contract as a hand-authored one. The learning-specific regression result is
    // the evidence adapter, not the gate type: it supplies the deterministic
    // verdict and evidence identifiers, and the release repository still owns the
    // subject digest, the attestation, and the pointer compare-and-set.
    let activated =
        activate_promoted_skill_revision(conn.as_mut(), &prepared, request, &regression_report)
            .await?;
    let artifact_uid = Some(activated.revision.artifact_uid);
    let mut evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &prepared.candidate,
        artifact_uid,
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        activated_artifact_revision_uid: Some(activated.revision.revision_uid),
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
        accepted_learning_entry(&prepared.candidate, request, &activated, evaluation_payload)?;
    store
        .append_learning_in_tx(conn.as_mut(), &learning_entry)
        .await?;
    let outcome = SkillReviewOutcome {
        candidate_id: prepared.candidate.id,
        status: LearningCandidateStatus::Promoted,
        artifact_uid,
        draft_artifact_revision_uid: Some(prepared.draft_artifact_revision_uid),
        activated_artifact_revision_uid: Some(activated.revision.revision_uid),
    };
    record_terminal_decision_in_tx(
        store,
        conn.as_mut(),
        request,
        request_digest,
        moa_core::types::experience::LearningReviewDecision::AcceptedSkill,
        LearningCandidateStatus::Evaluating,
        &outcome,
    )
    .await?;
    conn.commit().await?;

    Ok(outcome)
}

async fn record_terminal_decision_in_tx(
    store: &(impl LearningReviewStore + ?Sized),
    conn: &mut PgConnection,
    request: &SkillReviewRequest,
    request_digest: &[u8],
    decision: moa_core::types::experience::LearningReviewDecision,
    from_status: LearningCandidateStatus,
    outcome: &SkillReviewOutcome,
) -> Result<()> {
    if request_digest.len() != 32 {
        return Err(SkillReviewError::Moa(MoaError::ValidationError(
            "learning review request digest must be 32 bytes".to_string(),
        )));
    }
    let outcome_json = serde_json::to_value(outcome)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let inserted = store
        .record_learning_candidate_decision_in_tx(
            conn,
            &moa_core::types::experience::LearningCandidateDecisionRecord {
                id: Uuid::now_v7(),
                candidate_id: request.candidate_id,
                tenant_id: request.tenant_id,
                decision,
                from_status,
                to_status: outcome.status,
                reviewer_subject: Some(request.reviewer_subject.clone()),
                reason: request.reason.clone(),
                request_digest: Some(request_digest.to_vec()),
                outcome: Some(outcome_json),
                decided_at: Utc::now(),
            },
        )
        .await?;
    if !inserted {
        return Err(SkillReviewError::Conflict(
            "learning review decision already exists".to_string(),
        ));
    }
    Ok(())
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
///
/// Rejection claims the proposal first. The state machine has no direct
/// `Proposed -> Rejected` edge for either reviewable kind — a rejection is a
/// decision about a proposal someone is holding, so the reviewer takes the claim
/// exactly as `accept_skill` does and only then records the outcome. Claiming is
/// also what makes concurrent review safe: two reviewers cannot both reject, and
/// a reviewer cannot reject a proposal another one is mid-accept on. If the
/// terminal write loses a race after the claim succeeded, the claim is released
/// so a transient conflict never strands the proposal in `Evaluating`.
pub async fn reject_learning_candidate(
    store: &(impl LearningReviewStore + ?Sized),
    pool: sqlx::PgPool,
    request: &SkillReviewRequest,
    request_digest: &[u8],
) -> Result<SkillReviewOutcome> {
    let prepared = prepare_skill_rejection(store, request).await?;
    let outcome = SkillReviewOutcome {
        candidate_id: prepared.candidate.id,
        status: LearningCandidateStatus::Rejected,
        artifact_uid: prepared.artifact_uid,
        draft_artifact_revision_uid: prepared.draft_artifact_revision_uid,
        activated_artifact_revision_uid: None,
    };
    let mut conn = match ScopedConn::begin(&pool, &RlsContext::tenant(request.tenant_id)).await {
        Ok(conn) => conn,
        Err(error) => {
            release_rejection_claim(store, prepared.candidate.id, &error.to_string()).await;
            return Err(error.into());
        }
    };
    let rejected = async {
        finish_claimed_candidate_review_in_tx(
            store,
            conn.as_mut(),
            LearningCandidateStatusUpdate {
                candidate_id: prepared.candidate.id,
                status: LearningCandidateStatus::Rejected,
                status_reason: Some(
                    request
                        .reason
                        .clone()
                        .unwrap_or_else(|| "rejected by reviewer".to_string()),
                ),
                evaluation_payload: Some(prepared.evaluation_payload),
                updated_at: Utc::now(),
            },
        )
        .await?;
        record_terminal_decision_in_tx(
            store,
            conn.as_mut(),
            request,
            request_digest,
            moa_core::types::experience::LearningReviewDecision::Rejected,
            LearningCandidateStatus::Evaluating,
            &outcome,
        )
        .await?;
        conn.commit().await?;
        Ok::<(), SkillReviewError>(())
    }
    .await;
    if let Err(error) = rejected {
        release_rejection_claim(store, prepared.candidate.id, &error.to_string()).await;
        return Err(error);
    }

    Ok(outcome)
}

async fn prepare_skill_rejection(
    store: &(impl LearningReviewStore + ?Sized),
    request: &SkillReviewRequest,
) -> Result<PreparedSkillRejection> {
    let candidate = load_candidate(store, &request.tenant_id, request.candidate_id).await?;
    ensure_reviewable_proposal(&candidate)?;
    ensure_proposed_candidate(&candidate)?;
    let artifact_uid = optional_payload_uuid(&candidate.payload, "artifact_uid")?;
    let draft_artifact_revision_uid =
        optional_payload_uuid(&candidate.payload, "draft_artifact_revision_uid")?;
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request,
        candidate: &candidate,
        artifact_uid,
        draft_artifact_revision_uid,
        activated_artifact_revision_uid: None,
        regression_report: None,
    });

    finish_proposed_candidate_review(
        store,
        LearningCandidateStatusUpdate {
            candidate_id: candidate.id,
            status: LearningCandidateStatus::Evaluating,
            status_reason: Some("claimed for rejection".to_string()),
            evaluation_payload: None,
            updated_at: Utc::now(),
        },
    )
    .await?;

    Ok(PreparedSkillRejection {
        candidate,
        artifact_uid,
        draft_artifact_revision_uid,
        evaluation_payload,
    })
}

async fn release_rejection_claim(
    store: &(impl LearningReviewStore + ?Sized),
    candidate_id: Uuid,
    reason: &str,
) {
    if let Err(release_error) = release_claimed_skill_candidate(store, candidate_id, reason).await {
        tracing::warn!(
            candidate_id = %candidate_id,
            error = %release_error,
            "failed to release rejection claim after a terminal write failure"
        );
    }
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

/// Refuses anything but a skill draft at the accept-skill entry point.
///
/// Gated on `proposal_kind`, not `candidate_type`: the target domain (a skill)
/// does not say whether a materializer exists for the proposal. A skill
/// suggestion with no draft behind it is also `candidate_type = Skill`, and
/// accepting one would run the publish path against a revision that was never
/// generated.
fn ensure_skill_draft_proposal(candidate: &LearningCandidate) -> Result<()> {
    if candidate.proposal_kind != LearningProposalKind::SkillDraft {
        return Err(bad_request(
            "only skill draft proposals can be accepted by this endpoint",
        ));
    }
    Ok(())
}

/// Refuses any kind that has no reviewable outcome at the reject entry point.
///
/// Both `SkillDraft` and `SkillRollback` are decisions a reviewer can decline.
/// Advisory and authoring items are not: they have no `Rejected` state at all,
/// so their only close is a dismissal.
fn ensure_reviewable_proposal(candidate: &LearningCandidate) -> Result<()> {
    if !candidate.proposal_kind.is_reviewable() {
        return Err(bad_request(
            "only reviewable proposals can be rejected; advisory and authoring items are dismissed",
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
    activated_artifact_revision_uid: Option<Uuid>,
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
        "activated_artifact_revision_uid": input.activated_artifact_revision_uid,
        "regression_execution": regression_execution,
        "regression_report": input.regression_report,
    })
}

fn accepted_learning_entry(
    candidate: &LearningCandidate,
    request: &SkillReviewRequest,
    activated: &ActivatedSkillRevision,
    evaluation_payload: Value,
) -> Result<LearningEntry> {
    let learning_type = accepted_learning_type(candidate)?;
    let revision = &activated.revision;
    Ok(LearningEntry {
        id: Uuid::now_v7(),
        tenant_id: candidate.tenant_id,
        learning_type,
        target_id: target_id(candidate, revision),
        target_label: target_label(candidate),
        payload: json!({
            "candidate_id": candidate.id,
            "reviewer_subject": request.reviewer_subject,
            "reason": request.reason,
            "artifact_uid": revision.artifact_uid,
            "activated_artifact_revision_uid": revision.revision_uid,
            "activation_audit_uid": activated.activation.audit_uid,
            "activated_pointer_version": activated.activation.pointer_version,
            "review": evaluation_payload,
        }),
        confidence: candidate.confidence,
        sources: learning_log_sources(candidate, revision),
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

/// Builds the typed provenance recorded on an accepted candidate's learning-log entry.
///
/// The entry inherits the candidate's own normalized sources rather than
/// re-deriving them from payload strings, plus the candidate itself and the
/// revision that was actually published. That keeps the derivation chain
/// continuous: erasing a source experience now reaches the candidate, and
/// through the candidate, this entry.
fn learning_log_sources(
    candidate: &LearningCandidate,
    published: &StoredArtifactRevision,
) -> Vec<LearningLogSourceRef> {
    let mut sources = vec![
        LearningLogSourceRef::Candidate {
            candidate_id: candidate.id,
        },
        LearningLogSourceRef::ArtifactRevision {
            revision_uid: published.revision_uid,
        },
    ];
    for source in &candidate.sources {
        match source {
            LearningCandidateSourceRef::Experience { experience_id } => {
                sources.push(LearningLogSourceRef::Experience {
                    experience_id: *experience_id,
                });
            }
            LearningCandidateSourceRef::Session { session_id } => {
                sources.push(LearningLogSourceRef::Session {
                    session_id: *session_id,
                });
            }
            LearningCandidateSourceRef::TaskSegment { segment_id } => {
                sources.push(LearningLogSourceRef::TaskSegment {
                    segment_id: *segment_id,
                });
            }
            // Attribution, event, contact, experiment, score, and revision
            // references have no learning-log source column: the log's own
            // referent set is deliberately narrower. They stay reachable through
            // the candidate reference above rather than being flattened here.
            _ => {}
        }
    }
    sources
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
    use moa_core::types::experience::{
        LearningCandidateType, LearningProposalKind, LearningRiskClass,
    };
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
        /// Every compare-and-set attempted, in order. Review paths take more than
        /// one, and the ORDER is the property under test: recording only the last
        /// would hide a rejection that skipped its claim.
        recorded_updates: Mutex<Vec<(LearningCandidateStatusUpdate, LearningCandidateStatus)>>,
    }

    impl RecordingReviewStore {
        fn new(candidate: Option<LearningCandidate>, status_change_applies: bool) -> Self {
            Self {
                candidate,
                status_change_applies,
                recorded_updates: Mutex::new(Vec::new()),
            }
        }

        fn updates(&self) -> Vec<(LearningCandidateStatusUpdate, LearningCandidateStatus)> {
            self.recorded_updates
                .lock()
                .expect("read recorded updates")
                .clone()
        }
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
            self.recorded_updates
                .lock()
                .expect("record candidate status update")
                .push((update.clone(), expected_status));
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

        async fn record_learning_candidate_decision_in_tx(
            &self,
            _conn: &mut PgConnection,
            _decision: &moa_core::types::experience::LearningCandidateDecisionRecord,
        ) -> std::result::Result<bool, MoaError> {
            unreachable!("decision recording is only used by the service-backed dismiss path")
        }
    }

    fn proposed_skill_candidate(payload: Value) -> LearningCandidate {
        LearningCandidate {
            id: Uuid::from_u128(42),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            proposal_kind: LearningProposalKind::SkillDraft,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: None,
            task_fingerprint: None,
            task_facets: None,
            payload,
            evaluation_payload: None,
            sources: vec![LearningCandidateSourceRef::Experience {
                experience_id: Uuid::now_v7(),
            }],
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
    async fn prepare_skill_rejection_claims_before_the_terminal_transaction() {
        // Pins: rejection claims Proposed -> Evaluating before the database-backed
        // terminal transaction, and keeps the linked draft/artifact references.
        //
        // There is no direct Proposed -> Rejected edge for either reviewable kind, and the
        // database enforces that with a trigger. A reject that compare-and-set straight from
        // Proposed compiled, passed every unit test against a fake store, and failed only on
        // a live UPDATE — which is exactly what it did before this test existed.
        let draft_uid = Uuid::from_u128(77);
        let artifact_uid = Uuid::from_u128(88);
        let store = RecordingReviewStore::new(
            Some(proposed_skill_candidate(json!({
                "draft_artifact_revision_uid": draft_uid.to_string(),
                "artifact_uid": artifact_uid.to_string(),
            }))),
            true,
        );
        let request = reject_request(Some("not reusable"));

        let prepared = prepare_skill_rejection(&store, &request)
            .await
            .expect("rejection preparation succeeds");

        assert_eq!(prepared.candidate.id, Uuid::from_u128(42));
        assert_eq!(prepared.draft_artifact_revision_uid, Some(draft_uid));
        assert_eq!(prepared.artifact_uid, Some(artifact_uid));

        let recorded = store.updates();
        assert_eq!(recorded.len(), 1, "preparation only claims the proposal");
        assert_eq!(recorded[0].1, LearningCandidateStatus::Proposed);
        assert_eq!(recorded[0].0.status, LearningCandidateStatus::Evaluating);
    }

    #[tokio::test]
    async fn reject_refuses_a_kind_that_has_no_rejected_state() {
        // Pins: an advisory or authoring item cannot be rejected. Its state machine has no
        // Rejected state at all, so a reject that reached the database would fail as a
        // constraint violation; refusing at the handler keeps it a 400 the reviewer can act
        // on, and keeps dismissal the only way to close an informational item.
        let mut candidate = proposed_skill_candidate(json!({}));
        candidate.proposal_kind = LearningProposalKind::SkillAuthoring;
        candidate.status = LearningCandidateStatus::NeedsAuthoring;
        let store = RecordingReviewStore::new(Some(candidate), true);

        let error = prepare_skill_rejection(&store, &reject_request(None))
            .await
            .expect_err("an authoring item has no rejected state");

        // On the REASON, not merely on the variant. `ensure_proposed_candidate` also
        // returns `BadRequest` for this fixture, so matching the variant alone would
        // pass with no kind check at all — and the kind is the property under test.
        let SkillReviewError::BadRequest(message) = &error else {
            panic!("expected BadRequest, got {error:?}");
        };
        assert!(
            message.contains("only reviewable proposals can be rejected"),
            "refusal must name the kind, not the status: {message}"
        );
        assert!(
            store.updates().is_empty(),
            "the kind guard fires before any status write is attempted"
        );
    }

    #[tokio::test]
    async fn reject_learning_candidate_conflicts_when_status_changed() {
        // Pins: a compare-and-set miss (another writer moved the candidate first) surfaces as a
        // Conflict instead of silently succeeding.
        let store = RecordingReviewStore::new(Some(proposed_skill_candidate(json!({}))), false);
        let request = reject_request(None);

        let error = prepare_skill_rejection(&store, &request)
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
        let store = RecordingReviewStore::new(Some(candidate), true);
        let request = reject_request(None);

        let error = prepare_skill_rejection(&store, &request)
            .await
            .expect_err("a promoted candidate cannot be rejected");

        assert!(
            matches!(error, SkillReviewError::BadRequest(_)),
            "expected BadRequest, got {error:?}"
        );
        assert!(
            store.updates().is_empty(),
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
            proposal_kind: LearningProposalKind::SkillDraft,
            status: LearningCandidateStatus::Proposed,
            target_id: None,
            target_label: None,
            task_fingerprint: None,
            task_facets: None,
            payload: json!({"operation": "unknown_operation"}),
            evaluation_payload: None,
            sources: vec![LearningCandidateSourceRef::Experience {
                experience_id: Uuid::now_v7(),
            }],
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

//! Restate service for human-reviewed learning candidate promotion.

use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::validate_for_status;
use moa_authz::{fga_subject, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::wire::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
    LearningCandidateReviewResponse,
};
use moa_core::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningEntry, MemoryScope, MoaError, WorkspaceId,
};
use moa_session::PostgresSessionStore;
use moa_skills::registry::SkillRegistry;
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::skill_regression::skill_acceptance_regression_report;

/// Restate service surface for protected learning-candidate review.
#[restate_sdk::service]
#[name = "LearningReview"]
pub trait LearningReview {
    /// Loads one reviewable learning candidate.
    async fn get(
        request: Json<GetLearningCandidateRequest>,
    ) -> Result<Json<LearningCandidate>, HandlerError>;

    /// Accepts a proposed skill candidate and materializes its draft artifact.
    async fn accept_skill(
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError>;

    /// Rejects a proposed learning candidate while preserving its draft artifacts.
    async fn reject(
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError>;
}

/// Concrete learning-review service implementation.
#[derive(Clone, Default)]
pub struct LearningReviewImpl;

impl LearningReview for LearningReviewImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn get(
        &self,
        ctx: Context<'_>,
        request: Json<GetLearningCandidateRequest>,
    ) -> Result<Json<LearningCandidate>, HandlerError> {
        annotate_restate_handler_span("LearningReview", "get");
        let request = request.into_inner();
        authorize_workspace_editor(&ctx, &request.workspace_id).await?;
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                get_learning_candidate_after_authz(store, request)
                    .await
                    .map(Json::from)
            })
            .name("learning_review_get")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn accept_skill(
        &self,
        ctx: Context<'_>,
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError> {
        annotate_restate_handler_span("LearningReview", "accept_skill");
        let mut request = request.into_inner();
        let identity = authorize_workspace_editor(&ctx, &request.workspace_id).await?;
        request.reviewer_subject = fga_subject(&identity);
        let runtime = OrchestratorCtx::current();
        let store = runtime.session_store.clone();
        let config = runtime.config.clone();
        #[cfg(feature = "internal-eval-runner")]
        let providers = runtime.providers.clone();

        Ok(ctx
            .run(|| async move {
                accept_skill_candidate_after_authz(
                    store,
                    config,
                    #[cfg(feature = "internal-eval-runner")]
                    providers,
                    request,
                )
                .await
                .map(Json::from)
            })
            .name("learning_review_accept_skill")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn reject(
        &self,
        ctx: Context<'_>,
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError> {
        annotate_restate_handler_span("LearningReview", "reject");
        let mut request = request.into_inner();
        let identity = authorize_workspace_editor(&ctx, &request.workspace_id).await?;
        request.reviewer_subject = fga_subject(&identity);
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                reject_learning_candidate_after_authz(store, request)
                    .await
                    .map(Json::from)
            })
            .name("learning_review_reject")
            .await?)
    }
}

/// Loads one candidate after the caller has authorized workspace editor access.
pub async fn get_learning_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    request: GetLearningCandidateRequest,
) -> Result<LearningCandidate, HandlerError> {
    load_candidate(&store, &request.workspace_id, request.candidate_id).await
}

/// Accepts one skill candidate after the caller has authorized workspace editor access.
pub async fn accept_skill_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    config: Arc<moa_core::MoaConfig>,
    #[cfg(feature = "internal-eval-runner")] providers: Arc<
        crate::services::llm_gateway::ProviderRegistry,
    >,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Accept)?;
    let candidate = load_candidate(&store, &request.workspace_id, request.candidate_id).await?;
    ensure_skill_candidate(&candidate)?;
    ensure_proposed_candidate(&candidate)?;
    let draft_revision_uid = payload_uuid(&candidate.payload, "draft_artifact_revision_uid")?;

    let scope = workspace_scope(&request.workspace_id);
    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let draft = artifact_registry
        .load_revision(&scope, draft_revision_uid)
        .await
        .map_err(moa_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "draft artifact revision not found"))?;
    ensure_workspace_skill_draft(&draft, &request.workspace_id)?;

    let mut document = draft.document.clone();
    document.reference_resolutions =
        ArtifactResolver::new(ArtifactRegistry::new(store.pool().clone()))
            .resolve_document(&scope, &document)
            .await
            .map_err(moa_handler_error)?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        return Err(TerminalError::new_with_code(
            400,
            "skill draft artifact revision is not publishable",
        )
        .into());
    }

    let draft_files = artifact_registry
        .load_files(&scope, draft_revision_uid)
        .await
        .map_err(moa_handler_error)?;
    claim_candidate_for_acceptance(&store, candidate.id).await?;
    let regression_gate = skill_acceptance_regression_report(
        config.as_ref().clone(),
        #[cfg(feature = "internal-eval-runner")]
        providers,
        SkillRegistry::new(store.pool().clone()),
        scope.clone(),
        candidate.clone(),
        draft_files.clone(),
    )
    .await
    .map_err(moa_handler_error)?;
    if !regression_gate.allow_promotion {
        let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
            request: &request,
            candidate: &candidate,
            artifact_uid: Some(draft.artifact_uid),
            draft_artifact_revision_uid: Some(draft_revision_uid),
            published_artifact_revision_uid: None,
            skill_uid: None,
            regression_report: Some(regression_gate.report),
        })?;
        finish_claimed_candidate_review(
            &store,
            LearningCandidateStatusUpdate {
                candidate_id: candidate.id,
                status: LearningCandidateStatus::Rejected,
                status_reason: Some(
                    regression_gate.rejection_reason.unwrap_or_else(|| {
                        "skill regression rejected the proposed draft".to_string()
                    }),
                ),
                evaluation_payload: Some(evaluation_payload),
                updated_at: Utc::now(),
            },
        )
        .await?;

        return Ok(LearningCandidateReviewResponse {
            candidate_id: candidate.id,
            status: LearningCandidateStatus::Rejected,
            artifact_uid: Some(draft.artifact_uid),
            draft_artifact_revision_uid: Some(draft_revision_uid),
            published_artifact_revision_uid: None,
            skill_uid: None,
        });
    }

    let published = artifact_registry
        .publish_revision(&scope, draft_revision_uid, &report)
        .await
        .map_err(moa_handler_error)?;
    let skill_registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = skill_registry
        .materialize_published_artifact_revision(&scope, &published, draft_files)
        .await
        .map_err(moa_handler_error)?;
    let artifact_uid = Some(published.artifact_uid);
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request: &request,
        candidate: &candidate,
        artifact_uid,
        draft_artifact_revision_uid: Some(draft_revision_uid),
        published_artifact_revision_uid: Some(published.revision_uid),
        skill_uid: Some(skill_uid),
        regression_report: Some(regression_gate.report),
    })?;

    finish_claimed_candidate_review(
        &store,
        LearningCandidateStatusUpdate {
            candidate_id: candidate.id,
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
    store
        .append_learning(&accepted_learning_entry(
            &candidate,
            &request,
            skill_uid,
            &published,
            evaluation_payload,
        )?)
        .await
        .map_err(moa_handler_error)?;

    Ok(LearningCandidateReviewResponse {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Promoted,
        artifact_uid,
        draft_artifact_revision_uid: Some(draft_revision_uid),
        published_artifact_revision_uid: Some(published.revision_uid),
        skill_uid: Some(skill_uid),
    })
}

/// Rejects one candidate after the caller has authorized workspace editor access.
pub async fn reject_learning_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Reject)?;
    let candidate = load_candidate(&store, &request.workspace_id, request.candidate_id).await?;
    ensure_proposed_candidate(&candidate)?;
    let artifact_uid = optional_payload_uuid(&candidate.payload, "artifact_uid")?;
    let draft_artifact_revision_uid =
        optional_payload_uuid(&candidate.payload, "draft_artifact_revision_uid")?;
    let evaluation_payload = review_evaluation_payload(ReviewEvaluationPayload {
        request: &request,
        candidate: &candidate,
        artifact_uid,
        draft_artifact_revision_uid,
        published_artifact_revision_uid: None,
        skill_uid: None,
        regression_report: None,
    })?;

    finish_proposed_candidate_review(
        &store,
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

    Ok(LearningCandidateReviewResponse {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Rejected,
        artifact_uid,
        draft_artifact_revision_uid,
        published_artifact_revision_uid: None,
        skill_uid: None,
    })
}

async fn authorize_workspace_editor(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Editor,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

async fn load_candidate(
    store: &PostgresSessionStore,
    workspace_id: &WorkspaceId,
    candidate_id: Uuid,
) -> Result<LearningCandidate, HandlerError> {
    store
        .get_learning_candidate(workspace_id, candidate_id)
        .await
        .map_err(moa_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "learning candidate not found").into())
}

fn workspace_scope(workspace_id: &WorkspaceId) -> MemoryScope {
    MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    }
}

fn ensure_requested_action(
    actual: LearningCandidateReviewAction,
    expected: LearningCandidateReviewAction,
) -> Result<(), HandlerError> {
    if actual != expected {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "review action must be {} for this endpoint",
                review_action_label(expected)
            ),
        )
        .into());
    }
    Ok(())
}

fn ensure_skill_candidate(candidate: &LearningCandidate) -> Result<(), HandlerError> {
    if candidate.candidate_type != LearningCandidateType::Skill {
        return Err(TerminalError::new_with_code(
            400,
            "only skill learning candidates can be accepted by this endpoint",
        )
        .into());
    }
    Ok(())
}

fn ensure_proposed_candidate(candidate: &LearningCandidate) -> Result<(), HandlerError> {
    if candidate.status != LearningCandidateStatus::Proposed {
        return Err(TerminalError::new_with_code(
            400,
            "learning candidate must be proposed before review",
        )
        .into());
    }
    Ok(())
}

fn ensure_workspace_skill_draft(
    revision: &StoredArtifactRevision,
    workspace_id: &WorkspaceId,
) -> Result<(), HandlerError> {
    if revision.kind != ArtifactKind::Skill {
        return Err(TerminalError::new_with_code(
            400,
            "draft artifact revision must be a skill artifact",
        )
        .into());
    }
    if revision.status != ArtifactStatus::Draft {
        return Err(TerminalError::new_with_code(
            400,
            "skill artifact revision must still be a draft",
        )
        .into());
    }
    if revision.workspace_id.as_ref() != Some(workspace_id) || revision.user_id.is_some() {
        return Err(TerminalError::new_with_code(
            400,
            "skill draft artifact revision must belong to the requested workspace scope",
        )
        .into());
    }
    Ok(())
}

async fn claim_candidate_for_acceptance(
    store: &PostgresSessionStore,
    candidate_id: Uuid,
) -> Result<(), HandlerError> {
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
    store: &PostgresSessionStore,
    update: LearningCandidateStatusUpdate,
) -> Result<(), HandlerError> {
    finish_candidate_review_from(store, update, LearningCandidateStatus::Proposed).await
}

async fn finish_claimed_candidate_review(
    store: &PostgresSessionStore,
    update: LearningCandidateStatusUpdate,
) -> Result<(), HandlerError> {
    finish_candidate_review_from(store, update, LearningCandidateStatus::Evaluating).await
}

async fn finish_candidate_review_from(
    store: &PostgresSessionStore,
    update: LearningCandidateStatusUpdate,
    expected_status: LearningCandidateStatus,
) -> Result<(), HandlerError> {
    let changed = store
        .update_learning_candidate_status_from(&update, expected_status)
        .await
        .map_err(moa_handler_error)?;
    if changed {
        return Ok(());
    }
    Err(TerminalError::new_with_code(
        409,
        "learning candidate changed status before review could be applied",
    )
    .into())
}

fn payload_uuid(payload: &Value, key: &str) -> Result<Uuid, HandlerError> {
    let value = payload.get(key).and_then(Value::as_str).ok_or_else(|| {
        TerminalError::new_with_code(400, format!("candidate payload missing {key}"))
    })?;
    Uuid::parse_str(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("candidate payload {key} is invalid: {error}"))
            .into()
    })
}

fn optional_payload_uuid(payload: &Value, key: &str) -> Result<Option<Uuid>, HandlerError> {
    let Some(value) = payload.get(key).and_then(Value::as_str) else {
        return Ok(None);
    };
    Uuid::parse_str(value).map(Some).map_err(|error| {
        TerminalError::new_with_code(400, format!("candidate payload {key} is invalid: {error}"))
            .into()
    })
}

struct ReviewEvaluationPayload<'a> {
    request: &'a LearningCandidateReviewRequest,
    candidate: &'a LearningCandidate,
    artifact_uid: Option<Uuid>,
    draft_artifact_revision_uid: Option<Uuid>,
    published_artifact_revision_uid: Option<Uuid>,
    skill_uid: Option<Uuid>,
    regression_report: Option<Value>,
}

fn review_evaluation_payload(input: ReviewEvaluationPayload<'_>) -> Result<Value, HandlerError> {
    let regression_execution = input
        .regression_report
        .as_ref()
        .and_then(|report| report.get("regression_execution"))
        .cloned();
    Ok(json!({
        "reviewer_subject": input.request.reviewer_subject.clone(),
        "action": review_action_label(input.request.action),
        "reason": input.request.reason.clone(),
        "candidate_id": input.candidate.id,
        "artifact_uid": input.artifact_uid,
        "draft_artifact_revision_uid": input.draft_artifact_revision_uid,
        "published_artifact_revision_uid": input.published_artifact_revision_uid,
        "skill_uid": input.skill_uid,
        "regression_execution": regression_execution,
        "regression_report": input.regression_report,
    }))
}

fn accepted_learning_entry(
    candidate: &LearningCandidate,
    request: &LearningCandidateReviewRequest,
    skill_uid: Uuid,
    published: &StoredArtifactRevision,
    evaluation_payload: Value,
) -> Result<LearningEntry, HandlerError> {
    let learning_type = accepted_learning_type(candidate)?;
    Ok(LearningEntry {
        id: Uuid::now_v7(),
        tenant_id: candidate.tenant_id.clone(),
        learning_type,
        target_id: target_id(candidate, skill_uid),
        target_label: target_label(candidate),
        payload: json!({
            "candidate_id": candidate.id,
            "reviewer_subject": request.reviewer_subject,
            "reason": request.reason,
            "artifact_uid": published.artifact_uid,
            "published_artifact_revision_uid": published.revision_uid,
            "skill_uid": skill_uid,
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

fn accepted_learning_type(candidate: &LearningCandidate) -> Result<String, HandlerError> {
    match candidate.payload.get("operation").and_then(Value::as_str) {
        Some("skill_created") => Ok("skill_created".to_string()),
        Some("skill_improved") => Ok("skill_improved".to_string()),
        Some(other) => Err(TerminalError::new_with_code(
            400,
            format!("unsupported skill proposal operation `{other}`"),
        )
        .into()),
        None => {
            Err(TerminalError::new_with_code(400, "candidate payload missing operation").into())
        }
    }
}

fn target_id(candidate: &LearningCandidate, skill_uid: Uuid) -> String {
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
        .unwrap_or_else(|| format!("skill:{skill_uid}"))
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

fn review_action_label(action: LearningCandidateReviewAction) -> &'static str {
    match action {
        LearningCandidateReviewAction::Accept => "accept",
        LearningCandidateReviewAction::Reject => "reject",
    }
}

fn moa_handler_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::ValidationError(_) | MoaError::SerializationError(_) | MoaError::Uuid(_) => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        MoaError::Unsupported(_) | MoaError::NotImplemented(_) => {
            TerminalError::new_with_code(501, error.to_string()).into()
        }
        other if other.is_fatal() => TerminalError::new(other.to_string()).into(),
        other => HandlerError::from(other),
    }
}

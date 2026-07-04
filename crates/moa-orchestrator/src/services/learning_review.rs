//! Restate service for human-reviewed learning candidate promotion.

use std::sync::Arc;

use moa_authz::fga_subject;
use moa_authz_schema::Relation;
use moa_core::wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
    LearningCandidateReviewResponse,
};
use moa_core::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate, TenantId,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use moa_skills::registry::SkillRegistry;
use moa_skills::review::{
    LearningReviewStore, SkillReviewAction, SkillReviewError, SkillReviewOutcome,
    SkillReviewRequest, get_learning_candidate_for_review, prepare_skill_acceptance,
    promote_claimed_skill_candidate, reject_claimed_skill_candidate, reject_learning_candidate,
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::authorize_tenant;
use crate::services::skill_regression::skill_acceptance_regression_report;
use crate::workflows::errors::moa_error_to_status_handler_error;

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
        authorize_tenant_operator(&ctx, request.tenant_id).await?;
        let store = OrchestratorCtx::current().session_store_backend();

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
        let identity = authorize_tenant_operator(&ctx, request.tenant_id).await?;
        request.reviewer_subject = fga_subject(&identity);
        let runtime = OrchestratorCtx::current();
        let store = runtime.session_store_backend();
        let pool = runtime.graph_pool();
        let config = runtime.config();
        let providers = runtime.provider_registry();

        let response = ctx
            .run(move || async move {
                accept_skill_candidate_after_authz(store, pool, config, providers, request)
                    .await
                    .map(Json::from)
            })
            .name("learning_review_accept_skill")
            .await?;

        Ok(response)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn reject(
        &self,
        ctx: Context<'_>,
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError> {
        annotate_restate_handler_span("LearningReview", "reject");
        let mut request = request.into_inner();
        let identity = authorize_tenant_operator(&ctx, request.tenant_id).await?;
        request.reviewer_subject = fga_subject(&identity);
        let store = OrchestratorCtx::current().session_store_backend();

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

#[derive(Clone)]
struct SessionLearningReviewStore {
    store: Arc<PostgresSessionStore>,
}

impl SessionLearningReviewStore {
    fn new(store: Arc<PostgresSessionStore>) -> Self {
        Self { store }
    }
}

impl LearningReviewStore for SessionLearningReviewStore {
    fn get_learning_candidate<'a>(
        &'a self,
        tenant_id: &'a TenantId,
        candidate_id: Uuid,
    ) -> impl std::future::Future<
        Output = std::result::Result<Option<LearningCandidate>, moa_core::MoaError>,
    > + Send
    + 'a {
        async move {
            self.store
                .get_learning_candidate(tenant_id, candidate_id)
                .await
        }
    }

    fn update_learning_candidate_status_from<'a>(
        &'a self,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> impl std::future::Future<Output = std::result::Result<bool, moa_core::MoaError>> + Send + 'a
    {
        async move {
            self.store
                .update_learning_candidate_status_from(update, expected_status)
                .await
        }
    }

    fn update_learning_candidate_status_from_in_tx<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        update: &'a LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> impl std::future::Future<Output = std::result::Result<bool, moa_core::MoaError>> + Send + 'a
    {
        async move {
            self.store
                .update_learning_candidate_status_from_in_tx(conn, update, expected_status)
                .await
        }
    }

    fn append_learning_in_tx<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        entry: &'a moa_core::LearningEntry,
    ) -> impl std::future::Future<Output = std::result::Result<(), moa_core::MoaError>> + Send + 'a
    {
        async move { self.store.append_learning_in_tx(conn, entry).await }
    }
}

/// Loads one candidate after the caller has authorized tenant operator access.
pub async fn get_learning_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    request: GetLearningCandidateRequest,
) -> Result<LearningCandidate, HandlerError> {
    let review_store = SessionLearningReviewStore::new(store);
    get_learning_candidate_for_review(&review_store, &request.tenant_id, request.candidate_id)
        .await
        .map_err(skill_review_error_to_handler_error)
}

/// Accepts one skill candidate after the caller has authorized tenant operator access.
pub async fn accept_skill_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    pool: sqlx::PgPool,
    config: Arc<moa_core::MoaConfig>,
    providers: Arc<moa_providers::ProviderRegistry>,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Accept)?;
    let review_request = skill_review_request(&request, SkillReviewAction::Accept);
    let review_store = SessionLearningReviewStore::new(store.clone());
    let prepared = prepare_skill_acceptance(&review_store, pool.clone(), &review_request)
        .await
        .map_err(skill_review_error_to_handler_error)?;
    let regression_gate = skill_acceptance_regression_report(
        config.as_ref().clone(),
        providers,
        SkillRegistry::new(pool.clone()),
        prepared.scope,
        prepared.candidate.clone(),
        prepared.draft_files.clone(),
    )
    .await
    .map_err(moa_error_to_status_handler_error)?;
    if !regression_gate.allow_promotion {
        let outcome = reject_claimed_skill_candidate(
            &review_store,
            &review_request,
            &prepared,
            regression_gate.report,
            regression_gate.rejection_reason,
        )
        .await
        .map_err(skill_review_error_to_handler_error)?;

        return Ok(review_response_from_outcome(outcome));
    }

    let outcome = promote_claimed_skill_candidate(
        &review_store,
        pool,
        &review_request,
        prepared,
        regression_gate.report,
    )
    .await
    .map_err(skill_review_error_to_handler_error)?;

    Ok(review_response_from_outcome(outcome))
}

/// Rejects one candidate after the caller has authorized tenant operator access.
pub async fn reject_learning_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Reject)?;
    let review_store = SessionLearningReviewStore::new(store);
    let review_request = skill_review_request(&request, SkillReviewAction::Reject);
    let outcome = reject_learning_candidate(&review_store, &review_request)
        .await
        .map_err(skill_review_error_to_handler_error)?;

    Ok(review_response_from_outcome(outcome))
}

async fn authorize_tenant_operator(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    authorize_tenant(ctx, tenant_id, Relation::Operator).await
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

fn skill_review_request(
    request: &LearningCandidateReviewRequest,
    action: SkillReviewAction,
) -> SkillReviewRequest {
    SkillReviewRequest {
        tenant_id: request.tenant_id,
        candidate_id: request.candidate_id,
        action,
        reviewer_subject: request.reviewer_subject.clone(),
        reason: request.reason.clone(),
    }
}

fn review_response_from_outcome(outcome: SkillReviewOutcome) -> LearningCandidateReviewResponse {
    LearningCandidateReviewResponse {
        candidate_id: outcome.candidate_id,
        status: outcome.status,
        artifact_uid: outcome.artifact_uid,
        draft_artifact_revision_uid: outcome.draft_artifact_revision_uid,
        published_artifact_revision_uid: outcome.published_artifact_revision_uid,
    }
}

fn review_action_label(action: LearningCandidateReviewAction) -> &'static str {
    match action {
        LearningCandidateReviewAction::Accept => "accept",
        LearningCandidateReviewAction::Reject => "reject",
    }
}

fn skill_review_error_to_handler_error(error: SkillReviewError) -> HandlerError {
    match error {
        SkillReviewError::BadRequest(message) => TerminalError::new_with_code(400, message).into(),
        SkillReviewError::NotFound(message) => TerminalError::new_with_code(404, message).into(),
        SkillReviewError::Conflict(message) => TerminalError::new_with_code(409, message).into(),
        SkillReviewError::Moa(error) => moa_error_to_status_handler_error(error),
    }
}

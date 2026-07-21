//! Restate service for human-reviewed learning candidate promotion.

use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::registry::ArtifactRegistry;
use moa_authz::fga_subject;
use moa_authz_schema::Relation;
use moa_config::MoaConfig;
use moa_core::types::memory::RlsContext;
use moa_core::{
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateStatusUpdate, types::experience::LearningCandidateType,
    types::identifiers::TenantId, types::learning::LearningEntry,
};
use moa_db::ScopedConn;
use moa_hands::ToolRouter;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use moa_skills::registry::SkillRegistry;
use moa_skills::review::{
    AcceptanceChecks, LearningReviewStore, SkillReviewAction, SkillReviewError, SkillReviewOutcome,
    SkillReviewRequest, get_learning_candidate_for_review, prepare_skill_acceptance,
    promote_claimed_skill_candidate, reject_claimed_skill_candidate, reject_learning_candidate,
    release_claimed_skill_candidate,
};
use moa_wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
    LearningCandidateReviewResponse,
};
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::authorize_tenant;
use crate::services::skill_regression::{
    SkillRegressionExecution, SkillRegressionGate, skill_acceptance_regression_report,
};
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

    /// Accepts a proposed rollback proposal, archiving the regressed revision.
    async fn accept_rollback(
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError>;

    /// Rejects a proposed learning candidate while preserving its draft artifacts.
    async fn reject(
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError>;
}

/// Concrete learning-review service implementation.
#[derive(Clone)]
pub struct LearningReviewImpl {
    store: Arc<PostgresSessionStore>,
    pool: sqlx::PgPool,
    config: Arc<MoaConfig>,
    providers: Arc<ProviderRegistry>,
    router: Arc<ToolRouter>,
}

impl LearningReviewImpl {
    /// Creates learning review with its transactional store and regression dependencies.
    #[must_use]
    pub fn new(
        store: Arc<PostgresSessionStore>,
        pool: sqlx::PgPool,
        config: Arc<MoaConfig>,
        providers: Arc<ProviderRegistry>,
        router: Arc<ToolRouter>,
    ) -> Self {
        Self {
            store,
            pool,
            config,
            providers,
            router,
        }
    }
}

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
        let store = self.store.clone();

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
        let store = self.store.clone();
        let pool = self.pool.clone();
        let config = self.config.clone();
        let providers = self.providers.clone();
        let router = self.router.clone();

        let response = ctx
            .run(move || async move {
                accept_skill_candidate_after_authz(store, pool, config, providers, router, request)
                    .await
                    .map(Json::from)
            })
            .name("learning_review_accept_skill")
            .await?;

        Ok(response)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn accept_rollback(
        &self,
        ctx: Context<'_>,
        request: Json<LearningCandidateReviewRequest>,
    ) -> Result<Json<LearningCandidateReviewResponse>, HandlerError> {
        annotate_restate_handler_span("LearningReview", "accept_rollback");
        let mut request = request.into_inner();
        let identity = authorize_tenant_operator(&ctx, request.tenant_id).await?;
        request.reviewer_subject = fga_subject(&identity);
        let store = self.store.clone();
        let pool = self.pool.clone();

        let response = ctx
            .run(move || async move {
                accept_rollback_candidate_after_authz(store, pool, request)
                    .await
                    .map(Json::from)
            })
            .name("learning_review_accept_rollback")
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
        let store = self.store.clone();

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
    expected_compile_operation_key: Option<String>,
}

impl SessionLearningReviewStore {
    fn new(store: Arc<PostgresSessionStore>) -> Self {
        Self {
            store,
            expected_compile_operation_key: None,
        }
    }

    fn expect_compile_operation_key(&mut self, operation_key: Option<String>) {
        self.expected_compile_operation_key = operation_key;
    }
}

impl LearningReviewStore for SessionLearningReviewStore {
    async fn get_learning_candidate(
        &self,
        tenant_id: &TenantId,
        candidate_id: Uuid,
    ) -> std::result::Result<Option<LearningCandidate>, moa_core::error::MoaError> {
        self.store
            .get_learning_candidate(tenant_id, candidate_id)
            .await
    }

    async fn update_learning_candidate_status_from(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> std::result::Result<bool, moa_core::error::MoaError> {
        if matches!(
            update.status,
            LearningCandidateStatus::Promoted | LearningCandidateStatus::Rejected
        ) {
            self.store
                .finalize_learning_candidate_status_from(
                    update,
                    expected_status,
                    self.expected_compile_operation_key.as_deref(),
                )
                .await
        } else {
            self.store
                .update_learning_candidate_status_from(update, expected_status)
                .await
        }
    }

    async fn update_learning_candidate_status_from_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> std::result::Result<bool, moa_core::error::MoaError> {
        if matches!(
            update.status,
            LearningCandidateStatus::Promoted | LearningCandidateStatus::Rejected
        ) {
            self.store
                .finalize_learning_candidate_status_from_in_tx(
                    conn,
                    update,
                    expected_status,
                    self.expected_compile_operation_key.as_deref(),
                )
                .await
        } else {
            self.store
                .update_learning_candidate_status_from_in_tx(conn, update, expected_status)
                .await
        }
    }

    async fn append_learning_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        entry: &moa_core::types::learning::LearningEntry,
    ) -> std::result::Result<(), moa_core::error::MoaError> {
        self.store.append_learning_in_tx(conn, entry).await
    }
}

/// Records one skill-learning review decision.
///
/// Best-effort telemetry: recording never changes the review result.
fn record_review_decision(action: &str, outcome: &str) {
    moa_observability::runtime_metrics::record_skill_learning_review_decision(action, outcome);
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
    config: Arc<moa_config::MoaConfig>,
    providers: Arc<moa_providers::ProviderRegistry>,
    router: Arc<ToolRouter>,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Accept)?;
    let review_request = skill_review_request(&request, SkillReviewAction::Accept);
    let mut review_store = SessionLearningReviewStore::new(store.clone());
    let prepared = prepare_skill_acceptance(&review_store, pool.clone(), &review_request)
        .await
        .map_err(skill_review_error_to_handler_error)?;
    let regression_gate = match skill_acceptance_regression_report(
        config.as_ref().clone(),
        providers,
        SkillRegistry::new(pool.clone()),
        store,
        prepared.scope,
        prepared.candidate.clone(),
        crate::services::skill_regression::SkillRegressionCompileContext {
            router,
            draft: prepared.draft.clone(),
            draft_files: prepared.draft_files.clone(),
        },
    )
    .await
    {
        Ok(gate) => gate,
        Err(error) => {
            // Operational gate failure: release the Evaluating claim so the
            // accept can be retried once the deployment is fixed.
            if let Err(release_error) = release_claimed_skill_candidate(
                &review_store,
                prepared.candidate.id,
                &error.to_string(),
            )
            .await
            {
                tracing::warn!(
                    candidate_id = %prepared.candidate.id,
                    error = %release_error,
                    "failed to release claimed skill candidate after gate error"
                );
            }
            record_review_decision("accept_skill", "error");
            return Err(moa_error_to_status_handler_error(error));
        }
    };
    review_store.expect_compile_operation_key(regression_gate.compile_operation_key.clone());
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

        record_review_decision("accept_skill", "gate_rejected");
        return Ok(review_response_from_outcome(outcome));
    }

    let acceptance_checks = acceptance_checks_for_gate(&regression_gate);
    let outcome = promote_claimed_skill_candidate(
        &review_store,
        pool,
        &review_request,
        prepared,
        regression_gate.report,
        acceptance_checks,
    )
    .await
    .map_err(skill_review_error_to_handler_error)?;

    record_review_decision("accept_skill", "promoted");
    Ok(review_response_from_outcome(outcome))
}

/// Derives promotion acceptance checks from what the regression gate actually executed.
///
/// The descriptions become part of the candidate's permanent evaluation payload,
/// so they must state what ran — not what an ideal gate would have run. Promotion
/// is only reached when the gate allowed it, but the booleans are still derived
/// (not asserted) so a future gate outcome that neither blocks nor executes
/// cannot silently promote.
fn acceptance_checks_for_gate(gate: &SkillRegressionGate) -> AcceptanceChecks {
    let executed = gate.allow_promotion
        && matches!(
            gate.execution,
            SkillRegressionExecution::ComparedWithPrevious
                | SkillRegressionExecution::CandidateOnly
        );
    let held_out_description = match gate.execution {
        SkillRegressionExecution::ComparedWithPrevious if gate.held_out_sources > 0 => format!(
            "candidate showed no regression against the previous active revision, including \
             {} held-out suite source(s) pooled from prior revisions and sibling sessions",
            gate.held_out_sources
        ),
        SkillRegressionExecution::ComparedWithPrevious => {
            "candidate suite scores showed no regression against the previous active skill \
             revision; no held-out pool existed yet"
                .to_string()
        }
        SkillRegressionExecution::CandidateOnly if gate.held_out_sources > 0 => format!(
            "no previous active skill to compare against; candidate passed its generated suite \
             and {} held-out sibling suite source(s) (smoke gate)",
            gate.held_out_sources
        ),
        SkillRegressionExecution::CandidateOnly => {
            "no previous active skill to compare against; candidate executed its generated suite \
             without failures (smoke gate)"
                .to_string()
        }
        SkillRegressionExecution::Blocked => gate
            .rejection_reason
            .clone()
            .unwrap_or_else(|| "regression gate blocked promotion".to_string()),
    };
    AcceptanceChecks {
        held_in_pass: executed,
        held_in_description:
            "draft artifact revision validated as publishable and its generated regression suite \
             parsed and executed"
                .to_string(),
        held_out_pass: executed,
        held_out_description,
    }
}

/// Executes a rollback proposal after the caller authorized tenant operator access.
///
/// Semantics of an executed rollback (documented here because it reuses existing
/// candidate/artifact states rather than a bespoke one):
///
/// * the **rollback proposal** candidate moves `Proposed -> Evaluating` (claim)
///   `-> Promoted`, carrying `operation = skill_rollback` and the archived and
///   restored revision uids — `Promoted` means the proposal was accepted and its
///   action executed, exactly as for `accept_skill`;
/// * the regressed **published revision** is archived and the prior published
///   revision (if any) is restored as the serving revision;
/// * the **original promotion** candidate moves `Promoted -> RolledBack`
///   (best-effort compare-and-set) and its append-only `learning_log` entry is
///   invalidated with `valid_to`, matching rollback semantics;
/// * a new `skill_rollback` `learning_log` entry records the reviewer and the
///   archived/restored revisions.
///
/// Currentness is enforced transactionally: if a newer promotion has superseded
/// the proposal's revision (it is no longer the serving one), nothing is archived
/// and the proposal is moved `Evaluating -> Rejected` with a `409`, because a
/// retry would fail identically. A rollback with a predecessor restores that
/// predecessor's serving-side `description`/`tags` and drops the stale identity
/// embedding; a created-skill rollback (no predecessor) retires the artifact
/// identity and its embedding.
///
/// The claim is released back to `Proposed` only if the transactional rollback
/// fails for an operational reason, so a transient error never strands the
/// proposal in `Evaluating`.
pub async fn accept_rollback_candidate_after_authz(
    store: Arc<PostgresSessionStore>,
    pool: sqlx::PgPool,
    request: LearningCandidateReviewRequest,
) -> Result<LearningCandidateReviewResponse, HandlerError> {
    ensure_requested_action(request.action, LearningCandidateReviewAction::Accept)?;
    let tenant_id = request.tenant_id;
    let candidate = store
        .get_learning_candidate(&tenant_id, request.candidate_id)
        .await
        .map_err(moa_error_to_status_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "learning candidate not found"))?;

    ensure_rollback_proposal(&candidate)?;
    let skill_name = candidate.target_label.clone().unwrap_or_default();
    let artifact_uid = required_payload_uuid(&candidate.payload, "artifact_uid")?;
    let promoted_revision_uid = required_payload_uuid(&candidate.payload, "promoted_revision_uid")?;
    let previous_revision_uid = optional_payload_uuid(&candidate.payload, "previous_revision_uid")?;
    let promotion_candidate_id =
        required_payload_uuid(&candidate.payload, "promotion_candidate_id")?;

    // Claim the proposal so a concurrent reviewer cannot double-execute it.
    let claim = LearningCandidateStatusUpdate {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Evaluating,
        status_reason: Some("rollback accepted; executing revision archive".to_string()),
        evaluation_payload: None,
        updated_at: Utc::now(),
    };
    let claimed = store
        .update_learning_candidate_status_from(&claim, LearningCandidateStatus::Proposed)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    if !claimed {
        return Err(TerminalError::new_with_code(
            409,
            "rollback proposal changed status before review could be applied",
        )
        .into());
    }

    let executed = execute_rollback(
        &store,
        pool,
        &request,
        &candidate,
        RollbackExecution {
            tenant_id,
            skill_name: &skill_name,
            artifact_uid,
            promoted_revision_uid,
            previous_revision_uid,
            promotion_candidate_id,
        },
    )
    .await;

    match executed {
        Ok(RollbackOutcome::Applied(response)) => {
            record_review_decision("accept_rollback", "promoted");
            Ok(response)
        }
        Ok(RollbackOutcome::Superseded {
            serving_revision_uid,
        }) => {
            record_review_decision("accept_rollback", "superseded");
            // The proposal targets a revision a newer promotion has since
            // superseded, so it can never serve again — reject it terminally
            // rather than release it for a retry that would fail identically.
            let reject = LearningCandidateStatusUpdate {
                candidate_id: candidate.id,
                status: LearningCandidateStatus::Rejected,
                status_reason: Some(format!(
                    "rollback proposal superseded: revision {serving_revision_uid} now serves \
                     skill `{skill_name}`"
                )),
                evaluation_payload: None,
                updated_at: Utc::now(),
            };
            if let Err(reject_error) = store
                .update_learning_candidate_status_from(&reject, LearningCandidateStatus::Evaluating)
                .await
            {
                tracing::warn!(
                    candidate_id = %candidate.id,
                    error = %reject_error,
                    "failed to reject superseded rollback proposal"
                );
            }
            Err(TerminalError::new_with_code(
                409,
                format!(
                    "rollback proposal is stale: revision {serving_revision_uid} now serves skill \
                     `{skill_name}`; the proposal was rejected"
                ),
            )
            .into())
        }
        Err(error) => {
            record_review_decision("accept_rollback", "error");
            // Release the claim so the operator can retry once the fault is fixed.
            let release = LearningCandidateStatusUpdate {
                candidate_id: candidate.id,
                status: LearningCandidateStatus::Proposed,
                status_reason: Some(
                    "rollback execution failed; claim released for retry".to_string(),
                ),
                evaluation_payload: None,
                updated_at: Utc::now(),
            };
            if let Err(release_error) = store
                .update_learning_candidate_status_from(
                    &release,
                    LearningCandidateStatus::Evaluating,
                )
                .await
            {
                tracing::warn!(
                    candidate_id = %candidate.id,
                    error = %release_error,
                    "failed to release claimed rollback proposal after execution error"
                );
            }
            Err(error)
        }
    }
}

/// Immutable inputs for one rollback execution transaction.
struct RollbackExecution<'a> {
    tenant_id: TenantId,
    skill_name: &'a str,
    artifact_uid: Uuid,
    promoted_revision_uid: Uuid,
    previous_revision_uid: Option<Uuid>,
    promotion_candidate_id: Uuid,
}

/// Result of one rollback execution attempt.
enum RollbackOutcome {
    /// The regressed revision was archived and the proposal promoted.
    Applied(LearningCandidateReviewResponse),
    /// A newer revision now serves; the transaction changed nothing.
    Superseded {
        /// Revision currently serving the skill.
        serving_revision_uid: Uuid,
    },
}

/// Archives the regressed revision and records the rollback in one transaction.
///
/// Returns [`RollbackOutcome::Superseded`] without any write when the proposal's
/// promoted revision is no longer the serving one, so the caller can reject the
/// stale proposal instead of applying a rollback around a newer revision.
async fn execute_rollback(
    store: &Arc<PostgresSessionStore>,
    pool: sqlx::PgPool,
    request: &LearningCandidateReviewRequest,
    candidate: &LearningCandidate,
    execution: RollbackExecution<'_>,
) -> Result<RollbackOutcome, HandlerError> {
    let mut conn = ScopedConn::begin(&pool, &RlsContext::tenant(execution.tenant_id))
        .await
        .map_err(moa_error_to_status_handler_error)?;

    match ArtifactRegistry::rollback_published_revision_in_tx(
        conn.as_mut(),
        execution.promoted_revision_uid,
        execution.previous_revision_uid,
    )
    .await
    .map_err(moa_error_to_status_handler_error)?
    {
        moa_artifacts::registry::RollbackApplication::Applied => {}
        moa_artifacts::registry::RollbackApplication::Superseded {
            serving_revision_uid,
        } => {
            // Nothing was archived; drop the transaction and let the caller
            // terminally reject the stale proposal.
            return Ok(RollbackOutcome::Superseded {
                serving_revision_uid,
            });
        }
    }

    let evaluation_payload = json!({
        "operation": "skill_rollback",
        "reviewer_subject": request.reviewer_subject,
        "reason": request.reason,
        "skill_name": execution.skill_name,
        "artifact_uid": execution.artifact_uid,
        "archived_revision_uid": execution.promoted_revision_uid,
        "restored_revision_uid": execution.previous_revision_uid,
        "promotion_candidate_id": execution.promotion_candidate_id,
    });

    let promote = LearningCandidateStatusUpdate {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Promoted,
        status_reason: Some(
            request
                .reason
                .clone()
                .unwrap_or_else(|| "rollback accepted by reviewer".to_string()),
        ),
        evaluation_payload: Some(evaluation_payload.clone()),
        updated_at: Utc::now(),
    };
    if !store
        .update_learning_candidate_status_from_in_tx(
            conn.as_mut(),
            &promote,
            LearningCandidateStatus::Evaluating,
        )
        .await
        .map_err(moa_error_to_status_handler_error)?
    {
        return Err(TerminalError::new_with_code(
            409,
            "rollback proposal left the claimed state before it could be promoted",
        )
        .into());
    }

    // Best-effort: mark the original promotion rolled back. It may already be
    // superseded; only a still-Promoted promotion flips to RolledBack.
    let rolled_back = LearningCandidateStatusUpdate {
        candidate_id: execution.promotion_candidate_id,
        status: LearningCandidateStatus::RolledBack,
        status_reason: Some(format!(
            "superseded by rollback proposal {} for skill `{}`",
            candidate.id, execution.skill_name
        )),
        evaluation_payload: None,
        updated_at: Utc::now(),
    };
    store
        .update_learning_candidate_status_from_in_tx(
            conn.as_mut(),
            &rolled_back,
            LearningCandidateStatus::Promoted,
        )
        .await
        .map_err(moa_error_to_status_handler_error)?;

    store
        .invalidate_learning_by_candidate_in_tx(
            conn.as_mut(),
            &execution.tenant_id,
            execution.promotion_candidate_id,
            Utc::now(),
        )
        .await
        .map_err(moa_error_to_status_handler_error)?;

    let learning_entry =
        rollback_learning_entry(candidate, request, &execution, evaluation_payload);
    store
        .append_learning_in_tx(conn.as_mut(), &learning_entry)
        .await
        .map_err(moa_error_to_status_handler_error)?;

    conn.commit()
        .await
        .map_err(moa_error_to_status_handler_error)?;

    tracing::info!(
        tenant_id = %execution.tenant_id,
        skill = %execution.skill_name,
        candidate_id = %candidate.id,
        archived_revision_uid = %execution.promoted_revision_uid,
        "skill_regression_rollback_executed"
    );

    Ok(RollbackOutcome::Applied(LearningCandidateReviewResponse {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Promoted,
        artifact_uid: Some(execution.artifact_uid),
        draft_artifact_revision_uid: None,
        published_artifact_revision_uid: execution.previous_revision_uid,
    }))
}

fn rollback_learning_entry(
    candidate: &LearningCandidate,
    request: &LearningCandidateReviewRequest,
    execution: &RollbackExecution<'_>,
    evaluation_payload: Value,
) -> LearningEntry {
    LearningEntry {
        id: Uuid::now_v7(),
        tenant_id: candidate.tenant_id,
        learning_type: "skill_rollback".to_string(),
        target_id: execution.artifact_uid.to_string(),
        target_label: Some(execution.skill_name.to_string()),
        payload: json!({
            "candidate_id": candidate.id,
            "reviewer_subject": request.reviewer_subject,
            "reason": request.reason,
            "review": evaluation_payload,
        }),
        confidence: None,
        source_refs: vec![candidate.id, execution.promotion_candidate_id],
        actor: format!("review:{}", request.reviewer_subject),
        valid_from: Utc::now(),
        valid_to: None,
        batch_id: None,
        version: 1,
    }
}

fn ensure_rollback_proposal(candidate: &LearningCandidate) -> Result<(), HandlerError> {
    if candidate.candidate_type != LearningCandidateType::Skill {
        return Err(TerminalError::new_with_code(
            400,
            "only skill rollback proposals can be accepted here",
        )
        .into());
    }
    if candidate.status != LearningCandidateStatus::Proposed {
        return Err(TerminalError::new_with_code(
            400,
            "rollback proposal must be proposed before review",
        )
        .into());
    }
    if candidate.payload.get("kind").and_then(Value::as_str)
        != Some(moa_skills::rollback::ROLLBACK_PROPOSAL_KIND)
    {
        return Err(TerminalError::new_with_code(
            400,
            "learning candidate is not a skill rollback proposal",
        )
        .into());
    }
    Ok(())
}

fn required_payload_uuid(payload: &Value, key: &str) -> Result<Uuid, HandlerError> {
    let value = payload.get(key).and_then(Value::as_str).ok_or_else(|| {
        TerminalError::new_with_code(400, format!("rollback payload missing {key}"))
    })?;
    Uuid::parse_str(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("rollback payload {key} is invalid: {error}"))
            .into()
    })
}

fn optional_payload_uuid(payload: &Value, key: &str) -> Result<Option<Uuid>, HandlerError> {
    let Some(value) = payload.get(key).and_then(Value::as_str) else {
        return Ok(None);
    };
    Uuid::parse_str(value).map(Some).map_err(|error| {
        TerminalError::new_with_code(400, format!("rollback payload {key} is invalid: {error}"))
            .into()
    })
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

    record_review_decision("reject", "rejected");
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn gate(
        allow_promotion: bool,
        rejection_reason: Option<&str>,
        execution: SkillRegressionExecution,
    ) -> SkillRegressionGate {
        gate_with_held_out(allow_promotion, rejection_reason, execution, 0)
    }

    fn gate_with_held_out(
        allow_promotion: bool,
        rejection_reason: Option<&str>,
        execution: SkillRegressionExecution,
        held_out_sources: usize,
    ) -> SkillRegressionGate {
        SkillRegressionGate {
            report: json!({}),
            allow_promotion,
            rejection_reason: rejection_reason.map(ToString::to_string),
            execution,
            held_out_sources,
            compile_operation_key: None,
        }
    }

    #[test]
    fn acceptance_checks_reflect_compared_execution() {
        // Pins: a compared previous-vs-candidate run yields passing checks whose
        // description says a comparison ran — not a golden-set claim.
        let checks = acceptance_checks_for_gate(&gate(
            true,
            None,
            SkillRegressionExecution::ComparedWithPrevious,
        ));

        assert!(checks.held_in_pass);
        assert!(checks.held_out_pass);
        assert!(
            checks
                .held_out_description
                .contains("previous active skill")
        );
        assert!(!checks.held_out_description.contains("golden"));
    }

    #[test]
    fn acceptance_checks_reflect_candidate_only_smoke_run() {
        // Pins: a first-revision smoke run passes but records that no baseline existed,
        // so the audit record cannot be mistaken for a regression comparison.
        let checks =
            acceptance_checks_for_gate(&gate(true, None, SkillRegressionExecution::CandidateOnly));

        assert!(checks.held_in_pass);
        assert!(checks.held_out_pass);
        assert!(
            checks
                .held_out_description
                .contains("no previous active skill")
        );
        assert!(checks.held_out_description.contains("smoke gate"));
    }

    #[test]
    fn acceptance_checks_report_held_out_pool_when_it_executed() {
        // Pins: when held-out material actually ran, the audit record says so with the
        // source count — and when none existed, it says that instead of implying a split.
        let pooled = acceptance_checks_for_gate(&gate_with_held_out(
            true,
            None,
            SkillRegressionExecution::ComparedWithPrevious,
            2,
        ));
        assert!(pooled.held_out_pass);
        assert!(
            pooled
                .held_out_description
                .contains("2 held-out suite source(s)")
        );

        let unpooled = acceptance_checks_for_gate(&gate(
            true,
            None,
            SkillRegressionExecution::ComparedWithPrevious,
        ));
        assert!(
            unpooled
                .held_out_description
                .contains("no held-out pool existed yet")
        );
    }

    #[test]
    fn acceptance_checks_fail_when_gate_blocked() {
        // Pins: a blocked gate derives failing checks carrying the rejection reason, so a
        // future outcome that neither blocks nor executes cannot silently promote.
        let checks = acceptance_checks_for_gate(&gate(
            false,
            Some("generated regression suite could not be parsed"),
            SkillRegressionExecution::Blocked,
        ));

        assert!(!checks.held_in_pass);
        assert!(!checks.held_out_pass);
        assert_eq!(
            checks.held_out_description,
            "generated regression suite could not be parsed"
        );
    }
}

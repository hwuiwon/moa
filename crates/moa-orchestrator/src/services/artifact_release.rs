//! Restate service for artifact release candidates, decisions, and activation.
//!
//! Dispatch lives here too, and it is transactional. `submit` writes the release
//! candidate and its durable dispatch record in one transaction, so there is no
//! committed submission without a dispatch record and no dispatch record for a
//! submission that rolled back. Only after that transaction commits does it send
//! the `ArtifactReleaseEvaluation` workflow, keyed by the dispatch record id --
//! which is why a Restate replay of `submit` cannot start a second evaluation.
//!
//! The evaluation workflow is the other half of the fence. It derives the verdict
//! from persisted experiment evidence, then settles the open dispatch under both
//! the submission generation *and* the exact subject digest before it lets the
//! candidate state move. There is deliberately no client-facing decision handler:
//! callers cannot submit a verdict or mint their own attestation.
//!
//! Authorization is what separates the handlers, and the separation is the
//! point:
//!
//! * `submit` requires the tenant `Operator` relation. A submitter creates a
//!   release attempt and nothing else; it cannot name a policy, because the
//!   request has no policy field and the repository resolves the gate server-side.
//! * `activate` requires `Operator` again, because activation is an operator
//!   action -- but it can only spend an attestation that the evaluation workflow
//!   minted from persisted evidence, for the exact subject that was evaluated.
//! * `list_attempts` requires `Operator` and `review_attempt` requires `Admin`.
//!   Release attempts and attestation review live on this surface rather than in
//!   the learning-review queue: a hand-authored skill, action, or agent has no
//!   `SanitizedLearningEvidence` and no contribution rows, so the learning surface
//!   cannot represent its attempt at all.

use moa_agents::AgentResolver;
use moa_artifacts::registry::{
    ArtifactRegistry, CandidateSubjectInputs, ReleaseRepository, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationRequest, ActivationTarget, AgentRuntimeSubject, CatalogSnapshotBinding, Digest32,
    TenantScope,
};
use moa_authz_schema::Relation;
use moa_core::traits::Identity;
use moa_hands::ToolRouter;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::artifact_release::{
    ReleaseActivateRequest, ReleaseActivateResponse, ReleaseAttemptEntry,
    ReleaseAttemptListRequest, ReleaseAttemptListResponse, ReleaseAttemptReviewRequest,
    ReleaseAttemptReviewResponse, ReleaseSubmitRequest, ReleaseSubmitResponse,
};
use restate_sdk::prelude::*;
use sqlx::PgPool;

use crate::handlers::authz_shim::authorize_tenant;
use crate::workflows::artifact_release_evaluation::repository::{
    ReleaseEvaluationRepository, ReleaseSubjectEnvironment,
};
use crate::workflows::artifact_release_evaluation::types::{
    AttemptReviewState, PinnedDependency, ReleaseAttemptRow,
};
use crate::workflows::artifact_release_evaluation::{
    ArtifactReleaseEvaluationClient, Error as ReleaseEvaluationError,
    ReleaseEvaluationWorkflowRequest,
};

/// Restate service surface for artifact release control.
#[restate_sdk::service]
#[name = "ArtifactRelease"]
pub trait ArtifactRelease {
    /// Submits an immutable candidate revision for release evaluation.
    async fn submit(
        request: Json<ReleaseSubmitRequest>,
    ) -> Result<Json<ReleaseSubmitResponse>, HandlerError>;

    /// Moves a type-owned serving pointer by spending an activation attestation.
    async fn activate(
        request: Json<ReleaseActivateRequest>,
    ) -> Result<Json<ReleaseActivateResponse>, HandlerError>;

    /// Lists release attempts and their attestation review state.
    async fn list_attempts(
        request: Json<ReleaseAttemptListRequest>,
    ) -> Result<Json<ReleaseAttemptListResponse>, HandlerError>;

    /// Records attestation review against one release attempt.
    async fn review_attempt(
        request: Json<ReleaseAttemptReviewRequest>,
    ) -> Result<Json<ReleaseAttemptReviewResponse>, HandlerError>;
}

/// Concrete artifact release service implementation.
#[derive(Clone)]
pub struct ArtifactReleaseImpl {
    pool: PgPool,
    tool_router: std::sync::Arc<ToolRouter>,
}

impl ArtifactReleaseImpl {
    /// Creates the release adapter with its artifact pool.
    #[must_use]
    pub fn new(pool: PgPool, tool_router: std::sync::Arc<ToolRouter>) -> Self {
        Self { pool, tool_router }
    }
}

impl ArtifactRelease for ArtifactReleaseImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn submit(
        &self,
        ctx: Context<'_>,
        request: Json<ReleaseSubmitRequest>,
    ) -> Result<Json<ReleaseSubmitResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ArtifactRelease", "submit");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = self.pool.clone();
        let tool_router = self.tool_router.clone();
        let submitted = ctx
            .run(|| {
                let pool = pool.clone();
                let tool_router = tool_router.clone();
                let request = request.clone();
                let identity = identity.clone();
                async move {
                    submit_inner(pool, tool_router, request, identity)
                        .await
                        .map(Json::from)
                }
            })
            .name("artifact_release_submit")
            .await?
            .into_inner();
        // Sending is keyed by the dispatch record, and the dispatch record was
        // created inside the transaction above. A replay of this handler therefore
        // targets the same workflow key and Restate admits one invocation for it,
        // so no replay can start a second evaluation run.
        if let Some(dispatch) = submitted.dispatch {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ArtifactReleaseEvaluationClient>(
                    dispatch.outbox_uid.to_string(),
                )
                .run(Json::from(ReleaseEvaluationWorkflowRequest {
                    tenant_id: request.tenant_id,
                    outbox_uid: dispatch.outbox_uid,
                    identity,
                })),
            )
            .send();
        }
        Ok(Json::from(submitted.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn activate(
        &self,
        ctx: Context<'_>,
        request: Json<ReleaseActivateRequest>,
    ) -> Result<Json<ReleaseActivateResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ArtifactRelease", "activate");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = self.pool.clone();
        let tool_router = self.tool_router.clone();
        Ok(ctx
            .run(|| async move {
                activate_inner(pool, tool_router, request, identity)
                    .await
                    .map(Json::from)
            })
            .name("artifact_release_activate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_attempts(
        &self,
        ctx: Context<'_>,
        request: Json<ReleaseAttemptListRequest>,
    ) -> Result<Json<ReleaseAttemptListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ArtifactRelease", "list_attempts");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_attempts_inner(pool, request).await.map(Json::from) })
            .name("artifact_release_list_attempts")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn review_attempt(
        &self,
        ctx: Context<'_>,
        request: Json<ReleaseAttemptReviewRequest>,
    ) -> Result<Json<ReleaseAttemptReviewResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ArtifactRelease", "review_attempt");
        let request = request.into_inner();
        // `Admin`, matching `record_decision`: reviewing the evidence that minted a
        // permission to serve is part of the same authority that minted it.
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| {
                let pool = pool.clone();
                let request = request.clone();
                let identity = identity.clone();
                async move {
                    review_attempt_inner(pool, request, identity)
                        .await
                        .map(Json::from)
                }
            })
            .name("artifact_release_review_attempt")
            .await?)
    }
}

/// A submission plus what the caller must send the evaluation workflow.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct SubmitOutcome {
    response: ReleaseSubmitResponse,
    dispatch: Option<EvaluationDispatch>,
}

/// Everything the evaluation workflow needs, resolved before it is sent.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct EvaluationDispatch {
    outbox_uid: uuid::Uuid,
}

async fn submit_inner(
    pool: PgPool,
    tool_router: std::sync::Arc<ToolRouter>,
    request: ReleaseSubmitRequest,
    identity: Identity,
) -> Result<SubmitOutcome, HandlerError> {
    let scope = TenantScope::new(request.tenant_id);
    let registry = ArtifactRegistry::new(pool.clone());
    let revision = registry
        .load_revision(&scope.action_rule_scope(), request.revision_uid)
        .await
        .map_err(release_storage_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "artifact revision not found"))?;
    let activation_target = ActivationTarget::for_kind(
        &revision.kind,
        revision.artifact_uid,
        request.installation_uid,
    )
    .map_err(release_error)?;
    let agent_policy = if revision.kind == moa_artifacts::document::ArtifactKind::Agent {
        Some(
            AgentResolver::new(pool.clone())
                .resolve_release_candidate(&scope.action_rule_scope(), request.revision_uid)
                .await
                .map_err(release_storage_error)?,
        )
    } else {
        None
    };

    let pinned = request
        .pinned_draft_dependencies
        .iter()
        .map(|dependency| PinnedDependency {
            artifact_uid: dependency.artifact_uid,
            revision_uid: dependency.revision_uid,
        })
        .collect::<Vec<_>>();
    let release_repository = ReleaseEvaluationRepository::new(pool);
    let subject_environment = release_repository
        .resolve_subject_environment(request.tenant_id, activation_target.class())
        .await
        .map_err(ReleaseEvaluationError::terminal)?;
    let subject_inputs = subject_inputs_for_revision(
        &revision,
        agent_policy.as_ref(),
        &tool_router,
        subject_environment,
    )
    .map_err(release_error)?;
    let submitted = release_repository
        .submit_with_dispatch(
            SubmitCandidate {
                scope,
                activation_target,
                candidate_revision_uid: request.revision_uid,
                subject_inputs,
                submitted_by: identity.id.to_string(),
            },
            pinned,
        )
        .await
        .map_err(ReleaseEvaluationError::terminal)?;
    let candidate = submitted.submission.candidate;
    let activation_target = candidate.activation_target.class().as_str().to_string();
    let dispatch = submitted
        .dispatch
        .as_ref()
        .map(|record| EvaluationDispatch {
            outbox_uid: record.outbox_uid,
        });
    Ok(SubmitOutcome {
        response: ReleaseSubmitResponse {
            tenant_id: request.tenant_id,
            revision_uid: candidate.revision_uid,
            activation_target,
            state: candidate.state.as_str().to_string(),
            slot: candidate.slot.as_str().to_string(),
            subject_digest: candidate.subject_digest.to_string(),
            policy_uid: candidate.policy.policy_uid,
            policy_revision: candidate.policy.revision,
            dispatched: submitted.submission.dispatched,
            displaced_pending_revision_uid: submitted.submission.displaced_pending_revision_uid,
            generation: candidate.generation,
            outbox_uid: submitted.dispatch.as_ref().map(|record| record.outbox_uid),
            dispatch_idempotency_key: submitted
                .dispatch
                .as_ref()
                .map(|record| record.idempotency_key.clone()),
            abandoned_outbox_uids: submitted.abandoned_outbox_uids,
        },
        dispatch,
    })
}

async fn list_attempts_inner(
    pool: PgPool,
    request: ReleaseAttemptListRequest,
) -> Result<ReleaseAttemptListResponse, HandlerError> {
    let attempts = ReleaseEvaluationRepository::new(pool)
        .list_attempts(request.tenant_id, request.limit.unwrap_or(50))
        .await
        .map_err(ReleaseEvaluationError::terminal)?
        .iter()
        .map(attempt_entry)
        .collect();
    Ok(ReleaseAttemptListResponse {
        tenant_id: request.tenant_id,
        attempts,
    })
}

async fn review_attempt_inner(
    pool: PgPool,
    request: ReleaseAttemptReviewRequest,
    identity: Identity,
) -> Result<ReleaseAttemptReviewResponse, HandlerError> {
    let state: AttemptReviewState = request
        .review_state
        .parse()
        .map_err(ReleaseEvaluationError::terminal)?;
    let attempt = ReleaseEvaluationRepository::new(pool)
        .review_attempt(
            request.tenant_id,
            request.attempt_uid,
            state,
            &identity.id.to_string(),
            request.note.as_deref(),
        )
        .await
        .map_err(ReleaseEvaluationError::terminal)?;
    Ok(ReleaseAttemptReviewResponse {
        attempt: attempt_entry(&attempt),
    })
}

/// Projects a stored attempt onto the wire, without hidden cohort contents.
fn attempt_entry(row: &ReleaseAttemptRow) -> ReleaseAttemptEntry {
    ReleaseAttemptEntry {
        attempt_uid: row.attempt_uid,
        outbox_uid: row.outbox_uid,
        revision_uid: row.revision_uid,
        artifact_uid: row.artifact_uid,
        generation: row.generation,
        subject_digest: row.subject_digest.clone(),
        activation_target: row.activation_target.clone(),
        candidate_run_uid: row.candidate_run_uid,
        baseline_run_uid: row.baseline_run_uid,
        cohort_epoch: row.cohort_epoch,
        verdict: row.verdict.clone(),
        attestation_uid: row.attestation_uid,
        fenced_out: row.fenced_out,
        fence_reason: row.fence_reason.clone(),
        review_state: row.review_state.clone(),
        reviewed_by: row.reviewed_by.clone(),
        reviewed_at: row.reviewed_at,
        review_note: row.review_note.clone(),
        created_at: row.created_at,
    }
}

async fn activate_inner(
    pool: PgPool,
    tool_router: std::sync::Arc<ToolRouter>,
    request: ReleaseActivateRequest,
    identity: Identity,
) -> Result<ReleaseActivateResponse, HandlerError> {
    let scope = TenantScope::new(request.tenant_id);
    let repository = ReleaseRepository::new(pool.clone());
    let candidate = repository
        .load_candidate(&scope, request.revision_uid)
        .await
        .map_err(release_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "release candidate not found"))?;
    if matches!(
        candidate.activation_target,
        ActivationTarget::AgentDeployment { .. }
    ) {
        return Err(TerminalError::new_with_code(
            400,
            "agent releases activate through AgentDefinitions/deploy",
        )
        .into());
    }
    ensure_current_release_environment(&pool, &candidate).await?;
    ensure_current_tool_catalog(&candidate, &tool_router)?;
    // The expectation is read here rather than supplied by the caller, and it is
    // not the anti-drift check: the attested subject digest covers the serving
    // baseline, so a pointer that moved since evaluation fails the digest
    // recomputation inside the activation transaction.
    let expected_serving = repository
        .expected_serving(&scope, &candidate.activation_target)
        .await
        .map_err(release_error)?;
    let outcome = repository
        .activate(ActivationRequest {
            scope,
            activation_target: candidate.activation_target,
            candidate_revision_uid: request.revision_uid,
            candidate_revision_hash: candidate.candidate_revision_hash,
            attestation_uid: request.attestation_uid,
            expected_serving,
            agent_revision_lock: None,
            actor: identity.id.to_string(),
            reason: request.reason,
        })
        .await
        .map_err(release_error)?;
    Ok(ReleaseActivateResponse {
        audit_uid: outcome.audit_uid,
        activated_revision_uid: outcome.activated_revision_uid,
        previous_revision_uid: outcome.previous_revision_uid,
        pointer_version: outcome.pointer_version,
        superseded_revision_uids: outcome.superseded_revision_uids,
        deployment_uid: outcome.deployment_uid,
    })
}

/// Builds the subject inputs derived from the candidate and release environment.
fn subject_inputs_for_revision(
    revision: &moa_artifacts::registry::StoredArtifactRevision,
    agent_policy: Option<&moa_agents::AgentRuntimePolicy>,
    tool_router: &ToolRouter,
    environment: ReleaseSubjectEnvironment,
) -> moa_artifacts::Result<CandidateSubjectInputs> {
    let document_hash = Digest32(moa_artifacts::canonical::canonical_hash(
        &revision.document,
    )?);
    let references_hash = agent_policy.map_or_else(
        || {
            moa_artifacts::canonical::canonical_hash(&revision.document.reference_resolutions)
                .map(Digest32)
        },
        |policy| moa_artifacts::canonical::canonical_hash(&policy.revision_lock).map(Digest32),
    )?;
    let prompt_hash = match agent_policy {
        Some(policy) => Digest32(moa_artifacts::canonical::canonical_hash(
            &policy.instructions,
        )?),
        None => document_hash,
    };
    let runtime_policy_hash = agent_policy.map_or(document_hash, |_| references_hash);
    let tool_policy_hash = match agent_policy {
        Some(policy) => Digest32(moa_artifacts::canonical::canonical_hash(
            &policy.tool_policy,
        )?),
        None => references_hash,
    };
    let tool_bearing = agent_policy.is_some() || !revision.document.reference_paths().is_empty();
    let tool_catalog = if tool_bearing {
        Some(current_tool_catalog_binding(tool_router)?)
    } else {
        None
    };
    Ok(CandidateSubjectInputs {
        dependency_lock_hash: references_hash,
        agent_runtime: AgentRuntimeSubject {
            prompt_hash,
            model: agent_policy
                .and_then(|policy| policy.model_policy.default_model.clone())
                .unwrap_or_else(|| RELEASE_SUBJECT_MODEL_BINDING.to_string()),
            provider: RELEASE_SUBJECT_PROVIDER_BINDING.to_string(),
            runtime_policy_hash,
        },
        tool_policy_hash,
        tool_bearing,
        tool_catalog,
        plan: environment.plan,
        simulator: Some(environment.simulator),
    })
}

pub(crate) fn current_tool_catalog_binding(
    tool_router: &ToolRouter,
) -> moa_artifacts::Result<CatalogSnapshotBinding> {
    let pin =
        tool_router
            .activated_catalog()
            .pin()
            .map_err(|error| moa_artifacts::Error::Release {
                rejection: moa_artifacts::ReleaseRejection::ToolCatalogSnapshotMissing,
                detail: format!("activated tool catalog cannot be pinned: {error}"),
            })?;
    let bytes = hex::decode(&pin.contract_hash).map_err(|error| moa_artifacts::Error::Release {
        rejection: moa_artifacts::ReleaseRejection::ToolCatalogSnapshotMissing,
        detail: format!("activated tool catalog hash is not hex: {error}"),
    })?;
    let schema_hash = Digest32::from_slice(&bytes)?;
    let mut snapshot_bytes = [0_u8; 16];
    snapshot_bytes.copy_from_slice(&bytes[..16]);
    snapshot_bytes[6] = (snapshot_bytes[6] & 0x0f) | 0x80;
    snapshot_bytes[8] = (snapshot_bytes[8] & 0x3f) | 0x80;
    Ok(CatalogSnapshotBinding {
        snapshot_uid: uuid::Uuid::from_bytes(snapshot_bytes),
        schema_hash,
        activated: true,
    })
}

pub(crate) fn ensure_current_tool_catalog(
    candidate: &moa_artifacts::registry::ReleaseCandidate,
    tool_router: &ToolRouter,
) -> Result<(), HandlerError> {
    if !candidate.subject.tool_bearing {
        return Ok(());
    }
    let current = current_tool_catalog_binding(tool_router).map_err(release_error)?;
    if candidate.subject.tool_catalog.as_ref() != Some(&current) {
        return Err(TerminalError::new_with_code(
            409,
            "activated tool catalog changed since release evaluation",
        )
        .into());
    }
    Ok(())
}

pub(crate) async fn ensure_current_release_environment(
    pool: &PgPool,
    candidate: &moa_artifacts::registry::ReleaseCandidate,
) -> Result<(), HandlerError> {
    let current = ReleaseEvaluationRepository::new(pool.clone())
        .resolve_subject_environment(candidate.tenant_id, candidate.activation_target.class())
        .await
        .map_err(ReleaseEvaluationError::terminal)?;
    if candidate.subject.plan != current.plan
        || candidate.subject.simulator.as_ref() != Some(&current.simulator)
    {
        return Err(TerminalError::new_with_code(
            409,
            "approved release plan, case cohort, evaluators, or simulator changed since evaluation",
        )
        .into());
    }
    Ok(())
}

fn release_error(error: moa_artifacts::Error) -> HandlerError {
    match &error {
        moa_artifacts::Error::Release { rejection, .. } => {
            TerminalError::new_with_code(409, format!("{rejection}: {error}")).into()
        }
        _ => TerminalError::new_with_code(400, error.to_string()).into(),
    }
}

fn release_storage_error(error: moa_core::error::MoaError) -> HandlerError {
    crate::workflows::errors::moa_error_to_status_handler_error(error)
}

/// Model binding recorded for a release subject with no agent runtime of its own.
const RELEASE_SUBJECT_MODEL_BINDING: &str = "release-candidate-document";
/// Provider binding recorded for the same subject class.
const RELEASE_SUBJECT_PROVIDER_BINDING: &str = "moa-internal";

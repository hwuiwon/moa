//! Durable evaluation of an unpublished release candidate.
//!
//! `V000373` made a candidate immutable, gave it a lifecycle, a coalescing slot,
//! an exact subject digest, and a single-use activation attestation. It did not
//! make anything *run*: `submit_candidate` recorded a row and dispatched nothing,
//! so every attestation had to be minted from evidence nobody produced. This
//! workflow is the missing half.
//!
//! The shape is: the submission transaction writes a dispatch record
//! ([`repository::ReleaseEvaluationRepository::submit_with_dispatch`]); this
//! workflow, keyed by that record, claims it, provisions the evaluation-only
//! overlay and a copy-on-write fixture per arm, resolves the server-side approved
//! case pack plus the hidden release cohort, and dispatches one plan-backed paired
//! run through `Experiments/run`. The workflow derives the deterministic decision
//! from that run's persisted scores and settles the record under the same
//! generation and subject-digest fence.
//!
//! Replay safety comes from three places, and none of them is "the workflow is
//! careful":
//!
//! * the workflow key *is* the dispatch record id, so Restate admits one
//!   invocation per record;
//! * claiming is an idempotent state transition, and a replay that finds the
//!   record already `dispatched` reuses the runs the first invocation started;
//! * the dispatch record's idempotency key is deterministic in
//!   `(revision, generation, subject digest)` and is passed to `Experiments/run`,
//!   so re-admission returns the same run instead of a second one.

use moa_artifacts::registry::{RecordDecision, ReleaseRepository};
use moa_artifacts::release::{ActivationTargetClass, EvidenceAdapter, TenantScope};
use moa_core::traits::Identity;
use moa_core::types::identifiers::TenantId;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error as ThisError;
use uuid::Uuid;

pub mod decision;
pub mod dispatch;
pub mod repository;
pub mod types;

use crate::restate_identity::with_identity_headers;
use crate::services::experiments::ExperimentsClient;
use decision::decide_completed_run;
use dispatch::build_paired_run_request;
use moa_wire::experiments::ExperimentRunStatusRequest;
use repository::ReleaseEvaluationRepository;
use types::ArmRole;

/// Failures the release-evaluation surface can produce.
#[derive(Debug, ThisError)]
pub enum Error {
    /// Storage or scope failure.
    #[error("release evaluation storage failure: {0}")]
    Storage(String),
    /// The underlying release repository refused the write.
    #[error("release control refused the write: {0}")]
    Release(#[from] moa_artifacts::Error),
    /// The dispatch record no longer describes the subject being released.
    #[error("release dispatch is stale: {0}")]
    StaleDispatch(String),
    /// The resolved approved pack cannot gate anything.
    #[error("release case pack is unusable: {0}")]
    CasePackInvalid(String),
    /// The hidden cohort attempt budget for this epoch is spent.
    #[error("hidden release cohort budget exhausted: {0}")]
    HiddenCohortBudgetExhausted(String),
    /// A submitter pinned a dependency it does not own.
    #[error("pinned release dependency is invalid: {0}")]
    PinnedDependencyInvalid(String),
    /// The evaluation environment could not be provisioned.
    #[error("release evaluation could not be provisioned: {0}")]
    Provisioning(String),
    /// An experiment request did not match its durable release attempt.
    #[error("release experiment binding is invalid: {0}")]
    ExperimentBindingInvalid(String),
    /// An attempt review was refused.
    #[error("release attempt review refused: {0}")]
    ReviewInvalid(String),
}

impl Error {
    /// Returns the HTTP status a Restate terminal error should carry.
    ///
    /// The mapping is the fail-closed contract, not cosmetics. A stale dispatch,
    /// an exhausted hidden-cohort budget, and an unusable pack are all `409`: the
    /// request was well formed and was refused, and a caller that retries with the
    /// same inputs must be refused again.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::Storage(_) => 500,
            Self::Release(_)
            | Self::StaleDispatch(_)
            | Self::CasePackInvalid(_)
            | Self::HiddenCohortBudgetExhausted(_)
            | Self::Provisioning(_) => 409,
            Self::PinnedDependencyInvalid(_)
            | Self::ExperimentBindingInvalid(_)
            | Self::ReviewInvalid(_) => 400,
        }
    }

    /// Converts into a Restate *terminal* error.
    ///
    /// Deliberately not a `From` impl. Restate's blanket conversion would turn
    /// every one of these into a retryable failure, and a fail-closed refusal that
    /// is retried forever is not fail-closed: a stale dispatch, an exhausted
    /// hidden-cohort budget, and an unusable pack must all stop the invocation.
    #[must_use]
    pub fn terminal(self) -> HandlerError {
        TerminalError::new_with_code(self.status_code(), self.to_string()).into()
    }
}

/// Workflow input for one release-candidate evaluation attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseEvaluationWorkflowRequest {
    /// Tenant already authorized by `ArtifactRelease/submit`.
    pub tenant_id: TenantId,
    /// Dispatch record this invocation owns; also the workflow key.
    pub outbox_uid: Uuid,
    /// Candidate revision under evaluation.
    pub revision_uid: Uuid,
    /// Activation target class, as its database label.
    pub activation_target: String,
    /// Serving revision to compare against, absent for a first activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_revision_uid: Option<Uuid>,
    /// Identity snapshot used for the downstream `Experiments/run` authorization.
    pub identity: Identity,
}

/// What one release-evaluation invocation did.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseEvaluationWorkflowResponse {
    /// Dispatch record this invocation owned.
    pub outbox_uid: Uuid,
    /// Whether an experiment run was dispatched.
    pub dispatched: bool,
    /// Attempt row on the artifact-release review surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_uid: Option<Uuid>,
    /// Paired experiment run both arms executed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_uid: Option<Uuid>,
    /// Seed material both arms ran with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_material: Option<String>,
    /// Hidden cohort epoch the attempt faced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohort_epoch: Option<i32>,
    /// Why nothing was dispatched, when nothing was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

/// Restate workflow surface for one release-candidate evaluation.
#[restate_sdk::workflow]
pub trait ArtifactReleaseEvaluation {
    /// Claims a dispatch record and runs the candidate against the release cohort.
    async fn run(
        request: Json<ReleaseEvaluationWorkflowRequest>,
    ) -> Result<Json<ReleaseEvaluationWorkflowResponse>, HandlerError>;
}

/// Concrete release-evaluation workflow implementation.
#[derive(Clone)]
pub struct ArtifactReleaseEvaluationImpl {
    repository: ReleaseEvaluationRepository,
    pool: sqlx::PgPool,
}

impl ArtifactReleaseEvaluationImpl {
    /// Creates the workflow with its artifact pool.
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            repository: ReleaseEvaluationRepository::new(pool.clone()),
            pool,
        }
    }
}

impl ArtifactReleaseEvaluation for ArtifactReleaseEvaluationImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from ArtifactRelease/submit or by this workflow when it
    // advances the next durable dispatch. Submission authorizes the tenant before
    // creating the dispatch record this workflow validates by key. The downstream
    // Experiments/run call re-authorizes on its own.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ReleaseEvaluationWorkflowRequest>,
    ) -> Result<Json<ReleaseEvaluationWorkflowResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ArtifactReleaseEvaluation", "run");
        let request = request.into_inner();
        if request.outbox_uid.to_string() != ctx.key() {
            return Err(
                TerminalError::new_with_code(404, "release dispatch record id mismatch").into(),
            );
        }
        let activation_target: ActivationTargetClass = request
            .activation_target
            .parse()
            .map_err(|error| Error::Release(error).terminal())?;

        let repository = self.repository.clone();
        let tenant_id = request.tenant_id;
        let outbox_uid = request.outbox_uid;
        let claimed = ctx
            .run(|| {
                let repository = repository.clone();
                async move {
                    repository
                        .claim_dispatch(tenant_id, outbox_uid)
                        .await
                        .map(Json::from)
                        .map_err(Error::terminal)
                }
            })
            .name("release_evaluation_claim")
            .await?
            .into_inner();
        let Some(claimed) = claimed else {
            // The record was abandoned or already settled. A workflow that wakes
            // up for a superseded subject must dispatch nothing at all.
            return Ok(Json(ReleaseEvaluationWorkflowResponse {
                outbox_uid,
                dispatched: false,
                attempt_uid: None,
                run_uid: None,
                seed_material: None,
                cohort_epoch: None,
                skipped_reason: Some(
                    "dispatch record was superseded or already settled".to_string(),
                ),
            }));
        };

        // Journaled, not derived: the overlay token must be a secret the database
        // does not hold, and it must survive replay. A Restate `run` result is both.
        let tokens = ctx
            .run(|| async {
                Ok::<_, HandlerError>(Json::from(vec![
                    (
                        ArmRole::Candidate,
                        Uuid::now_v7().simple().to_string() + &Uuid::now_v7().simple().to_string(),
                    ),
                    (
                        ArmRole::Baseline,
                        Uuid::now_v7().simple().to_string() + &Uuid::now_v7().simple().to_string(),
                    ),
                ]))
            })
            .name("release_evaluation_overlay_tokens")
            .await?
            .into_inner();

        let repository = self.repository.clone();
        let record = claimed.record.clone();
        let baseline_revision_uid = request.baseline_revision_uid;
        let attempt = ctx
            .run(|| {
                let repository = repository.clone();
                let record = record.clone();
                let tokens = tokens.clone();
                async move {
                    repository
                        .provision_attempt(
                            &record,
                            activation_target,
                            baseline_revision_uid,
                            &tokens,
                        )
                        .await
                        .map(Json::from)
                        .map_err(Error::terminal)
                }
            })
            .name("release_evaluation_provision")
            .await?
            .into_inner();

        let run_request = build_paired_run_request(
            request.tenant_id,
            &claimed.record,
            activation_target,
            &attempt,
        )
        .map_err(Error::terminal)?;
        let run_uid = with_identity_headers(
            ctx.service_client::<ExperimentsClient>()
                .run(Json::from(run_request)),
            &request.identity,
        )
        .call()
        .await?
        .into_inner()
        .run_uid;

        let repository = self.repository.clone();
        // A first activation compares the candidate with the approved plan's
        // unmodified control target, so every release attempt remains paired.
        let baseline_run_uid = Some(run_uid);
        ctx.run(|| {
            let repository = repository.clone();
            async move {
                repository
                    .record_dispatched_runs(tenant_id, outbox_uid, run_uid, baseline_run_uid)
                    .await
                    .map(Json::from)
                    .map_err(Error::terminal)
            }
        })
        .name("release_evaluation_record_runs")
        .await?;

        wait_for_terminal_experiment(&ctx, request.tenant_id, run_uid, &request.identity).await?;

        let pool = self.pool.clone();
        let subject_digest = claimed.record.subject_digest;
        let candidate_revision_uid = claimed.record.revision_uid;
        let release_decision = ctx
            .run(|| async move {
                decide_completed_run(
                    pool,
                    tenant_id,
                    run_uid,
                    candidate_revision_uid,
                    subject_digest,
                )
                .await
                .map(Json::from)
                .map_err(Error::terminal)
            })
            .name("release_evaluation_decide")
            .await?
            .into_inner();

        let repository = self.repository.clone();
        let generation = claimed.record.generation;
        let identity = request.identity.clone();
        let settled = ctx
            .run(|| {
                let repository = repository.clone();
                let release_decision = release_decision.clone();
                async move {
                    let fenced = repository
                        .record_decision_with_fence(
                            RecordDecision {
                                scope: TenantScope::new(tenant_id),
                                candidate_revision_uid,
                                subject_digest,
                                verdict: release_decision.verdict,
                                run_uid,
                                trial_uids: release_decision.trial_uids,
                                evidence_ids: release_decision.evidence_ids,
                                gate_results: release_decision.gate_results,
                                blocking_assertions: release_decision.blocking_assertions,
                                evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
                                decided_by: format!("artifact-release-evaluation:{outbox_uid}"),
                            },
                            generation,
                            release_decision.detail,
                        )
                        .await
                        .map_err(Error::terminal)?;
                    Ok::<_, HandlerError>(Json::from(SettledEvaluation { next: fenced.next }))
                }
            })
            .name("release_evaluation_settle")
            .await?
            .into_inner();

        if let Some(next) = settled.next {
            let pool = self.pool.clone();
            let next_request = ctx
                .run(|| async move {
                    next_workflow_request(pool, next, identity)
                        .await
                        .map(Json::from)
                        .map_err(Error::terminal)
                })
                .name("release_evaluation_next_dispatch")
                .await?
                .into_inner();
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ArtifactReleaseEvaluationClient>(
                    next_request.outbox_uid.to_string(),
                )
                .run(Json::from(next_request)),
            )
            .send();
        }

        Ok(Json(ReleaseEvaluationWorkflowResponse {
            outbox_uid,
            dispatched: true,
            attempt_uid: Some(attempt.attempt_uid),
            run_uid: Some(run_uid),
            seed_material: Some(claimed.record.seed_material.clone()),
            cohort_epoch: Some(attempt.plan.cohort_epoch),
            skipped_reason: None,
        }))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SettledEvaluation {
    next: Option<types::DispatchRecord>,
}

async fn wait_for_terminal_experiment(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    identity: &Identity,
) -> Result<(), HandlerError> {
    loop {
        let response = with_identity_headers(
            ctx.service_client::<ExperimentsClient>().status(Json::from(
                ExperimentRunStatusRequest { tenant_id, run_uid },
            )),
            identity,
        )
        .call()
        .await?
        .into_inner();
        if matches!(
            response.status.as_str(),
            "completed" | "failed" | "cancelled"
        ) {
            return Ok(());
        }
        ctx.sleep(Duration::from_secs(1)).await?;
    }
}

async fn next_workflow_request(
    pool: sqlx::PgPool,
    record: types::DispatchRecord,
    identity: Identity,
) -> Result<ReleaseEvaluationWorkflowRequest, Error> {
    let scope = TenantScope::new(record.tenant_id);
    let candidate = ReleaseRepository::new(pool)
        .load_candidate(&scope, record.revision_uid)
        .await?
        .ok_or_else(|| {
            Error::StaleDispatch(format!(
                "next release candidate {} disappeared before dispatch",
                record.revision_uid
            ))
        })?;
    Ok(ReleaseEvaluationWorkflowRequest {
        tenant_id: record.tenant_id,
        outbox_uid: record.outbox_uid,
        revision_uid: record.revision_uid,
        activation_target: candidate.activation_target.class().as_str().to_string(),
        baseline_revision_uid: candidate
            .subject
            .serving_baseline
            .as_ref()
            .map(|baseline| baseline.revision_uid),
        identity,
    })
}

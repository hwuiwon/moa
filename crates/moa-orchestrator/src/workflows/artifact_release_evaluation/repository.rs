//! Persistence for release-evaluation dispatch, overlays, fixtures, and attempts.
//!
//! Every write here is fenced by the same two facts: the submission `generation`
//! and the exact `EvaluationSubjectV1` digest. A dispatch record is created with
//! both, a result is only accepted when both still match the open record, and a
//! record whose subject was superseded is abandoned rather than left to answer
//! later. That is the whole reason a superseded result cannot make any revision
//! ready.
//!
//! Two coalescing invariants live in the schema rather than in this code, which is
//! why concurrency cannot defeat them: `artifact_release_candidate` already allows
//! one `active` and one `pending` slot holder per artifact, and
//! `artifact_release_dispatch_outbox_open_uniq` additionally allows one *open
//! dispatch* per artifact. Ten concurrent submissions therefore serialize into one
//! running evaluation plus one newest waiting subject no matter how they interleave.

use chrono::{DateTime, Duration, Utc};
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    CandidateSubmission, DecisionOutcome, RecordDecision, ReleaseRepository, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationTargetClass, Digest32, EvaluationPlanSubject, EvaluationSubjectV1, ReleasePolicy,
    SimulatorPolicyBinding, TenantScope, overlay_token_hash,
};
use moa_core::types::identifiers::TenantId;
use moa_db::ScopedConn;
use moa_wire::experiments::{
    ARTIFACT_RELEASE_BASELINE_VARIANT_KEY, ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY,
    ArtifactReleaseExperimentBinding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row, types::Json as SqlJson};
use uuid::Uuid;

use std::collections::BTreeMap;

use super::Error;
use super::types::{
    ArmRole, AttemptReviewState, CohortVisibility, DispatchRecord, DispatchStatus, MergedCasePlan,
    PinnedDependency, ProvisionedArm, ProvisionedAttempt, ReleaseAttemptRow, ReleaseCase,
    ReleaseCasePack, ScenarioSource, dispatch_idempotency_key, merge_case_packs,
    release_seed_material,
};

/// How long a provisioned overlay may resolve before it stops answering.
///
/// An overlay outlives a normal trial by a wide margin but not indefinitely: a
/// crashed attempt must stop being able to resolve a draft revision on its own.
const OVERLAY_TTL_HOURS: i64 = 24;

/// How long a hidden cohort epoch lasts before rotation.
const HIDDEN_COHORT_PERIOD_DAYS: i64 = 7;

/// Name of the per-tenant base fixture snapshot every attempt clones from.
const BASE_FIXTURE_NAME: &str = "release-evaluation-base";

/// A submission plus the dispatch record written in the same transaction.
#[derive(Clone, Debug)]
pub struct SubmittedCandidate {
    /// What the release repository recorded.
    pub submission: CandidateSubmission,
    /// Dispatch record, present only when the submission took the active slot.
    pub dispatch: Option<DispatchRecord>,
    /// Dispatch records abandoned because a newer subject replaced them.
    pub abandoned_outbox_uids: Vec<Uuid>,
}

/// A dispatch record claimed by the evaluation workflow.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimedDispatch {
    /// The claimed record, with `status = dispatched`.
    pub record: DispatchRecord,
    /// Whether a previous invocation had already claimed it.
    ///
    /// A Restate replay sees `true` and reuses the runs the first invocation
    /// started rather than starting a second pair.
    pub already_claimed: bool,
}

/// A deterministic decision that passed the generation and digest fence.
#[derive(Clone, Debug)]
pub struct FencedDecision {
    /// What the release repository recorded.
    pub outcome: DecisionOutcome,
    /// Dispatch record the decision consumed.
    pub settled_outbox_uid: Uuid,
    /// Attempt row the verdict was recorded against.
    pub attempt_uid: Option<Uuid>,
    /// Dispatch record enqueued for the newly dispatched pending candidate.
    pub next: Option<DispatchRecord>,
}

/// Immutable plan and simulator inputs included in a release subject.
pub struct ReleaseSubjectEnvironment {
    /// Exact approved plan, case-pack, seed-contract, and evaluator hashes.
    pub plan: EvaluationPlanSubject,
    /// Exact certified simulator policy the approved plan executes.
    pub simulator: SimulatorPolicyBinding,
}

/// Postgres-backed release-evaluation repository.
#[derive(Clone)]
pub struct ReleaseEvaluationRepository {
    pool: PgPool,
}

impl ReleaseEvaluationRepository {
    /// Creates the repository with its artifact pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolves the approved release plan and its certified simulator binding.
    ///
    /// Submission calls this before constructing `EvaluationSubjectV1`, so the
    /// attestation binds the actual production plan/case cohort and simulator
    /// policy instead of document-shaped placeholders.
    pub async fn resolve_subject_environment(
        &self,
        tenant_id: TenantId,
        activation_target: ActivationTargetClass,
    ) -> Result<ReleaseSubjectEnvironment, Error> {
        let scope = TenantScope::new(tenant_id);
        let now = Utc::now();
        let mut conn = self.begin(&scope).await?;
        let case_plan = resolve_case_plan(conn.as_mut(), &scope, activation_target, now).await?;
        let plan_revision = moa_artifacts::registry::ArtifactRegistry::load_revision_in_tx(
            conn.as_mut(),
            case_plan.plan_revision_uid,
        )
        .await
        .map_err(|error| Error::Storage(error.to_string()))?;
        conn.commit().await.map_err(storage)?;
        if plan_revision.kind != ArtifactKind::ExperimentPlan
            || plan_revision.status != ArtifactStatus::Published
        {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan {} is not a published experiment_plan revision",
                case_plan.plan_revision_uid
            )));
        }
        let ArtifactDefinition::ExperimentPlan(definition) = &plan_revision.document.definition
        else {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan {} has the wrong definition",
                case_plan.plan_revision_uid
            )));
        };
        let release_policy = ReleaseRepository::new(self.pool.clone())
            .resolve_policy(&scope, activation_target)
            .await
            .map_err(Error::Release)?;
        let mut evaluator_versions = validate_release_scorecard(definition, &release_policy)?;
        let resolved_simulator =
            moa_experiments::simulator_policy::store::SimulatorPolicyStore::new(self.pool.clone())
                .resolve_policy(tenant_id, definition.simulator_policy, now)
                .await
                .map_err(|error| Error::CasePackInvalid(error.to_string()))?;
        let plan_hash =
            Digest32::from_slice(&plan_revision.canonical_hash).map_err(Error::Release)?;
        let scenario_dataset_hash = Digest32(
            moa_artifacts::canonical::canonical_hash(&case_plan)
                .map_err(|error| Error::CasePackInvalid(error.to_string()))?,
        );
        let seed_contract = serde_json::json!({
            "contract": "moa.experiment.paired_plan_seed.v1",
            "plan_revision_uid": case_plan.plan_revision_uid,
            "authoring_pack_uid": case_plan.authoring_pack_uid,
            "hidden_pack_uid": case_plan.hidden_pack_uid,
            "cohort_epoch": case_plan.cohort_epoch,
        });
        let seed_hash = Digest32(
            moa_artifacts::canonical::canonical_hash(&seed_contract)
                .map_err(|error| Error::CasePackInvalid(error.to_string()))?,
        );
        evaluator_versions.insert("artifact_release_decision".to_string(), "v1".to_string());
        evaluator_versions.insert(
            "simulator_fidelity".to_string(),
            resolved_simulator.binding.evaluator_version.to_string(),
        );
        Ok(ReleaseSubjectEnvironment {
            plan: EvaluationPlanSubject {
                plan_hash,
                scenario_dataset_hash,
                seed_hash,
                evaluator_versions,
            },
            simulator: SimulatorPolicyBinding {
                policy_uid: resolved_simulator.binding.policy_uid,
                revision: resolved_simulator.binding.revision,
                policy_hash: resolved_simulator.binding.policy_hash,
                certified_until: resolved_simulator.binding.certified_until,
            },
        })
    }

    /// Submits a candidate and its dispatch record in one transaction.
    ///
    /// Either both commit or neither does, which is the property that makes the
    /// outbox meaningful: there is no committed submission without a durable
    /// dispatch record, and no dispatch record for a submission that rolled back.
    pub async fn submit_with_dispatch(
        &self,
        request: SubmitCandidate,
        pinned_dependencies: Vec<PinnedDependency>,
    ) -> Result<SubmittedCandidate, Error> {
        let now = Utc::now();
        let scope = request.scope;
        let mut conn = self.begin(&scope).await?;
        let submission = ReleaseRepository::submit_candidate_in_tx(conn.as_mut(), &request, now)
            .await
            .map_err(Error::Release)?;
        let candidate = &submission.candidate;

        // Only a submission that took the active slot dispatches. A submission
        // that landed in the pending slot deliberately gets no dispatch record:
        // the active attempt keeps running, and the pending subject is dispatched
        // by the decision that frees the slot.
        let (dispatch, abandoned) = if submission.dispatched {
            // Anything still open for this artifact under a different
            // (revision, generation) names a subject that is no longer being
            // released -- an artifact whose previous attempt was superseded by an
            // activation, or this same revision resubmitted with a new subject.
            // Abandoning them is both the fence for their late results and what
            // frees the one-open-dispatch-per-artifact index.
            let abandoned = abandon_superseded_dispatch(
                conn.as_mut(),
                candidate.artifact_uid,
                candidate.revision_uid,
                candidate.generation,
                "superseded by a newer submitted subject",
                now,
            )
            .await?;
            for pin in &pinned_dependencies {
                ensure_pinned_dependency(conn.as_mut(), &scope, pin).await?;
            }
            let record = enqueue_dispatch(
                conn.as_mut(),
                &scope,
                candidate.revision_uid,
                candidate.artifact_uid,
                candidate.generation,
                candidate.subject_digest,
                candidate.attempt_count.max(1),
                &pinned_dependencies,
            )
            .await?;
            (Some(record), abandoned)
        } else {
            (None, Vec::new())
        };
        conn.commit().await.map_err(storage)?;
        Ok(SubmittedCandidate {
            submission,
            dispatch,
            abandoned_outbox_uids: abandoned,
        })
    }

    /// Claims a dispatch record for one evaluation workflow invocation.
    ///
    /// Returns `None` when the record was abandoned or already settled: a workflow
    /// that wakes up for a superseded subject must dispatch nothing.
    pub async fn claim_dispatch(
        &self,
        tenant_id: TenantId,
        outbox_uid: Uuid,
    ) -> Result<Option<ClaimedDispatch>, Error> {
        let scope = TenantScope::new(tenant_id);
        let now = Utc::now();
        let mut conn = self.begin(&scope).await?;
        let Some(record) = load_dispatch(conn.as_mut(), &scope, outbox_uid, true).await? else {
            conn.commit().await.map_err(storage)?;
            return Ok(None);
        };
        let claimed = match record.status {
            DispatchStatus::Pending => {
                sqlx::query(
                    r#"
                    UPDATE moa.artifact_release_dispatch_outbox
                    SET status = 'dispatched',
                        dispatched_at = $2,
                        updated_at = $2
                    WHERE outbox_uid = $1
                      AND status = 'pending'
                    "#,
                )
                .bind(outbox_uid)
                .bind(now)
                .execute(&mut *conn.as_mut())
                .await
                .map_err(storage)?;
                Some(ClaimedDispatch {
                    record: DispatchRecord {
                        status: DispatchStatus::Dispatched,
                        ..record
                    },
                    already_claimed: false,
                })
            }
            DispatchStatus::Dispatched => Some(ClaimedDispatch {
                record,
                already_claimed: true,
            }),
            DispatchStatus::Settled | DispatchStatus::Abandoned => None,
        };
        conn.commit().await.map_err(storage)?;
        Ok(claimed)
    }

    /// Provisions the case plan, overlays, fixtures, and attempt row for a record.
    ///
    /// Idempotent in every write, so a Restate replay re-reads what the first
    /// invocation created instead of provisioning a second environment. The one
    /// thing it cannot re-derive is the overlay token, which the caller journals.
    pub async fn provision_attempt(
        &self,
        record: &DispatchRecord,
        activation_target: ActivationTargetClass,
        baseline_revision_uid: Option<Uuid>,
        overlay_tokens: &[(ArmRole, String)],
    ) -> Result<ProvisionedAttempt, Error> {
        let scope = TenantScope::new(record.tenant_id);
        let now = Utc::now();
        let mut conn = self.begin(&scope).await?;

        let plan = resolve_case_plan(conn.as_mut(), &scope, activation_target, now).await?;
        ensure_plan_matches_subject(conn.as_mut(), &self.pool, &scope, record, &plan, now).await?;
        ensure_cohort_budget(
            conn.as_mut(),
            &scope,
            record.artifact_uid,
            plan.hidden_pack_uid,
            plan.cohort_epoch,
            record.outbox_uid,
        )
        .await?;
        bind_plan_to_dispatch(conn.as_mut(), record.outbox_uid, &plan, now).await?;

        let base_fixture_uid = ensure_base_fixture(conn.as_mut(), &scope).await?;
        let mut arms = Vec::new();
        let mut wanted = vec![(ArmRole::Candidate, record.revision_uid)];
        if let Some(baseline) = baseline_revision_uid {
            wanted.push((ArmRole::Baseline, baseline));
        }
        for (role, revision_uid) in wanted {
            let token = overlay_tokens
                .iter()
                .find(|(candidate_role, _)| *candidate_role == role)
                .map(|(_, token)| token.clone())
                .ok_or_else(|| {
                    Error::Provisioning(format!(
                        "no overlay token was journaled for the {role} arm"
                    ))
                })?;
            let fixture_uid =
                clone_fixture(conn.as_mut(), base_fixture_uid, record.outbox_uid, role).await?;
            let arm = upsert_overlay(
                conn.as_mut(),
                &scope,
                record,
                role,
                revision_uid,
                fixture_uid,
                &token,
                now,
            )
            .await?;
            arms.push(arm);
        }

        let attempt_uid =
            upsert_attempt(conn.as_mut(), &scope, record, activation_target, &plan).await?;
        conn.commit().await.map_err(storage)?;
        Ok(ProvisionedAttempt {
            attempt_uid,
            plan,
            arms,
        })
    }

    /// Records the experiment runs the dispatch started.
    pub async fn record_dispatched_runs(
        &self,
        tenant_id: TenantId,
        outbox_uid: Uuid,
        candidate_run_uid: Uuid,
        baseline_run_uid: Option<Uuid>,
    ) -> Result<(), Error> {
        let scope = TenantScope::new(tenant_id);
        let now = Utc::now();
        let mut conn = self.begin(&scope).await?;
        let updated = sqlx::query(
            r#"
            UPDATE moa.artifact_release_dispatch_outbox
            SET candidate_run_uid = $2,
                baseline_run_uid = $3,
                dispatched_at = coalesce(dispatched_at, $4),
                updated_at = $4
            WHERE outbox_uid = $1
              AND storage_partition_id = $5
              AND status = 'dispatched'
            "#,
        )
        .bind(outbox_uid)
        .bind(candidate_run_uid)
        .bind(baseline_run_uid)
        .bind(now)
        .bind(scope.storage_partition_id().to_string())
        .execute(&mut *conn.as_mut())
        .await
        .map_err(storage)?
        .rows_affected();
        if updated != 1 {
            conn.commit().await.map_err(storage)?;
            return Err(Error::StaleDispatch(format!(
                "dispatch record {outbox_uid} is no longer the open attempt"
            )));
        }
        sqlx::query(
            r#"
            UPDATE moa.artifact_release_attempt
            SET candidate_run_uid = $2,
                baseline_run_uid = $3,
                updated_at = $4
            WHERE outbox_uid = $1
            "#,
        )
        .bind(outbox_uid)
        .bind(candidate_run_uid)
        .bind(baseline_run_uid)
        .bind(now)
        .execute(&mut *conn.as_mut())
        .await
        .map_err(storage)?;
        conn.commit().await.map_err(storage)?;
        Ok(())
    }

    /// Records a deterministic decision under the generation and digest fence.
    ///
    /// One transaction does all of it: fence the open dispatch record on both
    /// `(generation, subject digest)`, apply the release decision, close the
    /// overlays, record the verdict on the attempt, and enqueue the dispatch record
    /// for whichever pending candidate took the freed active slot. The order is
    /// load-bearing -- the fence runs *before* the candidate state transition, so a
    /// superseded result aborts instead of moving a revision to `ready` and then
    /// discovering the problem.
    ///
    /// A refused fence is written down rather than dropped: the attempt row is
    /// marked `fenced_out`, which is the observable evidence that the fence fired
    /// and which the schema refuses to let carry an attestation.
    pub async fn record_decision_with_fence(
        &self,
        decision: RecordDecision,
        generation: i64,
        detail: Value,
    ) -> Result<FencedDecision, Error> {
        let now = Utc::now();
        let scope = decision.scope;
        let mut conn = self.begin(&scope).await?;
        let settled = Self::settle_dispatch_in_tx(
            conn.as_mut(),
            &scope,
            decision.candidate_revision_uid,
            generation,
            decision.subject_digest,
            now,
        )
        .await;
        let settled_outbox_uid = match settled {
            Ok(outbox_uid) => outbox_uid,
            Err(error) => {
                drop(conn);
                let reason = error.to_string();
                self.record_fenced_result(
                    scope.tenant_id(),
                    decision.candidate_revision_uid,
                    generation,
                    &reason,
                )
                .await?;
                return Err(error);
            }
        };
        let outcome = ReleaseRepository::record_decision_in_tx(conn.as_mut(), &decision, now)
            .await
            .map_err(Error::Release)?;
        let attempt_uid = Self::record_attempt_verdict_in_tx(
            conn.as_mut(),
            settled_outbox_uid,
            decision.verdict.as_str(),
            outcome
                .attestation
                .as_ref()
                .map(|attestation| attestation.attestation_uid),
            &detail,
            now,
        )
        .await?;
        let next = match outcome.dispatched_revision_uid {
            None => None,
            Some(next_revision_uid) => {
                Some(Self::enqueue_dispatch_in_tx(conn.as_mut(), &scope, next_revision_uid).await?)
            }
        };
        conn.commit().await.map_err(storage)?;
        Ok(FencedDecision {
            outcome,
            settled_outbox_uid,
            attempt_uid,
            next,
        })
    }

    /// Settles the open dispatch record for a decision, fenced by generation.
    ///
    /// The fence runs before the candidate state transition, so a result produced
    /// for a superseded generation aborts the whole transaction instead of moving a
    /// candidate to `ready` and then discovering the problem.
    async fn settle_dispatch_in_tx(
        conn: &mut PgConnection,
        scope: &TenantScope,
        revision_uid: Uuid,
        generation: i64,
        subject_digest: Digest32,
        now: DateTime<Utc>,
    ) -> Result<Uuid, Error> {
        let outbox_uid: Option<Uuid> = sqlx::query_scalar(
            r#"
            UPDATE moa.artifact_release_dispatch_outbox
            SET status = 'settled',
                settled_at = $5,
                updated_at = $5
            WHERE revision_uid = $1
              AND storage_partition_id = $2
              AND generation = $3
              AND subject_digest = $4
              AND status = 'dispatched'
            RETURNING outbox_uid
            "#,
        )
        .bind(revision_uid)
        .bind(scope.storage_partition_id().to_string())
        .bind(generation)
        .bind(subject_digest.to_vec())
        .bind(now)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
        let outbox_uid = outbox_uid.ok_or_else(|| {
            Error::StaleDispatch(format!(
                "no open dispatch record for revision {revision_uid} at generation {generation} and subject {subject_digest}"
            ))
        })?;
        close_overlays(&mut *conn, outbox_uid, now).await?;
        Ok(outbox_uid)
    }

    /// Records a fenced-out result against the attempt for a stale generation.
    ///
    /// A refused result is written down rather than dropped. A fenced-out attempt
    /// is the observable evidence that the fence worked, and the schema refuses to
    /// let it carry an attestation.
    pub async fn record_fenced_result(
        &self,
        tenant_id: TenantId,
        revision_uid: Uuid,
        generation: i64,
        reason: &str,
    ) -> Result<Option<Uuid>, Error> {
        let scope = TenantScope::new(tenant_id);
        let mut conn = self.begin(&scope).await?;
        let attempt_uid: Option<Uuid> = sqlx::query_scalar(
            r#"
            UPDATE moa.artifact_release_attempt
            SET fenced_out = true,
                fence_reason = $4,
                updated_at = now()
            WHERE revision_uid = $1
              AND storage_partition_id = $2
              AND generation = $3
              AND attestation_uid IS NULL
            RETURNING attempt_uid
            "#,
        )
        .bind(revision_uid)
        .bind(scope.storage_partition_id().to_string())
        .bind(generation)
        .bind(reason)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(storage)?;
        conn.commit().await.map_err(storage)?;
        Ok(attempt_uid)
    }

    /// Applies a decision outcome to the attempt row inside the caller's transaction.
    async fn record_attempt_verdict_in_tx(
        conn: &mut PgConnection,
        outbox_uid: Uuid,
        verdict: &str,
        attestation_uid: Option<Uuid>,
        detail: &Value,
        now: DateTime<Utc>,
    ) -> Result<Option<Uuid>, Error> {
        sqlx::query_scalar(
            r#"
            UPDATE moa.artifact_release_attempt
            SET verdict = $2,
                attestation_uid = $3,
                verdict_detail = $4,
                updated_at = $5
            WHERE outbox_uid = $1
            RETURNING attempt_uid
            "#,
        )
        .bind(outbox_uid)
        .bind(verdict)
        .bind(attestation_uid)
        .bind(SqlJson(detail))
        .bind(now)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)
    }

    /// Enqueues a dispatch record for a candidate the decision just dispatched.
    async fn enqueue_dispatch_in_tx(
        conn: &mut PgConnection,
        scope: &TenantScope,
        revision_uid: Uuid,
    ) -> Result<DispatchRecord, Error> {
        let row = sqlx::query(
            r#"
            SELECT artifact_uid, generation, subject_digest, attempt_count
            FROM moa.artifact_release_candidate
            WHERE revision_uid = $1
              AND storage_partition_id = $2
            "#,
        )
        .bind(revision_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            Error::Storage(format!(
                "dispatched candidate {revision_uid} has no release candidate row"
            ))
        })?;
        let artifact_uid: Uuid = row.try_get("artifact_uid").map_err(storage)?;
        let generation: i64 = row.try_get("generation").map_err(storage)?;
        let digest_bytes: Vec<u8> = row.try_get("subject_digest").map_err(storage)?;
        let attempt_count: i32 = row.try_get("attempt_count").map_err(storage)?;
        let subject_digest = Digest32::from_slice(&digest_bytes).map_err(Error::Release)?;
        let pinned = load_pinned_dependencies(&mut *conn, revision_uid).await?;
        enqueue_dispatch(
            &mut *conn,
            scope,
            revision_uid,
            artifact_uid,
            generation,
            subject_digest,
            attempt_count.max(1),
            &pinned,
        )
        .await
    }

    /// Lists the release-attempt review surface for a tenant.
    ///
    /// Hidden cohort case bodies are never included: the epoch is reported so an
    /// operator can tell attempts apart, and nothing more, because a tenant that
    /// could read the cohort could iterate against it.
    pub async fn list_attempts(
        &self,
        tenant_id: TenantId,
        limit: i64,
    ) -> Result<Vec<ReleaseAttemptRow>, Error> {
        let scope = TenantScope::new(tenant_id);
        let mut conn = self.begin(&scope).await?;
        let rows = sqlx::query(&format!(
            r#"
            SELECT {ATTEMPT_COLUMNS}
            FROM moa.artifact_release_attempt
            WHERE storage_partition_id = $1
            ORDER BY created_at DESC, attempt_uid DESC
            LIMIT $2
            "#
        ))
        .bind(scope.storage_partition_id().to_string())
        .bind(limit.clamp(1, 200))
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(storage)?;
        conn.commit().await.map_err(storage)?;
        rows.iter().map(attempt_from_row).collect()
    }

    /// Records attestation review against one release attempt.
    pub async fn review_attempt(
        &self,
        tenant_id: TenantId,
        attempt_uid: Uuid,
        state: AttemptReviewState,
        reviewer: &str,
        note: Option<&str>,
    ) -> Result<ReleaseAttemptRow, Error> {
        if state == AttemptReviewState::Unreviewed {
            return Err(Error::ReviewInvalid(
                "a review cannot record the unreviewed state".to_string(),
            ));
        }
        let scope = TenantScope::new(tenant_id);
        let mut conn = self.begin(&scope).await?;
        let row = sqlx::query(&format!(
            r#"
            UPDATE moa.artifact_release_attempt
            SET review_state = $3,
                reviewed_by = $4,
                reviewed_at = now(),
                review_note = $5,
                updated_at = now()
            WHERE attempt_uid = $1
              AND storage_partition_id = $2
            RETURNING {ATTEMPT_COLUMNS}
            "#
        ))
        .bind(attempt_uid)
        .bind(scope.storage_partition_id().to_string())
        .bind(state.as_str())
        .bind(reviewer)
        .bind(note)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(storage)?;
        conn.commit().await.map_err(storage)?;
        let row = row.ok_or_else(|| {
            Error::ReviewInvalid(format!(
                "release attempt {attempt_uid} does not exist in this tenant"
            ))
        })?;
        attempt_from_row(&row)
    }

    /// Resolves a revision through an evaluation overlay.
    ///
    /// The only overlay read path. It requires the overlay secret and the
    /// eval-owned session bound to that overlay, so a normal session cannot reach
    /// a draft revision through it even if it learned the overlay id.
    pub async fn resolve_overlay_revision(
        &self,
        tenant_id: TenantId,
        overlay_uid: Uuid,
        overlay_token: &str,
        eval_session_id: Uuid,
        artifact_uid: Uuid,
    ) -> Result<Option<Uuid>, Error> {
        let scope = TenantScope::new(tenant_id);
        let mut conn = self.begin(&scope).await?;
        let resolved: Option<Uuid> =
            sqlx::query_scalar("SELECT moa.resolve_release_overlay_revision($1, $2, $3, $4, $5)")
                .bind(overlay_uid)
                .bind(overlay_token_hash(overlay_token).to_vec())
                .bind(eval_session_id)
                .bind(artifact_uid)
                .bind(Utc::now())
                .fetch_optional(&mut *conn.as_mut())
                .await
                .map_err(storage)?
                .flatten();
        conn.commit().await.map_err(storage)?;
        Ok(resolved)
    }

    /// Verifies that an internal experiment request exactly matches its durable
    /// dispatch, attempt, and live overlay rows before run admission.
    ///
    /// The overlay token is a capability, but possession alone is not enough to
    /// alter a normal Behavior Lab run: every public request field is rebound to
    /// the server-provisioned attempt here before the experiment row is written.
    pub async fn validate_experiment_binding(
        &self,
        tenant_id: TenantId,
        plan_revision_uid: Uuid,
        idempotency_key: &str,
        binding: &ArtifactReleaseExperimentBinding,
    ) -> Result<(), Error> {
        let scope = TenantScope::new(tenant_id);
        let mut conn = self.begin(&scope).await?;
        let attempt = sqlx::query(
            r#"
            SELECT dispatch.status,
                   dispatch.revision_uid,
                   dispatch.idempotency_key,
                   dispatch.generation,
                   dispatch.subject_digest,
                   attempt.activation_target,
                   attempt.verdict,
                   attempt.fenced_out,
                   attempt.verdict_detail
            FROM moa.artifact_release_dispatch_outbox dispatch
            JOIN moa.artifact_release_attempt attempt
              ON attempt.outbox_uid = dispatch.outbox_uid
            WHERE dispatch.outbox_uid = $1
              AND dispatch.storage_partition_id = $2
              AND attempt.storage_partition_id = $2
            "#,
        )
        .bind(binding.outbox_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            Error::ExperimentBindingInvalid(format!(
                "dispatch {} has no provisioned attempt in this tenant",
                binding.outbox_uid
            ))
        })?;
        let status: String = attempt.try_get("status").map_err(storage)?;
        let revision_uid: Uuid = attempt.try_get("revision_uid").map_err(storage)?;
        let stored_key: String = attempt.try_get("idempotency_key").map_err(storage)?;
        let generation: i64 = attempt.try_get("generation").map_err(storage)?;
        let subject_digest: Vec<u8> = attempt.try_get("subject_digest").map_err(storage)?;
        let activation_target: String = attempt.try_get("activation_target").map_err(storage)?;
        let verdict: Option<String> = attempt.try_get("verdict").map_err(storage)?;
        let fenced_out: bool = attempt.try_get("fenced_out").map_err(storage)?;
        let detail: Value = attempt.try_get("verdict_detail").map_err(storage)?;
        let stored_plan_uid = detail
            .get("plan_revision_uid")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Uuid>().ok());
        let stored_case_plan = detail
            .get("case_plan")
            .cloned()
            .ok_or_else(|| {
                Error::ExperimentBindingInvalid(
                    "release attempt has no durable case plan".to_string(),
                )
            })
            .and_then(|value| {
                serde_json::from_value::<MergedCasePlan>(value).map_err(|error| {
                    Error::ExperimentBindingInvalid(format!(
                        "release attempt case plan is unreadable: {error}"
                    ))
                })
            })?;
        if status != DispatchStatus::Dispatched.as_str() || verdict.is_some() || fenced_out {
            return Err(Error::ExperimentBindingInvalid(format!(
                "dispatch {} is not an open provisioned attempt",
                binding.outbox_uid
            )));
        }
        if stored_key != idempotency_key {
            return Err(Error::ExperimentBindingInvalid(
                "experiment idempotency key does not match the release dispatch".to_string(),
            ));
        }
        if activation_target != binding.activation_target {
            return Err(Error::ExperimentBindingInvalid(
                "experiment activation target does not match the release attempt".to_string(),
            ));
        }
        if stored_plan_uid != Some(plan_revision_uid) {
            return Err(Error::ExperimentBindingInvalid(
                "experiment plan revision does not match the approved release plan".to_string(),
            ));
        }
        if stored_case_plan.experiment_cases()? != binding.cases {
            return Err(Error::ExperimentBindingInvalid(
                "experiment cases do not match the durable release attempt".to_string(),
            ));
        }

        let overlays = sqlx::query(
            r#"
            SELECT overlay_uid, role, revision_uid, eval_session_id, overlay_token_hash
            FROM moa.artifact_release_eval_overlay
            WHERE outbox_uid = $1
              AND storage_partition_id = $2
              AND generation = $3
              AND subject_digest = $4
              AND closed_at IS NULL
              AND expires_at > now()
            ORDER BY role
            "#,
        )
        .bind(binding.outbox_uid)
        .bind(scope.storage_partition_id().to_string())
        .bind(generation)
        .bind(subject_digest)
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(storage)?;
        if overlays.len() != binding.arms.len() || !(1..=2).contains(&overlays.len()) {
            return Err(Error::ExperimentBindingInvalid(
                "experiment arms do not match the live release overlays".to_string(),
            ));
        }

        let mut candidate_seen = false;
        let mut baseline_seen = false;
        for arm in &binding.arms {
            let row = overlays
                .iter()
                .find(|row| row.get::<Uuid, _>("overlay_uid") == arm.overlay_uid)
                .ok_or_else(|| {
                    Error::ExperimentBindingInvalid(format!(
                        "overlay {} is not owned by dispatch {}",
                        arm.overlay_uid, binding.outbox_uid
                    ))
                })?;
            let role: String = row.try_get("role").map_err(storage)?;
            match (role.as_str(), arm.variant_key.as_str()) {
                ("candidate", ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY) if !candidate_seen => {
                    candidate_seen = true;
                }
                ("baseline", ARTIFACT_RELEASE_BASELINE_VARIANT_KEY) if !baseline_seen => {
                    baseline_seen = true;
                }
                _ => {
                    return Err(Error::ExperimentBindingInvalid(format!(
                        "overlay {} has an invalid or duplicate release role",
                        arm.overlay_uid
                    )));
                }
            }
            let stored_revision_uid: Uuid = row.try_get("revision_uid").map_err(storage)?;
            let stored_session_id: Uuid = row.try_get("eval_session_id").map_err(storage)?;
            let stored_token_hash: Vec<u8> = row.try_get("overlay_token_hash").map_err(storage)?;
            if stored_revision_uid != arm.revision_uid
                || stored_session_id != arm.eval_session_id
                || stored_token_hash != overlay_token_hash(&arm.overlay_token).to_vec()
            {
                return Err(Error::ExperimentBindingInvalid(format!(
                    "overlay {} does not match its provisioned revision, session, or token",
                    arm.overlay_uid
                )));
            }
        }
        if !candidate_seen
            || binding
                .arms
                .iter()
                .find(|arm| arm.variant_key == ARTIFACT_RELEASE_CANDIDATE_VARIANT_KEY)
                .is_none_or(|arm| arm.revision_uid != revision_uid)
        {
            return Err(Error::ExperimentBindingInvalid(
                "release experiment does not carry the dispatch candidate revision".to_string(),
            ));
        }
        conn.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn begin(&self, scope: &TenantScope) -> Result<ScopedConn<'_>, Error> {
        ScopedConn::begin_tenant(&self.pool, scope.tenant_id())
            .await
            .map_err(storage)
    }
}

async fn ensure_plan_matches_subject(
    conn: &mut PgConnection,
    pool: &PgPool,
    scope: &TenantScope,
    record: &DispatchRecord,
    plan: &MergedCasePlan,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    let subject: Value = sqlx::query_scalar(
        r#"
        SELECT subject
        FROM moa.artifact_release_candidate
        WHERE revision_uid = $1
          AND storage_partition_id = $2
          AND generation = $3
          AND subject_digest = $4
        "#,
    )
    .bind(record.revision_uid)
    .bind(scope.storage_partition_id().to_string())
    .bind(record.generation)
    .bind(record.subject_digest.to_vec())
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or_else(|| {
        Error::StaleDispatch(format!(
            "release subject for dispatch {} is no longer current",
            record.outbox_uid
        ))
    })?;
    let subject: EvaluationSubjectV1 = serde_json::from_value(subject).map_err(|error| {
        Error::Storage(format!("stored release subject is unreadable: {error}"))
    })?;
    let case_hash = Digest32(
        moa_artifacts::canonical::canonical_hash(plan)
            .map_err(|error| Error::CasePackInvalid(error.to_string()))?,
    );
    if subject.plan.scenario_dataset_hash != case_hash {
        return Err(Error::StaleDispatch(
            "approved release case cohort changed after submission".to_string(),
        ));
    }
    let plan_revision = moa_artifacts::registry::ArtifactRegistry::load_revision_in_tx(
        conn,
        plan.plan_revision_uid,
    )
    .await
    .map_err(|error| Error::Storage(error.to_string()))?;
    let plan_hash = Digest32::from_slice(&plan_revision.canonical_hash).map_err(Error::Release)?;
    if subject.plan.plan_hash != plan_hash {
        return Err(Error::StaleDispatch(
            "approved release plan changed after submission".to_string(),
        ));
    }
    let ArtifactDefinition::ExperimentPlan(definition) = &plan_revision.document.definition else {
        return Err(Error::CasePackInvalid(
            "approved release plan has the wrong artifact definition".to_string(),
        ));
    };
    let resolved =
        moa_experiments::simulator_policy::store::SimulatorPolicyStore::new(pool.clone())
            .resolve_policy(scope.tenant_id(), definition.simulator_policy, now)
            .await
            .map_err(|error| Error::CasePackInvalid(error.to_string()))?;
    let simulator = SimulatorPolicyBinding {
        policy_uid: resolved.binding.policy_uid,
        revision: resolved.binding.revision,
        policy_hash: resolved.binding.policy_hash,
        certified_until: resolved.binding.certified_until,
    };
    if subject.simulator.as_ref() != Some(&simulator) {
        return Err(Error::StaleDispatch(
            "certified simulator binding changed after submission".to_string(),
        ));
    }
    Ok(())
}

fn validate_release_scorecard(
    definition: &moa_artifacts::simulation::ExperimentPlanDefinition,
    policy: &ReleasePolicy,
) -> Result<BTreeMap<String, String>, Error> {
    let scorecard = definition.scorecard.as_ref().ok_or_else(|| {
        Error::CasePackInvalid("approved release plan declares no scorecard".to_string())
    })?;
    let mut outputs = BTreeMap::new();
    let mut versions = BTreeMap::new();
    for requirement in scorecard.requirements() {
        let descriptor = moa_experiments::evaluator::descriptor(
            &requirement.evaluator_id,
            &requirement.evaluator_version,
        )
        .map_err(|error| Error::CasePackInvalid(error.to_string()))?;
        outputs.insert(
            descriptor.score_name,
            (requirement, descriptor.determinism.permits_blocking()),
        );
        versions.insert(
            requirement.evaluator_id.clone(),
            requirement.evaluator_version.clone(),
        );
    }
    for assertion in &policy.blocking_assertions {
        let Some((requirement, deterministic)) = outputs.get(assertion.id.as_str()) else {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan does not produce blocking assertion `{}`",
                assertion.id
            )));
        };
        if requirement.evaluator_version != assertion.version
            || !requirement.effect.is_blocking()
            || !deterministic
        {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan assertion `{}` is not the deterministic blocking version {}",
                assertion.id, assertion.version
            )));
        }
    }
    for metric in &policy.primary_gate_family {
        let Some((requirement, deterministic)) = outputs.get(metric.metric.as_str()) else {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan does not produce primary gate metric `{}`",
                metric.metric
            )));
        };
        if !requirement.effect.is_blocking() || !deterministic {
            return Err(Error::CasePackInvalid(format!(
                "approved release plan primary gate metric `{}` is not deterministic and blocking",
                metric.metric
            )));
        }
    }
    Ok(versions)
}

/// Columns the attempt review surface projects.
const ATTEMPT_COLUMNS: &str = "attempt_uid, outbox_uid, revision_uid, artifact_uid, generation, \
     subject_digest, activation_target, candidate_run_uid, baseline_run_uid, cohort_epoch, \
     verdict, attestation_uid, fenced_out, fence_reason, review_state, reviewed_by, reviewed_at, \
     review_note, created_at";

fn storage<E: std::fmt::Display>(error: E) -> Error {
    Error::Storage(error.to_string())
}

fn attempt_from_row(row: &sqlx::postgres::PgRow) -> Result<ReleaseAttemptRow, Error> {
    let digest: Vec<u8> = row.try_get("subject_digest").map_err(storage)?;
    Ok(ReleaseAttemptRow {
        attempt_uid: row.try_get("attempt_uid").map_err(storage)?,
        outbox_uid: row.try_get("outbox_uid").map_err(storage)?,
        revision_uid: row.try_get("revision_uid").map_err(storage)?,
        artifact_uid: row.try_get("artifact_uid").map_err(storage)?,
        generation: row.try_get("generation").map_err(storage)?,
        subject_digest: Digest32::from_slice(&digest)
            .map_err(Error::Release)?
            .to_string(),
        activation_target: row.try_get("activation_target").map_err(storage)?,
        candidate_run_uid: row.try_get("candidate_run_uid").map_err(storage)?,
        baseline_run_uid: row.try_get("baseline_run_uid").map_err(storage)?,
        cohort_epoch: row.try_get("cohort_epoch").map_err(storage)?,
        verdict: row.try_get("verdict").map_err(storage)?,
        attestation_uid: row.try_get("attestation_uid").map_err(storage)?,
        fenced_out: row.try_get("fenced_out").map_err(storage)?,
        fence_reason: row.try_get("fence_reason").map_err(storage)?,
        review_state: row.try_get("review_state").map_err(storage)?,
        reviewed_by: row.try_get("reviewed_by").map_err(storage)?,
        reviewed_at: row.try_get("reviewed_at").map_err(storage)?,
        review_note: row.try_get("review_note").map_err(storage)?,
        created_at: row.try_get("created_at").map_err(storage)?,
    })
}

/// Refuses a pin whose revision is not a revision of that artifact in this tenant.
async fn ensure_pinned_dependency(
    conn: &mut PgConnection,
    scope: &TenantScope,
    pin: &PinnedDependency,
) -> Result<(), Error> {
    let exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.artifact_revision r
            JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
            WHERE r.revision_uid = $1
              AND r.artifact_uid = $2
              AND r.valid_to IS NULL
              AND a.valid_to IS NULL
              AND a.user_id IS NULL
              AND a.storage_partition_id = $3
        )
        "#,
    )
    .bind(pin.revision_uid)
    .bind(pin.artifact_uid)
    .bind(scope.storage_partition_id().to_string())
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    if !exists {
        return Err(Error::PinnedDependencyInvalid(format!(
            "pinned revision {} is not a live tenant-scoped revision of artifact {}",
            pin.revision_uid, pin.artifact_uid
        )));
    }
    Ok(())
}

async fn load_pinned_dependencies(
    conn: &mut PgConnection,
    revision_uid: Uuid,
) -> Result<Vec<PinnedDependency>, Error> {
    // Carried forward from the newest dispatch record for the same revision, so a
    // retry after an inconclusive result evaluates the same pinned lock.
    let pinned: Option<Value> = sqlx::query_scalar(
        r#"
        SELECT pinned_dependencies
        FROM moa.artifact_release_dispatch_outbox
        WHERE revision_uid = $1
        ORDER BY generation DESC
        LIMIT 1
        "#,
    )
    .bind(revision_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    match pinned {
        None => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value).map_err(|error| {
            Error::Storage(format!(
                "stored pinned dependencies are unreadable: {error}"
            ))
        }),
    }
}

/// Abandons open dispatch records for an artifact under a superseded subject.
///
/// The predicate is on `(revision_uid, generation)`, not on the revision alone: a
/// candidate resubmitted with a new subject keeps its revision id, and its old
/// dispatch record must still stop being able to decide anything.
async fn abandon_superseded_dispatch(
    conn: &mut PgConnection,
    artifact_uid: Uuid,
    keep_revision_uid: Uuid,
    keep_generation: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>, Error> {
    let abandoned: Vec<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE moa.artifact_release_dispatch_outbox
        SET status = 'abandoned',
            settled_at = $4,
            updated_at = $4
        WHERE artifact_uid = $1
          AND (revision_uid, generation) <> ($2, $3)
          AND status IN ('pending', 'dispatched')
        RETURNING outbox_uid
        "#,
    )
    .bind(artifact_uid)
    .bind(keep_revision_uid)
    .bind(keep_generation)
    .bind(now)
    .fetch_all(&mut *conn)
    .await
    .map_err(storage)?;
    for outbox_uid in &abandoned {
        close_overlays(&mut *conn, *outbox_uid, now).await?;
        sqlx::query(
            r#"
            UPDATE moa.artifact_release_attempt
            SET fenced_out = true,
                fence_reason = $2,
                updated_at = $3
            WHERE outbox_uid = $1
              AND attestation_uid IS NULL
            "#,
        )
        .bind(outbox_uid)
        .bind(reason)
        .bind(now)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
    }
    Ok(abandoned)
}

async fn close_overlays(
    conn: &mut PgConnection,
    outbox_uid: Uuid,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        r#"
        UPDATE moa.artifact_release_eval_overlay
        SET closed_at = $2
        WHERE outbox_uid = $1
          AND closed_at IS NULL
        "#,
    )
    .bind(outbox_uid)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "every argument is part of the fenced dispatch identity; bundling them would hide what the idempotency key covers"
)]
async fn enqueue_dispatch(
    conn: &mut PgConnection,
    scope: &TenantScope,
    revision_uid: Uuid,
    artifact_uid: Uuid,
    generation: i64,
    subject_digest: Digest32,
    attempt_no: i32,
    pinned_dependencies: &[PinnedDependency],
) -> Result<DispatchRecord, Error> {
    let idempotency_key = dispatch_idempotency_key(revision_uid, generation, &subject_digest);
    let seed_material =
        release_seed_material(scope.tenant_id(), revision_uid, generation, &subject_digest);
    let outbox_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_dispatch_outbox (
            outbox_uid, storage_partition_id, user_id, revision_uid, artifact_uid, generation,
            subject_digest, idempotency_key, status, seed_material, pinned_dependencies, attempt_no
        )
        VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, 'pending', $8, $9, $10)
        ON CONFLICT (idempotency_key) DO NOTHING
        "#,
    )
    .bind(outbox_uid)
    .bind(scope.storage_partition_id().to_string())
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(generation)
    .bind(subject_digest.to_vec())
    .bind(&idempotency_key)
    .bind(&seed_material)
    .bind(SqlJson(pinned_dependencies))
    .bind(attempt_no)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    load_dispatch_by_key(conn, scope, &idempotency_key).await
}

async fn load_dispatch_by_key(
    conn: &mut PgConnection,
    scope: &TenantScope,
    idempotency_key: &str,
) -> Result<DispatchRecord, Error> {
    let row = sqlx::query(&format!(
        r#"
        SELECT {DISPATCH_COLUMNS}
        FROM moa.artifact_release_dispatch_outbox
        WHERE idempotency_key = $1
          AND storage_partition_id = $2
        "#
    ))
    .bind(idempotency_key)
    .bind(scope.storage_partition_id().to_string())
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or_else(|| {
        Error::Storage(format!(
            "dispatch record for `{idempotency_key}` disappeared after insert"
        ))
    })?;
    dispatch_from_row(&row, scope.tenant_id())
}

/// Columns the dispatch record projects.
const DISPATCH_COLUMNS: &str = "outbox_uid, revision_uid, artifact_uid, generation, subject_digest, idempotency_key, status, \
     seed_material, pinned_dependencies, case_pack_uid, hidden_pack_uid, cohort_epoch, \
     candidate_run_uid, baseline_run_uid, attempt_no";

async fn load_dispatch(
    conn: &mut PgConnection,
    scope: &TenantScope,
    outbox_uid: Uuid,
    for_update: bool,
) -> Result<Option<DispatchRecord>, Error> {
    let statement = format!(
        r#"
        SELECT {DISPATCH_COLUMNS}
        FROM moa.artifact_release_dispatch_outbox
        WHERE outbox_uid = $1
          AND storage_partition_id = $2
        {}
        "#,
        if for_update { "FOR UPDATE" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(outbox_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    row.as_ref()
        .map(|row| dispatch_from_row(row, scope.tenant_id()))
        .transpose()
}

fn dispatch_from_row(
    row: &sqlx::postgres::PgRow,
    tenant_id: TenantId,
) -> Result<DispatchRecord, Error> {
    let digest: Vec<u8> = row.try_get("subject_digest").map_err(storage)?;
    let status: String = row.try_get("status").map_err(storage)?;
    let pinned: Value = row.try_get("pinned_dependencies").map_err(storage)?;
    Ok(DispatchRecord {
        outbox_uid: row.try_get("outbox_uid").map_err(storage)?,
        tenant_id,
        revision_uid: row.try_get("revision_uid").map_err(storage)?,
        artifact_uid: row.try_get("artifact_uid").map_err(storage)?,
        generation: row.try_get("generation").map_err(storage)?,
        subject_digest: Digest32::from_slice(&digest).map_err(Error::Release)?,
        idempotency_key: row.try_get("idempotency_key").map_err(storage)?,
        status: status.parse()?,
        seed_material: row.try_get("seed_material").map_err(storage)?,
        pinned_dependencies: serde_json::from_value(pinned).map_err(|error| {
            Error::Storage(format!(
                "stored pinned dependencies are unreadable: {error}"
            ))
        })?,
        case_pack_uid: row.try_get("case_pack_uid").map_err(storage)?,
        hidden_pack_uid: row.try_get("hidden_pack_uid").map_err(storage)?,
        cohort_epoch: row.try_get("cohort_epoch").map_err(storage)?,
        candidate_run_uid: row.try_get("candidate_run_uid").map_err(storage)?,
        baseline_run_uid: row.try_get("baseline_run_uid").map_err(storage)?,
        attempt_no: row.try_get("attempt_no").map_err(storage)?,
    })
}

/// Resolves the approved plan/scenario pack, rotating the hidden cohort when due.
async fn resolve_case_plan(
    conn: &mut PgConnection,
    scope: &TenantScope,
    activation_target: ActivationTargetClass,
    now: DateTime<Utc>,
) -> Result<MergedCasePlan, Error> {
    // Rotation happens before resolution so an expired cohort can never decide a
    // release: the attempt faces the new epoch or it faces nothing.
    let rotated: Option<Uuid> =
        sqlx::query_scalar("SELECT moa.rotate_release_hidden_cohort($1, $2, $3::INTERVAL)")
            .bind(activation_target.as_str())
            .bind(now)
            .bind(format!("{HIDDEN_COHORT_PERIOD_DAYS} days"))
            .fetch_one(&mut *conn)
            .await
            .map_err(storage)?;
    if let Some(pack_uid) = rotated {
        tracing::debug!(%pack_uid, %activation_target, "resolved current hidden release cohort");
    }

    let platform_authoring = load_case_pack(
        conn,
        None,
        activation_target,
        CohortVisibility::Authoring,
        scope,
    )
    .await?
    .ok_or_else(|| {
        Error::CasePackInvalid(format!(
            "no approved authoring pack resolves for {activation_target}"
        ))
    })?;
    let hidden = load_case_pack(
        conn,
        None,
        activation_target,
        CohortVisibility::Hidden,
        scope,
    )
    .await?
    .ok_or_else(|| {
        Error::CasePackInvalid(format!(
            "no hidden release cohort resolves for {activation_target}"
        ))
    })?;
    let tenant_supplement = load_case_pack(
        conn,
        Some(scope),
        activation_target,
        CohortVisibility::Authoring,
        scope,
    )
    .await?;

    let cohort: Value = sqlx::query_scalar("SELECT moa.select_release_hidden_cohort($1, $2)")
        .bind(hidden.pack_uid)
        .bind(hidden.cohort_epoch)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
    let hidden_cases: Vec<ReleaseCase> = serde_json::from_value(cohort).map_err(|error| {
        Error::CasePackInvalid(format!("hidden cohort window is unreadable: {error}"))
    })?;

    merge_case_packs(
        &platform_authoring,
        &hidden,
        hidden_cases,
        tenant_supplement.as_ref(),
    )
}

async fn load_case_pack(
    conn: &mut PgConnection,
    tenant_scope: Option<&TenantScope>,
    activation_target: ActivationTargetClass,
    visibility: CohortVisibility,
    scope: &TenantScope,
) -> Result<Option<ReleaseCasePack>, Error> {
    let row = sqlx::query(
        r#"
        SELECT pack_uid, storage_partition_id, name, revision, visibility, cohort_epoch,
               cohort_size, rotates_at, max_attempts_per_epoch, plan_revision_uid, cases,
               mandatory_assertions, scenario_source, pack_hash
        FROM moa.artifact_release_case_pack
        WHERE valid_to IS NULL
          AND target_class = $1
          AND visibility = $2
          AND storage_partition_id IS NOT DISTINCT FROM $3
        "#,
    )
    .bind(activation_target.as_str())
    .bind(visibility.as_str())
    .bind(tenant_scope.map(|scope| scope.storage_partition_id().to_string()))
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let visibility_text: String = row.try_get("visibility").map_err(storage)?;
    let cases: Value = row.try_get("cases").map_err(storage)?;
    let mandatory: Value = row.try_get("mandatory_assertions").map_err(storage)?;
    let scenario_source: Value = row.try_get("scenario_source").map_err(storage)?;
    let pack_hash: Vec<u8> = row.try_get("pack_hash").map_err(storage)?;
    let stored_partition: Option<String> = row.try_get("storage_partition_id").map_err(storage)?;
    let scenario_source: ScenarioSource =
        serde_json::from_value(scenario_source).map_err(|error| {
            Error::CasePackInvalid(format!("pack scenario source is unreadable: {error}"))
        })?;
    let cases: Vec<ReleaseCase> = serde_json::from_value(cases)
        .map_err(|error| Error::CasePackInvalid(format!("pack cases are unreadable: {error}")))?;
    Ok(Some(ReleaseCasePack {
        pack_uid: row.try_get("pack_uid").map_err(storage)?,
        name: row.try_get("name").map_err(storage)?,
        revision: row.try_get("revision").map_err(storage)?,
        tenant_id: stored_partition.map(|_| scope.tenant_id()),
        visibility: visibility_text.parse()?,
        cohort_epoch: row.try_get("cohort_epoch").map_err(storage)?,
        cohort_size: row.try_get("cohort_size").map_err(storage)?,
        rotates_at: row.try_get("rotates_at").map_err(storage)?,
        max_attempts_per_epoch: row.try_get("max_attempts_per_epoch").map_err(storage)?,
        plan_revision_uid: row.try_get("plan_revision_uid").map_err(storage)?,
        cases,
        mandatory_assertions: serde_json::from_value(mandatory).map_err(|error| {
            Error::CasePackInvalid(format!("pack mandatory assertions are unreadable: {error}"))
        })?,
        scenario_source,
        pack_hash: Digest32::from_slice(&pack_hash).map_err(Error::Release)?,
    }))
}

/// Refuses an attempt that would exceed the hidden-cohort budget for one epoch.
///
/// Rotation alone would not stop overfitting: a tenant could resubmit against the
/// same epoch until it passed. The budget is what makes iteration against the
/// hidden gate finite, and exceeding it fails closed rather than falling back to
/// the authoring cases.
async fn ensure_cohort_budget(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
    hidden_pack_uid: Uuid,
    cohort_epoch: i32,
    outbox_uid: Uuid,
) -> Result<(), Error> {
    let budget: Option<i32> = sqlx::query_scalar(
        "SELECT max_attempts_per_epoch FROM moa.artifact_release_case_pack WHERE pack_uid = $1",
    )
    .bind(hidden_pack_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .flatten();
    let Some(budget) = budget else {
        return Err(Error::CasePackInvalid(format!(
            "hidden cohort {hidden_pack_uid} declares no attempt budget"
        )));
    };
    let spent: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM moa.artifact_release_attempt
        WHERE storage_partition_id = $1
          AND artifact_uid = $2
          AND hidden_pack_uid = $3
          AND cohort_epoch = $4
          AND outbox_uid <> $5
        "#,
    )
    .bind(scope.storage_partition_id().to_string())
    .bind(artifact_uid)
    .bind(hidden_pack_uid)
    .bind(cohort_epoch)
    .bind(outbox_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    if spent >= i64::from(budget) {
        return Err(Error::HiddenCohortBudgetExhausted(format!(
            "artifact {artifact_uid} has spent {spent} of {budget} attempts against hidden cohort epoch {cohort_epoch}"
        )));
    }
    Ok(())
}

async fn bind_plan_to_dispatch(
    conn: &mut PgConnection,
    outbox_uid: Uuid,
    plan: &MergedCasePlan,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        r#"
        UPDATE moa.artifact_release_dispatch_outbox
        SET case_pack_uid = $2,
            hidden_pack_uid = $3,
            cohort_epoch = $4,
            updated_at = $5
        WHERE outbox_uid = $1
        "#,
    )
    .bind(outbox_uid)
    .bind(plan.authoring_pack_uid)
    .bind(plan.hidden_pack_uid)
    .bind(plan.cohort_epoch)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(())
}

/// Returns the tenant's base fixture snapshot, creating it on first use.
///
/// The base carries no connector bindings at all, which is the strongest form of
/// "never production credentials": there is nothing to leak, and the schema
/// refuses to let an operator add a non-fixture binding later.
async fn ensure_base_fixture(conn: &mut PgConnection, scope: &TenantScope) -> Result<Uuid, Error> {
    let partition = scope.storage_partition_id().to_string();
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT fixture_uid
        FROM moa.artifact_release_fixture
        WHERE storage_partition_id = $1
          AND name = $2
          AND base_fixture_uid IS NULL
        "#,
    )
    .bind(&partition)
    .bind(BASE_FIXTURE_NAME)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    if let Some(fixture_uid) = existing {
        return Ok(fixture_uid);
    }
    let environment = serde_json::json!({
        "kind": "release_evaluation_fixture",
        "version": 1,
        "records": [],
    });
    let content_hash = blake3::hash(environment.to_string().as_bytes());
    let fixture_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_fixture (
            fixture_uid, storage_partition_id, user_id, base_fixture_uid, outbox_uid, name,
            revision, environment, connector_bindings, content_hash, writable
        )
        VALUES ($1, $2, NULL, NULL, NULL, $3, 1, $4, '[]'::JSONB, $5, false)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(fixture_uid)
    .bind(&partition)
    .bind(BASE_FIXTURE_NAME)
    .bind(SqlJson(&environment))
    .bind(content_hash.as_bytes().to_vec())
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    sqlx::query_scalar(
        r#"
        SELECT fixture_uid
        FROM moa.artifact_release_fixture
        WHERE storage_partition_id = $1
          AND name = $2
          AND base_fixture_uid IS NULL
        "#,
    )
    .bind(&partition)
    .bind(BASE_FIXTURE_NAME)
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)
}

async fn clone_fixture(
    conn: &mut PgConnection,
    base_fixture_uid: Uuid,
    outbox_uid: Uuid,
    role: ArmRole,
) -> Result<Uuid, Error> {
    sqlx::query_scalar("SELECT moa.clone_release_fixture($1, $2, $3)")
        .bind(base_fixture_uid)
        .bind(outbox_uid)
        .bind(role.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)
}

#[allow(
    clippy::too_many_arguments,
    reason = "an overlay row is the isolation boundary; every field it binds is named here rather than hidden behind a builder"
)]
async fn upsert_overlay(
    conn: &mut PgConnection,
    scope: &TenantScope,
    record: &DispatchRecord,
    role: ArmRole,
    revision_uid: Uuid,
    fixture_uid: Uuid,
    overlay_token: &str,
    now: DateTime<Utc>,
) -> Result<ProvisionedArm, Error> {
    let artifact_uid: Uuid = sqlx::query_scalar(
        "SELECT artifact_uid FROM moa.artifact_revision WHERE revision_uid = $1 AND valid_to IS NULL",
    )
    .bind(revision_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or_else(|| {
        Error::Provisioning(format!(
            "overlay revision {revision_uid} does not exist or was invalidated"
        ))
    })?;
    let expires_at = now
        + Duration::try_hours(OVERLAY_TTL_HOURS)
            .ok_or_else(|| Error::Provisioning("overlay lifetime is unusable".to_string()))?;
    let overlay_uid = Uuid::now_v7();
    let eval_session_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_eval_overlay (
            overlay_uid, storage_partition_id, user_id, outbox_uid, role, artifact_uid,
            revision_uid, generation, subject_digest, pinned_dependencies, overlay_token_hash,
            eval_session_id, fixture_uid, expires_at
        )
        VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (outbox_uid, role) DO NOTHING
        "#,
    )
    .bind(overlay_uid)
    .bind(scope.storage_partition_id().to_string())
    .bind(record.outbox_uid)
    .bind(role.as_str())
    .bind(artifact_uid)
    .bind(revision_uid)
    .bind(record.generation)
    .bind(record.subject_digest.to_vec())
    .bind(SqlJson(&record.pinned_dependencies))
    .bind(overlay_token_hash(overlay_token).to_vec())
    .bind(eval_session_id)
    .bind(fixture_uid)
    .bind(expires_at)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    let row = sqlx::query(
        r#"
        SELECT overlay_uid, revision_uid, eval_session_id, fixture_uid
        FROM moa.artifact_release_eval_overlay
        WHERE outbox_uid = $1
          AND role = $2
        "#,
    )
    .bind(record.outbox_uid)
    .bind(role.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(ProvisionedArm {
        role,
        overlay_uid: row.try_get("overlay_uid").map_err(storage)?,
        overlay_token: overlay_token.to_string(),
        revision_uid: row.try_get("revision_uid").map_err(storage)?,
        eval_session_id: row.try_get("eval_session_id").map_err(storage)?,
        fixture_uid: row.try_get("fixture_uid").map_err(storage)?,
    })
}

async fn upsert_attempt(
    conn: &mut PgConnection,
    scope: &TenantScope,
    record: &DispatchRecord,
    activation_target: ActivationTargetClass,
    plan: &MergedCasePlan,
) -> Result<Uuid, Error> {
    let attempt_uid = Uuid::now_v7();
    let detail = serde_json::json!({
        "paired": true,
        "authoring_cases": plan.authoring_cases.len(),
        "hidden_cases": plan.hidden_cases.len(),
        "total_repetitions": plan.total_repetitions(),
        "mandatory_assertions": plan.mandatory_assertions,
        "plan_revision_uid": plan.plan_revision_uid,
        "case_plan": plan,
    });
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_attempt (
            attempt_uid, storage_partition_id, user_id, outbox_uid, artifact_uid, revision_uid,
            generation, subject_digest, activation_target, seed_material, case_pack_uid,
            hidden_pack_uid, cohort_epoch, verdict_detail
        )
        VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (outbox_uid) DO NOTHING
        "#,
    )
    .bind(attempt_uid)
    .bind(scope.storage_partition_id().to_string())
    .bind(record.outbox_uid)
    .bind(record.artifact_uid)
    .bind(record.revision_uid)
    .bind(record.generation)
    .bind(record.subject_digest.to_vec())
    .bind(activation_target.as_str())
    .bind(&record.seed_material)
    .bind(plan.authoring_pack_uid)
    .bind(plan.hidden_pack_uid)
    .bind(plan.cohort_epoch)
    .bind(SqlJson(&detail))
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    sqlx::query_scalar("SELECT attempt_uid FROM moa.artifact_release_attempt WHERE outbox_uid = $1")
        .bind(record.outbox_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)
}

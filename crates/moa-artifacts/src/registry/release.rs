//! Release-candidate persistence and the single legal activation path.
//!
//! Three writes matter here, and all three are fail-closed:
//!
//! * [`ReleaseRepository::submit_candidate`] turns an immutable draft into a
//!   release attempt. It resolves the gate policy server-side, builds the exact
//!   [`EvaluationSubjectV1`], and assigns the artifact's coalescing slot, so ten
//!   rapid submissions become one active attempt plus the newest pending subject.
//! * [`ReleaseRepository::record_decision`] applies a deterministic verdict. Only
//!   a pass mints an [`ActivationAttestation`]; a regression or an inconclusive
//!   result moves the candidate state, releases the active slot, and dispatches
//!   the pending newest subject.
//! * [`ReleaseRepository::activate`] is the only code path that moves a
//!   type-owned serving pointer. It checks tenant scope, candidate state and
//!   hash, the expected pointer version, the attestation's subject and
//!   spendability, and a full subject recomputation, then writes the audit row,
//!   consumes the attestation, and moves the pointer with a compare-and-set --
//!   all in one transaction.

use std::fmt::Display;

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_db::ScopedConn;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row, types::Json as SqlJson};
use uuid::Uuid;

use crate::canonical::canonical_hash;
use crate::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use crate::reference::ReferenceState;
use crate::registry::serving::load_serving_pointer_in_tx;
use crate::release::{
    ActivationAttestation, ActivationOutcome, ActivationRequest, ActivationTarget,
    ActivationTargetClass, AgentRuntimeSubject, AssertionRef, CatalogSnapshotBinding,
    DecisionProvenance, DeterministicVerdict, Digest32, EvaluationPlanSubject, EvaluationSubjectV1,
    EvidenceAdapter, ExpectedServing, PolicyIdentity, ReleasePolicy, ReleaseSlot, ReleaseState,
    ServingBaseline, SimulatorPolicyBinding, TenantScope,
};
use crate::validation::ValidationReport;
use crate::{Error, ReleaseRejection, Result};

/// A stored release candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseCandidate {
    /// Candidate revision.
    pub revision_uid: Uuid,
    /// Artifact the revision belongs to.
    pub artifact_uid: Uuid,
    /// Tenant owning the candidate.
    pub tenant_id: TenantId,
    /// Exact serving mutation this candidate would perform.
    pub activation_target: ActivationTarget,
    /// Candidate lifecycle state, read from the revision status.
    pub state: ReleaseState,
    /// Coalescing slot.
    pub slot: ReleaseSlot,
    /// Exact evaluation subject.
    pub subject: EvaluationSubjectV1,
    /// Digest over that subject.
    pub subject_digest: Digest32,
    /// Canonical hash of the candidate revision document.
    pub candidate_revision_hash: Digest32,
    /// Gate policy identity the subject was built with.
    pub policy: PolicyIdentity,
    /// Monotonic submission generation for the artifact.
    pub generation: i64,
    /// How many release attempts this candidate has had.
    pub attempt_count: i32,
    /// Experiment run of the latest attempt.
    pub last_run_uid: Option<Uuid>,
    /// Latest deterministic verdict recorded, if any.
    pub last_decision: Option<String>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Row update time.
    pub updated_at: DateTime<Utc>,
}

/// Subject inputs the caller must supply because only it can resolve them.
///
/// The repository never accepts the parts it can determine itself -- candidate
/// hash, serving baseline, and gate policy identity -- so a caller cannot make a
/// subject that claims a different baseline or a weaker gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateSubjectInputs {
    /// Hash of the resolved dependency lock.
    pub dependency_lock_hash: Digest32,
    /// Prompt, model, provider, and runtime policy for the subject.
    pub agent_runtime: AgentRuntimeSubject,
    /// Hash of the resolved tool policy.
    pub tool_policy_hash: Digest32,
    /// Whether the subject can call tools.
    pub tool_bearing: bool,
    /// Activated catalog snapshot; required when tool-bearing.
    pub tool_catalog: Option<CatalogSnapshotBinding>,
    /// Plan, scenario, seed, and evaluator versions.
    pub plan: EvaluationPlanSubject,
    /// Certified simulator policy, when the plan uses a simulator.
    pub simulator: Option<SimulatorPolicyBinding>,
}

/// A request to submit a candidate for release evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitCandidate {
    /// Tenant owning the candidate.
    pub scope: TenantScope,
    /// Exact serving mutation the candidate would perform.
    pub activation_target: ActivationTarget,
    /// Candidate revision.
    pub candidate_revision_uid: Uuid,
    /// Subject inputs only the caller can resolve.
    pub subject_inputs: CandidateSubjectInputs,
    /// Identity submitting the candidate.
    pub submitted_by: String,
}

/// Result of submitting a candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateSubmission {
    /// The stored candidate.
    pub candidate: ReleaseCandidate,
    /// Whether this submission took the artifact's active run slot.
    pub dispatched: bool,
    /// Candidate displaced from the pending slot, if any.
    pub displaced_pending_revision_uid: Option<Uuid>,
}

/// A deterministic release decision to record against an active attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordDecision {
    /// Tenant owning the candidate.
    pub scope: TenantScope,
    /// Candidate the decision is about.
    pub candidate_revision_uid: Uuid,
    /// Subject digest the evaluation actually ran; fences superseded results.
    pub subject_digest: Digest32,
    /// Deterministic verdict.
    pub verdict: DeterministicVerdict,
    /// Experiment run that produced the evidence.
    pub run_uid: Uuid,
    /// Trials that produced the evidence.
    pub trial_uids: Vec<Uuid>,
    /// Evidence rows consumed.
    pub evidence_ids: Vec<Uuid>,
    /// Per-metric gate outcomes.
    pub gate_results: std::collections::BTreeMap<String, String>,
    /// Blocking assertions evaluated.
    pub blocking_assertions: Vec<AssertionRef>,
    /// Which evidence surface produced the result.
    pub evidence_adapter: EvidenceAdapter,
    /// Identity recording the decision.
    pub decided_by: String,
}

/// Result of recording a deterministic decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionOutcome {
    /// Candidate state after the decision.
    pub state: ReleaseState,
    /// Attestation minted by a passing verdict.
    pub attestation: Option<ActivationAttestation>,
    /// Pending candidate dispatched into the freed active slot.
    pub dispatched_revision_uid: Option<Uuid>,
}

/// Postgres-backed release control repository.
#[derive(Clone)]
pub struct ReleaseRepository {
    pool: PgPool,
}

impl ReleaseRepository {
    /// Creates a release repository backed by a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolves the server-side gate policy for a tenant and activation class.
    ///
    /// A tenant override wins over the platform default, and the resolved policy
    /// is validated before it is allowed to gate anything. There is no caller
    /// input: the submitter cannot name, weaken, or skip its own gate.
    pub async fn resolve_policy(
        &self,
        scope: &TenantScope,
        class: ActivationTargetClass,
    ) -> Result<ReleasePolicy> {
        let mut conn = self.begin(scope).await?;
        let policy = resolve_policy_in_tx(conn.as_mut(), scope, class).await?;
        conn.commit().await.map_err(storage)?;
        Ok(policy)
    }

    /// Resolves the server-side gate policy inside the caller's transaction.
    ///
    /// This is the transactional counterpart to [`Self::resolve_policy`] for
    /// adapters that submit and decide a candidate atomically.
    pub async fn resolve_policy_in_tx(
        conn: &mut PgConnection,
        scope: &TenantScope,
        class: ActivationTargetClass,
    ) -> Result<ReleasePolicy> {
        resolve_policy_in_tx(conn, scope, class).await
    }

    /// Loads one candidate.
    pub async fn load_candidate(
        &self,
        scope: &TenantScope,
        revision_uid: Uuid,
    ) -> Result<Option<ReleaseCandidate>> {
        let mut conn = self.begin(scope).await?;
        let candidate = load_candidate_in_tx(conn.as_mut(), scope, revision_uid, false).await?;
        conn.commit().await.map_err(storage)?;
        Ok(candidate)
    }

    /// Reads the serving state an activation request must expect.
    ///
    /// Callers pass the value back in [`ActivationRequest::expected_serving`], so
    /// a pointer that moves between the read and the activation is a
    /// compare-and-set failure rather than a silent overwrite.
    pub async fn expected_serving(
        &self,
        scope: &TenantScope,
        target: &ActivationTarget,
    ) -> Result<ExpectedServing> {
        let mut conn = self.begin(scope).await?;
        let observed = match target {
            ActivationTarget::SkillVisibility { artifact_uid }
            | ActivationTarget::ActionVisibility { artifact_uid } => {
                let pointer =
                    load_serving_pointer_in_tx(conn.as_mut(), scope, *artifact_uid, false)
                        .await
                        .map_err(storage)?;
                ExpectedServing {
                    revision_uid: pointer.as_ref().map(|pointer| pointer.revision_uid),
                    pointer_version: pointer
                        .as_ref()
                        .map_or(0, |pointer| pointer.pointer_version),
                }
            }
            ActivationTarget::AgentDeployment {
                installation_uid, ..
            } => {
                let installation =
                    load_installation_in_tx(conn.as_mut(), scope, *installation_uid, false)
                        .await?
                        .ok_or_else(|| {
                            reject(
                                ReleaseRejection::InstallationNotFound,
                                format!("agent installation {installation_uid} is not active"),
                            )
                        })?;
                ExpectedServing {
                    revision_uid: installation.current_revision_uid,
                    pointer_version: installation.serving_pointer_version,
                }
            }
        };
        conn.commit().await.map_err(storage)?;
        Ok(observed)
    }

    /// Submits an immutable candidate revision for release evaluation.
    #[cfg(feature = "test-support")]
    pub async fn submit_candidate(&self, request: SubmitCandidate) -> Result<CandidateSubmission> {
        let now = Utc::now();
        let mut conn = self.begin(&request.scope).await?;
        let outcome = submit_candidate_in_tx(conn.as_mut(), &request, now).await?;
        conn.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Records a deterministic verdict against the active release attempt.
    #[cfg(feature = "test-support")]
    pub async fn record_decision(&self, request: RecordDecision) -> Result<DecisionOutcome> {
        let now = Utc::now();
        let mut conn = self.begin(&request.scope).await?;
        let outcome = record_decision_in_tx(conn.as_mut(), &request, now).await?;
        conn.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Moves a type-owned serving pointer. The only legal activation path.
    pub async fn activate(&self, request: ActivationRequest) -> Result<ActivationOutcome> {
        let now = Utc::now();
        let mut conn = self.begin(&request.scope).await?;
        let outcome = activate_in_tx(conn.as_mut(), &request, now).await?;
        conn.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Submits a candidate using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and must have applied matching MOA
    /// scope GUCs. Exposed so a promotion that also writes learning bookkeeping
    /// can be one transaction rather than three that can half-apply.
    pub async fn submit_candidate_in_tx(
        conn: &mut PgConnection,
        request: &SubmitCandidate,
        now: DateTime<Utc>,
    ) -> Result<CandidateSubmission> {
        submit_candidate_in_tx(conn, request, now).await
    }

    /// Records a deterministic decision using the caller's open transaction.
    pub async fn record_decision_in_tx(
        conn: &mut PgConnection,
        request: &RecordDecision,
        now: DateTime<Utc>,
    ) -> Result<DecisionOutcome> {
        record_decision_in_tx(conn, request, now).await
    }

    /// Activates a candidate using the caller's open transaction.
    ///
    /// Every predicate still runs, and the audit row, attestation consumption, and
    /// pointer move are still one atomic unit -- the caller's.
    pub async fn activate_in_tx(
        conn: &mut PgConnection,
        request: &ActivationRequest,
        now: DateTime<Utc>,
    ) -> Result<ActivationOutcome> {
        activate_in_tx(conn, request, now).await
    }

    /// Reads the serving state an activation must expect, in the caller's transaction.
    pub async fn expected_serving_in_tx(
        conn: &mut PgConnection,
        scope: &TenantScope,
        target: &ActivationTarget,
    ) -> Result<ExpectedServing> {
        match target {
            ActivationTarget::SkillVisibility { artifact_uid }
            | ActivationTarget::ActionVisibility { artifact_uid } => {
                let pointer = load_serving_pointer_in_tx(conn, scope, *artifact_uid, false)
                    .await
                    .map_err(storage)?;
                Ok(ExpectedServing {
                    revision_uid: pointer.as_ref().map(|pointer| pointer.revision_uid),
                    pointer_version: pointer
                        .as_ref()
                        .map_or(0, |pointer| pointer.pointer_version),
                })
            }
            ActivationTarget::AgentDeployment {
                installation_uid, ..
            } => {
                let installation = load_installation_in_tx(conn, scope, *installation_uid, false)
                    .await?
                    .ok_or_else(|| {
                        reject(
                            ReleaseRejection::InstallationNotFound,
                            format!("agent installation {installation_uid} is not active"),
                        )
                    })?;
                Ok(ExpectedServing {
                    revision_uid: installation.current_revision_uid,
                    pointer_version: installation.serving_pointer_version,
                })
            }
        }
    }

    async fn begin(&self, scope: &TenantScope) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_tenant(&self.pool, scope.tenant_id())
            .await
            .map_err(storage)
    }
}

fn storage<E: Display>(error: E) -> Error {
    Error::Storage(error.to_string())
}

fn reject(rejection: ReleaseRejection, detail: impl Into<String>) -> Error {
    Error::Release {
        rejection,
        detail: detail.into(),
    }
}

/// Revision facts the release predicates need, read under the caller's lock.
pub(crate) struct RevisionFacts {
    artifact_uid: Uuid,
    kind: ArtifactKind,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
    status: ArtifactStatus,
    version: i32,
    canonical_hash: Digest32,
    document: ArtifactDocument,
    validation_report: ValidationReport,
}

async fn load_revision_facts(
    conn: &mut PgConnection,
    revision_uid: Uuid,
    for_update: bool,
) -> Result<RevisionFacts> {
    let statement = format!(
        r#"
        SELECT r.artifact_uid, a.kind, a.storage_partition_id, a.user_id,
               r.status, r.version, r.canonical_hash, r.definition, r.validation_report
        FROM moa.artifact_revision r
        JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
        WHERE r.revision_uid = $1
          AND r.valid_to IS NULL
        {}
        "#,
        if for_update { "FOR UPDATE OF r" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(revision_uid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            reject(
                ReleaseRejection::CandidateNotFound,
                format!("artifact revision {revision_uid} does not exist or was invalidated"),
            )
        })?;
    let kind: String = row.try_get("kind").map_err(storage)?;
    let status: String = row.try_get("status").map_err(storage)?;
    let hash: Vec<u8> = row.try_get("canonical_hash").map_err(storage)?;
    let definition: Value = row.try_get("definition").map_err(storage)?;
    let validation_report: Value = row.try_get("validation_report").map_err(storage)?;
    Ok(RevisionFacts {
        artifact_uid: row.try_get("artifact_uid").map_err(storage)?,
        kind: kind.parse()?,
        storage_partition_id: row.try_get("storage_partition_id").map_err(storage)?,
        user_id: row.try_get("user_id").map_err(storage)?,
        status: status.parse()?,
        version: row.try_get("version").map_err(storage)?,
        canonical_hash: Digest32::from_slice(&hash)?,
        document: serde_json::from_value(definition)?,
        validation_report: serde_json::from_value(validation_report).unwrap_or_default(),
    })
}

/// Refuses a candidate that generic validation never accepted.
///
/// Validation is what makes an immutable revision eligible for evaluation, and it
/// is only meaningful when it ran with reference resolution: a report with no
/// errors but no resolution for a declared dependency proves nothing. Both halves
/// are required here, so a candidate cannot enter evaluation -- and therefore
/// cannot reach an attestation -- on the strength of an unvalidated import.
fn ensure_candidate_eligible(facts: &RevisionFacts) -> Result<()> {
    if !facts.validation_report.is_ok() {
        return Err(reject(
            ReleaseRejection::CandidateNotEligible,
            format!(
                "candidate revision has {} validation errors",
                facts.validation_report.errors.len()
            ),
        ));
    }
    for (path, artifact_ref) in facts.document.reference_paths() {
        let resolved = facts.validation_report.references.iter().any(|resolution| {
            resolution.path == path
                && resolution.artifact_ref == artifact_ref
                && resolution.state == ReferenceState::Resolved
        });
        if !resolved {
            return Err(reject(
                ReleaseRejection::CandidateNotEligible,
                format!("candidate reference {artifact_ref} at {path} is not resolved"),
            ));
        }
    }
    Ok(())
}

/// Refuses a revision that does not belong to the tenant and target it claims.
fn ensure_revision_matches_target(
    facts: &RevisionFacts,
    scope: &TenantScope,
    target: &ActivationTarget,
) -> Result<()> {
    let expected_partition = scope.storage_partition_id().to_string();
    if facts.storage_partition_id.as_deref() != Some(expected_partition.as_str()) {
        return Err(reject(
            ReleaseRejection::WrongTenant,
            format!(
                "artifact {} belongs to storage partition {:?}, not {expected_partition}",
                facts.artifact_uid, facts.storage_partition_id
            ),
        ));
    }
    if facts.user_id.is_some() {
        return Err(reject(
            ReleaseRejection::ContactScopeUnsupported,
            format!(
                "artifact {} is contact-scoped and has no release subject",
                facts.artifact_uid
            ),
        ));
    }
    if facts.artifact_uid != target.artifact_uid() {
        return Err(reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "revision belongs to artifact {} but the target names {}",
                facts.artifact_uid,
                target.artifact_uid()
            ),
        ));
    }
    let class = ActivationTargetClass::for_artifact_kind(&facts.kind).ok_or_else(|| {
        reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "artifact kind {} has no release-gated serving pointer",
                facts.kind
            ),
        )
    })?;
    if class != target.class() {
        return Err(reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "artifact kind {} is gated by {class}, not {}",
                facts.kind,
                target.class()
            ),
        ));
    }
    Ok(())
}

async fn resolve_policy_in_tx(
    conn: &mut PgConnection,
    scope: &TenantScope,
    class: ActivationTargetClass,
) -> Result<ReleasePolicy> {
    // A tenant override wins, then the platform default. Both rows are written
    // under a stronger authorization relation than candidate submission, and the
    // platform row is global-scope, so no tenant role can write it at all.
    let row = sqlx::query(
        r#"
        SELECT policy_uid, storage_partition_id, name, revision, target_class,
               blocking_assertions, primary_gate_family, attestation_ttl_secs,
               resource_policy_hash, policy_hash,
               policy_hash = moa.artifact_release_policy_content_hash(
                   name,
                   revision,
                   target_class,
                   blocking_assertions,
                   primary_gate_family,
                   attestation_ttl_secs,
                   resource_policy_hash
               ) AS policy_hash_matches
        FROM moa.artifact_release_policy
        WHERE valid_to IS NULL
          AND target_class = $2
          AND (storage_partition_id = $1 OR storage_partition_id IS NULL)
        ORDER BY (storage_partition_id IS NULL) ASC
        LIMIT 1
        "#,
    )
    .bind(scope.storage_partition_id().to_string())
    .bind(class.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or_else(|| {
        reject(
            ReleaseRejection::PolicyNotFound,
            format!("no release policy resolves for {class} in this tenant"),
        )
    })?;

    let blocking_assertions: Value = row.try_get("blocking_assertions").map_err(storage)?;
    let primary_gate_family: Value = row.try_get("primary_gate_family").map_err(storage)?;
    let stored_partition: Option<String> = row.try_get("storage_partition_id").map_err(storage)?;
    let resource_policy_hash: Vec<u8> = row.try_get("resource_policy_hash").map_err(storage)?;
    let policy_hash: Vec<u8> = row.try_get("policy_hash").map_err(storage)?;
    let policy_hash_matches: bool = row.try_get("policy_hash_matches").map_err(storage)?;
    if !policy_hash_matches {
        let policy_name: String = row.try_get("name").map_err(storage)?;
        let policy_revision: i32 = row.try_get("revision").map_err(storage)?;
        return Err(reject(
            ReleaseRejection::PolicyInvalid,
            format!(
                "release policy {policy_name} revision {policy_revision} content does not match its canonical hash"
            ),
        ));
    }
    let policy = ReleasePolicy {
        policy_uid: row.try_get("policy_uid").map_err(storage)?,
        tenant_id: stored_partition.map(|_| scope.tenant_id()),
        name: row.try_get("name").map_err(storage)?,
        revision: row.try_get("revision").map_err(storage)?,
        target_class: class,
        blocking_assertions: serde_json::from_value(blocking_assertions)?,
        primary_gate_family: serde_json::from_value(primary_gate_family)?,
        attestation_ttl_secs: row.try_get("attestation_ttl_secs").map_err(storage)?,
        resource_policy_hash: Digest32::from_slice(&resource_policy_hash)?,
        policy_hash: Digest32::from_slice(&policy_hash)?,
    };
    policy.validate()?;
    Ok(policy)
}

async fn load_candidate_in_tx(
    conn: &mut PgConnection,
    scope: &TenantScope,
    revision_uid: Uuid,
    for_update: bool,
) -> Result<Option<ReleaseCandidate>> {
    let statement = format!(
        r#"
        SELECT c.revision_uid, c.artifact_uid, c.storage_partition_id, c.activation_target,
               c.target_installation_uid, c.subject, c.subject_digest, c.candidate_revision_hash,
               c.policy_uid, c.policy_revision, c.policy_hash, c.slot, c.generation,
               c.attempt_count, c.last_run_uid, c.last_decision, c.created_at, c.updated_at,
               r.status
        FROM moa.artifact_release_candidate c
        JOIN moa.artifact_revision r ON r.revision_uid = c.revision_uid
        WHERE c.revision_uid = $1
          AND c.storage_partition_id = $2
        {}
        "#,
        if for_update { "FOR UPDATE OF c" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(revision_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let status: String = row.try_get("status").map_err(storage)?;
    let slot: String = row.try_get("slot").map_err(storage)?;
    let subject: Value = row.try_get("subject").map_err(storage)?;
    let subject_digest: Vec<u8> = row.try_get("subject_digest").map_err(storage)?;
    let candidate_hash: Vec<u8> = row.try_get("candidate_revision_hash").map_err(storage)?;
    let policy_hash: Vec<u8> = row.try_get("policy_hash").map_err(storage)?;
    let target: String = row.try_get("activation_target").map_err(storage)?;
    let installation_uid: Option<Uuid> = row.try_get("target_installation_uid").map_err(storage)?;
    let artifact_uid: Uuid = row.try_get("artifact_uid").map_err(storage)?;
    let class: ActivationTargetClass = target.parse()?;
    let activation_target =
        ActivationTarget::for_kind(&class.artifact_kind(), artifact_uid, installation_uid)?;
    let status: ArtifactStatus = status.parse()?;
    Ok(Some(ReleaseCandidate {
        revision_uid: row.try_get("revision_uid").map_err(storage)?,
        artifact_uid,
        tenant_id: scope.tenant_id(),
        activation_target,
        state: ReleaseState::from_artifact_status(&status)?,
        slot: slot.parse()?,
        subject: serde_json::from_value(subject)?,
        subject_digest: Digest32::from_slice(&subject_digest)?,
        candidate_revision_hash: Digest32::from_slice(&candidate_hash)?,
        policy: PolicyIdentity {
            policy_uid: row.try_get("policy_uid").map_err(storage)?,
            revision: row.try_get("policy_revision").map_err(storage)?,
            policy_hash: Digest32::from_slice(&policy_hash)?,
        },
        generation: row.try_get("generation").map_err(storage)?,
        attempt_count: row.try_get("attempt_count").map_err(storage)?,
        last_run_uid: row.try_get("last_run_uid").map_err(storage)?,
        last_decision: row.try_get("last_decision").map_err(storage)?,
        created_at: row.try_get("created_at").map_err(storage)?,
        updated_at: row.try_get("updated_at").map_err(storage)?,
    }))
}

async fn load_attestation_in_tx(
    conn: &mut PgConnection,
    attestation_uid: Uuid,
    for_update: bool,
    scope: Option<&TenantScope>,
) -> Result<Option<ActivationAttestation>> {
    // Deliberately not filtered by tenant: a wrong-tenant attestation must be
    // refused as a wrong tenant, not reported as a missing row, so the predicate
    // that catches it is observable.
    let statement = format!(
        r#"
        SELECT attestation_uid, storage_partition_id, artifact_uid, candidate_revision_uid,
               activation_target, target_installation_uid, subject_digest, verdict, run_uid,
               trial_uids, evidence_ids, decision, created_at, expires_at, consumed_at,
               consumed_by_audit_uid
        FROM moa.artifact_activation_attestation
        WHERE attestation_uid = $1
        {}
        "#,
        if for_update { "FOR UPDATE" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(attestation_uid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored_partition: String = row.try_get("storage_partition_id").map_err(storage)?;
    if let Some(scope) = scope {
        let expected = scope.storage_partition_id().to_string();
        if stored_partition != expected {
            return Err(reject(
                ReleaseRejection::WrongTenant,
                format!("attestation {attestation_uid} belongs to another tenant"),
            ));
        }
    }
    let target: String = row.try_get("activation_target").map_err(storage)?;
    let class: ActivationTargetClass = target.parse()?;
    let artifact_uid: Uuid = row.try_get("artifact_uid").map_err(storage)?;
    let installation_uid: Option<Uuid> = row.try_get("target_installation_uid").map_err(storage)?;
    let subject_digest: Vec<u8> = row.try_get("subject_digest").map_err(storage)?;
    let decision: Value = row.try_get("decision").map_err(storage)?;
    let tenant_id = tenant_from_partition(&stored_partition)?;
    Ok(Some(ActivationAttestation {
        attestation_uid: row.try_get("attestation_uid").map_err(storage)?,
        tenant_id,
        activation_target: ActivationTarget::for_kind(
            &class.artifact_kind(),
            artifact_uid,
            installation_uid,
        )?,
        candidate_revision_uid: row.try_get("candidate_revision_uid").map_err(storage)?,
        subject_digest: Digest32::from_slice(&subject_digest)?,
        run_uid: row.try_get("run_uid").map_err(storage)?,
        trial_uids: row.try_get("trial_uids").map_err(storage)?,
        evidence_ids: row.try_get("evidence_ids").map_err(storage)?,
        decision: serde_json::from_value(decision)?,
        created_at: row.try_get("created_at").map_err(storage)?,
        expires_at: row.try_get("expires_at").map_err(storage)?,
        consumed_at: row.try_get("consumed_at").map_err(storage)?,
        consumed_by_audit_uid: row.try_get("consumed_by_audit_uid").map_err(storage)?,
    }))
}

fn tenant_from_partition(storage_partition_id: &str) -> Result<TenantId> {
    Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .map_err(|error| {
            Error::Storage(format!(
                "storage partition `{storage_partition_id}` is not a tenant id: {error}"
            ))
        })
}

async fn set_revision_state(
    conn: &mut PgConnection,
    revision_uid: Uuid,
    from: ReleaseState,
    to: ReleaseState,
) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let next = from.transition_to(to)?;
    let updated = sqlx::query(
        r#"
        UPDATE moa.artifact_revision
        SET status = $2,
            updated_at = now()
        WHERE revision_uid = $1
          AND status = $3
          AND valid_to IS NULL
        "#,
    )
    .bind(revision_uid)
    .bind(next.artifact_status().as_str())
    .bind(from.artifact_status().as_str())
    .execute(&mut *conn)
    .await
    .map_err(storage)?
    .rows_affected();
    if updated != 1 {
        return Err(reject(
            ReleaseRejection::IllegalStateTransition,
            format!("revision {revision_uid} was not in state {from} when moving to {next}"),
        ));
    }
    Ok(())
}

async fn set_candidate_slot(
    conn: &mut PgConnection,
    revision_uid: Uuid,
    slot: ReleaseSlot,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.artifact_release_candidate
        SET slot = $2,
            updated_at = now()
        WHERE revision_uid = $1
        "#,
    )
    .bind(revision_uid)
    .bind(slot.as_str())
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    Ok(())
}

/// Returns the candidate holding a slot for an artifact, if any.
async fn slot_holder(
    conn: &mut PgConnection,
    artifact_uid: Uuid,
    slot: ReleaseSlot,
) -> Result<Option<(Uuid, ReleaseState)>> {
    let row = sqlx::query(
        r#"
        SELECT c.revision_uid, r.status
        FROM moa.artifact_release_candidate c
        JOIN moa.artifact_revision r ON r.revision_uid = c.revision_uid
        WHERE c.artifact_uid = $1
          AND c.slot = $2
        FOR UPDATE OF c
        "#,
    )
    .bind(artifact_uid)
    .bind(slot.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let status: String = row.try_get("status").map_err(storage)?;
    let status: ArtifactStatus = status.parse()?;
    Ok(Some((
        row.try_get("revision_uid").map_err(storage)?,
        ReleaseState::from_artifact_status(&status)?,
    )))
}

async fn next_generation(conn: &mut PgConnection, artifact_uid: Uuid) -> Result<i64> {
    let generation = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT max(generation) FROM moa.artifact_release_candidate WHERE artifact_uid = $1",
    )
    .bind(artifact_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?
    .unwrap_or(0);
    Ok(generation.saturating_add(1))
}

async fn submit_candidate_in_tx(
    conn: &mut PgConnection,
    request: &SubmitCandidate,
    now: DateTime<Utc>,
) -> Result<CandidateSubmission> {
    let facts = load_revision_facts(conn, request.candidate_revision_uid, true).await?;
    ensure_revision_matches_target(&facts, &request.scope, &request.activation_target)?;

    ensure_candidate_eligible(&facts)?;

    let state = ReleaseState::from_artifact_status(&facts.status)?;
    if !state.is_retryable() {
        return Err(reject(
            ReleaseRejection::IllegalStateTransition,
            format!(
                "candidate {} is {state} and cannot start a release attempt",
                request.candidate_revision_uid
            ),
        ));
    }

    if let ActivationTarget::AgentDeployment {
        installation_uid, ..
    } = request.activation_target
    {
        ensure_installation(conn, &request.scope, facts.artifact_uid, installation_uid).await?;
    }

    let policy =
        resolve_policy_in_tx(conn, &request.scope, request.activation_target.class()).await?;
    let baseline = serving_baseline(
        conn,
        &request.scope,
        facts.artifact_uid,
        &request.activation_target,
    )
    .await?;

    let subject = EvaluationSubjectV1 {
        subject_version: EvaluationSubjectV1::VERSION,
        tenant_id: request.scope.tenant_id(),
        activation_target: request.activation_target,
        candidate_revision_uid: request.candidate_revision_uid,
        candidate_revision_hash: facts.canonical_hash,
        serving_baseline: baseline,
        dependency_lock_hash: request.subject_inputs.dependency_lock_hash,
        agent_runtime: request.subject_inputs.agent_runtime.clone(),
        tool_policy_hash: request.subject_inputs.tool_policy_hash,
        tool_bearing: request.subject_inputs.tool_bearing,
        tool_catalog: request.subject_inputs.tool_catalog.clone(),
        plan: request.subject_inputs.plan.clone(),
        simulator: request.subject_inputs.simulator.clone(),
        release_policy: policy.identity(),
        resource_policy_hash: policy.resource_policy_hash,
    };
    subject.validate(now)?;
    let subject_digest = subject.digest()?;

    let active = slot_holder(conn, facts.artifact_uid, ReleaseSlot::Active).await?;
    let takes_active_slot = match &active {
        None => true,
        Some((holder, _)) => *holder == request.candidate_revision_uid,
    };
    let generation = next_generation(conn, facts.artifact_uid).await?;

    let mut displaced_pending = None;
    let slot = if takes_active_slot {
        ReleaseSlot::Active
    } else {
        // Coalesce: only the newest waiting subject stays pending. The one it
        // replaces is superseded, not queued, so tenant impatience cannot grow an
        // unbounded backlog of stale subjects.
        if let Some((pending_uid, pending_state)) =
            slot_holder(conn, facts.artifact_uid, ReleaseSlot::Pending).await?
            && pending_uid != request.candidate_revision_uid
        {
            set_candidate_slot(conn, pending_uid, ReleaseSlot::Released).await?;
            set_revision_state(conn, pending_uid, pending_state, ReleaseState::Superseded).await?;
            displaced_pending = Some(pending_uid);
        }
        ReleaseSlot::Pending
    };

    let installation_uid = request.activation_target.installation_uid();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_candidate (
            revision_uid, artifact_uid, storage_partition_id, user_id, activation_target,
            target_installation_uid, subject, subject_digest, candidate_revision_hash,
            policy_uid, policy_revision, policy_hash, slot, generation, attempt_count,
            submitted_by
        )
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (revision_uid) DO UPDATE
        SET subject = EXCLUDED.subject,
            subject_digest = EXCLUDED.subject_digest,
            policy_uid = EXCLUDED.policy_uid,
            policy_revision = EXCLUDED.policy_revision,
            policy_hash = EXCLUDED.policy_hash,
            slot = EXCLUDED.slot,
            generation = EXCLUDED.generation,
            attempt_count = moa.artifact_release_candidate.attempt_count + 1,
            submitted_by = EXCLUDED.submitted_by,
            updated_at = now()
        "#,
    )
    .bind(request.candidate_revision_uid)
    .bind(facts.artifact_uid)
    .bind(request.scope.storage_partition_id().to_string())
    .bind(request.activation_target.class().as_str())
    .bind(installation_uid)
    .bind(SqlJson(&subject))
    .bind(subject_digest.to_vec())
    .bind(facts.canonical_hash.to_vec())
    .bind(policy.policy_uid)
    .bind(policy.revision)
    .bind(policy.policy_hash.to_vec())
    .bind(slot.as_str())
    .bind(generation)
    .bind(if takes_active_slot { 1_i32 } else { 0_i32 })
    .bind(&request.submitted_by)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    if takes_active_slot {
        set_revision_state(
            conn,
            request.candidate_revision_uid,
            state,
            ReleaseState::Evaluating,
        )
        .await?;
    }

    let candidate =
        load_candidate_in_tx(conn, &request.scope, request.candidate_revision_uid, false)
            .await
            .and_then(|candidate| {
                candidate.ok_or_else(|| {
                    Error::Storage("submitted release candidate disappeared".to_string())
                })
            })?;
    Ok(CandidateSubmission {
        candidate,
        dispatched: takes_active_slot,
        displaced_pending_revision_uid: displaced_pending,
    })
}

async fn load_candidate_in_tx_required(
    conn: &mut PgConnection,
    scope: &TenantScope,
    revision_uid: Uuid,
    for_update: bool,
) -> Result<ReleaseCandidate> {
    load_candidate_in_tx(conn, scope, revision_uid, for_update)
        .await?
        .ok_or_else(|| {
            reject(
                ReleaseRejection::CandidateNotFound,
                format!("no release candidate exists for revision {revision_uid} in this tenant"),
            )
        })
}

async fn record_decision_in_tx(
    conn: &mut PgConnection,
    request: &RecordDecision,
    now: DateTime<Utc>,
) -> Result<DecisionOutcome> {
    let candidate =
        load_candidate_in_tx_required(conn, &request.scope, request.candidate_revision_uid, true)
            .await?;
    if candidate.slot != ReleaseSlot::Active || candidate.state != ReleaseState::Evaluating {
        return Err(reject(
            ReleaseRejection::IllegalStateTransition,
            format!(
                "candidate {} is {} in slot {} and holds no active release attempt",
                candidate.revision_uid, candidate.state, candidate.slot
            ),
        ));
    }
    // Fence by subject digest: a result produced for a superseded subject cannot
    // decide anything about the candidate that is running now.
    if candidate.subject_digest != request.subject_digest {
        return Err(reject(
            ReleaseRejection::SubjectDigestMismatch,
            format!(
                "decision names subject {} but candidate {} is running subject {}",
                request.subject_digest, candidate.revision_uid, candidate.subject_digest
            ),
        ));
    }

    let next_state = request.verdict.candidate_state();
    set_revision_state(conn, candidate.revision_uid, candidate.state, next_state).await?;
    sqlx::query(
        r#"
        UPDATE moa.artifact_release_candidate
        SET slot = 'released',
            last_run_uid = $2,
            last_decision = $3,
            updated_at = now()
        WHERE revision_uid = $1
        "#,
    )
    .bind(candidate.revision_uid)
    .bind(request.run_uid)
    .bind(request.verdict.as_str())
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    let attestation = if request.verdict == DeterministicVerdict::Pass {
        // The policy is re-resolved and re-validated at mint time, and the
        // subject is re-validated against `now`, so an expired simulator
        // certification or a missing catalog snapshot cannot mint a permission to
        // serve even if it was fine at submission.
        let policy =
            resolve_policy_in_tx(conn, &request.scope, candidate.activation_target.class()).await?;
        if policy.identity() != candidate.policy {
            return Err(reject(
                ReleaseRejection::PolicyInvalid,
                format!(
                    "release policy moved from {:?} to {:?} during the attempt",
                    candidate.policy,
                    policy.identity()
                ),
            ));
        }
        candidate.subject.validate(now)?;
        Some(mint_attestation(conn, request, &candidate, &policy, now).await?)
    } else {
        None
    };

    // The active slot is free now, so the newest pending subject runs. This is
    // what stops an inconclusive result from starving a waiting candidate.
    let dispatched = dispatch_pending(conn, &request.scope, candidate.artifact_uid).await?;

    Ok(DecisionOutcome {
        state: next_state,
        attestation,
        dispatched_revision_uid: dispatched,
    })
}

async fn dispatch_pending(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
) -> Result<Option<Uuid>> {
    if slot_holder(conn, artifact_uid, ReleaseSlot::Active)
        .await?
        .is_some()
    {
        return Ok(None);
    }
    let Some((pending_uid, pending_state)) =
        slot_holder(conn, artifact_uid, ReleaseSlot::Pending).await?
    else {
        return Ok(None);
    };
    if !pending_state.is_retryable() {
        return Ok(None);
    }
    set_candidate_slot(conn, pending_uid, ReleaseSlot::Active).await?;
    set_revision_state(conn, pending_uid, pending_state, ReleaseState::Evaluating).await?;
    sqlx::query(
        r#"
        UPDATE moa.artifact_release_candidate
        SET attempt_count = attempt_count + 1,
            updated_at = now()
        WHERE revision_uid = $1
        "#,
    )
    .bind(pending_uid)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;
    // Re-read under the tenant scope so a cross-tenant row could not be
    // dispatched by artifact id alone.
    load_candidate_in_tx_required(conn, scope, pending_uid, false).await?;
    Ok(Some(pending_uid))
}

async fn mint_attestation(
    conn: &mut PgConnection,
    request: &RecordDecision,
    candidate: &ReleaseCandidate,
    policy: &ReleasePolicy,
    now: DateTime<Utc>,
) -> Result<ActivationAttestation> {
    if request.trial_uids.is_empty() || request.evidence_ids.is_empty() {
        return Err(reject(
            ReleaseRejection::VerdictNotPass,
            "a passing verdict must name at least one trial and one evidence row".to_string(),
        ));
    }
    for assertion in &request.blocking_assertions {
        if assertion.determinism != crate::release::DeterminismClass::Deterministic {
            return Err(reject(
                ReleaseRejection::PolicyInvalid,
                format!(
                    "blocking assertion {} is not deterministic; only deterministic evidence may block",
                    assertion.id
                ),
            ));
        }
    }
    let exact_policy_assertions = request.blocking_assertions.len()
        == policy.blocking_assertions.len()
        && policy.blocking_assertions.iter().all(|required| {
            request
                .blocking_assertions
                .iter()
                .any(|actual| actual.id == required.id && actual.version == required.version)
        });
    if !exact_policy_assertions {
        return Err(reject(
            ReleaseRejection::VerdictNotPass,
            format!(
                "passing decision blocker identities do not exactly match release policy {} revision {}",
                policy.name, policy.revision
            ),
        ));
    }
    let attestation_uid = Uuid::now_v7();
    let expires_at = now
        + chrono::Duration::try_seconds(policy.attestation_ttl_secs).ok_or_else(|| {
            reject(
                ReleaseRejection::PolicyInvalid,
                format!(
                    "release policy {} declares an unusable attestation lifetime",
                    policy.name
                ),
            )
        })?;
    let decision = DecisionProvenance {
        policy: policy.identity(),
        verdict: request.verdict,
        gate_results: request.gate_results.clone(),
        blocking_assertions: request.blocking_assertions.clone(),
        decided_by: request.decided_by.clone(),
        decided_at: now,
        evidence_adapter: request.evidence_adapter,
    };

    sqlx::query(
        r#"
        INSERT INTO moa.artifact_activation_attestation (
            attestation_uid, storage_partition_id, user_id, artifact_uid,
            candidate_revision_uid, activation_target, target_installation_uid, subject_digest,
            verdict, run_uid, trial_uids, evidence_ids, decision, policy_uid, policy_revision,
            policy_hash, decided_by, created_at, expires_at
        )
        VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, 'pass', $8, $9, $10, $11, $12, $13, $14, $15,
                $16, $17)
        "#,
    )
    .bind(attestation_uid)
    .bind(request.scope.storage_partition_id().to_string())
    .bind(candidate.artifact_uid)
    .bind(candidate.revision_uid)
    .bind(candidate.activation_target.class().as_str())
    .bind(candidate.activation_target.installation_uid())
    .bind(candidate.subject_digest.to_vec())
    .bind(request.run_uid)
    .bind(&request.trial_uids)
    .bind(&request.evidence_ids)
    .bind(SqlJson(&decision))
    .bind(policy.policy_uid)
    .bind(policy.revision)
    .bind(policy.policy_hash.to_vec())
    .bind(&request.decided_by)
    .bind(now)
    .bind(expires_at)
    .execute(&mut *conn)
    .await
    .map_err(storage)?;

    Ok(ActivationAttestation {
        attestation_uid,
        tenant_id: request.scope.tenant_id(),
        activation_target: candidate.activation_target,
        candidate_revision_uid: candidate.revision_uid,
        subject_digest: candidate.subject_digest,
        run_uid: request.run_uid,
        trial_uids: request.trial_uids.clone(),
        evidence_ids: request.evidence_ids.clone(),
        decision,
        created_at: now,
        expires_at,
        consumed_at: None,
        consumed_by_audit_uid: None,
    })
}

/// Reads the serving baseline a candidate is compared against.
async fn serving_baseline(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
    target: &ActivationTarget,
) -> Result<Option<ServingBaseline>> {
    match target {
        ActivationTarget::SkillVisibility { .. } | ActivationTarget::ActionVisibility { .. } => {
            Ok(load_serving_pointer_in_tx(conn, scope, artifact_uid, false)
                .await
                .map_err(storage)?
                .map(|pointer| ServingBaseline {
                    revision_uid: pointer.revision_uid,
                    revision_hash: pointer.revision_hash,
                    pointer_version: pointer.pointer_version,
                }))
        }
        ActivationTarget::AgentDeployment {
            installation_uid, ..
        } => {
            let installation = load_installation_in_tx(conn, scope, *installation_uid, false)
                .await?
                .ok_or_else(|| {
                    reject(
                        ReleaseRejection::InstallationNotFound,
                        format!("agent installation {installation_uid} is not active"),
                    )
                })?;
            match installation.current_revision_uid {
                None => Ok(None),
                Some(revision_uid) => {
                    let facts = load_revision_facts(conn, revision_uid, false).await?;
                    Ok(Some(ServingBaseline {
                        revision_uid,
                        revision_hash: facts.canonical_hash,
                        pointer_version: installation.serving_pointer_version,
                    }))
                }
            }
        }
    }
}

/// Agent installation facts the activation transaction needs.
struct InstallationFacts {
    artifact_uid: Uuid,
    user_id: Option<String>,
    current_revision_uid: Option<Uuid>,
    serving_pointer_version: i64,
}

async fn load_installation_in_tx(
    conn: &mut PgConnection,
    scope: &TenantScope,
    installation_uid: Uuid,
    for_update: bool,
) -> Result<Option<InstallationFacts>> {
    let statement = format!(
        r#"
        SELECT artifact_uid, user_id, current_revision_uid, serving_pointer_version
        FROM moa.agent_installation
        WHERE installation_uid = $1
          AND storage_partition_id = $2
          AND status <> 'retired'
        {}
        "#,
        if for_update { "FOR UPDATE" } else { "" }
    );
    let row = sqlx::query(&statement)
        .bind(installation_uid)
        .bind(scope.storage_partition_id().to_string())
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(InstallationFacts {
        artifact_uid: row.try_get("artifact_uid").map_err(storage)?,
        user_id: row.try_get("user_id").map_err(storage)?,
        current_revision_uid: row.try_get("current_revision_uid").map_err(storage)?,
        serving_pointer_version: row.try_get("serving_pointer_version").map_err(storage)?,
    }))
}

async fn ensure_installation(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
    installation_uid: Uuid,
) -> Result<InstallationFacts> {
    let installation = load_installation_in_tx(conn, scope, installation_uid, false)
        .await?
        .ok_or_else(|| {
            reject(
                ReleaseRejection::InstallationNotFound,
                format!(
                    "agent installation {installation_uid} is missing, retired, or in another tenant"
                ),
            )
        })?;
    if installation.artifact_uid != artifact_uid {
        return Err(reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "installation {installation_uid} was installed from artifact {} not {artifact_uid}",
                installation.artifact_uid
            ),
        ));
    }
    if installation.user_id.is_some() {
        return Err(reject(
            ReleaseRejection::ContactScopeUnsupported,
            format!("installation {installation_uid} is contact-scoped"),
        ));
    }
    Ok(installation)
}

async fn activate_in_tx(
    conn: &mut PgConnection,
    request: &ActivationRequest,
    now: DateTime<Utc>,
) -> Result<ActivationOutcome> {
    // 1. Candidate identity, tenant scope, and target class.
    let candidate =
        load_candidate_in_tx_required(conn, &request.scope, request.candidate_revision_uid, true)
            .await?;
    let facts = load_revision_facts(conn, request.candidate_revision_uid, true).await?;
    ensure_revision_matches_target(&facts, &request.scope, &request.activation_target)?;
    if candidate.activation_target != request.activation_target {
        return Err(reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "candidate {} was submitted for {:?}, not {:?}",
                candidate.revision_uid, candidate.activation_target, request.activation_target
            ),
        ));
    }

    // 2. Exact candidate state.
    if !candidate.state.is_activatable() {
        return Err(reject(
            ReleaseRejection::CandidateNotActivatable,
            format!(
                "candidate {} is {} and is not activatable",
                candidate.revision_uid, candidate.state
            ),
        ));
    }

    // 3. Exact candidate bytes.
    if facts.canonical_hash != request.candidate_revision_hash
        || candidate.candidate_revision_hash != request.candidate_revision_hash
    {
        return Err(reject(
            ReleaseRejection::CandidateHashMismatch,
            format!(
                "candidate {} hashes to {} but the request names {}",
                candidate.revision_uid, facts.canonical_hash, request.candidate_revision_hash
            ),
        ));
    }

    validate_activation_payload(request, &candidate)?;

    // 4. Expected serving pointer, locked for the rest of the transaction.
    let pointer_state = read_pointer_state(conn, request, facts.artifact_uid).await?;
    if pointer_state.observed != request.expected_serving {
        return Err(reject(
            ReleaseRejection::ServingPointerConflict,
            format!(
                "serving pointer is {:?} but the request expected {:?}",
                pointer_state.observed, request.expected_serving
            ),
        ));
    }

    // 5. An unconsumed, unexpired attestation for exactly this subject.
    let attestation =
        load_attestation_in_tx(conn, request.attestation_uid, true, Some(&request.scope))
            .await?
            .ok_or_else(|| {
                reject(
                    ReleaseRejection::AttestationNotFound,
                    format!("attestation {} does not exist", request.attestation_uid),
                )
            })?;
    if attestation.candidate_revision_uid != candidate.revision_uid
        || attestation.activation_target != request.activation_target
    {
        return Err(reject(
            ReleaseRejection::AttestationSubjectMismatch,
            format!(
                "attestation {} attests revision {} for {:?}",
                attestation.attestation_uid,
                attestation.candidate_revision_uid,
                attestation.activation_target
            ),
        ));
    }
    if attestation.decision.verdict != DeterministicVerdict::Pass {
        return Err(reject(
            ReleaseRejection::VerdictNotPass,
            format!(
                "attestation {} carries verdict {}",
                attestation.attestation_uid,
                attestation.decision.verdict.as_str()
            ),
        ));
    }
    if attestation.consumed_at.is_some() {
        return Err(reject(
            ReleaseRejection::AttestationAlreadyConsumed,
            format!(
                "attestation {} was consumed by audit {:?}",
                attestation.attestation_uid, attestation.consumed_by_audit_uid
            ),
        ));
    }
    if attestation.expires_at <= now {
        return Err(reject(
            ReleaseRejection::AttestationExpired,
            format!(
                "attestation {} expired at {}",
                attestation.attestation_uid, attestation.expires_at
            ),
        ));
    }

    // 6. Subject recomputation from live state. Any drift in the serving
    //    baseline, the gate policy, or the candidate bytes since evaluation
    //    changes the digest, and a mismatched digest fails closed.
    let policy =
        resolve_policy_in_tx(conn, &request.scope, request.activation_target.class()).await?;
    let mut recomputed = candidate.subject.clone();
    recomputed.candidate_revision_hash = facts.canonical_hash;
    recomputed.serving_baseline = pointer_state.baseline;
    recomputed.release_policy = policy.identity();
    recomputed.resource_policy_hash = policy.resource_policy_hash;
    recomputed.validate(now)?;
    let recomputed_digest = recomputed.digest()?;
    if recomputed_digest != attestation.subject_digest
        || recomputed_digest != candidate.subject_digest
    {
        return Err(reject(
            ReleaseRejection::SubjectDigestMismatch,
            format!(
                "subject recomputes to {recomputed_digest}, attestation names {}, candidate names {}",
                attestation.subject_digest, candidate.subject_digest
            ),
        ));
    }

    // 7. Record the decision, consume the attestation, and move the pointer.
    let audit_uid = Uuid::now_v7();
    let next_pointer_version = pointer_state.observed.pointer_version.saturating_add(1);
    let applied: i64 = sqlx::query_scalar(
        r#"
        SELECT moa.apply_artifact_activation_transition(
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17
        )
        "#,
    )
    .bind(audit_uid)
    .bind(request.scope.storage_partition_id().to_string())
    .bind(facts.artifact_uid)
    .bind(facts.kind.as_str())
    .bind(request.activation_target.class().as_str())
    .bind(request.activation_target.installation_uid())
    .bind(request.attestation_uid)
    .bind(recomputed_digest.to_vec())
    .bind(pointer_state.observed.revision_uid)
    .bind(pointer_state.observed.pointer_version)
    .bind(candidate.revision_uid)
    .bind(facts.version)
    .bind(facts.canonical_hash.to_vec())
    .bind(next_pointer_version)
    .bind(&request.actor)
    .bind(request.reason.as_deref())
    .bind(now)
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    if applied != 1 {
        return Err(reject(
            ReleaseRejection::ServingPointerConflict,
            format!(
                "serving pointer for artifact {} moved concurrently",
                facts.artifact_uid
            ),
        ));
    }

    let consumed = sqlx::query(
        r#"
        UPDATE moa.artifact_activation_attestation
        SET consumed_at = $2,
            consumed_by_audit_uid = $3
        WHERE attestation_uid = $1
          AND consumed_at IS NULL
        "#,
    )
    .bind(request.attestation_uid)
    .bind(now)
    .bind(audit_uid)
    .execute(&mut *conn)
    .await
    .map_err(storage)?
    .rows_affected();
    if consumed != 1 {
        return Err(reject(
            ReleaseRejection::AttestationAlreadyConsumed,
            format!(
                "attestation {} was consumed concurrently",
                request.attestation_uid
            ),
        ));
    }

    let deployment_uid = move_pointer(
        conn,
        request,
        &facts,
        &pointer_state,
        next_pointer_version,
        now,
    )
    .await?;

    // The revision that was serving is superseded, as is every other
    // non-terminal candidate for this artifact: they were evaluated against a
    // baseline that no longer exists.
    let mut superseded = Vec::new();
    if let Some(previous_revision_uid) = pointer_state.observed.revision_uid
        && previous_revision_uid != candidate.revision_uid
        && let Some(previous) =
            load_candidate_in_tx(conn, &request.scope, previous_revision_uid, true).await?
        && previous.state.can_transition_to(ReleaseState::Superseded)
    {
        set_revision_state(
            conn,
            previous.revision_uid,
            previous.state,
            ReleaseState::Superseded,
        )
        .await?;
        set_candidate_slot(conn, previous.revision_uid, ReleaseSlot::Released).await?;
        superseded.push(previous.revision_uid);
    }

    Ok(ActivationOutcome {
        audit_uid,
        activated_revision_uid: candidate.revision_uid,
        previous_revision_uid: pointer_state.observed.revision_uid,
        pointer_version: next_pointer_version,
        superseded_revision_uids: superseded,
        deployment_uid,
    })
}

fn validate_activation_payload(
    request: &ActivationRequest,
    candidate: &ReleaseCandidate,
) -> Result<()> {
    match request.activation_target {
        ActivationTarget::AgentDeployment { .. } => {
            let revision_lock = request.agent_revision_lock.as_ref().ok_or_else(|| {
                reject(
                    ReleaseRejection::SubjectDigestMismatch,
                    "agent activation has no evaluated deployment lock",
                )
            })?;
            if revision_lock.agent_revision_uid != request.candidate_revision_uid {
                return Err(reject(
                    ReleaseRejection::SubjectDigestMismatch,
                    format!(
                        "agent deployment lock names revision {}, expected {}",
                        revision_lock.agent_revision_uid, request.candidate_revision_uid
                    ),
                ));
            }
            let lock_hash = Digest32(canonical_hash(revision_lock)?);
            if lock_hash != candidate.subject.dependency_lock_hash {
                return Err(reject(
                    ReleaseRejection::SubjectDigestMismatch,
                    format!(
                        "agent deployment lock hashes to {lock_hash}, evaluated subject names {}",
                        candidate.subject.dependency_lock_hash
                    ),
                ));
            }
        }
        ActivationTarget::SkillVisibility { .. } | ActivationTarget::ActionVisibility { .. } => {
            if request.agent_revision_lock.is_some() {
                return Err(reject(
                    ReleaseRejection::TargetKindMismatch,
                    "only an agent deployment accepts an agent revision lock",
                ));
            }
        }
    }
    Ok(())
}

/// Live serving state observed under the activation lock.
struct PointerState {
    observed: ExpectedServing,
    baseline: Option<ServingBaseline>,
    installation: Option<InstallationFacts>,
}

async fn read_pointer_state(
    conn: &mut PgConnection,
    request: &ActivationRequest,
    artifact_uid: Uuid,
) -> Result<PointerState> {
    match request.activation_target {
        ActivationTarget::SkillVisibility { .. } | ActivationTarget::ActionVisibility { .. } => {
            lock_artifact_serving_pointer(conn, &request.scope, artifact_uid).await?;
            let pointer = load_serving_pointer_in_tx(conn, &request.scope, artifact_uid, false)
                .await
                .map_err(storage)?;
            let observed = ExpectedServing {
                revision_uid: pointer.as_ref().map(|pointer| pointer.revision_uid),
                pointer_version: pointer
                    .as_ref()
                    .map_or(0, |pointer| pointer.pointer_version),
            };
            let baseline = pointer.as_ref().map(|pointer| ServingBaseline {
                revision_uid: pointer.revision_uid,
                revision_hash: pointer.revision_hash,
                pointer_version: pointer.pointer_version,
            });
            Ok(PointerState {
                observed,
                baseline,
                installation: None,
            })
        }
        ActivationTarget::AgentDeployment {
            installation_uid, ..
        } => {
            let installation =
                ensure_installation_locked(conn, &request.scope, artifact_uid, installation_uid)
                    .await?;
            let observed = ExpectedServing {
                revision_uid: installation.current_revision_uid,
                pointer_version: installation.serving_pointer_version,
            };
            let baseline = match installation.current_revision_uid {
                None => None,
                Some(revision_uid) => {
                    let facts = load_revision_facts(conn, revision_uid, false).await?;
                    Some(ServingBaseline {
                        revision_uid,
                        revision_hash: facts.canonical_hash,
                        pointer_version: installation.serving_pointer_version,
                    })
                }
            };
            Ok(PointerState {
                observed,
                baseline,
                installation: Some(installation),
            })
        }
    }
}

/// Serializes one skill/action serving-pointer transition, including its absent
/// first-pointer state, through the database-owned lock seam.
pub(crate) async fn lock_artifact_serving_pointer(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
) -> Result<()> {
    sqlx::query("SELECT moa.lock_artifact_serving_pointer($1, $2)")
        .bind(scope.storage_partition_id().to_string())
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
    Ok(())
}

async fn ensure_installation_locked(
    conn: &mut PgConnection,
    scope: &TenantScope,
    artifact_uid: Uuid,
    installation_uid: Uuid,
) -> Result<InstallationFacts> {
    let installation = load_installation_in_tx(conn, scope, installation_uid, true)
        .await?
        .ok_or_else(|| {
            reject(
                ReleaseRejection::InstallationNotFound,
                format!(
                    "agent installation {installation_uid} is missing, retired, or in another tenant"
                ),
            )
        })?;
    if installation.artifact_uid != artifact_uid {
        return Err(reject(
            ReleaseRejection::TargetKindMismatch,
            format!(
                "installation {installation_uid} was installed from artifact {}",
                installation.artifact_uid
            ),
        ));
    }
    Ok(installation)
}

async fn move_pointer(
    conn: &mut PgConnection,
    request: &ActivationRequest,
    facts: &RevisionFacts,
    pointer_state: &PointerState,
    next_pointer_version: i64,
    now: DateTime<Utc>,
) -> Result<Option<Uuid>> {
    match request.activation_target {
        ActivationTarget::SkillVisibility { .. } | ActivationTarget::ActionVisibility { .. } => {
            // Identity-embedding staleness is driven by `artifact.updated_at`, so
            // an activation has to bump it or ranking would keep advertising the
            // previous revision's identity text as fresh.
            sqlx::query("UPDATE moa.artifact SET updated_at = $2 WHERE artifact_uid = $1")
                .bind(facts.artifact_uid)
                .bind(now)
                .execute(&mut *conn)
                .await
                .map_err(storage)?;
            Ok(None)
        }
        ActivationTarget::AgentDeployment {
            installation_uid, ..
        } => {
            let installation = pointer_state.installation.as_ref().ok_or_else(|| {
                Error::Storage("agent activation read no installation state".to_string())
            })?;
            let revision_lock = request.agent_revision_lock.as_ref().ok_or_else(|| {
                Error::Storage("validated agent activation lost its deployment lock".to_string())
            })?;
            let deployment_uid = Uuid::now_v7();
            sqlx::query(
                r#"
                UPDATE moa.agent_deployment
                SET status = 'superseded'
                WHERE installation_uid = $1
                  AND status = 'active'
                "#,
            )
            .bind(installation_uid)
            .execute(&mut *conn)
            .await
            .map_err(storage)?;
            sqlx::query(
                r#"
                INSERT INTO moa.agent_deployment (
                    deployment_uid, installation_uid, storage_partition_id, user_id, revision_uid,
                    deployed_by, status, reason, dependency_lock, dependency_lock_hash
                )
                VALUES ($1, $2, $3, $4, $5, $6, 'active', $7, $8, $9)
                "#,
            )
            .bind(deployment_uid)
            .bind(installation_uid)
            .bind(request.scope.storage_partition_id().to_string())
            .bind(installation.user_id.as_deref())
            .bind(request.candidate_revision_uid)
            .bind(&request.actor)
            .bind(request.reason.as_deref())
            .bind(SqlJson(revision_lock))
            .bind(&revision_lock.canonical_policy_hash)
            .execute(&mut *conn)
            .await
            .map_err(storage)?;
            let moved = sqlx::query(
                r#"
                UPDATE moa.agent_installation
                SET status = 'active',
                    current_revision_uid = $2,
                    serving_pointer_version = $3,
                    activation_attestation_uid = $4,
                    last_deployment_uid = $5,
                    last_deployed_at = $6,
                    updated_at = $6
                WHERE installation_uid = $1
                  AND serving_pointer_version = $7
                  AND current_revision_uid IS NOT DISTINCT FROM $8
                  AND status <> 'retired'
                "#,
            )
            .bind(installation_uid)
            .bind(request.candidate_revision_uid)
            .bind(next_pointer_version)
            .bind(request.attestation_uid)
            .bind(deployment_uid)
            .bind(now)
            .bind(installation.serving_pointer_version)
            .bind(installation.current_revision_uid)
            .execute(&mut *conn)
            .await
            .map_err(storage)?
            .rows_affected();
            if moved != 1 {
                return Err(reject(
                    ReleaseRejection::ServingPointerConflict,
                    format!("installation {installation_uid} moved concurrently"),
                ));
            }
            Ok(Some(deployment_uid))
        }
    }
}

/// Drives the database-owned activation CAS from a fixture with a stated version.
///
/// Lives here so `RevisionFacts` stays private: the fixture supplies only the
/// identifiers a caller could legitimately hold, and this assembles the rest from the
/// candidate row exactly as `activate` would.
#[cfg(feature = "test-support")]
pub(crate) async fn drive_compare_and_swap_for_tests(
    conn: &mut PgConnection,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
    attestation_uid: Uuid,
    expected_version: i64,
    next_pointer_version: i64,
) -> Result<u64> {
    let facts = load_revision_facts(conn, candidate_revision_uid, false).await?;
    lock_artifact_serving_pointer(conn, &scope, facts.artifact_uid).await?;
    let candidate =
        load_candidate_in_tx_required(conn, &scope, candidate_revision_uid, false).await?;
    if candidate.activation_target != activation_target {
        return Err(reject(
            ReleaseRejection::AttestationSubjectMismatch,
            "pointer-fence fixture target does not match its candidate",
        ));
    }
    let pointer = load_serving_pointer_in_tx(conn, &scope, facts.artifact_uid, false)
        .await
        .map_err(storage)?;
    let previous_revision_uid = pointer.as_ref().map(|pointer| pointer.revision_uid);
    let affected: i64 = sqlx::query_scalar(
        r#"
        SELECT moa.apply_artifact_activation_transition(
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(scope.storage_partition_id().to_string())
    .bind(facts.artifact_uid)
    .bind(facts.kind.as_str())
    .bind(activation_target.class().as_str())
    .bind(activation_target.installation_uid())
    .bind(attestation_uid)
    .bind(candidate.subject_digest.to_vec())
    .bind(previous_revision_uid)
    .bind(expected_version)
    .bind(candidate_revision_uid)
    .bind(facts.version)
    .bind(facts.canonical_hash.to_vec())
    .bind(next_pointer_version)
    .bind("pointer-fence-fixture")
    .bind(Option::<&str>::None)
    .bind(Utc::now())
    .fetch_one(&mut *conn)
    .await
    .map_err(storage)?;
    u64::try_from(affected).map_err(|_| Error::Storage("negative pointer row count".to_string()))
}

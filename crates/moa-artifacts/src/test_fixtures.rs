//! Fixture helpers for tests that need a serving artifact revision.
//!
//! Gated behind the `test-support` feature and enabled only through
//! dev-dependencies. Nothing here is a bypass: [`activate_revision`] drives the
//! real submit, decide, and activate path, so a fixture cannot make a revision
//! serve unless every release predicate accepts it. What it does supply is the
//! part a test has no other way to produce -- fixture evidence identifiers and
//! fixture subject inputs -- which is exactly what the platform release
//! evaluator supplies in production.

use std::collections::BTreeMap;

use moa_core::types::agent::AgentRevisionLock;
use moa_db::ScopedConn;
use sqlx::PgPool;
use uuid::Uuid;

use crate::document::ArtifactStatus;
use crate::registry::{
    ArtifactRegistry, CandidateSubjectInputs, RecordDecision, ReleaseRepository, SubmitCandidate,
};
use crate::release::{
    ActivationRequest, ActivationTarget, AgentRuntimeSubject, DeterministicVerdict, Digest32,
    EvaluationPlanSubject, EvidenceAdapter, TenantScope,
};
use crate::resolver::ArtifactResolver;
use crate::validation::validate_for_status;
use crate::{Error, Result};

/// What a fixture activation produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivatedRevision {
    /// Attestation the activation consumed.
    pub attestation_uid: Uuid,
    /// Audit row the activation wrote.
    pub audit_uid: Uuid,
    /// Serving pointer version after the move.
    pub pointer_version: i64,
}

/// Returns deterministic fixture subject inputs for a candidate.
#[must_use]
pub fn fixture_subject_inputs() -> CandidateSubjectInputs {
    CandidateSubjectInputs {
        dependency_lock_hash: Digest32([1_u8; 32]),
        agent_runtime: AgentRuntimeSubject {
            prompt_hash: Digest32([2_u8; 32]),
            model: "fixture-model".to_string(),
            provider: "fixture-provider".to_string(),
            runtime_policy_hash: Digest32([3_u8; 32]),
        },
        tool_policy_hash: Digest32([4_u8; 32]),
        tool_bearing: false,
        tool_catalog: None,
        plan: EvaluationPlanSubject {
            plan_hash: Digest32([5_u8; 32]),
            scenario_dataset_hash: Digest32([6_u8; 32]),
            seed_hash: Digest32([7_u8; 32]),
            evaluator_versions: BTreeMap::from([(
                "fixture.assertion".to_string(),
                "1.0.0".to_string(),
            )]),
        },
        simulator: None,
    }
}

/// Submits, passes, and activates a candidate revision through the real path.
pub async fn activate_revision(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
) -> Result<ActivatedRevision> {
    activate_revision_with_lock(pool, scope, activation_target, candidate_revision_uid, None).await
}

/// Submits, passes, and activates an agent candidate with its production-shaped lock.
pub async fn activate_agent_revision(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
    revision_lock: AgentRevisionLock,
) -> Result<ActivatedRevision> {
    activate_revision_with_lock(
        pool,
        scope,
        activation_target,
        candidate_revision_uid,
        Some(revision_lock),
    )
    .await
}

/// What a fixture release decision attested without activating.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttestedRevision {
    /// Single-use activation attestation.
    pub attestation_uid: Uuid,
    /// Candidate hash the activation request must carry.
    pub candidate_revision_hash: Digest32,
}

/// Submits and attests a candidate while leaving its serving pointer unchanged.
pub async fn attest_revision(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
) -> Result<AttestedRevision> {
    attest_revision_with_lock(pool, scope, activation_target, candidate_revision_uid, None).await
}

async fn activate_revision_with_lock(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
    revision_lock: Option<AgentRevisionLock>,
) -> Result<ActivatedRevision> {
    let attested = attest_revision_with_lock(
        pool,
        scope,
        activation_target,
        candidate_revision_uid,
        revision_lock.clone(),
    )
    .await?;
    let repository = ReleaseRepository::new(pool.clone());
    let expected_serving = repository
        .expected_serving(&scope, &activation_target)
        .await?;
    let outcome = repository
        .activate(ActivationRequest {
            scope,
            activation_target,
            candidate_revision_uid,
            candidate_revision_hash: attested.candidate_revision_hash,
            attestation_uid: attested.attestation_uid,
            expected_serving,
            agent_revision_lock: revision_lock,
            actor: "fixture".to_string(),
            reason: Some("fixture activation".to_string()),
        })
        .await?;
    Ok(ActivatedRevision {
        attestation_uid: attested.attestation_uid,
        audit_uid: outcome.audit_uid,
        pointer_version: outcome.pointer_version,
    })
}

async fn attest_revision_with_lock(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
    revision_lock: Option<AgentRevisionLock>,
) -> Result<AttestedRevision> {
    // Generic validation is what makes an immutable revision eligible for
    // evaluation, so the fixture performs that step exactly as the artifact service
    // does: resolve the declared references, validate at the activatable tier, and
    // record the report. Skipping it would let a fixture bypass the eligibility
    // predicate that `unvalidated_candidate_cannot_enter_evaluation` pins.
    let registry = ArtifactRegistry::new(pool.clone());
    let action_scope = scope.action_rule_scope();
    let mut document = registry
        .load_revision(&action_scope, candidate_revision_uid)
        .await
        .map_err(|error| Error::Storage(error.to_string()))?
        .ok_or_else(|| Error::Storage("fixture candidate revision not found".to_string()))?
        .document;
    document.reference_resolutions = ArtifactResolver::new(registry.clone())
        .resolve_document(&action_scope, &document)
        .await
        .map_err(|error| Error::Storage(error.to_string()))?;
    let report = validate_for_status(&document, ArtifactStatus::Ready);
    registry
        .record_validation_report(&action_scope, candidate_revision_uid, &report)
        .await
        .map_err(|error| Error::Storage(error.to_string()))?;

    let mut subject_inputs = fixture_subject_inputs();
    if let Some(lock) = revision_lock.as_ref() {
        subject_inputs.dependency_lock_hash = Digest32(crate::canonical::canonical_hash(lock)?);
    }
    let repository = ReleaseRepository::new(pool.clone());
    let submission = repository
        .submit_candidate(SubmitCandidate {
            scope,
            activation_target,
            candidate_revision_uid,
            subject_inputs,
            submitted_by: "fixture".to_string(),
        })
        .await?;
    let candidate = submission.candidate;
    let policy = repository
        .resolve_policy(&scope, candidate.activation_target.class())
        .await?;
    let decision = repository
        .record_decision(RecordDecision {
            scope,
            candidate_revision_uid,
            subject_digest: candidate.subject_digest,
            verdict: DeterministicVerdict::Pass,
            run_uid: Uuid::now_v7(),
            trial_uids: vec![Uuid::now_v7()],
            evidence_ids: vec![Uuid::now_v7()],
            gate_results: BTreeMap::from([("result_produced".to_string(), "pass".to_string())]),
            blocking_assertions: policy.blocking_assertions,
            evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
            decided_by: "fixture".to_string(),
        })
        .await?;
    let attestation = decision.attestation.ok_or_else(|| {
        Error::Storage("fixture decision minted no activation attestation".to_string())
    })?;
    Ok(AttestedRevision {
        attestation_uid: attestation.attestation_uid,
        candidate_revision_hash: candidate.candidate_revision_hash,
    })
}

/// Outcome of driving one serving-pointer fence directly.
///
/// The fences report row counts rather than errors: the activation transaction is
/// what turns "zero rows" into `ServingPointerConflict`, so a test that drives the
/// fence sees the count it actually produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerFenceOutcome {
    /// Rows the fence statement affected. Exactly `1` means the fence admitted the move.
    pub rows_affected: u64,
}

impl PointerFenceOutcome {
    /// Whether the fence admitted the move.
    #[must_use]
    pub const fn admitted(self) -> bool {
        self.rows_affected == 1
    }
}

/// Drives the serving-pointer compare-and-swap against a stated expected version.
///
/// This exists because the compare-and-swap is unreachable through
/// [`activate_revision`]: the activation transaction reads the pointer `FOR UPDATE`,
/// compares the observed version to the caller's expectation, and recomputes a
/// subject digest containing `pointer_version`, so a concurrent mover is refused
/// twice before the statement runs. A test confined to the activation path therefore
/// cannot tell a working compare-and-swap from a deleted one. Driving it here makes
/// the fence an observable unit, so removing its predicate fails a named test.
///
/// `expected_version` is deliberately a caller parameter rather than read from the
/// row — passing a stale value is the whole point.
pub async fn compare_and_swap_serving_pointer(
    pool: &PgPool,
    scope: TenantScope,
    activation_target: ActivationTarget,
    candidate_revision_uid: Uuid,
    attestation_uid: Uuid,
    expected_version: i64,
    next_pointer_version: i64,
) -> Result<PointerFenceOutcome> {
    let mut conn = ScopedConn::begin_tenant(pool, scope.tenant_id())
        .await
        .map_err(|error| Error::Storage(format!("begin pointer-fence scope: {error}")))?;
    let rows_affected = crate::registry::release::drive_compare_and_swap_for_tests(
        conn.as_mut(),
        scope,
        activation_target,
        candidate_revision_uid,
        attestation_uid,
        expected_version,
        next_pointer_version,
    )
    .await?;
    conn.commit()
        .await
        .map_err(|error| Error::Storage(format!("commit pointer fence: {error}")))?;
    Ok(PointerFenceOutcome { rows_affected })
}

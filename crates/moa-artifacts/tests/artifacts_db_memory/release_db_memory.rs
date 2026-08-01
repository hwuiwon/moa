//! Release-control coverage: serving-pointer parity, the candidate state
//! machine, and every activation predicate.

use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, RecordDecision, ReleaseRepository,
    StoredArtifactRevision, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationRequest, ActivationTarget, ActivationTargetClass, DeterministicVerdict, Digest32,
    EvidenceAdapter, ExpectedServing, ReleaseSlot, ReleaseState, SimulatorPolicyBinding,
    TenantScope,
};
use moa_artifacts::test_fixtures::{activate_revision, fixture_subject_inputs};
use moa_artifacts::{Error, ReleaseRejection};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::agent::AgentRevisionLock, types::identifiers::TenantId,
};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;

fn skill_doc(name: &str, description: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": { "name": name, "description": description, "tags": ["release"] },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "inputs": { "type": "object" },
                "outputs": { "type": "object" }
            }
        }
    }))
    .expect("skill fixture is valid")
}

async fn draft_skill(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    name: &str,
    description: &str,
) -> Result<StoredArtifactRevision> {
    let document = skill_doc(name, description);
    let source = document.to_yaml().expect("serialize skill");
    registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", b"# Skill\n".to_vec())],
            },
        )
        .await
}

fn storage_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn release_error(error: Error) -> MoaError {
    MoaError::ValidationError(error.to_string())
}

/// Runs the generic validation step that makes a revision eligible for evaluation.
async fn record_eligibility(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<()> {
    let mut document = registry
        .load_revision(scope, revision_uid)
        .await?
        .expect("revision exists")
        .document;
    document.reference_resolutions =
        moa_artifacts::resolver::ArtifactResolver::new(registry.clone())
            .resolve_document(scope, &document)
            .await?;
    let report = moa_artifacts::validation::validate_for_status(&document, ArtifactStatus::Ready);
    assert!(report.is_ok(), "fixture must validate: {report:?}");
    registry
        .record_validation_report(scope, revision_uid, &report)
        .await?;
    Ok(())
}

/// Moves an attestation's expiry, which the immutability trigger otherwise refuses.
async fn age_attestation(pool: &PgPool, attestation_uid: Uuid, interval: &str) -> Result<()> {
    sqlx::query(
        "ALTER TABLE moa.artifact_activation_attestation \
         DISABLE TRIGGER artifact_activation_attestation_immutable",
    )
    .execute(pool)
    .await
    .map_err(storage_error)?;
    // `created_at` moves with it: the row also carries an `expires_at > created_at`
    // check, which is what keeps a zero-lifetime attestation unrepresentable.
    sqlx::query(&format!(
        "UPDATE moa.artifact_activation_attestation \
         SET created_at = now() - INTERVAL '2 hours', \
             expires_at = now() + INTERVAL '{interval}' \
         WHERE attestation_uid = $1"
    ))
    .bind(attestation_uid)
    .execute(pool)
    .await
    .map_err(storage_error)?;
    sqlx::query(
        "ALTER TABLE moa.artifact_activation_attestation \
         ENABLE TRIGGER artifact_activation_attestation_immutable",
    )
    .execute(pool)
    .await
    .map_err(storage_error)?;
    Ok(())
}
#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn import_and_validation_cannot_make_a_skill_serve_db_memory() -> Result<()> {
    // Pins: neither an import nor generic validation moves a serving pointer, and
    // the release-gated publish helper is refused outright.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let name = format!("import-{}", Uuid::now_v7());
    let draft = draft_skill(&registry, &scope, &name, "imported").await?;

    let report =
        moa_artifacts::validation::validate_for_status(&draft.document, ArtifactStatus::Ready);
    let recorded = registry
        .record_validation_report(&scope, draft.revision_uid, &report)
        .await?;
    assert_eq!(
        recorded.status,
        ArtifactStatus::Draft,
        "validation records a report; it does not change state"
    );
    assert!(
        registry
            .load_serving(&scope, ArtifactKind::Skill, &name)
            .await?
            .is_none(),
        "a validated draft must not serve"
    );

    let refused = registry
        .publish_unserved_revision(&scope, draft.revision_uid, &report)
        .await
        .expect_err("a release-gated revision cannot be published");
    assert!(
        matches!(refused, MoaError::ValidationError(ref message) if message.contains("release-gated")),
        "unexpected refusal: {refused}"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn activation_predicates_fail_closed_db_memory() -> Result<()> {
    // Pins: every activation predicate, one mutation at a time. Each block below
    // changes exactly one input away from a request that would succeed, so a
    // predicate that stopped being checked would let that block through.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("gated-{}", Uuid::now_v7());
    let draft = draft_skill(&registry, &scope, &name, "candidate").await?;
    let target = ActivationTarget::SkillVisibility {
        artifact_uid: draft.artifact_uid,
    };

    // A draft is not activatable, and no attestation exists to try with.
    let submission = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: draft.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?;
    let candidate = submission.candidate;
    assert!(submission.dispatched);
    assert_eq!(candidate.state, ReleaseState::Evaluating);
    assert_eq!(candidate.slot, ReleaseSlot::Active);

    // An evaluating candidate is not activatable either, even with a well-formed
    // request, because no attestation can exist yet.
    let missing_attestation = repository
        .activate(ActivationRequest {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: draft.revision_uid,
            candidate_revision_hash: candidate.candidate_revision_hash,
            attestation_uid: Uuid::now_v7(),
            expected_serving: ExpectedServing {
                revision_uid: None,
                pointer_version: 0,
            },
            agent_revision_lock: None,
            actor: "operator".to_string(),
            reason: None,
        })
        .await
        .expect_err("an evaluating candidate cannot be activated");
    assert_rejection(
        &missing_attestation,
        ReleaseRejection::CandidateNotActivatable,
    );

    let decision = repository
        .record_decision(pass_decision(release_scope, &candidate))
        .await
        .map_err(release_error)?;
    assert_eq!(decision.state, ReleaseState::Ready);
    let attestation = decision.attestation.expect("a passing verdict mints one");

    let good = || ActivationRequest {
        scope: release_scope,
        activation_target: target,
        candidate_revision_uid: draft.revision_uid,
        candidate_revision_hash: candidate.candidate_revision_hash,
        attestation_uid: attestation.attestation_uid,
        expected_serving: ExpectedServing {
            revision_uid: None,
            pointer_version: 0,
        },
        agent_revision_lock: None,
        actor: "operator".to_string(),
        reason: None,
    };

    // Wrong tenant.
    let other_tenant = TenantScope::new(TenantId::from(Uuid::now_v7()));
    let wrong_tenant = repository
        .activate(ActivationRequest {
            scope: other_tenant,
            ..good()
        })
        .await
        .expect_err("another tenant cannot activate this candidate");
    assert_rejection(&wrong_tenant, ReleaseRejection::CandidateNotFound);

    // Wrong candidate bytes.
    let wrong_hash = repository
        .activate(ActivationRequest {
            candidate_revision_hash: Digest32([0_u8; 32]),
            ..good()
        })
        .await
        .expect_err("a hash mismatch cannot activate");
    assert_rejection(&wrong_hash, ReleaseRejection::CandidateHashMismatch);

    // Wrong expected serving pointer.
    let wrong_pointer = repository
        .activate(ActivationRequest {
            expected_serving: ExpectedServing {
                revision_uid: None,
                pointer_version: 7,
            },
            ..good()
        })
        .await
        .expect_err("a stale pointer expectation cannot activate");
    assert_rejection(&wrong_pointer, ReleaseRejection::ServingPointerConflict);

    // Unknown attestation.
    let unknown = repository
        .activate(ActivationRequest {
            attestation_uid: Uuid::now_v7(),
            ..good()
        })
        .await
        .expect_err("an unknown attestation cannot activate");
    assert_rejection(&unknown, ReleaseRejection::AttestationNotFound);

    // An attestation minted for a different candidate.
    let other_name = format!("other-{}", Uuid::now_v7());
    let other_draft = draft_skill(&registry, &scope, &other_name, "other candidate").await?;
    let other_target = ActivationTarget::SkillVisibility {
        artifact_uid: other_draft.artifact_uid,
    };
    let other_candidate = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: other_target,
            candidate_revision_uid: other_draft.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?
        .candidate;
    let other_attestation = repository
        .record_decision(pass_decision(release_scope, &other_candidate))
        .await
        .map_err(release_error)?
        .attestation
        .expect("second attestation");
    let wrong_subject = repository
        .activate(ActivationRequest {
            attestation_uid: other_attestation.attestation_uid,
            ..good()
        })
        .await
        .expect_err("another candidate's attestation cannot activate this one");
    assert_rejection(&wrong_subject, ReleaseRejection::AttestationSubjectMismatch);

    // An attestation is immutable, so even ageing its expiry is refused by the
    // database. That refusal is the first assertion; the trigger is then disabled
    // for this per-test database only, to reconstruct the expired condition
    // production reaches when the policy TTL elapses.
    let rewrite = sqlx::query(
        "UPDATE moa.artifact_activation_attestation SET expires_at = now() - INTERVAL '1 second' \
         WHERE attestation_uid = $1",
    )
    .bind(attestation.attestation_uid)
    .execute(&pool)
    .await;
    assert!(
        rewrite.is_err(),
        "an attestation must be immutable except for consumption"
    );
    age_attestation(&pool, attestation.attestation_uid, "-1 second").await?;
    let expired = repository
        .activate(good())
        .await
        .expect_err("an expired attestation cannot activate");
    assert_rejection(&expired, ReleaseRejection::AttestationExpired);
    age_attestation(&pool, attestation.attestation_uid, "1 hour").await?;

    // The valid request succeeds exactly once.
    let outcome = repository.activate(good()).await.map_err(release_error)?;
    assert_eq!(outcome.activated_revision_uid, draft.revision_uid);
    assert_eq!(outcome.pointer_version, 1);
    assert_eq!(outcome.previous_revision_uid, None);

    // Single use: the same attestation cannot be spent again. The replay uses the
    // pointer state the first activation left behind, so the compare-and-set
    // passes and consumption is the predicate that must refuse.
    let replayed = repository
        .activate(ActivationRequest {
            expected_serving: repository
                .expected_serving(&release_scope, &target)
                .await
                .map_err(release_error)?,
            ..good()
        })
        .await
        .expect_err("an attestation is single-use");
    assert_rejection(&replayed, ReleaseRejection::AttestationAlreadyConsumed);

    // The audit row and the pointer agree, and the audit row is append-only.
    let (audited_revision, audited_version, audited_attestation) =
        sqlx::query_as::<_, (Uuid, i64, Option<Uuid>)>(
            "SELECT activated_revision_uid, activated_pointer_version, attestation_uid \
         FROM moa.artifact_activation_audit WHERE audit_uid = $1",
        )
        .bind(outcome.audit_uid)
        .fetch_one(&pool)
        .await
        .map_err(storage_error)?;
    assert_eq!(audited_revision, draft.revision_uid);
    assert_eq!(audited_version, 1);
    assert_eq!(audited_attestation, Some(attestation.attestation_uid));
    let audit_update = sqlx::query(
        "UPDATE moa.artifact_activation_audit SET reason = 'edited' WHERE audit_uid = $1",
    )
    .bind(outcome.audit_uid)
    .execute(&pool)
    .await;
    assert!(audit_update.is_err(), "the activation audit is append-only");

    // A rejected candidate is not activatable.
    let rejected_name = format!("rejected-{}", Uuid::now_v7());
    let rejected_draft = draft_skill(&registry, &scope, &rejected_name, "regressed").await?;
    let rejected_target = ActivationTarget::SkillVisibility {
        artifact_uid: rejected_draft.artifact_uid,
    };
    let rejected_candidate = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: rejected_target,
            candidate_revision_uid: rejected_draft.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?
        .candidate;
    let rejection = repository
        .record_decision(RecordDecision {
            verdict: DeterministicVerdict::Regression,
            ..pass_decision(release_scope, &rejected_candidate)
        })
        .await
        .map_err(release_error)?;
    assert_eq!(rejection.state, ReleaseState::Rejected);
    assert!(
        rejection.attestation.is_none(),
        "a regression mints no permission to serve"
    );
    assert!(
        registry
            .load_serving(&scope, ArtifactKind::Skill, &rejected_name)
            .await?
            .is_none()
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn baseline_drift_invalidates_an_attestation_db_memory() -> Result<()> {
    // Pins: an attestation is bound to the serving baseline it was evaluated
    // against. When another revision activates first, the subject recomputation at
    // activation differs from the attested digest and the stale attestation fails
    // closed rather than overwriting the newer pointer.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("drift-{}", Uuid::now_v7());

    let v1 = draft_skill(&registry, &scope, &name, "v1").await?;
    let target = ActivationTarget::SkillVisibility {
        artifact_uid: v1.artifact_uid,
    };
    let v2 = draft_skill(&registry, &scope, &name, "v2").await?;

    // v2 is evaluated against an empty baseline and earns an attestation.
    let v2_candidate = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: v2.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?
        .candidate;
    let v2_attestation = repository
        .record_decision(pass_decision(release_scope, &v2_candidate))
        .await
        .map_err(release_error)?
        .attestation
        .expect("v2 attestation");

    // v1 activates first, so the baseline v2 was measured against no longer holds.
    activate_revision(&pool, release_scope, target, v1.revision_uid)
        .await
        .map_err(release_error)?;

    let stale = repository
        .activate(ActivationRequest {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: v2.revision_uid,
            candidate_revision_hash: v2_candidate.candidate_revision_hash,
            attestation_uid: v2_attestation.attestation_uid,
            expected_serving: repository
                .expected_serving(&release_scope, &target)
                .await
                .map_err(release_error)?,
            agent_revision_lock: None,
            actor: "operator".to_string(),
            reason: None,
        })
        .await
        .expect_err("an attestation measured against a different baseline is stale");
    assert_rejection(&stale, ReleaseRejection::SubjectDigestMismatch);

    let serving = registry
        .load_serving(&scope, ArtifactKind::Skill, &name)
        .await?
        .expect("v1 keeps serving");
    assert_eq!(serving.revision_uid, v1.revision_uid);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn inconclusive_frees_the_slot_and_dispatches_the_pending_candidate_db_memory() -> Result<()>
{
    // Pins: ten rapid submissions coalesce to one active attempt plus the newest
    // pending subject, an inconclusive result leaves the candidate non-serving and
    // retryable while releasing the slot, and the pending newest candidate is then
    // dispatched rather than starved.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("coalesce-{}", Uuid::now_v7());

    let first = draft_skill(&registry, &scope, &name, "rev-1").await?;
    let target = ActivationTarget::SkillVisibility {
        artifact_uid: first.artifact_uid,
    };
    let active = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: first.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?;
    assert!(active.dispatched);

    let mut pending_revisions = Vec::new();
    for index in 2..=10 {
        let draft = draft_skill(&registry, &scope, &name, &format!("rev-{index}")).await?;
        let submission = repository
            .submit_candidate(SubmitCandidate {
                scope: release_scope,
                activation_target: target,
                candidate_revision_uid: draft.revision_uid,
                subject_inputs: fixture_subject_inputs(),
                submitted_by: "operator".to_string(),
            })
            .await
            .map_err(release_error)?;
        assert!(
            !submission.dispatched,
            "only one attempt may hold the active slot"
        );
        assert_eq!(submission.candidate.slot, ReleaseSlot::Pending);
        pending_revisions.push(draft.revision_uid);
    }
    let newest = *pending_revisions.last().expect("nine pending submissions");

    let slots = sqlx::query_as::<_, (String, i64)>(
        "SELECT slot, count(*) FROM moa.artifact_release_candidate WHERE artifact_uid = $1 \
         GROUP BY slot ORDER BY slot",
    )
    .bind(first.artifact_uid)
    .fetch_all(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(
        slots,
        vec![
            ("active".to_string(), 1),
            ("pending".to_string(), 1),
            ("released".to_string(), 8),
        ],
        "coalescing keeps one active plus the newest pending subject"
    );

    let outcome = repository
        .record_decision(RecordDecision {
            verdict: DeterministicVerdict::Inconclusive,
            ..pass_decision(release_scope, &active.candidate)
        })
        .await
        .map_err(release_error)?;
    assert_eq!(outcome.state, ReleaseState::Inconclusive);
    assert!(outcome.attestation.is_none());
    assert_eq!(
        outcome.dispatched_revision_uid,
        Some(newest),
        "the freed slot runs the newest pending subject"
    );

    let inconclusive = repository
        .load_candidate(&release_scope, first.revision_uid)
        .await
        .map_err(release_error)?
        .expect("inconclusive candidate remains");
    assert_eq!(inconclusive.state, ReleaseState::Inconclusive);
    assert!(inconclusive.state.is_retryable());
    assert_eq!(inconclusive.slot, ReleaseSlot::Released);
    assert!(
        registry
            .load_serving(&scope, ArtifactKind::Skill, &name)
            .await?
            .is_none(),
        "an inconclusive release serves nothing"
    );

    let dispatched = repository
        .load_candidate(&release_scope, newest)
        .await
        .map_err(release_error)?
        .expect("newest candidate now runs");
    assert_eq!(dispatched.state, ReleaseState::Evaluating);
    assert_eq!(dispatched.slot, ReleaseSlot::Active);

    // Retrying the inconclusive candidate is legal and takes the pending slot.
    let retry = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: first.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?;
    assert!(!retry.dispatched);
    assert_eq!(retry.candidate.slot, ReleaseSlot::Pending);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn concurrent_activation_moves_the_pointer_once_db_memory() -> Result<()> {
    // Pins: the activation CAS and its audit row are atomic. Two attestations for
    // two candidates of the same artifact race; exactly one moves the pointer and
    // writes an audit row, and the loser fails closed.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("race-{}", Uuid::now_v7());

    let left = draft_skill(&registry, &scope, &name, "left").await?;
    let target = ActivationTarget::SkillVisibility {
        artifact_uid: left.artifact_uid,
    };
    let right = draft_skill(&registry, &scope, &name, "right").await?;

    let mut requests = Vec::new();
    for revision_uid in [left.revision_uid, right.revision_uid] {
        let candidate = repository
            .submit_candidate(SubmitCandidate {
                scope: release_scope,
                activation_target: target,
                candidate_revision_uid: revision_uid,
                subject_inputs: fixture_subject_inputs(),
                submitted_by: "operator".to_string(),
            })
            .await
            .map_err(release_error)?
            .candidate;
        // Both attempts must reach a decision, so the pending one is promoted by
        // deciding the active one first; the coalescer is exercised separately.
        if candidate.slot == ReleaseSlot::Pending {
            sqlx::query(
                "UPDATE moa.artifact_release_candidate SET slot = 'active' WHERE revision_uid = $1",
            )
            .bind(revision_uid)
            .execute(&pool)
            .await
            .map_err(storage_error)?;
            sqlx::query(
                "UPDATE moa.artifact_revision SET status = 'evaluating' WHERE revision_uid = $1",
            )
            .bind(revision_uid)
            .execute(&pool)
            .await
            .map_err(storage_error)?;
        }
        let candidate = repository
            .load_candidate(&release_scope, revision_uid)
            .await
            .map_err(release_error)?
            .expect("candidate");
        let attestation = repository
            .record_decision(pass_decision(release_scope, &candidate))
            .await
            .map_err(release_error)?
            .attestation
            .expect("attestation");
        requests.push(ActivationRequest {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: revision_uid,
            candidate_revision_hash: candidate.candidate_revision_hash,
            attestation_uid: attestation.attestation_uid,
            expected_serving: ExpectedServing {
                revision_uid: None,
                pointer_version: 0,
            },
            agent_revision_lock: None,
            actor: "operator".to_string(),
            reason: None,
        });
    }

    let second = requests.pop().expect("two requests");
    let first = requests.pop().expect("two requests");
    let left_repository = repository.clone();
    let right_repository = repository.clone();
    let (left_result, right_result) = tokio::join!(
        async move { left_repository.activate(first).await },
        async move { right_repository.activate(second).await }
    );
    let winners = [&left_result, &right_result]
        .iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(winners, 1, "exactly one concurrent activation may win");

    let audit_rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.artifact_activation_audit WHERE artifact_uid = $1 \
         AND decision_kind = 'activation'",
    )
    .bind(left.artifact_uid)
    .fetch_one(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(audit_rows, 1, "one audit row per pointer move");

    let pointer = registry
        .load_serving_pointer(&release_scope, left.artifact_uid)
        .await?
        .expect("one pointer exists");
    assert_eq!(pointer.pointer_version, 1);
    let consumed = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.artifact_activation_attestation WHERE artifact_uid = $1 \
         AND consumed_at IS NOT NULL",
    )
    .bind(left.artifact_uid)
    .fetch_one(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(consumed, 1, "the loser's attestation is not spent");

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn policy_and_certification_gaps_block_before_evaluation_db_memory() -> Result<()> {
    // Pins: an unresolvable release policy is refused before evaluation, and a
    // simulator-backed subject whose certification has lapsed cannot produce an
    // attestation.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);

    // A lapsed simulator certification is refused at submission.
    let lapsed_name = format!("lapsed-{}", Uuid::now_v7());
    let lapsed = draft_skill(&registry, &scope, &lapsed_name, "lapsed simulator").await?;
    let mut lapsed_inputs = fixture_subject_inputs();
    lapsed_inputs.simulator = Some(SimulatorPolicyBinding {
        policy_uid: Uuid::now_v7(),
        revision: 1,
        policy_hash: Digest32([3_u8; 32]),
        certified_until: chrono::Utc::now() - chrono::Duration::hours(1),
    });
    let lapsed_error = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: ActivationTarget::SkillVisibility {
                artifact_uid: lapsed.artifact_uid,
            },
            candidate_revision_uid: lapsed.revision_uid,
            subject_inputs: lapsed_inputs,
            submitted_by: "operator".to_string(),
        })
        .await
        .expect_err("an uncertified simulator cannot back a release subject");
    assert_rejection(
        &lapsed_error,
        ReleaseRejection::SimulatorCertificationExpired,
    );

    // A tool-bearing subject with no activated catalog snapshot is refused too.
    let tool_name = format!("tools-{}", Uuid::now_v7());
    let tool_draft = draft_skill(&registry, &scope, &tool_name, "tool bearing").await?;
    let mut tool_inputs = fixture_subject_inputs();
    tool_inputs.tool_bearing = true;
    let tool_error = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: ActivationTarget::SkillVisibility {
                artifact_uid: tool_draft.artifact_uid,
            },
            candidate_revision_uid: tool_draft.revision_uid,
            subject_inputs: tool_inputs,
            submitted_by: "operator".to_string(),
        })
        .await
        .expect_err("a tool-bearing subject needs an activated schema snapshot");
    assert_rejection(&tool_error, ReleaseRejection::ToolCatalogSnapshotMissing);

    // With no resolvable policy, submission is refused before any evaluation.
    sqlx::query("UPDATE moa.artifact_release_policy SET valid_to = now() WHERE target_class = $1")
        .bind(ActivationTargetClass::SkillVisibility.as_str())
        .execute(&pool)
        .await
        .map_err(storage_error)?;
    let policyless_name = format!("policyless-{}", Uuid::now_v7());
    let policyless = draft_skill(&registry, &scope, &policyless_name, "no policy").await?;
    let policy_error = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: ActivationTarget::SkillVisibility {
                artifact_uid: policyless.artifact_uid,
            },
            candidate_revision_uid: policyless.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .expect_err("no gate means no release attempt");
    assert_rejection(&policy_error, ReleaseRejection::PolicyNotFound);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn agent_readiness_alone_does_not_change_an_installation_db_memory() -> Result<()> {
    // Pins: an agent revision that is ready to activate changes no installation.
    // Only the attested activation moves `current_revision_uid`, and it advances
    // the installation's compare-and-set token.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("agent-{}", Uuid::now_v7());
    let document: ArtifactDocument = serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": { "name": name, "description": "release agent", "tags": [] },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": "Release Agent",
                "purpose": {
                    "summary": "Answer questions.",
                    "expected_outputs": ["answer"]
                },
                "tool_policy": { "mode": "allowlist", "tools": ["file_read"] }
            }
        }
    }))
    .expect("agent fixture is valid");
    let source = document.to_yaml().expect("serialize agent");
    let draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?;

    let installation_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.agent_installation (
            installation_uid, storage_partition_id, artifact_uid, definition_ref, display_name,
            status, current_revision_uid, serving_pointer_version
        )
        VALUES ($1, $2, $3, $4, 'Release Agent', 'inactive', NULL, 0)
        "#,
    )
    .bind(installation_uid)
    .bind(release_scope.storage_partition_id().to_string())
    .bind(draft.artifact_uid)
    .bind(format!("agent://{name}"))
    .execute(&pool)
    .await
    .map_err(storage_error)?;

    record_eligibility(&registry, &scope, draft.revision_uid).await?;
    let target = ActivationTarget::AgentDeployment {
        artifact_uid: draft.artifact_uid,
        installation_uid,
    };
    let revision_lock = AgentRevisionLock {
        agent_revision_uid: draft.revision_uid,
        artifact_dependencies: Vec::new(),
        tool_dependencies: Vec::new(),
        canonical_policy_hash: "fixture-agent-policy".to_string(),
    };
    let mut subject_inputs = fixture_subject_inputs();
    subject_inputs.dependency_lock_hash = Digest32(
        moa_artifacts::canonical::canonical_hash(&revision_lock)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
    );
    let candidate = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: draft.revision_uid,
            subject_inputs,
            submitted_by: "operator".to_string(),
        })
        .await
        .map_err(release_error)?
        .candidate;
    let attestation = repository
        .record_decision(pass_decision(release_scope, &candidate))
        .await
        .map_err(release_error)?
        .attestation
        .expect("attestation");

    let ready = repository
        .load_candidate(&release_scope, draft.revision_uid)
        .await
        .map_err(release_error)?
        .expect("candidate");
    assert_eq!(ready.state, ReleaseState::Ready);
    let current = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT current_revision_uid FROM moa.agent_installation WHERE installation_uid = $1",
    )
    .bind(installation_uid)
    .fetch_one(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(
        current, None,
        "readiness alone must not deploy anything into the installation"
    );

    let outcome = repository
        .activate(ActivationRequest {
            scope: release_scope,
            activation_target: target,
            candidate_revision_uid: draft.revision_uid,
            candidate_revision_hash: candidate.candidate_revision_hash,
            attestation_uid: attestation.attestation_uid,
            expected_serving: ExpectedServing {
                revision_uid: None,
                pointer_version: 0,
            },
            agent_revision_lock: Some(revision_lock),
            actor: "operator".to_string(),
            reason: None,
        })
        .await
        .map_err(release_error)?;
    assert!(outcome.deployment_uid.is_some());
    let (current, pointer_version) = sqlx::query_as::<_, (Option<Uuid>, i64)>(
        "SELECT current_revision_uid, serving_pointer_version FROM moa.agent_installation \
         WHERE installation_uid = $1",
    )
    .bind(installation_uid)
    .fetch_one(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(current, Some(draft.revision_uid));
    assert_eq!(pointer_version, 1);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

/// Builds a passing decision for a candidate with fixture evidence identifiers.
fn pass_decision(
    scope: TenantScope,
    candidate: &moa_artifacts::registry::ReleaseCandidate,
) -> RecordDecision {
    RecordDecision {
        scope,
        candidate_revision_uid: candidate.revision_uid,
        subject_digest: candidate.subject_digest,
        verdict: DeterministicVerdict::Pass,
        run_uid: Uuid::now_v7(),
        trial_uids: vec![Uuid::now_v7()],
        evidence_ids: vec![Uuid::now_v7()],
        gate_results: BTreeMap::from([("result_produced".to_string(), "pass".to_string())]),
        blocking_assertions: Vec::new(),
        evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
        decided_by: "release-evaluator".to_string(),
    }
}

/// Asserts the exact release predicate that refused.
///
/// Matching the typed rejection rather than a message keeps the assertion honest:
/// a predicate that silently stopped running cannot satisfy it by producing some
/// other error with similar text.
fn assert_rejection(error: &Error, expected: ReleaseRejection) {
    match error {
        Error::Release { rejection, .. } => assert_eq!(
            *rejection, expected,
            "expected rejection {expected}, got {rejection}: {error}"
        ),
        other => panic!("expected release rejection {expected}, got {other}"),
    }
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn release_policy_is_resolved_server_side_db_memory() -> Result<()> {
    // Pins: the gate is resolved from the policy table, a tenant override wins over
    // the platform default, and the resolved policy is validated before it can gate
    // anything. A submitter has no way to name a policy: the submit request carries
    // no policy field at all, which is why this test can only observe resolution.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let release_scope = TenantScope::new(tenant_id);

    let platform = repository
        .resolve_policy(&release_scope, ActivationTargetClass::SkillVisibility)
        .await
        .map_err(release_error)?;
    assert_eq!(platform.tenant_id, None, "the default is platform-owned");
    assert!(platform.name.starts_with("platform-default"));

    let override_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash
        )
        VALUES ($1, $2, NULL, 'tenant-strict', 3, 'skill_visibility', $3, $4, 3600, $5, $6)
        "#,
    )
    .bind(override_uid)
    .bind(release_scope.storage_partition_id().to_string())
    .bind(serde_json::json!([
        {"id": "target_completed", "version": "v1", "determinism": "deterministic"},
        {"id": "result_produced", "version": "v1", "determinism": "deterministic"},
        {"id": "privacy_safe_output", "version": "v1", "determinism": "deterministic"}
    ]))
    .bind(serde_json::json!([
        {"metric": "result_produced", "direction": "higher_is_better", "margin_bp": 100}
    ]))
    .bind(vec![7_u8; 32])
    .bind(vec![8_u8; 32])
    .execute(&pool)
    .await
    .map_err(storage_error)?;

    let resolved = repository
        .resolve_policy(&release_scope, ActivationTargetClass::SkillVisibility)
        .await
        .map_err(release_error)?;
    assert_eq!(resolved.policy_uid, override_uid);
    assert_eq!(resolved.revision, 3);
    assert_eq!(resolved.tenant_id, Some(tenant_id));

    // A policy row that could not block anything is refused when it is resolved,
    // not when a decision is recorded.
    sqlx::query(
        "UPDATE moa.artifact_release_policy SET blocking_assertions = $2 WHERE policy_uid = $1",
    )
    .bind(override_uid)
    .bind(serde_json::json!([
        {"id": "tenant.vibes", "version": "1", "determinism": "diagnostic"}
    ]))
    .execute(&pool)
    .await
    .map_err(storage_error)?;
    let invalid = repository
        .resolve_policy(&release_scope, ActivationTargetClass::SkillVisibility)
        .await
        .expect_err("a policy without platform blocking assertions cannot gate anything");
    assert_rejection(&invalid, ReleaseRejection::PolicyInvalid);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn unvalidated_candidate_cannot_enter_evaluation_db_memory() -> Result<()> {
    // Pins: eligibility is a real precondition. A revision whose declared
    // references were never resolved by generic validation cannot start a release
    // attempt, so it can never reach an attestation either.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let repository = ReleaseRepository::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let name = format!("tool-skill-{}", Uuid::now_v7());
    let document: ArtifactDocument = serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": { "name": name, "description": "declares a tool", "tags": [] },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "allowed_tools": ["file_read"]
            }
        }
    }))
    .expect("skill fixture is valid");
    let source = document.to_yaml().expect("serialize skill");
    let draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", b"# Skill\n".to_vec())],
            },
        )
        .await?;

    let unvalidated = repository
        .submit_candidate(SubmitCandidate {
            scope: release_scope,
            activation_target: ActivationTarget::SkillVisibility {
                artifact_uid: draft.artifact_uid,
            },
            candidate_revision_uid: draft.revision_uid,
            subject_inputs: fixture_subject_inputs(),
            submitted_by: "operator".to_string(),
        })
        .await
        .expect_err("an unvalidated candidate cannot enter evaluation");
    assert_rejection(&unvalidated, ReleaseRejection::CandidateNotEligible);

    // After validation resolves the declared reference, the same candidate is
    // accepted -- so the refusal above is about eligibility, not about the document.
    activate_revision(
        &pool,
        release_scope,
        ActivationTarget::SkillVisibility {
            artifact_uid: draft.artifact_uid,
        },
        draft.revision_uid,
    )
    .await
    .map_err(release_error)?;
    assert!(
        registry
            .load_serving(&scope, ArtifactKind::Skill, &name)
            .await?
            .is_some()
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL"]
async fn the_serving_pointer_compare_and_swap_refuses_a_version_it_did_not_observe_db_memory()
-> Result<()> {
    // Pins: the compare-and-swap in front of a serving-pointer move admits the
    // version the caller observed and refuses any other.
    //
    // This is driven directly rather than through `activate` on purpose. Activation
    // reads the pointer `FOR UPDATE`, compares the observed version against the
    // caller's expectation, and recomputes a subject digest that contains
    // `pointer_version` -- so a concurrent mover is refused twice before this
    // statement runs, and the state the compare-and-swap defends against is
    // unreachable from that path. A test confined to activation therefore cannot
    // distinguish a working fence from a deleted predicate, which is exactly the
    // gap this closes: delete `AND pointer_version = $8` and the stale case below
    // starts admitting the move.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope =
        TenantScope::from_action_rule_scope(&scope).expect("tenant scope for release");

    let name = format!("cas-{}", Uuid::now_v7());
    let v1 = draft_skill(&registry, &scope, &name, "cas-v1").await?;
    let v2 = draft_skill(&registry, &scope, &name, "cas-v2").await?;
    let target = ActivationTarget::SkillVisibility {
        artifact_uid: v1.artifact_uid,
    };

    // A real activation establishes the pointer, so the fence runs against state
    // the production path produced rather than a hand-built row.
    let activated = activate_revision(&pool, release_scope, target, v1.revision_uid)
        .await
        .map_err(release_error)?;
    let serving_version = activated.pointer_version;
    let v2_attested = moa_artifacts::test_fixtures::attest_revision(
        &pool,
        release_scope,
        target,
        v2.revision_uid,
    )
    .await
    .map_err(release_error)?;

    // Stale: one version behind what is actually stored.
    let stale = moa_artifacts::test_fixtures::compare_and_swap_serving_pointer(
        &pool,
        release_scope,
        target,
        v2.revision_uid,
        v2_attested.attestation_uid,
        serving_version - 1,
        serving_version,
    )
    .await
    .map_err(release_error)?;
    assert!(
        !stale.admitted(),
        "a stale expected version must not move the pointer, affected {} row(s)",
        stale.rows_affected
    );

    // Ahead: a version nobody has reached yet is equally unobserved.
    let ahead = moa_artifacts::test_fixtures::compare_and_swap_serving_pointer(
        &pool,
        release_scope,
        target,
        v2.revision_uid,
        v2_attested.attestation_uid,
        serving_version + 7,
        serving_version + 8,
    )
    .await
    .map_err(release_error)?;
    assert!(
        !ahead.admitted(),
        "an unreached version must not move the pointer"
    );

    // The refusals left the pointer exactly where the real activation put it.
    let (serving_revision, stored_version): (Uuid, i64) = sqlx::query_as(
        "SELECT revision_uid, pointer_version FROM moa.artifact_serving_pointer \
         WHERE artifact_uid = $1",
    )
    .bind(v1.artifact_uid)
    .fetch_one(&pool)
    .await
    .map_err(storage_error)?;
    assert_eq!(
        serving_revision, v1.revision_uid,
        "a refused move must not serve"
    );
    assert_eq!(
        stored_version, serving_version,
        "a refused move must not bump the version"
    );

    // Exactly the observed version is admitted, which is what proves the two
    // refusals above came from the predicate and not from a fence that refuses
    // everything.
    let observed = moa_artifacts::test_fixtures::compare_and_swap_serving_pointer(
        &pool,
        release_scope,
        target,
        v2.revision_uid,
        v2_attested.attestation_uid,
        serving_version,
        serving_version + 1,
    )
    .await
    .map_err(release_error)?;
    assert!(
        observed.admitted(),
        "the observed version must move the pointer"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

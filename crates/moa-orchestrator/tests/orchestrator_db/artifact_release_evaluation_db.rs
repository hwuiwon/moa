//! Postgres-backed coverage for release-candidate evaluation dispatch.
//!
//! What is pinned here is the part `V000373` left open: a submission now carries a
//! durable dispatch record, ten rapid submissions still coalesce to one open
//! dispatch, a result for a superseded generation cannot make a revision ready, the
//! evaluation overlay is unreachable from normal session resolution, and the hidden
//! release cohort rotates and runs out.

use std::collections::BTreeMap;

use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, CandidateSubjectInputs, NewArtifactDraft, NewArtifactFile, RecordDecision,
    StoredArtifactRevision, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationTarget, ActivationTargetClass, AssertionRef, DeterminismClass, DeterministicVerdict,
    EvidenceAdapter, PLATFORM_BLOCKING_ASSERTIONS, ReleaseSlot, ReleaseState, TenantScope,
};
use moa_artifacts::test_fixtures::fixture_subject_inputs;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use moa_orchestrator::workflows::artifact_release_evaluation::Error as ReleaseEvaluationError;
use moa_orchestrator::workflows::artifact_release_evaluation::dispatch::build_paired_run_request;
use moa_orchestrator::workflows::artifact_release_evaluation::repository::ReleaseEvaluationRepository;
use moa_orchestrator::workflows::artifact_release_evaluation::types::{
    ArmRole, AttemptReviewState, DispatchStatus, PinnedDependency, dispatch_idempotency_key,
    release_seed_material,
};
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

mod artifact_release_evaluation {
    use super::*;

    /// One seeded tenant with a draft skill candidate and an approved case pack.
    struct Fixture {
        pool: PgPool,
        tenant_id: TenantId,
        scope: ActionRuleScope,
        release_scope: TenantScope,
        registry: ArtifactRegistry,
        repository: ReleaseEvaluationRepository,
        artifact_name: String,
        subject_inputs: CandidateSubjectInputs,
    }

    impl Fixture {
        async fn draft(&self, description: &str) -> StoredArtifactRevision {
            draft_skill(
                &self.registry,
                &self.scope,
                &self.artifact_name,
                description,
            )
            .await
        }

        fn target(&self, artifact_uid: Uuid) -> ActivationTarget {
            ActivationTarget::SkillVisibility { artifact_uid }
        }

        fn submit(&self, revision_uid: Uuid, artifact_uid: Uuid) -> SubmitCandidate {
            SubmitCandidate {
                scope: self.release_scope,
                activation_target: self.target(artifact_uid),
                candidate_revision_uid: revision_uid,
                subject_inputs: self.subject_inputs.clone(),
                submitted_by: "operator".to_string(),
            }
        }
    }

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
    ) -> StoredArtifactRevision {
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
            .expect("create draft skill")
    }

    /// Seeds a tenant, certified simulator, published release plan, candidate,
    /// and tenant case-pack supplement.
    async fn fixture(label: &str) -> (moa_test_support::postgres::TestDb, Fixture) {
        let test_db = bootstrap_test_db()
            .await
            .expect("bootstrap release-evaluation db");
        let pool = test_db.store().pool().clone();
        let tenant_id = TenantId::from(Uuid::now_v7());
        let scope = ActionRuleScope::Tenant { tenant_id };
        let registry = ArtifactRegistry::new(pool.clone());
        let artifact_name = format!("{label}-{}", Uuid::now_v7());
        let environment = crate::artifact_release::seed_environment(
            &pool,
            tenant_id,
            ActivationTargetClass::SkillVisibility,
            label,
        )
        .await
        .expect("seed release environment");
        let repository = ReleaseEvaluationRepository::new(pool.clone());
        let mut subject_inputs = fixture_subject_inputs();
        subject_inputs.plan = environment.plan;
        subject_inputs.simulator = Some(environment.simulator);
        let fixture = Fixture {
            pool: pool.clone(),
            tenant_id,
            scope,
            release_scope: TenantScope::new(tenant_id),
            registry,
            repository,
            artifact_name,
            subject_inputs,
        };
        (test_db, fixture)
    }

    fn pass_decision(
        scope: TenantScope,
        revision_uid: Uuid,
        subject_digest: moa_artifacts::release::Digest32,
        verdict: DeterministicVerdict,
    ) -> RecordDecision {
        RecordDecision {
            scope,
            candidate_revision_uid: revision_uid,
            subject_digest,
            verdict,
            run_uid: Uuid::now_v7(),
            trial_uids: vec![Uuid::now_v7()],
            evidence_ids: vec![Uuid::now_v7()],
            gate_results: BTreeMap::from([("result_produced".to_string(), "pass".to_string())]),
            blocking_assertions: PLATFORM_BLOCKING_ASSERTIONS
                .iter()
                .map(|id| AssertionRef {
                    id: (*id).to_string(),
                    version: "1".to_string(),
                    determinism: DeterminismClass::Deterministic,
                })
                .collect(),
            evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
            decided_by: "admin".to_string(),
        }
    }

    #[tokio::test]
    async fn submission_and_dispatch_record_commit_together_db() {
        // Pins: a committed submission always carries a durable dispatch record
        // whose identity is deterministic in (revision, generation, subject digest),
        // and whose seeds both arms of the attempt will share.
        let (_db, fixture) = fixture("dispatch-record").await;
        let draft = fixture.draft("rev-1").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit with dispatch");

        assert!(submitted.submission.dispatched);
        let record = submitted
            .dispatch
            .expect("a dispatched submission enqueues");
        assert_eq!(record.status, DispatchStatus::Pending);
        assert_eq!(record.revision_uid, draft.revision_uid);
        assert_eq!(
            record.idempotency_key,
            dispatch_idempotency_key(
                draft.revision_uid,
                submitted.submission.candidate.generation,
                &submitted.submission.candidate.subject_digest
            ),
            "the dispatch key must be derivable from the fenced identity alone"
        );
        assert_eq!(
            record.seed_material,
            release_seed_material(
                fixture.tenant_id,
                draft.revision_uid,
                submitted.submission.candidate.generation,
                &submitted.submission.candidate.subject_digest
            ),
            "both arms must run seeds derived from the fenced identity"
        );

        let stored: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_release_dispatch_outbox WHERE revision_uid = $1",
        )
        .bind(draft.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count dispatch records");
        assert_eq!(stored, 1);
    }

    #[tokio::test]
    async fn an_unowned_pinned_dependency_is_refused_db() {
        // Pins: the evaluation overlay can only pin revisions the submitting tenant
        // owns, so a submitter cannot make evaluation resolve someone else's draft.
        let (_db, fixture) = fixture("pinned-dependency").await;
        let draft = fixture.draft("rev-1").await;
        let error = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                vec![PinnedDependency {
                    artifact_uid: Uuid::now_v7(),
                    revision_uid: Uuid::now_v7(),
                }],
            )
            .await
            .expect_err("an unowned pin must be refused");
        assert!(
            matches!(error, ReleaseEvaluationError::PinnedDependencyInvalid(_)),
            "unexpected error: {error}"
        );
        // The whole submission rolled back with it: there is no committed candidate
        // without a dispatch record and no dispatch record without a candidate.
        let candidates: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_release_candidate WHERE revision_uid = $1",
        )
        .bind(draft.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count candidates");
        assert_eq!(candidates, 0);
    }

    #[tokio::test]
    async fn ten_submissions_yield_one_open_dispatch_and_the_newest_runs_db() {
        // Pins: ten distinct candidates for one artifact collapse to one open
        // dispatch plus the newest pending subject, and the decision that frees the
        // slot enqueues a dispatch record for that newest subject rather than
        // starving it.
        let (_db, fixture) = fixture("coalesce").await;
        let first = fixture.draft("rev-1").await;
        let active = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(first.revision_uid, first.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("first submission");
        assert!(active.submission.dispatched);

        let mut later = Vec::new();
        for index in 2..=10 {
            let draft = fixture.draft(&format!("rev-{index}")).await;
            let submitted = fixture
                .repository
                .submit_with_dispatch(
                    fixture.submit(draft.revision_uid, draft.artifact_uid),
                    Vec::new(),
                )
                .await
                .expect("later submission");
            assert!(
                !submitted.submission.dispatched,
                "only one attempt may hold the active slot"
            );
            assert!(
                submitted.dispatch.is_none(),
                "a pending subject must not enqueue a second open dispatch"
            );
            assert_eq!(submitted.submission.candidate.slot, ReleaseSlot::Pending);
            later.push(draft.revision_uid);
        }
        let newest = *later.last().expect("nine later submissions");

        let open: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT revision_uid, status FROM moa.artifact_release_dispatch_outbox \
             WHERE artifact_uid = $1 AND status IN ('pending', 'dispatched')",
        )
        .bind(first.artifact_uid)
        .fetch_all(&fixture.pool)
        .await
        .expect("load open dispatch records");
        assert_eq!(
            open,
            vec![(first.revision_uid, "pending".to_string())],
            "exactly one dispatch record may be open for an artifact"
        );

        // An inconclusive result releases the slot. The newest pending subject is
        // promoted and given its own dispatch record in the same transaction.
        let candidate = active.submission.candidate;
        fixture
            .repository
            .claim_dispatch(
                fixture.tenant_id,
                active.dispatch.expect("record").outbox_uid,
            )
            .await
            .expect("claim")
            .expect("an open record is claimable");
        let fenced = fixture
            .repository
            .record_decision_with_fence(
                pass_decision(
                    fixture.release_scope,
                    candidate.revision_uid,
                    candidate.subject_digest,
                    DeterministicVerdict::Inconclusive,
                ),
                candidate.generation,
                json!({}),
            )
            .await
            .expect("inconclusive decision");
        assert!(
            fenced.outcome.attestation.is_none(),
            "an inconclusive result must not mint a permission to serve"
        );
        assert_eq!(fenced.outcome.dispatched_revision_uid, Some(newest));
        let next = fenced.next.expect("the newest subject is dispatched");
        assert_eq!(next.revision_uid, newest);
        assert_eq!(next.status, DispatchStatus::Pending);
    }

    #[tokio::test]
    async fn a_superseded_generation_cannot_make_a_revision_ready_db() {
        // Pins: the generation fence runs before the candidate state moves, so a
        // result for a stale generation leaves the candidate evaluating, mints no
        // attestation, and is recorded as fenced out. Settling twice is refused for
        // the same reason, which is what stops a replay from creating a second
        // activation opportunity.
        let (_db, fixture) = fixture("generation-fence").await;
        let draft = fixture.draft("rev-1").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit");
        let candidate = submitted.submission.candidate;
        let record = submitted.dispatch.expect("record");
        fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim")
            .expect("claimable");

        let stale = fixture
            .repository
            .record_decision_with_fence(
                pass_decision(
                    fixture.release_scope,
                    candidate.revision_uid,
                    candidate.subject_digest,
                    DeterministicVerdict::Pass,
                ),
                candidate.generation + 7,
                json!({}),
            )
            .await
            .expect_err("a stale generation must be refused");
        assert!(
            matches!(stale, ReleaseEvaluationError::StaleDispatch(_)),
            "unexpected error: {stale}"
        );

        let state: String =
            sqlx::query_scalar("SELECT status FROM moa.artifact_revision WHERE revision_uid = $1")
                .bind(draft.revision_uid)
                .fetch_one(&fixture.pool)
                .await
                .expect("load revision status");
        assert_eq!(
            state,
            ReleaseState::Evaluating.as_str(),
            "a fenced result must not move the candidate"
        );
        let attestations: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_activation_attestation WHERE candidate_revision_uid = $1",
        )
        .bind(draft.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count attestations");
        assert_eq!(attestations, 0);

        // The correct generation is accepted once and only once.
        let accepted = fixture
            .repository
            .record_decision_with_fence(
                pass_decision(
                    fixture.release_scope,
                    candidate.revision_uid,
                    candidate.subject_digest,
                    DeterministicVerdict::Pass,
                ),
                candidate.generation,
                json!({}),
            )
            .await
            .expect("the running generation is accepted");
        assert!(accepted.outcome.attestation.is_some());
        assert_eq!(accepted.settled_outbox_uid, record.outbox_uid);

        let replayed = fixture
            .repository
            .record_decision_with_fence(
                pass_decision(
                    fixture.release_scope,
                    candidate.revision_uid,
                    candidate.subject_digest,
                    DeterministicVerdict::Pass,
                ),
                candidate.generation,
                json!({}),
            )
            .await
            .expect_err("a settled record cannot be settled again");
        assert!(
            matches!(replayed, ReleaseEvaluationError::StaleDispatch(_)),
            "unexpected error: {replayed}"
        );
        let minted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_activation_attestation WHERE candidate_revision_uid = $1",
        )
        .bind(draft.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count attestations");
        assert_eq!(
            minted, 1,
            "a replayed decision must not create a second activation opportunity"
        );
    }

    #[tokio::test]
    async fn normal_sessions_cannot_resolve_the_evaluation_overlay_db() {
        // Pins: the overlay resolves the exact unpublished candidate and its pinned
        // draft dependency, while the normal serving resolver still resolves
        // nothing; and the overlay stops answering without its secret, in another
        // arm's session, or after the attempt closes.
        let (_db, fixture) = fixture("overlay").await;
        let dependency = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &format!("dependency-{}", Uuid::now_v7()),
            "pinned draft dependency",
        )
        .await;
        let draft = fixture.draft("rev-1").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                vec![PinnedDependency {
                    artifact_uid: dependency.artifact_uid,
                    revision_uid: dependency.revision_uid,
                }],
            )
            .await
            .expect("submit");
        let record = submitted.dispatch.expect("record");
        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim")
            .expect("claimable");

        let attempt = fixture
            .repository
            .provision_attempt(
                &claimed.record,
                ActivationTargetClass::SkillVisibility,
                // A synthetic baseline revision so both arms are provisioned: the
                // per-arm isolation below is exactly what a single-arm attempt
                // could not exercise.
                Some(dependency.revision_uid),
                &[
                    (ArmRole::Candidate, "candidate-secret".to_string()),
                    (ArmRole::Baseline, "baseline-secret".to_string()),
                ],
            )
            .await
            .expect("provision attempt");
        let arm = attempt.arm(ArmRole::Candidate).expect("candidate arm");
        let baseline_arm = attempt.arm(ArmRole::Baseline).expect("baseline arm");

        let run_request = build_paired_run_request(
            fixture.tenant_id,
            &claimed.record,
            ActivationTargetClass::SkillVisibility,
            &attempt,
        )
        .expect("build release run request");
        let binding = run_request
            .release_evaluation
            .expect("release run carries its durable binding");
        assert_eq!(binding.cases.len(), 5);
        assert_eq!(
            binding
                .cases
                .iter()
                .map(|case| case.repetitions)
                .sum::<u32>(),
            14,
            "the run must carry the two platform authoring cases, tenant supplement, and two-case hidden cohort"
        );
        fixture
            .repository
            .validate_experiment_binding(
                fixture.tenant_id,
                attempt.plan.plan_revision_uid,
                &claimed.record.idempotency_key,
                &binding,
            )
            .await
            .expect("the exact provisioned binding is admitted");
        let mut forged = binding.clone();
        forged.arms[0].eval_session_id = Uuid::now_v7();
        let error = fixture
            .repository
            .validate_experiment_binding(
                fixture.tenant_id,
                attempt.plan.plan_revision_uid,
                &claimed.record.idempotency_key,
                &forged,
            )
            .await
            .expect_err("a forged evaluation session must be refused");
        assert!(matches!(
            error,
            ReleaseEvaluationError::ExperimentBindingInvalid(_)
        ));
        let mut forged = binding.clone();
        forged.cases.pop();
        let error = fixture
            .repository
            .validate_experiment_binding(
                fixture.tenant_id,
                attempt.plan.plan_revision_uid,
                &claimed.record.idempotency_key,
                &forged,
            )
            .await
            .expect_err("a release run cannot omit one approved case");
        assert!(matches!(
            error,
            ReleaseEvaluationError::ExperimentBindingInvalid(_)
        ));

        assert_ne!(
            arm.eval_session_id, baseline_arm.eval_session_id,
            "each arm runs under its own eval-owned session"
        );
        assert_ne!(
            arm.fixture_uid, baseline_arm.fixture_uid,
            "each arm gets its own writable environment"
        );

        // The overlay resolves the draft; the normal resolver still resolves
        // nothing, because serving is a pointer and no pointer was moved.
        assert_eq!(
            fixture
                .repository
                .resolve_overlay_revision(
                    fixture.tenant_id,
                    arm.overlay_uid,
                    &arm.overlay_token,
                    arm.eval_session_id,
                    draft.artifact_uid,
                )
                .await
                .expect("overlay resolution"),
            Some(draft.revision_uid)
        );
        assert_eq!(
            fixture
                .repository
                .resolve_overlay_revision(
                    fixture.tenant_id,
                    arm.overlay_uid,
                    &arm.overlay_token,
                    arm.eval_session_id,
                    dependency.artifact_uid,
                )
                .await
                .expect("pinned dependency resolution"),
            Some(dependency.revision_uid)
        );
        assert!(
            fixture
                .registry
                .load_serving(&fixture.scope, ArtifactKind::Skill, &fixture.artifact_name)
                .await
                .expect("serving resolution")
                .is_none(),
            "an overlay must not make a draft resolvable to a normal session"
        );

        // Without the secret, in a session bound to another arm, or for an
        // unpinned artifact, the overlay answers nothing.
        for (label, token, session) in [
            ("wrong token", "not-the-secret", arm.eval_session_id),
            (
                "another arm's session",
                arm.overlay_token.as_str(),
                baseline_arm.eval_session_id,
            ),
        ] {
            assert_eq!(
                fixture
                    .repository
                    .resolve_overlay_revision(
                        fixture.tenant_id,
                        arm.overlay_uid,
                        token,
                        session,
                        draft.artifact_uid,
                    )
                    .await
                    .expect("overlay resolution"),
                None,
                "{label} must not resolve the overlay"
            );
        }
        assert_eq!(
            fixture
                .repository
                .resolve_overlay_revision(
                    fixture.tenant_id,
                    arm.overlay_uid,
                    &arm.overlay_token,
                    arm.eval_session_id,
                    Uuid::now_v7(),
                )
                .await
                .expect("overlay resolution"),
            None,
            "an artifact the overlay never pinned must not resolve through it"
        );

        // Each arm gets its own writable clone of the immutable base fixture, and
        // none of them can hold a production credential.
        let fixtures: Vec<(String, bool, Option<Uuid>)> = sqlx::query_as(
            "SELECT name, writable, base_fixture_uid FROM moa.artifact_release_fixture \
             WHERE storage_partition_id = $1 ORDER BY name",
        )
        .bind(fixture.tenant_id.to_string())
        .fetch_all(&fixture.pool)
        .await
        .expect("load fixtures");
        assert_eq!(
            fixtures.len(),
            3,
            "one base snapshot plus one clone per arm"
        );
        assert!(
            fixtures
                .iter()
                .filter(|(_, writable, base)| *writable && base.is_some())
                .count()
                == 2,
            "both arms must get their own writable environment: {fixtures:?}"
        );
        let base_uid: Uuid = sqlx::query_scalar(
            "SELECT fixture_uid FROM moa.artifact_release_fixture \
             WHERE storage_partition_id = $1 AND base_fixture_uid IS NULL",
        )
        .bind(fixture.tenant_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .expect("load base fixture");
        let mutated_base = sqlx::query(
            "UPDATE moa.artifact_release_fixture SET environment = '{\"records\":[1]}'::JSONB \
             WHERE fixture_uid = $1",
        )
        .bind(base_uid)
        .execute(&fixture.pool)
        .await;
        assert!(
            mutated_base.is_err(),
            "a shared base snapshot must be immutable, so the environment is copy-on-write"
        );
        let credential = sqlx::query(
            "UPDATE moa.artifact_release_fixture \
             SET connector_bindings = '[{\"connector\":\"gmail\",\"credential_source\":\"vault\"}]'::JSONB \
             WHERE fixture_uid = $1",
        )
        .bind(arm.fixture_uid)
        .execute(&fixture.pool)
        .await;
        assert!(
            credential.is_err(),
            "a fixture environment must not be able to name a production credential source"
        );

        // Closing the attempt closes the overlay, so a crashed or settled attempt
        // stops being able to resolve a draft revision.
        sqlx::query(
            "UPDATE moa.artifact_release_eval_overlay SET closed_at = now() WHERE overlay_uid = $1",
        )
        .bind(arm.overlay_uid)
        .execute(&fixture.pool)
        .await
        .expect("close overlay");
        assert_eq!(
            fixture
                .repository
                .resolve_overlay_revision(
                    fixture.tenant_id,
                    arm.overlay_uid,
                    &arm.overlay_token,
                    arm.eval_session_id,
                    draft.artifact_uid,
                )
                .await
                .expect("overlay resolution"),
            None,
            "a closed overlay must stop resolving"
        );
    }

    #[tokio::test]
    async fn hidden_cohort_rotates_and_bounds_release_attempts_db() {
        // Pins: the hidden cohort exposes a rotating window rather than a fixed set,
        // and one artifact may only be measured against one epoch a bounded number
        // of times before the release fails closed.
        let (_db, fixture) = fixture("hidden-cohort").await;
        let hidden_pack_uid: Uuid = sqlx::query_scalar(
            "SELECT pack_uid FROM moa.artifact_release_case_pack \
             WHERE storage_partition_id IS NULL AND visibility = 'hidden' \
               AND target_class = 'skill_visibility' AND valid_to IS NULL",
        )
        .fetch_one(&fixture.pool)
        .await
        .expect("load platform hidden cohort");

        let first: serde_json::Value =
            sqlx::query_scalar("SELECT moa.select_release_hidden_cohort($1, 1)")
                .bind(hidden_pack_uid)
                .fetch_one(&fixture.pool)
                .await
                .expect("epoch one cohort");
        let second: serde_json::Value =
            sqlx::query_scalar("SELECT moa.select_release_hidden_cohort($1, 2)")
                .bind(hidden_pack_uid)
                .fetch_one(&fixture.pool)
                .await
                .expect("epoch two cohort");
        assert_ne!(
            first, second,
            "rotation must change which hidden cases decide a release"
        );

        let budget: i32 = sqlx::query_scalar(
            "SELECT max_attempts_per_epoch FROM moa.artifact_release_case_pack WHERE pack_uid = $1",
        )
        .bind(hidden_pack_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load attempt budget");

        // Spend the whole budget through the real provisioning path, then prove the
        // next attempt is refused rather than silently downgraded to the authoring
        // cases.
        let mut spent = 0;
        let mut last_error = None;
        for index in 1..=(budget + 1) {
            let draft = fixture.draft(&format!("rev-{index}")).await;
            let submitted = fixture
                .repository
                .submit_with_dispatch(
                    fixture.submit(draft.revision_uid, draft.artifact_uid),
                    Vec::new(),
                )
                .await
                .expect("submit");
            let Some(record) = submitted.dispatch else {
                // Later submissions coalesce into the pending slot; free the active
                // slot so the next iteration can dispatch its own attempt.
                let candidate = submitted.submission.candidate;
                assert_eq!(candidate.slot, ReleaseSlot::Pending);
                continue;
            };
            let claimed = fixture
                .repository
                .claim_dispatch(fixture.tenant_id, record.outbox_uid)
                .await
                .expect("claim")
                .expect("claimable");
            match fixture
                .repository
                .provision_attempt(
                    &claimed.record,
                    ActivationTargetClass::SkillVisibility,
                    None,
                    &[
                        (ArmRole::Candidate, format!("candidate-{index}")),
                        (ArmRole::Baseline, format!("baseline-{index}")),
                    ],
                )
                .await
            {
                Ok(_) => spent += 1,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
            // Settle so the next candidate can take the active slot.
            let candidate = submitted.submission.candidate;
            fixture
                .repository
                .record_decision_with_fence(
                    pass_decision(
                        fixture.release_scope,
                        candidate.revision_uid,
                        candidate.subject_digest,
                        DeterministicVerdict::Inconclusive,
                    ),
                    candidate.generation,
                    json!({}),
                )
                .await
                .expect("settle attempt");
        }
        assert_eq!(
            spent, budget,
            "the hidden cohort budget must be exactly what the pack declares"
        );
        assert!(
            matches!(
                last_error,
                Some(ReleaseEvaluationError::HiddenCohortBudgetExhausted(_))
            ),
            "the attempt past the budget must fail closed: {last_error:?}"
        );
    }

    #[tokio::test]
    async fn release_attempts_are_reviewed_on_the_release_surface_db() {
        // Pins: attempt and attestation review live on the artifact-release surface,
        // the review cannot record the unreviewed state, and the reported row never
        // carries hidden cohort contents.
        let (_db, fixture) = fixture("attempt-review").await;
        let draft = fixture.draft("rev-1").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit");
        let record = submitted.dispatch.expect("record");
        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim")
            .expect("claimable");
        let attempt = fixture
            .repository
            .provision_attempt(
                &claimed.record,
                ActivationTargetClass::SkillVisibility,
                None,
                &[(ArmRole::Candidate, "candidate-secret".to_string())],
            )
            .await
            .expect("provision");

        let listed = fixture
            .repository
            .list_attempts(fixture.tenant_id, 10)
            .await
            .expect("list attempts");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].attempt_uid, attempt.attempt_uid);
        assert_eq!(listed[0].review_state, "unreviewed");
        assert_eq!(listed[0].generation, claimed.record.generation);
        assert!(!listed[0].fenced_out);

        let reviewed = fixture
            .repository
            .review_attempt(
                fixture.tenant_id,
                attempt.attempt_uid,
                AttemptReviewState::Disputed,
                "admin",
                Some("baseline drifted"),
            )
            .await
            .expect("review attempt");
        assert_eq!(reviewed.review_state, "disputed");
        assert_eq!(reviewed.reviewed_by.as_deref(), Some("admin"));
        assert!(reviewed.reviewed_at.is_some());

        let refused = fixture
            .repository
            .review_attempt(
                fixture.tenant_id,
                attempt.attempt_uid,
                AttemptReviewState::Unreviewed,
                "admin",
                None,
            )
            .await
            .expect_err("a review cannot un-review an attempt");
        assert!(
            matches!(refused, ReleaseEvaluationError::ReviewInvalid(_)),
            "unexpected error: {refused}"
        );

        // Another tenant cannot review it, and cannot see it either.
        let stranger = TenantId::from(Uuid::now_v7());
        assert!(
            fixture
                .repository
                .list_attempts(stranger, 10)
                .await
                .expect("list for another tenant")
                .is_empty()
        );
        assert!(
            fixture
                .repository
                .review_attempt(
                    stranger,
                    attempt.attempt_uid,
                    AttemptReviewState::Acknowledged,
                    "stranger",
                    None,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn raw_transcript_release_cases_are_unrepresentable_db() {
        // Pins: the schema refuses a case body carrying conversational content and a
        // learned scenario source missing erasure provenance, so a release gate
        // cannot be built out of raw transcripts.
        let (_db, fixture) = fixture("case-pack-shape").await;
        let transcript = sqlx::query(
            r#"
            INSERT INTO moa.artifact_release_case_pack (
                pack_uid, storage_partition_id, user_id, name, revision, target_class,
                visibility, cohort_epoch, cases, mandatory_assertions, scenario_source, pack_hash
            )
            VALUES ($1, $2, NULL, 'transcript-pack', 1, 'action_visibility', 'authoring', 1,
                    '[{"case_id":"c","transcript":[{"role":"user"}]}]'::JSONB,
                    '["target_completed"]'::JSONB,
                    '{"kind":"approved_pack"}'::JSONB, digest('transcript', 'sha256'))
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(fixture.tenant_id.to_string())
        .execute(&fixture.pool)
        .await;
        assert!(
            transcript.is_err(),
            "a case body carrying a transcript must be unrepresentable"
        );

        let unerasable = sqlx::query(
            r#"
            INSERT INTO moa.artifact_release_case_pack (
                pack_uid, storage_partition_id, user_id, name, revision, target_class,
                visibility, cohort_epoch, cases, mandatory_assertions, scenario_source, pack_hash
            )
            VALUES ($1, $2, NULL, 'learned-pack', 1, 'action_visibility', 'authoring', 1,
                    '[{"case_id":"c","persona_ref":"p","profile":"default","repetitions":1,"assertions":[]}]'::JSONB,
                    '["target_completed"]'::JSONB,
                    '{"kind":"learned","evidence":{"contribution_uid":"00000000-0000-4000-8000-000000000001","retention_class":"short","consent_basis":"contract"}}'::JSONB,
                    digest('learned', 'sha256'))
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(fixture.tenant_id.to_string())
        .execute(&fixture.pool)
        .await;
        assert!(
            unerasable.is_err(),
            "learned scenario input without erasure provenance must be unrepresentable"
        );

        let complete = sqlx::query(
            r#"
            INSERT INTO moa.artifact_release_case_pack (
                pack_uid, storage_partition_id, user_id, name, revision, target_class,
                visibility, cohort_epoch, cases, mandatory_assertions, scenario_source, pack_hash
            )
            VALUES ($1, $2, NULL, 'learned-pack', 1, 'action_visibility', 'authoring', 1,
                    '[{"case_id":"c","persona_ref":"p","profile":"default","repetitions":1,"assertions":[]}]'::JSONB,
                    '["target_completed"]'::JSONB,
                    '{"kind":"learned","evidence":{"contribution_uid":"00000000-0000-4000-8000-000000000001","retention_class":"short","consent_basis":"contract","erasure_provenance":"moa.artifact_revision_contribution"}}'::JSONB,
                    digest('learned', 'sha256'))
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(fixture.tenant_id.to_string())
        .execute(&fixture.pool)
        .await;
        assert!(
            complete.is_ok(),
            "full provenance must be accepted: {complete:?}"
        );

        // And the status guard still refuses to let a candidate serve without a
        // pointer, which is what the dispatch above is gating.
        let draft = fixture.draft("rev-1").await;
        assert_eq!(
            draft.status,
            ArtifactStatus::Draft,
            "an import creates a draft, never a serving revision"
        );
    }

    #[tokio::test]
    async fn the_overlay_resolver_substitutes_the_candidate_and_leaks_nothing_else_db() {
        // Pins the seam that makes a release evaluation evaluate the *candidate*.
        //
        // The overlay row and its SQL guard were already proven above, but nothing
        // consumed them: a dispatched run resolved artifacts through the serving
        // pointer, so the gate would have decided on whatever the tenant already
        // served while every predicate around it looked correct. This drives the
        // consumer — `ArtifactRegistry::load_serving_with_overlay` — and asserts the
        // substitution happens, is confined to pinned artifacts, and is not a bypass.
        let (_db, fixture) = fixture("overlay-consumer").await;
        let unpinned = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &format!("unpinned-{}", Uuid::now_v7()),
            "never pinned by the overlay",
        )
        .await;
        let draft = fixture.draft("candidate under evaluation").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit");
        let record = submitted.dispatch.expect("record");
        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim")
            .expect("claimable");
        let attempt = fixture
            .repository
            .provision_attempt(
                &claimed.record,
                ActivationTargetClass::SkillVisibility,
                None,
                &[(ArmRole::Candidate, "candidate-secret".to_string())],
            )
            .await
            .expect("provision attempt");
        let arm = attempt.arm(ArmRole::Candidate).expect("candidate arm");
        let binding = moa_artifacts::release::EvalOverlayBinding {
            overlay_uid: arm.overlay_uid,
            overlay_token: arm.overlay_token.clone(),
            eval_session_id: arm.eval_session_id,
        };

        // Substitution: under the binding the candidate resolves even though nothing
        // serves. This is the assertion that was missing entirely.
        let resolved = fixture
            .registry
            .load_serving_with_overlay(
                &fixture.scope,
                ArtifactKind::Skill,
                &fixture.artifact_name,
                Some(&binding),
            )
            .await
            .expect("overlay resolution")
            .expect("the candidate resolves under its own overlay");
        assert_eq!(
            resolved.revision_uid, draft.revision_uid,
            "the overlay must resolve the exact candidate revision"
        );

        // Not a bypass: the same call without the binding resolves nothing, because
        // serving is a pointer and no pointer was moved.
        assert!(
            fixture
                .registry
                .load_serving_with_overlay(
                    &fixture.scope,
                    ArtifactKind::Skill,
                    &fixture.artifact_name,
                    None,
                )
                .await
                .expect("pointer resolution")
                .is_none(),
            "without the binding the candidate must stay invisible"
        );

        // Confined: an artifact this overlay never pinned resolves identically with
        // and without the binding, so an evaluation cannot silently substitute a
        // dependency the submitter did not enumerate.
        let unpinned_name = unpinned.document.metadata.name.clone();
        assert!(
            fixture
                .registry
                .load_serving_with_overlay(
                    &fixture.scope,
                    ArtifactKind::Skill,
                    &unpinned_name,
                    Some(&binding),
                )
                .await
                .expect("unpinned resolution")
                .is_none(),
            "an unpinned artifact must fall through to the serving pointer"
        );

        // A wrong secret is refused by the SQL guard, so the caller falls back
        // rather than being handed the candidate.
        let forged = moa_artifacts::release::EvalOverlayBinding {
            overlay_uid: arm.overlay_uid,
            overlay_token: "not-the-secret".to_string(),
            eval_session_id: arm.eval_session_id,
        };
        assert!(
            fixture
                .registry
                .load_serving_with_overlay(
                    &fixture.scope,
                    ArtifactKind::Skill,
                    &fixture.artifact_name,
                    Some(&forged),
                )
                .await
                .expect("forged resolution")
                .is_none(),
            "a forged overlay token must not resolve the candidate"
        );
    }
}

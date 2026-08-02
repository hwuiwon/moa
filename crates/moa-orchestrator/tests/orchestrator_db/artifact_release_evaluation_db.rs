//! Postgres-backed coverage for release-candidate evaluation dispatch.
//!
//! What is pinned here is the part `V000045` left open: a submission now carries a
//! durable dispatch record, ten rapid submissions still coalesce to one open
//! dispatch, a result for a superseded generation cannot make a revision ready, the
//! evaluation overlay is unreachable from normal session resolution, and the hidden
//! release cohort rotates and runs out.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, CandidateSubjectInputs, NewArtifactDraft, NewArtifactFile, RecordDecision,
    StoredArtifactRevision, SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationTarget, ActivationTargetClass, AssertionRef, DeterminismClass, DeterministicVerdict,
    EvidenceAdapter, PLATFORM_BLOCKING_ASSERTIONS, PLATFORM_RELEASE_PLAN_REVISION_UID, ReleaseSlot,
    ReleaseState, TenantScope,
};
use moa_artifacts::test_fixtures::fixture_subject_inputs;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use moa_orchestrator::workflows::artifact_release_evaluation::Error as ReleaseEvaluationError;
use moa_orchestrator::workflows::artifact_release_evaluation::dispatch::build_paired_run_request;
use moa_orchestrator::workflows::artifact_release_evaluation::repository::ReleaseEvaluationRepository;
use moa_orchestrator::workflows::artifact_release_evaluation::types::{
    ArmRole, AttemptReviewState, DispatchStatus, MAX_PINNED_DEPENDENCIES, PinnedDependency,
    dispatch_idempotency_key, release_seed_material,
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

    /// Seeds a tenant whose release subjects resolve the platform-owned plan.
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
                    version: "v1".to_string(),
                    determinism: DeterminismClass::Deterministic,
                })
                .collect(),
            evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
            decided_by: "admin".to_string(),
        }
    }

    async fn insert_historical_case_pack(
        pool: &PgPool,
        name: &str,
        cases: serde_json::Value,
        scenario_source: serde_json::Value,
    ) -> std::result::Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        let mandatory = json!(["target_completed"]);
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_release_case_pack (
                pack_uid, storage_partition_id, user_id, name, revision, target_class,
                visibility, cohort_epoch, plan_revision_uid, cases,
                mandatory_assertions, scenario_source, pack_hash, valid_to
            )
            VALUES (
                $1, NULL, NULL, $2, 1, 'action_visibility', 'authoring', 1, $3,
                $4, $5, $6,
                moa.artifact_release_case_pack_content_hash(
                    $2, 1, 'action_visibility', 'authoring', 1,
                    NULL, NULL, NULL, $3, $4, $5, $6
                ),
                now()
            )
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(name)
        .bind(PLATFORM_RELEASE_PLAN_REVISION_UID)
        .bind(cases)
        .bind(mandatory)
        .bind(scenario_source)
        .execute(pool)
        .await
    }

    #[tokio::test]
    async fn platform_plan_stores_its_exact_canonical_document_hash_db() {
        // Pins: the release subject hashes the immutable executable ArtifactDocument,
        // not a migration label that could stay unchanged while plan bytes drift.
        let (_db, fixture) = fixture("platform-plan-hash").await;
        let revision = fixture
            .registry
            .load_revision(&fixture.scope, PLATFORM_RELEASE_PLAN_REVISION_UID)
            .await
            .expect("load platform release plan")
            .expect("platform release plan exists");
        let expected = moa_artifacts::canonical::canonical_hash(&revision.document)
            .expect("platform plan document canonicalizes");
        assert_eq!(revision.canonical_hash.as_slice(), expected.as_slice());

        let mut tenant_conn = moa_db::ScopedConn::begin(
            &fixture.pool,
            &moa_core::types::memory::RlsContext::tenant(fixture.tenant_id),
        )
        .await
        .expect("begin tenant-scoped artifact transaction");
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(tenant_conn.as_mut())
            .await
            .expect("apply tenant application role");
        let error = sqlx::query(
            "UPDATE moa.artifact SET description = 'tenant rewrite' WHERE artifact_uid = $1",
        )
        .bind(revision.artifact_uid)
        .execute(tenant_conn.as_mut())
        .await
        .expect_err("tenant role must not modify a global artifact");
        assert!(
            matches!(&error, sqlx::Error::Database(error) if error.code().as_deref() == Some("42501")),
            "unexpected global artifact write rejection: {error}"
        );
        tenant_conn
            .rollback()
            .await
            .expect("rollback rejected tenant write");
    }

    #[tokio::test]
    async fn platform_case_pack_authority_is_hashed_immutable_and_verified_db() {
        // Pins: the case-pack hash covers executable authority, the same revision
        // cannot be rewritten in place, and repository resolution independently
        // refuses owner-level corruption before an experiment is provisioned.
        let (_db, fixture) = fixture("case-pack-integrity").await;
        let hashes_match: bool = sqlx::query_scalar(
            r#"
            SELECT bool_and(
                pack_hash = moa.artifact_release_case_pack_content_hash(
                    name,
                    revision,
                    target_class,
                    visibility,
                    cohort_epoch,
                    cohort_size,
                    rotates_at,
                    max_attempts_per_epoch,
                    plan_revision_uid,
                    cases,
                    mandatory_assertions,
                    scenario_source
                )
            )
            FROM moa.artifact_release_case_pack
            WHERE storage_partition_id IS NULL
            "#,
        )
        .fetch_one(&fixture.pool)
        .await
        .expect("verify seeded case-pack digests");
        assert!(
            hashes_match,
            "every platform case-pack hash must digest its exact executable authority"
        );

        let pack_uid: Uuid = sqlx::query_scalar(
            "SELECT pack_uid FROM moa.artifact_release_case_pack \
             WHERE target_class = 'skill_visibility' AND visibility = 'authoring' \
               AND valid_to IS NULL",
        )
        .fetch_one(&fixture.pool)
        .await
        .expect("load active authoring case pack");
        let rewrite = sqlx::query(
            r#"
            UPDATE moa.artifact_release_case_pack
            SET name = 'rewritten',
                pack_hash = moa.artifact_release_case_pack_content_hash(
                    'rewritten',
                    revision,
                    target_class,
                    visibility,
                    cohort_epoch,
                    cohort_size,
                    rotates_at,
                    max_attempts_per_epoch,
                    plan_revision_uid,
                    cases,
                    mandatory_assertions,
                    scenario_source
                )
            WHERE pack_uid = $1
            "#,
        )
        .bind(pack_uid)
        .execute(&fixture.pool)
        .await;
        assert!(
            rewrite.is_err(),
            "a promoter cannot rewrite executable pack authority in place"
        );

        sqlx::query(
            "ALTER TABLE moa.artifact_release_case_pack DISABLE TRIGGER artifact_release_case_pack_immutable",
        )
        .execute(&fixture.pool)
        .await
        .expect("disable immutability only to simulate owner-level corruption");
        sqlx::query(
            "UPDATE moa.artifact_release_case_pack SET name = 'tampered' WHERE pack_uid = $1",
        )
        .bind(pack_uid)
        .execute(&fixture.pool)
        .await
        .expect("simulate corrupted case-pack authority");
        sqlx::query(
            "ALTER TABLE moa.artifact_release_case_pack ENABLE TRIGGER artifact_release_case_pack_immutable",
        )
        .execute(&fixture.pool)
        .await
        .expect("restore case-pack immutability");

        let draft = fixture.draft("candidate").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit candidate before case-plan resolution");
        let record = submitted.dispatch.expect("candidate dispatch record");
        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim dispatch")
            .expect("dispatch is claimable");
        let error = fixture
            .repository
            .provision_attempt(
                &claimed.record,
                &[(ArmRole::Candidate, "candidate-secret".to_string())],
            )
            .await
            .expect_err("a corrupted pack cannot construct a release subject");
        assert!(
            matches!(error, ReleaseEvaluationError::CasePackInvalid(_)),
            "unexpected corrupted pack error: {error}"
        );
    }

    #[tokio::test]
    async fn hidden_cohort_rotation_inserts_a_canonical_immutable_revision_db() {
        // Pins: rotation is insert-new-revision, closes the old cohort, and hashes
        // the exact new epoch and deadline instead of a human-readable label.
        let (_db, fixture) = fixture("case-pack-rotation").await;
        let (old_uid, old_revision, old_epoch, rotates_at): (
            Uuid,
            i32,
            i32,
            chrono::DateTime<chrono::Utc>,
        ) = sqlx::query_as(
            "SELECT pack_uid, revision, cohort_epoch, rotates_at \
             FROM moa.artifact_release_case_pack \
             WHERE target_class = 'skill_visibility' AND visibility = 'hidden' \
               AND valid_to IS NULL",
        )
        .fetch_one(&fixture.pool)
        .await
        .expect("load current hidden cohort");
        let rotation_time = rotates_at + chrono::Duration::seconds(1);
        let next_uid: Uuid = sqlx::query_scalar(
            "SELECT moa.rotate_release_hidden_cohort($1, $2, INTERVAL '7 days')",
        )
        .bind(ActivationTargetClass::SkillVisibility.as_str())
        .bind(rotation_time)
        .fetch_one(&fixture.pool)
        .await
        .expect("rotate hidden cohort");
        assert_ne!(next_uid, old_uid, "rotation must insert a new pack row");

        let old_valid_to: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT valid_to FROM moa.artifact_release_case_pack WHERE pack_uid = $1",
        )
        .bind(old_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load closed prior cohort");
        assert_eq!(old_valid_to, rotation_time);
        let reopened = sqlx::query(
            "UPDATE moa.artifact_release_case_pack SET valid_to = NULL WHERE pack_uid = $1",
        )
        .bind(old_uid)
        .execute(&fixture.pool)
        .await;
        assert!(
            reopened.is_err(),
            "a closed cohort revision cannot be reopened in place"
        );

        let (revision, epoch, hash_matches): (i32, i32, bool) = sqlx::query_as(
            r#"
            SELECT
                revision,
                cohort_epoch,
                pack_hash = moa.artifact_release_case_pack_content_hash(
                    name,
                    revision,
                    target_class,
                    visibility,
                    cohort_epoch,
                    cohort_size,
                    rotates_at,
                    max_attempts_per_epoch,
                    plan_revision_uid,
                    cases,
                    mandatory_assertions,
                    scenario_source
                )
            FROM moa.artifact_release_case_pack
            WHERE pack_uid = $1
            "#,
        )
        .bind(next_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load rotated cohort");
        assert_eq!(revision, old_revision + 1);
        assert_eq!(epoch, old_epoch + 1);
        assert!(hash_matches, "rotation must hash the exact new cohort row");

        let rewrite = sqlx::query(
            "UPDATE moa.artifact_release_case_pack SET cohort_epoch = cohort_epoch + 1 WHERE pack_uid = $1",
        )
        .bind(next_uid)
        .execute(&fixture.pool)
        .await;
        assert!(rewrite.is_err(), "a rotated revision is immutable too");
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
    async fn multi_pin_submission_is_canonical_and_provision_replay_is_stable_db() {
        // Pins: unordered exact duplicate pins are stored once in canonical order,
        // validated together, and replay into the same provisioned attempt.
        let (_db, fixture) = fixture("multi-pin-canonical").await;
        let first_dependency = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &format!("first-dependency-{}", Uuid::now_v7()),
            "first pinned draft dependency",
        )
        .await;
        let second_dependency = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &format!("second-dependency-{}", Uuid::now_v7()),
            "second pinned draft dependency",
        )
        .await;
        let first_pin = PinnedDependency {
            artifact_uid: first_dependency.artifact_uid,
            revision_uid: first_dependency.revision_uid,
        };
        let second_pin = PinnedDependency {
            artifact_uid: second_dependency.artifact_uid,
            revision_uid: second_dependency.revision_uid,
        };
        let mut expected = vec![first_pin, second_pin];
        expected.sort_unstable_by_key(|pin| (pin.artifact_uid, pin.revision_uid));
        let candidate = fixture.draft("candidate").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(candidate.revision_uid, candidate.artifact_uid),
                vec![second_pin, first_pin, second_pin],
            )
            .await
            .expect("submit canonical multi-pin release");
        let record = submitted.dispatch.expect("active release dispatches");
        assert_eq!(
            record.pinned_dependencies, expected,
            "dispatch persistence must use the canonical pin set"
        );

        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim canonical dispatch")
            .expect("dispatch is claimable");
        let tokens = [
            (ArmRole::Candidate, "candidate-secret".to_string()),
            (ArmRole::Baseline, "baseline-secret".to_string()),
        ];
        let provisioned = fixture
            .repository
            .provision_attempt(&claimed.record, &tokens)
            .await
            .expect("provision canonical pins");
        let replayed = fixture
            .repository
            .provision_attempt(&claimed.record, &tokens)
            .await
            .expect("replay canonical pin provisioning");
        assert_eq!(
            replayed, provisioned,
            "provision replay must preserve the canonical pinned dependency set"
        );
    }

    #[tokio::test]
    async fn an_unowned_pinned_dependency_is_refused_db() {
        // Pins: the evaluation overlay can only pin revisions the submitting tenant
        // owns, so a submitter cannot make evaluation resolve someone else's draft.
        let (_db, fixture) = fixture("pinned-dependency").await;
        let draft = fixture.draft("rev-1").await;
        let mut invalid = vec![
            PinnedDependency {
                artifact_uid: Uuid::from_u128(0xeeee_0000_0000_0000_0000_0000_0000_0002),
                revision_uid: Uuid::from_u128(0xffff_0000_0000_0000_0000_0000_0000_0002),
            },
            PinnedDependency {
                artifact_uid: Uuid::from_u128(0xeeee_0000_0000_0000_0000_0000_0000_0001),
                revision_uid: Uuid::from_u128(0xffff_0000_0000_0000_0000_0000_0000_0001),
            },
        ];
        let error = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(draft.revision_uid, draft.artifact_uid),
                invalid.clone(),
            )
            .await
            .expect_err("an unowned pin must be refused");
        invalid.sort_unstable_by_key(|pin| (pin.artifact_uid, pin.revision_uid));
        let expected_missing = invalid
            .iter()
            .map(|pin| format!("{}:{}", pin.artifact_uid, pin.revision_uid))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(matches!(
            &error,
            ReleaseEvaluationError::PinnedDependencyInvalid(detail)
                if detail == &format!(
                    "pinned dependencies are not live tenant-scoped artifact/revision pairs: [{expected_missing}]"
                )
        ));
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
    async fn conflicting_duplicate_artifact_pins_are_refused_before_release_dml_db() {
        // Pins: one artifact cannot resolve to two revisions based on caller order,
        // and the shape error happens before candidate or dispatch persistence.
        let (_db, fixture) = fixture("conflicting-pin").await;
        let dependency_name = format!("conflicting-dependency-{}", Uuid::now_v7());
        let first = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &dependency_name,
            "first dependency revision",
        )
        .await;
        let second = draft_skill(
            &fixture.registry,
            &fixture.scope,
            &dependency_name,
            "second dependency revision",
        )
        .await;
        assert_eq!(first.artifact_uid, second.artifact_uid);
        let candidate = fixture.draft("candidate").await;
        let error = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(candidate.revision_uid, candidate.artifact_uid),
                vec![
                    PinnedDependency {
                        artifact_uid: first.artifact_uid,
                        revision_uid: second.revision_uid,
                    },
                    PinnedDependency {
                        artifact_uid: first.artifact_uid,
                        revision_uid: first.revision_uid,
                    },
                ],
            )
            .await
            .expect_err("conflicting artifact pins must be refused");
        let mut revisions = [first.revision_uid, second.revision_uid];
        revisions.sort_unstable();
        let expected_conflict = format!(
            "artifact {} is pinned to conflicting revisions {} and {}",
            first.artifact_uid, revisions[0], revisions[1]
        );
        assert!(
            matches!(&error, ReleaseEvaluationError::PinnedDependencyInvalid(detail) if detail == &expected_conflict),
            "unexpected conflict error: {error}"
        );
        let release_rows: (i64, i64) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) FROM moa.artifact_release_candidate WHERE revision_uid = $1),
                (SELECT count(*) FROM moa.artifact_release_dispatch_outbox WHERE revision_uid = $1)
            "#,
        )
        .bind(candidate.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count release rows after conflicting pins");
        assert_eq!(release_rows, (0, 0));
    }

    #[tokio::test]
    async fn over_cap_pins_are_refused_before_release_dml_db() {
        // Pins: the dispatch payload and validation relation never accept more
        // than the documented maximum, even before ownership is considered.
        let (_db, fixture) = fixture("over-cap-pin").await;
        let candidate = fixture.draft("candidate").await;
        let pins = (0..=MAX_PINNED_DEPENDENCIES)
            .map(|_| PinnedDependency {
                artifact_uid: Uuid::now_v7(),
                revision_uid: Uuid::now_v7(),
            })
            .collect::<Vec<_>>();
        let error = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(candidate.revision_uid, candidate.artifact_uid),
                pins,
            )
            .await
            .expect_err("an over-cap pin collection must be refused");
        assert!(matches!(
            &error,
            ReleaseEvaluationError::PinnedDependencyInvalid(detail)
                if detail == &format!(
                    "a release may pin at most {MAX_PINNED_DEPENDENCIES} dependencies; received {}",
                    MAX_PINNED_DEPENDENCIES + 1
                )
        ));
        let release_rows: (i64, i64) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) FROM moa.artifact_release_candidate WHERE revision_uid = $1),
                (SELECT count(*) FROM moa.artifact_release_dispatch_outbox WHERE revision_uid = $1)
            "#,
        )
        .bind(candidate.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count release rows after over-cap pins");
        assert_eq!(release_rows, (0, 0));
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
    async fn terminal_provisioning_failure_releases_the_active_slot_db() {
        // Pins: a terminal refusal before Experiments/run is admitted must close
        // the claimed attempt as inconclusive and advance the newest pending
        // subject instead of stranding the artifact's single active slot.
        let (_db, fixture) = fixture("terminal-provisioning-failure").await;
        let active = fixture.draft("active").await;
        let submitted = fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(active.revision_uid, active.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit active candidate");
        let record = submitted.dispatch.expect("active dispatch record");
        let pending = fixture.draft("pending").await;
        fixture
            .repository
            .submit_with_dispatch(
                fixture.submit(pending.revision_uid, pending.artifact_uid),
                Vec::new(),
            )
            .await
            .expect("submit pending candidate");
        let claimed = fixture
            .repository
            .claim_dispatch(fixture.tenant_id, record.outbox_uid)
            .await
            .expect("claim active dispatch")
            .expect("active dispatch is claimable");

        let failure = fixture
            .repository
            .provision_attempt(&claimed.record, &[])
            .await
            .expect_err("missing candidate overlay token is terminal");
        let failure_detail = failure.to_string();
        let settled = fixture
            .repository
            .settle_terminal_failure(
                fixture.tenant_id,
                record.outbox_uid,
                "provision",
                &failure_detail,
            )
            .await
            .expect("terminal provisioning failure settles the release attempt");
        assert_eq!(
            settled.next.as_ref().map(|next| next.revision_uid),
            Some(pending.revision_uid)
        );

        let failed_dispatch: (String, bool) = sqlx::query_as(
            "SELECT status, settled_at IS NOT NULL FROM moa.artifact_release_dispatch_outbox WHERE outbox_uid = $1",
        )
        .bind(record.outbox_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load failed dispatch status");
        assert_eq!(
            failed_dispatch,
            (DispatchStatus::Abandoned.as_str().to_string(), true),
            "the failed dispatch must be terminal"
        );

        let attempt: (String, Option<Uuid>, Option<Uuid>, serde_json::Value) = sqlx::query_as(
            r#"
            SELECT verdict, attestation_uid, candidate_run_uid, verdict_detail
            FROM moa.artifact_release_attempt
            WHERE attempt_uid = $1
            "#,
        )
        .bind(settled.attempt_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load terminally failed attempt");
        assert_eq!(attempt.0, DeterministicVerdict::Inconclusive.as_str());
        assert_eq!(attempt.1, None, "terminal failure must never attest");
        assert_eq!(attempt.2, None, "no experiment run was admitted");
        assert_eq!(
            attempt.3,
            json!({
                "terminal_failure": {
                    "phase": "provision",
                    "error": failure_detail,
                }
            }),
            "the review surface preserves stable phase and error detail"
        );

        let active_state: (String, String, Option<Uuid>, Option<String>) = sqlx::query_as(
            r#"
            SELECT r.status, c.slot, c.last_run_uid, c.last_decision
            FROM moa.artifact_release_candidate c
            JOIN moa.artifact_revision r ON r.revision_uid = c.revision_uid
            WHERE c.revision_uid = $1
            "#,
        )
        .bind(active.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load failed candidate state");
        assert_eq!(
            active_state,
            (
                ReleaseState::Inconclusive.as_str().to_string(),
                ReleaseSlot::Released.as_str().to_string(),
                None,
                Some(DeterministicVerdict::Inconclusive.as_str().to_string()),
            ),
            "the failed candidate must be retryable and carry no fabricated run"
        );

        let pending_state: (String, String) = sqlx::query_as(
            r#"
            SELECT c.slot, r.status
            FROM moa.artifact_release_candidate c
            JOIN moa.artifact_revision r ON r.revision_uid = c.revision_uid
            WHERE c.revision_uid = $1
            "#,
        )
        .bind(pending.revision_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("load pending candidate state");
        assert_eq!(
            pending_state,
            (
                ReleaseSlot::Active.as_str().to_string(),
                ReleaseState::Evaluating.as_str().to_string(),
            )
        );

        let replayed = fixture
            .repository
            .settle_terminal_failure(
                fixture.tenant_id,
                record.outbox_uid,
                "provision",
                "a replay must preserve the first committed detail",
            )
            .await
            .expect("terminal settlement is idempotent");
        assert_eq!(replayed, settled);
        let replayed_detail: serde_json::Value = sqlx::query_scalar(
            "SELECT verdict_detail FROM moa.artifact_release_attempt WHERE attempt_uid = $1",
        )
        .bind(settled.attempt_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("reload terminal failure detail");
        assert_eq!(replayed_detail, attempt.3, "first failure detail is stable");
        let dispatch_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_release_dispatch_outbox WHERE artifact_uid = $1",
        )
        .bind(active.artifact_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count dispatch records after settlement replay");
        assert_eq!(
            dispatch_count, 2,
            "settlement replay must not enqueue twice"
        );
    }

    #[tokio::test]
    async fn post_claim_terminal_failures_release_the_active_slot_without_attestation_db() {
        // Pins: terminal status-poll and decision failures happen after an
        // experiment identity is recorded. Both must preserve that diagnostic
        // identity on the attempt while releasing the candidate and advancing
        // pending work without minting an attestation.
        for phase in ["experiments_status", "release_decision"] {
            let (_db, fixture) = fixture(&format!("terminal-{phase}")).await;
            let active = fixture.draft("active").await;
            let submitted = fixture
                .repository
                .submit_with_dispatch(
                    fixture.submit(active.revision_uid, active.artifact_uid),
                    Vec::new(),
                )
                .await
                .expect("submit active candidate");
            let record = submitted.dispatch.expect("active dispatch record");
            let pending = fixture.draft("pending").await;
            fixture
                .repository
                .submit_with_dispatch(
                    fixture.submit(pending.revision_uid, pending.artifact_uid),
                    Vec::new(),
                )
                .await
                .expect("submit pending candidate");
            let claimed = fixture
                .repository
                .claim_dispatch(fixture.tenant_id, record.outbox_uid)
                .await
                .expect("claim active dispatch")
                .expect("active dispatch is claimable");
            fixture
                .repository
                .provision_attempt(
                    &claimed.record,
                    &[(ArmRole::Candidate, "candidate-secret".to_string())],
                )
                .await
                .expect("provision release attempt");
            let run_uid = Uuid::now_v7();
            fixture
                .repository
                .record_dispatched_runs(fixture.tenant_id, record.outbox_uid, run_uid, None)
                .await
                .expect("record admitted experiment identity");

            let settled = fixture
                .repository
                .settle_terminal_failure(
                    fixture.tenant_id,
                    record.outbox_uid,
                    phase,
                    "terminal downstream refusal",
                )
                .await
                .expect("post-claim terminal failure settles the release attempt");
            assert_eq!(
                settled.next.as_ref().map(|next| next.revision_uid),
                Some(pending.revision_uid),
                "{phase} failure must advance the newest pending candidate"
            );

            let attempt: (String, Option<Uuid>, Option<Uuid>, serde_json::Value) = sqlx::query_as(
                r#"
                    SELECT verdict, attestation_uid, candidate_run_uid, verdict_detail
                    FROM moa.artifact_release_attempt
                    WHERE attempt_uid = $1
                    "#,
            )
            .bind(settled.attempt_uid)
            .fetch_one(&fixture.pool)
            .await
            .expect("load terminal release attempt");
            assert_eq!(attempt.0, DeterministicVerdict::Inconclusive.as_str());
            assert_eq!(attempt.1, None, "{phase} failure must never attest");
            assert_eq!(
                attempt.2,
                Some(run_uid),
                "the admitted run remains available for diagnosis"
            );
            assert_eq!(attempt.3["terminal_failure"]["phase"], json!(phase));
            assert_eq!(
                attempt.3["terminal_failure"]["error"],
                json!("terminal downstream refusal")
            );
            assert!(
                attempt.3.get("case_plan").is_some(),
                "terminal settlement must preserve provisioned diagnostic metadata"
            );

            let candidate: (String, String, Option<Uuid>) = sqlx::query_as(
                r#"
                SELECT r.status, c.slot, c.last_run_uid
                FROM moa.artifact_release_candidate c
                JOIN moa.artifact_revision r ON r.revision_uid = c.revision_uid
                WHERE c.revision_uid = $1
                "#,
            )
            .bind(active.revision_uid)
            .fetch_one(&fixture.pool)
            .await
            .expect("load terminal candidate state");
            assert_eq!(
                candidate,
                (
                    ReleaseState::Inconclusive.as_str().to_string(),
                    ReleaseSlot::Released.as_str().to_string(),
                    None,
                )
            );
        }
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
        let baseline = fixture.draft("serving baseline").await;
        moa_artifacts::test_fixtures::activate_revision(
            &fixture.pool,
            fixture.release_scope,
            fixture.target(baseline.artifact_uid),
            baseline.revision_uid,
        )
        .await
        .expect("activate serving baseline");
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
                &[
                    (ArmRole::Candidate, "candidate-secret".to_string()),
                    (ArmRole::Baseline, "baseline-secret".to_string()),
                ],
            )
            .await
            .expect("provision attempt");
        let replayed = fixture
            .repository
            .provision_attempt(
                &claimed.record,
                &[
                    (ArmRole::Candidate, "candidate-secret".to_string()),
                    (ArmRole::Baseline, "baseline-secret".to_string()),
                ],
            )
            .await
            .expect("replay provision attempt");
        assert_eq!(
            replayed, attempt,
            "a replay must reuse every trial overlay, session, fixture, and token"
        );
        let overlay_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.artifact_release_eval_overlay WHERE outbox_uid = $1",
        )
        .bind(claimed.record.outbox_uid)
        .fetch_one(&fixture.pool)
        .await
        .expect("count provisioned overlays after replay");
        assert_eq!(
            overlay_count, 24,
            "replay must not create duplicate per-trial overlays"
        );
        let candidate_trials = attempt
            .trials
            .iter()
            .filter(|trial| trial.role == ArmRole::Candidate)
            .collect::<Vec<_>>();
        let baseline_trials = attempt
            .trials
            .iter()
            .filter(|trial| trial.role == ArmRole::Baseline)
            .collect::<Vec<_>>();
        assert_eq!(candidate_trials.len(), 12);
        assert_eq!(baseline_trials.len(), 12);
        assert_eq!(attempt.trials.len(), 24);
        for (label, distinct) in [
            (
                "trial keys",
                attempt
                    .trials
                    .iter()
                    .map(|trial| trial.trial_key.clone())
                    .collect::<BTreeSet<_>>(),
            ),
            (
                "eval sessions",
                attempt
                    .trials
                    .iter()
                    .map(|trial| trial.eval_session_id.to_string())
                    .collect::<BTreeSet<_>>(),
            ),
        ] {
            assert_eq!(
                distinct.len(),
                attempt.trials.len(),
                "every exact arm/case/repetition needs a distinct {label}"
            );
        }
        let arm = candidate_trials[0];
        let baseline_arm = baseline_trials[0];
        assert_eq!(
            baseline_arm.revision_uid, baseline.revision_uid,
            "the baseline overlay must come from the fenced release subject, not caller input"
        );

        let run_request = build_paired_run_request(fixture.tenant_id, &claimed.record, &attempt)
            .expect("build release run request");
        let binding = run_request
            .release_evaluation
            .expect("release run carries its durable binding");
        assert_eq!(binding.trials.len(), 24);
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
        forged.trials[0].arm.eval_session_id = Uuid::now_v7();
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
        forged.trials.pop();
        let error = fixture
            .repository
            .validate_experiment_binding(
                fixture.tenant_id,
                attempt.plan.plan_revision_uid,
                &claimed.record.idempotency_key,
                &forged,
            )
            .await
            .expect_err("a release run cannot omit one approved trial");
        assert!(matches!(
            error,
            ReleaseEvaluationError::ExperimentBindingInvalid(_)
        ));

        assert_ne!(arm.eval_session_id, baseline_arm.eval_session_id);
        // The candidate overlay resolves the draft; the normal resolver remains
        // pinned to the serving baseline captured in the release subject.
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
        let normal = fixture
            .registry
            .load_serving(&fixture.scope, ArtifactKind::Skill, &fixture.artifact_name)
            .await
            .expect("serving resolution")
            .expect("serving baseline");
        assert_eq!(
            normal.revision_uid, baseline.revision_uid,
            "an overlay must not replace the revision visible to a normal session"
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
        let transcript = insert_historical_case_pack(
            &fixture.pool,
            "transcript-pack",
            json!([{"case_id":"c","transcript":[{"role":"user"}]}]),
            json!({"kind":"approved_pack"}),
        )
        .await;
        assert!(
            transcript.is_err(),
            "a case body carrying a transcript must be unrepresentable"
        );

        let cases = json!([{
            "case_id":"c",
            "persona_ref":"p",
            "profile":"default",
            "repetitions":1,
            "assertions":[]
        }]);
        let unerasable = insert_historical_case_pack(
            &fixture.pool,
            "unerasable-learned-pack",
            cases.clone(),
            json!({
                "kind":"learned",
                "evidence":{
                    "contribution_uid":"00000000-0000-4000-8000-000000000001",
                    "retention_class":"short",
                    "consent_basis":"contract"
                }
            }),
        )
        .await;
        assert!(
            unerasable.is_err(),
            "learned scenario input without erasure provenance must be unrepresentable"
        );

        let complete = insert_historical_case_pack(
            &fixture.pool,
            "erasable-learned-pack",
            cases,
            json!({
                "kind":"learned",
                "evidence":{
                    "contribution_uid":"00000000-0000-4000-8000-000000000001",
                    "retention_class":"short",
                    "consent_basis":"contract",
                    "erasure_provenance":"moa.artifact_revision_contribution"
                }
            }),
        )
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
                &[(ArmRole::Candidate, "candidate-secret".to_string())],
            )
            .await
            .expect("provision attempt");
        assert_eq!(
            attempt.trials.len(),
            12,
            "a first activation provisions one candidate trial for every approved repetition"
        );
        assert!(
            attempt
                .trials
                .iter()
                .all(|trial| trial.role == ArmRole::Candidate),
            "a first activation must not fabricate baseline overlays"
        );
        let run_uid = Uuid::now_v7();
        fixture
            .repository
            .record_dispatched_runs(fixture.tenant_id, record.outbox_uid, run_uid, None)
            .await
            .expect("record candidate-only experiment run");
        let recorded = fixture
            .repository
            .list_attempts(fixture.tenant_id, 10)
            .await
            .expect("list release attempts")
            .into_iter()
            .find(|row| row.attempt_uid == attempt.attempt_uid)
            .expect("recorded release attempt");
        assert_eq!(recorded.candidate_run_uid, Some(run_uid));
        assert_eq!(
            recorded.baseline_run_uid, None,
            "a first activation must not relabel the candidate run as a baseline"
        );
        let arm = attempt
            .trials
            .iter()
            .find(|trial| trial.role == ArmRole::Candidate)
            .expect("candidate trial");
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

//! Learning-review service tests for skill draft acceptance and rejection.

#![recursion_limit = "256"]

use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_core::wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
};
use moa_core::{
    config::MoaConfig, error::MoaError, types::action_policy::ActionRuleScope,
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateStatusUpdate, types::experience::LearningCandidateType,
    types::experience::LearningRiskClass, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::learning::LearningEntry,
};
use moa_orchestrator::services::learning_review::{
    accept_skill_candidate_after_authz, get_learning_candidate_after_authz,
    reject_learning_candidate_after_authz,
};
use moa_session::PostgresSessionStore;
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::package::{SkillPackage, ValidatedSkillPackage};
use moa_skills::review::{
    AcceptanceChecks, LearningReviewStore, SkillReviewAction, SkillReviewRequest,
    prepare_skill_acceptance, promote_claimed_skill_candidate,
};
use moa_test_support::fixtures::tenant_id_from_storage_partition_id;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use uuid::Uuid;

mod skill_learning_review {
    use super::*;

    #[tokio::test]
    async fn promote_claimed_skill_candidate_publishes_draft_and_appends_learning() {
        // Pins: promoting a claimed skill candidate publishes its draft artifact for
        // artifact-backed skill loading, records the review evaluation payload, and appends
        // one promoted learning entry. The regression gate itself is pinned separately;
        // this drives the promote path with explicit passing acceptance checks.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-accept");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-accept");
        let package = skill_package(&skill_name, "Review accepted draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;
        let store = Arc::new(test_db.store().clone());

        let loaded = get_learning_candidate_after_authz(
            store.clone(),
            GetLearningCandidateRequest {
                tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate_id: candidate.id,
            },
        )
        .await
        .expect("load review candidate");
        assert_eq!(loaded.id, candidate.id);
        assert_eq!(
            loaded.tenant_id,
            tenant_id_from_storage_partition_id(&storage_partition_id)
        );

        let request = SkillReviewRequest {
            tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
            candidate_id: candidate.id,
            action: SkillReviewAction::Accept,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("looks reusable".to_string()),
        };
        let review_store = PassthroughReviewStore {
            store: store.clone(),
        };
        let pool = store.pool().clone();
        let prepared = prepare_skill_acceptance(&review_store, pool.clone(), &request)
            .await
            .expect("prepare skill acceptance");
        let outcome = promote_claimed_skill_candidate(
            &review_store,
            pool,
            &request,
            prepared,
            json!({"regression_execution": "completed", "decision": "accepted"}),
            AcceptanceChecks {
                held_in_pass: true,
                held_in_description: "draft validated and suite executed".to_string(),
                held_out_pass: true,
                held_out_description: "candidate suite showed no regression".to_string(),
            },
        )
        .await
        .expect("promote skill candidate");

        assert_eq!(outcome.status, LearningCandidateStatus::Promoted);
        assert_eq!(outcome.artifact_uid, Some(draft.artifact_uid));
        assert_eq!(
            outcome.draft_artifact_revision_uid,
            Some(draft.revision_uid)
        );
        assert_eq!(
            outcome.published_artifact_revision_uid,
            Some(draft.revision_uid)
        );

        let published = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load published artifact")
            .expect("published artifact exists");
        assert_eq!(published.kind, ArtifactKind::Skill);
        assert_eq!(published.status, ArtifactStatus::Published);

        let promoted = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload promoted candidate")
            .expect("promoted candidate exists");
        assert_eq!(promoted.status, LearningCandidateStatus::Promoted);
        let evaluation = promoted
            .evaluation_payload
            .expect("promotion evaluation payload");
        assert_eq!(evaluation["reviewer_subject"], "user:reviewer");
        assert_eq!(evaluation["action"], "accept");
        assert_eq!(evaluation["reason"], "looks reusable");
        assert_eq!(
            evaluation["published_artifact_revision_uid"],
            draft.revision_uid.to_string()
        );
        assert!(evaluation.get("skill_uid").is_none());
        assert_eq!(evaluation["regression_execution"], "completed");
        assert_eq!(
            evaluation["acceptance_checks"]["held_out_pass"],
            serde_json::Value::Bool(true)
        );

        let learnings = test_db
            .store()
            .list_learnings(storage_partition_id.as_str(), Some("skill_created"), 10)
            .await
            .expect("list accepted skill learning");
        assert_eq!(learnings.len(), 1);
        assert_eq!(
            learnings[0].target_label.as_deref(),
            Some(skill_name.as_str())
        );
        assert_eq!(learnings[0].actor, "review:user:reviewer");
        assert_eq!(
            learnings[0].payload["candidate_id"],
            candidate.id.to_string()
        );
    }

    #[tokio::test]
    async fn accept_terminally_rejects_candidate_without_generated_suite() {
        // Pins: a candidate missing its generated regression suite is a content defect —
        // accept fails closed by rejecting the candidate and preserving the draft artifact,
        // instead of promoting with an "unavailable" regression report.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-no-suite");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-no-suite");
        let package = skill_package(&skill_name, "Review suite-less draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate_with_suite(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
            None,
        )
        .await;
        let store = Arc::new(test_db.store().clone());

        let response = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_request(
                &storage_partition_id,
                candidate.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect("accept disposition for suite-less candidate");

        assert_eq!(response.status, LearningCandidateStatus::Rejected);
        assert_eq!(response.published_artifact_revision_uid, None);

        let rejected = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload rejected candidate")
            .expect("rejected candidate exists");
        assert_eq!(rejected.status, LearningCandidateStatus::Rejected);
        assert_eq!(
            rejected.status_reason.as_deref(),
            Some("candidate has no generated regression suite")
        );
        let evaluation = rejected
            .evaluation_payload
            .expect("rejection evaluation payload");
        assert_eq!(
            evaluation["regression_report"]["reason"],
            "candidate has no generated regression suite"
        );

        let preserved = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load preserved draft")
            .expect("draft artifact remains visible");
        assert_eq!(preserved.status, ArtifactStatus::Draft);
        let learnings = test_db
            .store()
            .list_learnings(storage_partition_id.as_str(), Some("skill_created"), 10)
            .await
            .expect("list learning entries");
        assert!(learnings.is_empty(), "rejection must not append learning");
    }

    #[tokio::test]
    async fn accept_terminally_rejects_candidate_with_unparseable_suite() {
        // Pins: an LLM-generated suite that fails to parse blocks promotion as a content
        // defect instead of silently waiving the regression gate.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-bad-suite");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-bad-suite");
        let package = skill_package(&skill_name, "Review unparseable-suite draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate_with_suite(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
            Some(json!({
                "relative_path": "tests/suite.toml",
                "source_format": "toml",
                "source_text": "this is [not toml",
            })),
        )
        .await;
        let store = Arc::new(test_db.store().clone());

        let response = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_request(
                &storage_partition_id,
                candidate.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect("accept disposition for unparseable-suite candidate");

        assert_eq!(response.status, LearningCandidateStatus::Rejected);
        let rejected = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload rejected candidate")
            .expect("rejected candidate exists");
        assert_eq!(
            rejected.status_reason.as_deref(),
            Some("generated regression suite could not be parsed")
        );
        let preserved = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load preserved draft")
            .expect("draft artifact remains visible");
        assert_eq!(preserved.status, ArtifactStatus::Draft);
    }

    #[tokio::test]
    async fn accept_errors_when_review_provider_is_unavailable() {
        // Pins: an unavailable regression provider is an operational failure — accept
        // errors, releases the Evaluating claim back to Proposed so the accept can be
        // retried after the deployment is fixed, and never promotes with the gate waived.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-no-provider");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-no-provider");
        let package = skill_package(&skill_name, "Review provider-less draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;
        let store = Arc::new(test_db.store().clone());

        let error = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_request(
                &storage_partition_id,
                candidate.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect_err("empty provider registry must fail the accept request");
        assert!(
            format!("{error:?}").contains("provider"),
            "error should surface the provider failure: {error:?}"
        );

        let released = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload released candidate")
            .expect("released candidate exists");
        assert_eq!(
            released.status,
            LearningCandidateStatus::Proposed,
            "operational failure must release the claim so accept can be retried"
        );
        assert!(
            released
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("claim released")),
            "status reason records the operational release: {:?}",
            released.status_reason
        );
        let preserved = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load preserved draft")
            .expect("draft artifact remains visible");
        assert_eq!(preserved.status, ArtifactStatus::Draft);
    }

    #[tokio::test]
    async fn reject_skill_candidate_preserves_draft_without_publishing() {
        // Pins: rejecting a skill candidate marks it rejected but keeps the draft artifact for audit.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-reject");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-reject");
        let package = skill_package(&skill_name, "Review rejected draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;

        let response = reject_learning_candidate_after_authz(
            Arc::new(test_db.store().clone()),
            LearningCandidateReviewRequest {
                tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate_id: candidate.id,
                action: LearningCandidateReviewAction::Reject,
                reviewer_subject: "user:reviewer".to_string(),
                reason: Some("too narrow".to_string()),
            },
        )
        .await
        .expect("reject skill candidate");

        assert_eq!(response.status, LearningCandidateStatus::Rejected);
        assert_eq!(response.artifact_uid, Some(draft.artifact_uid));
        assert_eq!(
            response.draft_artifact_revision_uid,
            Some(draft.revision_uid)
        );
        assert_eq!(response.published_artifact_revision_uid, None);

        let preserved = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load preserved draft")
            .expect("draft artifact remains visible");
        assert_eq!(preserved.status, ArtifactStatus::Draft);

        let rejected = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload rejected candidate")
            .expect("rejected candidate exists");
        assert_eq!(rejected.status, LearningCandidateStatus::Rejected);
        let evaluation = rejected
            .evaluation_payload
            .expect("rejection evaluation payload");
        assert_eq!(evaluation["reviewer_subject"], "user:reviewer");
        assert_eq!(evaluation["action"], "reject");
        assert_eq!(evaluation["reason"], "too narrow");

        let learnings = test_db
            .store()
            .list_learnings(storage_partition_id.as_str(), Some("skill_created"), 10)
            .await
            .expect("list rejected skill learning");
        assert!(
            learnings.is_empty(),
            "reject must not append promoted learning"
        );
    }

    #[tokio::test]
    async fn promotion_rolls_back_artifact_publish_when_learning_append_fails() {
        // Pins: accept promotion publishes, promotes, and appends learning in one transaction.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-rollback");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-rollback");
        let package = skill_package(&skill_name, "Review rollback draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;
        let review_store = FailingLearningAppendStore {
            store: Arc::new(test_db.store().clone()),
        };
        let request = SkillReviewRequest {
            tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
            candidate_id: candidate.id,
            action: SkillReviewAction::Accept,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("exercise rollback".to_string()),
        };
        let pool = test_db.store().pool().clone();
        let prepared = prepare_skill_acceptance(&review_store, pool.clone(), &request)
            .await
            .expect("prepare skill acceptance");

        let error = promote_claimed_skill_candidate(
            &review_store,
            pool,
            &request,
            prepared,
            json!({"regression_execution": "skipped"}),
            AcceptanceChecks {
                held_in_pass: true,
                held_in_description: "held-in checks satisfied".to_string(),
                held_out_pass: true,
                held_out_description: "held-out checks satisfied".to_string(),
            },
        )
        .await
        .expect_err("injected learning append failure should abort promotion");
        assert!(
            error
                .to_string()
                .contains("injected learning append failure"),
            "error should preserve the append failure: {error}"
        );

        let artifact = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load draft after rollback")
            .expect("draft artifact remains");
        assert_eq!(
            artifact.status,
            ArtifactStatus::Draft,
            "failed promotion must roll back artifact publication"
        );
        let reloaded = test_db
            .store()
            .get_learning_candidate(
                &tenant_id_from_storage_partition_id(&storage_partition_id),
                candidate.id,
            )
            .await
            .expect("reload candidate")
            .expect("candidate remains");
        assert_eq!(
            reloaded.status,
            LearningCandidateStatus::Evaluating,
            "claiming the candidate happens before promotion gates, but final promotion rolls back"
        );
        let learnings = test_db
            .store()
            .list_learnings(storage_partition_id.as_str(), Some("skill_created"), 10)
            .await
            .expect("list learning entries");
        assert!(
            learnings.is_empty(),
            "failed promotion must not append promoted learning"
        );
    }

    #[tokio::test]
    async fn accept_refuses_non_skill_and_reject_handles_non_skill_candidate() {
        // Pins: accept stays skill-specific, while reject can disposition non-skill proposals.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-guard");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-guard");
        let package = skill_package(&skill_name, "Review guard draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let policy_candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Policy,
            LearningCandidateStatus::Proposed,
            "policy_updated",
            &skill_name,
            &draft,
        )
        .await;
        let non_proposed = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Rejected,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let pool = store.pool().clone();
        let config = review_config(&test_db);
        assert!(
            accept_skill_candidate_after_authz(
                store.clone(),
                pool.clone(),
                config.clone(),
                review_providers(),
                review_request(
                    &storage_partition_id,
                    policy_candidate.id,
                    LearningCandidateReviewAction::Accept,
                ),
            )
            .await
            .is_err(),
            "accept_skill must reject non-skill candidates"
        );
        let rejected_policy = reject_learning_candidate_after_authz(
            store.clone(),
            review_request(
                &storage_partition_id,
                policy_candidate.id,
                LearningCandidateReviewAction::Reject,
            ),
        )
        .await
        .expect("reject policy candidate");
        assert_eq!(rejected_policy.status, LearningCandidateStatus::Rejected);
        assert!(
            accept_skill_candidate_after_authz(
                store.clone(),
                pool,
                config,
                review_providers(),
                review_request(
                    &storage_partition_id,
                    non_proposed.id,
                    LearningCandidateReviewAction::Accept
                ),
            )
            .await
            .is_err(),
            "accept must reject non-proposed candidates"
        );
        assert!(
            reject_learning_candidate_after_authz(
                store,
                review_request(
                    &storage_partition_id,
                    non_proposed.id,
                    LearningCandidateReviewAction::Reject
                ),
            )
            .await
            .is_err(),
            "reject must reject non-proposed candidates"
        );
    }

    fn tenant_scope(storage_partition_id: &StoragePartitionId) -> ActionRuleScope {
        ActionRuleScope::Tenant {
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
        }
    }

    /// Delegates every review-store operation to the Postgres store unchanged.
    ///
    /// The promote-path tests drive `prepare_skill_acceptance` and
    /// `promote_claimed_skill_candidate` directly, so they need a working
    /// `LearningReviewStore` outside the private service wrapper.
    #[derive(Clone)]
    struct PassthroughReviewStore {
        store: Arc<PostgresSessionStore>,
    }

    impl LearningReviewStore for PassthroughReviewStore {
        async fn get_learning_candidate(
            &self,
            tenant_id: &TenantId,
            candidate_id: Uuid,
        ) -> std::result::Result<Option<LearningCandidate>, MoaError> {
            self.store
                .get_learning_candidate(tenant_id, candidate_id)
                .await
        }

        async fn update_learning_candidate_status_from(
            &self,
            update: &LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> std::result::Result<bool, MoaError> {
            self.store
                .update_learning_candidate_status_from(update, expected_status)
                .await
        }

        async fn update_learning_candidate_status_from_in_tx(
            &self,
            conn: &mut sqlx::PgConnection,
            update: &LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> std::result::Result<bool, MoaError> {
            self.store
                .update_learning_candidate_status_from_in_tx(conn, update, expected_status)
                .await
        }

        async fn append_learning_in_tx(
            &self,
            conn: &mut sqlx::PgConnection,
            entry: &LearningEntry,
        ) -> std::result::Result<(), MoaError> {
            self.store.append_learning_in_tx(conn, entry).await
        }
    }

    #[derive(Clone)]
    struct FailingLearningAppendStore {
        store: Arc<PostgresSessionStore>,
    }

    impl LearningReviewStore for FailingLearningAppendStore {
        async fn get_learning_candidate(
            &self,
            tenant_id: &TenantId,
            candidate_id: Uuid,
        ) -> std::result::Result<Option<LearningCandidate>, MoaError> {
            self.store
                .get_learning_candidate(tenant_id, candidate_id)
                .await
        }

        async fn update_learning_candidate_status_from(
            &self,
            update: &LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> std::result::Result<bool, MoaError> {
            self.store
                .update_learning_candidate_status_from(update, expected_status)
                .await
        }

        async fn update_learning_candidate_status_from_in_tx(
            &self,
            conn: &mut sqlx::PgConnection,
            update: &LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> std::result::Result<bool, MoaError> {
            self.store
                .update_learning_candidate_status_from_in_tx(conn, update, expected_status)
                .await
        }

        async fn append_learning_in_tx(
            &self,
            _conn: &mut sqlx::PgConnection,
            _entry: &LearningEntry,
        ) -> std::result::Result<(), MoaError> {
            Err(MoaError::StorageError(
                "injected learning append failure".to_string(),
            ))
        }
    }

    fn review_config(test_db: &moa_test_support::postgres::TestDb) -> Arc<MoaConfig> {
        let mut config = MoaConfig::default();
        config.database.url = test_db.database_url().to_string();
        config.query_rewrite.enabled = false;
        Arc::new(config)
    }

    fn review_providers() -> Arc<moa_providers::ProviderRegistry> {
        Arc::new(moa_providers::ProviderRegistry::default())
    }

    fn unique_workspace(_prefix: &str) -> StoragePartitionId {
        StoragePartitionId::new(Uuid::now_v7().to_string())
    }

    fn unique_skill_name(prefix: &str) -> String {
        format!("{prefix}-{}", short_uuid())
    }

    fn short_uuid() -> String {
        Uuid::now_v7().simple().to_string()
    }

    fn skill_package(skill_name: &str, description: &str) -> ValidatedSkillPackage {
        let markdown = format!(
            "---\n\
         name: {skill_name}\n\
         description: \"{description}\"\n\
         allowed-tools: bash file_read\n\
         metadata:\n\
           moa-version: \"1.0\"\n\
           moa-tags: \"review, skill\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {skill_name}\n\n\
         Use this reviewed procedure when the task pattern recurs.\n"
        );
        SkillPackage::from_skill_markdown(markdown)
            .validate()
            .expect("test skill package validates")
    }

    async fn create_draft_skill_artifact(
        test_db: &moa_test_support::postgres::TestDb,
        scope: &ActionRuleScope,
        package: &ValidatedSkillPackage,
    ) -> moa_artifacts::registry::StoredArtifactRevision {
        let document = skill_artifact_document_from_package(package, ArtifactStatus::Draft)
            .expect("build skill artifact document");
        let source = document.to_yaml().expect("render artifact yaml");
        let files = package
            .files
            .iter()
            .map(|file| NewArtifactFile {
                path: file.path.clone(),
                content: file.content.clone(),
                content_type: file.content_type.clone(),
                executable: file.executable,
            })
            .collect::<Vec<_>>();

        ArtifactRegistry::new(test_db.store().pool().clone())
            .create_draft(
                scope,
                NewArtifactDraft {
                    document: &document,
                    source_format: "yaml",
                    source_text: source.as_bytes(),
                    files: &files,
                },
            )
            .await
            .expect("create draft skill artifact")
    }

    async fn append_candidate(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        candidate_type: LearningCandidateType,
        status: LearningCandidateStatus,
        operation: &str,
        skill_name: &str,
        draft: &moa_artifacts::registry::StoredArtifactRevision,
    ) -> LearningCandidate {
        let suite = json!({
            "relative_path": format!("tenants/{}/skills/{skill_name}/tests/suite.toml", tenant_id_from_storage_partition_id(storage_partition_id)),
            "source_format": "toml",
            "source_text": format!(
                "[suite]\nname = \"{skill_name}-regression\"\n\n[[cases]]\nname = \"smoke\"\ninput = \"run it\"\n"
            ),
        });
        append_candidate_with_suite(
            test_db,
            storage_partition_id,
            candidate_type,
            status,
            operation,
            skill_name,
            draft,
            Some(suite),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_candidate_with_suite(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        candidate_type: LearningCandidateType,
        status: LearningCandidateStatus,
        operation: &str,
        skill_name: &str,
        draft: &moa_artifacts::registry::StoredArtifactRevision,
        generated_suite: Option<serde_json::Value>,
    ) -> LearningCandidate {
        let now = Utc::now();
        let mut payload = json!({
            "kind": "skill_draft_proposal",
            "operation": operation,
            "artifact_uid": draft.artifact_uid,
            "draft_artifact_revision_uid": draft.revision_uid,
            "artifact_kind": "skill",
            "artifact_name": skill_name,
            "artifact_path": format!("skills/{skill_name}/SKILL.md"),
            "source_session_id": Uuid::now_v7(),
        });
        if let Some(suite) = generated_suite {
            payload["generated_regression_suite"] = suite;
        }
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            user_id: None,
            candidate_type,
            status,
            target_id: Some(format!("skills/{skill_name}/SKILL.md")),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload,
            evaluation_payload: None,
            source_experience_ids: vec![Uuid::now_v7()],
            confidence: Some(0.86),
            risk_class: LearningRiskClass::Low,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now,
        };
        test_db
            .store()
            .append_learning_candidate(&candidate)
            .await
            .expect("append review candidate");
        candidate
    }

    fn review_request(
        storage_partition_id: &StoragePartitionId,
        candidate_id: Uuid,
        action: LearningCandidateReviewAction,
    ) -> LearningCandidateReviewRequest {
        LearningCandidateReviewRequest {
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            candidate_id,
            action,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("guard test".to_string()),
        }
    }
}

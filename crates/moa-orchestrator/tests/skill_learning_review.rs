//! Learning-review service tests for skill draft acceptance and rejection.

#![recursion_limit = "256"]

use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_core::wire::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
};
use moa_core::{
    ActionRuleScope, LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningEntry, LearningRiskClass, MoaConfig, MoaError, TenantId,
    WorkspaceId,
};
use moa_orchestrator::services::learning_review::{
    accept_skill_candidate_after_authz, get_learning_candidate_after_authz,
    reject_learning_candidate_after_authz,
};
use moa_session::PostgresSessionStore;
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::package::{SkillPackage, ValidatedSkillPackage};
use moa_skills::review::{
    LearningReviewStore, LearningReviewStoreFuture, SkillReviewAction, SkillReviewRequest,
    prepare_skill_acceptance, promote_claimed_skill_candidate,
};
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use uuid::Uuid;

mod skill_learning_review {
    use super::*;

    #[tokio::test]
    async fn accept_skill_candidate_publishes_draft_for_artifact_backed_registry() {
        // Pins: accepting a skill candidate publishes its draft artifact for artifact-backed skill loading.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let workspace_id = unique_workspace("review-accept");
        let scope = workspace_scope(&workspace_id);
        let skill_name = unique_skill_name("review-accept");
        let package = skill_package(&skill_name, "Review accepted draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &workspace_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;
        let store = Arc::new(test_db.store().clone());
        let config = review_config(&test_db);

        let loaded = get_learning_candidate_after_authz(
            store.clone(),
            GetLearningCandidateRequest {
                tenant_id: tenant_id_from_workspace(&workspace_id),
                candidate_id: candidate.id,
            },
        )
        .await
        .expect("load review candidate");
        assert_eq!(loaded.id, candidate.id);
        assert_eq!(loaded.workspace_id, workspace_id);

        let response = accept_skill_candidate_after_authz(
            store.clone(),
            config,
            #[cfg(feature = "internal-eval-runner")]
            review_providers(),
            LearningCandidateReviewRequest {
                tenant_id: tenant_id_from_workspace(&workspace_id),
                candidate_id: candidate.id,
                action: LearningCandidateReviewAction::Accept,
                reviewer_subject: "user:reviewer".to_string(),
                reason: Some("looks reusable".to_string()),
            },
        )
        .await
        .expect("accept skill candidate");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        assert_eq!(response.artifact_uid, Some(draft.artifact_uid));
        assert_eq!(
            response.draft_artifact_revision_uid,
            Some(draft.revision_uid)
        );
        assert_eq!(
            response.published_artifact_revision_uid,
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
            .get_learning_candidate(&workspace_id, candidate.id)
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
        #[cfg(feature = "internal-eval-runner")]
        {
            assert_eq!(evaluation["regression_execution"], "skipped");
            assert_eq!(
                evaluation["regression_report"]["reason"],
                "no previous active skill exists for comparison"
            );
        }
        #[cfg(not(feature = "internal-eval-runner"))]
        {
            assert_eq!(evaluation["regression_execution"], "unavailable");
            assert_eq!(
                evaluation["regression_report"]["reason"],
                "internal-eval-runner feature disabled"
            );
        }
        assert_eq!(
            evaluation["regression_report"]["runner"], "moa-eval",
            "review payload should identify the orchestrator-owned eval runner"
        );

        let learnings = test_db
            .store()
            .list_learnings(workspace_id.as_str(), Some("skill_created"), 10)
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
    async fn reject_skill_candidate_preserves_draft_without_publishing() {
        // Pins: rejecting a skill candidate marks it rejected but keeps the draft artifact for audit.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let workspace_id = unique_workspace("review-reject");
        let scope = workspace_scope(&workspace_id);
        let skill_name = unique_skill_name("review-reject");
        let package = skill_package(&skill_name, "Review rejected draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &workspace_id,
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
                tenant_id: tenant_id_from_workspace(&workspace_id),
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
            .get_learning_candidate(&workspace_id, candidate.id)
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
            .list_learnings(workspace_id.as_str(), Some("skill_created"), 10)
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
        let workspace_id = unique_workspace("review-rollback");
        let scope = workspace_scope(&workspace_id);
        let skill_name = unique_skill_name("review-rollback");
        let package = skill_package(&skill_name, "Review rollback draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let candidate = append_candidate(
            &test_db,
            &workspace_id,
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
            workspace_id: workspace_id.clone(),
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
            .get_learning_candidate(&workspace_id, candidate.id)
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
            .list_learnings(workspace_id.as_str(), Some("skill_created"), 10)
            .await
            .expect("list learning entries");
        assert!(
            learnings.is_empty(),
            "failed promotion must not append promoted learning"
        );
    }

    #[test]
    fn accept_reject_requires_tenant_operator() {
        // Pins: LearningReview handlers authorize tenant operator access before candidate payload reads.
        let source = include_str!("../src/services/learning_review.rs");
        assert!(
            source.contains("ObjectType::Tenant") && source.contains("Relation::Operator"),
            "LearningReview must authorize Tenant:Operator"
        );

        for handler in [
            "async fn get(\n        &self",
            "async fn accept_skill(\n        &self",
            "async fn reject(\n        &self",
        ] {
            let handler_start = source.find(handler).expect("handler exists in source");
            let handler_source = &source[handler_start..];
            let auth_pos = handler_source
                .find("authorize_tenant_operator(&ctx, request.tenant_id).await?")
                .expect("handler authorizes tenant operator");
            let boundary_pos = handler_source
                .find(".run(|| async move")
                .or_else(|| handler_source.find("let runtime = OrchestratorCtx::current();"))
                .expect("handler enters protected runtime work after authorization");
            assert!(
                auth_pos < boundary_pos,
                "{handler} must authorize before reading or mutating candidate state"
            );
        }

        assert_eq!(
            source
                .matches("request.reviewer_subject = fga_subject(&identity);")
                .count(),
            2,
            "mutating handlers must derive reviewer_subject from authenticated identity"
        );
        assert!(
            source.contains(".name(\"learning_review_accept_skill\")"),
            "accept_skill must run promotion and regression work in a named durable step"
        );
    }

    #[tokio::test]
    async fn accept_refuses_non_skill_and_reject_handles_workflow_candidate() {
        // Pins: accept stays skill-specific, while reject can disposition workflow proposals.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let workspace_id = unique_workspace("review-guard");
        let scope = workspace_scope(&workspace_id);
        let skill_name = unique_skill_name("review-guard");
        let package = skill_package(&skill_name, "Review guard draft skills");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let workflow_candidate = append_candidate(
            &test_db,
            &workspace_id,
            LearningCandidateType::Workflow,
            LearningCandidateStatus::Proposed,
            "workflow_improved",
            &skill_name,
            &draft,
        )
        .await;
        let non_proposed = append_candidate(
            &test_db,
            &workspace_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Rejected,
            "skill_created",
            &skill_name,
            &draft,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let config = review_config(&test_db);
        assert!(
            accept_skill_candidate_after_authz(
                store.clone(),
                config.clone(),
                #[cfg(feature = "internal-eval-runner")]
                review_providers(),
                review_request(
                    &workspace_id,
                    workflow_candidate.id,
                    LearningCandidateReviewAction::Accept,
                ),
            )
            .await
            .is_err(),
            "accept_skill must reject non-skill candidates"
        );
        let rejected_workflow = reject_learning_candidate_after_authz(
            store.clone(),
            review_request(
                &workspace_id,
                workflow_candidate.id,
                LearningCandidateReviewAction::Reject,
            ),
        )
        .await
        .expect("reject workflow candidate");
        assert_eq!(rejected_workflow.status, LearningCandidateStatus::Rejected);
        assert!(
            accept_skill_candidate_after_authz(
                store.clone(),
                config,
                #[cfg(feature = "internal-eval-runner")]
                review_providers(),
                review_request(
                    &workspace_id,
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
                    &workspace_id,
                    non_proposed.id,
                    LearningCandidateReviewAction::Reject
                ),
            )
            .await
            .is_err(),
            "reject must reject non-proposed candidates"
        );
    }

    fn workspace_scope(workspace_id: &WorkspaceId) -> ActionRuleScope {
        ActionRuleScope::Tenant {
            tenant_id: tenant_id_from_workspace(workspace_id),
        }
    }

    fn tenant_id_from_workspace(workspace_id: &WorkspaceId) -> TenantId {
        uuid::Uuid::parse_str(workspace_id.as_str())
            .map(TenantId::from)
            .unwrap_or_else(|_| {
                let hash = blake3::hash(workspace_id.as_str().as_bytes());
                let mut bytes = [0_u8; 16];
                bytes.copy_from_slice(&hash.as_bytes()[..16]);
                bytes[6] = (bytes[6] & 0x0f) | 0x80;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                TenantId::from(uuid::Uuid::from_bytes(bytes))
            })
    }

    #[derive(Clone)]
    struct FailingLearningAppendStore {
        store: Arc<PostgresSessionStore>,
    }

    impl LearningReviewStore for FailingLearningAppendStore {
        fn get_learning_candidate<'a>(
            &'a self,
            workspace_id: &'a WorkspaceId,
            candidate_id: Uuid,
        ) -> LearningReviewStoreFuture<'a, Option<LearningCandidate>> {
            Box::pin(async move {
                self.store
                    .get_learning_candidate(workspace_id, candidate_id)
                    .await
            })
        }

        fn update_learning_candidate_status_from<'a>(
            &'a self,
            update: &'a LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> LearningReviewStoreFuture<'a, bool> {
            Box::pin(async move {
                self.store
                    .update_learning_candidate_status_from(update, expected_status)
                    .await
            })
        }

        fn update_learning_candidate_status_from_in_tx<'a>(
            &'a self,
            conn: &'a mut sqlx::PgConnection,
            update: &'a LearningCandidateStatusUpdate,
            expected_status: LearningCandidateStatus,
        ) -> LearningReviewStoreFuture<'a, bool> {
            Box::pin(async move {
                self.store
                    .update_learning_candidate_status_from_in_tx(conn, update, expected_status)
                    .await
            })
        }

        fn append_learning<'a>(
            &'a self,
            entry: &'a LearningEntry,
        ) -> LearningReviewStoreFuture<'a, ()> {
            Box::pin(async move { self.store.append_learning(entry).await })
        }

        fn append_learning_in_tx<'a>(
            &'a self,
            _conn: &'a mut sqlx::PgConnection,
            _entry: &'a LearningEntry,
        ) -> LearningReviewStoreFuture<'a, ()> {
            Box::pin(async move {
                Err(MoaError::StorageError(
                    "injected learning append failure".to_string(),
                ))
            })
        }
    }

    fn review_config(test_db: &moa_test_support::postgres::TestDb) -> Arc<MoaConfig> {
        let mut config = MoaConfig::default();
        config.database.url = test_db.database_url().to_string();
        config.query_rewrite.enabled = false;
        Arc::new(config)
    }

    #[cfg(feature = "internal-eval-runner")]
    fn review_providers() -> Arc<moa_providers::ProviderRegistry> {
        Arc::new(moa_providers::ProviderRegistry::default())
    }

    fn unique_workspace(_prefix: &str) -> WorkspaceId {
        WorkspaceId::new(Uuid::now_v7().to_string())
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
           moa-one-liner: \"{description}\"\n\
           moa-tags: \"review, skill\"\n\
           moa-auto-generated: \"true\"\n\
           moa-use-count: \"0\"\n\
           moa-success-rate: \"1.0\"\n\
           moa-estimated-tokens: \"300\"\n\
         ---\n\n\
         # {skill_name}\n\n\
         Use this reviewed workflow when the task pattern recurs.\n"
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
        workspace_id: &WorkspaceId,
        candidate_type: LearningCandidateType,
        status: LearningCandidateStatus,
        operation: &str,
        skill_name: &str,
        draft: &moa_artifacts::registry::StoredArtifactRevision,
    ) -> LearningCandidate {
        let now = Utc::now();
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_workspace(workspace_id),
            workspace_id: workspace_id.clone(),
            user_id: None,
            candidate_type,
            status,
            target_id: Some(format!("skills/{skill_name}/SKILL.md")),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: json!({
                "kind": "skill_draft_proposal",
                "operation": operation,
                "artifact_uid": draft.artifact_uid,
                "draft_artifact_revision_uid": draft.revision_uid,
                "artifact_kind": "skill",
                "artifact_name": skill_name,
                "artifact_path": format!("skills/{skill_name}/SKILL.md"),
                "source_session_id": Uuid::now_v7(),
                "generated_regression_suite": {
                    "relative_path": format!("workspaces/{}/skills/{skill_name}/tests/suite.toml", workspace_id.as_str()),
                    "source_format": "toml",
                    "source_text": format!(
                        "[suite]\nname = \"{skill_name}-regression\"\n\n[[cases]]\nname = \"smoke\"\ninput = \"run it\"\n"
                    ),
                },
            }),
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
        workspace_id: &WorkspaceId,
        candidate_id: Uuid,
        action: LearningCandidateReviewAction,
    ) -> LearningCandidateReviewRequest {
        LearningCandidateReviewRequest {
            tenant_id: tenant_id_from_workspace(workspace_id),
            candidate_id,
            action,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("guard test".to_string()),
        }
    }
}

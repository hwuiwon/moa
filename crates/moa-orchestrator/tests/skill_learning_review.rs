//! Learning-review service tests for skill draft acceptance and rejection.

use std::sync::Arc;

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_core::wire::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
};
use moa_core::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateType, LearningRiskClass,
    MemoryScope, MoaConfig, WorkspaceId,
};
use moa_orchestrator::services::learning_review::{
    accept_skill_candidate_after_authz, get_learning_candidate_after_authz,
    reject_learning_candidate_after_authz,
};
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::package::{SkillPackage, ValidatedSkillPackage};
use moa_skills::registry::SkillRegistry;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use uuid::Uuid;

mod skill_learning_review {
    use super::*;

    #[tokio::test]
    async fn accept_skill_candidate_publishes_draft_and_materializes_active_skill() {
        // Pins: accepting a skill candidate publishes its draft artifact and materializes the active skill row.
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
                workspace_id: workspace_id.clone(),
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
            LearningCandidateReviewRequest {
                workspace_id: workspace_id.clone(),
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
        assert!(response.skill_uid.is_some());

        let published = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load published artifact")
            .expect("published artifact exists");
        assert_eq!(published.kind, ArtifactKind::Skill);
        assert_eq!(published.status, ArtifactStatus::Published);

        let active_skill = SkillRegistry::new(test_db.store().pool().clone())
            .load_by_name(&scope, &skill_name)
            .await
            .expect("load active skill")
            .expect("active skill should be materialized");
        assert_eq!(Some(active_skill.skill_uid), response.skill_uid);

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
        assert_eq!(
            evaluation["skill_uid"],
            response
                .skill_uid
                .expect("accepted response includes skill uid")
                .to_string()
        );
        assert_eq!(evaluation["regression_execution"], "unavailable");
        assert_eq!(
            evaluation["regression_report"]["reason"],
            "internal-eval-runner feature disabled"
        );
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
    async fn reject_skill_candidate_preserves_draft_without_active_skill() {
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
                workspace_id: workspace_id.clone(),
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
        assert_eq!(response.skill_uid, None);

        let preserved = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load preserved draft")
            .expect("draft artifact remains visible");
        assert_eq!(preserved.status, ArtifactStatus::Draft);
        assert!(
            SkillRegistry::new(test_db.store().pool().clone())
                .load_by_name(&scope, &skill_name)
                .await
                .expect("load optional active skill")
                .is_none(),
            "reject must not materialize an active skill"
        );

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

    #[test]
    fn accept_reject_requires_workspace_editor() {
        // Pins: LearningReview handlers authorize workspace editor access before candidate payload reads.
        let source = include_str!("../src/services/learning_review.rs");
        assert!(
            source.contains("Relation::Editor"),
            "LearningReview must authorize Workspace:Editor"
        );

        for handler in [
            "async fn get(\n        &self",
            "async fn accept_skill(\n        &self",
            "async fn reject(\n        &self",
        ] {
            let handler_start = source.find(handler).expect("handler exists in source");
            let handler_source = &source[handler_start..];
            let auth_pos = handler_source
                .find("authorize_workspace_editor(&ctx, &request.workspace_id).await?")
                .expect("handler authorizes workspace editor");
            let run_pos = handler_source
                .find(".run(|| async move")
                .expect("handler enters ctx.run after authorization");
            assert!(
                auth_pos < run_pos,
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

    fn workspace_scope(workspace_id: &WorkspaceId) -> MemoryScope {
        MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        }
    }

    fn review_config(test_db: &moa_test_support::postgres::TestDb) -> Arc<MoaConfig> {
        let mut config = MoaConfig::default();
        config.database.url = test_db.database_url().to_string();
        config.query_rewrite.enabled = false;
        Arc::new(config)
    }

    fn unique_workspace(prefix: &str) -> WorkspaceId {
        WorkspaceId::new(format!("{prefix}-{}", short_uuid()))
    }

    fn unique_skill_name(prefix: &str) -> String {
        format!("{prefix}-{}", short_uuid())
    }

    fn short_uuid() -> String {
        Uuid::now_v7()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect()
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
        scope: &MemoryScope,
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
            tenant_id: workspace_id.as_str().to_string(),
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
                        "name = \"{skill_name}-regression\"\n\n[[cases]]\nname = \"smoke\"\ninput = \"run it\"\n"
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
            workspace_id: workspace_id.clone(),
            candidate_id,
            action,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("guard test".to_string()),
        }
    }
}

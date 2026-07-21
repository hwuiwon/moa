//! Learning-review service tests for skill draft acceptance and rejection.

#![recursion_limit = "256"]

use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_config::MoaConfig;
use moa_config::RegressionMonitorConfig;
use moa_core::{
    error::MoaError, traits::SessionStore, types::action_policy::ActionRuleScope,
    types::agent::AgentContext, types::contact::SessionActorRef,
    types::experience::LearningCandidate, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateStatusUpdate, types::experience::LearningCandidateType,
    types::experience::LearningRiskClass, types::identifiers::ModelId,
    types::identifiers::SegmentId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::learning::LearningEntry, types::segments::TaskSegment, types::session::SessionMeta,
};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_orchestrator::services::learning_review::{
    accept_rollback_candidate_after_authz, accept_skill_candidate_after_authz,
    get_learning_candidate_after_authz, reject_learning_candidate_after_authz,
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
use moa_wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
};
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
    async fn accept_terminally_rejects_candidate_without_generated_suite_or_planning_audit() {
        // Pins: a candidate missing its generated regression suite is a content defect —
        // accept fails closed by rejecting the candidate and preserving the draft artifact,
        // instead of compiling, recording a planning audit, or promoting with an
        // "unavailable" regression report.
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
            review_tool_router(),
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
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_compile_audit \
             WHERE tenant_id = $1 AND source = 'skill_regression'",
        )
        .bind(tenant_id_from_storage_partition_id(&storage_partition_id).0)
        .fetch_one(test_db.store().pool())
        .await
        .expect("count normalized skill-regression compile audits");
        assert_eq!(audit_count, 0);

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
            review_tool_router(),
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
            review_tool_router(),
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
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_compile_audit \
             WHERE tenant_id = $1 AND source = 'skill_regression'",
        )
        .bind(tenant_id_from_storage_partition_id(&storage_partition_id).0)
        .fetch_one(test_db.store().pool())
        .await
        .expect("count normalized skill-regression compile audits");
        assert_eq!(audit_count, 0);
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
                review_tool_router(),
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
                review_tool_router(),
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

    #[tokio::test]
    async fn accept_rollback_archives_regressed_revision_and_restores_predecessor() {
        // Pins: accepting a rollback proposal archives the regressed published revision, restores
        // the prior published revision as the serving one, flips the original promotion to
        // RolledBack, invalidates its learning-log entry, and records a skill_rollback entry.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-accept");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-accept");
        let package = skill_package(&skill_name, "Rollback accepted skills");

        let previous = publish_skill_revision(&test_db, &scope, &package).await;
        let promoted = publish_skill_revision(&test_db, &scope, &package).await;
        assert_ne!(previous.revision_uid, promoted.revision_uid);

        let promotion_candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Promoted,
            "skill_improved",
            &skill_name,
            &promoted,
        )
        .await;
        seed_promotion_learning(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &promoted,
            promotion_candidate.id,
        )
        .await;
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            promoted.artifact_uid,
            promoted.revision_uid,
            Some(previous.revision_uid),
            promotion_candidate.id,
            LearningCandidateStatus::Proposed,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let response = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_request(
                &storage_partition_id,
                rollback.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect("accept rollback proposal");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        assert_eq!(
            response.published_artifact_revision_uid,
            Some(previous.revision_uid)
        );

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let archived = registry
            .load_revision(&scope, promoted.revision_uid)
            .await
            .expect("load archived revision")
            .expect("archived revision exists");
        assert_eq!(archived.status, ArtifactStatus::Archived);
        let serving = registry
            .load_visible_published(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving published revision")
            .expect("a published revision still serves");
        assert_eq!(
            serving.revision_uid, previous.revision_uid,
            "the prior revision serves once the regressed one is archived"
        );

        let rollback_reloaded = test_db
            .store()
            .get_learning_candidate(&tenant, rollback.id)
            .await
            .expect("reload rollback candidate")
            .expect("rollback candidate exists");
        assert_eq!(rollback_reloaded.status, LearningCandidateStatus::Promoted);

        let promotion_reloaded = test_db
            .store()
            .get_learning_candidate(&tenant, promotion_candidate.id)
            .await
            .expect("reload promotion candidate")
            .expect("promotion candidate exists");
        assert_eq!(
            promotion_reloaded.status,
            LearningCandidateStatus::RolledBack,
            "the original promotion is marked rolled back"
        );

        assert!(
            test_db
                .store()
                .list_learnings(storage_partition_id.as_str(), Some("skill_improved"), 10)
                .await
                .expect("list improved learning")
                .is_empty(),
            "the promotion learning-log entry is invalidated"
        );
        let rollback_learnings = test_db
            .store()
            .list_learnings(storage_partition_id.as_str(), Some("skill_rollback"), 10)
            .await
            .expect("list rollback learning");
        assert_eq!(rollback_learnings.len(), 1);
        assert_eq!(rollback_learnings[0].actor, "review:user:reviewer");
        assert_eq!(
            rollback_learnings[0].target_label.as_deref(),
            Some(skill_name.as_str())
        );
    }

    #[tokio::test]
    async fn accept_rollback_rejects_claimed_and_non_rollback_candidates() {
        // Pins: accept_rollback only executes an open (Proposed) rollback proposal — a claimed
        // (Evaluating) proposal and an ordinary skill draft candidate are both refused.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-guard");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-guard");
        let package = skill_package(&skill_name, "Rollback guard skills");
        let promoted = publish_skill_revision(&test_db, &scope, &package).await;

        let claimed = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            promoted.artifact_uid,
            promoted.revision_uid,
            None,
            Uuid::now_v7(),
            LearningCandidateStatus::Evaluating,
        )
        .await;
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let non_rollback = append_candidate(
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
        assert!(
            accept_rollback_candidate_after_authz(
                store.clone(),
                store.pool().clone(),
                review_request(
                    &storage_partition_id,
                    claimed.id,
                    LearningCandidateReviewAction::Accept,
                ),
            )
            .await
            .is_err(),
            "a claimed rollback proposal is not in Proposed and must be refused"
        );
        assert!(
            accept_rollback_candidate_after_authz(
                store.clone(),
                store.pool().clone(),
                review_request(
                    &storage_partition_id,
                    non_rollback.id,
                    LearningCandidateReviewAction::Accept,
                ),
            )
            .await
            .is_err(),
            "an ordinary skill draft candidate is not a rollback proposal and must be refused"
        );
    }

    #[tokio::test]
    async fn monitor_files_rollback_proposal_when_improved_skill_regresses() {
        // Pins: the monitor compares a promoted skill's post-promotion resolution rate against
        // its pre-promotion baseline over real seeded segments, resolves the predecessor revision
        // through the artifact join, and files exactly one Proposed rollback proposal.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("monitor-regress");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("monitor-regress");
        let package = skill_package(&skill_name, "Monitor regression skills");

        let previous = publish_skill_revision(&test_db, &scope, &package).await;
        let promoted = publish_skill_revision(&test_db, &scope, &package).await;
        let promotion_candidate_id = Uuid::now_v7();
        let promoted_at = Utc::now() - chrono::Duration::days(1);
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &promoted,
            promotion_candidate_id,
            promoted_at,
        )
        .await;

        let session = create_session_for_tenant(&test_db, tenant).await;
        // Strong pre-promotion baseline (all resolved) then a weak post-promotion window.
        for index in 0u32..6 {
            seed_segment(
                &test_db,
                session,
                &tenant,
                &skill_name,
                index,
                promoted_at - chrono::Duration::hours(6 + i64::from(index)),
                "resolved",
            )
            .await;
        }
        for index in 0u32..6 {
            let outcome = if index == 0 { "resolved" } else { "failed" };
            seed_segment(
                &test_db,
                session,
                &tenant,
                &skill_name,
                100 + index,
                promoted_at + chrono::Duration::hours(1 + i64::from(index)),
                outcome,
            )
            .await;
        }

        let filed = moa_skills::rollback::monitor_and_file_skill_regressions(
            test_db.store(),
            &RegressionMonitorConfig::default(),
            Utc::now(),
        )
        .await
        .expect("run regression monitor");
        assert_eq!(filed, 1, "one regressed skill files one rollback proposal");

        let proposals = test_db
            .store()
            .list_learning_candidates(
                &tenant.to_string(),
                Some(LearningCandidateStatus::Proposed),
                50,
            )
            .await
            .expect("list proposed candidates");
        let proposal = proposals
            .iter()
            .find(|candidate| {
                candidate
                    .payload
                    .get("kind")
                    .and_then(|value| value.as_str())
                    == Some(moa_skills::rollback::ROLLBACK_PROPOSAL_KIND)
            })
            .expect("a rollback proposal was filed");

        assert_eq!(
            proposal.id,
            moa_skills::rollback::rollback_candidate_id(tenant, &skill_name, promoted.revision_uid)
        );
        assert_eq!(
            proposal.payload["promoted_revision_uid"],
            promoted.revision_uid.to_string()
        );
        assert_eq!(
            proposal.payload["previous_revision_uid"],
            previous.revision_uid.to_string(),
            "the artifact join resolves the predecessor revision to restore"
        );
        assert_eq!(proposal.payload["post_samples"], 6);
        assert_eq!(proposal.payload["regressed_operation"], "skill_improved");

        // Re-running bumps the open proposal rather than duplicating it.
        let refiled = moa_skills::rollback::monitor_and_file_skill_regressions(
            test_db.store(),
            &RegressionMonitorConfig::default(),
            Utc::now(),
        )
        .await
        .expect("re-run regression monitor");
        assert_eq!(refiled, 1, "re-observation bumps rather than files anew");
        let proposals_after = test_db
            .store()
            .list_learning_candidates(
                &tenant.to_string(),
                Some(LearningCandidateStatus::Proposed),
                50,
            )
            .await
            .expect("list proposed candidates after re-run");
        let rollback_count = proposals_after
            .iter()
            .filter(|candidate| {
                candidate
                    .payload
                    .get("kind")
                    .and_then(|value| value.as_str())
                    == Some(moa_skills::rollback::ROLLBACK_PROPOSAL_KIND)
            })
            .count();
        assert_eq!(
            rollback_count, 1,
            "no duplicate proposal for the same skill"
        );
    }

    #[tokio::test]
    async fn accept_rollback_rejects_superseded_proposal() {
        // Pins: a rollback proposal whose promoted revision a newer promotion has
        // superseded is rejected terminally (not applied), leaving the superseded
        // revision published and the newer revision serving. Without the
        // currentness guard the stale proposal would archive a non-serving
        // revision and report success while the newer revision kept serving.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-superseded");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-superseded");
        let package = skill_package(&skill_name, "Superseded rollback skills");

        let v1 = publish_skill_revision(&test_db, &scope, &package).await;
        let v2 = publish_skill_revision(&test_db, &scope, &package).await;
        let v3 = publish_skill_revision(&test_db, &scope, &package).await;
        assert_ne!(v2.revision_uid, v3.revision_uid);

        // The proposal targets v2, but v3 has since been promoted and now serves.
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            v2.artifact_uid,
            v2.revision_uid,
            Some(v1.revision_uid),
            Uuid::now_v7(),
            LearningCandidateStatus::Proposed,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let result = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_request(
                &storage_partition_id,
                rollback.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await;
        assert!(
            result.is_err(),
            "a superseded rollback proposal must be refused, not applied"
        );

        let reloaded = test_db
            .store()
            .get_learning_candidate(&tenant, rollback.id)
            .await
            .expect("reload rollback candidate")
            .expect("rollback candidate exists");
        assert_eq!(
            reloaded.status,
            LearningCandidateStatus::Rejected,
            "the stale proposal is rejected, not left claimed or reproposed for retry"
        );

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let v2_reloaded = registry
            .load_revision(&scope, v2.revision_uid)
            .await
            .expect("load v2 revision")
            .expect("v2 revision exists");
        assert_eq!(
            v2_reloaded.status,
            ArtifactStatus::Published,
            "a stale rollback must not archive the superseded revision"
        );
        let serving = registry
            .load_visible_published(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving published revision")
            .expect("a published revision still serves");
        assert_eq!(
            serving.revision_uid, v3.revision_uid,
            "the newer promoted revision keeps serving"
        );
    }

    #[tokio::test]
    async fn accept_rollback_restores_predecessor_metadata() {
        // Pins: rolling back a regressed revision restores the predecessor's
        // serving-side description and tags — not only the SKILL.md body — so
        // ranking, summaries, and embeddings stop advertising the regressed
        // identity.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-metadata");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-metadata");
        let package_v1 = skill_package_with(
            &skill_name,
            "Stable predecessor behavior",
            "stable, reviewed",
        );
        let package_v2 = skill_package_with(
            &skill_name,
            "Regressed successor behavior",
            "regressed, risky",
        );

        let v1 = publish_skill_revision(&test_db, &scope, &package_v1).await;
        let v2 = publish_skill_revision(&test_db, &scope, &package_v2).await;

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let before = registry
            .load_visible_published(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving published revision")
            .expect("a published revision serves");
        assert_eq!(before.revision_uid, v2.revision_uid);
        assert_eq!(
            before.description, v2.document.metadata.description,
            "before rollback the artifact advertises the regressed description"
        );
        assert_ne!(
            v1.document.metadata.description,
            v2.document.metadata.description
        );
        assert_ne!(v1.document.metadata.tags, v2.document.metadata.tags);

        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            v2.artifact_uid,
            v2.revision_uid,
            Some(v1.revision_uid),
            Uuid::now_v7(),
            LearningCandidateStatus::Proposed,
        )
        .await;
        let store = Arc::new(test_db.store().clone());
        accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_request(
                &storage_partition_id,
                rollback.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect("accept rollback proposal");

        let serving = registry
            .load_visible_published(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving published revision")
            .expect("a published revision still serves");
        assert_eq!(
            serving.revision_uid, v1.revision_uid,
            "the predecessor revision serves again"
        );
        assert_eq!(
            serving.description, v1.document.metadata.description,
            "the artifact description is restored to the predecessor's"
        );
        assert_eq!(
            serving.tags, v1.document.metadata.tags,
            "the artifact tags are restored to the predecessor's"
        );
        assert_ne!(
            serving.description, v2.document.metadata.description,
            "the regressed description no longer serves"
        );
    }

    #[tokio::test]
    async fn accept_rollback_retires_created_skill_without_predecessor() {
        // Pins: rolling back a created skill that has no predecessor retires the
        // artifact identity (valid_to set) so nothing keeps serving it.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-created");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-created");
        let package = skill_package(&skill_name, "Created skill rollback");

        let created = publish_skill_revision(&test_db, &scope, &package).await;
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            created.artifact_uid,
            created.revision_uid,
            None,
            Uuid::now_v7(),
            LearningCandidateStatus::Proposed,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let response = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_request(
                &storage_partition_id,
                rollback.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect("accept created-skill rollback");
        assert_eq!(response.status, LearningCandidateStatus::Promoted);

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let serving = registry
            .load_visible_published(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("query serving published revision");
        assert!(
            serving.is_none(),
            "a retired created skill no longer serves any revision"
        );

        let artifact_valid_to = sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
            "SELECT valid_to FROM moa.artifact WHERE artifact_uid = $1",
        )
        .bind(created.artifact_uid)
        .fetch_one(test_db.store().pool())
        .await
        .expect("load retired artifact row");
        assert!(
            artifact_valid_to.is_some(),
            "the created-skill artifact identity is retired, not merely archived"
        );
    }

    #[tokio::test]
    async fn monitor_evaluates_only_the_latest_promotion_per_skill() {
        // Pins: with two live promotions of one skill, the monitor judges only the
        // latest (serving) promotion over its own post-window. The earlier
        // promotion — whose open-ended window folds in the newer revision's
        // outcomes and would otherwise regress — is not measured, so no stale
        // proposal is filed. Reverting to returning every promotion re-files it.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("monitor-latest");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("monitor-latest");
        let package = skill_package(&skill_name, "Latest-only monitor skills");

        // v1 predecessor, v2 earlier promotion, v3 latest/serving promotion.
        let _v1 = publish_skill_revision(&test_db, &scope, &package).await;
        let v2 = publish_skill_revision(&test_db, &scope, &package).await;
        let v3 = publish_skill_revision(&test_db, &scope, &package).await;
        let promoted_v2_at = Utc::now() - chrono::Duration::days(5);
        let promoted_v3_at = Utc::now() - chrono::Duration::days(2);
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &v2,
            Uuid::now_v7(),
            promoted_v2_at,
        )
        .await;
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &v3,
            Uuid::now_v7(),
            promoted_v3_at,
        )
        .await;

        let session = create_session_for_tenant(&test_db, tenant).await;
        // Strong baseline before v2 (all resolved).
        for index in 0u32..6 {
            seed_segment(
                &test_db,
                session,
                &tenant,
                &skill_name,
                index,
                promoted_v2_at - chrono::Duration::hours(6 + i64::from(index)),
                "resolved",
            )
            .await;
        }
        // Weak segments in v2's era (between v2 and v3): these fall inside v2's
        // open-ended post-window, and inside v3's pre-promotion baseline.
        for index in 0u32..6 {
            seed_segment(
                &test_db,
                session,
                &tenant,
                &skill_name,
                100 + index,
                promoted_v2_at + chrono::Duration::hours(1 + i64::from(index)),
                "failed",
            )
            .await;
        }
        // Strong segments after v3 (v3's post-window): v3 does not regress.
        for index in 0u32..6 {
            seed_segment(
                &test_db,
                session,
                &tenant,
                &skill_name,
                200 + index,
                promoted_v3_at + chrono::Duration::hours(1 + i64::from(index)),
                "resolved",
            )
            .await;
        }

        let filed = moa_skills::rollback::monitor_and_file_skill_regressions(
            test_db.store(),
            &RegressionMonitorConfig::default(),
            Utc::now(),
        )
        .await
        .expect("run regression monitor");
        assert_eq!(
            filed, 0,
            "only the serving promotion (v3) is judged, and it did not regress"
        );

        let proposals = test_db
            .store()
            .list_learning_candidates(
                &tenant.to_string(),
                Some(LearningCandidateStatus::Proposed),
                50,
            )
            .await
            .expect("list proposed candidates");
        assert!(
            proposals.iter().all(|candidate| {
                candidate
                    .payload
                    .get("kind")
                    .and_then(|value| value.as_str())
                    != Some(moa_skills::rollback::ROLLBACK_PROPOSAL_KIND)
            }),
            "no rollback proposal is filed for the superseded promotion"
        );
    }

    fn tenant_scope(storage_partition_id: &StoragePartitionId) -> ActionRuleScope {
        ActionRuleScope::Tenant {
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
        }
    }

    async fn create_session_for_tenant(
        test_db: &moa_test_support::postgres::TestDb,
        tenant: TenantId,
    ) -> SessionId {
        let meta = SessionMeta {
            tenant_id: tenant,
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(1),
            }),
            model: ModelId::new("test-model"),
            agent_context: Some(AgentContext::system_default()),
            ..SessionMeta::default()
        };
        test_db
            .store()
            .create_session(meta)
            .await
            .expect("create session for segments")
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_segment(
        test_db: &moa_test_support::postgres::TestDb,
        session_id: SessionId,
        tenant: &TenantId,
        skill_name: &str,
        segment_index: u32,
        started_at: chrono::DateTime<Utc>,
        outcome: &str,
    ) {
        let segment = TaskSegment {
            id: SegmentId(Uuid::now_v7()),
            session_id,
            tenant_id: tenant.to_string(),
            segment_index,
            task_summary: None,
            started_at,
            ended_at: Some(started_at + chrono::Duration::minutes(5)),
            turn_count: 9,
            tools_used: vec!["bash".to_string()],
            skills_activated: vec![skill_name.to_string()],
            skills_used: vec![skill_name.to_string()],
            token_cost: 1_000,
            previous_segment_id: None,
            outcome: Some(outcome.to_string()),
            assessment: None,
            outcome_confidence: Some(0.9),
        };
        test_db
            .store()
            .create_segment(&segment)
            .await
            .expect("seed task segment");
    }

    async fn seed_promotion_learning_at(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        promoted: &moa_artifacts::registry::StoredArtifactRevision,
        promotion_candidate_id: Uuid,
        promoted_at: chrono::DateTime<Utc>,
    ) {
        let entry = LearningEntry {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            learning_type: "skill_improved".to_string(),
            target_id: promoted.artifact_uid.to_string(),
            target_label: Some(skill_name.to_string()),
            payload: json!({
                "candidate_id": promotion_candidate_id,
                "artifact_uid": promoted.artifact_uid,
                "published_artifact_revision_uid": promoted.revision_uid,
            }),
            confidence: None,
            source_refs: vec![promotion_candidate_id],
            actor: "review:user:reviewer".to_string(),
            valid_from: promoted_at,
            valid_to: None,
            batch_id: None,
            version: 1,
        };
        test_db
            .store()
            .append_learning(&entry)
            .await
            .expect("append promotion learning");
    }

    /// Creates and publishes one skill artifact revision, returning it.
    async fn publish_skill_revision(
        test_db: &moa_test_support::postgres::TestDb,
        scope: &ActionRuleScope,
        package: &ValidatedSkillPackage,
    ) -> moa_artifacts::registry::StoredArtifactRevision {
        let draft = create_draft_skill_artifact(test_db, scope, package).await;
        let report = validate_for_status(&draft.document, ArtifactStatus::Published);
        ArtifactRegistry::new(test_db.store().pool().clone())
            .publish_revision(scope, draft.revision_uid, &report)
            .await
            .expect("publish skill revision")
    }

    /// Appends the `skill_improved`/`skill_created` learning-log entry a promotion writes.
    async fn seed_promotion_learning(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        promoted: &moa_artifacts::registry::StoredArtifactRevision,
        promotion_candidate_id: Uuid,
    ) {
        let entry = LearningEntry {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            learning_type: "skill_improved".to_string(),
            target_id: promoted.artifact_uid.to_string(),
            target_label: Some(skill_name.to_string()),
            payload: json!({
                "candidate_id": promotion_candidate_id,
                "artifact_uid": promoted.artifact_uid,
                "published_artifact_revision_uid": promoted.revision_uid,
            }),
            confidence: None,
            source_refs: vec![promotion_candidate_id],
            actor: "review:user:reviewer".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            batch_id: None,
            version: 1,
        };
        test_db
            .store()
            .append_learning(&entry)
            .await
            .expect("append promotion learning");
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_rollback_candidate(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        artifact_uid: Uuid,
        promoted_revision_uid: Uuid,
        previous_revision_uid: Option<Uuid>,
        promotion_candidate_id: Uuid,
        status: LearningCandidateStatus,
    ) -> LearningCandidate {
        let now = Utc::now();
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status,
            target_id: Some(artifact_uid.to_string()),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: json!({
                "kind": moa_skills::rollback::ROLLBACK_PROPOSAL_KIND,
                "rollback_key": skill_name,
                "skill_name": skill_name,
                "artifact_uid": artifact_uid,
                "promoted_revision_uid": promoted_revision_uid,
                "previous_revision_uid": previous_revision_uid,
                "promotion_candidate_id": promotion_candidate_id,
            }),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::High,
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
            .expect("append rollback candidate");
        candidate
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

    fn review_tool_router() -> Arc<ToolRouter> {
        Arc::new(ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
        ))
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
         Use this reviewed workflow when the task pattern recurs.\n"
        );
        SkillPackage::from_skill_markdown(markdown)
            .validate()
            .expect("test skill package validates")
    }

    /// Builds a skill package with correctly-indented frontmatter so `moa-tags`
    /// nest under `metadata:` and land in the document's `metadata.tags`.
    fn skill_package_with(
        skill_name: &str,
        description: &str,
        tags_csv: &str,
    ) -> ValidatedSkillPackage {
        let markdown = format!(
            "---\nname: {skill_name}\ndescription: \"{description}\"\n\
             allowed-tools: bash file_read\nmetadata:\n  moa-version: \"1.0\"\n  \
             moa-tags: \"{tags_csv}\"\n  moa-estimated-tokens: \"300\"\n---\n\n# {skill_name}\n\n\
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

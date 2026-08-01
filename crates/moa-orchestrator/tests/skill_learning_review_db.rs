//! Learning-review service tests for skill draft acceptance and rejection.

#![recursion_limit = "256"]

use std::{collections::HashMap, sync::Arc};

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_config::MoaConfig;
use moa_config::RegressionMonitorConfig;
use moa_core::{
    error::MoaError, traits::SessionStore, types::action_policy::ActionRuleScope,
    types::agent::AgentContext, types::contact::SessionActorRef,
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateStatusUpdate,
    types::experience::LearningCandidateType, types::experience::LearningProposalKind,
    types::experience::LearningRiskClass, types::identifiers::ModelId,
    types::identifiers::SegmentId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::learning::LearningEntry, types::learning::LearningLogSourceRef,
    types::memory::RlsContext, types::segments::TaskSegment, types::session::SessionMeta,
};
use moa_db::ScopedConn;
use moa_hands::{ToolRegistry, ToolRouter};
use moa_orchestrator::services::learning_review::{
    accept_rollback_candidate_after_authz, accept_skill_candidate_after_authz,
    dismiss_learning_candidate_after_authz, get_learning_candidate_after_authz,
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
use moa_wire::session_store::{
    GetLearningCandidateRequest, LearningCandidateReviewAction, LearningCandidateReviewRequest,
};
use serde_json::json;
use uuid::Uuid;

mod skill_learning_review {
    use super::*;

    #[tokio::test]
    async fn promote_claimed_skill_candidate_activates_draft_and_appends_learning() {
        // Pins: promoting a claimed skill candidate activates its draft artifact for
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
        let wire_request = LearningCandidateReviewRequest {
            tenant_id: request.tenant_id,
            candidate_id: request.candidate_id,
            action: LearningCandidateReviewAction::Accept,
            reviewer_subject: request.reviewer_subject.clone(),
            reason: request.reason.clone(),
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
            &wire_request.replay_digest(),
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
            outcome.activated_artifact_revision_uid,
            Some(draft.revision_uid)
        );

        let activated = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, draft.revision_uid)
            .await
            .expect("load activated artifact")
            .expect("activated artifact exists");
        assert_eq!(activated.kind, ArtifactKind::Skill);
        assert_eq!(activated.status, ArtifactStatus::Ready);

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
            evaluation["activated_artifact_revision_uid"],
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

        let decisions = test_db
            .store()
            .list_learning_candidate_decisions(candidate.id)
            .await
            .expect("list accepted-skill decision");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            moa_core::types::experience::LearningReviewDecision::AcceptedSkill
        );
        assert_eq!(
            decisions[0].request_digest.as_deref().map(<[u8]>::len),
            Some(32)
        );
        assert!(decisions[0].outcome.is_some());

        let replay_request = wire_request;
        let replay = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_tool_router(),
            replay_request.clone(),
        )
        .await
        .expect("terminal accepted-skill replay returns the committed outcome");
        assert_eq!(replay.candidate_id, outcome.candidate_id);
        assert_eq!(replay.status, outcome.status);
        assert_eq!(replay.artifact_uid, outcome.artifact_uid);
        assert_eq!(
            replay.activated_artifact_revision_uid,
            outcome.activated_artifact_revision_uid
        );

        let mut changed = replay_request;
        changed.reason = Some("different acceptance inputs".to_string());
        let conflict = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_tool_router(),
            changed,
        )
        .await
        .expect_err("changed terminal replay inputs conflict");
        assert!(format!("{conflict:?}").contains("different inputs"));
        let activation_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.artifact_activation_audit \
             WHERE activated_revision_uid = $1 AND decision_kind = 'activation'",
        )
        .bind(draft.revision_uid)
        .fetch_one(store.pool())
        .await
        .expect("count activation audits after replay");
        assert_eq!(activation_count, 1, "replay does not activate twice");
        assert_eq!(
            test_db
                .store()
                .list_learning_candidate_decisions(candidate.id)
                .await
                .expect("list decisions after replay")
                .len(),
            1,
            "replay does not append another terminal decision"
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
        let request = review_request(
            &storage_partition_id,
            candidate.id,
            LearningCandidateReviewAction::Accept,
        );

        let response = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_tool_router(),
            request.clone(),
        )
        .await
        .expect("accept disposition for suite-less candidate");

        assert_eq!(response.status, LearningCandidateStatus::Rejected);
        assert_eq!(response.activated_artifact_revision_uid, None);

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

        let replay = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_tool_router(),
            request.clone(),
        )
        .await
        .expect("matching gate-rejected retry returns the recorded response");
        assert_eq!(replay, response);
        let mut changed = request;
        changed.reason = Some("changed gate rejection reason".to_string());
        let conflict = accept_skill_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_config(&test_db),
            review_providers(),
            review_tool_router(),
            changed,
        )
        .await
        .expect_err("changed gate-rejected retry conflicts");
        assert!(format!("{conflict:?}").contains("different inputs"));
        let decisions = store
            .list_learning_candidate_decisions(candidate.id)
            .await
            .expect("list gate-rejection decisions");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            moa_core::types::experience::LearningReviewDecision::Rejected
        );
        assert!(decisions[0].outcome.is_some());
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
            Some("this is [not toml"),
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
    async fn dismiss_closes_an_advisory_item_with_exactly_one_audit_under_replay() {
        // Pins the whole dismissal contract on the real store, in one flow:
        //
        //  1. An advisory item — which no code can promote — is closed by dismissal.
        //  2. The closure writes a DURABLE audit naming the reviewer, the reason, and the
        //     status it came from. Without it an item is closed with no record of who
        //     closed it, and a compliance export cannot answer "what happened to this".
        //  3. A REPLAY (the same Restate re-execution the orchestrator can produce)
        //     succeeds and writes NOTHING new. The audit is keyed (candidate, decision),
        //     so a second identical row would be a fabricated second decision.
        //  4. A REVIEWABLE proposal is refused: dismissal is not a back door for closing
        //     a skill draft without the regression gate.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-dismiss");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-dismiss");
        let package = skill_package(&skill_name, "Review dismissed advisory items");
        let draft = create_draft_skill_artifact(&test_db, &scope, &package).await;

        let advisory = append_candidate_with_kind(
            &test_db,
            &storage_partition_id,
            LearningProposalKind::MemoryAdvisory,
            LearningCandidateStatus::Advisory,
            &skill_name,
            &draft,
        )
        .await;
        let store = Arc::new(test_db.store().clone());
        let request = LearningCandidateReviewRequest {
            tenant_id: tenant,
            candidate_id: advisory.id,
            action: LearningCandidateReviewAction::Dismiss,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("already covered by an existing note".to_string()),
        };

        let response = dismiss_learning_candidate_after_authz(store.clone(), request.clone())
            .await
            .expect("dismiss advisory item");
        assert_eq!(response.status, LearningCandidateStatus::Dismissed);
        assert_eq!(
            response.activated_artifact_revision_uid, None,
            "a dismissal activates nothing"
        );

        let dismissed = test_db
            .store()
            .get_learning_candidate(&tenant, advisory.id)
            .await
            .expect("reload dismissed candidate")
            .expect("candidate exists");
        assert_eq!(dismissed.status, LearningCandidateStatus::Dismissed);

        let decisions = test_db
            .store()
            .list_learning_candidate_decisions(advisory.id)
            .await
            .expect("list durable decisions");
        assert_eq!(decisions.len(), 1, "a dismissal writes exactly one audit");
        assert_eq!(
            decisions[0].decision,
            moa_core::types::experience::LearningReviewDecision::Dismissed
        );
        assert_eq!(decisions[0].from_status, LearningCandidateStatus::Advisory);
        assert_eq!(decisions[0].to_status, LearningCandidateStatus::Dismissed);
        assert_eq!(
            decisions[0].reviewer_subject.as_deref(),
            Some("user:reviewer")
        );
        assert_eq!(
            decisions[0].reason.as_deref(),
            Some("already covered by an existing note")
        );

        let replayed = dismiss_learning_candidate_after_authz(store.clone(), request)
            .await
            .expect("a replayed dismissal succeeds rather than conflicting");
        assert_eq!(replayed.status, LearningCandidateStatus::Dismissed);
        assert_eq!(
            test_db
                .store()
                .list_learning_candidate_decisions(advisory.id)
                .await
                .expect("list durable decisions after replay")
                .len(),
            1,
            "a replay must not append a second identical audit"
        );

        let reviewable = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_created",
            &unique_skill_name("review-dismiss-draft"),
            &draft,
        )
        .await;
        let refusal = dismiss_learning_candidate_after_authz(
            store,
            LearningCandidateReviewRequest {
                tenant_id: tenant,
                candidate_id: reviewable.id,
                action: LearningCandidateReviewAction::Dismiss,
                reviewer_subject: "user:reviewer".to_string(),
                reason: None,
            },
        )
        .await;
        // On the REASON. A `Proposed` draft is also refused by the state check that
        // follows, so `is_err()` would pass with no kind guard at all — and then a
        // reviewable proposal sitting in `Advisory`-adjacent state (or any future
        // status widening) would slip through the one check meant to stop it.
        let refusal = refusal.expect_err("a reviewable proposal is not dismissible");
        assert!(
            format!("{refusal:?}").contains("reviewable proposals are accepted or rejected"),
            "refusal must name the proposal kind, not the current status: {refusal:?}"
        );
        assert!(
            test_db
                .store()
                .list_learning_candidate_decisions(reviewable.id)
                .await
                .expect("list decisions for the refused candidate")
                .is_empty(),
            "a refused dismissal writes no audit"
        );
    }

    #[tokio::test]
    async fn reject_skill_candidate_preserves_draft_without_activation() {
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
        assert_eq!(response.activated_artifact_revision_uid, None);

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
    async fn promotion_rolls_back_artifact_activation_when_learning_append_fails() {
        // Pins: accept promotion activates, promotes, and appends learning in one transaction.
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
            &[7_u8; 32],
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
            "failed promotion must roll back artifact activation"
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
    async fn promotion_refuses_a_serving_baseline_that_moved_after_prepare() {
        // Pins: the regression gate and activation compare the same captured
        // serving generation; a concurrent promotion cannot silently rebind the
        // passing report to its newer package.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("review-baseline-cas");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("review-baseline-cas");
        let package = skill_package(&skill_name, "Fence the reviewed serving baseline");
        let initial = activate_skill_revision(&test_db, &scope, &package).await;
        let stale_draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let mover_draft = create_draft_skill_artifact(&test_db, &scope, &package).await;
        let stale_candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_improved",
            &skill_name,
            &stale_draft,
        )
        .await;
        let mover_candidate = append_candidate(
            &test_db,
            &storage_partition_id,
            LearningCandidateType::Skill,
            LearningCandidateStatus::Proposed,
            "skill_improved",
            &skill_name,
            &mover_draft,
        )
        .await;
        let store = PassthroughReviewStore {
            store: Arc::new(test_db.store().clone()),
        };
        let stale_request = SkillReviewRequest {
            tenant_id: tenant,
            candidate_id: stale_candidate.id,
            action: SkillReviewAction::Accept,
            reviewer_subject: "user:reviewer".to_string(),
            reason: Some("stale regression result".to_string()),
        };
        let mover_request = SkillReviewRequest {
            candidate_id: mover_candidate.id,
            reason: Some("concurrent promotion".to_string()),
            ..stale_request.clone()
        };
        let pool = test_db.store().pool().clone();
        let stale = prepare_skill_acceptance(&store, pool.clone(), &stale_request)
            .await
            .expect("capture stale baseline");
        let mover = prepare_skill_acceptance(&store, pool.clone(), &mover_request)
            .await
            .expect("capture mover baseline");
        assert_eq!(
            stale.expected_serving.revision_uid,
            Some(initial.revision_uid)
        );
        assert_eq!(stale.expected_serving, mover.expected_serving);

        promote_claimed_skill_candidate(
            &store,
            pool.clone(),
            &mover_request,
            mover,
            json!({"regression_execution": "completed", "decision": "accepted"}),
            passing_acceptance_checks(),
            &[8_u8; 32],
        )
        .await
        .expect("move serving pointer");
        let error = promote_claimed_skill_candidate(
            &store,
            pool,
            &stale_request,
            stale,
            json!({"regression_execution": "completed", "decision": "accepted"}),
            passing_acceptance_checks(),
            &[9_u8; 32],
        )
        .await
        .expect_err("stale baseline must fail closed");
        assert!(matches!(
            error,
            moa_skills::review::SkillReviewError::Conflict(_)
        ));

        let serving = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_serving(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving skill")
            .expect("mover remains serving");
        assert_eq!(serving.revision_uid, mover_draft.revision_uid);
        let stale_revision = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(&scope, stale_draft.revision_uid)
            .await
            .expect("load stale draft")
            .expect("stale draft remains");
        assert_eq!(stale_revision.status, ArtifactStatus::Draft);
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
    async fn accept_rollback_archives_regressed_revision_and_stops_serving() {
        // Pins: accepting a rollback proposal archives the regressed revision and
        // leaves the skill serving nothing, flips the original promotion to
        // RolledBack, invalidates its learning-log entry, and records a
        // skill_rollback entry. The predecessor is deliberately not restored:
        // restoring an older revision is itself a serving transition that must pass a
        // separate learned-skill review, so rollback performs only the safe half:
        // taking the regressed revision out of service.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-accept");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-accept");
        let package = skill_package(&skill_name, "Rollback accepted skills");

        let previous = activate_skill_revision(&test_db, &scope, &package).await;
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;
        assert_ne!(previous.revision_uid, promoted.revision_uid);

        let promotion_candidate_id = activation_promotion_candidate_id(
            &test_db,
            promoted.artifact_uid,
            promoted.revision_uid,
        )
        .await;
        seed_promotion_learning(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &promoted,
            promotion_candidate_id,
        )
        .await;
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            promoted.artifact_uid,
            promoted.revision_uid,
            Some(previous.revision_uid),
            LearningCandidateStatus::Proposed,
        )
        .await;

        let store = Arc::new(test_db.store().clone());
        let request = review_request(
            &storage_partition_id,
            rollback.id,
            LearningCandidateReviewAction::Accept,
        );
        let response = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            request.clone(),
        )
        .await
        .expect("accept rollback proposal");

        assert_eq!(response.status, LearningCandidateStatus::Promoted);
        assert_eq!(
            response.activated_artifact_revision_uid, None,
            "rollback unserves the regressed revision; it must not report the predecessor as restored"
        );

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let archived = registry
            .load_revision(&scope, promoted.revision_uid)
            .await
            .expect("load archived revision")
            .expect("archived revision exists");
        assert_eq!(archived.status, ArtifactStatus::Archived);
        assert!(
            registry
                .load_serving(&scope, ArtifactKind::Skill, &skill_name)
                .await
                .expect("load serving revision")
                .is_none(),
            "the regressed revision stops serving and no predecessor is auto-restored"
        );
        let predecessor = registry
            .load_revision(&scope, previous.revision_uid)
            .await
            .expect("load predecessor revision")
            .expect("predecessor revision exists");
        assert_eq!(
            predecessor.status,
            ArtifactStatus::Superseded,
            "the predecessor stays superseded and must go back through release to serve"
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
            .get_learning_candidate(&tenant, promotion_candidate_id)
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

        let decisions = store
            .list_learning_candidate_decisions(rollback.id)
            .await
            .expect("list accepted rollback decision");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            moa_core::types::experience::LearningReviewDecision::AcceptedRollback
        );
        assert_eq!(
            decisions[0].request_digest.as_deref().map(<[u8]>::len),
            Some(32)
        );
        assert!(decisions[0].outcome.is_some());

        let replay = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            request.clone(),
        )
        .await
        .expect("terminal rollback replay returns committed outcome");
        assert_eq!(replay, response);
        let mut changed = request;
        changed.reason = Some("different rollback inputs".to_string());
        let conflict =
            accept_rollback_candidate_after_authz(store.clone(), store.pool().clone(), changed)
                .await
                .expect_err("changed terminal rollback inputs conflict");
        assert!(format!("{conflict:?}").contains("different inputs"));
        let rollback_audits: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.artifact_activation_audit \
             WHERE previous_revision_uid = $1 AND decision_kind = 'rollback'",
        )
        .bind(promoted.revision_uid)
        .fetch_one(store.pool())
        .await
        .expect("count rollback transitions after replay");
        assert_eq!(rollback_audits, 1, "replay does not archive twice");
        assert_eq!(
            store
                .list_learning_candidate_decisions(rollback.id)
                .await
                .expect("list rollback decisions after replay")
                .len(),
            1,
            "replay does not append another terminal decision"
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
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;

        let claimed = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            promoted.artifact_uid,
            promoted.revision_uid,
            None,
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
        // Asserting on the REASON, not merely on failure. A skill draft carries no
        // `promoted_revision_uid`, so a handler with no kind check at all still errors
        // here — for a missing-payload reason, several steps after it should already
        // have been refused. `is_err()` alone cannot tell those apart, and a
        // wrong-kind candidate reaching payload parsing is a gate that is not gating.
        let wrong_kind = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            review_request(
                &storage_partition_id,
                non_rollback.id,
                LearningCandidateReviewAction::Accept,
            ),
        )
        .await
        .expect_err("an ordinary skill draft is not a rollback proposal");
        assert!(
            format!("{wrong_kind:?}").contains("only skill rollback proposals can be accepted"),
            "refusal must name the wrong proposal kind, not a downstream payload defect: \
             {wrong_kind:?}"
        );
    }

    #[tokio::test]
    async fn accept_rollback_rejects_payload_ids_that_disagree_with_typed_provenance() {
        // Pins: payload UUIDs are descriptive only. Destructive rollback
        // authority is derived from the typed promotion source and immutable
        // activation audit, so a producer cannot retarget a proposal by changing
        // either the artifact or promotion candidate in JSON.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-tampered-payload");
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-tampered-payload");
        let package = skill_package(&skill_name, "Reject tampered rollback authority");
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;
        let store = Arc::new(test_db.store().clone());

        for field in ["artifact_uid", "promotion_candidate_id"] {
            let mut proposal = build_rollback_candidate(
                &test_db,
                &storage_partition_id,
                &skill_name,
                promoted.artifact_uid,
                promoted.revision_uid,
                None,
                LearningCandidateStatus::Proposed,
            )
            .await;
            proposal.payload[field] = json!(Uuid::now_v7());
            store
                .append_learning_candidate(&proposal)
                .await
                .expect("append payload-tampered rollback proposal");

            let error = accept_rollback_candidate_after_authz(
                store.clone(),
                store.pool().clone(),
                review_request(
                    &storage_partition_id,
                    proposal.id,
                    LearningCandidateReviewAction::Accept,
                ),
            )
            .await
            .expect_err("tampered rollback authority must be refused");
            assert!(
                format!("{error:?}").contains("does not match its activation provenance"),
                "{field} mismatch is reported as a provenance failure: {error:?}"
            );
            let unchanged = store
                .get_learning_candidate(
                    &tenant_id_from_storage_partition_id(&storage_partition_id),
                    proposal.id,
                )
                .await
                .expect("reload tampered proposal")
                .expect("tampered proposal remains auditable");
            assert_eq!(unchanged.status, LearningCandidateStatus::Proposed);
        }
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

        let previous = activate_skill_revision(&test_db, &scope, &package).await;
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;
        let promoted_at = moa_test_support::fixtures::pg_now() - chrono::Duration::days(1);
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &promoted,
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
            moa_test_support::fixtures::pg_now(),
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
            moa_skills::rollback::rollback_candidate_id(
                tenant,
                &skill_name,
                promoted.revision_uid,
                Uuid::parse_str(
                    proposal.payload["expected_transition_audit_uid"]
                        .as_str()
                        .expect("rollback proposal transition audit uid"),
                )
                .expect("valid rollback proposal transition audit uid"),
                proposal.payload["expected_pointer_version"]
                    .as_i64()
                    .expect("rollback proposal pointer version"),
            )
        );
        assert_eq!(
            proposal.payload["promoted_revision_uid"],
            promoted.revision_uid.to_string()
        );
        assert_eq!(
            proposal.payload["previous_revision_uid"],
            previous.revision_uid.to_string(),
            "the artifact join records predecessor provenance without restoring it"
        );
        assert_eq!(proposal.payload["post_samples"], 6);
        assert_eq!(proposal.payload["regressed_operation"], "skill_improved");

        // Re-running bumps the open proposal rather than duplicating it.
        let refiled = moa_skills::rollback::monitor_and_file_skill_regressions(
            test_db.store(),
            &RegressionMonitorConfig::default(),
            moa_test_support::fixtures::pg_now(),
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
    async fn monitor_does_not_treat_a_tombstone_revision_as_a_serving_predecessor() {
        // Pins: reactivation after rollback retains the prior revision UID in
        // pointer history, but the prior generation was not serving. The monitor
        // must not export that tombstone as a baseline that rollback could appear
        // to restore.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("monitor-tombstone-predecessor");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let release_scope = moa_artifacts::release::TenantScope::new(tenant);
        let skill_name = unique_skill_name("monitor-tombstone-predecessor");
        let package = skill_package(&skill_name, "Ignore an unserved tombstone baseline");
        let _v1 = activate_skill_revision(&test_db, &scope, &package).await;
        let v2 = activate_skill_revision(&test_db, &scope, &package).await;
        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let pointer = registry
            .load_serving_pointer(&release_scope, v2.artifact_uid)
            .await
            .expect("load v2 pointer")
            .expect("v2 serves");
        let activation_audit_uid = pointer.activation_audit_uid;
        let mut conn =
            ScopedConn::begin_as_app(test_db.store().pool(), &RlsContext::tenant(tenant), true)
                .await
                .expect("begin rollback");
        assert_eq!(
            ArtifactRegistry::rollback_serving_revision_in_tx(
                conn.as_mut(),
                &release_scope,
                v2.revision_uid,
                activation_audit_uid,
                pointer.pointer_version,
                "user:reviewer",
                Some("remove v2"),
            )
            .await
            .expect("rollback v2"),
            moa_artifacts::registry::RollbackApplication::Applied
        );
        conn.commit().await.expect("commit rollback");

        let v3 = activate_skill_revision(&test_db, &scope, &package).await;
        assert_eq!(v3.artifact_uid, v2.artifact_uid);
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &v3,
            moa_test_support::fixtures::pg_now(),
        )
        .await;

        let promotions = test_db
            .store()
            .list_recent_skill_promotions(
                &tenant,
                moa_test_support::fixtures::pg_now() - chrono::Duration::days(1),
            )
            .await
            .expect("list recent promotions");
        let promotion = promotions
            .iter()
            .find(|promotion| promotion.promoted_revision_uid == v3.revision_uid)
            .expect("v3 promotion is visible");
        assert_eq!(
            promotion.previous_revision_uid, None,
            "a retained tombstone UID is not a serving predecessor"
        );
    }

    #[tokio::test]
    async fn accept_rollback_rejects_superseded_proposal() {
        // Pins: a rollback proposal whose promoted revision a newer promotion has
        // superseded is rejected terminally (not applied), leaving the superseded
        // revision superseded and the newer revision serving. Without the
        // currentness guard the stale proposal would archive a non-serving
        // revision and report success while the newer revision kept serving.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-superseded");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-superseded");
        let package = skill_package(&skill_name, "Superseded rollback skills");

        let v1 = activate_skill_revision(&test_db, &scope, &package).await;
        let v2 = activate_skill_revision(&test_db, &scope, &package).await;
        let v3 = activate_skill_revision(&test_db, &scope, &package).await;
        assert_ne!(v2.revision_uid, v3.revision_uid);

        // The proposal targets v2, but v3 has since been promoted and now serves.
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            v2.artifact_uid,
            v2.revision_uid,
            Some(v1.revision_uid),
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
        .expect("a superseded rollback proposal is terminally rejected");
        assert_eq!(response.status, LearningCandidateStatus::Rejected);

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
            ArtifactStatus::Superseded,
            "a stale rollback must leave the superseded revision alone, not archive it"
        );
        let serving = registry
            .load_serving(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving activated revision")
            .expect("an activated revision still serves");
        assert_eq!(
            serving.revision_uid, v3.revision_uid,
            "the newer promoted revision keeps serving"
        );
    }

    #[tokio::test]
    async fn rollback_refuses_stale_epoch_for_the_same_serving_revision() {
        // Pins: revision identity alone is not sufficient rollback authority. A
        // stale audit uid or pointer epoch must leave even the same revision
        // serving, closing the reactivation ABA case.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-generation-cas");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-generation-cas");
        let package = skill_package(&skill_name, "Fence rollback serving generations");
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;
        let release_scope = moa_artifacts::release::TenantScope::new(tenant);
        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let pointer = registry
            .load_serving_pointer(&release_scope, promoted.artifact_uid)
            .await
            .expect("load serving pointer")
            .expect("activated revision has a pointer");
        let transition = pointer.activation_audit_uid;

        for (audit_uid, pointer_version) in [
            (Uuid::now_v7(), pointer.pointer_version),
            (transition, pointer.pointer_version + 1),
        ] {
            let mut conn =
                ScopedConn::begin_as_app(test_db.store().pool(), &RlsContext::tenant(tenant), true)
                    .await
                    .expect("begin stale rollback transaction");
            let application = ArtifactRegistry::rollback_serving_revision_in_tx(
                conn.as_mut(),
                &release_scope,
                promoted.revision_uid,
                audit_uid,
                pointer_version,
                "user:reviewer",
                Some("stale rollback proposal"),
            )
            .await
            .expect("stale rollback is a terminal application result");
            assert!(matches!(
                application,
                moa_artifacts::registry::RollbackApplication::Superseded {
                    serving_revision_uid
                } if serving_revision_uid == promoted.revision_uid
            ));
            conn.commit()
                .await
                .expect("commit no-op stale rollback transaction");
        }

        let unchanged = registry
            .load_serving_pointer(&release_scope, promoted.artifact_uid)
            .await
            .expect("reload serving pointer")
            .expect("pointer remains serving");
        assert_eq!(unchanged, pointer);
        let revision = registry
            .load_revision(&scope, promoted.revision_uid)
            .await
            .expect("reload promoted revision")
            .expect("promoted revision remains");
        assert_eq!(revision.status, ArtifactStatus::Ready);
    }

    #[tokio::test]
    async fn rollback_proposal_is_rejected_truthfully_when_skill_is_already_unserved() {
        // Pins: a tombstoned pointer is not reported as though the proposed
        // revision still serves; the stale proposal is rejected with the actual
        // unserved state and no additional pointer move.
        let test_db = bootstrap_test_db().await.expect("bootstrap review test db");
        let storage_partition_id = unique_workspace("rollback-already-unserved");
        let tenant = tenant_id_from_storage_partition_id(&storage_partition_id);
        let scope = tenant_scope(&storage_partition_id);
        let skill_name = unique_skill_name("rollback-already-unserved");
        let package = skill_package(&skill_name, "Report an already unserved rollback");
        let promoted = activate_skill_revision(&test_db, &scope, &package).await;
        let release_scope = moa_artifacts::release::TenantScope::new(tenant);
        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let active = registry
            .load_serving_pointer(&release_scope, promoted.artifact_uid)
            .await
            .expect("load active pointer")
            .expect("activated revision has a pointer");
        let mut conn =
            ScopedConn::begin_as_app(test_db.store().pool(), &RlsContext::tenant(tenant), true)
                .await
                .expect("begin initial rollback");
        let activation_audit_uid = active.activation_audit_uid;
        assert_eq!(
            ArtifactRegistry::rollback_serving_revision_in_tx(
                conn.as_mut(),
                &release_scope,
                promoted.revision_uid,
                activation_audit_uid,
                active.pointer_version,
                "user:first-reviewer",
                Some("first rollback"),
            )
            .await
            .expect("apply initial rollback"),
            moa_artifacts::registry::RollbackApplication::Applied
        );
        conn.commit().await.expect("commit initial rollback");
        let tombstone = registry
            .load_serving_pointer(&release_scope, promoted.artifact_uid)
            .await
            .expect("load serving pointer after rollback");
        assert!(tombstone.is_none(), "rollback removes the serving pointer");

        let proposal = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            promoted.artifact_uid,
            promoted.revision_uid,
            None,
            LearningCandidateStatus::Proposed,
        )
        .await;
        let store = Arc::new(test_db.store().clone());
        let request = review_request(
            &storage_partition_id,
            proposal.id,
            LearningCandidateReviewAction::Accept,
        );
        let response = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            request.clone(),
        )
        .await
        .expect("already-unserved rollback proposal is terminally rejected");
        assert_eq!(response.status, LearningCandidateStatus::Rejected);
        let rejected = store
            .get_learning_candidate(&tenant, proposal.id)
            .await
            .expect("reload rollback proposal")
            .expect("rollback proposal remains auditable");
        assert_eq!(rejected.status, LearningCandidateStatus::Rejected);
        assert!(
            rejected
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("serves no revision"))
        );
        assert_eq!(
            registry
                .load_serving_pointer(&release_scope, promoted.artifact_uid)
                .await
                .expect("reload tombstone"),
            tombstone
        );

        let replay = accept_rollback_candidate_after_authz(
            store.clone(),
            store.pool().clone(),
            request.clone(),
        )
        .await
        .expect("matching stale rollback retry returns its recorded rejection");
        assert_eq!(replay, response);
        let mut changed = request;
        changed.reason = Some("changed stale rollback reason".to_string());
        let conflict =
            accept_rollback_candidate_after_authz(store.clone(), store.pool().clone(), changed)
                .await
                .expect_err("changed stale rollback retry conflicts");
        assert!(format!("{conflict:?}").contains("different inputs"));
        let decisions = store
            .list_learning_candidate_decisions(proposal.id)
            .await
            .expect("list stale rollback decisions");
        assert_eq!(decisions.len(), 1);
        assert_eq!(
            decisions[0].decision,
            moa_core::types::experience::LearningReviewDecision::Rejected
        );
        assert_eq!(
            decisions[0].request_digest.as_deref().map(<[u8]>::len),
            Some(32)
        );
    }

    #[tokio::test]
    async fn accept_rollback_stops_advertising_the_regressed_identity() {
        // Pins: rolling back a regressed revision stops the artifact from serving and
        // drops its identity embedding, so ranking and summaries stop advertising the
        // regressed skill. The predecessor's metadata is not silently restored --
        // that would bypass its own learned-skill review -- so this pins the absence
        // of any serving identity, which is the fail-closed outcome.
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

        let v1 = activate_skill_revision(&test_db, &scope, &package_v1).await;
        let v2 = activate_skill_revision(&test_db, &scope, &package_v2).await;

        let registry = ArtifactRegistry::new(test_db.store().pool().clone());
        let before = registry
            .load_serving(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("load serving revision")
            .expect("a revision serves");
        assert_eq!(before.revision_uid, v2.revision_uid);
        assert_eq!(
            before.description, v2.document.metadata.description,
            "before rollback the artifact advertises the regressed description"
        );
        assert_ne!(
            v1.document.metadata.description,
            v2.document.metadata.description
        );

        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            v2.artifact_uid,
            v2.revision_uid,
            Some(v1.revision_uid),
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

        assert!(
            registry
                .load_serving(&scope, ArtifactKind::Skill, &skill_name)
                .await
                .expect("load serving revision")
                .is_none(),
            "nothing serves after the regressed revision is rolled back"
        );
        let embeddings = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.skill_embedding WHERE artifact_uid = $1",
        )
        .bind(v2.artifact_uid)
        .fetch_one(test_db.store().pool())
        .await
        .expect("count skill embeddings");
        assert_eq!(
            embeddings, 0,
            "the stale identity embedding is dropped so ranking stops advertising it"
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

        let created = activate_skill_revision(&test_db, &scope, &package).await;
        let rollback = append_rollback_candidate(
            &test_db,
            &storage_partition_id,
            &skill_name,
            created.artifact_uid,
            created.revision_uid,
            None,
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
            .load_serving(&scope, ArtifactKind::Skill, &skill_name)
            .await
            .expect("query serving activated revision");
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
        let _v1 = activate_skill_revision(&test_db, &scope, &package).await;
        let v2 = activate_skill_revision(&test_db, &scope, &package).await;
        let v3 = activate_skill_revision(&test_db, &scope, &package).await;
        let promoted_v2_at = moa_test_support::fixtures::pg_now() - chrono::Duration::days(5);
        let promoted_v3_at = moa_test_support::fixtures::pg_now() - chrono::Duration::days(2);
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &v2,
            promoted_v2_at,
        )
        .await;
        seed_promotion_learning_at(
            &test_db,
            &storage_partition_id,
            &skill_name,
            &v3,
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
            moa_test_support::fixtures::pg_now(),
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
        promoted_at: chrono::DateTime<Utc>,
    ) {
        let promotion_candidate_id = activation_promotion_candidate_id(
            test_db,
            promoted.artifact_uid,
            promoted.revision_uid,
        )
        .await;
        seed_promoted_candidate(
            test_db,
            storage_partition_id,
            skill_name,
            promoted.revision_uid,
            promoted.artifact_uid,
            promotion_candidate_id,
        )
        .await;
        let pointer = ArtifactRegistry::new(test_db.store().pool().clone())
            .load_serving_pointer(
                &moa_artifacts::release::TenantScope::new(tenant_id_from_storage_partition_id(
                    storage_partition_id,
                )),
                promoted.artifact_uid,
            )
            .await
            .expect("load promotion pointer")
            .expect("promotion pointer exists");
        let entry = LearningEntry {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            learning_type: "skill_improved".to_string(),
            target_id: promoted.artifact_uid.to_string(),
            target_label: Some(skill_name.to_string()),
            payload: json!({
                "candidate_id": promotion_candidate_id,
                "artifact_uid": promoted.artifact_uid,
                "activated_artifact_revision_uid": promoted.revision_uid,
                "activation_audit_uid": pointer.activation_audit_uid,
                "activated_pointer_version": pointer.pointer_version,
            }),
            confidence: None,
            sources: vec![LearningLogSourceRef::ArtifactRevision {
                revision_uid: promoted.revision_uid,
            }],
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

    /// Creates and activates one skill artifact revision, returning it.
    async fn activate_skill_revision(
        test_db: &moa_test_support::postgres::TestDb,
        scope: &ActionRuleScope,
        package: &ValidatedSkillPackage,
    ) -> moa_artifacts::registry::StoredArtifactRevision {
        let draft = create_draft_skill_artifact(test_db, scope, package).await;
        // A skill reaches serving only through learned-skill activation, so the
        // fixture drives that path explicitly.
        moa_artifacts::test_fixtures::activate_revision(
            test_db.store().pool(),
            moa_artifacts::release::TenantScope::from_action_rule_scope(scope)
                .expect("skill fixtures are tenant scoped"),
            moa_artifacts::release::ActivationTarget::SkillVisibility {
                artifact_uid: draft.artifact_uid,
            },
            draft.revision_uid,
        )
        .await
        .expect("activate skill revision");
        ArtifactRegistry::new(test_db.store().pool().clone())
            .load_revision(scope, draft.revision_uid)
            .await
            .expect("load activated skill revision")
            .expect("activated skill revision exists")
    }

    async fn activation_promotion_candidate_id(
        _test_db: &moa_test_support::postgres::TestDb,
        artifact_uid: Uuid,
        revision_uid: Uuid,
    ) -> Uuid {
        Uuid::from_u128(
            artifact_uid.as_u128() ^ revision_uid.as_u128() ^ 0x51c1_11ea_4e00_8a11_u128,
        )
    }

    /// Appends one candidate of an explicit proposal kind at its initial status.
    ///
    /// The informational kinds have no draft to activate, so they carry the same
    /// typed artifact-revision source as a draft proposal only because the fixture
    /// needs one real referent; the deferred completeness trigger refuses a
    /// candidate that ends its transaction with no source at all.
    async fn append_candidate_with_kind(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        proposal_kind: LearningProposalKind,
        status: LearningCandidateStatus,
        skill_name: &str,
        draft: &moa_artifacts::registry::StoredArtifactRevision,
    ) -> LearningCandidate {
        let now = moa_test_support::fixtures::pg_now();
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            user_id: None,
            candidate_type: LearningCandidateType::Memory,
            proposal_kind,
            status,
            target_id: Some(format!("memory/{skill_name}")),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: json!({ "kind": "memory_advisory", "observation": skill_name }),
            evaluation_payload: None,
            sources: vec![LearningCandidateSourceRef::ArtifactRevision {
                revision_uid: draft.revision_uid,
            }],
            confidence: Some(0.4),
            risk_class: LearningRiskClass::Low,
            promotion_requirements: Vec::new(),
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now,
        };
        test_db
            .store()
            .append_learning_candidate(&candidate)
            .await
            .expect("append informational candidate");
        candidate
    }

    /// Seeds the promoted candidate a promotion learning entry points back at.
    ///
    /// In production a promotion learning entry's `candidate_id` always names the
    /// candidate that was just promoted, and both the rollback proposal's typed
    /// `PromotionCandidate` source and the rollback learning entry's `Candidate`
    /// source carry a real foreign key to it. A fixture that invented a bare
    /// `Uuid::now_v7()` was describing a promotion that never happened, which the
    /// schema now refuses to store — so the fixture seeds the promotion instead of
    /// asserting against a shape production cannot produce.
    async fn seed_promoted_candidate(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        promoted_revision_uid: Uuid,
        artifact_uid: Uuid,
        promotion_candidate_id: Uuid,
    ) {
        let tenant_id = tenant_id_from_storage_partition_id(storage_partition_id);
        if test_db
            .store()
            .get_learning_candidate(&tenant_id, promotion_candidate_id)
            .await
            .expect("look up promoted candidate fixture")
            .is_some()
        {
            return;
        }
        let now = moa_test_support::fixtures::pg_now();
        let candidate = LearningCandidate {
            id: promotion_candidate_id,
            tenant_id,
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            proposal_kind: LearningProposalKind::SkillDraft,
            status: LearningCandidateStatus::Promoted,
            target_id: Some(format!("skills/{skill_name}/SKILL.md")),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload: json!({
                "kind": "skill_draft_proposal",
                "operation": "skill_improved",
                "artifact_uid": artifact_uid,
                "draft_artifact_revision_uid": promoted_revision_uid,
                "artifact_name": skill_name,
            }),
            evaluation_payload: None,
            sources: vec![LearningCandidateSourceRef::ArtifactRevision {
                revision_uid: promoted_revision_uid,
            }],
            confidence: Some(0.9),
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
            .expect("seed the promoted candidate a rollback would reverse");
    }

    /// Appends the `skill_improved`/`skill_created` learning-log entry a promotion writes.
    async fn seed_promotion_learning(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        promoted: &moa_artifacts::registry::StoredArtifactRevision,
        promotion_candidate_id: Uuid,
    ) {
        seed_promoted_candidate(
            test_db,
            storage_partition_id,
            skill_name,
            promoted.revision_uid,
            promoted.artifact_uid,
            promotion_candidate_id,
        )
        .await;
        let entry = LearningEntry {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            learning_type: "skill_improved".to_string(),
            target_id: promoted.artifact_uid.to_string(),
            target_label: Some(skill_name.to_string()),
            payload: {
                let scope = moa_artifacts::release::TenantScope::new(
                    tenant_id_from_storage_partition_id(storage_partition_id),
                );
                let pointer = ArtifactRegistry::new(test_db.store().pool().clone())
                    .load_serving_pointer(&scope, promoted.artifact_uid)
                    .await
                    .expect("load promotion pointer")
                    .expect("promotion pointer exists");
                json!({
                "candidate_id": promotion_candidate_id,
                "artifact_uid": promoted.artifact_uid,
                "activated_artifact_revision_uid": promoted.revision_uid,
                "activation_audit_uid": pointer.activation_audit_uid,
                "activated_pointer_version": pointer.pointer_version,
                })
            },
            confidence: None,
            sources: vec![LearningLogSourceRef::ArtifactRevision {
                revision_uid: promoted.revision_uid,
            }],
            actor: "review:user:reviewer".to_string(),
            valid_from: moa_test_support::fixtures::pg_now(),
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

    async fn append_rollback_candidate(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        artifact_uid: Uuid,
        promoted_revision_uid: Uuid,
        previous_revision_uid: Option<Uuid>,
        status: LearningCandidateStatus,
    ) -> LearningCandidate {
        let candidate = build_rollback_candidate(
            test_db,
            storage_partition_id,
            skill_name,
            artifact_uid,
            promoted_revision_uid,
            previous_revision_uid,
            status,
        )
        .await;
        test_db
            .store()
            .append_learning_candidate(&candidate)
            .await
            .expect("append rollback candidate");
        candidate
    }

    async fn build_rollback_candidate(
        test_db: &moa_test_support::postgres::TestDb,
        storage_partition_id: &StoragePartitionId,
        skill_name: &str,
        artifact_uid: Uuid,
        promoted_revision_uid: Uuid,
        previous_revision_uid: Option<Uuid>,
        status: LearningCandidateStatus,
    ) -> LearningCandidate {
        let now = moa_test_support::fixtures::pg_now();
        let (expected_transition_audit_uid, expected_pointer_version) =
            sqlx::query_as::<_, (Uuid, i64)>(
                "SELECT audit_uid, activated_pointer_version \
                 FROM moa.artifact_activation_audit \
                 WHERE artifact_uid = $1 \
                   AND activated_revision_uid = $2 \
                   AND decision_kind = 'activation' \
                 ORDER BY activated_pointer_version DESC \
                 LIMIT 1",
            )
            .bind(artifact_uid)
            .bind(promoted_revision_uid)
            .fetch_one(test_db.store().pool())
            .await
            .expect("load exact activation generation for rollback fixture");
        let promotion_candidate_id =
            activation_promotion_candidate_id(test_db, artifact_uid, promoted_revision_uid).await;
        seed_promoted_candidate(
            test_db,
            storage_partition_id,
            skill_name,
            promoted_revision_uid,
            artifact_uid,
            promotion_candidate_id,
        )
        .await;
        let mut sources = vec![
            LearningCandidateSourceRef::ArtifactRevision {
                revision_uid: promoted_revision_uid,
            },
            LearningCandidateSourceRef::PromotionCandidate {
                candidate_id: promotion_candidate_id,
            },
        ];
        if let Some(previous_revision_uid) = previous_revision_uid {
            sources.push(LearningCandidateSourceRef::ArtifactRevision {
                revision_uid: previous_revision_uid,
            });
        }
        // Rollback authority comes from the exact activation audit. The fixture
        // must not invent an unrelated promoted-candidate UUID and merely repeat
        // it in payload and sources.
        LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            proposal_kind: LearningProposalKind::SkillRollback,
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
                "expected_transition_audit_uid": expected_transition_audit_uid,
                "expected_pointer_version": expected_pointer_version,
            }),
            evaluation_payload: None,
            // A rollback proposal stands on the revision it would archive AND on
            // the promotion it reverses. Both are real rows, so both are typed
            // sources: the promotion link is what makes the original promotion
            // reachable when an erasure enters through the rollback.
            sources,
            confidence: None,
            risk_class: LearningRiskClass::High,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: None,
            batch_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn passing_acceptance_checks() -> AcceptanceChecks {
        AcceptanceChecks {
            held_in_pass: true,
            held_in_description: "candidate suite passed".to_string(),
            held_out_pass: true,
            held_out_description: "held-out suite passed".to_string(),
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

        async fn record_learning_candidate_decision_in_tx(
            &self,
            conn: &mut sqlx::PgConnection,
            decision: &moa_core::types::experience::LearningCandidateDecisionRecord,
        ) -> std::result::Result<bool, MoaError> {
            self.store
                .record_learning_candidate_decision_in_tx(conn, decision)
                .await
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

        async fn record_learning_candidate_decision_in_tx(
            &self,
            _conn: &mut sqlx::PgConnection,
            _decision: &moa_core::types::experience::LearningCandidateDecisionRecord,
        ) -> std::result::Result<bool, MoaError> {
            unreachable!("learning append fails before decision recording")
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
            moa_hands::local_development_sandbox_policy(),
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
             metadata:\n  moa-version: \"1.0\"\n  \
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
        let suite = format!(
            "[suite]\nname = \"{skill_name}-regression\"\n\n[[cases]]\nname = \"smoke\"\ninput = \"run it\"\n"
        );
        append_candidate_with_suite(
            test_db,
            storage_partition_id,
            candidate_type,
            status,
            operation,
            skill_name,
            draft,
            Some(suite.as_str()),
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
        generated_suite: Option<&str>,
    ) -> LearningCandidate {
        let now = moa_test_support::fixtures::pg_now();
        let payload = json!({
            "kind": "skill_draft_proposal",
            "operation": operation,
            "artifact_uid": draft.artifact_uid,
            "draft_artifact_revision_uid": draft.revision_uid,
            "artifact_kind": "skill",
            "artifact_name": skill_name,
            "artifact_path": format!("skills/{skill_name}/SKILL.md"),
            "source_session_id": Uuid::now_v7(),
        });
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
            user_id: None,
            candidate_type,
            proposal_kind: LearningProposalKind::SkillDraft,
            status,
            target_id: Some(format!("skills/{skill_name}/SKILL.md")),
            target_label: Some(skill_name.to_string()),
            task_fingerprint: None,
            task_facets: None,
            payload,
            evaluation_payload: None,
            sources: vec![LearningCandidateSourceRef::ArtifactRevision {
                revision_uid: draft.revision_uid,
            }],
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
        // The suite is attributable storage owned by the artifact registry, so a
        // fixture that wants the gate to see one has to write the row the
        // production filing path writes — not a payload key the gate no longer
        // reads.
        if let Some(suite_source) = generated_suite {
            // A real session, because the contribution's source columns carry real
            // foreign keys: an unattributable suite is exactly what the table
            // refuses to store, and a fixture that could fake one would be
            // testing a shape production cannot produce.
            let source_session_id = create_session_for_tenant(test_db, candidate.tenant_id).await;
            let mut conn = test_db
                .store()
                .pool()
                .acquire()
                .await
                .expect("acquire connection for suite contribution");
            ArtifactRegistry::record_suite_contribution_in_tx(
                &mut conn,
                storage_partition_id.as_str(),
                &candidate.tenant_id.to_string(),
                &moa_artifacts::registry::NewSuiteContribution {
                    candidate_id: candidate.id,
                    revision_uid: Some(draft.revision_uid),
                    kind: moa_artifacts::registry::SuiteContributionKind::Generated,
                    suite_name: format!("skills/{skill_name}/tests/suite.toml"),
                    suite_source: suite_source.to_string(),
                    source_session_id: Some(source_session_id.0),
                    source_experience_id: None,
                },
            )
            .await
            .expect("record generated suite contribution");
        }
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

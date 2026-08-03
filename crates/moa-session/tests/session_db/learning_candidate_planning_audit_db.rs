//! PostgreSQL contracts for normalized skill-regression compile audits.

use moa_core::types::{
    agent::AgentContext,
    contact::SessionActorRef,
    execution_planning::{
        ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlanningAuditEnvelope,
        ExecutionPlanningAuditPayload, bounded_audit_report,
    },
    experience::{
        LearningCandidate, LearningCandidateSourceRef, LearningCandidateStatus,
        LearningCandidateStatusUpdate, LearningCandidateType, LearningProposalKind,
        LearningRiskClass,
    },
    identifiers::{ModelId, SessionId, TenantId},
    session::SessionMeta,
};
use moa_core::{canonical_json::canonical_json_bytes, traits::SessionStore};
use moa_execution::repository::{CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::{Value, json};
use uuid::Uuid;

async fn learning_audit_test_db() -> TestDb {
    bootstrap_test_db()
        .await
        .expect("bootstrap learning planning-audit test database")
}

/// Seeds one session so a candidate fixture has a source row its foreign key can
/// actually resolve. Provenance is normalized now: a fabricated uuid is rejected.
async fn seed_source_session(test_db: &TestDb, tenant_id: TenantId) -> SessionId {
    test_db
        .store()
        .create_session(SessionMeta {
            tenant_id,
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(1),
            }),
            model: ModelId::new("test-model"),
            agent_context: Some(AgentContext::system_default()),
            ..SessionMeta::default()
        })
        .await
        .expect("seed learning-candidate source session")
}

fn candidate(
    tenant_id: TenantId,
    source_session_id: SessionId,
    draft_revision_uid: Uuid,
    evaluation_payload: Option<Value>,
) -> LearningCandidate {
    let now = moa_test_support::fixtures::pg_now();
    LearningCandidate {
        id: Uuid::now_v7(),
        tenant_id,
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        proposal_kind: LearningProposalKind::SkillDraft,
        status: LearningCandidateStatus::Evaluating,
        target_id: Some(format!("skill://audit-{}", draft_revision_uid.simple())),
        target_label: Some(format!("audit-{}", draft_revision_uid.simple())),
        task_fingerprint: None,
        task_facets: None,
        payload: json!({
            "kind": "skill_draft_proposal",
            "draft_artifact_revision_uid": draft_revision_uid,
        }),
        evaluation_payload,
        sources: vec![LearningCandidateSourceRef::Session {
            session_id: source_session_id,
        }],
        confidence: Some(0.9),
        risk_class: LearningRiskClass::Low,
        promotion_requirements: vec!["human_review".to_string()],
        status_reason: Some("claimed for review".to_string()),
        batch_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn operation_key(draft_revision_uid: Uuid, hash_byte: char) -> String {
    format!(
        "skill_regression:{draft_revision_uid}:{}",
        hash_byte.to_string().repeat(64)
    )
}

fn compiler_report() -> String {
    let report = bounded_audit_report(true, Vec::new()).expect("build empty compiler report");
    String::from_utf8(canonical_json_bytes(&report).expect("serialize compiler report"))
        .expect("canonical compiler report is UTF-8")
}

fn audit(tenant_id: TenantId, operation_key: &str) -> ExecutionPlanningAuditEnvelope {
    ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id: None,
        originating_sequence: None,
        payload: ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::SkillRegression,
            operation_key: operation_key.to_string(),
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionCompileOutcome::Accepted,
            candidate_hash: "1".repeat(64),
            final_plan_hash: Some("2".repeat(64)),
            validation_report: compiler_report(),
            duration_micros: 23,
            created_at: moa_test_support::fixtures::pg_now(),
        },
    }
}

#[tokio::test]
async fn learning_candidate_finalization_requires_normalized_compile_audit_db() {
    // Pins: compiling skill reviews finalize only after their exact normalized compile audit
    // exists, and terminal evaluation payloads do not embed a compatibility history copy.
    let test_db = learning_audit_test_db().await;
    let tenant_id = TenantId::new();
    let draft_revision_uid = Uuid::now_v7();
    let source_session_id = seed_source_session(&test_db, tenant_id).await;
    let candidate = candidate(
        tenant_id,
        source_session_id,
        draft_revision_uid,
        Some(json!({"seed": {"keep": true}})),
    );
    test_db
        .store()
        .append_learning_candidate(&candidate)
        .await
        .expect("append evaluating skill candidate");
    let operation_key = operation_key(draft_revision_uid, 'a');
    let update = LearningCandidateStatusUpdate {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Promoted,
        status_reason: Some("review completed".to_string()),
        evaluation_payload: Some(json!({
            "regression_report": {"decision": "promoted"},
            "terminal": {"keep": true},
        })),
        updated_at: moa_test_support::fixtures::pg_now(),
    };

    let missing = test_db
        .store()
        .finalize_learning_candidate_status_from(
            &update,
            LearningCandidateStatus::Evaluating,
            Some(&operation_key),
        )
        .await
        .expect_err("missing normalized compile audit must fail closed");
    assert!(missing.to_string().contains("was not persisted"));

    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let written = repository
        .write_compile_audit(
            ExecutionScope::Tenant { tenant_id },
            &audit(tenant_id, &operation_key),
        )
        .await
        .expect("write normalized skill-regression compile audit");
    assert!(matches!(written, CompileAuditWriteOutcome::Applied(_)));

    assert!(
        test_db
            .store()
            .finalize_learning_candidate_status_from(
                &update,
                LearningCandidateStatus::Evaluating,
                Some(&operation_key),
            )
            .await
            .expect("finalize candidate with normalized compile audit")
    );

    let loaded = test_db
        .store()
        .get_learning_candidate(&tenant_id, candidate.id)
        .await
        .expect("load finalized candidate")
        .expect("finalized candidate exists");
    assert_eq!(loaded.status, LearningCandidateStatus::Promoted);
    let evaluation = loaded.evaluation_payload.expect("evaluation payload");
    assert_eq!(evaluation["seed"], json!({"keep": true}));
    assert_eq!(evaluation["terminal"], json!({"keep": true}));
    assert_eq!(
        evaluation["regression_report"]["decision"],
        json!("promoted")
    );
    assert!(
        evaluation
            .pointer("/review/regression_report/planning_audit_history")
            .is_none()
    );
}

#[tokio::test]
async fn learning_candidate_finalization_rejects_a_different_compile_operation_db() {
    // Pins: one tenant's normalized skill-regression audit cannot satisfy a different operation
    // key, and the candidate remains evaluating when the compare-and-set prerequisite fails.
    let test_db = learning_audit_test_db().await;
    let tenant_id = TenantId::new();
    let draft_revision_uid = Uuid::now_v7();
    let source_session_id = seed_source_session(&test_db, tenant_id).await;
    let candidate = candidate(tenant_id, source_session_id, draft_revision_uid, None);
    test_db
        .store()
        .append_learning_candidate(&candidate)
        .await
        .expect("append evaluating skill candidate");
    let persisted_key = operation_key(draft_revision_uid, 'b');
    let expected_key = operation_key(draft_revision_uid, 'c');
    ExecutionRepository::new(test_db.store().pool().clone())
        .write_compile_audit(
            ExecutionScope::Tenant { tenant_id },
            &audit(tenant_id, &persisted_key),
        )
        .await
        .expect("write a different normalized compile audit");
    let update = LearningCandidateStatusUpdate {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Rejected,
        status_reason: Some("review rejected".to_string()),
        evaluation_payload: Some(json!({"decision": "rejected"})),
        updated_at: moa_test_support::fixtures::pg_now(),
    };

    let error = test_db
        .store()
        .finalize_learning_candidate_status_from(
            &update,
            LearningCandidateStatus::Evaluating,
            Some(&expected_key),
        )
        .await
        .expect_err("different compile operation must not satisfy finalization");
    assert!(error.to_string().contains("was not persisted"));
    assert_eq!(
        test_db
            .store()
            .get_learning_candidate(&tenant_id, candidate.id)
            .await
            .expect("reload candidate")
            .expect("candidate exists")
            .status,
        LearningCandidateStatus::Evaluating
    );
}

#[tokio::test]
async fn noncompiling_skill_review_finalizes_without_compile_audit_db() {
    // Pins: instruction-only skill reviews have no compiler operation and can finalize without
    // manufacturing an audit record or compatibility payload.
    let test_db = learning_audit_test_db().await;
    let tenant_id = TenantId::new();
    let source_session_id = seed_source_session(&test_db, tenant_id).await;
    let candidate = candidate(
        tenant_id,
        source_session_id,
        Uuid::now_v7(),
        Some(json!({"skill_form": "instruction_only", "seed": {"keep": true}})),
    );
    test_db
        .store()
        .append_learning_candidate(&candidate)
        .await
        .expect("append noncompiling skill candidate");
    let update = LearningCandidateStatusUpdate {
        candidate_id: candidate.id,
        status: LearningCandidateStatus::Rejected,
        status_reason: Some("review rejected".to_string()),
        evaluation_payload: Some(json!({"terminal": {"decision": "rejected"}})),
        updated_at: moa_test_support::fixtures::pg_now(),
    };

    assert!(
        test_db
            .store()
            .finalize_learning_candidate_status_from(
                &update,
                LearningCandidateStatus::Evaluating,
                None,
            )
            .await
            .expect("finalize noncompiling skill")
    );
    let loaded = test_db
        .store()
        .get_learning_candidate(&tenant_id, candidate.id)
        .await
        .expect("load noncompiling candidate")
        .expect("noncompiling candidate exists");
    assert_eq!(loaded.status, LearningCandidateStatus::Rejected);
    assert_eq!(
        loaded.evaluation_payload.expect("evaluation payload")["seed"],
        json!({"keep": true})
    );
}

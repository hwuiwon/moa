//! Skills service DTO mapping tests.

use chrono::{TimeZone, Utc};
use moa_artifacts::registry::{ArtifactRun, ArtifactRunStatus};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_orchestrator::services::skills::{
    procedure_run_summary_from_run, skill_summary_from_skill,
};
use moa_skills::package::SkillPackageManifest;
use moa_skills::registry::Skill;
use uuid::Uuid;

#[test]
fn skill_summary_from_skill_preserves_visible_row_fields() {
    // Pins: list responses expose scope, version, identity, tags, and timestamps from registry rows.
    let now = Utc
        .with_ymd_and_hms(2026, 6, 8, 0, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let skill_uid =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture skill id parses");
    let tenant_id = TenantId::from(
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("fixture tenant id parses"),
    );
    let summary = skill_summary_from_skill(Skill {
        skill_uid,
        tenant_id: Some(tenant_id),
        user_id: None,
        scope: "tenant".to_string(),
        name: "debug-oauth-refresh".to_string(),
        description: "Investigate OAuth refresh bugs".to_string(),
        package_hash: vec![0xab, 0xcd],
        skill_md_hash: vec![0x12, 0x34],
        file_count: 2,
        total_size_bytes: 64,
        manifest: SkillPackageManifest {
            schema_version: 1,
            skill_md_path: "SKILL.md".to_string(),
            skill_md_estimated_tokens: 12,
            allowed_tools: Vec::new(),
            artifact_schema_version: "moa.artifact/v1".to_string(),
            artifact_kind: "skill".to_string(),
            inputs_schema: serde_json::json!({}),
            outputs_schema: serde_json::json!({}),
            actions: Vec::new(),
            connectors: Vec::new(),
            ui: serde_json::json!({}),
            files: Vec::new(),
        },
        version: 2,
        tags: vec!["oauth".to_string(), "auth".to_string()],
        valid_to: None,
        created_at: now,
        updated_at: now,
    })
    .expect("tenant skill row should map to summary");

    assert_eq!(summary.skill_uid, skill_uid);
    assert_eq!(summary.scope, ActionRuleScope::Tenant { tenant_id });
    assert_eq!(summary.version, 2);
    assert_eq!(summary.name, "debug-oauth-refresh");
    assert_eq!(summary.description, "Investigate OAuth refresh bugs");
    assert_eq!(summary.tags, vec!["oauth".to_string(), "auth".to_string()]);
    assert_eq!(summary.package_hash, "abcd");
    assert_eq!(summary.skill_md_hash, "1234");
    assert_eq!(summary.file_count, 2);
    assert_eq!(summary.total_size_bytes, 64);
    assert_eq!(summary.created_at, now);
    assert_eq!(summary.updated_at, now);
}

#[test]
fn procedure_run_summary_from_run_preserves_dashboard_fields() {
    // Pins: procedure list summaries expose stable run identity, backing artifact,
    // lifecycle state, and timestamps without requiring a status detail fetch.
    let started_at = Utc
        .with_ymd_and_hms(2026, 7, 5, 12, 0, 0)
        .single()
        .expect("fixture started_at should be valid");
    let completed_at = Utc
        .with_ymd_and_hms(2026, 7, 5, 12, 5, 0)
        .single()
        .expect("fixture completed_at should be valid");
    let run_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture run id parses");
    let artifact_uid =
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("artifact uid parses");
    let revision_uid =
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("revision uid parses");
    let session_id =
        Uuid::parse_str("44444444-4444-4444-4444-444444444444").expect("session id parses");

    let summary = procedure_run_summary_from_run(ArtifactRun {
        run_uid: run_id,
        artifact_uid: Some(artifact_uid),
        revision_uid: Some(revision_uid),
        session_id: Some(moa_core::types::identifiers::SessionId(session_id)),
        procedure_ref: "skill://support-flow".to_string(),
        status: ArtifactRunStatus::PendingReview,
        current_node_id: Some("review".to_string()),
        input: serde_json::json!({ "secret": "not in summary" }),
        state: serde_json::json!({ "internal": true }),
        output: Some(serde_json::json!({ "raw": "not in summary" })),
        error: Some("waiting for reviewer".to_string()),
        started_at,
        completed_at: Some(completed_at),
    });

    assert_eq!(summary.run_id, run_id);
    assert_eq!(summary.artifact_uid, Some(artifact_uid));
    assert_eq!(summary.revision_uid, Some(revision_uid));
    assert_eq!(
        summary.session_id,
        Some(moa_core::types::identifiers::SessionId(session_id))
    );
    assert_eq!(summary.procedure_ref, "skill://support-flow");
    assert_eq!(summary.status, "pending_review");
    assert_eq!(summary.current_node_id.as_deref(), Some("review"));
    assert_eq!(summary.started_at, started_at);
    assert_eq!(summary.completed_at, Some(completed_at));
}

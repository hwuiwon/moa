//! Skills service DTO mapping tests.

use chrono::{TimeZone, Utc};
use moa_core::{ActionRuleScope, TenantId, WorkspaceId};
use moa_orchestrator::services::skills::skill_summary_from_skill;
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
        workspace_id: Some(WorkspaceId::new(tenant_id.to_string())),
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
            use_count: 0,
            last_used: None,
            success_rate: 1.0,
            auto_generated: false,
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
    .expect("workspace skill row should map to summary");

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

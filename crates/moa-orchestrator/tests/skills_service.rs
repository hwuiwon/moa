//! Skills service scope-validation and DTO mapping tests.

use chrono::{TimeZone, Utc};
use moa_core::traits::{Identity, IdentityType};
use moa_core::{MemoryScope, UserId, WorkspaceId};
use moa_orchestrator::services::skills::{
    SkillScopeError, checked_import_scope, effective_user_id, skill_summary_from_skill,
};
use moa_skills::{Skill, SkillPackageManifest};
use uuid::Uuid;

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: user_id,
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn agent_identity(agent_id: Uuid, acting_on_behalf_of: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Agent,
        id: agent_id,
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: Some(acting_on_behalf_of),
    }
}

#[test]
fn checked_import_scope_accepts_trusted_user_scope() {
    // Pins: user-scope skill import can only target the trusted caller user in the authorized workspace.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = user_identity(user_id);
    let workspace_id = WorkspaceId::new("workspace-a");

    let scope = checked_import_scope(
        &workspace_id,
        MemoryScope::User {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new(user_id.to_string()),
        },
        &identity,
    )
    .expect("matching user scope should be accepted");

    assert_eq!(
        scope,
        MemoryScope::User {
            workspace_id,
            user_id: UserId::new(user_id.to_string())
        }
    );
}

#[test]
fn checked_import_scope_accepts_agent_delegated_user_scope() {
    // Pins: agent imports may target only the user carried by trusted delegation headers.
    let agent_id =
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("fixture agent id parses");
    let acting_user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = agent_identity(agent_id, acting_user_id);
    let workspace_id = WorkspaceId::new("workspace-a");

    assert_eq!(
        effective_user_id(&identity),
        Some(UserId::new(acting_user_id.to_string()))
    );
    let scope = checked_import_scope(
        &workspace_id,
        MemoryScope::User {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new(acting_user_id.to_string()),
        },
        &identity,
    )
    .expect("delegated user scope should be accepted");

    assert_eq!(
        scope,
        MemoryScope::User {
            workspace_id,
            user_id: UserId::new(acting_user_id.to_string())
        }
    );
}

#[test]
fn checked_import_scope_rejects_workspace_mismatch() {
    // Pins: skill import cannot authorize one workspace and write a different workspace scope.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = user_identity(user_id);

    let error = checked_import_scope(
        &WorkspaceId::new("workspace-a"),
        MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-b"),
        },
        &identity,
    )
    .expect_err("workspace mismatch should be rejected");

    assert_eq!(
        error,
        SkillScopeError::WorkspaceMismatch {
            request_workspace_id: WorkspaceId::new("workspace-a"),
            scope_workspace_id: WorkspaceId::new("workspace-b"),
        }
    );
}

#[test]
fn checked_import_scope_rejects_user_mismatch() {
    // Pins: caller payloads cannot impersonate another user's skill scope.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let requested = UserId::new("22222222-2222-2222-2222-222222222222");
    let identity = user_identity(user_id);

    let error = checked_import_scope(
        &WorkspaceId::new("workspace-a"),
        MemoryScope::User {
            workspace_id: WorkspaceId::new("workspace-a"),
            user_id: requested.clone(),
        },
        &identity,
    )
    .expect_err("mismatched user scope should be rejected");

    assert_eq!(
        error,
        SkillScopeError::UserMismatch {
            requested,
            effective: user_id.to_string(),
        }
    );
}

#[test]
fn checked_import_scope_accepts_global_scope_after_authz() {
    // Pins: global skill import preserves the former deployment-wide scope once the handler has authorized it.
    let identity = user_identity(Uuid::new_v4());

    let scope = checked_import_scope(
        &WorkspaceId::new("workspace-a"),
        MemoryScope::Global,
        &identity,
    )
    .expect("global import scope should pass validation");

    assert_eq!(scope, MemoryScope::Global);
}

#[test]
fn skill_summary_from_skill_preserves_visible_row_fields() {
    // Pins: list responses expose scope, version, identity, tags, and timestamps from registry rows.
    let now = Utc
        .with_ymd_and_hms(2026, 6, 8, 0, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let skill_uid =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture skill id parses");
    let summary = skill_summary_from_skill(Skill {
        skill_uid,
        workspace_id: Some(WorkspaceId::new("workspace-a")),
        user_id: None,
        scope: "workspace".to_string(),
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
            files: Vec::new(),
        },
        version: 2,
        previous_skill_uid: None,
        tags: vec!["oauth".to_string(), "auth".to_string()],
        valid_to: None,
        created_at: now,
        updated_at: now,
    })
    .expect("workspace skill row should map to summary");

    assert_eq!(summary.skill_uid, skill_uid);
    assert_eq!(
        summary.scope,
        MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-a")
        }
    );
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

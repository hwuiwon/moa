use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactFile, StoredArtifactRevision};
use moa_artifacts::skill::SkillActionKind;
use moa_skills::artifact::{
    SKILL_ARTIFACT_PATH, skill_artifact_document_from_package, skill_definition_from_package,
    skill_definition_to_package, skill_package_from_artifact_revision,
};
use moa_skills::package::{SkillPackage, SkillPackageFile};
use uuid::Uuid;

const SKILL_MD: &str = r#"---
name: refund-helper
description: "Refund support helper"
allowed-tools: bash file_read
metadata:
  moa-tags: "support, refunds"
---

# Refund helper

Use policy snippets to decide when a refund applies.
"#;

#[test]
fn skill_md_only_package_converts_to_minimal_skill_artifact() {
    // Pins: old skill packages remain valid artifact-backed skills.
    let package = SkillPackage::from_skill_markdown(SKILL_MD.to_string())
        .validate()
        .expect("valid skill package");
    let definition = skill_definition_from_package(&package).expect("skill definition");

    assert_eq!(definition.instructions.path, "SKILL.md");
    assert_eq!(definition.allowed_tools, vec!["bash", "file_read"]);
    assert!(definition.actions.is_empty());
    assert!(definition.connectors.is_empty());
    assert_eq!(package.manifest.artifact_schema_version, "moa.artifact/v1");
    assert_eq!(package.manifest.artifact_kind, "skill");
}

#[test]
fn skill_moa_yaml_extends_manifest_with_actions_and_connectors() {
    // Pins: UI-authored skill action metadata survives package validation.
    let package = SkillPackage::new(vec![
        SkillPackageFile::new("SKILL.md", SKILL_MD.as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
        SkillPackageFile::new(
            SKILL_ARTIFACT_PATH,
            br#"
inputs:
  type: object
outputs:
  type: object
connectors:
  - connector://payments
allowed_tools:
  - file_read
actions:
  - id: issue_refund
    description: Issue the approved refund.
    kind: connector_action
    ref: action://payments.issue_refund
ui:
  label: Refund helper
"#
            .to_vec(),
        )
        .with_content_type("application/yaml; charset=utf-8"),
    ])
    .validate()
    .expect("valid artifact-backed skill package");

    let definition = skill_definition_from_package(&package).expect("skill definition");
    assert_eq!(
        definition.connectors,
        vec![ArtifactRef::connector("payments")]
    );
    assert_eq!(definition.allowed_tools, vec!["file_read"]);
    assert_eq!(definition.actions.len(), 1);
    assert_eq!(definition.actions[0].id, "issue_refund");
    assert_eq!(definition.actions[0].kind, SkillActionKind::ConnectorAction);
    assert_eq!(package.manifest.actions[0].id, "issue_refund");
    assert_eq!(
        package.manifest.connectors,
        vec![ArtifactRef::connector("payments")]
    );
}

#[test]
fn skill_definition_to_package_replaces_skill_moa_yaml() {
    // Pins: code-authored skill definitions can round-trip back into package files.
    let original = SkillPackage::new(vec![
        SkillPackageFile::new("SKILL.md", SKILL_MD.as_bytes().to_vec()),
        SkillPackageFile::new(SKILL_ARTIFACT_PATH, b"allowed_tools: [old]".to_vec()),
    ])
    .validate()
    .expect("valid original package");
    let mut definition = skill_definition_from_package(&original).expect("definition");
    definition.allowed_tools = vec!["bash".to_string()];

    let package = skill_definition_to_package(
        &definition,
        vec![
            SkillPackageFile::new("SKILL.md", SKILL_MD.as_bytes().to_vec()),
            SkillPackageFile::new(SKILL_ARTIFACT_PATH, b"allowed_tools: [old]".to_vec()),
        ],
    )
    .expect("package from definition")
    .validate()
    .expect("generated package validates");
    let artifact_files = package
        .files
        .iter()
        .filter(|file| file.path == SKILL_ARTIFACT_PATH)
        .collect::<Vec<_>>();

    assert_eq!(artifact_files.len(), 1);
    assert_eq!(package.manifest.allowed_tools, vec!["bash"]);
}

#[test]
fn executable_skill_artifact_revision_files_convert_to_skill_package() {
    // Pins: ready and superseded skill artifacts remain materializable, while
    // rollback-archived package bytes are audit-only and non-executable.
    let original = SkillPackage::from_skill_markdown(SKILL_MD.to_string())
        .validate()
        .expect("valid skill package");
    let document = skill_artifact_document_from_package(&original, ArtifactStatus::Ready)
        .expect("skill artifact document");
    let now = moa_test_support::fixtures::pg_now();
    let tenant_id = uuid::Uuid::now_v7();
    let mut revision = StoredArtifactRevision {
        artifact_uid: Uuid::now_v7(),
        revision_uid: Uuid::now_v7(),
        storage_partition_id: Some(
            moa_core::types::identifiers::StoragePartitionId::for_tenant(
                moa_core::types::identifiers::TenantId::from(tenant_id),
            ),
        ),
        user_id: None,
        scope: "tenant".to_string(),
        kind: ArtifactKind::Skill,
        name: "refund-helper".to_string(),
        description: "Refund support helper".to_string(),
        tags: vec!["support".to_string()],
        document,
        canonical_hash: Vec::new(),
        source_format: "yaml".to_string(),
        source_text: Vec::new(),
        status: ArtifactStatus::Ready,
        validation_report: serde_json::json!({}),
        version: 1,
        published_at: Some(now),
        valid_to: None,
        created_at: now,
        updated_at: now,
    };
    let files = vec![ArtifactFile {
        file_uid: Uuid::now_v7(),
        path: "SKILL.md".to_string(),
        content: SKILL_MD.as_bytes().to_vec(),
        content_sha256: Vec::new(),
        content_type: Some("text/markdown; charset=utf-8".to_string()),
        executable: false,
        file_size_bytes: SKILL_MD.len() as i64,
    }];

    let package = skill_package_from_artifact_revision(&revision, files.clone())
        .expect("package from artifact revision")
        .validate()
        .expect("converted package validates");

    assert_eq!(package.name, "refund-helper");
    assert_eq!(package.files.len(), 2);
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == SKILL_ARTIFACT_PATH)
    );

    revision.status = ArtifactStatus::Superseded;
    skill_package_from_artifact_revision(&revision, files.clone())
        .expect("superseded exact skill package remains executable");

    revision.status = ArtifactStatus::Archived;
    let error = skill_package_from_artifact_revision(&revision, files)
        .expect_err("archived exact skill package must be non-executable");
    assert_eq!(
        error.to_string(),
        format!(
            "artifact revision {} is archived and is not executable skill content",
            revision.revision_uid
        )
    );
}

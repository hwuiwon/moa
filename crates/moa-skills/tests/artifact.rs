use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::skill::SkillActionKind;
use moa_skills::artifact::{
    SKILL_ARTIFACT_PATH, skill_definition_from_package, skill_definition_to_package,
};
use moa_skills::package::{SkillPackage, SkillPackageFile};

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

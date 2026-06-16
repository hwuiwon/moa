//! Conversion between legacy skill packages and canonical skill artifacts.

use moa_artifacts::document::{
    ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactMetadata, ArtifactStatus,
    ArtifactUi,
};
use moa_artifacts::skill::{SkillDefinition, SkillInstructionSource};
use moa_core::{MoaError, Result};
use serde_json::Value;

use crate::format::SkillDocument;
use crate::package::{
    SKILL_MD_PATH, SkillPackage, SkillPackageFile, ValidatedSkillPackage, ValidatedSkillPackageFile,
};

/// Optional package file containing canonical skill artifact metadata.
pub const SKILL_ARTIFACT_PATH: &str = "skill.moa.yaml";

/// Builds a canonical skill definition from a validated skill package.
pub fn skill_definition_from_package(package: &ValidatedSkillPackage) -> Result<SkillDefinition> {
    skill_definition_from_parts(&package.document, &package.files)
}

/// Builds a canonical artifact document from a validated skill package.
pub fn skill_artifact_document_from_package(
    package: &ValidatedSkillPackage,
    status: ArtifactStatus,
) -> Result<ArtifactDocument> {
    Ok(ArtifactDocument {
        api_version: "moa.artifact/v1".to_string(),
        kind: ArtifactKind::Skill,
        metadata: ArtifactMetadata {
            name: package.name.clone(),
            description: package.description.clone(),
            tags: package.tags.clone(),
            version: Some(package.document.frontmatter.version()),
        },
        status,
        definition: ArtifactDefinition::Skill(skill_definition_from_package(package)?),
        ui: ArtifactUi::default(),
        reference_resolutions: Vec::new(),
    })
}

/// Builds a skill package from a canonical definition and package files.
pub fn skill_definition_to_package(
    definition: &SkillDefinition,
    files: Vec<SkillPackageFile>,
) -> Result<SkillPackage> {
    let instruction_path = if definition.instructions.path.trim().is_empty() {
        SKILL_MD_PATH
    } else {
        definition.instructions.path.as_str()
    };
    if !files.iter().any(|file| file.path == instruction_path) {
        return Err(MoaError::ValidationError(format!(
            "skill package must contain instruction file `{instruction_path}`"
        )));
    }

    let mut output = files
        .into_iter()
        .filter(|file| file.path != SKILL_ARTIFACT_PATH)
        .collect::<Vec<_>>();
    let artifact_yaml = serde_yaml::to_string(definition)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    output.push(
        SkillPackageFile::new(SKILL_ARTIFACT_PATH, artifact_yaml.into_bytes())
            .with_content_type("application/yaml; charset=utf-8"),
    );
    Ok(SkillPackage::new(output))
}

pub(crate) fn skill_definition_from_parts(
    document: &SkillDocument,
    files: &[ValidatedSkillPackageFile],
) -> Result<SkillDefinition> {
    let mut definition =
        if let Some(file) = files.iter().find(|file| file.path == SKILL_ARTIFACT_PATH) {
            parse_skill_artifact_file(file)?
        } else {
            SkillDefinition {
                instructions: SkillInstructionSource {
                    path: SKILL_MD_PATH.to_string(),
                },
                inputs: empty_object(),
                outputs: empty_object(),
                actions: Vec::new(),
                connectors: Vec::new(),
                allowed_tools: document.frontmatter.allowed_tools.clone(),
                ui: empty_object(),
            }
        };

    if definition.instructions.path.trim().is_empty() {
        definition.instructions.path = SKILL_MD_PATH.to_string();
    }
    if definition.allowed_tools.is_empty() {
        definition.allowed_tools = document.frontmatter.allowed_tools.clone();
    }
    Ok(definition)
}

fn parse_skill_artifact_file(file: &ValidatedSkillPackageFile) -> Result<SkillDefinition> {
    let text = std::str::from_utf8(&file.content).map_err(|error| {
        MoaError::ValidationError(format!("{SKILL_ARTIFACT_PATH} must be UTF-8: {error}"))
    })?;
    let value: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|error| MoaError::ValidationError(format!("{SKILL_ARTIFACT_PATH}: {error}")))?;
    if yaml_mapping_contains(&value, "definition") {
        let document = ArtifactDocument::from_yaml(text).map_err(|error| {
            MoaError::ValidationError(format!("{SKILL_ARTIFACT_PATH}: {error}"))
        })?;
        let ArtifactDefinition::Skill(definition) = document.definition else {
            return Err(MoaError::ValidationError(format!(
                "{SKILL_ARTIFACT_PATH} must contain a skill definition"
            )));
        };
        return Ok(definition);
    }

    serde_yaml::from_value(value)
        .map_err(|error| MoaError::ValidationError(format!("{SKILL_ARTIFACT_PATH}: {error}")))
}

fn yaml_mapping_contains(value: &serde_yaml::Value, key: &str) -> bool {
    value
        .as_mapping()
        .map(|mapping| mapping.contains_key(serde_yaml::Value::String(key.to_string())))
        .unwrap_or(false)
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

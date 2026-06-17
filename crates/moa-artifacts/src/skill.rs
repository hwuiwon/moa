//! Skill artifact definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{document::empty_object, reference::ArtifactRef};

/// Location of the instruction body used for a skill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillInstructionSource {
    /// Package-relative path to the Markdown instruction body.
    #[serde(default = "default_skill_path")]
    pub path: String,
}

impl Default for SkillInstructionSource {
    fn default() -> Self {
        Self {
            path: default_skill_path(),
        }
    }
}

/// Canonical reusable skill definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillDefinition {
    /// Instruction source, normally `SKILL.md`.
    #[serde(default)]
    pub instructions: SkillInstructionSource,
    /// JSON schema for skill inputs.
    #[serde(default = "empty_object")]
    pub inputs: Value,
    /// JSON schema for skill outputs.
    #[serde(default = "empty_object")]
    pub outputs: Value,
    /// Callable actions exposed by the skill.
    #[serde(default)]
    pub actions: Vec<SkillActionDefinition>,
    /// Connector definitions this skill expects.
    #[serde(default)]
    pub connectors: Vec<ArtifactRef>,
    /// Built-in or MCP tools the skill may use.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Runtime style for a skill action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActionKind {
    /// Delegates to a connector action reference.
    ConnectorAction,
    /// Delegates to a built-in or MCP tool.
    Tool,
    /// Runs code packaged with the skill.
    Code,
}

/// Callable action exposed by a skill.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkillActionDefinition {
    /// Stable action identifier within the skill.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
    /// Action implementation style.
    pub kind: SkillActionKind,
    /// Optional artifact or tool reference backing the action.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<ArtifactRef>,
    /// Optional runtime label for code actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Optional package-relative entrypoint for code actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// JSON schema for action inputs.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// JSON schema for action outputs.
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl SkillActionDefinition {
    /// Returns whether the action points at a connector action.
    #[must_use]
    pub fn uses_connector_action(&self) -> bool {
        matches!(self.artifact_ref, Some(ArtifactRef::Action { .. }))
    }
}

fn default_skill_path() -> String {
    "SKILL.md".to_string()
}

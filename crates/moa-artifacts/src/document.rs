//! Top-level artifact document envelope.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connector::ConnectorDefinition;
use crate::reference::{ArtifactRef, ReferenceResolution};
use crate::simulation::ExperimentPlanDefinition;
use crate::skill::SkillDefinition;
use crate::workflow::WorkflowDefinition;
use crate::{Error, Result};

/// Artifact family stored in the canonical artifact registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Reusable instructions, resources, and optional callable actions.
    Skill,
    /// Connector and action declaration.
    Connector,
    /// Declarative workflow graph.
    Workflow,
    /// Standalone action declaration.
    Action,
    /// Experiment matrix and budget plan for behavior-lab runs.
    ExperimentPlan,
}

impl ArtifactKind {
    /// Returns the lowercase persisted label for this artifact kind.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Connector => "connector",
            Self::Workflow => "workflow",
            Self::Action => "action",
            Self::ExperimentPlan => "experiment_plan",
        }
    }
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "skill" => Ok(Self::Skill),
            "connector" => Ok(Self::Connector),
            "workflow" => Ok(Self::Workflow),
            "action" => Ok(Self::Action),
            "experiment_plan" => Ok(Self::ExperimentPlan),
            _ => Err(Error::InvalidReference {
                reference: value.to_string(),
                message: "unsupported artifact kind".to_string(),
            }),
        }
    }
}

/// Lifecycle status for an artifact revision.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Editable artifact that may contain unresolved references.
    #[default]
    Draft,
    /// Runtime-visible artifact revision.
    Published,
    /// Hidden artifact revision retained for history.
    Archived,
}

impl ArtifactStatus {
    /// Returns the lowercase database label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Published => "published",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for ArtifactStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ArtifactStatus {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(Self::Draft),
            "published" => Ok(Self::Published),
            "archived" => Ok(Self::Archived),
            _ => Err(Error::InvalidReference {
                reference: value.to_string(),
                message: "unsupported artifact status".to_string(),
            }),
        }
    }
}

/// Human and API metadata shared by all artifact kinds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactMetadata {
    /// Stable user-facing artifact name within a scope and kind.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
    /// Optional tag set for search and filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Optional author-facing semantic version.
    #[serde(default)]
    pub version: Option<String>,
}

/// UI metadata that can survive code-to-canvas round-trips.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactUi {
    /// Optional display label override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional icon identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Builder-owned layout data.
    #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
    pub layout: Value,
}

impl Default for ArtifactUi {
    fn default() -> Self {
        Self {
            label: None,
            icon: None,
            layout: empty_object(),
        }
    }
}

/// Kind-specific artifact definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "spec", rename_all = "snake_case")]
pub enum ArtifactDefinition {
    /// Skill artifact body.
    Skill(SkillDefinition),
    /// Connector artifact body.
    Connector(ConnectorDefinition),
    /// Workflow artifact body.
    Workflow(WorkflowDefinition),
    /// Behavior-lab experiment plan body.
    ExperimentPlan(ExperimentPlanDefinition),
}

impl ArtifactDefinition {
    /// Returns the top-level artifact kind represented by this definition.
    #[must_use]
    pub fn kind(&self) -> ArtifactKind {
        match self {
            Self::Skill(_) => ArtifactKind::Skill,
            Self::Connector(_) => ArtifactKind::Connector,
            Self::Workflow(_) => ArtifactKind::Workflow,
            Self::ExperimentPlan(_) => ArtifactKind::ExperimentPlan,
        }
    }

    /// Returns every static reference declared by this definition.
    #[must_use]
    pub fn references(&self) -> Vec<ArtifactRef> {
        self.reference_paths()
            .into_iter()
            .map(|(_, artifact_ref)| artifact_ref)
            .collect()
    }

    /// Returns every static reference declared by this definition with its document path.
    #[must_use]
    pub fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        match self {
            Self::Skill(definition) => {
                let mut refs = definition
                    .connectors
                    .iter()
                    .enumerate()
                    .map(|(index, artifact_ref)| {
                        (
                            format!("definition.spec.connectors[{index}]"),
                            artifact_ref.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                refs.extend(
                    definition
                        .allowed_tools
                        .iter()
                        .enumerate()
                        .map(|(index, tool)| {
                            (
                                format!("definition.spec.allowed_tools[{index}]"),
                                ArtifactRef::tool(tool.clone()),
                            )
                        }),
                );
                refs.extend(
                    definition
                        .actions
                        .iter()
                        .enumerate()
                        .filter_map(|(index, action)| {
                            action.artifact_ref.clone().map(|artifact_ref| {
                                (
                                    format!("definition.spec.actions[{index}].ref"),
                                    artifact_ref,
                                )
                            })
                        }),
                );
                refs
            }
            Self::Connector(_) => Vec::new(),
            Self::ExperimentPlan(definition) => definition.reference_paths(),
            Self::Workflow(definition) => {
                let mut refs = Vec::new();
                for (node_index, node) in definition.nodes.iter().enumerate() {
                    if let Some(artifact_ref) = &node.artifact_ref {
                        refs.push((
                            format!("definition.spec.nodes[{node_index}].ref"),
                            artifact_ref.clone(),
                        ));
                    }
                    refs.extend(node.skill_refs.iter().enumerate().map(
                        |(ref_index, artifact_ref)| {
                            (
                                format!(
                                    "definition.spec.nodes[{node_index}].skill_refs[{ref_index}]"
                                ),
                                artifact_ref.clone(),
                            )
                        },
                    ));
                    refs.extend(node.tool_refs.iter().enumerate().map(
                        |(ref_index, artifact_ref)| {
                            (
                                format!(
                                    "definition.spec.nodes[{node_index}].tool_refs[{ref_index}]"
                                ),
                                artifact_ref.clone(),
                            )
                        },
                    ));
                }
                refs
            }
        }
    }
}

/// Canonical artifact document imported from JSON/YAML or stored in Postgres.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactDocument {
    /// Schema version for the document envelope.
    #[serde(default = "default_api_version")]
    pub api_version: String,
    /// Top-level artifact kind.
    pub kind: ArtifactKind,
    /// Metadata common to all artifact kinds.
    pub metadata: ArtifactMetadata,
    /// Draft/published/archive status for this revision.
    #[serde(default)]
    pub status: ArtifactStatus,
    /// Kind-specific artifact definition.
    pub definition: ArtifactDefinition,
    /// UI builder metadata for the top-level artifact.
    #[serde(default)]
    pub ui: ArtifactUi,
    /// Reference resolution results produced by validation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reference_resolutions: Vec<ReferenceResolution>,
}

impl ArtifactDocument {
    /// Parses an artifact document from JSON text.
    pub fn from_json(input: &str) -> Result<Self> {
        Ok(serde_json::from_str(input)?)
    }

    /// Parses an artifact document from YAML text.
    pub fn from_yaml(input: &str) -> Result<Self> {
        Ok(serde_yaml::from_str(input)?)
    }

    /// Serializes this document to pretty JSON text.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Serializes this document to YAML text.
    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Returns every static reference declared by this document.
    #[must_use]
    pub fn references(&self) -> Vec<ArtifactRef> {
        self.definition.references()
    }

    /// Returns every static reference declared by this document with its document path.
    #[must_use]
    pub fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        self.definition.reference_paths()
    }
}

fn default_api_version() -> String {
    "moa.artifact/v1".to_string()
}

pub(crate) fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

pub(crate) fn is_empty_object(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.is_empty())
}

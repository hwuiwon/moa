//! Top-level artifact document envelope.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action::ActionDefinition;
use crate::agent::AgentDefinition;
use crate::connector::ConnectorDefinition;
use crate::reference::{ArtifactRef, ReferenceResolution};
use crate::simulation::ExperimentPlanDefinition;
use crate::skill::SkillDefinition;
use crate::{Error, Result};

/// Artifact family stored in the canonical artifact registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Tenant-configurable agent behavior policy.
    Agent,
    /// Reusable instructions, resources, and optional callable actions.
    Skill,
    /// Connector and action declaration.
    Connector,
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
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Connector => "connector",
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
            "agent" => Ok(Self::Agent),
            "skill" => Ok(Self::Skill),
            "connector" => Ok(Self::Connector),
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
///
/// For release-gated kinds (skill, action, agent) this is the candidate
/// lifecycle, and none of its states serve: a session resolves the type-owned
/// serving pointer, never a status. [`Self::Ready`] means "evaluated and
/// activatable", not "visible". The artifact release-control schema makes
/// [`Self::Published`] unrepresentable for those kinds.
///
/// [`Self::Published`] survives only for kinds whose activation seam is owned
/// elsewhere: a connector catalog snapshot is activated by the platform, and an
/// experiment plan is evaluation configuration rather than served behavior. The
/// same trigger makes the candidate states unrepresentable for those kinds, so
/// the two state spaces cannot be mixed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    /// Immutable candidate that has not been submitted for evaluation.
    #[default]
    Draft,
    /// A release attempt holds this artifact's active run slot.
    Evaluating,
    /// Deterministic evidence passed the gate; activation may be requested.
    Ready,
    /// Deterministic assertions failed for this revision.
    Rejected,
    /// Evidence was incomplete or the gate could not resolve; retryable.
    Inconclusive,
    /// Replaced by a newer candidate or activation.
    Superseded,
    /// Hidden artifact revision retained for history.
    Archived,
    /// Validated revision of a kind whose activation seam is owned elsewhere.
    Published,
}

impl ArtifactStatus {
    /// Returns the lowercase database label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Evaluating => "evaluating",
            Self::Ready => "ready",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
            Self::Published => "published",
        }
    }

    /// Returns whether every declared reference must resolve at this status.
    ///
    /// A candidate is evaluated as the exact thing that would serve, so the
    /// activatable and platform-validated statuses both demand resolution; the
    /// non-serving candidate states do not.
    #[must_use]
    pub fn requires_resolved_references(&self) -> bool {
        matches!(self, Self::Ready | Self::Published)
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
            "evaluating" => Ok(Self::Evaluating),
            "ready" => Ok(Self::Ready),
            "rejected" => Ok(Self::Rejected),
            "inconclusive" => Ok(Self::Inconclusive),
            "superseded" => Ok(Self::Superseded),
            "archived" => Ok(Self::Archived),
            "published" => Ok(Self::Published),
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
#[allow(
    clippy::large_enum_variant,
    reason = "the execution-plan contract requires SkillDefinition to own its optional plan"
)]
pub enum ArtifactDefinition {
    /// Tenant-configurable agent artifact body.
    Agent(Box<AgentDefinition>),
    /// Skill artifact body.
    Skill(SkillDefinition),
    /// Connector artifact body.
    Connector(ConnectorDefinition),
    /// Standalone action artifact body.
    Action(ActionDefinition),
    /// Behavior-lab experiment plan body.
    ExperimentPlan(ExperimentPlanDefinition),
}

impl ArtifactDefinition {
    /// Returns the top-level artifact kind represented by this definition.
    #[must_use]
    pub fn kind(&self) -> ArtifactKind {
        match self {
            Self::Agent(_) => ArtifactKind::Agent,
            Self::Skill(_) => ArtifactKind::Skill,
            Self::Connector(_) => ArtifactKind::Connector,
            Self::Action(_) => ArtifactKind::Action,
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
            Self::Agent(definition) => definition.reference_paths(),
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
                if let Some(execution_plan) = &definition.execution_plan {
                    refs.extend(
                        execution_plan.skill_reference_paths("definition.spec.execution_plan"),
                    );
                }
                refs
            }
            Self::Connector(_) => Vec::new(),
            Self::Action(definition) => definition.reference_paths(),
            Self::ExperimentPlan(definition) => definition.reference_paths(),
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

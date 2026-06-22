//! Stable references between artifacts, connector actions, and tools.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result, document::ArtifactKind};

/// A stable, code-addressable reference to another building block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactRef {
    /// A published artifact revision selected by kind and name.
    Artifact {
        /// Artifact family stored in the registry.
        kind: ArtifactKind,
        /// Stable artifact name in the visible scope set.
        name: String,
    },
    /// A callable connector action, written as `action://connector.action`.
    Action {
        /// Connector artifact name.
        connector: String,
        /// Action identifier declared by the connector.
        action: String,
    },
    /// A built-in tool or MCP tool name.
    Tool {
        /// Tool identifier.
        name: String,
    },
}

impl ArtifactRef {
    /// Builds an artifact reference for a specific registry kind.
    #[must_use]
    pub fn artifact(kind: ArtifactKind, name: impl Into<String>) -> Self {
        Self::Artifact {
            kind,
            name: name.into(),
        }
    }

    /// Builds a skill reference.
    #[must_use]
    pub fn skill(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Skill, name)
    }

    /// Builds an agent reference.
    #[must_use]
    pub fn agent(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Agent, name)
    }

    /// Builds a standalone action artifact reference.
    #[must_use]
    pub fn action_artifact(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Action, name)
    }

    /// Builds a connector-action reference.
    #[must_use]
    pub fn action(connector: impl Into<String>, action: impl Into<String>) -> Self {
        Self::Action {
            connector: connector.into(),
            action: action.into(),
        }
    }

    /// Builds a workflow reference.
    #[must_use]
    pub fn workflow(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Workflow, name)
    }

    /// Builds a connector reference.
    #[must_use]
    pub fn connector(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Connector, name)
    }

    /// Builds an experiment plan reference.
    #[must_use]
    pub fn experiment_plan(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::ExperimentPlan, name)
    }

    /// Builds a tool reference.
    #[must_use]
    pub fn tool(name: impl Into<String>) -> Self {
        Self::Tool { name: name.into() }
    }

    /// Returns the artifact kind when this reference points at the registry.
    #[must_use]
    pub const fn artifact_kind(&self) -> Option<&ArtifactKind> {
        match self {
            Self::Artifact { kind, .. } => Some(kind),
            Self::Action { .. } | Self::Tool { .. } => None,
        }
    }

    /// Returns the primary target name for diagnostics and lookup.
    #[must_use]
    pub fn target_name(&self) -> &str {
        match self {
            Self::Artifact { name, .. } | Self::Tool { name } => name,
            Self::Action { connector, .. } => connector,
        }
    }

    /// Returns the connector action name when this is an action reference.
    #[must_use]
    pub fn action_name(&self) -> Option<&str> {
        match self {
            Self::Action { action, .. } => Some(action),
            Self::Artifact { .. } | Self::Tool { .. } => None,
        }
    }

    /// Returns the URI scheme used when this reference is displayed.
    #[must_use]
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Artifact { kind, .. } => kind.as_str(),
            Self::Action { .. } => "action",
            Self::Tool { .. } => "tool",
        }
    }
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact { kind, name } => write!(formatter, "{kind}://{name}"),
            Self::Action { connector, action } => {
                write!(formatter, "action://{connector}.{action}")
            }
            Self::Tool { name } => write!(formatter, "tool://{name}"),
        }
    }
}

impl FromStr for ArtifactRef {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (scheme, rest) = value
            .split_once("://")
            .ok_or_else(|| invalid_ref(value, "missing URI scheme"))?;
        if rest.is_empty() {
            return Err(invalid_ref(value, "missing reference target"));
        }

        match scheme {
            "action" => {
                let Some((connector, action)) = rest.split_once('.') else {
                    return Ok(Self::action_artifact(rest));
                };
                if connector.is_empty() || action.is_empty() {
                    return Err(invalid_ref(
                        value,
                        "action connector and action must be non-empty",
                    ));
                }
                Ok(Self::action(connector, action))
            }
            "tool" => Ok(Self::tool(rest)),
            _ => ArtifactKind::from_str(scheme).map(|kind| Self::artifact(kind, rest)),
        }
    }
}

impl Serialize for ArtifactRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ArtifactRef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Resolution state for references found in an artifact document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceState {
    /// The target exists in the publish-visible scope set.
    Resolved,
    /// The target does not exist yet.
    Unresolved,
}

/// A validation-time reference resolution entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceResolution {
    /// JSON-ish path of the field containing the reference.
    pub path: String,
    /// Reference that was checked.
    #[serde(rename = "ref")]
    pub artifact_ref: ArtifactRef,
    /// Whether the reference resolved.
    pub state: ReferenceState,
    /// Optional human-readable detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ReferenceResolution {
    /// Builds an unresolved reference entry.
    #[must_use]
    pub fn unresolved(path: impl Into<String>, artifact_ref: ArtifactRef) -> Self {
        Self {
            path: path.into(),
            artifact_ref,
            state: ReferenceState::Unresolved,
            message: None,
        }
    }

    /// Builds a resolved reference entry.
    #[must_use]
    pub fn resolved(path: impl Into<String>, artifact_ref: ArtifactRef) -> Self {
        Self {
            path: path.into(),
            artifact_ref,
            state: ReferenceState::Resolved,
            message: None,
        }
    }
}

fn invalid_ref(reference: &str, message: &str) -> Error {
    Error::InvalidReference {
        reference: reference.to_string(),
        message: message.to_string(),
    }
}

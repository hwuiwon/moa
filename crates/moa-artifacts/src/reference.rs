//! Stable references between skills, connectors, tools, and workflows.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

/// Artifact family addressed by an [`ArtifactRef`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactRefKind {
    /// A reusable skill package.
    Skill,
    /// A callable connector action, written as `action://connector.action`.
    Action,
    /// A reusable workflow definition.
    Workflow,
    /// A connector definition.
    Connector,
    /// A built-in tool or MCP tool name.
    Tool,
}

impl ArtifactRefKind {
    /// Returns the URI scheme used for this reference kind.
    #[must_use]
    pub fn scheme(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Action => "action",
            Self::Workflow => "workflow",
            Self::Connector => "connector",
            Self::Tool => "tool",
        }
    }
}

/// A stable, code-addressable reference to another building block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRef {
    /// Kind of referenced artifact.
    pub kind: ArtifactRefKind,
    /// Name of the referenced artifact, connector, or tool.
    pub target: String,
    /// Optional action name for `action://connector.action` references.
    pub action: Option<String>,
}

impl ArtifactRef {
    /// Builds a skill reference.
    #[must_use]
    pub fn skill(name: impl Into<String>) -> Self {
        Self {
            kind: ArtifactRefKind::Skill,
            target: name.into(),
            action: None,
        }
    }

    /// Builds a connector-action reference.
    #[must_use]
    pub fn action(connector: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            kind: ArtifactRefKind::Action,
            target: connector.into(),
            action: Some(action.into()),
        }
    }

    /// Builds a workflow reference.
    #[must_use]
    pub fn workflow(name: impl Into<String>) -> Self {
        Self {
            kind: ArtifactRefKind::Workflow,
            target: name.into(),
            action: None,
        }
    }

    /// Builds a connector reference.
    #[must_use]
    pub fn connector(name: impl Into<String>) -> Self {
        Self {
            kind: ArtifactRefKind::Connector,
            target: name.into(),
            action: None,
        }
    }

    /// Builds a tool reference.
    #[must_use]
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            kind: ArtifactRefKind::Tool,
            target: name.into(),
            action: None,
        }
    }
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ArtifactRefKind::Action => {
                let action = self.action.as_deref().unwrap_or_default();
                write!(formatter, "action://{}.{}", self.target, action)
            }
            _ => write!(formatter, "{}://{}", self.kind.scheme(), self.target),
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
            "skill" => Ok(Self::skill(rest)),
            "workflow" => Ok(Self::workflow(rest)),
            "connector" => Ok(Self::connector(rest)),
            "tool" => Ok(Self::tool(rest)),
            "action" => {
                let (connector, action) = rest.split_once('.').ok_or_else(|| {
                    invalid_ref(value, "action references must be connector.action")
                })?;
                if connector.is_empty() || action.is_empty() {
                    return Err(invalid_ref(
                        value,
                        "action connector and action must be non-empty",
                    ));
                }
                Ok(Self::action(connector, action))
            }
            _ => Err(invalid_ref(value, "unsupported URI scheme")),
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
    /// Creates an unresolved reference resolution entry.
    #[must_use]
    pub fn unresolved(path: impl Into<String>, artifact_ref: ArtifactRef) -> Self {
        Self {
            path: path.into(),
            artifact_ref,
            state: ReferenceState::Unresolved,
            message: None,
        }
    }

    /// Creates a resolved reference resolution entry.
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

fn invalid_ref(reference: &str, message: impl Into<String>) -> Error {
    Error::InvalidReference {
        reference: reference.to_string(),
        message: message.into(),
    }
}

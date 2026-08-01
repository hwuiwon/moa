//! Stable references between artifacts, connector actions, and tools.

use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result, document::ArtifactKind};

/// A stable, code-addressable reference to another building block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactRef {
    /// A visible artifact revision selected by kind and name.
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
    /// Returns the exact canonical string or rejects an internally invalid reference.
    pub fn canonical_string(&self) -> Result<String> {
        let (scheme, target) = match self {
            Self::Artifact { kind, name } => {
                if *kind == ArtifactKind::Action && name.contains('.') {
                    return Err(invalid_ref(
                        name,
                        "standalone action artifact target may not contain a dot",
                    ));
                }
                (kind.as_str(), name.as_str())
            }
            Self::Action { connector, action } => {
                validate_target_component(connector, "connector")?;
                validate_target_component(action, "action")?;
                if connector.contains('.') {
                    return Err(invalid_ref(
                        &format!("action://{connector}.{action}"),
                        "connector action connector may not contain a dot",
                    ));
                }
                let target = format!("{connector}.{action}");
                validate_target(&target)?;
                return Ok(format!("action://{target}"));
            }
            Self::Tool { name } => ("tool", name.as_str()),
        };
        validate_target(target)?;
        Ok(format!("{scheme}://{target}"))
    }

    /// Builds an artifact reference for a specific registry kind.
    #[must_use]
    pub fn artifact(kind: ArtifactKind, name: impl Into<String>) -> Self {
        Self::Artifact {
            kind,
            name: name.into(),
        }
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

    /// Builds a connector reference.
    #[must_use]
    pub fn connector(name: impl Into<String>) -> Self {
        Self::artifact(ArtifactKind::Connector, name)
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
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let canonical = self.canonical_string().map_err(|_| fmt::Error)?;
        formatter.write_str(&canonical)
    }
}

impl FromStr for ArtifactRef {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (scheme, rest) = value
            .split_once("://")
            .ok_or_else(|| invalid_ref(value, "missing URI scheme"))?;
        let candidate = match scheme {
            "action" => {
                let Some((connector, action)) = rest.split_once('.') else {
                    return canonical_round_trip(value, Self::action_artifact(rest));
                };
                if connector.is_empty() || action.is_empty() {
                    return Err(invalid_ref(
                        value,
                        "action connector and action must be non-empty",
                    ));
                }
                Self::action(connector, action)
            }
            "tool" => Self::tool(rest),
            _ => Self::artifact(ArtifactKind::from_str(scheme)?, rest),
        };
        canonical_round_trip(value, candidate)
    }
}

impl Serialize for ArtifactRef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let canonical = self.canonical_string().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&canonical)
    }
}

impl JsonSchema for ArtifactRef {
    fn schema_name() -> Cow<'static, str> {
        "ArtifactRef".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::ArtifactRef").into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        let target = r"[A-Za-z0-9](?:[A-Za-z0-9_.~:/@#-]{0,510}[A-Za-z0-9])?";
        let no_dot_target = r"[A-Za-z0-9](?:[A-Za-z0-9_~:/@#-]{0,510}[A-Za-z0-9])?";
        json_schema!({
            "type": "string",
            "oneOf": [
                {
                    "pattern": format!(
                        r"^(?:agent|skill|connector|experiment_plan|tool)://(?!.*://){target}$"
                    ),
                    "maxLength": 530
                },
                {
                    "pattern": format!(r"^action://(?!.*://){no_dot_target}$"),
                    "maxLength": 521
                },
                {
                    "pattern": format!(
                        r"^action://(?!.*://)(?=.{{1,512}}$){no_dot_target}\.{target}$"
                    ),
                    "maxLength": 521
                }
            ]
        })
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

fn canonical_round_trip(value: &str, candidate: ArtifactRef) -> Result<ArtifactRef> {
    let canonical = candidate.canonical_string()?;
    if canonical != value {
        return Err(invalid_ref(
            value,
            "reference is not byte-identical canonical text",
        ));
    }
    Ok(candidate)
}

fn validate_target(target: &str) -> Result<()> {
    if target.is_empty() {
        return Err(invalid_ref(target, "missing reference target"));
    }
    if target.len() > 512 {
        return Err(invalid_ref(
            target,
            "reference target exceeds 512 UTF-8 bytes",
        ));
    }
    if target.contains("://") {
        return Err(invalid_ref(
            target,
            "reference target contains an embedded URI scheme",
        ));
    }
    let bytes = target.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(invalid_ref(
            target,
            "reference target must start and end with an ASCII alphanumeric byte",
        ));
    }
    if bytes.iter().any(|byte| {
        !byte.is_ascii_alphanumeric()
            && !matches!(byte, b'_' | b'-' | b'.' | b'~' | b':' | b'/' | b'@' | b'#')
    }) {
        return Err(invalid_ref(
            target,
            "reference target contains a noncanonical byte",
        ));
    }
    Ok(())
}

fn validate_target_component(component: &str, label: &str) -> Result<()> {
    validate_target(component).map_err(|_| {
        invalid_ref(
            component,
            &format!("connector action {label} is not canonical"),
        )
    })
}

//! Connector and connector action artifact definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::document::empty_object;

/// Connector declaration with callable actions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ConnectorDefinition {
    /// Authentication or setup metadata.
    #[serde(default = "empty_object")]
    pub auth: Value,
    /// Callable actions exposed by the connector.
    #[serde(default)]
    pub actions: Vec<ConnectorActionDefinition>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Callable action exposed by a connector.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ConnectorActionDefinition {
    /// Stable action identifier within the connector.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
    /// Optional internal tool name used to dispatch the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// JSON schema for action inputs.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// JSON schema for action outputs.
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    /// Whether this action requires approval before execution.
    #[serde(default)]
    pub approval_required: bool,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

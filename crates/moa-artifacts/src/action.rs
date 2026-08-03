//! Standalone action artifact definitions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{document::empty_object, reference::ArtifactRef};

/// Tenant-authored callable action that can be versioned independently.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActionDefinition {
    /// Stable action identifier inside the artifact.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
    /// Optional connector artifact that owns the backing capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_ref: Option<ArtifactRef>,
    /// Built-in or MCP tool name used to execute the action.
    ///
    /// A connector-backed action must name this backing tool explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// JSON schema for action inputs.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// JSON schema for action outputs.
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    /// Whether this action should be routed through tenant-admin review.
    #[serde(default)]
    pub admin_review_required: bool,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl ActionDefinition {
    /// Returns every static reference declared by this action definition.
    #[must_use]
    pub fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        let mut refs = Vec::new();
        if let Some(connector_ref) = &self.connector_ref {
            refs.push((
                "definition.spec.connector_ref".to_string(),
                connector_ref.clone(),
            ));
        }
        if let Some(tool_name) = &self.tool_name {
            refs.push((
                "definition.spec.tool_name".to_string(),
                ArtifactRef::tool(tool_name.clone()),
            ));
        }
        refs
    }
}

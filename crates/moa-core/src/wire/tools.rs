//! Tool descriptor wire DTOs.

use crate::*;
use serde::{Deserialize, Serialize};

/// Public metadata returned by `ToolExecutor/list_tools`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Stable tool name.
    pub name: String,
    /// Human-readable tool description.
    pub description: String,
    /// JSON schema for the tool input.
    pub schema: serde_json::Value,
    /// Declared retry/idempotency contract for the tool.
    pub idempotency_class: IdempotencyClass,
    /// Risk level assigned to this tool.
    pub risk_level: RiskLevel,
    /// Policy/audit class assigned to this tool.
    pub action_class: ActionClass,
}

/// Builds the public descriptor for one registered tool definition.
pub fn tool_descriptor(definition: ToolDefinition) -> ToolDescriptor {
    ToolDescriptor {
        name: definition.name,
        description: definition.description,
        schema: definition.schema,
        idempotency_class: definition.idempotency_class,
        risk_level: definition.policy.risk_level,
        action_class: definition.policy.action_class,
    }
}

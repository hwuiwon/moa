//! Capabilities catalog wire DTOs.
//!
//! The tenant-admin procedure builder renders skill procedures as numbered
//! steps and offers an `@`-mention dropdown of everything a step can attach to.
//! These DTOs describe that read-only catalog: the merged set of built-in
//! tools, connector/action artifacts, skill actions, graph-memory operations,
//! and knowledge datasources visible to a tenant.

use crate::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Family of building block a procedure step can attach to.
///
/// Declaration order is the catalog sort order for the `Datasource`-last
/// dropdown grouping; the derived [`Ord`] relies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// A built-in agent or hand tool (e.g. `bash`, `spawn_worker`, `run_procedure`).
    Tool,
    /// A callable connector action or standalone action artifact.
    ConnectorAction,
    /// A callable action declared by a skill artifact.
    SkillAction,
    /// A graph-memory read or write operation.
    Memory,
    /// A tenant knowledge datasource connection.
    Datasource,
}

/// One attachable capability surfaced in the procedure-builder dropdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    /// Family this capability belongs to.
    pub kind: CapabilityKind,
    /// Human-readable display name shown in the dropdown.
    pub name: String,
    /// Stable attachment reference a `ProcedureNode` can carry.
    ///
    /// The form depends on `kind`: a bare tool name for tools, `action://<name>`
    /// for standalone action artifacts, `action://<connector>.<action>` for
    /// connector actions, `skill://<skill>#<action_id>` for skill actions,
    /// `memory_read`/`memory_write` for memory operations, and the connection
    /// identifier for datasources.
    pub reference: String,
    /// Short human-readable description of what the capability does.
    pub description: String,
    /// JSON schema for the capability input, when it accepts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// Short provenance string, e.g. `builtin`, `artifact`, or `knowledge_connection`.
    pub source: String,
}

/// Request payload for listing a tenant's attachable capabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesListRequest {
    /// Tenant whose visible capabilities should be listed.
    pub tenant_id: TenantId,
}

/// Response payload containing the merged, sorted capability catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesListResponse {
    /// Capabilities sorted by kind then name for deterministic rendering.
    #[serde(default)]
    pub capabilities: Vec<CapabilityEntry>,
}

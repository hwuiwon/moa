//! Procedure graph definitions.
//!
//! A procedure is the optional deterministic graph carried by a skill
//! definition. The graph is authored here and interpreted by the pure procedure
//! interpreter in `moa-skills`; skills without a procedure are purely
//! agent-mediated.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{document::empty_object, reference::ArtifactRef};

/// Declarative procedure graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcedureDefinition {
    /// JSON schema for procedure inputs.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// JSON schema for persisted procedure state.
    #[serde(default = "empty_object")]
    pub state_schema: Value,
    /// Graph nodes in builder order.
    #[serde(default)]
    pub nodes: Vec<ProcedureNode>,
    /// Directed graph edges.
    #[serde(default)]
    pub edges: Vec<ProcedureEdge>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Node in a procedure graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcedureNode {
    /// Stable node identifier.
    pub id: String,
    /// Runtime node kind.
    pub kind: ProcedureNodeKind,
    /// Optional artifact/tool reference for action or agent nodes.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<ArtifactRef>,
    /// Skill references pinned for an agent node.
    #[serde(default)]
    pub skill_refs: Vec<ArtifactRef>,
    /// Tool references pinned for an agent or action node.
    #[serde(default)]
    pub tool_refs: Vec<ArtifactRef>,
    /// Optional condition evaluated by this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<ProcedureCondition>,
    /// Static node input or template metadata.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// Maximum autonomous turns for agent nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Directed transition between two procedure nodes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcedureEdge {
    /// Optional stable edge identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Source node ID.
    pub from: String,
    /// Destination node ID.
    pub to: String,
    /// Optional edge condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<ProcedureCondition>,
}

/// Supported procedure node kinds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcedureNodeKind {
    /// Initialize the procedure run state.
    Start,
    /// Dispatch a connector action or tool-backed action.
    Action,
    /// Evaluate a condition against procedure state.
    Condition,
    /// Wait for a tenant-admin review decision.
    Review,
    /// Dispatch to the existing autonomous agent loop.
    Agent,
    /// Persist terminal output and stop execution.
    End,
    /// Direct tool invocation node.
    Tool,
    /// Skill-declared action invocation node.
    SkillAction,
    /// Worker invocation node.
    Worker,
    /// Parallel branch fan-out.
    Parallel,
    /// Parallel branch join.
    Join,
    /// Wait for an external signal.
    WaitSignal,
    /// Read from memory.
    MemoryRead,
    /// Write to memory.
    MemoryWrite,
}

/// Typed condition expression for nodes and edges.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcedureCondition {
    /// Compare a state/input path to an expected value.
    Equals {
        /// JSON-path-like left-hand side.
        left: String,
        /// Expected JSON value.
        right: Value,
    },
    /// Check that a state/input path exists.
    Exists {
        /// JSON-path-like state/input path.
        path: String,
    },
    /// Escape hatch for future expression languages.
    Expression {
        /// Expression language identifier.
        language: String,
        /// Expression source text.
        source: String,
    },
}

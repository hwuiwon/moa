//! Tenant-configurable agent runtime policy types.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::guardrails::AgentGuardrailPolicy;

/// Built-in global default agent revision identifier.
pub const SYSTEM_DEFAULT_AGENT_REVISION_UID: Uuid =
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0a02);
/// Built-in global default agent reference.
pub const SYSTEM_DEFAULT_AGENT_REF: &str = "agent://system-default";
/// Built-in global default agent policy hash.
pub const SYSTEM_DEFAULT_AGENT_POLICY_HASH: &str = "system-default-agent-v1";

/// Agent revision selection accepted by strict agent session creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionSelection {
    /// Installed-agent deployment pointer to resolve and pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_uid: Option<Uuid>,
    /// Exact published agent revision for simulation or explicit preview sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_uid: Option<Uuid>,
}

/// Exact artifact revision selected by an agent policy lock.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResolvedArtifactRevisionRef {
    /// Original symbolic reference, such as `skill://refund-policy`.
    pub reference: String,
    /// Artifact kind resolved from the reference.
    pub kind: String,
    /// Stable artifact name resolved from the reference.
    pub name: String,
    /// Stable artifact row identifier.
    pub artifact_uid: Uuid,
    /// Exact immutable revision identifier.
    pub revision_uid: Uuid,
    /// Artifact-local revision version.
    pub version: i32,
}

/// Exact built-in, hand, or MCP tool dependency selected by an agent policy lock.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LockedToolRef {
    /// Stable tool name used in model-visible schemas and tool calls.
    pub name: String,
    /// Stable schema or catalog hash used for replay and audit.
    pub schema_hash: String,
    /// Optional provider or catalog namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Reproducibility lock for one agent revision.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentRevisionLock {
    /// Exact agent artifact revision selected for execution.
    pub agent_revision_uid: Uuid,
    /// Exact artifact dependency revisions selected for execution.
    #[serde(default)]
    pub artifact_dependencies: Vec<ResolvedArtifactRevisionRef>,
    /// Exact tool catalog entries selected for execution.
    #[serde(default)]
    pub tool_dependencies: Vec<LockedToolRef>,
    /// Stable hash over the canonical runtime policy snapshot.
    pub canonical_policy_hash: String,
}

/// Skill filtering mode resolved from an agent definition.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSkillPolicyMode {
    /// Visible skills can be discovered and ranked automatically.
    #[default]
    Auto,
    /// Only listed skills are eligible.
    Allowlist,
    /// Listed skills are always included before ranking.
    Pinned,
    /// Listed skills are never eligible.
    Denylist,
}

/// Memory scope mode resolved from an agent definition.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKnowledgeScopeMode {
    /// Retrieve tenant knowledge and admitted contact memory.
    #[default]
    Enabled,
    /// Disable graph memory retrieval for this agent.
    Disabled,
}

/// Runtime model policy copied onto a pinned agent session.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentModelPolicy {
    /// Default model id for sessions using this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Explicit model ids this agent may use.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Optional fallback model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
}

/// Runtime knowledge policy copied onto a pinned agent session.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentKnowledgePolicy {
    /// Scope mode used by graph-memory retrieval.
    #[serde(default)]
    pub mode: AgentKnowledgeScopeMode,
    /// Optional retrieval filters interpreted by the memory stage.
    #[serde(default)]
    pub filters: Value,
    /// Optional maximum final hit count for memory retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_budget: Option<u32>,
    /// Optional minimum PII handling floor label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pii_floor: Option<String>,
}

impl Default for AgentKnowledgePolicy {
    fn default() -> Self {
        Self {
            mode: AgentKnowledgeScopeMode::Enabled,
            filters: Value::Object(Default::default()),
            retrieval_budget: None,
            pii_floor: None,
        }
    }
}

/// Runtime skill policy copied onto a pinned agent session.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSkillPolicy {
    /// Skill selection mode.
    #[serde(default)]
    pub mode: AgentSkillPolicyMode,
    /// Symbolic skill references from the authoring document.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Maximum number of skills the context stage may expose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_visible: Option<u32>,
}

/// Runtime action policy copied onto a pinned agent session.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentActionPolicy {
    /// Action references the agent may use.
    #[serde(default)]
    pub allowed: Vec<String>,
    /// Actions that require administrator review.
    #[serde(default)]
    pub require_admin_review: Vec<String>,
}

/// Tool filtering mode resolved from an agent definition.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolPolicyMode {
    /// All registered tools are eligible except explicit denies.
    #[default]
    Auto,
    /// Only listed tools are eligible.
    Allowlist,
    /// All registered tools are eligible except listed tools.
    Denylist,
}

/// Runtime tool policy for a pinned agent session.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentToolPolicy {
    /// Tool filtering mode.
    pub mode: AgentToolPolicyMode,
    /// Explicitly allowed or pinned tool names.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Explicitly denied tool names.
    #[serde(default)]
    pub denied_tools: Vec<String>,
}

impl AgentToolPolicy {
    /// Returns whether this policy allows a tool call by name.
    #[must_use]
    pub fn allows(&self, tool_name: &str) -> bool {
        if self.denied_tools.iter().any(|name| name == tool_name) {
            return false;
        }
        match self.mode {
            AgentToolPolicyMode::Auto | AgentToolPolicyMode::Denylist => true,
            AgentToolPolicyMode::Allowlist => self.tools.iter().any(|name| name == tool_name),
        }
    }
}

/// Compact policy snapshot copied onto a session when an agent revision is selected.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentPolicySnapshot {
    /// Stable instructions injected by the configured agent.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// Runtime model defaults and restrictions.
    #[serde(default)]
    pub model_policy: AgentModelPolicy,
    /// Runtime graph-memory policy.
    #[serde(default)]
    pub knowledge_policy: AgentKnowledgePolicy,
    /// Runtime skill visibility and pinning policy.
    #[serde(default)]
    pub skill_policy: AgentSkillPolicy,
    /// Runtime action visibility and review policy.
    #[serde(default)]
    pub action_policy: AgentActionPolicy,
    /// Runtime tool filter.
    #[serde(default)]
    pub tool_policy: AgentToolPolicy,
    /// Runtime input and output guardrail policy.
    #[serde(default)]
    pub guardrail_policy: AgentGuardrailPolicy,
    /// Reproducibility lock used for this session or simulation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_lock: Option<AgentRevisionLock>,
}

/// Agent context pinned onto a durable session.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentContext {
    /// Optional agent principal used for delegation or API-key identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    /// Installed-agent pointer selected at session creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_uid: Option<Uuid>,
    /// Deployment row selected at session creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_uid: Option<Uuid>,
    /// Stable agent artifact reference, such as `agent://support-triage`.
    pub definition_ref: String,
    /// Exact agent artifact revision pinned for this session.
    pub revision_uid: Uuid,
    /// Stable hash over the runtime policy snapshot.
    pub policy_hash: String,
    /// User-facing configured-agent display name.
    pub display_name: String,
    /// Exact artifact dependency revisions copied from the lock.
    #[serde(default)]
    pub artifact_dependencies: Vec<ResolvedArtifactRevisionRef>,
    /// Exact tool dependencies copied from the lock.
    #[serde(default)]
    pub tool_dependencies: Vec<LockedToolRef>,
    /// Serialized policy snapshot used by runtime gates and replay.
    #[serde(default)]
    pub policy_snapshot: Value,
}

impl AgentContext {
    /// Returns the built-in default agent context used when tests or internal callers need
    /// an explicit agent relation without tenant-authored policy.
    #[must_use]
    pub fn system_default() -> Self {
        let revision_lock = AgentRevisionLock {
            agent_revision_uid: SYSTEM_DEFAULT_AGENT_REVISION_UID,
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            canonical_policy_hash: SYSTEM_DEFAULT_AGENT_POLICY_HASH.to_string(),
        };
        let snapshot = AgentPolicySnapshot {
            instructions: Vec::new(),
            model_policy: AgentModelPolicy::default(),
            knowledge_policy: AgentKnowledgePolicy::default(),
            skill_policy: AgentSkillPolicy::default(),
            action_policy: AgentActionPolicy::default(),
            tool_policy: AgentToolPolicy::default(),
            guardrail_policy: AgentGuardrailPolicy::default(),
            revision_lock: Some(revision_lock),
        };
        Self {
            agent_id: None,
            installation_uid: None,
            deployment_uid: None,
            definition_ref: SYSTEM_DEFAULT_AGENT_REF.to_string(),
            revision_uid: SYSTEM_DEFAULT_AGENT_REVISION_UID,
            policy_hash: SYSTEM_DEFAULT_AGENT_POLICY_HASH.to_string(),
            display_name: "MOA Default Agent".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::json!(snapshot),
        }
    }

    /// Returns whether this context is the built-in default agent placeholder.
    #[must_use]
    pub fn is_system_default(&self) -> bool {
        self.definition_ref == SYSTEM_DEFAULT_AGENT_REF
            && self.revision_uid == SYSTEM_DEFAULT_AGENT_REVISION_UID
            && self.policy_hash == SYSTEM_DEFAULT_AGENT_POLICY_HASH
    }

    /// Parses the typed policy snapshot copied onto this session.
    pub fn parsed_policy_snapshot(&self) -> crate::error::Result<AgentPolicySnapshot> {
        if self.policy_snapshot.is_null() {
            return Ok(AgentPolicySnapshot::default());
        }
        serde_json::from_value(self.policy_snapshot.clone())
            .map_err(|error| crate::error::MoaError::SerializationError(error.to_string()))
    }

    /// Returns whether this pinned context allows a tool call by name.
    pub fn allows_tool(&self, tool_name: &str) -> crate::error::Result<bool> {
        Ok(self.parsed_policy_snapshot()?.tool_policy.allows(tool_name))
    }
}

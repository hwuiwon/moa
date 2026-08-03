//! Tenant-configurable agent runtime policy types.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::guardrails::AgentGuardrailPolicy;
use super::hands::{BuiltinPolicyRevision, SandboxPolicySnapshot, SandboxProfile};
use super::identifiers::ConnectorConnectionId;
use super::memory::{InformationBarrierClearances, InformationBarrierId};

/// Built-in global default agent revision identifier.
pub const SYSTEM_DEFAULT_AGENT_REVISION_UID: Uuid =
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0a02);
/// Built-in global default agent reference.
pub const SYSTEM_DEFAULT_AGENT_REF: &str = "agent://system-default";
/// Built-in global default agent policy hash.
pub const SYSTEM_DEFAULT_AGENT_POLICY_HASH: &str = "system-default-agent-v1";

/// Agent selection shared by serving admission and eval-owned preview execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionSelection {
    /// Installed-agent deployment pointer to resolve and pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_uid: Option<Uuid>,
    /// Exact agent revision for internal evaluation preview sessions only.
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
    ///
    /// For a connector tool this is the server-qualified reference, not the name
    /// the server publishes, because that is the name the model is shown and the
    /// only one that identifies the tool unambiguously across connectors.
    pub name: String,
    /// Hash over this dependency's name and provider namespace.
    ///
    /// This is a tool *identity*, not a schema pin, and the distinction is
    /// load-bearing: two revisions of a tool whose input schema changed
    /// completely produce the same value here, because nothing about the schema
    /// is hashed. Comparing this across agent revisions answers "is this the
    /// same tool", never "is this the same contract".
    ///
    /// Governed contract pinning is deliberately separate from deployment
    /// identity. Conversational completion requests carry the exact catalog pin
    /// paired with their model-visible tools, while durable execution
    /// capabilities carry their own contract revision. Policy evaluation and
    /// dispatch compare those revisions with the immutable catalog snapshot they
    /// use, so changing this identity hash into a second contract pin would only
    /// duplicate the runtime source of truth.
    pub identity_hash: String,
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
    /// Information barriers this agent is cleared for (need-to-know).
    ///
    /// Each entry is a barrier tag the agent's memory retrieval may see. It is
    /// threaded onto the retrieval request and installed as the
    /// `moa.cleared_barriers` GUC so the `rd_barrier_need_to_know` RLS policy
    /// reveals nodes tagged with a cleared barrier. An empty set fails closed:
    /// barriered nodes stay hidden.
    #[serde(default)]
    pub cleared_barriers: InformationBarrierClearances,
    /// Barrier assigned to new memory written by this agent.
    ///
    /// When present, this value must also appear in [`Self::cleared_barriers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_barrier: Option<InformationBarrierId>,
}

impl Default for AgentKnowledgePolicy {
    fn default() -> Self {
        Self {
            mode: AgentKnowledgeScopeMode::Enabled,
            filters: Value::Object(Default::default()),
            retrieval_budget: None,
            pii_floor: None,
            cleared_barriers: InformationBarrierClearances::new(),
            write_barrier: None,
        }
    }
}

impl AgentKnowledgePolicy {
    /// Validates that the write barrier is included in this policy's clearances.
    pub fn validate(&self) -> crate::error::Result<()> {
        if let Some(write_barrier) = &self.write_barrier
            && !self.cleared_barriers.contains(write_barrier)
        {
            return Err(crate::error::MoaError::ValidationError(format!(
                "agent write barrier `{write_barrier}` is not included in cleared barriers"
            )));
        }
        Ok(())
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
    /// Exact installed connector connections selected for logical connector references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connector_bindings: Vec<AgentConnectorBinding>,
}

/// Runtime binding from one logical connector reference to an installed connection.
///
/// The artifact and revision identifiers pin the exact published connector
/// contract the resolver selected. Connection existence and delegated
/// authorization are checked at later runtime boundaries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentConnectorBinding {
    /// Canonical logical connector reference, such as `connector://billing`.
    pub connector_ref: String,
    /// Tenant-installed connection selected for the connector.
    pub connection_id: ConnectorConnectionId,
    /// Stable connector artifact row identifier.
    pub artifact_uid: Uuid,
    /// Exact published connector revision selected by the agent resolver.
    pub revision_uid: Uuid,
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

/// Sandbox policy an agent revision declares for the hands it uses.
///
/// This is the third of the four layers intersected into a sandbox's effective
/// profile. `Unset` is not "no policy": it is the identity element of that
/// intersection, so an agent that declares nothing cannot widen what the
/// deployment, the tenant, or the route already bounded. It still carries a
/// named revision into the policy identity hash, so an agent that later starts
/// declaring limits changes the hash and cannot reuse an old sandbox.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AgentSandboxPolicy {
    /// The agent adds no sandbox restriction of its own.
    #[default]
    Unset,
    /// The agent tightens the sandbox with its own authored profile.
    Declared {
        /// Revision identifying the agent's authored sandbox policy.
        revision: String,
        /// The profile the agent declares.
        profile: SandboxProfile,
    },
}

impl AgentSandboxPolicy {
    /// Builds the policy-layer snapshot this agent contributes.
    ///
    /// [`AgentSandboxPolicy::Unset`] contributes the fully permissive identity
    /// layer, which restricts nothing but is still hash-significant.
    pub fn snapshot(&self) -> crate::error::Result<SandboxPolicySnapshot> {
        match self {
            Self::Unset => Ok(SandboxPolicySnapshot::builtin(
                BuiltinPolicyRevision::AgentUnset,
            )),
            Self::Declared { revision, profile } => {
                SandboxPolicySnapshot::new(revision, profile.clone())
            }
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
    /// Runtime sandbox resource and egress policy layer.
    #[serde(default)]
    pub sandbox_policy: AgentSandboxPolicy,
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
            sandbox_policy: AgentSandboxPolicy::Unset,
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

    /// Returns validated information-barrier clearances bound to this policy revision.
    pub fn information_barrier_clearances(
        &self,
    ) -> crate::error::Result<InformationBarrierClearances> {
        let knowledge_policy = self.parsed_policy_snapshot()?.knowledge_policy;
        knowledge_policy.validate()?;
        Ok(knowledge_policy
            .cleared_barriers
            .with_policy_revision(self.policy_hash.clone()))
    }

    /// Returns whether this pinned context allows a tool call by name.
    pub fn allows_tool(&self, tool_name: &str) -> crate::error::Result<bool> {
        Ok(self.parsed_policy_snapshot()?.tool_policy.allows(tool_name))
    }

    /// Returns the tool names this agent explicitly depends on, without
    /// duplicates and in the order the lock records them.
    ///
    /// These are the agent's own tools plus the tools its pinned skills declare.
    /// The order is not resorted here: the revision lock is already canonically
    /// ordered so its hash is stable, and imposing a second order would only
    /// disagree with the pinned one. The locked list comes first because it is
    /// the replayable set; names appearing only in the tool policy are appended
    /// so an agent that pins tools without a full revision lock is still
    /// honoured.
    ///
    /// A consumer that must reduce a loadout to fit a schema cap keeps these
    /// ahead of undeclared tools: an agent or skill that named a tool cannot do
    /// its job without it, whatever that tool's name happens to sort as.
    #[must_use]
    pub fn declared_tool_names(&self) -> Vec<String> {
        let mut declared = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let policy_tools = self
            .parsed_policy_snapshot()
            .map(|snapshot| snapshot.tool_policy.tools)
            .unwrap_or_default();
        for name in self
            .tool_dependencies
            .iter()
            .map(|locked| locked.name.clone())
            .chain(policy_tools)
        {
            if seen.insert(name.clone()) {
                declared.push(name);
            }
        }
        declared
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{AgentActionPolicy, AgentConnectorBinding};
    use crate::types::identifiers::ConnectorConnectionId;

    #[test]
    fn empty_connector_bindings_preserve_pre_binding_action_policy_json() {
        // Pins: adding connection-aware actions does not change canonical policy
        // bytes or revision locks for agents that declare no connector binding.
        let policy = AgentActionPolicy {
            allowed: vec!["action://legacy".to_string()],
            require_admin_review: vec!["action://legacy".to_string()],
            connector_bindings: Vec::new(),
        };

        let legacy_json = json!({
            "allowed": ["action://legacy"],
            "require_admin_review": ["action://legacy"]
        });

        assert_eq!(
            serde_json::to_value(&policy).expect("runtime action policy should serialize"),
            legacy_json
        );
        let decoded: AgentActionPolicy = serde_json::from_value(legacy_json)
            .expect("pre-binding runtime action policy should deserialize");
        assert_eq!(decoded, policy);
    }

    #[test]
    fn connector_binding_serializes_only_non_secret_revision_identity() {
        // Pins: runtime agent snapshots bind one connection and exact connector
        // revision without carrying credential material or endpoint details.
        let binding = AgentConnectorBinding {
            connector_ref: "connector://billing".to_string(),
            connection_id: ConnectorConnectionId(Uuid::from_u128(0x0c01_1ec7)),
            artifact_uid: Uuid::from_u128(0xa471_fac7),
            revision_uid: Uuid::from_u128(0x2e71_5100),
        };
        let policy = AgentActionPolicy {
            connector_bindings: vec![binding],
            ..AgentActionPolicy::default()
        };

        assert_eq!(
            serde_json::to_value(policy).expect("bound runtime action policy should serialize"),
            json!({
                "allowed": [],
                "require_admin_review": [],
                "connector_bindings": [{
                    "connector_ref": "connector://billing",
                    "connection_id": Uuid::from_u128(0x0c01_1ec7),
                    "artifact_uid": Uuid::from_u128(0xa471_fac7),
                    "revision_uid": Uuid::from_u128(0x2e71_5100)
                }]
            })
        );
    }
}

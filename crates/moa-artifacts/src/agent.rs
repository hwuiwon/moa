//! Tenant-configurable agent artifact definitions.

use moa_core::types::guardrails::GuardrailMode;
use moa_core::types::memory::{InformationBarrierClearances, InformationBarrierId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{document::empty_object, reference::ArtifactRef};

/// Tenant-configurable agent definition stored as an immutable artifact revision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentDefinition {
    /// Human-readable display name shown to admins and users.
    pub display_name: String,
    /// Purpose and expected outcomes for this agent.
    pub purpose: AgentPurpose,
    /// Default and allowed model behavior.
    #[serde(default)]
    pub model_policy: ModelPolicy,
    /// Agent-specific prompt instructions.
    #[serde(default)]
    pub instruction_policy: InstructionPolicy,
    /// Graph memory and retrieval bounds.
    #[serde(default)]
    pub knowledge_policy: KnowledgePolicy,
    /// Skill visibility and pinning policy.
    #[serde(default)]
    pub skill_policy: SkillPolicy,
    /// Action visibility and review policy.
    #[serde(default)]
    pub action_policy: ActionPolicy,
    /// Built-in and MCP tool visibility policy.
    #[serde(default)]
    pub tool_policy: ToolPolicy,
    /// Optional input and output guardrails for this agent.
    #[serde(default)]
    pub guardrail_policy: GuardrailPolicy,
    /// Optional admin-facing revision note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_note: Option<String>,
    /// Builder-owned UI and product metadata.
    #[serde(default = "empty_object")]
    pub metadata: Value,
}

impl AgentDefinition {
    /// Returns every static reference declared by this agent definition.
    #[must_use]
    pub fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        let mut refs = Vec::new();
        refs.extend(self.instruction_policy.reference_paths());
        refs.extend(self.skill_policy.reference_paths());
        refs.extend(self.action_policy.reference_paths());
        refs.extend(self.tool_policy.reference_paths());
        refs
    }
}

/// Purpose and expected outputs for a configured agent.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPurpose {
    /// Concise statement of what the agent is for.
    pub summary: String,
    /// Optional default task framing used when a session starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_task: Option<String>,
    /// Expected output styles or deliverables.
    #[serde(default)]
    pub expected_outputs: Vec<String>,
}

/// Default and allowed model behavior.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelPolicy {
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

/// Prompt instructions owned by the agent definition.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstructionPolicy {
    /// Stable system instructions for this agent.
    #[serde(default)]
    pub system_prompt: String,
    /// Additional ordered instructions.
    #[serde(default)]
    pub instructions: Vec<String>,
    /// Artifact references that provide additional instruction material.
    #[serde(default)]
    pub instruction_refs: Vec<ArtifactRef>,
}

impl InstructionPolicy {
    fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        self.instruction_refs
            .iter()
            .enumerate()
            .map(|(index, artifact_ref)| {
                (
                    format!("definition.spec.instruction_policy.instruction_refs[{index}]"),
                    artifact_ref.clone(),
                )
            })
            .collect()
    }
}

/// Memory visibility mode for a configured agent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeScopeMode {
    /// Retrieve tenant knowledge and admitted contact memory.
    #[default]
    Enabled,
    /// Disable graph memory retrieval for this agent.
    Disabled,
}

/// Graph memory and retrieval bounds.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct KnowledgePolicy {
    /// Scope mode used by memory retrieval.
    #[serde(default)]
    pub mode: KnowledgeScopeMode,
    /// Optional source filters applied by the resolver/runtime.
    #[serde(default = "empty_object")]
    pub filters: Value,
    /// Optional retrieval item budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_budget: Option<u32>,
    /// Optional minimum PII handling floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pii_floor: Option<String>,
    /// Information barriers this agent is cleared for (need-to-know).
    ///
    /// Operator-authored list of barrier tags this agent may see. Each entry
    /// flows to the runtime `AgentKnowledgePolicy.cleared_barriers`, is threaded
    /// onto the retrieval request, and installed as the `moa.cleared_barriers`
    /// GUC so the `rd_barrier_need_to_know` RLS policy reveals nodes tagged with
    /// a cleared barrier. Empty (the default) fails closed: barriered nodes stay
    /// hidden.
    #[serde(default)]
    pub cleared_barriers: InformationBarrierClearances,
    /// Barrier assigned to memory written from sessions pinned to this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_barrier: Option<InformationBarrierId>,
}

impl Default for KnowledgePolicy {
    fn default() -> Self {
        Self {
            mode: KnowledgeScopeMode::Enabled,
            filters: empty_object(),
            retrieval_budget: None,
            pii_floor: None,
            cleared_barriers: InformationBarrierClearances::new(),
            write_barrier: None,
        }
    }
}

/// How a skill policy interprets its ref list.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPolicyMode {
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

/// Skill visibility and pinning policy.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillPolicy {
    /// Selection mode for the referenced skill set.
    #[serde(default)]
    pub mode: SkillPolicyMode,
    /// Referenced skill artifacts.
    #[serde(default)]
    pub refs: Vec<ArtifactRef>,
    /// Maximum number of skill manifests to include.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_visible: Option<u32>,
}

impl SkillPolicy {
    fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        named_ref_paths("skill_policy.refs", &self.refs)
    }
}

/// Action visibility and review policy.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActionPolicy {
    /// Standalone action artifacts or connector actions the agent may use.
    #[serde(default)]
    pub allowed: Vec<ArtifactRef>,
    /// Actions that must always require administrator review.
    #[serde(default)]
    pub require_admin_review: Vec<ArtifactRef>,
}

impl ActionPolicy {
    fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        let mut refs = named_ref_paths("action_policy.allowed", &self.allowed);
        refs.extend(named_ref_paths(
            "action_policy.require_admin_review",
            &self.require_admin_review,
        ));
        refs
    }
}

/// Built-in and MCP tool filtering mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicyMode {
    /// All registered tools are eligible except explicit denies.
    #[default]
    Auto,
    /// Only listed tools are eligible.
    Allowlist,
    /// All registered tools are eligible except listed tools.
    Denylist,
}

/// Built-in and MCP tool visibility policy.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolPolicy {
    /// Selection mode for tool names.
    #[serde(default)]
    pub mode: ToolPolicyMode,
    /// Tool names allowed or pinned by this policy.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tool names explicitly denied by this policy.
    #[serde(default)]
    pub denied_tools: Vec<String>,
}

impl ToolPolicy {
    fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        self.tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                (
                    format!("definition.spec.tool_policy.tools[{index}]"),
                    ArtifactRef::tool(tool.clone()),
                )
            })
            .chain(self.denied_tools.iter().enumerate().map(|(index, tool)| {
                (
                    format!("definition.spec.tool_policy.denied_tools[{index}]"),
                    ArtifactRef::tool(tool.clone()),
                )
            }))
            .collect()
    }
}

/// Optional input and output guardrails configured on an agent artifact.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GuardrailPolicy {
    /// Optional guardrail applied to user text before agent processing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<GuardrailStagePolicy>,
    /// Optional guardrail applied to assistant text before user delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<GuardrailStagePolicy>,
}

/// Authoring policy for one guardrail direction.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GuardrailStagePolicy {
    /// Whether this stage should call the configured judge.
    #[serde(default)]
    pub enabled: bool,
    /// Whether blocking judge results are enforced or only recorded.
    #[serde(default)]
    pub mode: GuardrailMode,
    /// Optional model override for the guardrail judge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Instructions the judge uses to decide whether text passes this guardrail.
    #[serde(default)]
    pub policy_prompt: String,
    /// Optional message returned when an enforced guardrail blocks text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_message: Option<String>,
}

fn named_ref_paths(field: &str, refs: &[ArtifactRef]) -> Vec<(String, ArtifactRef)> {
    refs.iter()
        .enumerate()
        .map(|(index, artifact_ref)| {
            (
                format!("definition.spec.{field}[{index}]"),
                artifact_ref.clone(),
            )
        })
        .collect()
}

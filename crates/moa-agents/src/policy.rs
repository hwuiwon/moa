//! Resolved runtime policy returned by the configured-agent resolver.

use moa_core::{
    AgentActionPolicy, AgentContext, AgentKnowledgePolicy, AgentModelPolicy, AgentRevisionLock,
    AgentSkillPolicy, AgentToolPolicy, AgentWorkflowPolicy,
};

/// Compact deterministic runtime policy for a pinned configured-agent revision.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRuntimePolicy {
    /// Session-ready pinned agent context.
    pub agent_context: AgentContext,
    /// Exact artifact and tool dependency lock.
    pub revision_lock: AgentRevisionLock,
    /// Instructions resolved from the agent definition.
    pub instructions: Vec<String>,
    /// Runtime model defaults and restrictions.
    pub model_policy: AgentModelPolicy,
    /// Runtime graph-memory policy.
    pub knowledge_policy: AgentKnowledgePolicy,
    /// Runtime skill visibility and pinning policy.
    pub skill_policy: AgentSkillPolicy,
    /// Runtime workflow affordance policy.
    pub workflow_policy: AgentWorkflowPolicy,
    /// Runtime action visibility and review policy.
    pub action_policy: AgentActionPolicy,
    /// Runtime tool filter.
    pub tool_policy: AgentToolPolicy,
}

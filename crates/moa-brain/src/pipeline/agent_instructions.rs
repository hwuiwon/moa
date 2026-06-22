//! Stage 2: injects configured-agent instructions pinned to the session.

use std::collections::BTreeMap;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, ContextSourceRef, ProcessorOutput, Result, WorkingContext,
};

use super::estimate_tokens;

const AGENT_CONTEXT_METADATA_KEY: &str = "agent_context";

/// Injects the session-pinned configured-agent instructions into the stable prompt prefix.
#[derive(Debug, Clone, Default)]
pub struct AgentInstructionProcessor;

impl AgentInstructionProcessor {
    /// Creates an agent instruction processor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContextProcessor for AgentInstructionProcessor {
    fn name(&self) -> &str {
        "agent_instructions"
    }

    fn stage(&self) -> u8 {
        2
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let Some(agent_context) = ctx.agent_context.as_ref() else {
            return Ok(ProcessorOutput::default());
        };
        let definition_ref = agent_context.definition_ref.clone();
        let revision_uid = agent_context.revision_uid;
        let policy_hash = agent_context.policy_hash.clone();
        let snapshot = ctx.agent_policy_snapshot()?.unwrap_or_default();
        let instructions = snapshot
            .instructions
            .iter()
            .map(|instruction| instruction.trim())
            .filter(|instruction| !instruction.is_empty())
            .collect::<Vec<_>>();
        let workflow_section =
            workflow_affordance_section(&snapshot.workflow_policy, agent_context);
        ctx.insert_metadata(
            AGENT_CONTEXT_METADATA_KEY,
            serde_json::json!({
                "definition_ref": definition_ref.clone(),
                "revision_uid": revision_uid,
                "policy_hash": policy_hash.clone(),
            }),
        );
        if instructions.is_empty() && workflow_section.is_none() {
            return Ok(ProcessorOutput {
                items_included: vec![definition_ref],
                ..ProcessorOutput::default()
            });
        }

        let mut body = String::new();
        if !instructions.is_empty() {
            body.push_str(&instructions.join("\n\n"));
        }
        if let Some(workflows) = workflow_section {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&workflows);
        }
        let content = format!(
            "<agent_instructions ref=\"{}\" revision_uid=\"{}\" policy_hash=\"{}\">\n{}\n</agent_instructions>",
            definition_ref, revision_uid, policy_hash, body
        );
        let tokens_added = estimate_tokens(&content);
        ctx.append_message(ContextMessage::system(content).with_source_ref(
            ContextSourceRef::synthetic(format!("agent:{}", definition_ref)),
        ));

        Ok(ProcessorOutput {
            tokens_added,
            items_included: vec![definition_ref],
            ..ProcessorOutput::default()
        })
    }
}

fn workflow_affordance_section(
    policy: &moa_core::AgentWorkflowPolicy,
    agent_context: &moa_core::AgentContext,
) -> Option<String> {
    let resolved = resolved_workflow_dependencies(agent_context);
    let allowed = resolved_policy_refs(&policy.allowed, &resolved);
    if allowed.is_empty() {
        return None;
    }

    Some(format!(
        "<agent_workflows>\nAllowed workflows: {}\n</agent_workflows>",
        allowed.join(", ")
    ))
}

fn resolved_workflow_dependencies(
    agent_context: &moa_core::AgentContext,
) -> BTreeMap<String, String> {
    agent_context
        .artifact_dependencies
        .iter()
        .filter(|dependency| dependency.kind == "workflow")
        .map(|dependency| {
            (
                dependency.reference.clone(),
                format!(
                    "{} revision_uid={} version={}",
                    dependency.reference, dependency.revision_uid, dependency.version
                ),
            )
        })
        .collect()
}

fn resolved_policy_refs(references: &[String], resolved: &BTreeMap<String, String>) -> Vec<String> {
    references
        .iter()
        .filter_map(|reference| resolved.get(reference).cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use moa_core::{
        AgentContext, AgentPolicySnapshot, AgentToolPolicy, ContextProcessor, ModelCapabilities,
        ModelId, SYSTEM_DEFAULT_AGENT_REF, SessionMeta, TokenPricing, ToolCallFormat,
        WorkingContext,
    };
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn agent_instruction_processor_injects_session_pinned_instructions() {
        // Pins: configured-agent instructions come from the session pin, not latest artifact state.
        let session = SessionMeta {
            agent_context: Some(agent_context(vec![
                "Handle only refund triage.".to_string(),
                "Escalate payment disputes.".to_string(),
            ])),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());

        let output = AgentInstructionProcessor::new()
            .process(&mut ctx)
            .await
            .expect("agent instructions should compile");

        assert_eq!(ctx.messages.len(), 1);
        assert!(ctx.messages[0].content.contains("<agent_instructions"));
        assert!(
            ctx.messages[0]
                .content
                .contains("Handle only refund triage.")
        );
        assert!(ctx.metadata().contains_key(AGENT_CONTEXT_METADATA_KEY));
        assert_eq!(output.items_included, vec!["agent://support".to_string()]);
        assert!(output.tokens_added > 0);
    }

    #[tokio::test]
    async fn agent_instruction_processor_records_default_agent_without_instructions() {
        // Pins: default sessions are still agent-pinned, but empty default policy adds no prompt text.
        let mut ctx = WorkingContext::new(&SessionMeta::default(), capabilities());

        let output = AgentInstructionProcessor::new()
            .process(&mut ctx)
            .await
            .expect("default agent context should compile");

        assert!(ctx.messages.is_empty());
        assert_eq!(
            output.items_included,
            vec![SYSTEM_DEFAULT_AGENT_REF.to_string()]
        );
        assert!(ctx.metadata().contains_key(AGENT_CONTEXT_METADATA_KEY));
    }

    fn agent_context(instructions: Vec<String>) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            instructions,
            workflow_policy: moa_core::AgentWorkflowPolicy {
                allowed: Vec::new(),
            },
            tool_policy: AgentToolPolicy::default(),
            revision_lock: None,
            ..AgentPolicySnapshot::default()
        };
        AgentContext {
            agent_id: None,
            installation_uid: Some(Uuid::now_v7()),
            deployment_uid: Some(Uuid::now_v7()),
            definition_ref: "agent://support".to_string(),
            revision_uid: Uuid::now_v7(),
            policy_hash: "policy-hash".to_string(),
            display_name: "Support".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }
}

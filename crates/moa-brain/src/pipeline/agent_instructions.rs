//! Stage 2: injects configured-agent instructions pinned to the session.

use async_trait::async_trait;
use moa_core::{
    error::Result, traits::ContextProcessor, types::context::ContextMessage,
    types::context::ContextSourceRef, types::context::ProcessorOutput,
    types::context::WorkingContext, types::context::estimate_text_tokens,
};

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
        ctx.insert_metadata(
            AGENT_CONTEXT_METADATA_KEY,
            serde_json::json!({
                "definition_ref": definition_ref.clone(),
                "revision_uid": revision_uid,
                "policy_hash": policy_hash.clone(),
            }),
        );
        if instructions.is_empty() {
            return Ok(ProcessorOutput {
                items_included: vec![definition_ref],
                ..ProcessorOutput::default()
            });
        }

        let body = instructions.join("\n\n");
        let content = format!(
            "<agent_instructions ref=\"{}\" revision_uid=\"{}\" policy_hash=\"{}\">\n{}\n</agent_instructions>",
            definition_ref, revision_uid, policy_hash, body
        );
        let tokens_added = estimate_text_tokens(&content);
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

#[cfg(test)]
mod tests {
    use moa_core::{
        traits::ContextProcessor, types::agent::AgentContext, types::agent::AgentPolicySnapshot,
        types::agent::AgentToolPolicy, types::agent::SYSTEM_DEFAULT_AGENT_REF,
        types::context::WorkingContext, types::identifiers::ModelId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
        types::session::SessionMeta,
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

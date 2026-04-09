//! Stage 1: injects the static MOA identity prompt.

use moa_core::{ContextProcessor, ProcessorOutput, Result, WorkingContext};

use super::estimate_tokens;

const IDENTITY_PROMPT: &str = "\
You are MOA, a general-purpose AI agent. You help users accomplish tasks by \
reasoning, using tools, and building on accumulated knowledge.\n\n\
You have access to tools for file operations, shell commands, web search, \
and memory management. You can request additional tools if needed.\n\n\
When you make changes, explain what you did and why. When you encounter \
errors, preserve them in context so they are not repeated.";

/// Injects the brain identity prompt into the working context.
#[derive(Debug, Default)]
pub struct IdentityProcessor;

impl ContextProcessor for IdentityProcessor {
    fn name(&self) -> &str {
        "identity"
    }

    fn stage(&self) -> u8 {
        1
    }

    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        ctx.append_system(IDENTITY_PROMPT);
        Ok(ProcessorOutput {
            tokens_added: estimate_tokens(IDENTITY_PROMPT),
            items_included: vec!["moa_identity".to_string()],
            ..ProcessorOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ModelCapabilities, Platform, SessionId, SessionMeta, TokenPricing, ToolCallFormat, UserId,
        WorkspaceId,
    };

    use super::*;

    #[test]
    fn identity_processor_appends_system_prompt() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: "claude-sonnet-4-6".to_string(),
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
            },
        };
        let mut ctx = WorkingContext::new(&session, capabilities);

        let output = IdentityProcessor.process(&mut ctx).unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, moa_core::MessageRole::System);
        assert!(output.tokens_added > 0);
    }
}

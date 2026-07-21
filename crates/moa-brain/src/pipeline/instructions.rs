//! Stage 2: injects configured workspace and user instructions.

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::{
    error::Result, traits::ContextProcessor, types::context::ProcessorOutput,
    types::context::WorkingContext, types::context::estimate_text_tokens,
};

/// Injects optional workspace and user instructions into the prompt.
#[derive(Debug, Clone, Default)]
pub struct InstructionProcessor {
    workspace_instructions: Option<String>,
    user_instructions: Option<String>,
}

impl InstructionProcessor {
    /// Creates an instruction processor from explicit workspace and user sections.
    pub fn new(workspace_instructions: Option<String>, user_instructions: Option<String>) -> Self {
        Self {
            workspace_instructions,
            user_instructions,
        }
    }

    /// Creates an instruction processor from the loaded MOA configuration.
    pub fn from_config(config: &MoaConfig) -> Self {
        Self::new(
            config.general.workspace_instructions.clone(),
            config.general.user_instructions.clone(),
        )
    }
}

#[async_trait]
impl ContextProcessor for InstructionProcessor {
    fn name(&self) -> &str {
        "instructions"
    }

    fn stage(&self) -> u8 {
        2
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let mut sections = Vec::new();
        let mut items_included = Vec::new();

        if let Some(instructions) = &self.workspace_instructions {
            sections.push(format!(
                "<workspace_instructions>\n{instructions}\n</workspace_instructions>"
            ));
            items_included.push("workspace_instructions".to_string());
        }

        if let Some(instructions) = &self.user_instructions {
            sections.push(format!(
                "<user_preferences>\n{instructions}\n</user_preferences>"
            ));
            items_included.push("user_instructions".to_string());
        }

        if sections.is_empty() {
            return Ok(ProcessorOutput::default());
        }

        let content = sections.join("\n\n");
        let tokens_added = estimate_text_tokens(&content);
        ctx.append_system(content);

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use moa_config::GeneralConfig;
    use moa_core::{
        types::channel::Channel, types::identifiers::ModelId, types::identifiers::SessionId,
        types::identifiers::TenantId, types::model::ModelCapabilities, types::model::TokenPricing,
        types::model::ToolCallFormat, types::session::SessionMeta,
    };

    use super::*;

    #[tokio::test]
    async fn instruction_processor_appends_config_backed_sections() {
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
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
        };
        let mut ctx = WorkingContext::new(&session, capabilities);
        let config = MoaConfig {
            general: GeneralConfig {
                workspace_instructions: Some("Follow the repo conventions.".to_string()),
                user_instructions: Some("Keep responses terse.".to_string()),
                ..GeneralConfig::default()
            },
            ..MoaConfig::default()
        };

        let output = InstructionProcessor::from_config(&config)
            .process(&mut ctx)
            .await
            .unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert!(ctx.messages[0].content.contains("<workspace_instructions>"));
        assert!(ctx.messages[0].content.contains("<user_preferences>"));
        assert_eq!(output.items_included.len(), 2);
        assert!(output.tokens_added > 0);
    }
}

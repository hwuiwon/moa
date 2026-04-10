//! Stage 7: validates cache breakpoint ordering and reports cache efficiency.

use super::{estimate_tokens, sort_json_keys};
use async_trait::async_trait;
use moa_core::{ContextProcessor, MoaError, ProcessorOutput, Result, WorkingContext};

/// Final cache verification pass.
#[derive(Debug, Default)]
pub struct CacheOptimizer;

#[async_trait]
impl ContextProcessor for CacheOptimizer {
    fn name(&self) -> &str {
        "cache"
    }

    fn stage(&self) -> u8 {
        7
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let Some(cache_breakpoint) = ctx.cache_breakpoints.last().copied() else {
            return Err(MoaError::ValidationError(
                "cache breakpoint must be set before the cache optimizer runs".to_string(),
            ));
        };
        if cache_breakpoint > ctx.messages.len() {
            return Err(MoaError::ValidationError(
                "cache breakpoint exceeds the number of compiled messages".to_string(),
            ));
        }

        for message in &mut ctx.messages[..cache_breakpoint] {
            if let Some(tools) = &mut message.tools {
                sort_json_keys(tools);
            }
        }

        for schema in ctx.tools_mut() {
            sort_json_keys(schema);
        }

        let prefix_tokens = ctx.messages[..cache_breakpoint]
            .iter()
            .map(|message| estimate_tokens(&message.content))
            .sum::<usize>();
        let total_tokens = ctx.token_count.max(1);
        let cache_ratio = prefix_tokens as f64 / total_tokens as f64;

        tracing::info!(
            cache_breakpoint,
            prefix_tokens,
            total_tokens,
            cache_ratio = %format!("{:.1}%", cache_ratio * 100.0),
            "context cache efficiency"
        );

        Ok(ProcessorOutput::default())
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ContextMessage, ModelCapabilities, Platform, SessionId, SessionMeta, TokenPricing,
        ToolCallFormat, UserId, WorkspaceId,
    };

    use super::*;

    #[tokio::test]
    async fn cache_optimizer_validates_cache_breakpoint() {
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
        ctx.extend_messages(vec![
            ContextMessage::system("identity"),
            ContextMessage::user("hello"),
        ]);
        ctx.mark_cache_breakpoint();

        let output = CacheOptimizer.process(&mut ctx).await.unwrap();

        assert_eq!(output.tokens_added, 0);
        assert_eq!(ctx.cache_breakpoints, vec![2]);
    }
}

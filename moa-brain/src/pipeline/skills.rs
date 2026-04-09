//! Stage 4: injects skill metadata and marks the cache breakpoint.

use moa_core::{ContextProcessor, ProcessorOutput, Result, WorkingContext};

/// Stub skill injector used until the skill registry exists.
#[derive(Debug, Default)]
pub struct SkillInjector;

impl ContextProcessor for SkillInjector {
    fn name(&self) -> &str {
        "skills"
    }

    fn stage(&self) -> u8 {
        4
    }

    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        ctx.mark_cache_breakpoint();
        Ok(ProcessorOutput::default())
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
    fn skill_injector_marks_cache_breakpoint() {
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

        SkillInjector.process(&mut ctx).unwrap();

        assert_eq!(ctx.cache_breakpoints, vec![0]);
    }
}

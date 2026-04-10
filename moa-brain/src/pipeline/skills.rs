//! Stage 4: injects skill metadata and marks the cache breakpoint.

use moa_core::{ContextProcessor, ProcessorOutput, Result, SkillMetadata, WorkingContext};

pub(crate) const SKILLS_STAGE_DATA_METADATA_KEY: &str = "moa.pipeline.skills_stage_data";

/// Injects workspace skill metadata into the stable prompt prefix.
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
        let skills = load_skills(ctx)?;
        let tokens_before = ctx.token_count;
        let mut items_included = Vec::new();

        if !skills.is_empty() {
            let mut section = String::from(
                "<available_skills>\n\
If one of these skills is clearly relevant and you need the exact workflow, call `memory_read` with the listed `path` before acting.\n",
            );
            for skill in &skills {
                section.push_str(&format!(
                    "- {name}: {description} | path: {path} | tags: {tags} | tools: {tools} | est_tokens: {estimated_tokens} | uses: {use_count} | success_rate: {success_rate:.2}\n",
                    name = skill.name,
                    description = skill.description,
                    path = skill.path,
                    tags = skill.tags.join(", "),
                    tools = skill.allowed_tools.join(", "),
                    estimated_tokens = skill.estimated_tokens,
                    use_count = skill.use_count,
                    success_rate = skill.success_rate,
                ));
                items_included.push(skill.name.clone());
            }
            section.push_str("</available_skills>");
            ctx.append_system(section);
        }

        ctx.mark_cache_breakpoint();

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

fn load_skills(ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    match ctx.metadata.get(SKILLS_STAGE_DATA_METADATA_KEY) {
        Some(value) => serde_json::from_value(value.clone()).map_err(Into::into),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ContextProcessor, ModelCapabilities, Platform, SessionId, SessionMeta, SkillMetadata,
        TokenPricing, ToolCallFormat, UserId, WorkspaceId,
    };

    use super::{SKILLS_STAGE_DATA_METADATA_KEY, SkillInjector};

    #[test]
    fn skill_injector_marks_cache_breakpoint_and_formats_metadata() {
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
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities);
        ctx.metadata.insert(
            SKILLS_STAGE_DATA_METADATA_KEY.to_string(),
            serde_json::to_value(vec![SkillMetadata {
                path: "skills/debug-oauth/SKILL.md".into(),
                name: "debug-oauth".to_string(),
                description: "OAuth refresh-token debugging workflow".to_string(),
                tags: vec!["oauth".to_string(), "auth".to_string()],
                allowed_tools: vec!["bash".to_string(), "file_read".to_string()],
                estimated_tokens: 900,
                use_count: 3,
                success_rate: 0.9,
                auto_generated: true,
            }])
            .unwrap(),
        );

        let output = SkillInjector.process(&mut ctx).unwrap();

        assert_eq!(ctx.cache_breakpoints, vec![1]);
        assert!(ctx.messages[0].content.contains("<available_skills>"));
        assert!(ctx.messages[0].content.contains("debug-oauth"));
        assert!(ctx.messages[0].content.contains("memory_read"));
        assert!(
            ctx.messages[0]
                .content
                .contains("skills/debug-oauth/SKILL.md")
        );
        assert!(
            output.tokens_added
                >= crate::pipeline::estimate_tokens("OAuth refresh-token debugging workflow")
        );
        assert_eq!(output.items_included, vec!["debug-oauth"]);
    }

    #[test]
    fn skill_injector_marks_breakpoint_without_skills() {
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
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities);

        let output = SkillInjector.process(&mut ctx).unwrap();

        assert!(ctx.messages.is_empty());
        assert_eq!(ctx.cache_breakpoints, vec![0]);
        assert_eq!(output.tokens_added, 0);
    }
}

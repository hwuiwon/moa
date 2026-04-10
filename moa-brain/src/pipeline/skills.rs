//! Stage 4: injects skill metadata and marks the cache breakpoint.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{ContextProcessor, ProcessorOutput, Result, WorkingContext};
use moa_skills::SkillRegistry;

/// Injects workspace skill metadata into the stable prompt prefix.
pub struct SkillInjector {
    skill_registry: Arc<SkillRegistry>,
}

impl SkillInjector {
    /// Creates a skill injector backed by the shared skill registry.
    pub fn new(skill_registry: Arc<SkillRegistry>) -> Self {
        Self { skill_registry }
    }
}

#[async_trait]
impl ContextProcessor for SkillInjector {
    fn name(&self) -> &str {
        "skills"
    }

    fn stage(&self) -> u8 {
        4
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let skills = self
            .skill_registry
            .list_for_pipeline(&ctx.workspace_id)
            .await?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::{
        ContextProcessor, MemoryPath, MemoryScope, MemoryStore, ModelCapabilities, PageSummary,
        PageType, Platform, Result, SessionId, SessionMeta, TokenPricing, ToolCallFormat, UserId,
        WikiPage, WorkspaceId,
    };
    use moa_skills::SkillRegistry;

    use super::SkillInjector;

    #[derive(Clone)]
    struct StubSkillMemoryStore {
        pages: HashMap<MemoryPath, WikiPage>,
        summaries: Vec<PageSummary>,
    }

    #[async_trait]
    impl MemoryStore for StubSkillMemoryStore {
        async fn search(
            &self,
            _query: &str,
            _scope: MemoryScope,
            _limit: usize,
        ) -> Result<Vec<moa_core::MemorySearchResult>> {
            Ok(Vec::new())
        }

        async fn read_page(&self, _scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
            self.pages
                .get(path)
                .cloned()
                .ok_or_else(|| moa_core::MoaError::StorageError("skill page not found".to_string()))
        }

        async fn write_page(
            &self,
            _scope: MemoryScope,
            _path: &MemoryPath,
            _page: WikiPage,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_page(&self, _scope: MemoryScope, _path: &MemoryPath) -> Result<()> {
            Ok(())
        }

        async fn list_pages(
            &self,
            _scope: MemoryScope,
            _filter: Option<PageType>,
        ) -> Result<Vec<PageSummary>> {
            Ok(self.summaries.clone())
        }

        async fn get_index(&self, _scope: MemoryScope) -> Result<String> {
            Ok(String::new())
        }

        async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn skill_injector_marks_cache_breakpoint_and_formats_metadata() {
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
        let skill_path = MemoryPath::new("skills/debug-oauth/SKILL.md");
        let store = StubSkillMemoryStore {
            pages: HashMap::from([(
                skill_path.clone(),
                WikiPage {
                    path: Some(skill_path.clone()),
                    title: "debug-oauth".to_string(),
                    page_type: PageType::Skill,
                    content: "## When to use\nUse it for OAuth refresh token issues.".to_string(),
                    created: chrono::Utc::now(),
                    updated: chrono::Utc::now(),
                    confidence: moa_core::ConfidenceLevel::High,
                    related: Vec::new(),
                    sources: Vec::new(),
                    tags: vec!["oauth".to_string(), "auth".to_string()],
                    auto_generated: true,
                    last_referenced: chrono::Utc::now(),
                    reference_count: 3,
                    metadata: HashMap::from([
                        ("name".to_string(), serde_json::json!("debug-oauth")),
                        (
                            "description".to_string(),
                            serde_json::json!("OAuth refresh-token debugging workflow"),
                        ),
                        (
                            "allowed-tools".to_string(),
                            serde_json::json!("bash file_read"),
                        ),
                        (
                            "metadata".to_string(),
                            serde_json::json!({
                                "moa-estimated-tokens": "900",
                                "moa-use-count": "3",
                                "moa-success-rate": "0.9",
                                "moa-tags": "oauth, auth",
                            }),
                        ),
                    ]),
                },
            )]),
            summaries: vec![PageSummary {
                path: skill_path,
                title: "debug-oauth".to_string(),
                page_type: PageType::Skill,
                updated: chrono::Utc::now(),
                confidence: moa_core::ConfidenceLevel::High,
            }],
        };
        let registry = Arc::new(SkillRegistry::new(Arc::new(store)));

        let output = SkillInjector::new(registry)
            .process(&mut ctx)
            .await
            .unwrap();

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

    #[tokio::test]
    async fn skill_injector_marks_breakpoint_without_skills() {
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
        let store = StubSkillMemoryStore {
            pages: HashMap::new(),
            summaries: Vec::new(),
        };

        let output = SkillInjector::new(Arc::new(SkillRegistry::new(Arc::new(store))))
            .process(&mut ctx)
            .await
            .unwrap();

        assert!(ctx.messages.is_empty());
        assert_eq!(ctx.cache_breakpoints, vec![0]);
        assert_eq!(output.tokens_added, 0);
    }
}

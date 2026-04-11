//! Stage 4: injects skill metadata and marks the cache breakpoint.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextProcessor, MemoryStore, PageType, ProcessorOutput, Result, SkillMetadata, WorkingContext,
};
use serde_json::Value;

/// Injects workspace skill metadata into the stable prompt prefix.
pub struct SkillInjector {
    memory_store: Arc<dyn MemoryStore>,
}

impl SkillInjector {
    /// Creates a skill injector backed by the shared memory store.
    pub fn new(memory_store: Arc<dyn MemoryStore>) -> Self {
        Self { memory_store }
    }

    /// Creates a skill injector from a memory store.
    pub fn from_memory(memory_store: Arc<dyn MemoryStore>) -> Self {
        Self::new(memory_store)
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
        let skills = load_skills(self.memory_store.as_ref(), &ctx.workspace_id).await?;
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

async fn load_skills(
    memory_store: &dyn MemoryStore,
    workspace_id: &moa_core::WorkspaceId,
) -> Result<Vec<SkillMetadata>> {
    let scope = moa_core::MemoryScope::Workspace(workspace_id.clone());
    let summaries = memory_store
        .list_pages(scope.clone(), Some(PageType::Skill))
        .await?;
    let mut skills = Vec::with_capacity(summaries.len());

    for summary in summaries {
        let page = memory_store.read_page(scope.clone(), &summary.path).await?;
        skills.push(skill_metadata_from_page(summary.path, &page));
    }

    skills.sort_by(|left, right| {
        right
            .use_count
            .cmp(&left.use_count)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(skills)
}

fn skill_metadata_from_page(
    path: moa_core::MemoryPath,
    page: &moa_core::WikiPage,
) -> SkillMetadata {
    SkillMetadata {
        path,
        name: metadata_string(&page.metadata, "name").unwrap_or_else(|| page.title.clone()),
        description: metadata_string(&page.metadata, "description")
            .unwrap_or_else(|| page.title.clone()),
        tags: skill_tags(page),
        allowed_tools: allowed_tools(page),
        estimated_tokens: metadata_nested_usize(&page.metadata, "metadata", "moa-estimated-tokens")
            .unwrap_or_else(|| estimate_skill_tokens(&page.content)),
        use_count: metadata_nested_u32(&page.metadata, "metadata", "moa-use-count")
            .unwrap_or(page.reference_count.min(u64::from(u32::MAX)) as u32),
        success_rate: metadata_nested_f32(&page.metadata, "metadata", "moa-success-rate")
            .unwrap_or(1.0),
        auto_generated: page.auto_generated,
    }
}

fn skill_tags(page: &moa_core::WikiPage) -> Vec<String> {
    if !page.tags.is_empty() {
        return page.tags.clone();
    }

    metadata_nested_csv(&page.metadata, "metadata", "moa-tags")
}

fn allowed_tools(page: &moa_core::WikiPage) -> Vec<String> {
    match page.metadata.get("allowed-tools") {
        Some(Value::String(value)) => value
            .split_whitespace()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn metadata_string(
    metadata: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_nested_string(
    metadata: &std::collections::HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<String> {
    metadata
        .get(container)
        .and_then(Value::as_object)
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_nested_csv(
    metadata: &std::collections::HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Vec<String> {
    metadata_nested_string(metadata, container, key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn metadata_nested_usize(
    metadata: &std::collections::HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<usize> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn metadata_nested_u32(
    metadata: &std::collections::HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<u32> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn metadata_nested_f32(
    metadata: &std::collections::HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<f32> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
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

        let output = SkillInjector::from_memory(Arc::new(store))
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

        let output = SkillInjector::from_memory(Arc::new(store))
            .process(&mut ctx)
            .await
            .unwrap();

        assert_eq!(ctx.cache_breakpoints, vec![0]);
        assert!(ctx.messages.is_empty());
        assert_eq!(output.tokens_added, 0);
        assert!(output.items_included.is_empty());
    }
}

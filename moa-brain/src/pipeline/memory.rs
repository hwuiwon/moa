//! Stage 5: memory retrieval and prompt injection.

use moa_core::{ContextProcessor, Event, EventRecord, ProcessorOutput, Result, WorkingContext};
use serde::{Deserialize, Serialize};

pub(crate) const MEMORY_STAGE_DATA_METADATA_KEY: &str = "moa.pipeline.memory_stage_data";
const MEMORY_BUDGET_DIVISOR: usize = 5;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;

/// Preloaded memory data fetched asynchronously before the stage runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct PreloadedMemoryStageData {
    /// Truncated user-scoped `MEMORY.md`.
    pub user_index: String,
    /// Truncated workspace-scoped `MEMORY.md`.
    pub workspace_index: String,
    /// Relevant retrieved pages for the current turn.
    pub relevant_pages: Vec<RelevantMemoryPage>,
}

/// Single retrieved page snippet prepared for Stage 5 formatting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelevantMemoryPage {
    /// Scope label used in prompt formatting.
    pub scope_label: String,
    /// Logical page path.
    pub path: String,
    /// Human-readable page title.
    pub title: String,
    /// Retrieved page excerpt or fallback search snippet.
    pub excerpt: String,
}

/// Injects scoped memory indexes and relevant page snippets.
#[derive(Debug, Default)]
pub struct MemoryRetriever;

impl ContextProcessor for MemoryRetriever {
    fn name(&self) -> &str {
        "memory"
    }

    fn stage(&self) -> u8 {
        5
    }

    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let data = load_preloaded_memory(ctx)?;
        let tokens_before = ctx.token_count;
        let mut items_included = Vec::new();

        if !data.user_index.trim().is_empty() {
            ctx.append_system(format!(
                "<user_memory>\n{}\n</user_memory>",
                data.user_index.trim()
            ));
            items_included.push("user:MEMORY.md".to_string());
        }

        if !data.workspace_index.trim().is_empty() {
            ctx.append_system(format!(
                "<workspace_memory>\n{}\n</workspace_memory>",
                data.workspace_index.trim()
            ));
            items_included.push("workspace:MEMORY.md".to_string());
        }

        if !data.relevant_pages.is_empty() {
            let memory_budget = (ctx.token_budget / MEMORY_BUDGET_DIVISOR)
                .saturating_sub(ctx.token_count.saturating_sub(tokens_before));
            let per_page_budget =
                (memory_budget / data.relevant_pages.len().max(1)).max(MIN_PAGE_EXCERPT_TOKENS);
            let mut section = String::from("<relevant_memory>\n");

            for page in &data.relevant_pages {
                let excerpt = truncate_excerpt(&page.excerpt, per_page_budget);
                section.push_str(&format!(
                    "## {} [{}:{}]\n{}\n\n",
                    page.title, page.scope_label, page.path, excerpt
                ));
                items_included.push(format!("{}:{}", page.scope_label, page.path));
            }
            section.push_str("</relevant_memory>");

            ctx.append_system(section);
        }

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

pub(crate) fn extract_search_query(events: &[EventRecord]) -> Option<String> {
    let text = events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some(text.as_str()),
        Event::QueuedMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })?;
    let keywords = extract_search_keywords(text);
    if keywords.is_empty() {
        None
    } else {
        Some(keywords.join(" "))
    }
}

pub(crate) fn extract_search_keywords(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "about", "after", "again", "agent", "answer", "around", "because", "before", "being",
        "between", "could", "explain", "find", "from", "have", "into", "just", "like", "make",
        "need", "please", "respond", "should", "that", "the", "their", "them", "there", "these",
        "they", "this", "what", "when", "where", "which", "with", "would", "your",
    ];

    let mut keywords = Vec::new();
    for token in text
        .split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
    {
        let normalized = token.to_ascii_lowercase();
        if STOPWORDS.contains(&normalized.as_str()) || keywords.contains(&normalized) {
            continue;
        }
        keywords.push(normalized);
        if keywords.len() >= 6 {
            break;
        }
    }

    keywords
}

pub(crate) fn load_preloaded_memory(ctx: &WorkingContext) -> Result<PreloadedMemoryStageData> {
    match ctx.metadata.get(MEMORY_STAGE_DATA_METADATA_KEY) {
        Some(value) => serde_json::from_value(value.clone()).map_err(Into::into),
        None => Ok(PreloadedMemoryStageData::default()),
    }
}

fn truncate_excerpt(excerpt: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if excerpt.chars().count() <= max_chars {
        return excerpt.trim().to_string();
    }

    let mut truncated = excerpt.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ContextProcessor, ModelCapabilities, Platform, SessionId, SessionMeta, TokenPricing,
        ToolCallFormat, UserId, WorkspaceId,
    };

    use super::{
        MEMORY_STAGE_DATA_METADATA_KEY, MemoryRetriever, PreloadedMemoryStageData,
        RelevantMemoryPage, extract_search_keywords,
    };

    #[test]
    fn memory_retriever_loads_preloaded_indexes_and_results() {
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
            MEMORY_STAGE_DATA_METADATA_KEY.to_string(),
            serde_json::to_value(PreloadedMemoryStageData {
                user_index: "User prefers concise responses.".to_string(),
                workspace_index: "Workspace uses Rust and libsql.".to_string(),
                relevant_pages: vec![RelevantMemoryPage {
                    scope_label: "workspace".to_string(),
                    path: "topics/storage.md".to_string(),
                    title: "Storage".to_string(),
                    excerpt: "Use libsql for durable session state.".to_string(),
                }],
            })
            .unwrap(),
        );

        let output = MemoryRetriever.process(&mut ctx).unwrap();

        assert_eq!(ctx.messages.len(), 3);
        assert!(ctx.messages[0].content.contains("<user_memory>"));
        assert!(ctx.messages[1].content.contains("<workspace_memory>"));
        assert!(ctx.messages[2].content.contains("<relevant_memory>"));
        assert!(
            output.tokens_added
                >= super::super::estimate_tokens("Use libsql for durable session state.")
        );
    }

    #[test]
    fn keyword_extraction_filters_stopwords_and_duplicates() {
        let keywords =
            extract_search_keywords("Please explain the OAuth refresh token race condition bug");

        assert_eq!(
            keywords,
            vec!["oauth", "refresh", "token", "race", "condition", "bug"]
        );
    }
}

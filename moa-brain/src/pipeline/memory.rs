//! Stage 6: memory retrieval and prompt injection.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, MemoryPath, MemoryScope, MemoryStore, ProcessorOutput,
    QueryRewriteResult, Result, RewriteSource, WorkingContext,
};

const MEMORY_BUDGET_DIVISOR: usize = 5;
const MEMORY_RESULTS_PER_SCOPE: usize = 2;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;
const WORKSPACE_BOOTSTRAP_PAGE_PATH: &str = "topics/project.md";
pub(crate) const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

/// In-memory stage data fetched during processing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct MemoryStageData {
    /// Truncated user-scoped `MEMORY.md`.
    user_index: String,
    /// Truncated workspace-scoped `MEMORY.md`.
    workspace_index: String,
    /// Relevant retrieved pages for the current turn.
    relevant_pages: Vec<RelevantMemoryPage>,
}

/// Single retrieved page snippet prepared for memory formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelevantMemoryPage {
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
pub struct MemoryRetriever {
    memory_store: Arc<dyn MemoryStore>,
}

impl MemoryRetriever {
    /// Creates a memory retriever backed by the shared memory store.
    pub fn new(memory_store: Arc<dyn MemoryStore>) -> Self {
        Self { memory_store }
    }

    async fn load_stage_data(&self, ctx: &WorkingContext) -> Result<MemoryStageData> {
        let user_scope = MemoryScope::User(ctx.user_id.clone());
        let workspace_scope = MemoryScope::Workspace(ctx.workspace_id.clone());
        let user_index = self.memory_store.get_index(&user_scope).await?;
        let workspace_index = self.memory_store.get_index(&workspace_scope).await?;
        let mut relevant_pages = Vec::new();

        if let Some(query) = extract_search_query(ctx) {
            for scope in [user_scope, workspace_scope] {
                let scope_label = scope_label_for(&scope);
                let results = match self
                    .memory_store
                    .search(&query, &scope, MEMORY_RESULTS_PER_SCOPE)
                    .await
                {
                    Ok(results) => results,
                    Err(moa_core::MoaError::NotImplemented(message)) => {
                        tracing::debug!(
                            query,
                            scope = %scope_label,
                            error = %message,
                            "memory search unavailable; skipping relevant page retrieval"
                        );
                        relevant_pages.extend(
                            self.load_bootstrap_fallback_pages(&query, &scope, &scope_label)
                                .await?,
                        );
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                for result in results {
                    let result_scope = result.scope.clone();
                    let excerpt = match self
                        .memory_store
                        .read_page(&result_scope, &result.path)
                        .await
                    {
                        Ok(page) => page.content,
                        Err(error) => {
                            tracing::debug!(
                                path = %result.path,
                                scope = %scope_label_for(&result_scope),
                                error = %error,
                                "falling back to memory search snippet after read failure"
                            );
                            result.snippet.clone()
                        }
                    };
                    relevant_pages.push(RelevantMemoryPage {
                        scope_label: scope_label_for(&result_scope),
                        path: result.path.to_string(),
                        title: result.title,
                        excerpt,
                    });
                }
            }
        }

        Ok(MemoryStageData {
            user_index,
            workspace_index,
            relevant_pages,
        })
    }

    async fn load_bootstrap_fallback_pages(
        &self,
        query: &str,
        scope: &MemoryScope,
        scope_label: &str,
    ) -> Result<Vec<RelevantMemoryPage>> {
        if !matches!(scope, MemoryScope::Workspace(_)) {
            return Ok(Vec::new());
        }

        let path = MemoryPath::new(WORKSPACE_BOOTSTRAP_PAGE_PATH);
        let page = match self.memory_store.read_page(scope, &path).await {
            Ok(page) => page,
            Err(_) => return Ok(Vec::new()),
        };
        if !query_matches_page(query, &page.title, &page.content) {
            return Ok(Vec::new());
        }

        Ok(vec![RelevantMemoryPage {
            scope_label: scope_label.to_string(),
            path: path.as_str().to_string(),
            title: page.title,
            excerpt: page.content,
        }])
    }
}

#[async_trait]
impl ContextProcessor for MemoryRetriever {
    fn name(&self) -> &str {
        "memory"
    }

    fn stage(&self) -> u8 {
        6
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let data = self.load_stage_data(ctx).await?;
        let tokens_before = ctx.token_count;
        let mut items_included = Vec::new();
        let mut sections = Vec::new();

        if !data.user_index.trim().is_empty() {
            sections.push(format!(
                "<user_memory>\n{}\n</user_memory>",
                data.user_index.trim()
            ));
            items_included.push("user:MEMORY.md".to_string());
        }

        if !data.workspace_index.trim().is_empty() {
            sections.push(format!(
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

            sections.push(section);
        }

        if !sections.is_empty() {
            let reminder = format!(
                "{MEMORY_REMINDER_PREFIX}\n{}\n</memory-reminder>",
                sections.join("\n\n")
            );
            let insertion_index = trailing_user_insertion_index(&ctx.messages);
            ctx.insert_message(insertion_index, ContextMessage::user(reminder));
        }

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

fn trailing_user_insertion_index(messages: &[ContextMessage]) -> usize {
    let mut insertion_index = messages.len();
    while insertion_index > 0
        && matches!(
            messages[insertion_index - 1].role,
            moa_core::MessageRole::User
        )
    {
        insertion_index -= 1;
    }
    insertion_index
}

fn extract_search_query(ctx: &WorkingContext) -> Option<String> {
    if let Some(query) = ctx
        .metadata()
        .get("query_rewrite")
        .and_then(query_from_rewrite_metadata)
    {
        return Some(query);
    }

    extract_search_query_from_messages(&ctx.messages)
}

fn query_from_rewrite_metadata(value: &serde_json::Value) -> Option<String> {
    let result = serde_json::from_value::<QueryRewriteResult>(value.clone()).ok()?;
    if result.source != RewriteSource::Rewritten {
        return None;
    }
    let query = result.rewritten_query.trim();
    (!query.is_empty()).then(|| query.to_string())
}

fn extract_search_query_from_messages(messages: &[ContextMessage]) -> Option<String> {
    let text = messages
        .iter()
        .rev()
        .find_map(|message| match message.role {
            moa_core::MessageRole::User => Some(message.content.as_str()),
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

fn truncate_excerpt(excerpt: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if excerpt.chars().count() <= max_chars {
        return excerpt.trim().to_string();
    }

    let mut truncated = excerpt.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn scope_label_for(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::User(_) => "user".to_string(),
        MemoryScope::Workspace(_) => "workspace".to_string(),
    }
}

fn query_matches_page(query: &str, title: &str, content: &str) -> bool {
    let normalized_title = title.to_ascii_lowercase();
    let normalized_content = content.to_ascii_lowercase();

    query
        .split_whitespace()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .any(|token| normalized_title.contains(&token) || normalized_content.contains(&token))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::{
        ContextProcessor, MemoryPath, MemoryScope, MemorySearchResult, MemoryStore,
        ModelCapabilities, ModelId, PageSummary, PageType, Platform, Result, SessionId,
        SessionMeta, TokenPricing, ToolCallFormat, UserId, WikiPage, WorkspaceId,
    };

    use super::{MEMORY_REMINDER_PREFIX, MemoryRetriever, extract_search_keywords};

    #[derive(Clone)]
    struct StubMemoryStore {
        page_by_scope: HashMap<(String, String), WikiPage>,
        user_results: Vec<MemorySearchResult>,
        workspace_results: Vec<MemorySearchResult>,
        queries: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MemoryStore for StubMemoryStore {
        async fn search(
            &self,
            query: &str,
            scope: &MemoryScope,
            _limit: usize,
        ) -> Result<Vec<MemorySearchResult>> {
            self.queries
                .lock()
                .expect("query log should not be poisoned")
                .push(query.to_string());
            Ok(match scope {
                MemoryScope::User(_) => self.user_results.clone(),
                MemoryScope::Workspace(_) => self.workspace_results.clone(),
            })
        }

        async fn read_page(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
            self.page_by_scope
                .get(&(super::scope_label_for(scope), path.as_str().to_string()))
                .cloned()
                .ok_or_else(|| {
                    moa_core::MoaError::StorageError(format!("mock page not found: {}", path))
                })
        }

        async fn write_page(
            &self,
            _scope: &MemoryScope,
            _path: &MemoryPath,
            _page: WikiPage,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<()> {
            Ok(())
        }

        async fn list_pages(
            &self,
            _scope: &MemoryScope,
            _filter: Option<PageType>,
        ) -> Result<Vec<PageSummary>> {
            Ok(Vec::new())
        }

        async fn get_index(&self, scope: &MemoryScope) -> Result<String> {
            Ok(format!("{} memory", super::scope_label_for(scope)))
        }

        async fn rebuild_search_index(&self, _scope: &MemoryScope) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn memory_retriever_loads_indexes_and_results_directly() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
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
            },
            native_tools: Vec::new(),
        };
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities);
        ctx.append_message(moa_core::ContextMessage::user(
            "How do we store durable session state?",
        ));
        let shared_path = MemoryPath::new("topics/storage.md");
        let memory_store = StubMemoryStore {
            page_by_scope: HashMap::from([(
                ("workspace".to_string(), shared_path.as_str().to_string()),
                WikiPage {
                    path: Some(shared_path.clone()),
                    title: "Storage".to_string(),
                    page_type: PageType::Topic,
                    content: "Use Postgres for durable session state.".to_string(),
                    created: Utc::now(),
                    updated: Utc::now(),
                    confidence: moa_core::ConfidenceLevel::High,
                    related: Vec::new(),
                    sources: Vec::new(),
                    tags: Vec::new(),
                    auto_generated: false,
                    last_referenced: Utc::now(),
                    reference_count: 0,
                    metadata: HashMap::new(),
                },
            )]),
            user_results: Vec::new(),
            workspace_results: vec![MemorySearchResult {
                scope: MemoryScope::Workspace(session.workspace_id.clone()),
                path: shared_path,
                title: "Storage".to_string(),
                page_type: PageType::Topic,
                snippet: "storage snippet".to_string(),
                confidence: moa_core::ConfidenceLevel::High,
                updated: Utc::now(),
                reference_count: 0,
            }],
            queries: Arc::new(Mutex::new(Vec::new())),
        };
        let output = MemoryRetriever::new(Arc::new(memory_store))
            .process(&mut ctx)
            .await
            .unwrap();

        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].role, moa_core::MessageRole::User);
        assert!(ctx.messages[0].content.contains(MEMORY_REMINDER_PREFIX));
        assert!(ctx.messages[0].content.contains("<user_memory>"));
        assert!(ctx.messages[0].content.contains("<workspace_memory>"));
        assert!(ctx.messages[0].content.contains("<relevant_memory>"));
        assert_eq!(ctx.messages[1].role, moa_core::MessageRole::User);
        assert_eq!(
            ctx.messages[1].content,
            "How do we store durable session state?"
        );
        assert!(
            output.tokens_added
                >= super::super::estimate_tokens("Use Postgres for durable session state.")
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

    #[tokio::test]
    async fn memory_retriever_uses_rewritten_query_metadata_for_search() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
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
            },
            native_tools: Vec::new(),
        };
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities);
        ctx.append_message(moa_core::ContextMessage::user("fix that"));
        ctx.insert_metadata(
            "query_rewrite",
            serde_json::to_value(moa_core::QueryRewriteResult {
                rewritten_query: "Fix the OAuth refresh token race condition in auth/refresh.rs"
                    .to_string(),
                intent: moa_core::QueryIntent::Coding,
                sub_queries: Vec::new(),
                suggested_tools: Vec::new(),
                needs_clarification: false,
                clarification_question: None,
                source: moa_core::RewriteSource::Rewritten,
            })
            .expect("rewrite metadata should serialize"),
        );
        let queries = Arc::new(Mutex::new(Vec::new()));
        let memory_store = StubMemoryStore {
            page_by_scope: HashMap::new(),
            user_results: Vec::new(),
            workspace_results: Vec::new(),
            queries: queries.clone(),
        };

        MemoryRetriever::new(Arc::new(memory_store))
            .process(&mut ctx)
            .await
            .expect("memory stage should process");

        assert_eq!(
            queries
                .lock()
                .expect("query log should not be poisoned")
                .as_slice(),
            [
                "Fix the OAuth refresh token race condition in auth/refresh.rs",
                "Fix the OAuth refresh token race condition in auth/refresh.rs"
            ]
        );
    }
}

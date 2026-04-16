//! Stage 5: memory retrieval and prompt injection.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextProcessor, Event, EventRange, EventRecord, MemoryScope, MemoryStore, ProcessorOutput,
    Result, SessionStore, WorkingContext,
};

const MEMORY_BUDGET_DIVISOR: usize = 5;
const MEMORY_RESULTS_PER_SCOPE: usize = 2;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;

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

/// Single retrieved page snippet prepared for Stage 5 formatting.
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
    session_store: Arc<dyn SessionStore>,
}

impl MemoryRetriever {
    /// Creates a memory retriever backed by memory and session stores.
    pub fn new(memory_store: Arc<dyn MemoryStore>, session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            memory_store,
            session_store,
        }
    }

    async fn load_stage_data(&self, ctx: &WorkingContext) -> Result<MemoryStageData> {
        let user_scope = MemoryScope::User(ctx.user_id.clone());
        let workspace_scope = MemoryScope::Workspace(ctx.workspace_id.clone());
        let user_index = self.memory_store.get_index(user_scope.clone()).await?;
        let workspace_index = self.memory_store.get_index(workspace_scope.clone()).await?;
        let events = self
            .session_store
            .get_events(ctx.session_id.clone(), EventRange::all())
            .await?;
        let mut relevant_pages = Vec::new();

        if let Some(query) = extract_search_query(&events) {
            for scope in [user_scope, workspace_scope] {
                let results = self
                    .memory_store
                    .search(&query, scope, MEMORY_RESULTS_PER_SCOPE)
                    .await?;
                for result in results {
                    let result_scope = result.scope.clone();
                    let excerpt = match self
                        .memory_store
                        .read_page(result_scope.clone(), &result.path)
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
}

#[async_trait]
impl ContextProcessor for MemoryRetriever {
    fn name(&self) -> &str {
        "memory"
    }

    fn stage(&self) -> u8 {
        5
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let data = self.load_stage_data(ctx).await?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        ContextProcessor, Event, EventFilter, EventRange, EventRecord, MemoryPath, MemoryScope,
        MemorySearchResult, MemoryStore, ModelCapabilities, PageSummary, PageType, PendingSignal,
        PendingSignalId, Platform, Result, SequenceNum, SessionFilter, SessionId, SessionMeta,
        SessionStatus, SessionStore, SessionSummary, TokenPricing, ToolCallFormat, UserId,
        WikiPage, WorkspaceId,
    };
    use tokio::sync::Mutex;

    use super::{MemoryRetriever, extract_search_keywords};

    #[derive(Clone)]
    struct StubSessionStore {
        session: Arc<Mutex<SessionMeta>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
    }

    impl StubSessionStore {
        fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
            Self {
                session: Arc::new(Mutex::new(session)),
                events: Arc::new(Mutex::new(events)),
            }
        }
    }

    #[async_trait]
    impl SessionStore for StubSessionStore {
        async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
            let id = meta.id.clone();
            *self.session.lock().await = meta;
            Ok(id)
        }

        async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
            let mut events = self.events.lock().await;
            let sequence_num = events.len() as SequenceNum;
            events.push(EventRecord {
                id: uuid::Uuid::now_v7(),
                session_id,
                sequence_num,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            });
            Ok(sequence_num)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(self.events.lock().await.clone())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(self.session.lock().await.clone())
        }

        async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
            self.session.lock().await.status = status;
            Ok(())
        }

        async fn store_pending_signal(
            &self,
            _session_id: SessionId,
            signal: PendingSignal,
        ) -> Result<PendingSignalId> {
            Ok(signal.id)
        }

        async fn get_pending_signals(&self, _session_id: SessionId) -> Result<Vec<PendingSignal>> {
            Ok(Vec::new())
        }

        async fn resolve_pending_signal(&self, _signal_id: PendingSignalId) -> Result<()> {
            Ok(())
        }

        async fn search_events(
            &self,
            _query: &str,
            _filter: EventFilter,
        ) -> Result<Vec<EventRecord>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        async fn workspace_cost_since(
            &self,
            _workspace_id: &WorkspaceId,
            _since: DateTime<Utc>,
        ) -> Result<u32> {
            Ok(0)
        }

        async fn delete_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Clone)]
    struct StubMemoryStore {
        page_by_scope: HashMap<(String, String), WikiPage>,
        user_results: Vec<MemorySearchResult>,
        workspace_results: Vec<MemorySearchResult>,
    }

    #[async_trait]
    impl MemoryStore for StubMemoryStore {
        async fn search(
            &self,
            _query: &str,
            scope: MemoryScope,
            _limit: usize,
        ) -> Result<Vec<MemorySearchResult>> {
            Ok(match scope {
                MemoryScope::User(_) => self.user_results.clone(),
                MemoryScope::Workspace(_) => self.workspace_results.clone(),
            })
        }

        async fn read_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
            self.page_by_scope
                .get(&(super::scope_label_for(&scope), path.as_str().to_string()))
                .cloned()
                .ok_or_else(|| {
                    moa_core::MoaError::StorageError(format!("mock page not found: {}", path))
                })
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
            Ok(Vec::new())
        }

        async fn get_index(&self, scope: MemoryScope) -> Result<String> {
            Ok(format!("{} memory", super::scope_label_for(&scope)))
        }

        async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
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
            native_tools: Vec::new(),
        };
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities);
        let shared_path = MemoryPath::new("topics/storage.md");
        let memory_store = StubMemoryStore {
            page_by_scope: HashMap::from([(
                ("workspace".to_string(), shared_path.as_str().to_string()),
                WikiPage {
                    path: Some(shared_path.clone()),
                    title: "Storage".to_string(),
                    page_type: PageType::Topic,
                    content: "Use libsql for durable session state.".to_string(),
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
        };
        let session_store = StubSessionStore::new(
            session.clone(),
            vec![EventRecord {
                id: uuid::Uuid::now_v7(),
                session_id: session.id.clone(),
                sequence_num: 0,
                event_type: moa_core::EventType::UserMessage,
                event: Event::UserMessage {
                    text: "How do we store durable session state?".to_string(),
                    attachments: Vec::new(),
                },
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            }],
        );

        let output = MemoryRetriever::new(Arc::new(memory_store), Arc::new(session_store))
            .process(&mut ctx)
            .await
            .unwrap();

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

//! Stage 6: graph memory retrieval and prompt injection.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, MemoryScope, ProcessorOutput, QueryRewriteResult, Result,
    RewriteSource, ScopeContext, WorkingContext,
};
use moa_memory_graph::{AgeGraphStore, PiiClass};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use sqlx::PgPool;

const MEMORY_BUDGET_DIVISOR: usize = 5;
const GRAPH_MEMORY_RESULTS: usize = 4;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;
pub(crate) const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

/// Injects graph-memory retrieval hits into the active turn context.
pub struct GraphMemoryRetriever {
    pool: PgPool,
    assume_app_role: bool,
}

impl GraphMemoryRetriever {
    /// Creates a graph-memory retriever backed by the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_app_role: false,
        }
    }

    /// Configures owner-role tests to assume the production app role during scoped reads.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    async fn retrieve_hits(
        &self,
        ctx: &WorkingContext,
        query: String,
    ) -> Result<Vec<crate::retrieval::RetrievalHit>> {
        let scope = MemoryScope::Workspace {
            workspace_id: ctx.workspace_id.clone(),
        };
        let scope_context = ScopeContext::workspace(ctx.workspace_id.clone());
        let vector = Arc::new(PgvectorStore::new(self.pool.clone(), scope_context.clone()));
        let graph = Arc::new(
            AgeGraphStore::scoped(self.pool.clone(), scope_context)
                .with_vector_store(vector.clone()),
        );
        let retriever =
            crate::retrieval::HybridRetriever::from_env(self.pool.clone(), graph, vector)
                .with_assume_app_role(self.assume_app_role);

        retriever
            .retrieve(crate::retrieval::RetrievalRequest {
                seeds: Vec::new(),
                query_text: query,
                query_embedding: vec![0.0; VECTOR_DIMENSION],
                scope,
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: GRAPH_MEMORY_RESULTS,
                use_reranker: false,
                strategy: None,
            })
            .await
            .map_err(|error| {
                moa_core::MoaError::StorageError(format!("graph memory retrieval failed: {error}"))
            })
    }
}

#[async_trait]
impl ContextProcessor for GraphMemoryRetriever {
    fn name(&self) -> &str {
        "graph_memory"
    }

    fn stage(&self) -> u8 {
        6
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let Some(query) = extract_search_query(ctx) else {
            return Ok(ProcessorOutput::default());
        };
        let hits = self.retrieve_hits(ctx, query).await?;
        if hits.is_empty() {
            return Ok(ProcessorOutput::default());
        }

        let tokens_before = ctx.token_count;
        let memory_budget = (ctx.token_budget / MEMORY_BUDGET_DIVISOR).max(MIN_PAGE_EXCERPT_TOKENS);
        let per_hit_budget = (memory_budget / hits.len().max(1)).max(MIN_PAGE_EXCERPT_TOKENS);
        let mut section = String::from("<graph_memory>\n");
        let mut items_included = Vec::with_capacity(hits.len());

        for hit in &hits {
            let excerpt = truncate_excerpt(&graph_hit_excerpt(&hit.node), per_hit_budget);
            section.push_str(&format!(
                "## {} [{}:{} score={:.3}]\n{}\n\n",
                hit.node.name,
                hit.node.label.as_str(),
                hit.uid,
                hit.score,
                excerpt
            ));
            items_included.push(format!("graph:{}:{}", hit.node.label.as_str(), hit.uid));
        }
        section.push_str("</graph_memory>");

        let reminder = format!("{MEMORY_REMINDER_PREFIX}\n{section}\n</memory-reminder>");
        let insertion_index = trailing_user_insertion_index(&ctx.messages);
        ctx.insert_message(insertion_index, ContextMessage::user(reminder));

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            ..ProcessorOutput::default()
        })
    }
}

fn graph_hit_excerpt(row: &moa_memory_graph::NodeIndexRow) -> String {
    if let Some(summary) = row
        .properties_summary
        .as_ref()
        .and_then(|value| value.get("summary"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        return summary.to_string();
    }

    if let Some(properties) = &row.properties_summary {
        return serde_json::to_string(properties).unwrap_or_else(|_| row.name.clone());
    }

    row.name.clone()
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

#[cfg(test)]
mod tests {
    use super::extract_search_keywords;

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

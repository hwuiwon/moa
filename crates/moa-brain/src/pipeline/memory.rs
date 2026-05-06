//! Stage 6: graph memory retrieval and prompt injection.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    ContextMessage, ContextProcessor, LineageHandle, MemoryScope, NullLineageHandle,
    ProcessorOutput, QueryRewriteResult, Result, RewriteSource, ScopeContext, WorkingContext,
};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, LineageEvent, RerankHit, RetrievalLineage, RetrievalStage,
    ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{AgeGraphStore, PiiClass};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use sqlx::PgPool;
use tracing::Span;
use uuid::Uuid;

const MEMORY_BUDGET_DIVISOR: usize = 5;
const GRAPH_MEMORY_RESULTS: usize = 4;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;
pub(crate) const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

/// Injects graph-memory retrieval hits into the active turn context.
pub struct GraphMemoryRetriever {
    pool: PgPool,
    assume_app_role: bool,
    lineage: Arc<dyn LineageHandle>,
}

impl GraphMemoryRetriever {
    /// Creates a graph-memory retriever backed by the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_app_role: false,
            lineage: Arc::new(NullLineageHandle),
        }
    }

    /// Configures owner-role tests to assume the production app role during scoped reads.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Attaches the lineage sink used to capture retrieval traces.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.lineage = lineage;
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
        let retrieval_started = Instant::now();
        let hits = self.retrieve_hits(ctx, query.clone()).await?;
        self.emit_lineage(ctx, &query, &hits, retrieval_started.elapsed());
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

impl GraphMemoryRetriever {
    fn emit_lineage(
        &self,
        ctx: &WorkingContext,
        query: &str,
        hits: &[crate::retrieval::RetrievalHit],
        elapsed: std::time::Duration,
    ) {
        let retrieval = RetrievalLineage {
            turn_id: turn_id_from_context(ctx).unwrap_or_else(TurnId::new_v7),
            session_id: ctx.session_id,
            workspace_id: ctx.workspace_id.clone(),
            user_id: ctx.user_id.clone(),
            scope: MemoryScope::Workspace {
                workspace_id: ctx.workspace_id.clone(),
            },
            ts: Utc::now(),
            query_original: query.to_string(),
            query_expansions: query_expansions_from_context(ctx),
            vector_hits: hits
                .iter()
                .map(|hit| VecHit {
                    chunk_id: hit.uid,
                    score: hit.score as f32,
                    source: "hybrid".to_string(),
                    embedder: "configured".to_string(),
                    embed_dim: VECTOR_DIMENSION as u16,
                })
                .collect(),
            graph_paths: Vec::new(),
            fusion_scores: hits
                .iter()
                .map(|hit| FusedHit {
                    chunk_id: hit.uid,
                    fused_score: hit.score as f32,
                    vector_contribution: contribution(hit.legs.vector),
                    graph_contribution: contribution(hit.legs.graph),
                    lexical_contribution: contribution(hit.legs.lexical),
                    fusion_method: "rrf".to_string(),
                })
                .collect(),
            rerank_scores: hits
                .iter()
                .enumerate()
                .map(|(idx, hit)| RerankHit {
                    chunk_id: hit.uid,
                    original_index: idx.min(u16::MAX as usize) as u16,
                    relevance_score: hit.score as f32,
                    rerank_model: "noop".to_string(),
                })
                .collect(),
            top_k: hits.iter().map(|hit| hit.uid).collect(),
            timings: StageTimings {
                total_ms: duration_ms_u32(elapsed),
                ..StageTimings::default()
            },
            introspection: BackendIntrospection::default(),
            stage: RetrievalStage::Single,
        };

        match serde_json::to_value(LineageEvent::Retrieval(retrieval.clone())) {
            Ok(json) => self.lineage.record(json),
            Err(error) => tracing::warn!(%error, "failed to serialize retrieval lineage"),
        }
        let zero_recall_score = ScoreRecord {
            score_id: Uuid::now_v7(),
            ts: Utc::now(),
            target: ScoreTarget::Turn {
                turn_id: retrieval.turn_id,
            },
            workspace_id: retrieval.workspace_id.clone(),
            user_id: Some(retrieval.user_id.clone()),
            name: "retrieval_zero_recall".to_string(),
            value: ScoreValue::Boolean(retrieval.top_k.is_empty()),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: "hybrid-retriever".to_string(),
            run_id: None,
            dataset_id: None,
            comment: None,
        };
        match serde_json::to_value(LineageEvent::Eval(zero_recall_score)) {
            Ok(json) => self.lineage.record(json),
            Err(error) => tracing::warn!(%error, "failed to serialize retrieval score"),
        }
        metrics::counter!(
            "moa_turn_count",
            "workspace_id" => retrieval.workspace_id.to_string()
        )
        .increment(1);
        if retrieval.top_k.is_empty() {
            metrics::counter!(
                "moa_zero_recall_count",
                "workspace_id" => retrieval.workspace_id.to_string()
            )
            .increment(1);
        }
        moa_lineage_otel::emit_retrieval_attrs(&Span::current(), &retrieval);
    }
}

fn contribution(enabled: bool) -> f32 {
    if enabled { 1.0 } else { 0.0 }
}

fn duration_ms_u32(duration: std::time::Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

fn turn_id_from_context(ctx: &WorkingContext) -> Option<TurnId> {
    let value = ctx.metadata().get("_moa.turn_id")?.as_str()?;
    Uuid::parse_str(value).ok().map(TurnId)
}

fn query_expansions_from_context(ctx: &WorkingContext) -> Vec<String> {
    ctx.metadata()
        .get("query_rewrite")
        .and_then(query_from_rewrite_metadata)
        .into_iter()
        .collect()
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
        .split(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == '-')
        })
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

    #[test]
    fn keyword_extraction_preserves_memory_article_ids() {
        let keywords = extract_search_keywords("What is news_article_001 about?");

        assert_eq!(keywords, vec!["news_article_001"]);
    }
}

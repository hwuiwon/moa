//! Query planner that prepares graph-memory retrieval requests.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use moa_core::{MemoryScope, MoaError, traits::EmbeddingProvider};
use moa_memory_graph::{GraphError, GraphStore, NodeLabel, PiiClass};
use uuid::Uuid;

use crate::planning::ner::{NerExtractor, NerSpan};
use crate::retrieval::{CachedHybridRetriever, RetrievalError, RetrievalHit, RetrievalRequest};

const DEFAULT_SEED_LIMIT_PER_SPAN: i64 = 5;

/// Result type returned by query planning.
pub type Result<T> = std::result::Result<T, PlanError>;

/// Error returned by query planning.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// Graph seed lookup failed.
    #[error("graph seed lookup failed: {0}")]
    Graph(#[from] GraphError),
    /// Query embedding failed.
    #[error("query embedding failed: {0}")]
    Embed(#[from] MoaError),
    /// Query embedding returned no vector.
    #[error("query embedding returned no vector")]
    EmptyQueryEmbedding,
    /// Hybrid retrieval failed.
    #[error("hybrid retrieval failed: {0}")]
    Retrieval(#[from] RetrievalError),
}

/// Retrieval strategy selected from a query's wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// Graph traversal should have the strongest influence.
    GraphFirst,
    /// Vector similarity should have the strongest influence.
    VectorFirst,
    /// All retrieval legs should run with default weights.
    Both,
}

/// Planned query produced before retrieval.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedQuery {
    /// Retrieval strategy chosen by the planner.
    pub strategy: Strategy,
    /// NER-grounded graph seed node ids.
    pub seeds: Vec<Uuid>,
    /// Optional graph node label allowlist inferred from the query.
    pub label_hint: Option<Vec<NodeLabel>>,
    /// Most-specific request scope.
    pub scope: MemoryScope,
    /// Ancestor chain from global through the most-specific scope.
    pub scope_ancestors: Vec<MemoryScope>,
    /// Optional application-time filter. V1 leaves this unset.
    pub temporal_filter: Option<DateTime<Utc>>,
}

impl PlannedQuery {
    /// Converts this plan into a hybrid retrieval request.
    #[must_use]
    pub fn into_retrieval_request(
        self,
        query_text: impl Into<String>,
        query_embedding: Vec<f32>,
        max_pii_class: PiiClass,
        k_final: usize,
        use_reranker: bool,
    ) -> RetrievalRequest {
        RetrievalRequest {
            seeds: self.seeds,
            query_text: query_text.into(),
            query_embedding,
            scope: self.scope,
            label_filter: self.label_hint,
            max_pii_class,
            k_final,
            use_reranker,
            strategy: Some(self.strategy),
            as_of: self.temporal_filter,
        }
    }
}

/// Request-scoped inputs used during query planning.
#[derive(Clone)]
pub struct PlanningCtx {
    /// Most-specific request memory scope.
    pub scope: MemoryScope,
    /// Graph store used for seed grounding through `moa.node_index`.
    pub graph: Arc<dyn GraphStore>,
    /// Number of sidecar seed candidates fetched per extracted NER span.
    pub seed_limit_per_span: i64,
}

impl PlanningCtx {
    /// Creates planning context with the default seed limit.
    #[must_use]
    pub fn new(scope: MemoryScope, graph: Arc<dyn GraphStore>) -> Self {
        Self {
            scope,
            graph,
            seed_limit_per_span: DEFAULT_SEED_LIMIT_PER_SPAN,
        }
    }

    /// Overrides the seed lookup limit used for each NER span.
    #[must_use]
    pub fn with_seed_limit_per_span(mut self, limit: i64) -> Self {
        self.seed_limit_per_span = limit.max(0);
        self
    }
}

/// Fast query planner for graph-memory retrieval.
#[derive(Debug, Clone, Default)]
pub struct QueryPlanner {
    ner: NerExtractor,
}

impl QueryPlanner {
    /// Creates a planner with the bundled v1 NER extractor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a planner with an explicit NER extractor.
    #[must_use]
    pub fn with_ner(ner: NerExtractor) -> Self {
        Self { ner }
    }

    /// Plans one free-form query into seed nodes, labels, scope, and strategy.
    pub async fn plan(&self, query_text: &str, ctx: &PlanningCtx) -> Result<PlannedQuery> {
        let spans = self.ner.extract(query_text);
        let mut seeds = Vec::new();
        for span in &spans {
            let candidates = ctx
                .graph
                .lookup_seeds(&span.text, ctx.seed_limit_per_span)
                .await?;
            seeds.extend(candidates.into_iter().map(|candidate| candidate.uid));
        }
        seeds.sort_unstable();
        seeds.dedup();

        Ok(PlannedQuery {
            strategy: classify_strategy(query_text),
            seeds,
            label_hint: infer_label_hint(query_text, &spans),
            scope: ctx.scope.clone(),
            scope_ancestors: ctx.scope.ancestors(),
            temporal_filter: parse_temporal(query_text),
        })
    }
}

/// Inputs needed to plan, embed, and run one retrieval.
pub struct QueryRetrievalCtx<'a> {
    /// Planner instance used to classify and seed the query.
    pub planner: &'a QueryPlanner,
    /// Request-scoped graph planning inputs.
    pub planning: &'a PlanningCtx,
    /// Embedder used to produce the query vector.
    pub embedder: &'a dyn EmbeddingProvider,
    /// Cached hybrid retriever used after planning.
    pub hybrid: &'a CachedHybridRetriever,
    /// Maximum PII class visible to the caller.
    pub max_pii_class: PiiClass,
    /// Number of final hits requested.
    pub k_final: usize,
    /// Whether the retriever should call the configured reranker.
    pub use_reranker: bool,
}

impl<'a> QueryRetrievalCtx<'a> {
    /// Creates a query retrieval context with required backends.
    #[must_use]
    pub fn new(
        planner: &'a QueryPlanner,
        planning: &'a PlanningCtx,
        embedder: &'a dyn EmbeddingProvider,
        hybrid: &'a CachedHybridRetriever,
        max_pii_class: PiiClass,
    ) -> Self {
        Self {
            planner,
            planning,
            embedder,
            hybrid,
            max_pii_class,
            k_final: 5,
            use_reranker: false,
        }
    }

    /// Overrides the number of final hits requested.
    #[must_use]
    pub fn with_k_final(mut self, k_final: usize) -> Self {
        self.k_final = k_final;
        self
    }

    /// Overrides whether Cohere-compatible reranking should be used.
    #[must_use]
    pub fn with_reranker(mut self, use_reranker: bool) -> Self {
        self.use_reranker = use_reranker;
        self
    }
}

/// Plans, embeds, and retrieves graph-memory hits for one query.
///
/// This helper is the pipeline-facing path for callers that already own the graph,
/// vector, and embedding backends.
pub async fn retrieve_for_query(
    query_text: &str,
    ctx: &QueryRetrievalCtx<'_>,
) -> Result<Vec<RetrievalHit>> {
    let planned = ctx.planner.plan(query_text, ctx.planning).await?;
    let query_input = vec![query_text.to_string()];
    let embed_started = std::time::Instant::now();
    let mut embeddings = ctx.embedder.embed(&query_input).await?;
    metrics::histogram!("moa_retrieval_embedder_seconds")
        .record(embed_started.elapsed().as_secs_f64());
    let embedding = embeddings.pop().ok_or(PlanError::EmptyQueryEmbedding)?;
    let request = planned.clone().into_retrieval_request(
        query_text,
        embedding,
        ctx.max_pii_class,
        ctx.k_final,
        ctx.use_reranker,
    );
    ctx.hybrid
        .retrieve(&planned, request)
        .await
        .map_err(PlanError::from)
}

/// Classifies the retrieval strategy using explicit v1 heuristics.
#[must_use]
pub fn classify_strategy(text: &str) -> Strategy {
    let lower = text.to_ascii_lowercase();
    if contains_any(
        &lower,
        &[
            "depends on",
            "connects to",
            "connected to",
            "impacted by",
            "impacts ",
            "relate",
            "upstream",
            "downstream",
            "dependency",
        ],
    ) {
        return Strategy::GraphFirst;
    }
    if contains_any(
        &lower,
        &[
            "when ",
            "how often",
            "history of",
            "similar to",
            "usually",
            "has anything been done",
        ],
    ) {
        return Strategy::VectorFirst;
    }
    Strategy::Both
}

fn infer_label_hint(text: &str, _spans: &[NerSpan]) -> Option<Vec<NodeLabel>> {
    let lower = text.to_ascii_lowercase();
    if contains_any(&lower, &["decision", "decided", "decide"]) {
        return Some(vec![NodeLabel::Decision]);
    }
    if contains_any(&lower, &["incident", "outage", "postmortem"]) {
        return Some(vec![NodeLabel::Incident]);
    }
    if contains_any(&lower, &["lesson", "learned", "learning"]) {
        return Some(vec![NodeLabel::Lesson]);
    }
    if contains_any(&lower, &["source", "document", "doc "]) {
        return Some(vec![NodeLabel::Source]);
    }
    if contains_any(&lower, &["concept", "term"]) {
        return Some(vec![NodeLabel::Concept]);
    }
    None
}

fn parse_temporal(text: &str) -> Option<DateTime<Utc>> {
    parse_as_of_fragment(text)
        .or_else(|| parse_rfc3339_in_text(text))
        .or_else(|| parse_iso_date_in_text(text))
        .or_else(|| parse_month_year_phrase(text))
}

fn parse_as_of_fragment(text: &str) -> Option<DateTime<Utc>> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("as of")?;
    let fragment = text.get(start + "as of".len()..)?.trim_start();
    parse_rfc3339_in_text(fragment)
        .or_else(|| parse_iso_date_in_text(fragment))
        .or_else(|| parse_month_year_fragment(fragment))
}

fn parse_rfc3339_in_text(text: &str) -> Option<DateTime<Utc>> {
    text.split_whitespace()
        .map(trim_temporal_token)
        .find_map(|token| {
            DateTime::parse_from_rfc3339(token)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        })
}

fn parse_iso_date_in_text(text: &str) -> Option<DateTime<Utc>> {
    text.split(|ch: char| !(ch.is_ascii_digit() || ch == '-'))
        .filter(|token| token.len() == 10)
        .find_map(parse_iso_date_token)
}

fn parse_iso_date_token(token: &str) -> Option<DateTime<Utc>> {
    let date = NaiveDate::parse_from_str(token, "%Y-%m-%d").ok()?;
    let datetime = date.and_hms_opt(0, 0, 0)?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(datetime, Utc))
}

fn parse_month_year_phrase(text: &str) -> Option<DateTime<Utc>> {
    let words = temporal_words(text);
    words.windows(3).find_map(|window| {
        if window[0] == "in" {
            parse_month_year(&window[1], &window[2])
        } else {
            None
        }
    })
}

fn parse_month_year_fragment(text: &str) -> Option<DateTime<Utc>> {
    let words = temporal_words(text);
    words
        .windows(2)
        .find_map(|window| parse_month_year(&window[0], &window[1]))
}

fn parse_month_year(month: &str, year: &str) -> Option<DateTime<Utc>> {
    let year = parse_four_digit_year(year)?;
    let date = NaiveDate::from_ymd_opt(year, month_number(month)?, 1)?;
    let datetime = date.and_hms_opt(0, 0, 0)?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(datetime, Utc))
}

fn parse_four_digit_year(value: &str) -> Option<i32> {
    if value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse::<i32>().ok()
    } else {
        None
    }
}

fn month_number(value: &str) -> Option<u32> {
    match value {
        "january" | "jan" => Some(1),
        "february" | "feb" => Some(2),
        "march" | "mar" => Some(3),
        "april" | "apr" => Some(4),
        "may" => Some(5),
        "june" | "jun" => Some(6),
        "july" | "jul" => Some(7),
        "august" | "aug" => Some(8),
        "september" | "sep" | "sept" => Some(9),
        "october" | "oct" => Some(10),
        "november" | "nov" => Some(11),
        "december" | "dec" => Some(12),
        _ => None,
    }
}

fn temporal_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn trim_temporal_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | ':' | '+' | '.'))
    })
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{NodeLabel, Strategy, classify_strategy, infer_label_hint, parse_temporal};

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test timestamp should be valid RFC3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn planner_classify_graphfirst_for_dependency_queries() {
        assert_eq!(
            classify_strategy("What depends on the auth service?"),
            Strategy::GraphFirst
        );
    }

    #[test]
    fn planner_classify_vectorfirst_for_history_queries() {
        assert_eq!(
            classify_strategy("How often does the deploy fail?"),
            Strategy::VectorFirst
        );
    }

    #[test]
    fn planner_classify_defaults_to_both() {
        assert_eq!(classify_strategy("tell me about deploys"), Strategy::Both);
    }

    #[test]
    fn planner_label_hint_detects_incidents() {
        assert_eq!(
            infer_label_hint("show auth outage incidents", &[]),
            Some(vec![NodeLabel::Incident])
        );
    }

    #[test]
    fn temporal_parser_recognizes_rfc3339_timestamp() {
        // Pins: query planning preserves explicit RFC3339 application time.
        assert_eq!(
            parse_temporal("incident status at 2026-03-11T14:15:16Z?"),
            Some(utc("2026-03-11T14:15:16Z"))
        );
    }

    #[test]
    fn temporal_parser_recognizes_iso_date_as_utc_midnight() {
        // Pins: date-only historical filters use deterministic UTC midnight.
        assert_eq!(
            parse_temporal("what did we know on 2026-03-11"),
            Some(utc("2026-03-11T00:00:00Z"))
        );
    }

    #[test]
    fn temporal_parser_prefers_as_of_fragment() {
        // Pins: `as of` selects the historical filter even when other dates appear first.
        assert_eq!(
            parse_temporal("compare 2026-01-01 as of 2026-04-15"),
            Some(utc("2026-04-15T00:00:00Z"))
        );
    }

    #[test]
    fn temporal_parser_recognizes_month_year_phrase() {
        // Pins: month/year phrases resolve to the first day of that month in UTC.
        assert_eq!(
            parse_temporal("what changed in March 2026?"),
            Some(utc("2026-03-01T00:00:00Z"))
        );
    }
}

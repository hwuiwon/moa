//! Hybrid contradiction detection for graph-memory ingestion.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{ScopeContext, ScopedConn};
use moa_memory_graph::{NodeIndexRow, NodeLabel};
use moa_memory_vector::{Error as VectorError, VECTOR_DIMENSION, VectorQuery, VectorStore};
use moka::future::Cache;
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use tokio::time::timeout;
use uuid::Uuid;

use crate::{EmbeddedFact, IngestError, Result};

const VECTOR_K: usize = 10;
const LEXICAL_K: i64 = 10;
const RERANK_TOP_N: usize = 5;
const RRF_K: f64 = 60.0;
const DEFAULT_FAST_BUDGET: Duration = Duration::from_millis(250);
const DEFAULT_SLOW_BUDGET: Duration = Duration::from_secs(5);
const DEFAULT_JUDGE_BUDGET: Duration = Duration::from_millis(200);
const COHERE_RERANK_URL: &str = "https://api.cohere.com/v2/rerank";
const COHERE_RERANK_MODEL: &str = "rerank-v4.0-fast";
const CACHE_CAPACITY: u64 = 10_000;
const JUDGE_PROMPT: &str = include_str!("../prompts/judge.txt");

/// Conflict routing decision returned by contradiction detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Conflict {
    /// No conflicting or duplicate fact was found.
    Insert,
    /// New fact strictly contradicts this existing fact and should supersede it.
    Supersede(Uuid),
    /// New fact restates this existing fact and should not create a duplicate.
    Duplicate(Uuid),
    /// The detector could not decide inside its budget.
    Indeterminate,
}

/// Request-scoped services used by a contradiction detector.
#[derive(Clone)]
pub struct ContradictionContext {
    pool: PgPool,
    scope: ScopeContext,
    vector: Arc<dyn VectorStore>,
    assume_app_role: bool,
}

impl ContradictionContext {
    /// Creates a contradiction context using production RLS role assumptions.
    #[must_use]
    pub fn new(pool: PgPool, scope: ScopeContext, vector: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            scope,
            vector,
            assume_app_role: false,
        }
    }

    /// Creates a contradiction context that assumes `moa_app` inside transactions.
    ///
    /// This is used by local integration tests that connect with the owner role while still
    /// exercising production RLS policies.
    #[must_use]
    pub fn for_app_role(pool: PgPool, scope: ScopeContext, vector: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            scope,
            vector,
            assume_app_role: true,
        }
    }

    /// Returns the underlying Postgres pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the request scope used for RLS GUCs.
    #[must_use]
    pub fn scope(&self) -> &ScopeContext {
        &self.scope
    }

    /// Returns the vector store used for KNN candidate retrieval.
    #[must_use]
    pub fn vector(&self) -> &dyn VectorStore {
        self.vector.as_ref()
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin(&self.pool, &self.scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        Ok(conn)
    }
}

/// Typed contradiction detector used by slow-path and fast-path ingestion.
#[async_trait]
pub trait ContradictionDetector: Send + Sync {
    /// Checks one free-form fact under the fast-path latency budget.
    async fn check_one_fast(
        &self,
        fact_text: &str,
        embedding: &[f32],
        label: NodeLabel,
        ctx: &ContradictionContext,
    ) -> Result<Conflict>;

    /// Checks one extracted embedded fact under the slow-path latency budget.
    async fn check_one_slow(
        &self,
        fact: &EmbeddedFact,
        ctx: &ContradictionContext,
    ) -> Result<Conflict>;
}

/// One rerank hit returned by a reranker backend.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RerankHit {
    /// Candidate index inside the document list supplied to the reranker.
    pub index: usize,
    /// Backend-specific relevance score.
    pub relevance_score: f32,
}

/// Reranker abstraction used between RRF retrieval and the judge.
#[async_trait]
trait Reranker: Send + Sync {
    /// Reranks candidate names for one new fact and returns candidate indices.
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>>;
}

/// Deterministic reranker that preserves retrieval order.
#[derive(Debug, Clone, Default)]
struct NoopReranker;

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _model: &str,
        _query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        Ok((0..documents.len().min(top_n))
            .map(|index| RerankHit {
                index,
                relevance_score: 1.0,
            })
            .collect())
    }
}

/// Cohere Rerank v4 client used for top-N selection.
#[derive(Clone)]
struct CohereReranker {
    client: Client,
    api_key: SecretString,
    endpoint: String,
}

impl CohereReranker {
    /// Creates a Cohere reranker from an API key.
    #[must_use]
    fn new(api_key: SecretString) -> Self {
        Self {
            client: Client::new(),
            api_key,
            endpoint: COHERE_RERANK_URL.to_string(),
        }
    }
}

#[async_trait]
impl Reranker for CohereReranker {
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        if documents.is_empty() || top_n == 0 {
            return Ok(Vec::new());
        }

        let request = CohereRerankRequest {
            model,
            query,
            documents,
            top_n,
        };
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose_secret())
            .json(&request)
            .send()
            .await
            .map_err(|error| IngestError::Rerank(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read error body: {error}"));
            return Err(IngestError::Rerank(format!(
                "Cohere rerank returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        let body = response
            .json::<CohereRerankResponse>()
            .await
            .map_err(|error| IngestError::Rerank(error.to_string()))?;
        Ok(body
            .results
            .into_iter()
            .filter(|hit| hit.index < documents.len())
            .map(|hit| RerankHit {
                index: hit.index,
                relevance_score: hit.relevance_score,
            })
            .collect())
    }
}

/// Verdict returned by the final fact-comparison judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JudgeVerdict {
    /// The new fact makes the candidate false.
    Contradicts,
    /// The new fact says the same thing as the candidate.
    Restates,
    /// The new fact is unrelated to or compatible with candidates.
    Independent,
    /// The judge abstained.
    Indeterminate,
}

/// Structured response from the fact-comparison judge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeResponse {
    /// Judge verdict.
    pub verdict: JudgeVerdict,
    /// Candidate uid selected by the judge, if any.
    pub candidate_uid: Option<Uuid>,
    /// Short rationale for observability and future audit.
    pub rationale: String,
}

/// LLM judge abstraction used after reranking.
#[async_trait]
pub trait JudgeModel: Send + Sync {
    /// Judges one new fact against a small candidate set.
    async fn judge(
        &self,
        prompt: &str,
        fact_text: &str,
        candidates: &[NodeIndexRow],
    ) -> Result<JudgeResponse>;
}

/// Deterministic local judge used when no low-latency LLM judge is configured.
#[derive(Debug, Clone, Default)]
pub struct HeuristicJudge;

#[async_trait]
impl JudgeModel for HeuristicJudge {
    async fn judge(
        &self,
        _prompt: &str,
        fact_text: &str,
        candidates: &[NodeIndexRow],
    ) -> Result<JudgeResponse> {
        Ok(heuristic_judge(fact_text, candidates))
    }
}

/// RRF plus rerank plus judge contradiction detector.
#[derive(Clone)]
pub struct RrfPlusJudgeDetector {
    reranker: Arc<dyn Reranker>,
    judge: Arc<dyn JudgeModel>,
    judge_cache: Cache<[u8; 32], Conflict>,
    fast_budget: Duration,
    slow_budget: Duration,
    judge_budget: Duration,
}

impl RrfPlusJudgeDetector {
    /// Creates a detector from explicit reranker and judge backends.
    #[must_use]
    fn new(reranker: Arc<dyn Reranker>, judge: Arc<dyn JudgeModel>) -> Self {
        Self {
            reranker,
            judge,
            judge_cache: Cache::builder().max_capacity(CACHE_CAPACITY).build(),
            fast_budget: DEFAULT_FAST_BUDGET,
            slow_budget: DEFAULT_SLOW_BUDGET,
            judge_budget: DEFAULT_JUDGE_BUDGET,
        }
    }

    /// Creates a local detector, using Cohere Rerank when an API key is present.
    #[must_use]
    pub fn from_env_or_heuristic() -> Self {
        let reranker: Arc<dyn Reranker> = std::env::var("COHERE_API_KEY")
            .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
            .map(|api_key| {
                Arc::new(CohereReranker::new(SecretString::from(api_key))) as Arc<dyn Reranker>
            })
            .unwrap_or_else(|_| Arc::new(NoopReranker));
        Self::new(reranker, Arc::new(HeuristicJudge))
    }

    /// Overrides all latency budgets, primarily for deterministic tests.
    #[must_use]
    pub fn with_budgets(
        mut self,
        fast_budget: Duration,
        slow_budget: Duration,
        judge_budget: Duration,
    ) -> Self {
        self.fast_budget = fast_budget;
        self.slow_budget = slow_budget;
        self.judge_budget = judge_budget;
        self
    }

    /// Retrieves hybrid candidates using vector KNN, lexical search, and RRF.
    pub async fn candidates(
        &self,
        fact_text: &str,
        embedding: &[f32],
        label: NodeLabel,
        ctx: &ContradictionContext,
    ) -> Result<Vec<NodeIndexRow>> {
        let vector_hits = vector_candidate_uids(fact_text, embedding, label, ctx).await?;
        let lexical_hits = lexical_candidate_uids(fact_text, label, ctx).await?;
        let ranked = rrf_fuse(
            &vector_hits,
            &lexical_hits,
            VECTOR_K.max(usize::try_from(LEXICAL_K).unwrap_or(VECTOR_K)),
        );
        let uids = ranked
            .into_iter()
            .take(VECTOR_K)
            .map(|(uid, _score)| uid)
            .collect::<Vec<_>>();
        hydrate_candidates(&uids, ctx).await
    }

    /// Reranks candidates to the top five using the configured reranker.
    pub async fn rerank_top5(
        &self,
        fact_text: &str,
        candidates: &[NodeIndexRow],
    ) -> Result<Vec<NodeIndexRow>> {
        if candidates.len() <= RERANK_TOP_N {
            return Ok(candidates.to_vec());
        }

        let documents = candidates
            .iter()
            .map(|candidate| candidate.name.clone())
            .collect::<Vec<_>>();
        let hits = self
            .reranker
            .rerank(COHERE_RERANK_MODEL, fact_text, &documents, RERANK_TOP_N)
            .await?;
        let mut reranked = Vec::with_capacity(hits.len());
        for hit in hits {
            if let Some(candidate) = candidates.get(hit.index) {
                reranked.push(candidate.clone());
            }
        }
        if reranked.is_empty() {
            Ok(candidates.iter().take(RERANK_TOP_N).cloned().collect())
        } else {
            Ok(reranked)
        }
    }

    /// Judges a new fact against hydrated candidates and uses a prompt-hash cache.
    pub async fn judge_candidates(
        &self,
        fact_text: &str,
        candidates: &[NodeIndexRow],
    ) -> Result<Conflict> {
        if candidates.is_empty() {
            return Ok(Conflict::Insert);
        }

        let prompt = build_judge_prompt(fact_text, candidates);
        let cache_key = *blake3::hash(prompt.as_bytes()).as_bytes();
        if let Some(conflict) = self.judge_cache.get(&cache_key).await {
            return Ok(conflict);
        }

        let response = match timeout(
            self.judge_budget,
            self.judge.judge(&prompt, fact_text, candidates),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => return Ok(Conflict::Indeterminate),
        };
        let conflict = conflict_from_judge(response);
        self.judge_cache.insert(cache_key, conflict).await;
        Ok(conflict)
    }

    async fn run(
        &self,
        fact_text: &str,
        embedding: &[f32],
        label: NodeLabel,
        ctx: &ContradictionContext,
    ) -> Result<Conflict> {
        let candidates = self.candidates(fact_text, embedding, label, ctx).await?;
        let candidates = self.rerank_top5(fact_text, &candidates).await?;
        self.judge_candidates(fact_text, &candidates).await
    }
}

impl Default for RrfPlusJudgeDetector {
    fn default() -> Self {
        Self::new(Arc::new(NoopReranker), Arc::new(HeuristicJudge))
    }
}

#[async_trait]
impl ContradictionDetector for RrfPlusJudgeDetector {
    async fn check_one_fast(
        &self,
        fact_text: &str,
        embedding: &[f32],
        label: NodeLabel,
        ctx: &ContradictionContext,
    ) -> Result<Conflict> {
        match timeout(self.fast_budget, self.run(fact_text, embedding, label, ctx)).await {
            Ok(result) => result,
            Err(_) => Ok(Conflict::Indeterminate),
        }
    }

    async fn check_one_slow(
        &self,
        fact: &EmbeddedFact,
        ctx: &ContradictionContext,
    ) -> Result<Conflict> {
        let fact_text = &fact.classified.fact.summary;
        let empty_embedding = [];
        let embedding = fact.embedding.as_deref().unwrap_or(&empty_embedding);
        match timeout(
            self.slow_budget,
            self.run(fact_text, embedding, NodeLabel::Fact, ctx),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Ok(Conflict::Indeterminate),
        }
    }
}

/// Fuses vector and lexical ranks using reciprocal rank fusion.
#[must_use]
pub fn rrf_fuse(vector_hits: &[Uuid], lexical_hits: &[Uuid], limit: usize) -> Vec<(Uuid, f64)> {
    let mut scores = HashMap::<Uuid, f64>::new();
    for (rank, uid) in vector_hits.iter().enumerate() {
        *scores.entry(*uid).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    for (rank, uid) in lexical_hits.iter().enumerate() {
        *scores.entry(*uid).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
    }
    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(limit);
    ranked
}

/// Builds the fixed fact-comparison prompt for the judge.
#[must_use]
pub fn build_judge_prompt(fact_text: &str, candidates: &[NodeIndexRow]) -> String {
    let candidates_list = candidates
        .iter()
        .map(|candidate| format!("{} -> {}", candidate.uid, candidate.name))
        .collect::<Vec<_>>()
        .join("\n");
    JUDGE_PROMPT
        .replace("{{ fact_text }}", fact_text)
        .replace("{{ candidates_list }}", &candidates_list)
}

async fn vector_candidate_uids(
    fact_text: &str,
    embedding: &[f32],
    label: NodeLabel,
    ctx: &ContradictionContext,
) -> Result<Vec<Uuid>> {
    let _ = fact_text;
    if embedding.is_empty() {
        return Ok(Vec::new());
    }
    if embedding.len() != ctx.vector().dimension() || embedding.len() != VECTOR_DIMENSION {
        return Err(IngestError::Vector(VectorError::DimensionMismatch {
            expected: VECTOR_DIMENSION,
            actual: embedding.len(),
        }));
    }

    let hits = ctx
        .vector()
        .knn(&VectorQuery {
            embedding: embedding.to_vec(),
            k: VECTOR_K,
            label_filter: Some(vec![label.as_str().to_string()]),
            max_pii_class: "phi".to_string(),
            include_global: true,
        })
        .await?;
    Ok(hits.into_iter().map(|hit| hit.uid).collect())
}

async fn lexical_candidate_uids(
    fact_text: &str,
    label: NodeLabel,
    ctx: &ContradictionContext,
) -> Result<Vec<Uuid>> {
    let mut conn = ctx.begin().await?;
    let uids = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND label = $2
          AND name_tsv @@ plainto_tsquery('simple', $1)
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                 last_accessed_at DESC
        LIMIT $3
        "#,
    )
    .bind(fact_text)
    .bind(label.as_str())
    .bind(LEXICAL_K)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(uids)
}

async fn hydrate_candidates(
    uids: &[Uuid],
    ctx: &ContradictionContext,
) -> Result<Vec<NodeIndexRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = ctx.begin().await?;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY($1)
          AND valid_to IS NULL
        "#,
    )
    .bind(uids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;

    let mut by_uid = rows
        .into_iter()
        .map(|row| (row.uid, row))
        .collect::<HashMap<_, _>>();
    Ok(uids
        .iter()
        .filter_map(|uid| by_uid.remove(uid))
        .collect::<Vec<_>>())
}

fn conflict_from_judge(response: JudgeResponse) -> Conflict {
    match (response.verdict, response.candidate_uid) {
        (JudgeVerdict::Contradicts, Some(uid)) => Conflict::Supersede(uid),
        (JudgeVerdict::Restates, Some(uid)) => Conflict::Duplicate(uid),
        (JudgeVerdict::Independent, _) => Conflict::Insert,
        _ => Conflict::Indeterminate,
    }
}

fn heuristic_judge(fact_text: &str, candidates: &[NodeIndexRow]) -> JudgeResponse {
    let normalized_fact = normalize_fact_text(fact_text);
    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| normalize_fact_text(&candidate.name) == normalized_fact)
    {
        return JudgeResponse {
            verdict: JudgeVerdict::Restates,
            candidate_uid: Some(candidate.uid),
            rationale: "normalized fact text matches candidate".to_string(),
        };
    }

    if let Some((candidate, _old_provider, _new_provider)) =
        contradictory_deployment_provider(fact_text, candidates)
    {
        return JudgeResponse {
            verdict: JudgeVerdict::Contradicts,
            candidate_uid: Some(candidate.uid),
            rationale: "deployment provider changed".to_string(),
        };
    }

    JudgeResponse {
        verdict: JudgeVerdict::Independent,
        candidate_uid: candidates.first().map(|candidate| candidate.uid),
        rationale: "no strict contradiction or restatement found".to_string(),
    }
}

fn contradictory_deployment_provider<'a>(
    fact_text: &str,
    candidates: &'a [NodeIndexRow],
) -> Option<(&'a NodeIndexRow, &'static str, &'static str)> {
    let new_provider = deployment_provider(fact_text)?;
    let fact = fact_text.to_ascii_lowercase();
    if !fact.contains("deploy") && !fact.contains("deployment") {
        return None;
    }
    for candidate in candidates {
        let candidate_text = candidate_text(candidate).to_ascii_lowercase();
        if !candidate_text.contains("deploy") && !candidate_text.contains("deployment") {
            continue;
        }
        let Some(old_provider) = deployment_provider(&candidate_text) else {
            continue;
        };
        if old_provider != new_provider {
            return Some((candidate, old_provider, new_provider));
        }
    }
    None
}

fn candidate_text(candidate: &NodeIndexRow) -> String {
    candidate
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or(&candidate.name)
        .to_string()
}

fn deployment_provider(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    [
        ("fly.io", &["fly.io", "flyio", "fly"][..]),
        ("aws", &["aws", "amazon web services", "ec2"][..]),
        ("gcp", &["gcp", "google cloud", "cloud run"][..]),
        ("azure", &["azure"][..]),
        ("heroku", &["heroku"][..]),
        ("vercel", &["vercel"][..]),
        ("netlify", &["netlify"][..]),
    ]
    .into_iter()
    .find_map(|(provider, aliases)| {
        aliases
            .iter()
            .any(|alias| lower.contains(alias))
            .then_some(provider)
    })
}

fn normalize_fact_text(text: &str) -> String {
    text.split_whitespace()
        .map(|part| part.trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '.'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[derive(Serialize)]
struct CohereRerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
    top_n: usize,
}

#[derive(Deserialize)]
struct CohereRerankResponse {
    results: Vec<CohereRerankHit>,
}

#[derive(Deserialize)]
struct CohereRerankHit {
    index: usize,
    relevance_score: f32,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use chrono::Utc;
    use moa_memory_graph::PiiClass;
    use tokio::time::sleep;

    use super::*;

    #[test]
    fn rrf_fusion_prioritizes_hits_seen_by_both_rankers() {
        let shared = Uuid::now_v7();
        let vector_only = Uuid::now_v7();
        let lexical_only = Uuid::now_v7();

        let ranked = rrf_fuse(&[vector_only, shared], &[lexical_only, shared], 3);

        assert_eq!(ranked[0].0, shared);
        assert_eq!(ranked.len(), 3);
    }

    #[tokio::test]
    async fn contradiction_judge_restating_fact_returns_duplicate() {
        let candidate = candidate("we deploy to fly.io", None);
        let detector = RrfPlusJudgeDetector::default();

        let conflict = detector
            .judge_candidates("we deploy to fly.io", std::slice::from_ref(&candidate))
            .await
            .expect("judge duplicate");

        assert_eq!(conflict, Conflict::Duplicate(candidate.uid));
    }

    #[tokio::test]
    async fn contradiction_judge_provider_change_returns_supersede() {
        let candidate = candidate("we deploy to fly.io", None);
        let detector = RrfPlusJudgeDetector::default();

        let conflict = detector
            .judge_candidates("we deploy to AWS", std::slice::from_ref(&candidate))
            .await
            .expect("judge contradiction");

        assert_eq!(conflict, Conflict::Supersede(candidate.uid));
    }

    #[tokio::test]
    async fn contradiction_judge_empty_candidates_returns_insert() {
        let detector = RrfPlusJudgeDetector::default();

        let conflict = detector
            .judge_candidates("we deploy to fly.io", &[])
            .await
            .expect("judge empty candidates");

        assert_eq!(conflict, Conflict::Insert);
    }

    #[tokio::test]
    async fn contradiction_judge_timeout_returns_indeterminate() {
        let detector = RrfPlusJudgeDetector::new(
            Arc::new(NoopReranker),
            Arc::new(SleepingJudge {
                delay: Duration::from_millis(80),
            }),
        )
        .with_budgets(
            Duration::from_millis(250),
            Duration::from_secs(5),
            Duration::from_millis(10),
        );
        let candidate = candidate("we deploy to fly.io", None);

        let conflict = detector
            .judge_candidates("we deploy to AWS", &[candidate])
            .await
            .expect("judge timeout");

        assert_eq!(conflict, Conflict::Indeterminate);
    }

    #[tokio::test]
    async fn contradiction_judge_cache_hit_is_sub_5ms() {
        let judge = Arc::new(CountingJudge {
            calls: AtomicUsize::new(0),
        });
        let detector = RrfPlusJudgeDetector::new(Arc::new(NoopReranker), judge.clone());
        let candidate = candidate("the API gateway is envoy", None);

        let first = detector
            .judge_candidates("the API gateway is envoy", std::slice::from_ref(&candidate))
            .await
            .expect("first judge");
        let started = Instant::now();
        let second = detector
            .judge_candidates("the API gateway is envoy", &[candidate])
            .await
            .expect("cached judge");

        assert_eq!(first, second);
        assert_eq!(judge.calls.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_millis(5));
    }

    #[derive(Debug)]
    struct SleepingJudge {
        delay: Duration,
    }

    #[async_trait]
    impl JudgeModel for SleepingJudge {
        async fn judge(
            &self,
            _prompt: &str,
            _fact_text: &str,
            _candidates: &[NodeIndexRow],
        ) -> Result<JudgeResponse> {
            sleep(self.delay).await;
            Ok(JudgeResponse {
                verdict: JudgeVerdict::Independent,
                candidate_uid: None,
                rationale: "slept".to_string(),
            })
        }
    }

    #[derive(Debug)]
    struct CountingJudge {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl JudgeModel for CountingJudge {
        async fn judge(
            &self,
            _prompt: &str,
            _fact_text: &str,
            candidates: &[NodeIndexRow],
        ) -> Result<JudgeResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(JudgeResponse {
                verdict: JudgeVerdict::Restates,
                candidate_uid: candidates.first().map(|candidate| candidate.uid),
                rationale: "counted".to_string(),
            })
        }
    }

    fn candidate(name: &str, properties_summary: Option<Value>) -> NodeIndexRow {
        NodeIndexRow {
            uid: Uuid::now_v7(),
            label: NodeLabel::Fact,
            workspace_id: Some(Uuid::now_v7().to_string()),
            user_id: None,
            scope: "workspace".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: Utc::now(),
            properties_summary,
            last_accessed_at: Utc::now(),
        }
    }
}

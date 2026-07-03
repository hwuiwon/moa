//! Hybrid contradiction detection for graph-memory ingestion.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::MoaConfig;
use moa_core::RlsContext;
use moa_db::ScopedConn;
use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_vector::{Error as VectorError, VECTOR_DIMENSION, VectorQuery, VectorStore};
use moa_providers::{
    COHERE_DEFAULT_RERANK_MODEL, CohereReranker, ConfiguredReranker, LlmChatClient, NoopReranker,
    Reranker, build_reranker_from_config,
};
use moka::future::Cache;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, PgPool, Row};
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
    scope: RlsContext,
    vector: Arc<dyn VectorStore>,
    assume_app_role: bool,
}

impl ContradictionContext {
    /// Creates a contradiction context using production RLS role assumptions.
    #[must_use]
    pub fn new(pool: PgPool, scope: RlsContext, vector: Arc<dyn VectorStore>) -> Self {
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
    pub fn for_app_role(pool: PgPool, scope: RlsContext, vector: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            scope,
            vector,
            assume_app_role: true,
        }
    }

    /// Returns the vector store used for KNN candidate retrieval.
    #[must_use]
    pub fn vector(&self) -> &dyn VectorStore {
        self.vector.as_ref()
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        Ok(ScopedConn::begin_as_app(&self.pool, &self.scope, self.assume_app_role).await?)
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
        pii_class: PiiClass,
        ctx: &ContradictionContext,
    ) -> Result<Conflict>;

    /// Checks one extracted embedded fact under the slow-path latency budget.
    async fn check_one_slow(
        &self,
        fact: &EmbeddedFact,
        ctx: &ContradictionContext,
    ) -> Result<Conflict>;
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

/// Chat-backed judge using the provider-owned LLM transport.
#[derive(Clone)]
struct LlmJudge {
    client: LlmChatClient,
}

impl LlmJudge {
    fn new(client: LlmChatClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl JudgeModel for LlmJudge {
    async fn judge(
        &self,
        prompt: &str,
        _fact_text: &str,
        _candidates: &[NodeIndexRow],
    ) -> Result<JudgeResponse> {
        let response = self.client.chat("", prompt).await?;
        parse_judge_response(&response)
    }
}

/// RRF plus rerank plus judge contradiction detector.
#[derive(Clone)]
pub struct RrfPlusJudgeDetector {
    reranker: Arc<dyn Reranker>,
    rerank_model: String,
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
        Self::new_with_model(reranker, COHERE_DEFAULT_RERANK_MODEL.to_string(), judge)
    }

    fn new_with_model(
        reranker: Arc<dyn Reranker>,
        rerank_model: String,
        judge: Arc<dyn JudgeModel>,
    ) -> Self {
        Self {
            reranker,
            rerank_model,
            judge,
            judge_cache: Cache::builder().max_capacity(CACHE_CAPACITY).build(),
            fast_budget: DEFAULT_FAST_BUDGET,
            slow_budget: DEFAULT_SLOW_BUDGET,
            judge_budget: DEFAULT_JUDGE_BUDGET,
        }
    }

    /// Creates a detector using shared MOA config.
    #[must_use]
    pub fn from_config_or_heuristic(config: &MoaConfig) -> Self {
        let configured = configured_reranker_or_noop(config);
        let api_key = cohere_judge_api_key(config);
        let judge = llm_judge_or_heuristic(
            api_key,
            &config.memory.extraction.model,
            config.memory.extraction.timeout_ms,
        );
        Self::new_with_model(configured.reranker, configured.model, judge)
    }

    /// Creates a detector using a direct Cohere API key and judge model.
    #[must_use]
    pub fn from_cohere_api_key_model_or_heuristic(
        api_key: &str,
        judge_model: &str,
        timeout_ms: u64,
    ) -> Self {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Self::new(NoopReranker::shared(), Arc::new(HeuristicJudge));
        }
        let reranker: Arc<dyn Reranker> = match CohereReranker::new(api_key.to_string()) {
            Ok(reranker) => Arc::new(reranker),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "falling back to no-op contradiction reranker"
                );
                NoopReranker::shared()
            }
        };
        let judge: Arc<dyn JudgeModel> = Arc::new(LlmJudge::new(LlmChatClient::from_api_key(
            SecretString::from(api_key.to_string()),
            judge_model,
            timeout_ms,
        )));
        Self::new(reranker, judge)
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
        pii_class: PiiClass,
        ctx: &ContradictionContext,
    ) -> Result<Vec<NodeIndexRow>> {
        let vector_hits =
            vector_candidate_uids(fact_text, embedding, label, pii_class, ctx).await?;
        let (lexical_hits, hydrated) =
            lexical_candidates_and_hydrate(fact_text, label, &vector_hits, ctx).await?;
        let ranked = rrf_fuse(
            &vector_hits,
            &lexical_hits,
            VECTOR_K.max(usize::try_from(LEXICAL_K).unwrap_or(VECTOR_K)),
        );
        Ok(ranked
            .into_iter()
            .take(VECTOR_K)
            .filter_map(|(uid, _score)| hydrated.get(&uid).cloned())
            .collect())
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
            .rerank(&self.rerank_model, fact_text, &documents, RERANK_TOP_N)
            .await
            .map_err(|error| IngestError::Rerank(error.to_string()))?;
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
        pii_class: PiiClass,
        ctx: &ContradictionContext,
    ) -> Result<Conflict> {
        let candidates = self
            .candidates(fact_text, embedding, label, pii_class, ctx)
            .await?;
        let candidates = self.rerank_top5(fact_text, &candidates).await?;
        self.judge_candidates(fact_text, &candidates).await
    }
}

fn configured_reranker_or_noop(config: &MoaConfig) -> ConfiguredReranker {
    build_reranker_from_config(config).unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "contradiction reranking disabled because reranker configuration is invalid"
        );
        ConfiguredReranker::noop()
    })
}

fn cohere_judge_api_key(config: &MoaConfig) -> &str {
    if config.providers.cohere.api_key.trim().is_empty() {
        &config.memory.extraction.api_key
    } else {
        &config.providers.cohere.api_key
    }
}

fn llm_judge_or_heuristic(
    api_key: &str,
    judge_model: &str,
    timeout_ms: u64,
) -> Arc<dyn JudgeModel> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Arc::new(HeuristicJudge);
    }
    Arc::new(LlmJudge::new(LlmChatClient::from_api_key(
        SecretString::from(api_key.to_string()),
        judge_model,
        timeout_ms,
    )))
}

impl Default for RrfPlusJudgeDetector {
    fn default() -> Self {
        let configured = ConfiguredReranker::noop();
        Self::new_with_model(
            configured.reranker,
            configured.model,
            Arc::new(HeuristicJudge),
        )
    }
}

#[async_trait]
impl ContradictionDetector for RrfPlusJudgeDetector {
    async fn check_one_fast(
        &self,
        fact_text: &str,
        embedding: &[f32],
        label: NodeLabel,
        pii_class: PiiClass,
        ctx: &ContradictionContext,
    ) -> Result<Conflict> {
        match timeout(
            self.fast_budget,
            self.run(fact_text, embedding, label, pii_class, ctx),
        )
        .await
        {
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
            self.run(
                fact_text,
                embedding,
                NodeLabel::Fact,
                fact.classified.pii_class,
                ctx,
            ),
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
    pii_class: PiiClass,
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
            max_pii_class: contradiction_candidate_max_pii_class(pii_class).to_string(),
            include_global: true,
            as_of: None,
        })
        .await?;
    Ok(hits.into_iter().map(|hit| hit.uid).collect())
}

fn contradiction_candidate_max_pii_class(pii_class: PiiClass) -> &'static str {
    pii_class.as_str()
}

/// Retrieves lexical candidates and hydrates them together with the vector
/// candidates in a single query.
///
/// This merges the former two round trips — a uid-only lexical search followed by
/// a hydrate of the reciprocal-rank-fused set — into one statement. The `lexical`
/// CTE reproduces the previous lexical selection exactly (same filter, ordering,
/// and `LIMIT`), assigning each kept row a `lexical_ord` so callers can rebuild
/// the ranked uid list for RRF. The outer select then hydrates the union of the
/// lexical rows and the caller-supplied active vector uids, returning full
/// [`NodeIndexRow`]s keyed by uid. Every returned row is active (`valid_to IS
/// NULL`), matching the old hydrate filter, so a fused uid absent from the map is
/// dropped exactly as before.
async fn lexical_candidates_and_hydrate(
    fact_text: &str,
    label: NodeLabel,
    vector_uids: &[Uuid],
    ctx: &ContradictionContext,
) -> Result<(Vec<Uuid>, HashMap<Uuid, NodeIndexRow>)> {
    let mut conn = ctx.begin().await?;
    let rows = sqlx::query(
        r#"
        WITH lexical AS (
            SELECT uid,
                   ROW_NUMBER() OVER (
                       ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                                last_accessed_at DESC
                   ) AS lexical_ord
            FROM moa.node_index
            WHERE valid_to IS NULL
              AND label = $2
              AND name_tsv @@ plainto_tsquery('simple', $1)
            ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                     last_accessed_at DESC
            LIMIT $3
        )
        SELECT n.uid, n.label, n.storage_partition_id, n.user_id, n.scope, n.name, n.pii_class,
               n.valid_to, n.valid_from, n.properties_summary, n.last_accessed_at,
               COALESCE(n.quality_score, 0.5) AS quality_score,
               lexical.lexical_ord AS lexical_ord
        FROM moa.node_index AS n
        LEFT JOIN lexical ON lexical.uid = n.uid
        WHERE n.valid_to IS NULL
          AND (lexical.lexical_ord IS NOT NULL OR n.uid = ANY($4))
        "#,
    )
    .bind(fact_text)
    .bind(label.as_str())
    .bind(LEXICAL_K)
    .bind(vector_uids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;

    let mut lexical_ranked: Vec<(i64, Uuid)> = Vec::new();
    let mut hydrated: HashMap<Uuid, NodeIndexRow> = HashMap::with_capacity(rows.len());
    for row in &rows {
        let lexical_ord: Option<i64> = row.try_get("lexical_ord")?;
        let node = NodeIndexRow::from_row(row).map_err(IngestError::from)?;
        if let Some(ord) = lexical_ord {
            lexical_ranked.push((ord, node.uid));
        }
        hydrated.insert(node.uid, node);
    }
    lexical_ranked.sort_by_key(|(ord, _)| *ord);
    let lexical_hits = lexical_ranked.into_iter().map(|(_, uid)| uid).collect();
    Ok((lexical_hits, hydrated))
}

fn conflict_from_judge(response: JudgeResponse) -> Conflict {
    match (response.verdict, response.candidate_uid) {
        (JudgeVerdict::Contradicts, Some(uid)) => Conflict::Supersede(uid),
        (JudgeVerdict::Restates, Some(uid)) => Conflict::Duplicate(uid),
        (JudgeVerdict::Independent, _) => Conflict::Insert,
        _ => Conflict::Indeterminate,
    }
}

fn parse_judge_response(response: &str) -> Result<JudgeResponse> {
    let stripped = strip_json_code_fence(response);
    serde_json::from_str::<JudgeResponse>(stripped)
        .map_err(|error| IngestError::Judge(format!("failed to parse judge JSON: {error}")))
}

fn strip_json_code_fence(response: &str) -> &str {
    let trimmed = response.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

fn heuristic_judge(fact_text: &str, candidates: &[NodeIndexRow]) -> JudgeResponse {
    let normalized_fact = normalize_fact_text(fact_text);
    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.valid_to.is_none())
        .find(|candidate| normalize_fact_text(&candidate.name) == normalized_fact)
    {
        return JudgeResponse {
            verdict: JudgeVerdict::Restates,
            candidate_uid: Some(candidate.uid),
            rationale: "normalized fact text matches candidate".to_string(),
        };
    }

    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.valid_to.is_none())
        .find(|candidate| structured_fact_match(fact_text, candidate, true))
    {
        return JudgeResponse {
            verdict: JudgeVerdict::Restates,
            candidate_uid: Some(candidate.uid),
            rationale: "same subject, predicate, and object".to_string(),
        };
    }

    if let Some(candidate) = candidates
        .iter()
        .filter(|candidate| candidate.valid_to.is_none())
        .find(|candidate| structured_fact_match(fact_text, candidate, false))
    {
        return JudgeResponse {
            verdict: JudgeVerdict::Contradicts,
            candidate_uid: Some(candidate.uid),
            rationale: "same subject and predicate with different object".to_string(),
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

fn structured_fact_match(
    fact_text: &str,
    candidate: &NodeIndexRow,
    require_same_object: bool,
) -> bool {
    let Some(fact) = fact_parts_from_text(fact_text) else {
        return false;
    };
    let Some(candidate) = fact_parts_from_candidate(candidate) else {
        return false;
    };
    fact.subject == candidate.subject
        && fact.predicate == candidate.predicate
        && (fact.object == candidate.object) == require_same_object
}

fn fact_parts_from_candidate(candidate: &NodeIndexRow) -> Option<FactParts> {
    if let Some(properties) = candidate.properties_summary.as_ref() {
        let subject = properties.get("subject").and_then(Value::as_str);
        let predicate = properties.get("predicate").and_then(Value::as_str);
        let object = properties.get("object").and_then(Value::as_str);
        if let (Some(subject), Some(predicate), Some(object)) = (subject, predicate, object) {
            return Some(FactParts {
                subject: normalize_fact_component(subject),
                predicate: normalize_fact_component(predicate),
                object: normalize_fact_component(object),
            });
        }
    }
    fact_parts_from_text(&candidate_text(candidate))
}

fn fact_parts_from_text(text: &str) -> Option<FactParts> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    let [subject, predicate, rest @ ..] = words.as_slice() else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    Some(FactParts {
        subject: normalize_fact_component(subject),
        predicate: normalize_fact_component(predicate),
        object: normalize_fact_component(&rest.join(" ")),
    })
}

fn normalize_fact(value: &str, allowed_extra: &[char]) -> String {
    value
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|ch: char| !ch.is_alphanumeric() && !allowed_extra.contains(&ch))
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn normalize_fact_component(value: &str) -> String {
    normalize_fact(value, &['_', '.'])
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FactParts {
    subject: String,
    predicate: String,
    object: String,
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
    normalize_fact(text, &['.'])
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use chrono::Utc;
    use moa_memory_graph::PiiClass;
    use secrecy::SecretString;
    use tokio::time::sleep;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[test]
    fn contradiction_candidate_query_uses_fact_pii_class() {
        // Pins: contradiction candidate retrieval does not widen beyond the fact's own PII class.
        assert_eq!(
            contradiction_candidate_max_pii_class(PiiClass::None),
            "none"
        );
        assert_eq!(contradiction_candidate_max_pii_class(PiiClass::Pii), "pii");
        assert_eq!(contradiction_candidate_max_pii_class(PiiClass::Phi), "phi");
        assert_eq!(
            contradiction_candidate_max_pii_class(PiiClass::Restricted),
            "restricted"
        );
    }

    #[tokio::test]
    async fn contradiction_judge_parsing_unchanged_on_shared_client() {
        // Pins: the shared Cohere chat transport preserves judge prompt content and JSON parsing.
        let candidate = candidate("we deploy to fly.io", None);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(body_string_contains("NEW FACT:"))
            .and(body_string_contains(candidate.uid.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": [{
                        "type": "text",
                        "text": format!(
                            "```json\n{{\"verdict\":\"CONTRADICTS\",\"candidate_uid\":\"{}\",\"rationale\":\"deployment provider changed\"}}\n```",
                            candidate.uid
                        )
                    }]
                }
            })))
            .mount(&server)
            .await;
        let client =
            LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", 1_000)
                .with_endpoint(format!("{}/v2/chat", server.uri()));
        let detector =
            RrfPlusJudgeDetector::new(Arc::new(NoopReranker), Arc::new(LlmJudge::new(client)));

        let conflict = detector
            .judge_candidates("we deploy to AWS", std::slice::from_ref(&candidate))
            .await
            .expect("judge should parse shared-client response");

        assert_eq!(conflict, Conflict::Supersede(candidate.uid));
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
            storage_partition_id: Some(Uuid::now_v7().to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: Utc::now(),
            properties_summary,
            last_accessed_at: Utc::now(),
            quality_score: 0.5,
        }
    }
}

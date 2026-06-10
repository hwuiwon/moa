//! Hermetic memory-retrieval evaluation runner.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::{
    PlanningCtx, QueryPlanner, QueryRetrievalCtx, parse_temporal, retrieve_for_query,
};
use moa_brain::retrieval::{
    CachedHybridRetriever, HybridRetriever, RankingConfig, RankingMode, RetrievalHit,
};
use moa_core::{MemoryScope, ScopeContext, WorkspaceId, traits::EmbeddingProvider};
use moa_memory_graph::{AgeGraphStore, GraphStore, PiiClass};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, IngestCtx, IngestError,
    SessionTurn, chunk_turn, extract_facts,
};
use moa_memory_pii::{PiiCategory, PiiClassifier, PiiError, PiiResult, PiiSpan, redact_text};
use moa_memory_vector::{PgvectorStore, VectorStore};
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    BootstrapConfig, CachedEmbeddingFixture, CachedEmbeddingProvider, ClusterBootstrapReport,
    CorpusManifest, DEFAULT_BOOTSTRAP_RESAMPLES, EmbeddingInput, ExtractionPrecisionCounts,
    GoldPiiStatus, GoldResolutionReport, LedgerFact, Probe, ProbeResult, ProbeType,
    RetrievalEvalReport, RetrievalMetrics, RetrievedCandidate, SyntheticSession,
    candidates_from_retrieval_hits, embedding_text_hash, read_embedding_inputs_jsonl,
    read_embeddings_jsonl, read_ledger_jsonl, read_manifest_json, read_probes_jsonl,
    read_sessions_jsonl, resolve_gold_nodes, validate_corpus,
};
use crate::{EvalError, Result};

/// Number of fused candidates collected for each probe before metric truncation.
pub const RETRIEVAL_EVAL_CANDIDATE_K: usize = 25;

/// Final recall cutoff used by the retrieval metrics.
pub const RETRIEVAL_EVAL_FINAL_K: usize = 4;

const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;

/// Options for running a hermetic memory-retrieval eval.
#[derive(Debug, Clone)]
pub struct MemoryRetrievalEvalOptions {
    corpus_dir: PathBuf,
    output_path: PathBuf,
    bootstrap_config: BootstrapConfig,
    reranker_enabled: bool,
    ranking_config: RankingConfig,
}

impl MemoryRetrievalEvalOptions {
    /// Creates options for a corpus directory and JSON report output path.
    pub fn new(corpus_dir: impl Into<PathBuf>, output_path: impl Into<PathBuf>) -> Self {
        Self {
            corpus_dir: corpus_dir.into(),
            output_path: output_path.into(),
            bootstrap_config: BootstrapConfig {
                resamples: DEFAULT_BOOTSTRAP_RESAMPLES,
                seed: 13_579,
            },
            reranker_enabled: false,
            ranking_config: RankingConfig::default(),
        }
    }

    /// Overrides the bootstrap settings used for confidence intervals.
    #[must_use]
    pub fn with_bootstrap_config(mut self, bootstrap_config: BootstrapConfig) -> Self {
        self.bootstrap_config = bootstrap_config;
        self
    }

    /// Overrides whether the eval should collect a post-rerank top-4 window.
    #[must_use]
    pub fn with_reranker(mut self, enabled: bool) -> Self {
        self.reranker_enabled = enabled;
        self
    }

    /// Overrides the deterministic ranking mode used by the eval run.
    #[must_use]
    pub fn with_ranking_mode(mut self, ranking_mode: RankingMode) -> Self {
        self.ranking_config.mode = ranking_mode;
        self
    }

    /// Overrides the full deterministic ranking configuration used by the eval run.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
        self
    }

    /// Returns the corpus directory.
    #[must_use]
    pub fn corpus_dir(&self) -> &Path {
        &self.corpus_dir
    }

    /// Returns the report output path.
    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Returns whether this eval run is collecting post-rerank retrieval.
    #[must_use]
    pub fn reranker_enabled(&self) -> bool {
        self.reranker_enabled
    }

    /// Returns the configured deterministic ranking mode.
    #[must_use]
    pub fn ranking_mode(&self) -> RankingMode {
        self.ranking_config.mode
    }

    /// Returns the configured deterministic ranking config.
    #[must_use]
    pub fn ranking_config(&self) -> &RankingConfig {
        &self.ranking_config
    }
}

/// JSON report written by `run-memory-retrieval-eval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrievalEvalReport {
    /// Corpus manifest loaded for this run.
    pub manifest: CorpusManifest,
    /// Number of candidates requested from production retrieval for metric scoring.
    pub candidate_k: usize,
    /// Final answer-context cutoff used by recall@4 and nDCG@4 metrics.
    pub final_k: usize,
    /// Whether the eval collected a post-rerank top-4 retrieval pass.
    #[serde(default)]
    pub reranker_enabled: bool,
    /// Aggregated retrieval metrics.
    pub metrics: RetrievalMetrics,
    /// Per-probe retrieval results with candidate attribution.
    pub probe_results: Vec<ProbeResult>,
    /// Cluster-bootstrap confidence intervals by user.
    pub bootstrap: Vec<ClusterBootstrapReport>,
    /// Probe ids that retrieved blocked facts.
    pub cross_user_leak_probe_ids: Vec<String>,
    /// Gold-resolution ingestion and fact-to-node mapping details.
    pub gold_resolution: GoldResolutionReport,
}

impl MemoryRetrievalEvalReport {
    fn from_retrieval_report(
        manifest: CorpusManifest,
        gold_resolution: GoldResolutionReport,
        retrieval: RetrievalEvalReport,
        reranker_enabled: bool,
    ) -> Self {
        Self {
            manifest,
            candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
            final_k: RETRIEVAL_EVAL_FINAL_K,
            reranker_enabled,
            metrics: retrieval.metrics,
            probe_results: retrieval.probe_results,
            bootstrap: retrieval.bootstrap,
            cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
            gold_resolution,
        }
    }
}

/// Runs the hermetic memory-retrieval eval and writes `report.json`.
pub async fn run_memory_retrieval_eval(
    options: MemoryRetrievalEvalOptions,
) -> Result<MemoryRetrievalEvalReport> {
    let corpus = LoadedMemoryEvalCorpus::load(options.corpus_dir()).await?;
    let store = IsolatedEvalStore::create().await?;
    let result = run_memory_retrieval_eval_in_store(&options, corpus, &store).await;
    let cleanup = store.cleanup().await;

    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(error = %cleanup_error, "failed to clean up memory retrieval eval store");
            Err(error)
        }
    }
}

async fn run_memory_retrieval_eval_in_store(
    options: &MemoryRetrievalEvalOptions,
    corpus: LoadedMemoryEvalCorpus,
    store: &IsolatedEvalStore,
) -> Result<MemoryRetrievalEvalReport> {
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    let embedder = Arc::new(cached_embedding_provider_for_corpus(&corpus)?);
    let ingest_ctx = store.ingest_ctx(embedder.clone());
    let mut gold_resolution =
        resolve_gold_nodes(ingest_ctx, &corpus.ledger, &corpus.sessions).await?;
    apply_eval_validity_windows(store.pool(), &mut gold_resolution).await?;
    let ranking_reference_time = Some(deterministic_ranking_reference_time(&corpus.ledger));
    let fact_ids_by_uid = fact_ids_by_uid(&gold_resolution);
    let extraction_precision =
        extraction_precision_counts(store.pool(), &corpus.ledger, &fact_ids_by_uid).await?;
    let gold_records_by_fact_id = gold_records_by_fact_id(&gold_resolution);
    let planner = QueryPlanner::new();
    let mut probe_results = Vec::with_capacity(corpus.probes.len());

    for probe in &corpus.probes {
        let retrieval = retrieve_probe(
            store.pool(),
            &planner,
            embedder.as_ref(),
            probe,
            options.reranker_enabled(),
            options.ranking_config().clone(),
            ranking_reference_time,
        )
        .await?;
        let candidates =
            candidates_from_retrieval_hits(&retrieval.pre_rerank_hits, &fact_ids_by_uid);
        let post_rerank_candidates =
            candidates_from_retrieval_hits(&retrieval.post_rerank_hits, &fact_ids_by_uid);
        probe_results.push(probe_result_for(
            probe,
            candidates,
            Some(post_rerank_candidates),
            retrieval.retrieval_latency_ms,
            &gold_records_by_fact_id,
        ));
    }

    let retrieval = super::aggregate_retrieval_eval_with_extraction_precision(
        &gold_resolution,
        probe_results,
        options.bootstrap_config,
        extraction_precision,
    );
    let report = MemoryRetrievalEvalReport::from_retrieval_report(
        corpus.manifest,
        gold_resolution,
        retrieval,
        options.reranker_enabled(),
    );
    write_report(options.output_path(), &report).await?;
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    Ok(report)
}

async fn cleanup_eval_graph_rows(pool: &PgPool, ledger: &[LedgerFact]) -> Result<()> {
    let workspace_ids = eval_workspace_ids(ledger);
    if workspace_ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM moa.ingest_dlq WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.ingest_dedup WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.graph_changelog WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.node_index WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.workspace_state WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    Ok(())
}

fn eval_workspace_ids(ledger: &[LedgerFact]) -> Vec<String> {
    ledger
        .iter()
        .map(|fact| fact.workspace_id.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn apply_eval_validity_windows(
    pool: &PgPool,
    gold_resolution: &mut GoldResolutionReport,
) -> Result<()> {
    for record in &mut gold_resolution.records {
        let Some(valid_to) = record.expected_valid_to else {
            continue;
        };
        if record.node_uids.is_empty() {
            continue;
        }

        sqlx::query("UPDATE moa.node_index SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE moa.embeddings SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await?;

        record.valid_to = Some(valid_to);
        record.active = false;
        for node in &mut record.nodes {
            node.valid_to = Some(valid_to);
            node.active = false;
        }
    }
    Ok(())
}

async fn retrieve_probe(
    pool: &PgPool,
    planner: &QueryPlanner,
    embedder: &dyn EmbeddingProvider,
    probe: &Probe,
    use_reranker: bool,
    ranking_config: RankingConfig,
    ranking_reference_time: Option<DateTime<Utc>>,
) -> Result<ProbeRetrieval> {
    let started = Instant::now();
    let scope = MemoryScope::User {
        workspace_id: probe.workspace_id.clone(),
        user_id: probe.user_id.clone(),
    };
    let scope_context = ScopeContext::new(scope.clone());
    let vector: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        scope_context.clone(),
    ));
    let graph_store = AgeGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        .with_vector_store(vector.clone());
    let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
    let hybrid = Arc::new(
        HybridRetriever::new(pool.clone(), graph.clone(), vector)
            .with_ranking_config(ranking_config)
            .with_assume_app_role(true),
    );
    let cached = CachedHybridRetriever::new_for_app_role(hybrid, pool.clone());
    let planning = PlanningCtx::new(scope, graph);

    let pre_rerank_hits = retrieve_probe_hits(
        planner,
        &planning,
        embedder,
        &cached,
        probe,
        ProbeHitOptions {
            k_final: RETRIEVAL_EVAL_CANDIDATE_K,
            use_reranker: false,
            ranking_reference_time,
        },
    )
    .await?;
    let post_rerank_hits = if use_reranker {
        retrieve_probe_hits(
            planner,
            &planning,
            embedder,
            &cached,
            probe,
            ProbeHitOptions {
                k_final: RETRIEVAL_EVAL_FINAL_K,
                use_reranker: true,
                ranking_reference_time,
            },
        )
        .await?
    } else {
        pre_rerank_hits
            .iter()
            .take(RETRIEVAL_EVAL_FINAL_K)
            .cloned()
            .collect()
    };

    Ok(ProbeRetrieval {
        pre_rerank_hits,
        post_rerank_hits,
        retrieval_latency_ms: duration_ms_u64(started.elapsed()),
    })
}

async fn retrieve_probe_hits(
    planner: &QueryPlanner,
    planning: &PlanningCtx,
    embedder: &dyn EmbeddingProvider,
    cached: &CachedHybridRetriever,
    probe: &Probe,
    options: ProbeHitOptions,
) -> Result<Vec<RetrievalHit>> {
    let mut retrieval_ctx =
        QueryRetrievalCtx::new(planner, planning, embedder, cached, PiiClass::Restricted)
            .with_k_final(options.k_final)
            .with_reranker(options.use_reranker);
    if let Some(ranking_reference_time) = options.ranking_reference_time {
        retrieval_ctx = retrieval_ctx.with_ranking_reference_time(ranking_reference_time);
    }

    retrieve_for_query(&probe.query, &retrieval_ctx)
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory retrieval failed for probe {}: {error}",
                probe.probe_id
            ))
        })
}

fn deterministic_ranking_reference_time(ledger: &[LedgerFact]) -> DateTime<Utc> {
    ledger
        .iter()
        .map(|fact| fact.valid_from)
        .max()
        .unwrap_or_else(Utc::now)
        + chrono::Duration::days(7)
}

struct ProbeRetrieval {
    pre_rerank_hits: Vec<RetrievalHit>,
    post_rerank_hits: Vec<RetrievalHit>,
    retrieval_latency_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProbeHitOptions {
    k_final: usize,
    use_reranker: bool,
    ranking_reference_time: Option<DateTime<Utc>>,
}

fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

fn probe_result_for(
    probe: &Probe,
    candidates: Vec<RetrievedCandidate>,
    post_rerank_candidates: Option<Vec<RetrievedCandidate>>,
    retrieval_latency_ms: u64,
    gold_records_by_fact_id: &HashMap<String, super::GoldNodeRecord>,
) -> ProbeResult {
    let final_candidates = post_rerank_candidates.as_deref().unwrap_or(&candidates);
    let expected_found_at_4 = all_expected_found_at_k(
        final_candidates,
        &probe.expected_fact_ids,
        RETRIEVAL_EVAL_FINAL_K,
    );
    let blocked_leaked = any_blocked_found_at_k(
        &candidates,
        &probe.blocked_fact_ids,
        RETRIEVAL_EVAL_CANDIDATE_K,
    );
    let pii_redacted = pii_redacted_for_probe(probe, gold_records_by_fact_id);
    let answer_faithful =
        answer_faithful_for_probe(probe, expected_found_at_4, blocked_leaked, pii_redacted);
    let (temporal_filter_parsed, temporal_filter_matches_as_of) = temporal_parse_diagnostics(probe);

    ProbeResult {
        probe_id: probe.probe_id.clone(),
        user_id: probe.user_id.as_str().to_string(),
        probe_type: probe.probe_type,
        expected_fact_ids: probe.expected_fact_ids.clone(),
        blocked_fact_ids: probe.blocked_fact_ids.clone(),
        candidates,
        post_rerank_candidates,
        retrieval_latency_ms,
        answer_faithful,
        abstention_correct: abstention_correct_for_probe(probe, blocked_leaked),
        pii_redacted,
        temporal_as_of_correct: temporal_as_of_correct_for_probe(probe, expected_found_at_4),
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
    }
}

fn answer_faithful_for_probe(
    probe: &Probe,
    expected_found_at_4: bool,
    blocked_leaked: bool,
    pii_redacted: Option<bool>,
) -> Option<bool> {
    match probe.probe_type {
        ProbeType::Abstention | ProbeType::CrossUserIsolation => Some(!blocked_leaked),
        ProbeType::PiiRedaction => pii_redacted.map(|redacted| redacted && expected_found_at_4),
        _ if probe.expected_fact_ids.is_empty() => None,
        _ => Some(expected_found_at_4),
    }
}

fn abstention_correct_for_probe(probe: &Probe, blocked_leaked: bool) -> Option<bool> {
    match probe.probe_type {
        ProbeType::Abstention | ProbeType::CrossUserIsolation => Some(!blocked_leaked),
        _ => None,
    }
}

fn temporal_as_of_correct_for_probe(probe: &Probe, expected_found_at_4: bool) -> Option<bool> {
    if probe.probe_type == ProbeType::TemporalAsOf {
        Some(expected_found_at_4)
    } else {
        None
    }
}

fn temporal_parse_diagnostics(probe: &Probe) -> (Option<bool>, Option<bool>) {
    if probe.probe_type != ProbeType::TemporalAsOf {
        return (None, None);
    }

    let parsed = parse_temporal(&probe.query);
    (
        Some(parsed.is_some()),
        parsed.map(|instant| Some(instant) == probe.as_of),
    )
}

fn pii_redacted_for_probe(
    probe: &Probe,
    gold_records_by_fact_id: &HashMap<String, super::GoldNodeRecord>,
) -> Option<bool> {
    if probe.probe_type != ProbeType::PiiRedaction {
        return None;
    }

    let mut resolved_pii = false;
    for fact_id in &probe.expected_fact_ids {
        let Some(record) = gold_records_by_fact_id.get(fact_id) else {
            continue;
        };
        match record.pii_status {
            GoldPiiStatus::Unredacted | GoldPiiStatus::Mixed => return Some(false),
            GoldPiiStatus::Redacted => resolved_pii = true,
            GoldPiiStatus::NotExpected | GoldPiiStatus::NotResolved => {}
        }
    }
    resolved_pii.then_some(true)
}

fn all_expected_found_at_k(
    candidates: &[RetrievedCandidate],
    expected: &[String],
    k: usize,
) -> bool {
    if expected.is_empty() {
        return false;
    }
    expected.iter().all(|expected_fact_id| {
        candidates.iter().any(|candidate| {
            candidate.rank > 0
                && candidate.rank <= k
                && candidate.fact_id.as_deref() == Some(expected_fact_id.as_str())
        })
    })
}

fn any_blocked_found_at_k(candidates: &[RetrievedCandidate], blocked: &[String], k: usize) -> bool {
    if blocked.is_empty() {
        return false;
    }
    candidates.iter().any(|candidate| {
        candidate.rank > 0
            && candidate.rank <= k
            && candidate
                .fact_id
                .as_ref()
                .is_some_and(|fact_id| blocked.contains(fact_id))
    })
}

fn fact_ids_by_uid(gold_resolution: &GoldResolutionReport) -> HashMap<Uuid, String> {
    let mut fact_ids = HashMap::new();
    for record in &gold_resolution.records {
        for uid in &record.node_uids {
            fact_ids
                .entry(*uid)
                .or_insert_with(|| record.fact_id.clone());
        }
    }
    fact_ids
}

async fn extraction_precision_counts(
    pool: &PgPool,
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<ExtractionPrecisionCounts> {
    let workspace_ids = eval_workspace_ids(ledger);
    let total_fact_nodes = if workspace_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE label = 'Fact' AND workspace_id = ANY($1)",
        )
        .bind(&workspace_ids)
        .fetch_one(pool)
        .await?
    };
    Ok(ExtractionPrecisionCounts {
        mapped_fact_nodes: fact_ids_by_uid.len(),
        total_fact_nodes: usize::try_from(total_fact_nodes).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "stored Fact node count {total_fact_nodes} cannot fit usize"
            ))
        })?,
    })
}

fn gold_records_by_fact_id(
    gold_resolution: &GoldResolutionReport,
) -> HashMap<String, super::GoldNodeRecord> {
    gold_resolution
        .records
        .iter()
        .map(|record| (record.fact_id.clone(), record.clone()))
        .collect()
}

async fn write_report(path: &Path, report: &MemoryRetrievalEvalReport) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    let json = serde_json::to_vec_pretty(report)?;
    tokio::fs::write(path, json)
        .await
        .map_err(|source| io_error(path, source))
}

fn cached_embedding_provider_for_corpus(
    corpus: &LoadedMemoryEvalCorpus,
) -> Result<CachedEmbeddingProvider> {
    let mut fixtures_by_hash = BTreeMap::<String, CachedEmbeddingFixture>::new();
    for fixture in corpus.embeddings.clone() {
        insert_fixture(&mut fixtures_by_hash, fixture)?;
    }
    ensure_embedding_input_coverage(&corpus.embedding_inputs, &fixtures_by_hash)?;

    for text in extracted_embedding_texts(&corpus.sessions)? {
        insert_fixture(
            &mut fixtures_by_hash,
            CachedEmbeddingFixture::for_text(&text),
        )?;
    }

    CachedEmbeddingProvider::from_fixtures(fixtures_by_hash.into_values().collect())
}

fn insert_fixture(
    fixtures_by_hash: &mut BTreeMap<String, CachedEmbeddingFixture>,
    fixture: CachedEmbeddingFixture,
) -> Result<()> {
    match fixtures_by_hash.get(&fixture.text_hash) {
        Some(existing) if existing == &fixture => Ok(()),
        Some(_) => Err(EvalError::InvalidConfig(format!(
            "cached embedding text_hash {} has conflicting fixture values",
            fixture.text_hash
        ))),
        None => {
            fixtures_by_hash.insert(fixture.text_hash.clone(), fixture);
            Ok(())
        }
    }
}

fn ensure_embedding_input_coverage(
    inputs: &[EmbeddingInput],
    fixtures_by_hash: &BTreeMap<String, CachedEmbeddingFixture>,
) -> Result<()> {
    for input in inputs {
        let text_hash = embedding_text_hash(&input.text);
        if !fixtures_by_hash.contains_key(&text_hash) {
            return Err(EvalError::InvalidConfig(format!(
                "embeddings.jsonl is missing text_hash {text_hash} for embedding input {}",
                input.input_id
            )));
        }
    }
    Ok(())
}

fn extracted_embedding_texts(sessions: &[SyntheticSession]) -> Result<Vec<String>> {
    let finalized_at = DateTime::<Utc>::from_timestamp(0, 0).ok_or_else(|| {
        EvalError::InvalidConfig("failed to construct deterministic eval timestamp".to_string())
    })?;
    let mut texts = BTreeMap::<String, ()>::new();
    for session in sessions {
        for turn in &session.turns {
            let session_turn = SessionTurn {
                workspace_id: session.workspace_id.clone(),
                user_id: session.user_id.clone(),
                session_id: session.session_id,
                turn_seq: turn.turn_seq,
                transcript: turn.transcript.clone(),
                dominant_pii_class: "none".to_string(),
                finalized_at,
            };
            let chunks = chunk_turn(&session_turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS)
                .map_err(|error| {
                    EvalError::InvalidConfig(format!(
                        "failed to chunk synthetic session {} turn {}: {error}",
                        session.session_id, turn.turn_seq
                    ))
                })?;
            for fact in extract_facts(&chunks) {
                texts.insert(fact.summary.clone(), ());
                let pii = deterministic_pii_result(&fact.summary);
                let redacted = redact_text(&fact.summary, &pii.spans);
                if redacted != fact.summary {
                    texts.insert(redacted, ());
                }
            }
        }
    }
    Ok(texts.into_keys().collect())
}

struct LoadedMemoryEvalCorpus {
    manifest: CorpusManifest,
    ledger: Vec<LedgerFact>,
    sessions: Vec<SyntheticSession>,
    probes: Vec<Probe>,
    embedding_inputs: Vec<EmbeddingInput>,
    embeddings: Vec<CachedEmbeddingFixture>,
}

impl LoadedMemoryEvalCorpus {
    async fn load(corpus_dir: &Path) -> Result<Self> {
        let manifest = read_manifest_json(&corpus_dir.join("manifest.json")).await?;
        let ledger = read_ledger_jsonl(&corpus_dir.join("ledger.jsonl")).await?;
        let sessions = read_sessions_jsonl(&corpus_dir.join("sessions.jsonl")).await?;
        let probes = read_probes_jsonl(&corpus_dir.join("probes.jsonl"), &ledger).await?;
        validate_corpus(&manifest, &ledger, &sessions, &probes)?;
        let embedding_inputs = read_embedding_inputs_jsonl(
            &corpus_dir.join("embedding_inputs.jsonl"),
            &ledger,
            &probes,
        )
        .await?;
        let embeddings = read_embeddings_jsonl(&corpus_dir.join("embeddings.jsonl")).await?;
        Ok(Self {
            manifest,
            ledger,
            sessions,
            probes,
            embedding_inputs,
            embeddings,
        })
    }
}

struct IsolatedEvalStore {
    store: PostgresSessionStore,
    database_url: String,
    schema_name: String,
}

impl IsolatedEvalStore {
    async fn create() -> Result<Self> {
        let database_url = test_database_url()?;
        let schema_name = format!("moa_memory_eval_{}", Uuid::now_v7().simple());
        let store = PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?;
        Ok(Self {
            store,
            database_url,
            schema_name,
        })
    }

    fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    fn ingest_ctx(&self, embedder: Arc<dyn EmbeddingProvider>) -> IngestCtx {
        let workspace_id = WorkspaceId::new(format!("memory-eval-runner-{}", self.schema_name));
        let scope = ScopeContext::workspace(workspace_id);
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool().clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(self.pool().clone(), scope)
                .with_vector_store(vector.clone()),
        );
        IngestCtx::new(
            self.pool().clone(),
            graph,
            vector,
            embedder,
            Arc::new(MemoryEvalPiiClassifier),
            Arc::new(InsertOnlyContradictionDetector),
        )
    }

    async fn cleanup(self) -> Result<()> {
        let pool = self.store.pool().clone();
        drop(self.store);
        pool.close().await;
        moa_session::testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(EvalError::from)
    }
}

fn test_database_url() -> Result<String> {
    env::var("MOA_TEST_POSTGRES_URL")
        .or_else(|_| env::var("TEST_DATABASE_URL"))
        .or_else(|_| env::var("DATABASE_URL"))
        .map_err(|_| {
            EvalError::InvalidConfig(
                "MOA_TEST_POSTGRES_URL, TEST_DATABASE_URL, or DATABASE_URL must be set for memory retrieval eval"
                    .to_string(),
            )
        })
}

#[derive(Debug, Clone)]
struct MemoryEvalPiiClassifier;

#[async_trait]
impl PiiClassifier for MemoryEvalPiiClassifier {
    async fn classify(&self, text: &str) -> std::result::Result<PiiResult, PiiError> {
        Ok(deterministic_pii_result(text))
    }
}

fn deterministic_pii_result(text: &str) -> PiiResult {
    let mut spans = Vec::new();
    let mut cursor = 0_usize;
    for token in text.split_whitespace() {
        let Some(offset) = text[cursor..].find(token) else {
            continue;
        };
        let start = cursor + offset;
        let end = start + token.len();
        cursor = end;
        if token.contains('@') {
            spans.push(PiiSpan {
                start,
                end,
                category: PiiCategory::Email,
                confidence: 0.95,
            });
        } else if token.contains("sk-") || token.to_ascii_lowercase().contains("secret") {
            spans.push(PiiSpan {
                start,
                end,
                category: PiiCategory::Secret,
                confidence: 0.90,
            });
        }
    }

    PiiResult {
        class: if spans.is_empty() {
            PiiClass::None
        } else {
            PiiClass::Pii
        },
        spans,
        model_version: "memory-eval-deterministic-pii-v1".to_string(),
        abstained: false,
    }
}

#[derive(Debug, Clone)]
struct InsertOnlyContradictionDetector;

#[async_trait]
impl ContradictionDetector for InsertOnlyContradictionDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: moa_memory_graph::NodeLabel,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

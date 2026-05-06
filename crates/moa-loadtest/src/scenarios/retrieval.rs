//! Graph-memory retrieval performance gate scenario.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use futures_util::future::try_join_all;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use moa_brain::{
    planning::{PlannedQuery, Strategy},
    retrieval::{CachedHybridRetriever, HybridRetriever, RetrievalRequest},
};
use moa_core::{MemoryScope, ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_vector::{CohereV4Embedder, Embedder, PgvectorStore, VECTOR_DIMENSION};
use moa_session::{PostgresSessionStore, testing::cleanup_test_schema};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};
use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

const QUERY_SEED: u64 = 0xDEAD_BEEF;
const HISTOGRAM_BUCKETS_SECONDS: &[f64] = &[0.005, 0.010, 0.020, 0.040, 0.080, 0.160, 0.320, 0.640];
const LEG_CEILINGS_MS: &[(&str, f64)] = &[
    ("cache_hit", 5.0),
    ("embedder", 30.0),
    ("vector", 15.0),
    ("lexical", 10.0),
    ("graph", 15.0),
    ("rrf_rerank", 10.0),
];

/// Performance gate configuration parsed by the `perf_gate` binary.
#[derive(Debug, Clone)]
pub struct PerfGateConfig {
    /// Number of tenant workspaces to seed and query.
    pub workspaces: usize,
    /// Number of facts to seed per workspace.
    pub facts_per_workspace: usize,
    /// Target query rate.
    pub qps: u32,
    /// Load window duration.
    pub duration: Duration,
    /// Hard P95 latency budget in milliseconds.
    pub p95_budget_ms: u64,
    /// Soft P99 latency target in milliseconds.
    pub p99_soft_target_ms: u64,
    /// Minimum cache hit rate for the repeated-query slice.
    pub cache_hit_floor: f64,
    /// Prometheus textfile output path.
    pub prom_out: PathBuf,
}

/// Runs the graph-memory retrieval performance gate.
pub async fn run_perf_gate(cfg: PerfGateConfig) -> Result<()> {
    let result = async {
        validate_config(&cfg)?;
        validate_hardware_floor()?;
        let metrics = install_metrics_recorder()?;
        run_perf_gate_inner(&cfg, &metrics).await
    }
    .await;
    if let Err(error) = &result {
        let snapshot = format!(
            "# TYPE perf_gate_infrastructure_error gauge\nperf_gate_infrastructure_error 1\n# error: {}\n",
            sanitize_prom_comment(&error.to_string())
        );
        write_snapshot(&cfg.prom_out, &snapshot).await?;
    }
    result
}

async fn run_perf_gate_inner(cfg: &PerfGateConfig, metrics: &PrometheusHandle) -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required for perf_gate Postgres/AGE/pgvector access")?;
    let api_key = std::env::var("COHERE_API_KEY")
        .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
        .context("COHERE_API_KEY or MOA_COHERE_API_KEY is required for perf_gate embeddings")?;
    let embedder = Arc::new(CohereV4Embedder::new(SecretString::from(api_key)));

    let mut stack = Stack::up(&database_url, embedder).await?;
    let run_result: Result<()> = async {
        stack.seed_workspaces(cfg).await?;
        stack.build_retrievers();
        warm_cache(&stack, cfg).await?;

        let before_load = metrics.render();
        let stop_attacks = Arc::new(AtomicBool::new(false));
        let attack_handle = spawn_cross_tenant_attacks(stack.clone(), stop_attacks.clone());
        let report = drive_load(stack.clone(), cfg).await?;
        stop_attacks.store(true, Ordering::Relaxed);
        let leaks = attack_handle.await.context("RLS attack task panicked")??;
        let after_load = metrics.render();
        let report = report.with_metrics_delta(&before_load, &after_load, cfg);
        let snapshot = render_prometheus(metrics, &report, &leaks);
        write_snapshot(&cfg.prom_out, &snapshot).await?;
        write_stdout(&print_summary_table(&report, &leaks))?;
        enforce_gates(cfg, &report, &leaks)?;
        Ok(())
    }
    .await;
    let cleanup_result = stack.cleanup().await;
    run_result?;
    cleanup_result
}

#[derive(Clone)]
struct Stack {
    database_url: String,
    schema_name: String,
    pool: PgPool,
    embedder: Arc<CohereV4Embedder>,
    workspaces: Vec<WorkspaceFixture>,
    retrievers: Vec<Arc<WorkspaceRetriever>>,
}

impl Stack {
    async fn up(database_url: &str, embedder: Arc<CohereV4Embedder>) -> Result<Self> {
        let schema_name = format!("perf_gate_{}", Uuid::now_v7().simple());
        let store = PostgresSessionStore::new_in_schema(database_url, &schema_name)
            .await
            .map_err(|error| anyhow!("failed to initialize perf schema: {error}"))?;
        let pool = store.pool().clone();
        drop(store);
        Ok(Self {
            database_url: database_url.to_string(),
            schema_name,
            pool,
            embedder,
            workspaces: Vec::new(),
            retrievers: Vec::new(),
        })
    }

    async fn seed_workspaces(&mut self, cfg: &PerfGateConfig) -> Result<()> {
        let mut fixtures = Vec::with_capacity(cfg.workspaces);
        for workspace_index in 0..cfg.workspaces {
            let workspace_id = Uuid::now_v7();
            let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
            let vector = Arc::new(PgvectorStore::new_for_app_role(
                self.pool.clone(),
                scope.clone(),
            ));
            let graph = AgeGraphStore::scoped_for_app_role(self.pool.clone(), scope)
                .with_vector_store(vector);

            let fact_texts = (0..cfg.facts_per_workspace)
                .map(|fact_index| fact_text(workspace_index, fact_index))
                .collect::<Vec<_>>();
            let embeddings = embed_texts(self.embedder.as_ref(), &fact_texts).await?;
            let mut first_uid = None;
            for (fact_index, (text, embedding)) in
                fact_texts.into_iter().zip(embeddings).enumerate()
            {
                let uid = Uuid::now_v7();
                if first_uid.is_none() {
                    first_uid = Some(uid);
                }
                graph
                    .create_node(NodeWriteIntent {
                        uid,
                        label: NodeLabel::Fact,
                        workspace_id: Some(workspace_id.to_string()),
                        user_id: None,
                        scope: "workspace".to_string(),
                        name: text.clone(),
                        properties: json!({
                            "summary": text,
                            "workspace_index": workspace_index,
                            "fact_index": fact_index,
                            "source": "perf_gate",
                        }),
                        pii_class: PiiClass::None,
                        confidence: Some(0.9),
                        valid_from: Utc::now(),
                        embedding: Some(embedding),
                        embedding_model: Some(self.embedder.model_name().to_string()),
                        embedding_model_version: Some(self.embedder.model_version()),
                        actor_id: Uuid::now_v7().to_string(),
                        actor_kind: "system".to_string(),
                    })
                    .await
                    .map_err(|error| anyhow!("failed to seed graph node: {error}"))?;
            }
            seed_attack_dlq(&self.pool, workspace_id).await?;
            fixtures.push(WorkspaceFixture {
                workspace_id,
                first_uid: first_uid.context("workspace seeded no facts")?,
            });
        }
        self.workspaces = fixtures;
        Ok(())
    }

    fn build_retrievers(&mut self) {
        self.retrievers = self
            .workspaces
            .iter()
            .map(|workspace| {
                Arc::new(WorkspaceRetriever::new(
                    self.pool.clone(),
                    workspace.workspace_id,
                ))
            })
            .collect();
    }

    async fn cleanup(self) -> Result<()> {
        self.pool.close().await;
        cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(|error| anyhow!("failed to cleanup perf schema: {error}"))
    }
}

#[derive(Debug, Clone)]
struct WorkspaceFixture {
    workspace_id: Uuid,
    first_uid: Uuid,
}

struct WorkspaceRetriever {
    scope: MemoryScope,
    cache: CachedHybridRetriever,
}

impl WorkspaceRetriever {
    fn new(pool: PgPool, workspace_id: Uuid) -> Self {
        let workspace = WorkspaceId::new(workspace_id.to_string());
        let scope_ctx = ScopeContext::workspace(workspace.clone());
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            pool.clone(),
            scope_ctx.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(pool.clone(), scope_ctx)
                .with_vector_store(vector.clone()),
        );
        let hybrid = HybridRetriever::new(pool.clone(), graph, vector).with_assume_app_role(true);
        Self {
            scope: MemoryScope::Workspace {
                workspace_id: workspace,
            },
            cache: CachedHybridRetriever::new_for_app_role(Arc::new(hybrid), pool),
        }
    }

    async fn retrieve(&self, query: &RetrievalQuery) -> Result<usize> {
        let planned = PlannedQuery {
            strategy: Strategy::Both,
            seeds: Vec::new(),
            label_hint: Some(vec![NodeLabel::Fact]),
            scope: self.scope.clone(),
            scope_ancestors: self.scope.ancestors(),
            temporal_filter: None,
        };
        let request = RetrievalRequest {
            seeds: Vec::new(),
            query_text: query.text.clone(),
            query_embedding: query.embedding.clone(),
            scope: self.scope.clone(),
            label_filter: Some(vec![NodeLabel::Fact]),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: Some(Strategy::Both),
        };
        let hits = self
            .cache
            .retrieve(&planned, request)
            .await
            .map_err(|error| anyhow!("retrieval failed: {error}"))?;
        Ok(hits.len())
    }
}

#[derive(Debug, Clone)]
struct RetrievalQuery {
    workspace_index: usize,
    text: String,
    embedding: Vec<f32>,
    is_repeated: bool,
}

#[derive(Debug, Clone)]
struct QueryTemplate {
    workspace_index: usize,
    text: String,
    is_repeated: bool,
}

#[derive(Debug, Clone)]
struct LoadReport {
    total_requests: usize,
    ok_requests: usize,
    failed_requests: usize,
    repeated_requests: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    cache_hit_rate: f64,
    cache_hit_p95_ms: f64,
    embedder_p95_ms: f64,
    vector_p95_ms: f64,
    lexical_p95_ms: f64,
    graph_p95_ms: f64,
    rrf_rerank_p95_ms: f64,
}

impl LoadReport {
    fn from_outcomes(outcomes: Vec<QueryOutcome>) -> Self {
        let total_requests = outcomes.len();
        let ok_requests = outcomes.iter().filter(|outcome| outcome.ok).count();
        let failed_requests = total_requests.saturating_sub(ok_requests);
        let repeated_requests = outcomes
            .iter()
            .filter(|outcome| outcome.is_repeated)
            .count();
        let mut latencies = outcomes
            .iter()
            .filter(|outcome| outcome.ok)
            .map(|outcome| outcome.elapsed.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>();
        latencies.sort_by(f64::total_cmp);
        Self {
            total_requests,
            ok_requests,
            failed_requests,
            repeated_requests,
            p50_ms: percentile_sorted(&latencies, 0.50),
            p95_ms: percentile_sorted(&latencies, 0.95),
            p99_ms: percentile_sorted(&latencies, 0.99),
            cache_hit_rate: 0.0,
            cache_hit_p95_ms: 0.0,
            embedder_p95_ms: 0.0,
            vector_p95_ms: 0.0,
            lexical_p95_ms: 0.0,
            graph_p95_ms: 0.0,
            rrf_rerank_p95_ms: 0.0,
        }
    }

    fn with_metrics_delta(mut self, before: &str, after: &str, cfg: &PerfGateConfig) -> Self {
        let hit_before = prom_counter(before, "moa_retrieval_cache_total", &[("outcome", "hit")]);
        let hit_after = prom_counter(after, "moa_retrieval_cache_total", &[("outcome", "hit")]);
        let cache_hits = (hit_after - hit_before).max(0.0);
        self.cache_hit_rate = if self.repeated_requests == 0 {
            0.0
        } else {
            (cache_hits / self.repeated_requests as f64).min(1.0)
        };
        self.cache_hit_p95_ms =
            prom_histogram_p95_ms(after, "moa_retrieval_cache_hit_seconds", &[]);
        self.embedder_p95_ms = prom_histogram_p95_ms(after, "moa_retrieval_embedder_seconds", &[]);
        self.vector_p95_ms =
            prom_histogram_p95_ms(after, "moa_retrieval_leg_seconds", &[("leg", "vector")]);
        self.lexical_p95_ms =
            prom_histogram_p95_ms(after, "moa_retrieval_leg_seconds", &[("leg", "lexical")]);
        self.graph_p95_ms =
            prom_histogram_p95_ms(after, "moa_retrieval_leg_seconds", &[("leg", "graph")]);
        self.rrf_rerank_p95_ms =
            prom_histogram_p95_ms(after, "moa_retrieval_rrf_rerank_seconds", &[]);

        if self.cache_hit_p95_ms == 0.0 && cfg.duration <= Duration::from_secs(1) {
            self.cache_hit_rate = 0.0;
        }
        self
    }

    fn leg_breaches(&self) -> Vec<(&'static str, f64, f64)> {
        [
            ("cache_hit", self.cache_hit_p95_ms),
            ("embedder", self.embedder_p95_ms),
            ("vector", self.vector_p95_ms),
            ("lexical", self.lexical_p95_ms),
            ("graph", self.graph_p95_ms),
            ("rrf_rerank", self.rrf_rerank_p95_ms),
        ]
        .into_iter()
        .filter_map(|(leg, p95)| {
            let ceiling = LEG_CEILINGS_MS
                .iter()
                .find_map(|(name, ceiling)| (*name == leg).then_some(*ceiling))?;
            (p95 == 0.0 || p95 > ceiling).then_some((leg, p95, ceiling))
        })
        .collect()
    }
}

#[derive(Debug, Clone)]
struct QueryOutcome {
    ok: bool,
    elapsed: Duration,
    is_repeated: bool,
}

#[derive(Debug, Clone, Default)]
struct LeakReport {
    count: usize,
    attempts: usize,
    failures: Vec<String>,
}

fn validate_config(cfg: &PerfGateConfig) -> Result<()> {
    if cfg.workspaces < 2 {
        bail!("perf_gate requires at least 2 workspaces for concurrent RLS probes");
    }
    if cfg.facts_per_workspace == 0 {
        bail!("facts_per_workspace must be greater than zero");
    }
    if cfg.qps == 0 {
        bail!("qps must be greater than zero");
    }
    if !(0.0..=1.0).contains(&cfg.cache_hit_floor) {
        bail!("cache_hit_floor must be between 0 and 1");
    }
    Ok(())
}

fn validate_hardware_floor() -> Result<()> {
    let cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    if cpus < 8 {
        bail!("hardware floor unmet: expected at least 8 vCPU, found {cpus}");
    }

    validate_x86_avx2()?;
    if let Some(memory_gb) = linux_memory_gb()?
        && memory_gb < 32
    {
        bail!("hardware floor unmet: expected at least 32 GB memory, found {memory_gb} GB");
    }
    Ok(())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn validate_x86_avx2() -> Result<()> {
    if !std::is_x86_feature_detected!("avx2") {
        bail!("hardware floor unmet: AVX2 is required");
    }
    Ok(())
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn validate_x86_avx2() -> Result<()> {
    bail!("hardware floor unmet: x86_64 with AVX2 is required");
}

fn linux_memory_gb() -> Result<Option<u64>> {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return Ok(None);
    };
    let Some(line) = meminfo.lines().find(|line| line.starts_with("MemTotal:")) else {
        return Ok(None);
    };
    let kb = line
        .split_whitespace()
        .nth(1)
        .context("MemTotal line missing value")?
        .parse::<u64>()
        .context("MemTotal value was not an integer")?;
    Ok(Some(kb / 1024 / 1024))
}

fn install_metrics_recorder() -> Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .set_buckets(HISTOGRAM_BUCKETS_SECONDS)
        .context("failed to configure perf histogram buckets")?
        .set_buckets_for_metric(
            Matcher::Full("perf_gate_cache_hit_rate".to_string()),
            &[0.50, 0.60, 0.70, 0.80, 0.90, 0.95, 1.0],
        )
        .context("failed to configure cache hit rate buckets")?
        .install_recorder()
        .context("failed to install Prometheus metrics recorder")
}

async fn embed_texts(embedder: &dyn Embedder, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let started = Instant::now();
    let embeddings = embedder
        .embed(texts)
        .await
        .map_err(|error| anyhow!("embedding provider failed: {error}"))?;
    let per_text = if texts.is_empty() {
        0.0
    } else {
        started.elapsed().as_secs_f64() / texts.len() as f64
    };
    for _ in texts {
        metrics::histogram!("moa_retrieval_embedder_seconds").record(per_text);
    }
    Ok(embeddings)
}

async fn warm_cache(stack: &Stack, cfg: &PerfGateConfig) -> Result<()> {
    let mut queries = hydrate_queries(
        stack.embedder.as_ref(),
        build_repeated_pool(QUERY_SEED, cfg.workspaces, cfg.facts_per_workspace),
    )
    .await?;
    for query in queries.drain(..) {
        let retriever = stack
            .retrievers
            .get(query.workspace_index)
            .context("warm query referenced missing workspace retriever")?;
        let _ = retriever.retrieve(&query).await?;
    }
    Ok(())
}

async fn drive_load(stack: Stack, cfg: &PerfGateConfig) -> Result<LoadReport> {
    let total = cfg.qps as usize * cfg.duration.as_secs() as usize;
    let queries = hydrate_queries(
        stack.embedder.as_ref(),
        build_query_mix(QUERY_SEED, total, cfg.workspaces, cfg.facts_per_workspace),
    )
    .await?;
    let tick_micros = (1_000_000_u64 / u64::from(cfg.qps)).max(1);
    let mut tick = tokio::time::interval(Duration::from_micros(tick_micros));
    let permits = ((cfg.qps as f64) * (cfg.p95_budget_ms as f64 / 1000.0) * 2.0)
        .ceil()
        .max(1.0) as usize;
    let semaphore = Arc::new(Semaphore::new(permits));
    let started = Instant::now();
    let mut joins = Vec::with_capacity(queries.len());

    for query in queries {
        if started.elapsed() >= cfg.duration {
            break;
        }
        tick.tick().await;
        let permit = semaphore.clone().acquire_owned().await?;
        let retriever = stack
            .retrievers
            .get(query.workspace_index)
            .context("load query referenced missing workspace retriever")?
            .clone();
        joins.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let result = retriever.retrieve(&query).await;
            let elapsed = t0.elapsed();
            metrics::histogram!("perf_gate_retrieval_seconds").record(elapsed.as_secs_f64());
            drop(permit);
            QueryOutcome {
                ok: result.is_ok(),
                elapsed,
                is_repeated: query.is_repeated,
            }
        }));
    }

    let outcomes = try_join_all(joins)
        .await
        .context("failed to join load driver tasks")?;
    Ok(LoadReport::from_outcomes(outcomes))
}

async fn hydrate_queries(
    embedder: &dyn Embedder,
    templates: Vec<QueryTemplate>,
) -> Result<Vec<RetrievalQuery>> {
    let mut unique_texts = templates
        .iter()
        .map(|query| query.text.clone())
        .collect::<Vec<_>>();
    unique_texts.sort();
    unique_texts.dedup();
    let embeddings = embed_texts(embedder, &unique_texts).await?;
    let embeddings_by_text = unique_texts
        .into_iter()
        .zip(embeddings)
        .collect::<HashMap<_, _>>();
    templates
        .into_iter()
        .map(|template| {
            let embedding = embeddings_by_text
                .get(&template.text)
                .context("missing query embedding")?
                .clone();
            Ok(RetrievalQuery {
                workspace_index: template.workspace_index,
                text: template.text,
                embedding,
                is_repeated: template.is_repeated,
            })
        })
        .collect()
}

fn build_query_mix(
    seed: u64,
    total: usize,
    workspaces: usize,
    facts_per_workspace: usize,
) -> Vec<QueryTemplate> {
    let mut rng = StdRng::seed_from_u64(seed);
    let repeated_pool = build_repeated_pool(seed, workspaces, facts_per_workspace);
    let mut out = Vec::with_capacity(total);
    for _ in 0..(total * 70 / 100) {
        if let Some(query) = repeated_pool.choose(&mut rng) {
            out.push(query.clone());
        }
    }
    for _ in 0..(total * 20 / 100) {
        if let Some(base) = repeated_pool.choose(&mut rng) {
            out.push(paraphrase(base, &mut rng));
        }
    }
    for _ in 0..(total.saturating_sub(out.len())) {
        out.push(novel_query(&mut rng, workspaces, facts_per_workspace));
    }
    out.shuffle(&mut rng);
    out
}

fn build_repeated_pool(
    seed: u64,
    workspaces: usize,
    facts_per_workspace: usize,
) -> Vec<QueryTemplate> {
    let mut rng = StdRng::seed_from_u64(seed ^ 0x0A11_CE55);
    (0..50)
        .map(|index| {
            let workspace_index = index % workspaces;
            let fact_index = (index * 37 + rng.r#gen::<usize>()) % facts_per_workspace;
            QueryTemplate {
                workspace_index,
                text: canonical_query(workspace_index, fact_index),
                is_repeated: true,
            }
        })
        .collect()
}

fn paraphrase(base: &QueryTemplate, rng: &mut StdRng) -> QueryTemplate {
    let prefix = ["lookup", "find", "recall", "fetch"]
        .choose(rng)
        .copied()
        .unwrap_or("lookup");
    QueryTemplate {
        workspace_index: base.workspace_index,
        text: format!("{prefix} {}", base.text),
        is_repeated: false,
    }
}

fn novel_query(rng: &mut StdRng, workspaces: usize, facts_per_workspace: usize) -> QueryTemplate {
    let workspace_index = rng.gen_range(0..workspaces);
    let fact_index = rng.gen_range(0..facts_per_workspace);
    QueryTemplate {
        workspace_index,
        text: canonical_query(workspace_index, fact_index),
        is_repeated: false,
    }
}

fn canonical_query(workspace_index: usize, fact_index: usize) -> String {
    format!(
        "workspace {workspace_index} fact {fact_index} topic {} retrieval memory",
        fact_index % 17
    )
}

fn fact_text(workspace_index: usize, fact_index: usize) -> String {
    format!(
        "workspace {workspace_index} fact {fact_index} topic {} shard {} owner team{} retrieval memory record",
        fact_index % 17,
        fact_index % 31,
        workspace_index % 5
    )
}

fn spawn_cross_tenant_attacks(
    stack: Stack,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<Result<LeakReport>> {
    tokio::spawn(async move {
        let report = Arc::new(Mutex::new(LeakReport::default()));
        while !stop.load(Ordering::Relaxed) {
            for outcome in run_attack_round(&stack).await {
                let mut guard = report.lock().await;
                guard.attempts += 1;
                if let Err(error) = outcome {
                    guard.count += 1;
                    guard.failures.push(error);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(report.lock().await.clone())
    })
}

async fn run_attack_round(stack: &Stack) -> Vec<Result<(), String>> {
    vec![
        attack_unset_guc(stack).await,
        attack_cte_leak(stack).await,
        attack_vector_oracle(stack).await,
        attack_changelog_leak(stack).await,
        attack_dlq_leak(stack).await,
    ]
}

async fn attack_unset_guc(stack: &Stack) -> Result<(), String> {
    let mut tx = stack.pool.begin().await.map_err(display)?;
    sqlx::query("RESET ALL")
        .execute(&mut *tx)
        .await
        .map_err(display)?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .map_err(display)?;
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index")
        .fetch_one(&mut *tx)
        .await
        .map_err(display)?;
    tx.rollback().await.map_err(display)?;
    (count == 0)
        .then_some(())
        .ok_or_else(|| format!("unset GUC leaked {count} node_index rows"))
}

async fn attack_cte_leak(stack: &Stack) -> Result<(), String> {
    let workspace_a = stack.workspaces[0].workspace_id;
    let workspace_b = stack.workspaces[1].workspace_id;
    let mut conn = app_scoped_conn(&stack.pool, workspace_a)
        .await
        .map_err(display)?;
    let leaked = sqlx::query_scalar::<_, i64>(
        "WITH cte AS (SELECT * FROM moa.node_index) SELECT count(*) FROM cte WHERE workspace_id = $1",
    )
    .bind(workspace_b.to_string())
    .fetch_one(conn.as_mut())
    .await
    .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (leaked == 0)
        .then_some(())
        .ok_or_else(|| format!("CTE leaked {leaked} workspace B rows"))
}

async fn attack_vector_oracle(stack: &Stack) -> Result<(), String> {
    let workspace_a = stack.workspaces[0].workspace_id;
    let workspace_b = stack.workspaces[1].workspace_id;
    let embedding = first_embedding(&stack.pool, workspace_a)
        .await
        .map_err(display)?;
    let vector = PgvectorStore::new_for_app_role(
        stack.pool.clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_b.to_string())),
    );
    let matches = moa_memory_vector::VectorStore::knn(
        &vector,
        &moa_memory_vector::VectorQuery {
            workspace_id: Some(workspace_b.to_string()),
            embedding,
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
        },
    )
    .await
    .map_err(display)?;
    matches
        .is_empty()
        .then_some(())
        .ok_or_else(|| format!("vector oracle leaked matches: {matches:?}"))
}

async fn attack_changelog_leak(stack: &Stack) -> Result<(), String> {
    let a_uid = stack.workspaces[0].first_uid;
    let workspace_b = stack.workspaces[1].workspace_id;
    let mut conn = app_scoped_conn(&stack.pool, workspace_b)
        .await
        .map_err(display)?;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1",
    )
    .bind(a_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (count == 0)
        .then_some(())
        .ok_or_else(|| format!("graph_changelog leaked {count} workspace A rows"))
}

async fn attack_dlq_leak(stack: &Stack) -> Result<(), String> {
    let workspace_a = stack.workspaces[0].workspace_id;
    let workspace_b = stack.workspaces[1].workspace_id;
    let a_dlq = first_dlq(&stack.pool, workspace_a).await.map_err(display)?;
    let mut conn = app_scoped_conn(&stack.pool, workspace_b)
        .await
        .map_err(display)?;
    let leaked =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.ingest_dlq WHERE dlq_id = $1")
            .bind(a_dlq)
            .fetch_one(conn.as_mut())
            .await
            .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (leaked == 0)
        .then_some(())
        .ok_or_else(|| format!("ingest_dlq leaked workspace A row {a_dlq}"))
}

async fn seed_attack_dlq(pool: &PgPool, workspace_id: Uuid) -> Result<()> {
    sqlx::query("INSERT INTO moa.ingest_dlq (workspace_id, payload, error) VALUES ($1, $2, $3)")
        .bind(workspace_id.to_string())
        .bind(json!({ "source": "perf_gate" }))
        .bind("perf_gate_fixture")
        .execute(pool)
        .await
        .context("seed perf gate DLQ fixture")?;
    Ok(())
}

async fn first_embedding(pool: &PgPool, workspace_id: Uuid) -> Result<Vec<f32>, sqlx::Error> {
    let mut conn = app_scoped_conn(pool, workspace_id)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let row = sqlx::query(
        "SELECT embedding::vector::text AS embedding FROM moa.embeddings WHERE workspace_id = $1 LIMIT 1",
    )
    .bind(workspace_id.to_string())
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit()
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    parse_vector_text(&row.try_get::<String, _>("embedding")?)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))
}

async fn first_dlq(pool: &PgPool, workspace_id: Uuid) -> Result<i64, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>(
        "SELECT dlq_id FROM moa.ingest_dlq WHERE workspace_id = $1 ORDER BY dlq_id LIMIT 1",
    )
    .bind(workspace_id.to_string())
    .fetch_one(pool)
    .await?;
    Ok(row)
}

async fn app_scoped_conn<'a>(
    pool: &'a PgPool,
    workspace_id: Uuid,
) -> moa_core::Result<ScopedConn<'a>> {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    let mut conn = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(conn)
}

fn parse_vector_text(value: &str) -> Result<Vec<f32>> {
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    let vector = trimmed
        .split(',')
        .map(|part| part.trim().parse::<f32>().map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    if vector.len() != VECTOR_DIMENSION {
        bail!(
            "expected {VECTOR_DIMENSION} dimensions from pgvector text, got {}",
            vector.len()
        );
    }
    Ok(vector)
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn enforce_gates(cfg: &PerfGateConfig, report: &LoadReport, leaks: &LeakReport) -> Result<()> {
    let mut breaches = Vec::new();
    if report.failed_requests > 0 {
        breaches.push(format!(
            "{} retrieval requests failed",
            report.failed_requests
        ));
    }
    if report.p95_ms > cfg.p95_budget_ms as f64 {
        breaches.push(format!(
            "P95 {:.1} ms > budget {} ms",
            report.p95_ms, cfg.p95_budget_ms
        ));
    }
    if report.cache_hit_rate < cfg.cache_hit_floor {
        breaches.push(format!(
            "cache hit {:.2} < floor {:.2}",
            report.cache_hit_rate, cfg.cache_hit_floor
        ));
    }
    if leaks.count > 0 {
        breaches.push(format!("RLS leaks observed: {}", leaks.count));
    }
    for (leg, p95, ceiling) in report.leg_breaches() {
        breaches.push(format!("leg {leg} P95 {p95:.1} ms > {ceiling:.1} ms"));
    }

    if report.p99_ms > cfg.p99_soft_target_ms as f64 {
        write_stderr(&format!(
            "P99 {:.1} ms exceeds soft target {} ms (warning, not failure)\n",
            report.p99_ms, cfg.p99_soft_target_ms
        ))?;
    }

    if breaches.is_empty() {
        write_stderr("all gates green\n")?;
        Ok(())
    } else {
        for breach in &breaches {
            write_stderr(&format!("{breach}\n"))?;
        }
        std::process::exit(2);
    }
}

fn print_summary_table(report: &LoadReport, leaks: &LeakReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "perf_gate summary");
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "| --- | ---: |");
    let _ = writeln!(out, "| Requests | {} |", report.total_requests);
    let _ = writeln!(out, "| Successful requests | {} |", report.ok_requests);
    let _ = writeln!(out, "| Failed requests | {} |", report.failed_requests);
    let _ = writeln!(out, "| Total P50 | {:.1} ms |", report.p50_ms);
    let _ = writeln!(out, "| Total P95 | {:.1} ms |", report.p95_ms);
    let _ = writeln!(out, "| Total P99 | {:.1} ms |", report.p99_ms);
    let _ = writeln!(out, "| Cache hit rate | {:.3} |", report.cache_hit_rate);
    let _ = writeln!(out, "| RLS attack attempts | {} |", leaks.attempts);
    let _ = writeln!(out, "| RLS leaks | {} |", leaks.count);
    let _ = writeln!(out, "| Cache hit P95 | {:.1} ms |", report.cache_hit_p95_ms);
    let _ = writeln!(out, "| Embedder P95 | {:.1} ms |", report.embedder_p95_ms);
    let _ = writeln!(out, "| Graph leg P95 | {:.1} ms |", report.graph_p95_ms);
    let _ = writeln!(out, "| Vector leg P95 | {:.1} ms |", report.vector_p95_ms);
    let _ = writeln!(out, "| Lexical leg P95 | {:.1} ms |", report.lexical_p95_ms);
    let _ = writeln!(
        out,
        "| RRF + rerank P95 | {:.1} ms |",
        report.rrf_rerank_p95_ms
    );
    if !leaks.failures.is_empty() {
        let _ = writeln!(out, "\nRLS failures:");
        for failure in &leaks.failures {
            let _ = writeln!(out, "- {failure}");
        }
    }
    out
}

fn render_prometheus(handle: &PrometheusHandle, report: &LoadReport, leaks: &LeakReport) -> String {
    let mut snapshot = handle.render();
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p95_ms gauge");
    let _ = writeln!(snapshot, "perf_gate_total_p95_ms {}", report.p95_ms);
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p99_ms gauge");
    let _ = writeln!(snapshot, "perf_gate_total_p99_ms {}", report.p99_ms);
    let _ = writeln!(snapshot, "# TYPE perf_gate_cache_hit_rate gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_cache_hit_rate {}",
        report.cache_hit_rate
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_rls_leaks gauge");
    let _ = writeln!(snapshot, "perf_gate_rls_leaks {}", leaks.count);
    let _ = writeln!(snapshot, "# TYPE perf_gate_requests_total gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_requests_total {}",
        report.total_requests
    );
    snapshot
}

async fn write_snapshot(path: &PathBuf, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create perf snapshot directory {}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(path, body)
        .await
        .with_context(|| format!("failed to write perf snapshot {}", path.display()))
}

fn write_stdout(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stdout()
        .write_all(message.as_bytes())
        .context("failed to write perf summary")
}

fn write_stderr(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stderr()
        .write_all(message.as_bytes())
        .context("failed to write perf gate status")
}

fn sanitize_prom_comment(value: &str) -> String {
    value.replace('\n', " ")
}

fn prom_counter(snapshot: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
    snapshot
        .lines()
        .find_map(|line| {
            if !line.starts_with(metric) || line.contains("_bucket") {
                return None;
            }
            if !labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            {
                return None;
            }
            line.split_whitespace().last()?.parse::<f64>().ok()
        })
        .unwrap_or(0.0)
}

fn prom_histogram_p95_ms(snapshot: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
    prometheus_histogram_percentile(snapshot, metric, labels, 0.95) * 1000.0
}

fn prometheus_histogram_percentile(
    snapshot: &str,
    metric: &str,
    labels: &[(&str, &str)],
    quantile: f64,
) -> f64 {
    let bucket_prefix = format!("{metric}_bucket");
    let mut buckets = snapshot
        .lines()
        .filter_map(|line| {
            if !line.starts_with(&bucket_prefix) {
                return None;
            }
            if !labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            {
                return None;
            }
            let le = label_value(line, "le")?;
            if le == "+Inf" {
                return None;
            }
            let upper = le.parse::<f64>().ok()?;
            let count = line.split_whitespace().last()?.parse::<f64>().ok()?;
            Some((upper, count))
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.0.total_cmp(&right.0));
    let total = buckets.last().map(|(_, count)| *count).unwrap_or(0.0);
    if total <= 0.0 {
        return 0.0;
    }
    let target = total * quantile;
    buckets
        .into_iter()
        .find_map(|(upper, cumulative)| (cumulative >= target).then_some(upper))
        .unwrap_or(0.0)
}

fn label_value<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let start = line.find(&format!("{label}=\""))? + label.len() + 2;
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn percentile_sorted(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Returns the percentile bucket upper bound for non-cumulative histogram buckets.
#[must_use]
pub fn histogram_percentile(buckets: &[f64], counts: &[u64], quantile: f64) -> f64 {
    let total = counts.iter().sum::<u64>();
    if total == 0 || buckets.is_empty() || counts.is_empty() {
        return 0.0;
    }
    let target = (total as f64 * quantile).ceil() as u64;
    let mut cumulative = 0_u64;
    for (bucket, count) in buckets.iter().zip(counts) {
        cumulative += count;
        if cumulative >= target {
            return *bucket;
        }
    }
    *buckets.last().unwrap_or(&0.0)
}

#[cfg(test)]
mod tests {
    use super::histogram_percentile;

    #[test]
    fn histogram_math_percentile_is_monotonic_and_within_bucket() {
        let buckets = vec![5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0, 640.0];
        let counts = vec![10, 20, 30, 25, 10, 3, 1, 1];
        let p50 = histogram_percentile(&buckets, &counts, 0.50);
        let p95 = histogram_percentile(&buckets, &counts, 0.95);
        let p99 = histogram_percentile(&buckets, &counts, 0.99);
        assert!(p50 <= p95 && p95 <= p99);
        assert!((40.0..=80.0).contains(&p95));
    }
}

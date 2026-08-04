//! Retrieval query generation and load-driving logic.

use super::*;

#[derive(Debug, Clone)]
pub(super) struct RetrievalQuery {
    pub(super) tenant_index: usize,
    pub(super) text: String,
    pub(super) embedding: moa_memory_vector::QueryEmbedding,
    pub(super) is_repeated: bool,
}

#[derive(Debug, Clone)]
pub(super) struct QueryTemplate {
    pub(super) tenant_index: usize,
    pub(super) text: String,
    pub(super) is_repeated: bool,
}

#[derive(Debug, Clone)]
pub(super) struct LoadReport {
    pub(super) total_requests: usize,
    pub(super) ok_requests: usize,
    pub(super) failed_requests: usize,
    pub(super) repeated_requests: usize,
    pub(super) p50_ms: f64,
    pub(super) p95_ms: f64,
    pub(super) p99_ms: f64,
    pub(super) cache_hit_rate: f64,
    pub(super) cache_hit_p95_ms: f64,
    pub(super) embedder_p95_ms: f64,
    pub(super) vector_p95_ms: f64,
    pub(super) lexical_p95_ms: f64,
    pub(super) graph_p95_ms: f64,
    pub(super) rrf_rerank_p95_ms: f64,
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

    pub(super) fn with_metrics_delta(
        mut self,
        before: &str,
        after: &str,
        cfg: &PerfGateConfig,
    ) -> Self {
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
        self.embedder_p95_ms = prom_histogram_p95_ms(after, "perf_gate_embedder_seconds", &[]);
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

    pub(super) fn leg_breaches(&self) -> Vec<(&'static str, f64, f64)> {
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
pub(super) struct QueryOutcome {
    ok: bool,
    elapsed: Duration,
    pub(super) is_repeated: bool,
}

pub(super) async fn warm_cache(stack: &Stack, cfg: &PerfGateConfig) -> Result<()> {
    let mut queries = hydrate_queries(
        stack.embedder.as_ref(),
        build_repeated_pool(QUERY_SEED, cfg.tenants, cfg.facts_per_tenant),
    )
    .await?;
    for query in queries.drain(..) {
        let retriever = stack
            .retrievers
            .get(query.tenant_index)
            .context("warm query referenced missing tenant retriever")?;
        let _ = retriever.retrieve(&query).await?;
    }
    Ok(())
}

pub(super) async fn drive_load(stack: Stack, cfg: &PerfGateConfig) -> Result<LoadReport> {
    let total = cfg.qps as usize * cfg.duration.as_secs() as usize;
    let queries = hydrate_queries(
        stack.embedder.as_ref(),
        build_query_mix(QUERY_SEED, total, cfg.tenants, cfg.facts_per_tenant),
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
            .get(query.tenant_index)
            .context("load query referenced missing tenant retriever")?
            .clone();
        joins.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let result = retriever.retrieve(&query).await;
            let elapsed = t0.elapsed();
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

pub(super) async fn hydrate_queries(
    embedder: &dyn EmbeddingProvider,
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
                tenant_index: template.tenant_index,
                text: template.text,
                embedding: moa_memory_vector::QueryEmbedding::new(embedding, embedder.model_id())?,
                is_repeated: template.is_repeated,
            })
        })
        .collect()
}

pub(super) fn build_query_mix(
    seed: u64,
    total: usize,
    tenants: usize,
    facts_per_tenant: usize,
) -> Vec<QueryTemplate> {
    let mut rng = StdRng::seed_from_u64(seed);
    let repeated_pool = build_repeated_pool(seed, tenants, facts_per_tenant);
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
        out.push(novel_query(&mut rng, tenants, facts_per_tenant));
    }
    out.shuffle(&mut rng);
    out
}

pub(super) fn build_repeated_pool(
    seed: u64,
    tenants: usize,
    facts_per_tenant: usize,
) -> Vec<QueryTemplate> {
    let mut rng = StdRng::seed_from_u64(seed ^ 0x0A11_CE55);
    (0..50)
        .map(|index| {
            let tenant_index = index % tenants;
            let fact_index = (index * 37 + rng.r#gen::<usize>()) % facts_per_tenant;
            QueryTemplate {
                tenant_index,
                text: canonical_query(tenant_index, fact_index),
                is_repeated: true,
            }
        })
        .collect()
}

pub(super) fn paraphrase(base: &QueryTemplate, rng: &mut StdRng) -> QueryTemplate {
    let prefix = ["lookup", "find", "recall", "fetch"]
        .choose(rng)
        .copied()
        .unwrap_or("lookup");
    QueryTemplate {
        tenant_index: base.tenant_index,
        text: format!("{prefix} {}", base.text),
        is_repeated: false,
    }
}

pub(super) fn novel_query(
    rng: &mut StdRng,
    tenants: usize,
    facts_per_tenant: usize,
) -> QueryTemplate {
    let tenant_index = rng.gen_range(0..tenants);
    let fact_index = rng.gen_range(0..facts_per_tenant);
    QueryTemplate {
        tenant_index,
        text: canonical_query(tenant_index, fact_index),
        is_repeated: false,
    }
}

pub(super) fn canonical_query(tenant_index: usize, fact_index: usize) -> String {
    format!(
        "tenant {tenant_index} fact {fact_index} topic {} retrieval memory",
        fact_index % 17
    )
}

pub(super) fn percentile_sorted(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

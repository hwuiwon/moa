//! `xtask run-memory-retrieval-eval` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{
    EvalLane, GraphExpansionEvalPolicy, MemoryEvalExtractorMode, MemoryRetrievalEvalOptions,
    QueryRewritePolicy, RankingConfig, run_memory_retrieval_eval,
};

/// Runs the hermetic memory retrieval evaluation command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for memory retrieval eval")?;
    let report = runtime
        .block_on(run_memory_retrieval_eval(
            MemoryRetrievalEvalOptions::new(&options.corpus, &options.output)
                .with_reranker(options.reranker_enabled)
                .with_ranking_config(options.ranking_config.clone())
                .with_rewrite_policy(options.rewrite_policy)
                .with_extractor_mode(options.extractor_mode)
                .with_lane(options.lane)
                .with_consolidation(options.consolidate)
                .with_digests(options.digests)
                .with_inverted_quality_priors(options.invert_quality_priors)
                .with_graph_expansion_policy(options.graph_expansion_policy)
                .with_parity(options.parity)
                .apply_budget_usd(options.budget_usd)
                .apply_extractions_path(options.extractions_path.clone())
                .apply_merges_path(options.merges_path.clone()),
        ))
        .with_context(|| {
            format!(
                "run memory retrieval eval corpus={} output={}",
                options.corpus.display(),
                options.output.display()
            )
        })?;
    let reported_extractor = report
        .providers
        .as_ref()
        .map(|providers| providers.extractor_model.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{:?}", options.extractor_mode));
    println!(
        "wrote memory retrieval eval report: output={} probes={} lane={:?} parity={} rewrite_policy={:?} graph_expansion_policy={} graph_policy={} rewrite_calls={} rewrite_skips={} rewrite_call_rate={:.3} reranker={} extractor={} consolidate={} digests={} merged={} duplicates_remaining={} digests_rebuilt={} est_usd={:.4} aborted_over_budget={} pre_recall_at_4={:.3} pre_recall_at_25={:.3} post_recall_at_4={:.3} ndcg_at_4={:.3} precision_at_4={:.3} pre_precision_at_4={:.3} rendered_context_precision={:.3} abstention_fp_rate={:.3} preference_context_rate={:.3} p95_retrieval_latency_ms={} retrieval_plus_rewrite_p95_latency_ms={}",
        options.output.display(),
        report.probe_results.len(),
        options.lane,
        report.parity,
        options.rewrite_policy,
        options.graph_expansion_policy.as_str(),
        report.graph_retrieval_policy.as_str(),
        report.query_rewrite_call_count,
        report.query_rewrite_skip_count,
        report.query_rewrite_call_rate,
        if report.reranker_enabled { "on" } else { "off" },
        reported_extractor,
        options.consolidate,
        options.digests,
        report
            .consolidation
            .as_ref()
            .map_or(0, |value| value.merged),
        report
            .consolidation
            .as_ref()
            .map_or(0, |value| value.duplicates_remaining),
        report
            .consolidation
            .as_ref()
            .map_or(0, |value| value.digests_rebuilt),
        report.cost.as_ref().map_or(0.0, |cost| cost.est_usd),
        report.aborted_over_budget,
        report.metrics.pre_rerank_recall_at_4.value,
        report.metrics.pre_rerank_recall_at_25.value,
        report.metrics.post_rerank_recall_at_4.value,
        report.metrics.ndcg_at_4.value,
        report.metrics.precision_at_4.value,
        report.metrics.pre_rerank_precision_at_4.value,
        report.metrics.rendered_context_precision.value,
        report.metrics.abstention_false_positive_rate.value,
        report.metrics.preference_context_rate.value,
        report.metrics.p95_retrieval_latency_ms,
        report.retrieval_plus_rewrite_p95_latency_ms
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    corpus: PathBuf,
    output: PathBuf,
    reranker_enabled: bool,
    ranking_config: RankingConfig,
    rewrite_policy: QueryRewritePolicy,
    extractor_mode: MemoryEvalExtractorMode,
    extractions_path: Option<PathBuf>,
    merges_path: Option<PathBuf>,
    lane: EvalLane,
    budget_usd: Option<f64>,
    consolidate: bool,
    digests: bool,
    invert_quality_priors: bool,
    graph_expansion_policy: GraphExpansionEvalPolicy,
    parity: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut corpus = None;
        let mut output = None;
        let mut reranker_enabled = false;
        let mut ranking_config = RankingConfig::default();
        let mut rewrite_policy = QueryRewritePolicy::Gated;
        let mut extractor_mode = MemoryEvalExtractorMode::Heuristic;
        let mut extractions_path = None;
        let mut merges_path = None;
        let mut lane = EvalLane::Pr;
        let mut budget_usd = None;
        let mut consolidate = false;
        let mut digests = false;
        let mut invert_quality_priors = false;
        let mut graph_expansion_policy = GraphExpansionEvalPolicy::Current;
        let mut parity = false;
        let mut extractor_specified = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--corpus" => {
                    let value = args.next().context("--corpus requires a path")?;
                    corpus = Some(PathBuf::from(value));
                }
                "--output" => {
                    let value = args.next().context("--output requires a path")?;
                    output = Some(PathBuf::from(value));
                }
                "--reranker" => {
                    let value = args.next().context("--reranker requires off|on")?;
                    reranker_enabled = parse_reranker(&value)?;
                }
                "--rewrite-policy" => {
                    let value = args
                        .next()
                        .context("--rewrite-policy requires off|always|gated")?;
                    rewrite_policy = parse_rewrite_policy(&value)?;
                }
                "--lane" => {
                    let value = args.next().context("--lane requires pr|live")?;
                    lane = parse_lane(&value)?;
                }
                "--budget-usd" => {
                    let value = args
                        .next()
                        .context("--budget-usd requires a numeric value")?;
                    budget_usd = Some(parse_f64(&value, "--budget-usd")?);
                }
                "--consolidate" => {
                    consolidate = true;
                }
                "--digests" => {
                    digests = true;
                }
                "--invert-quality-priors" => {
                    invert_quality_priors = true;
                }
                "--parity" => {
                    parity = true;
                }
                "--graph-expansion-policy" => {
                    let value = args
                        .next()
                        .context("--graph-expansion-policy requires current|skip-exact-direct|legacy-broad-expansion")?;
                    graph_expansion_policy = parse_graph_expansion_policy(&value)?;
                }
                "--extractor" => {
                    let value = args
                        .next()
                        .context("--extractor requires heuristic|recorded")?;
                    extractor_mode = parse_extractor_mode(&value)?;
                    extractor_specified = true;
                }
                "--extractions" => {
                    let value = args.next().context("--extractions requires a path")?;
                    extractions_path = Some(PathBuf::from(value));
                }
                "--merges" => {
                    let value = args.next().context("--merges requires a path")?;
                    merges_path = Some(PathBuf::from(value));
                }
                "--ranking-subject-match" => {
                    let value = args
                        .next()
                        .context("--ranking-subject-match requires a numeric weight")?;
                    ranking_config.weights.subject_match =
                        parse_f64(&value, "--ranking-subject-match")?;
                }
                "--ranking-recency" => {
                    let value = args
                        .next()
                        .context("--ranking-recency requires a numeric weight")?;
                    ranking_config.weights.recency = parse_f64(&value, "--ranking-recency")?;
                }
                "--ranking-access" => {
                    let value = args
                        .next()
                        .context("--ranking-access requires a numeric weight")?;
                    ranking_config.weights.access = parse_f64(&value, "--ranking-access")?;
                }
                "--ranking-rrf" => {
                    let value = args
                        .next()
                        .context("--ranking-rrf requires a numeric weight")?;
                    ranking_config.weights.rrf = parse_f64(&value, "--ranking-rrf")?;
                }
                "--ranking-overlap" => {
                    let value = args
                        .next()
                        .context("--ranking-overlap requires a numeric weight")?;
                    ranking_config.weights.overlap = parse_f64(&value, "--ranking-overlap")?;
                }
                "--quality-weight" => {
                    let value = args
                        .next()
                        .context("--quality-weight requires a numeric weight")?;
                    ranking_config.weights.quality = parse_f64(&value, "--quality-weight")?;
                }
                "--ranking-scope-user" => {
                    let value = args
                        .next()
                        .context("--ranking-scope-user requires a numeric weight")?;
                    ranking_config.weights.scope_user = parse_f64(&value, "--ranking-scope-user")?;
                }
                "--ranking-recency-half-life-days" => {
                    let value = args
                        .next()
                        .context("--ranking-recency-half-life-days requires a numeric day count")?;
                    ranking_config.weights.recency_half_life_days =
                        parse_f64(&value, "--ranking-recency-half-life-days")?;
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown run-memory-retrieval-eval argument: {other}\n{}",
                    usage()
                ),
            }
        }

        if lane == EvalLane::Pr && budget_usd.is_some() {
            bail!("--budget-usd is only valid with --lane live");
        }
        if lane == EvalLane::Live
            && (extractor_specified || extractions_path.is_some() || merges_path.is_some())
        {
            bail!(
                "--lane live uses live extraction and merge verification; omit --extractor, --extractions, and --merges"
            );
        }

        Ok(Self {
            corpus: corpus.context("--corpus <path> is required")?,
            output: output.context("--output <path> is required")?,
            reranker_enabled,
            ranking_config,
            rewrite_policy,
            extractor_mode,
            extractions_path,
            merges_path,
            lane,
            budget_usd,
            consolidate,
            digests,
            invert_quality_priors,
            graph_expansion_policy,
            parity,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask --features eval-tools -- run-memory-retrieval-eval --corpus <path> --output <path> [--lane pr|live] [--parity] [--budget-usd N] [--extractor heuristic|recorded] [--extractions <path>] [--merges <path>] [--consolidate] [--digests] [--invert-quality-priors] [--graph-expansion-policy current|skip-exact-direct|legacy-broad-expansion] [--rewrite-policy off|always|gated] [--reranker off|on] [--ranking-rrf N] [--ranking-subject-match N] [--ranking-recency N] [--ranking-access N] [--ranking-overlap N] [--quality-weight N] [--ranking-scope-user N] [--ranking-recency-half-life-days N]"
}

fn parse_reranker(value: &str) -> Result<bool> {
    match value {
        "off" => Ok(false),
        "on" => Ok(true),
        other => bail!("unsupported --reranker value `{other}`; expected off|on"),
    }
}

fn parse_lane(value: &str) -> Result<EvalLane> {
    match value {
        "pr" => Ok(EvalLane::Pr),
        "live" => Ok(EvalLane::Live),
        other => bail!("unsupported --lane value `{other}`; expected pr|live"),
    }
}

fn parse_rewrite_policy(value: &str) -> Result<QueryRewritePolicy> {
    match value {
        "off" => Ok(QueryRewritePolicy::Off),
        "always" => Ok(QueryRewritePolicy::Always),
        "gated" => Ok(QueryRewritePolicy::Gated),
        other => bail!("unsupported --rewrite-policy value `{other}`; expected off|always|gated"),
    }
}

fn parse_graph_expansion_policy(value: &str) -> Result<GraphExpansionEvalPolicy> {
    match value {
        "current" => Ok(GraphExpansionEvalPolicy::Current),
        "skip-exact-direct" => Ok(GraphExpansionEvalPolicy::SkipExactDirect),
        other => bail!(
            "unsupported --graph-expansion-policy value `{other}`; expected current|skip-exact-direct|legacy-broad-expansion"
        ),
    }
}

fn parse_extractor_mode(value: &str) -> Result<MemoryEvalExtractorMode> {
    match value {
        "heuristic" => Ok(MemoryEvalExtractorMode::Heuristic),
        "recorded" => Ok(MemoryEvalExtractorMode::Recorded),
        other => bail!("unsupported --extractor value `{other}`; expected heuristic|recorded"),
    }
}

trait MemoryRetrievalEvalOptionsExt {
    fn apply_budget_usd(self, budget_usd: Option<f64>) -> MemoryRetrievalEvalOptions;
    fn apply_extractions_path(self, path: Option<PathBuf>) -> MemoryRetrievalEvalOptions;
    fn apply_merges_path(self, path: Option<PathBuf>) -> MemoryRetrievalEvalOptions;
}

impl MemoryRetrievalEvalOptionsExt for MemoryRetrievalEvalOptions {
    fn apply_budget_usd(self, budget_usd: Option<f64>) -> MemoryRetrievalEvalOptions {
        match budget_usd {
            Some(budget_usd) => self.with_budget_usd(budget_usd),
            None => self,
        }
    }

    fn apply_extractions_path(self, path: Option<PathBuf>) -> MemoryRetrievalEvalOptions {
        match path {
            Some(path) => self.with_extractions_path(path),
            None => self,
        }
    }

    fn apply_merges_path(self, path: Option<PathBuf>) -> MemoryRetrievalEvalOptions {
        match path {
            Some(path) => self.with_merges_path(path),
            None => self,
        }
    }
}

fn parse_f64(value: &str, flag: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("parse {flag} value `{value}`"))
}

//! `xtask run-memory-retrieval-eval` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{
    MemoryRetrievalEvalOptions, RankingConfig, RankingMode, run_memory_retrieval_eval,
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
                .with_ranking_config(options.ranking_config.clone()),
        ))
        .with_context(|| {
            format!(
                "run memory retrieval eval corpus={} output={}",
                options.corpus.display(),
                options.output.display()
            )
        })?;
    println!(
        "wrote memory retrieval eval report: output={} probes={} ranking={:?} reranker={} pre_recall_at_4={:.3} pre_recall_at_25={:.3} post_recall_at_4={:.3} ndcg_at_4={:.3} p95_retrieval_latency_ms={}",
        options.output.display(),
        report.probe_results.len(),
        options.ranking_config.mode,
        if report.reranker_enabled { "on" } else { "off" },
        report.metrics.pre_rerank_recall_at_4.value,
        report.metrics.pre_rerank_recall_at_25.value,
        report.metrics.post_rerank_recall_at_4.value,
        report.metrics.ndcg_at_4.value,
        report.metrics.p95_retrieval_latency_ms
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    corpus: PathBuf,
    output: PathBuf,
    reranker_enabled: bool,
    ranking_config: RankingConfig,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut corpus = None;
        let mut output = None;
        let mut reranker_enabled = false;
        let mut ranking_config = RankingConfig::default();
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
                "--ranking" => {
                    let value = args
                        .next()
                        .context("--ranking requires legacy|feature_v1")?;
                    ranking_config.mode = parse_ranking_mode(&value)?;
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

        Ok(Self {
            corpus: corpus.context("--corpus <path> is required")?,
            output: output.context("--output <path> is required")?,
            reranker_enabled,
            ranking_config,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- run-memory-retrieval-eval --corpus <path> --output <path> [--reranker off|on] [--ranking legacy|feature_v1] [--ranking-rrf N] [--ranking-subject-match N] [--ranking-recency N] [--ranking-access N] [--ranking-overlap N] [--ranking-scope-user N] [--ranking-recency-half-life-days N]"
}

fn parse_reranker(value: &str) -> Result<bool> {
    match value {
        "off" => Ok(false),
        "on" => Ok(true),
        other => bail!("unsupported --reranker value `{other}`; expected off|on"),
    }
}

fn parse_ranking_mode(value: &str) -> Result<RankingMode> {
    match value {
        "legacy" => Ok(RankingMode::Legacy),
        "feature_v1" => Ok(RankingMode::FeatureV1),
        other => bail!("unsupported --ranking value `{other}`; expected legacy|feature_v1"),
    }
}

fn parse_f64(value: &str, flag: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("parse {flag} value `{value}`"))
}

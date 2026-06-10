//! `xtask run-memory-retrieval-eval` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{MemoryRetrievalEvalOptions, run_memory_retrieval_eval};

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
                .with_reranker(options.reranker_enabled),
        ))
        .with_context(|| {
            format!(
                "run memory retrieval eval corpus={} output={}",
                options.corpus.display(),
                options.output.display()
            )
        })?;
    println!(
        "wrote memory retrieval eval report: output={} probes={} reranker={} pre_recall_at_4={:.3} pre_recall_at_25={:.3} post_recall_at_4={:.3} ndcg_at_4={:.3} p95_retrieval_latency_ms={}",
        options.output.display(),
        report.probe_results.len(),
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
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut corpus = None;
        let mut output = None;
        let mut reranker_enabled = false;
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
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- run-memory-retrieval-eval --corpus <path> --output <path> [--reranker off|on]"
}

fn parse_reranker(value: &str) -> Result<bool> {
    match value {
        "off" => Ok(false),
        "on" => Ok(true),
        other => bail!("unsupported --reranker value `{other}`; expected off|on"),
    }
}

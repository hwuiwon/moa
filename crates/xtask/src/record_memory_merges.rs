//! `xtask record-memory-merges` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{MemoryMergeRecordingOptions, record_memory_merges};

/// Records live LLM entity-merge fixtures for a memory eval corpus.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let mut recording = MemoryMergeRecordingOptions::new(&options.corpus);
    if let Some(output) = &options.output {
        recording = recording.with_output_path(output);
    }
    if let Some(extractions) = &options.extractions {
        recording = recording.with_extractions_path(extractions);
    }
    if let Some(api_key_env) = &options.api_key_env {
        recording = recording.with_api_key_env(api_key_env);
    }
    if let Some(model) = &options.model {
        recording = recording.with_model(model);
    }
    if let Some(timeout_ms) = options.timeout_ms {
        recording = recording.with_timeout_ms(timeout_ms);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for merge recording")?;
    let report = runtime
        .block_on(record_memory_merges(recording))
        .with_context(|| format!("record memory merges corpus={}", options.corpus.display()))?;

    println!(
        "recorded {} sessions, {} merge decisions -> {}",
        report.sessions,
        report.decisions,
        report.output_path.display()
    );
    println!(
        "tokens: in={} out={} est_cost_usd={:.2}",
        report.estimated_input_tokens, report.estimated_output_tokens, report.estimated_cost_usd
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    corpus: PathBuf,
    output: Option<PathBuf>,
    extractions: Option<PathBuf>,
    api_key_env: Option<String>,
    model: Option<String>,
    timeout_ms: Option<u64>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut corpus = None;
        let mut output = None;
        let mut extractions = None;
        let mut api_key_env = None;
        let mut model = None;
        let mut timeout_ms = None;
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
                "--extractions" => {
                    let value = args.next().context("--extractions requires a path")?;
                    extractions = Some(PathBuf::from(value));
                }
                "--api-key-env" => {
                    api_key_env = Some(args.next().context("--api-key-env requires a name")?);
                }
                "--model" => {
                    model = Some(args.next().context("--model requires a model id")?);
                }
                "--timeout-ms" => {
                    let value = args.next().context("--timeout-ms requires a number")?;
                    timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown record-memory-merges argument: {other}\n{}",
                    usage()
                ),
            }
        }

        Ok(Self {
            corpus: corpus.context("--corpus <path> is required")?,
            output,
            extractions,
            api_key_env,
            model,
            timeout_ms,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- record-memory-merges --corpus <path> [--output <path>] [--extractions <path>] [--api-key-env COHERE_API_KEY] [--model command-a-plus-05-2026] [--timeout-ms N]"
}

fn parse_u64(value: &str, flag: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("parse {flag} value `{value}`"))
}

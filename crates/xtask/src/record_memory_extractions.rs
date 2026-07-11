//! `xtask record-memory-extractions` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{MemoryExtractionRecordingOptions, record_memory_extractions};

/// Records live model extraction fixtures for a memory eval corpus.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let mut recording = MemoryExtractionRecordingOptions::new(&options.corpus);
    if let Some(output) = &options.output {
        recording = recording.with_output_path(output);
    }
    if let Some(api_key_env) = &options.api_key_env {
        recording = recording.with_api_key_env(api_key_env);
    }
    if let Some(model) = &options.model {
        recording = recording.with_model(model);
    }
    if let Some(max_facts_per_chunk) = options.max_facts_per_chunk {
        recording = recording.with_max_facts_per_chunk(max_facts_per_chunk);
    }
    if let Some(timeout_ms) = options.timeout_ms {
        recording = recording.with_timeout_ms(timeout_ms);
    }
    if let Some(delay_ms) = options.delay_ms {
        recording = recording.with_request_delay_ms(delay_ms);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for extraction recording")?;
    let report = runtime
        .block_on(record_memory_extractions(recording))
        .with_context(|| {
            format!(
                "record memory extractions corpus={}",
                options.corpus.display()
            )
        })?;

    println!(
        "recorded {} sessions, {} chunks, {} facts -> {}",
        report.sessions,
        report.chunks,
        report.facts,
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
    api_key_env: Option<String>,
    model: Option<String>,
    max_facts_per_chunk: Option<usize>,
    timeout_ms: Option<u64>,
    delay_ms: Option<u64>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut corpus = None;
        let mut output = None;
        let mut api_key_env = None;
        let mut model = None;
        let mut max_facts_per_chunk = None;
        let mut timeout_ms = None;
        let mut delay_ms = None;
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
                "--api-key-env" => {
                    api_key_env = Some(args.next().context("--api-key-env requires a name")?);
                }
                "--model" => {
                    model = Some(args.next().context("--model requires a model id")?);
                }
                "--max-facts-per-chunk" => {
                    let value = args
                        .next()
                        .context("--max-facts-per-chunk requires a number")?;
                    max_facts_per_chunk = Some(parse_usize(&value, "--max-facts-per-chunk")?);
                }
                "--timeout-ms" => {
                    let value = args.next().context("--timeout-ms requires a number")?;
                    timeout_ms = Some(parse_u64(&value, "--timeout-ms")?);
                }
                "--delay-ms" => {
                    let value = args.next().context("--delay-ms requires a number")?;
                    delay_ms = Some(parse_u64(&value, "--delay-ms")?);
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown record-memory-extractions argument: {other}\n{}",
                    usage()
                ),
            }
        }

        Ok(Self {
            corpus: corpus.context("--corpus <path> is required")?,
            output,
            api_key_env,
            model,
            max_facts_per_chunk,
            timeout_ms,
            delay_ms,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus <path> [--output <path>] [--api-key-env MOA_OPENAI_API_KEY] [--model gpt-5.4-mini] [--max-facts-per-chunk N] [--timeout-ms N] [--delay-ms N]"
}

fn parse_usize(value: &str, flag: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .with_context(|| format!("parse {flag} value `{value}`"))
}

fn parse_u64(value: &str, flag: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("parse {flag} value `{value}`"))
}

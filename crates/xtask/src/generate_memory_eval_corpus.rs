//! `xtask generate-memory-eval-corpus` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{
    CorpusProfile, build_cached_embedding_fixtures, generate_memory_eval_corpus,
    write_embeddings_jsonl, write_memory_eval_corpus,
};

/// Runs the memory evaluation corpus generator command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let corpus = generate_memory_eval_corpus(options.profile, options.seeds)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for corpus writer")?;
    let embeddings = build_cached_embedding_fixtures(&corpus.embedding_inputs)?;
    runtime
        .block_on(async {
            write_memory_eval_corpus(&options.output, &corpus).await?;
            write_embeddings_jsonl(&options.output.join("embeddings.jsonl"), &embeddings).await
        })
        .with_context(|| format!("write memory eval corpus to {}", options.output.display()))?;
    println!(
        "wrote memory eval corpus: profile={:?} seeds={:?} output={} probes={} embeddings={}",
        corpus.manifest.profile,
        corpus.manifest.seeds,
        options.output.display(),
        corpus.probes.len(),
        embeddings.len()
    );
    Ok(())
}

#[derive(Debug)]
struct Options {
    profile: CorpusProfile,
    seeds: Vec<u64>,
    output: PathBuf,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut profile = None;
        let mut seeds = Vec::new();
        let mut output = None;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--profile" => {
                    let value = args.next().context("--profile requires pr or full")?;
                    profile = Some(parse_profile(&value)?);
                }
                "--seed" => {
                    let value = args.next().context("--seed requires a u64 value")?;
                    let seed = value
                        .parse::<u64>()
                        .with_context(|| format!("parse --seed value {value:?} as u64"))?;
                    seeds.push(seed);
                }
                "--output" => {
                    let value = args.next().context("--output requires a path")?;
                    output = Some(PathBuf::from(value));
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown generate-memory-eval-corpus argument: {other}\n{}",
                    usage()
                ),
            }
        }

        Ok(Self {
            profile: profile.context("--profile pr|full is required")?,
            seeds,
            output: output.context("--output <path> is required")?,
        })
    }
}

fn parse_profile(value: &str) -> Result<CorpusProfile> {
    match value {
        "pr" => Ok(CorpusProfile::Pr),
        "full" => Ok(CorpusProfile::Full),
        other => bail!("unsupported memory eval corpus profile {other:?}; expected pr or full"),
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- generate-memory-eval-corpus --profile pr|full --seed <u64> --seed <u64> --seed <u64> --output <path>"
}

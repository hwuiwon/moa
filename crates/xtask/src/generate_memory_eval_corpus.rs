//! `xtask generate-memory-eval-corpus` command implementation.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::memory_eval::{
    CorpusProfile, HELD_OUT_GOLDEN_SEEDS, TranscriptStyle, build_cached_embedding_fixtures,
    generate_memory_eval_corpus_with_style, write_embeddings_jsonl, write_memory_eval_corpus,
};

/// Runs the memory evaluation corpus generator command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let corpus =
        generate_memory_eval_corpus_with_style(options.profile, options.seeds, options.style)?;
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
        "wrote memory eval corpus: profile={:?} style={:?} seeds={:?} output={} probes={} embeddings={}",
        corpus.manifest.profile,
        corpus.manifest.transcript_style,
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
    style: TranscriptStyle,
    seeds: Vec<u64>,
    output: PathBuf,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut profile = None;
        let mut style = TranscriptStyle::Marked;
        let mut seeds = Vec::new();
        let mut held_out = false;
        let mut output = None;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--profile" => {
                    let value = args.next().context("--profile requires pr or full")?;
                    profile = Some(parse_profile(&value)?);
                }
                "--transcript-style" => {
                    let value = args
                        .next()
                        .context("--transcript-style requires marked or natural")?;
                    style = parse_transcript_style(&value)?;
                }
                "--seed" => {
                    let value = args.next().context("--seed requires a u64 value")?;
                    let seed = value
                        .parse::<u64>()
                        .with_context(|| format!("parse --seed value {value:?} as u64"))?;
                    seeds.push(seed);
                }
                "--held-out" => held_out = true,
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

        let profile = profile.context("--profile pr|full is required")?;
        if held_out && !seeds.is_empty() {
            bail!("--held-out cannot be combined with explicit --seed values");
        }
        if held_out && profile != CorpusProfile::Pr {
            bail!("--held-out requires --profile pr");
        }
        if held_out && style != TranscriptStyle::Marked {
            bail!("--held-out requires --transcript-style marked");
        }
        if held_out {
            seeds = HELD_OUT_GOLDEN_SEEDS.to_vec();
        } else {
            let reserved = seeds
                .iter()
                .copied()
                .filter(|seed| HELD_OUT_GOLDEN_SEEDS.contains(seed))
                .collect::<BTreeSet<_>>();
            if !reserved.is_empty() {
                let reserved = reserved
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("explicit development seeds intersect held-out reservation: {reserved}");
            }
        }

        Ok(Self {
            profile,
            style,
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

fn parse_transcript_style(value: &str) -> Result<TranscriptStyle> {
    match value {
        "marked" => Ok(TranscriptStyle::Marked),
        "natural" => Ok(TranscriptStyle::Natural),
        other => {
            bail!("unsupported memory eval transcript style {other:?}; expected marked or natural")
        }
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- generate-memory-eval-corpus --profile pr|full [--transcript-style marked|natural] (--held-out | --seed <u64> --seed <u64> --seed <u64>) --output <path>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_eval::memory_eval::HELD_OUT_GOLDEN_SEEDS;

    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(args.iter().map(|value| (*value).to_string()))
    }

    #[test]
    fn held_out_selects_the_reserved_marked_pr_split() {
        // Pins: the protected acceptance flag owns its exact corpus identity instead of accepting
        // caller-selected seeds or a realism profile that would require provider recordings.
        let options = parse(&[
            "--profile",
            "pr",
            "--held-out",
            "--output",
            "target/memory-eval/pr-held-out",
        ])
        .expect("the reserved held-out split should parse");

        assert_eq!(options.profile, CorpusProfile::Pr);
        assert_eq!(options.style, TranscriptStyle::Marked);
        assert_eq!(options.seeds, HELD_OUT_GOLDEN_SEEDS);
    }

    #[test]
    fn held_out_rejects_explicit_seeds() {
        // Pins: callers cannot relabel selected development seeds as protected acceptance data.
        let error = parse(&[
            "--profile",
            "pr",
            "--held-out",
            "--seed",
            "1",
            "--output",
            "target/memory-eval/pr-held-out",
        ])
        .expect_err("--held-out plus --seed must be rejected");

        assert_eq!(
            error.to_string(),
            "--held-out cannot be combined with explicit --seed values"
        );
    }

    #[test]
    fn development_seeds_reject_the_held_out_reservation() {
        // Pins: ordinary corpus generation cannot consume even one reserved acceptance seed.
        let error = parse(&[
            "--profile",
            "pr",
            "--seed",
            "1",
            "--seed",
            "101",
            "--seed",
            "103",
            "--output",
            "target/memory-eval/pr",
        ])
        .expect_err("development seeds must remain disjoint from the held-out reservation");

        assert_eq!(
            error.to_string(),
            "explicit development seeds intersect held-out reservation: 101, 103"
        );
    }

    #[test]
    fn held_out_rejects_non_pr_or_non_marked_corpora() {
        // Pins: the protected split remains the hermetic marked PR corpus named by the baseline.
        let full_error = parse(&[
            "--profile",
            "full",
            "--held-out",
            "--output",
            "target/memory-eval/full-held-out",
        ])
        .expect_err("the held-out acceptance split must use the PR profile");
        assert_eq!(full_error.to_string(), "--held-out requires --profile pr");

        let natural_error = parse(&[
            "--profile",
            "pr",
            "--transcript-style",
            "natural",
            "--held-out",
            "--output",
            "target/memory-eval/pr-held-out",
        ])
        .expect_err("the held-out acceptance split must use marked transcripts");
        assert_eq!(
            natural_error.to_string(),
            "--held-out requires --transcript-style marked"
        );
    }
}

//! `xtask compare-eval-reports` command implementation.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use moa_eval::kernel::compare::compare_eval_report_files;

/// Runs paired eval report comparison.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    let comparison = match compare_eval_report_files(&options.baseline, &options.candidate) {
        Ok(comparison) => comparison,
        Err(error) if error.is_pairing_error() => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
        Err(error) => return Err(error).context("compare eval reports"),
    };
    print!("{}", comparison.render_table());
    if let Some(output) = options.output {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(&comparison).context("serialize comparison report")?;
        std::fs::write(&output, json)
            .with_context(|| format!("write comparison report {}", output.display()))?;
    }
    Ok(())
}

#[derive(Debug)]
struct Options {
    baseline: PathBuf,
    candidate: PathBuf,
    output: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut baseline = None;
        let mut candidate = None;
        let mut output = None;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--baseline" => {
                    baseline = Some(PathBuf::from(
                        args.next().context("--baseline requires a path")?,
                    ));
                }
                "--candidate" => {
                    candidate = Some(PathBuf::from(
                        args.next().context("--candidate requires a path")?,
                    ));
                }
                "--output" => {
                    output = Some(PathBuf::from(
                        args.next().context("--output requires a path")?,
                    ));
                }
                "--help" | "-h" => bail!(usage()),
                other => bail!(
                    "unknown compare-eval-reports argument: {other}\n{}",
                    usage()
                ),
            }
        }

        Ok(Self {
            baseline: baseline.context("--baseline <path> is required")?,
            candidate: candidate.context("--candidate <path> is required")?,
            output,
        })
    }
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask -- compare-eval-reports --baseline <a.json> --candidate <b.json> [--output <json>]"
}

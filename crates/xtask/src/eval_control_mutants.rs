//! `xtask eval-control-mutants` command implementation.
//!
//! Runs a targeted `cargo-mutants` slice over the eval scorer and gate code —
//! null-ceiling derivation, the validity audit, the leakage scanner, cohort
//! pairing, and the command adapters that enforce leakage scans — and persists
//! the parsed report.
//!
//! Mutation testing replaces informal "break the grader and see" checks: a
//! surviving mutant is a scorer change no test noticed, named and recorded rather
//! than remembered.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use moa_eval::execution::compare::mutation_report_from_outcomes;

const DEFAULT_OUTPUT_DIR: &str = "target/eval-control-mutants";
const CONFIG_PATH: &str = ".cargo/mutants-eval-controls.toml";
const DEFAULT_MINIMUM_SCORE: f64 = 0.90;

struct Options {
    output_dir: PathBuf,
    minimum_score: f64,
    list_only: bool,
    help: bool,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            output_dir: PathBuf::from(DEFAULT_OUTPUT_DIR),
            minimum_score: DEFAULT_MINIMUM_SCORE,
            list_only: false,
            help: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--out-dir" => {
                    options.output_dir =
                        PathBuf::from(args.next().context("--out-dir requires a path")?);
                }
                "--minimum-score" => {
                    options.minimum_score = args
                        .next()
                        .context("--minimum-score requires a value")?
                        .parse()
                        .context("--minimum-score must be a number")?;
                }
                "--list" => options.list_only = true,
                "--help" | "-h" => options.help = true,
                other => bail!("unknown eval-control-mutants argument: {other}"),
            }
        }
        if !(0.0..=1.0).contains(&options.minimum_score) {
            bail!("--minimum-score must be between 0 and 1");
        }
        Ok(options)
    }
}

fn print_help() {
    println!(
        "xtask eval-control-mutants [--out-dir <dir>] [--minimum-score <0..1>] [--list]\n\n\
         Runs cargo-mutants over the eval control scorer and gate code using\n\
         {CONFIG_PATH}, then writes selected-mutants.txt, outcomes.json, and\n\
         mutation-report.json into the output directory."
    );
}

/// Runs the eval control mutation slice.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    if options.help {
        print_help();
        return Ok(());
    }
    let root = repo_root();
    let config = root.join(CONFIG_PATH);
    if !config.exists() {
        bail!("missing mutants config {}", config.display());
    }
    if Command::new("cargo")
        .args(["mutants", "--version"])
        .current_dir(&root)
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
    {
        bail!("cargo-mutants is required; install it with: cargo install --locked cargo-mutants");
    }
    std::fs::create_dir_all(&options.output_dir)
        .with_context(|| format!("create {}", options.output_dir.display()))?;

    let list = run_mutants(
        &root,
        &config,
        &options.output_dir,
        &["--list", "--no-times"],
    )?;
    let selected = options.output_dir.join("selected-mutants.txt");
    std::fs::write(&selected, &list).with_context(|| format!("write {}", selected.display()))?;
    let selected_count = list.lines().filter(|line| !line.trim().is_empty()).count();
    if selected_count == 0 {
        bail!(
            "cargo-mutants selected no mutants; the control slice in {} matches nothing",
            config.display()
        );
    }
    println!(
        "selected {selected_count} control mutants (persisted to {})",
        selected.display()
    );
    if options.list_only {
        return Ok(());
    }

    let status = Command::new("cargo")
        .args(["mutants", "--workspace", "--config"])
        .arg(&config)
        .args(["--output"])
        .arg(&options.output_dir)
        .current_dir(&root)
        .status()
        .context("run cargo mutants")?;

    let outcomes_path = find_outcomes(&options.output_dir)?;
    let outcomes: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&outcomes_path)
            .with_context(|| format!("read {}", outcomes_path.display()))?,
    )
    .with_context(|| format!("parse {}", outcomes_path.display()))?;
    let report = mutation_report_from_outcomes(&outcomes)
        .map_err(|error| anyhow::anyhow!("parse cargo-mutants outcomes: {error}"))?;

    let report_path = options.output_dir.join("mutation-report.json");
    let mut json = serde_json::to_string_pretty(&report)?;
    json.push('\n');
    std::fs::write(&report_path, json)
        .with_context(|| format!("write {}", report_path.display()))?;

    println!(
        "wrote control mutation report: output={} caught={} missed={} timeouts={} viable={} score={:.3} mutants_exit={}",
        report_path.display(),
        report.caught,
        report.missed,
        report.timeouts,
        report.viable,
        report.mutation_score,
        status.code().unwrap_or(-1)
    );
    if report.mutation_score < options.minimum_score {
        bail!(
            "control mutation score {:.3} is below the {:.3} minimum; missed: {}",
            report.mutation_score,
            options.minimum_score,
            report.missed_mutants.join(", ")
        );
    }
    Ok(())
}

fn run_mutants(root: &Path, config: &Path, output_dir: &Path, extra: &[&str]) -> Result<String> {
    let output = Command::new("cargo")
        .args(["mutants", "--workspace", "--config"])
        .arg(config)
        .args(["--output"])
        .arg(output_dir)
        .args(extra)
        .current_dir(root)
        .output()
        .context("run cargo mutants")?;
    if !output.status.success() {
        bail!(
            "cargo mutants {extra:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn find_outcomes(output_dir: &Path) -> Result<PathBuf> {
    for candidate in [
        output_dir.join("mutants.out/outcomes.json"),
        output_dir.join("outcomes.json"),
    ] {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!(
        "cargo-mutants produced no outcomes.json under {}",
        output_dir.display()
    )
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

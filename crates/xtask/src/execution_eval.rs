//! `xtask execution-eval` offline report, gate, comparison, and mutation commands.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use moa_eval::execution::{
    ExecutionEvalCaseResultV1, ExecutionEvalComparisonConfigV1, ExecutionEvalLaneV1,
    ExecutionEvalReportV1, ExecutionJudgeCalibrationStatusV1, compare_execution_eval_reports,
    load_execution_corpus, mutation_report_from_outcomes, score_contract_case, score_routing_cases,
};

const DEFAULT_MANIFEST: &str = "crates/moa-eval/scenarios/execution/manifest.toml";

/// Runs one execution-eval subcommand.
pub(crate) fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    match args.next().as_deref() {
        Some("run-offline") => run_offline(OfflineOptions::parse(args)?),
        Some("check") => check(CheckOptions::parse(args)?),
        Some("compare") => compare(CompareOptions::parse(args)?),
        Some("mutation-report") => mutation_report(MutationOptions::parse(args)?),
        Some("--help" | "-h") => bail!(usage()),
        Some(command) => bail!("unknown execution-eval subcommand `{command}`\n{}", usage()),
        None => bail!(usage()),
    }
}

fn run_offline(options: OfflineOptions) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for execution offline eval")?;
    let corpus = runtime
        .block_on(load_execution_corpus(&options.manifest))
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "load execution eval manifest {}",
                options.manifest.display()
            )
        })?;
    let routing = runtime
        .block_on(score_routing_cases(&corpus.routing_cases))
        .map_err(anyhow::Error::from)
        .context("score execution routing corpus")?;
    let mut cases = routing
        .cases
        .iter()
        .map(|result| ExecutionEvalCaseResultV1 {
            case_id: format!("routing:{}", result.case_id),
            passed: result.passed,
            contract_omission: None,
            contract_score: None,
            impossible_case: false,
            execution_false_completion: false,
            observed_run_status: None,
            observed_route: None,
            route_provenance: None,
            invariants: Vec::new(),
            cost_microusd: 0,
            latency_ms: 0,
            task_count: 0,
            terminal_output_hash: None,
            final_response_hash: None,
        })
        .collect::<Vec<_>>();
    for contract_case in &corpus.contract_cases {
        let score = score_contract_case(contract_case).map_err(anyhow::Error::from)?;
        cases.push(ExecutionEvalCaseResultV1 {
            case_id: format!("contract:{}", contract_case.case_id),
            passed: !score.contract_omission,
            contract_omission: Some(score.contract_omission),
            contract_score: Some(score.macro_f1),
            impossible_case: false,
            execution_false_completion: false,
            observed_run_status: None,
            observed_route: None,
            route_provenance: None,
            invariants: Vec::new(),
            cost_microusd: 0,
            latency_ms: 0,
            task_count: u64::try_from(contract_case.candidate.plan.nodes.len())
                .context("contract candidate node count exceeds u64")?,
            terminal_output_hash: None,
            final_response_hash: None,
        });
    }
    let mut report = ExecutionEvalReportV1::new(
        ExecutionEvalLaneV1::OfflinePr,
        manifest_hashes(&corpus.manifest),
        vec![0],
        1,
        ExecutionJudgeCalibrationStatusV1::Unavailable,
        None,
        cases,
    )
    .map_err(anyhow::Error::from)?;
    routing.apply_to_report_metrics(&mut report.metrics);
    report.validate().map_err(anyhow::Error::from)?;
    write_json(&options.output, &report)?;
    println!(
        "wrote execution offline eval: output={} cases={} passed={} respond_on_run_rate={:.6}",
        options.output.display(),
        report.metrics.total_cases,
        report.metrics.passed_cases,
        report.metrics.respond_on_run_rate.unwrap_or_default()
    );
    Ok(())
}

fn check(options: CheckOptions) -> Result<()> {
    let report: ExecutionEvalReportV1 = read_json(&options.report)?;
    report.validate().map_err(anyhow::Error::from)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for execution report check")?;
    let corpus = runtime
        .block_on(load_execution_corpus(&options.manifest))
        .map_err(anyhow::Error::from)?;
    let expected_hashes = manifest_hashes(&corpus.manifest);
    if report.corpus_hashes != expected_hashes {
        bail!("execution report corpus hashes do not match the checked manifest");
    }
    let failed_cases = report
        .cases
        .iter()
        .filter(|case| !case.passed)
        .map(|case| case.case_id.as_str())
        .collect::<Vec<_>>();
    if !failed_cases.is_empty() {
        bail!("execution report contains failed cases: {failed_cases:?}");
    }
    if report.metrics.execution_false_completions != 0
        || report
            .metrics
            .execution_false_completion_rate
            .unwrap_or_default()
            > options.max_execution_false_completion_rate
    {
        bail!("execution false-completion gate failed");
    }
    if report.metrics.respond_on_run_rate.unwrap_or_default() > options.max_respond_on_run_rate {
        bail!("Respond-on-Run routing gate failed");
    }
    if report.metrics.contract_omission_rate.unwrap_or_default()
        > options.max_contract_omission_rate
    {
        bail!("execution contract-omission gate failed");
    }
    if report.metrics.weighted_routing_cost.unwrap_or_default() > options.max_weighted_routing_cost
    {
        bail!("weighted routing-cost gate failed");
    }
    if report.metrics.near_boundary_act_recall.unwrap_or_default()
        < options.min_near_boundary_act_recall
    {
        bail!("near-boundary Act recall gate failed");
    }
    let pass_rate = if report.metrics.total_cases == 0 {
        0.0
    } else {
        report.metrics.passed_cases as f64 / report.metrics.total_cases as f64
    };
    if pass_rate < options.min_pass_rate {
        bail!("execution case pass-rate gate failed");
    }
    println!(
        "execution eval check passed: report={} cases={} false_completions=0 respond_on_run_rate={:.6}",
        options.report.display(),
        report.metrics.total_cases,
        report.metrics.respond_on_run_rate.unwrap_or_default()
    );
    Ok(())
}

fn compare(options: CompareOptions) -> Result<()> {
    let baseline: ExecutionEvalReportV1 = read_json(&options.baseline)?;
    let candidate: ExecutionEvalReportV1 = read_json(&options.candidate)?;
    let comparison = compare_execution_eval_reports(
        &baseline,
        &candidate,
        ExecutionEvalComparisonConfigV1 {
            practical_pass_rate_regression: options.practical_pass_rate_regression,
            ..ExecutionEvalComparisonConfigV1::default()
        },
    )
    .map_err(anyhow::Error::from)?;
    if let Some(output) = options.output.as_deref() {
        write_json(output, &comparison)?;
    }
    println!(
        "execution eval comparison: paired={} pass_delta={:+.6} significant_regression={} gate_failed={}",
        comparison.paired_cases,
        comparison.pass_rate_delta.mean,
        comparison.significant_pass_regression,
        comparison.gate_failed
    );
    if comparison.gate_failed {
        bail!("significant practical live execution-eval regression");
    }
    Ok(())
}

fn mutation_report(options: MutationOptions) -> Result<()> {
    let outcomes: serde_json::Value = read_json(&options.outcomes)?;
    let report = mutation_report_from_outcomes(&outcomes).map_err(anyhow::Error::from)?;
    write_json(&options.output, &report)?;
    if report.mutation_score < options.min_score {
        bail!(
            "execution mutation score {:.4} is below {:.4}; missed={:?}; timeouts={:?}",
            report.mutation_score,
            options.min_score,
            report.missed_mutants,
            report.timeout_mutants
        );
    }
    println!(
        "execution mutation report: output={} caught={} viable={} score={:.4}",
        options.output.display(),
        report.caught,
        report.viable,
        report.mutation_score
    );
    Ok(())
}

fn manifest_hashes(
    manifest: &moa_eval::execution::ExecutionCorpusManifestV1,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("contract".to_string(), manifest.contract.sha256.clone()),
        ("routing".to_string(), manifest.routing.sha256.clone()),
        (
            "task_quality".to_string(),
            manifest.task_quality.sha256.clone(),
        ),
    ])
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(value).context("serialize execution eval JSON")?;
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[derive(Debug)]
struct OfflineOptions {
    manifest: PathBuf,
    output: PathBuf,
}

impl OfflineOptions {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut manifest = None;
        let mut output = None;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--manifest" => manifest = Some(next_path(&mut args, "--manifest")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                other => bail!("unknown run-offline argument `{other}`"),
            }
        }
        Ok(Self {
            manifest: manifest.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST)),
            output: output.context("run-offline requires --output <path>")?,
        })
    }
}

#[derive(Debug)]
struct CheckOptions {
    report: PathBuf,
    manifest: PathBuf,
    max_execution_false_completion_rate: f64,
    max_respond_on_run_rate: f64,
    max_contract_omission_rate: f64,
    max_weighted_routing_cost: f64,
    min_near_boundary_act_recall: f64,
    min_pass_rate: f64,
}

impl CheckOptions {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut report = None;
        let mut manifest = None;
        let mut max_execution_false_completion_rate = 0.0;
        let mut max_respond_on_run_rate = 0.0;
        let mut max_contract_omission_rate = 0.0;
        let mut max_weighted_routing_cost = f64::INFINITY;
        let mut min_near_boundary_act_recall = 0.0;
        let mut min_pass_rate = 1.0;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--report" => report = Some(next_path(&mut args, "--report")?),
                "--manifest" => manifest = Some(next_path(&mut args, "--manifest")?),
                "--max-execution-false-completion-rate" => {
                    max_execution_false_completion_rate = next_rate(&mut args, &arg)?;
                }
                "--max-respond-on-run-rate" => {
                    max_respond_on_run_rate = next_rate(&mut args, &arg)?;
                }
                "--max-contract-omission-rate" => {
                    max_contract_omission_rate = next_rate(&mut args, &arg)?;
                }
                "--max-weighted-routing-cost" => {
                    max_weighted_routing_cost = next_nonnegative(&mut args, &arg)?;
                }
                "--min-near-boundary-act-recall" => {
                    min_near_boundary_act_recall = next_rate(&mut args, &arg)?;
                }
                "--min-pass-rate" => min_pass_rate = next_rate(&mut args, &arg)?,
                other => bail!("unknown check argument `{other}`"),
            }
        }
        Ok(Self {
            report: report.context("check requires --report <path>")?,
            manifest: manifest.unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST)),
            max_execution_false_completion_rate,
            max_respond_on_run_rate,
            max_contract_omission_rate,
            max_weighted_routing_cost,
            min_near_boundary_act_recall,
            min_pass_rate,
        })
    }
}

#[derive(Debug)]
struct CompareOptions {
    baseline: PathBuf,
    candidate: PathBuf,
    output: Option<PathBuf>,
    practical_pass_rate_regression: f64,
}

impl CompareOptions {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut baseline = None;
        let mut candidate = None;
        let mut output = None;
        let mut practical_pass_rate_regression = 0.02;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--baseline" => baseline = Some(next_path(&mut args, "--baseline")?),
                "--candidate" => candidate = Some(next_path(&mut args, "--candidate")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--practical-pass-rate-regression" => {
                    practical_pass_rate_regression = next_rate(&mut args, &arg)?;
                }
                other => bail!("unknown compare argument `{other}`"),
            }
        }
        Ok(Self {
            baseline: baseline.context("compare requires --baseline <path>")?,
            candidate: candidate.context("compare requires --candidate <path>")?,
            output,
            practical_pass_rate_regression,
        })
    }
}

#[derive(Debug)]
struct MutationOptions {
    outcomes: PathBuf,
    output: PathBuf,
    min_score: f64,
}

impl MutationOptions {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut outcomes = None;
        let mut output = None;
        let mut min_score = 0.90;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--outcomes" => outcomes = Some(next_path(&mut args, "--outcomes")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--min-score" => min_score = next_rate(&mut args, &arg)?,
                other => bail!("unknown mutation-report argument `{other}`"),
            }
        }
        Ok(Self {
            outcomes: outcomes.context("mutation-report requires --outcomes <path>")?,
            output: output.context("mutation-report requires --output <path>")?,
            min_score,
        })
    }
}

fn next_path(args: &mut impl Iterator<Item = String>, option: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        args.next()
            .with_context(|| format!("{option} requires a path"))?,
    ))
}

fn next_rate(args: &mut impl Iterator<Item = String>, option: &str) -> Result<f64> {
    let value = next_nonnegative(args, option)?;
    if value > 1.0 {
        bail!("{option} must be within [0, 1]");
    }
    Ok(value)
}

fn next_nonnegative(args: &mut impl Iterator<Item = String>, option: &str) -> Result<f64> {
    let raw = args
        .next()
        .with_context(|| format!("{option} requires a number"))?;
    let value = raw
        .parse::<f64>()
        .with_context(|| format!("parse {option} value `{raw}`"))?;
    if !value.is_finite() || value < 0.0 {
        bail!("{option} must be finite and nonnegative");
    }
    Ok(value)
}

fn usage() -> &'static str {
    "usage: cargo run -p xtask --features eval-tools -- execution-eval <run-offline|check|compare|mutation-report> ..."
}

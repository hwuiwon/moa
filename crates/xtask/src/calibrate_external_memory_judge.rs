//! Prepare and score the manual LongMemEval absolute-judge calibration contract.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use moa_eval::external_memory::answer::ExternalMemoryMode;
use moa_eval::external_memory::calibration::{
    CalibrationManifestV1, CalibrationSourceCase, CalibrationStratum, prepare_calibration,
    score_calibration,
};
use moa_eval::external_memory::dataset::DatasetPackageV1;
use moa_eval::external_memory::longmemeval::{
    LONGMEMEVAL_DATASET, LONGMEMEVAL_FILE, LongMemEvalDataset, LongMemEvalQuestionType,
    load_full_longmemeval_package,
};
use moa_eval::external_memory::report::{ExternalMemoryReportV2, FailureKind};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CalibrationCommand {
    Prepare {
        dataset: PathBuf,
        report: PathBuf,
        output_manifest: PathBuf,
        labeler_a_template: PathBuf,
        labeler_b_template: PathBuf,
    },
    Score {
        manifest: PathBuf,
        report: PathBuf,
        labeler_a: PathBuf,
        labeler_b: PathBuf,
        adjudication: PathBuf,
        output: PathBuf,
    },
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    match parse_args(args)? {
        CalibrationCommand::Prepare {
            dataset,
            report,
            output_manifest,
            labeler_a_template,
            labeler_b_template,
        } => prepare(
            &dataset,
            &report,
            &output_manifest,
            &labeler_a_template,
            &labeler_b_template,
        ),
        CalibrationCommand::Score {
            manifest,
            report,
            labeler_a,
            labeler_b,
            adjudication,
            output,
        } => score(
            &manifest,
            &report,
            &labeler_a,
            &labeler_b,
            &adjudication,
            &output,
        ),
    }
}

fn prepare(
    dataset_root: &Path,
    report_path: &Path,
    output_manifest: &Path,
    labeler_a_template: &Path,
    labeler_b_template: &Path,
) -> Result<()> {
    let package_path = dataset_root.join("package.json");
    let package_bytes = read(&package_path)?;
    let package: DatasetPackageV1 =
        serde_json::from_slice(&package_bytes).context("parse strict dataset package")?;
    package.validate().map_err(anyhow::Error::from)?;
    if package.manifest.dataset != LONGMEMEVAL_DATASET
        || package.manifest.files.len() != 1
        || package.manifest.files[0].path != LONGMEMEVAL_FILE
    {
        bail!("calibration prepare requires the pinned LongMemEval-S cleaned package");
    }
    let dataset = load_full_longmemeval_package(&package, dataset_root)
        .map_err(anyhow::Error::from)
        .context("load verified LongMemEval package")?;
    let report_bytes = read(report_path)?;
    let report = parse_report(&report_bytes)?;
    if report.dataset_package != package {
        bail!("calibration report dataset package does not match the input package");
    }
    let cases = source_cases_from_report(&dataset, &report)?;
    let prepared = prepare_calibration(
        &package.manifest.source.revision,
        &cases,
        &package_bytes,
        &report_bytes,
    )
    .map_err(anyhow::Error::from)?;

    write_json(output_manifest, &prepared.manifest)?;
    write_json(labeler_a_template, &prepared.labeler_a)?;
    write_json(labeler_b_template, &prepared.labeler_b)?;
    Ok(())
}

fn score(
    manifest_path: &Path,
    report_path: &Path,
    labeler_a_path: &Path,
    labeler_b_path: &Path,
    adjudication_path: &Path,
    output: &Path,
) -> Result<()> {
    let manifest_bytes = read(manifest_path)?;
    let manifest: CalibrationManifestV1 =
        serde_json::from_slice(&manifest_bytes).context("parse strict calibration manifest")?;
    manifest.validate().map_err(anyhow::Error::from)?;
    let report_bytes = read(report_path)?;
    let report = parse_report(&report_bytes)?;
    if report.dataset_package.manifest.dataset != LONGMEMEVAL_DATASET
        || report.dataset_package.manifest.source.revision != manifest.dataset_revision
    {
        bail!("calibration report dataset/revision does not match the manifest");
    }
    let judge_outcomes = primary_judge_outcomes(&report, &manifest)?;
    let labeler_a_bytes = read(labeler_a_path)?;
    let labeler_b_bytes = read(labeler_b_path)?;
    let adjudication_bytes = read(adjudication_path)?;
    let results = score_calibration(
        &manifest_bytes,
        &report_bytes,
        &labeler_a_bytes,
        &labeler_b_bytes,
        &adjudication_bytes,
        &judge_outcomes,
    )
    .map_err(anyhow::Error::from)?;
    write_json(output, &results)
}

fn parse_report(bytes: &[u8]) -> Result<ExternalMemoryReportV2> {
    let report: ExternalMemoryReportV2 =
        serde_json::from_slice(bytes).context("parse strict ExternalMemoryReportV2")?;
    report
        .canonical_json()
        .map_err(anyhow::Error::from)
        .context("validate ExternalMemoryReportV2")?;
    Ok(report)
}

fn source_cases_from_report(
    dataset: &LongMemEvalDataset,
    report: &ExternalMemoryReportV2,
) -> Result<Vec<CalibrationSourceCase>> {
    let primary = unique_primary_cases(report)?;
    dataset
        .cases
        .iter()
        .map(|case| {
            let case_report = primary
                .get(case.prepared.case.isolation_key.as_str())
                .with_context(|| {
                    format!(
                        "primary report is missing LongMemEval case {}",
                        case.metadata.question_id
                    )
                })?;
            let candidate_answer = case_report
                .reader
                .as_ref()
                .map(|reader| reader.answer.clone());
            let reader_failure_kind = if candidate_answer.is_none() {
                Some(
                    case_report
                        .failure
                        .as_ref()
                        .map_or("missing_reader", |failure| failure_kind(failure.kind))
                        .to_string(),
                )
            } else {
                None
            };
            let judge_outcome = if case_report.failure.is_none() {
                case_report
                    .absolute_judge
                    .as_ref()
                    .map(|judge| judge.supported)
            } else {
                None
            };
            Ok(CalibrationSourceCase {
                question_id: case.metadata.question_id.clone(),
                stratum: if case.is_abstention {
                    CalibrationStratum::Abstention
                } else {
                    question_stratum(case.metadata.question_type)
                },
                question: case.prepared.case.question.clone(),
                reference_answer: case.prepared.case.answer.clone(),
                candidate_answer,
                reader_failure_kind,
                judge_outcome,
            })
        })
        .collect()
}

fn primary_judge_outcomes(
    report: &ExternalMemoryReportV2,
    manifest: &CalibrationManifestV1,
) -> Result<BTreeMap<String, Option<bool>>> {
    let primary = unique_primary_cases(report)?;
    Ok(manifest
        .sample
        .iter()
        .map(|sample| {
            let isolation_key = format!(
                "{LONGMEMEVAL_DATASET}/{}/{}",
                manifest.dataset_revision, sample.question_id
            );
            let outcome = primary.get(isolation_key.as_str()).and_then(|case| {
                if case.failure.is_none() {
                    case.absolute_judge.as_ref().map(|judge| judge.supported)
                } else {
                    None
                }
            });
            (sample.question_id.clone(), outcome)
        })
        .collect())
}

fn unique_primary_cases(
    report: &ExternalMemoryReportV2,
) -> Result<BTreeMap<&str, &moa_eval::external_memory::report::CaseReportV2>> {
    let primary_modes = report
        .modes
        .iter()
        .filter(|mode| mode.mode == ExternalMemoryMode::Primary)
        .collect::<Vec<_>>();
    if primary_modes.len() != 1 {
        bail!("calibration report must contain exactly one primary mode");
    }
    let mut cases = BTreeMap::new();
    for case in &primary_modes[0].cases {
        if cases.insert(case.isolation_key.as_str(), case).is_some() {
            bail!(
                "calibration report contains duplicate primary isolation key {}",
                case.isolation_key
            );
        }
    }
    Ok(cases)
}

const fn question_stratum(question_type: LongMemEvalQuestionType) -> CalibrationStratum {
    match question_type {
        LongMemEvalQuestionType::KnowledgeUpdate => CalibrationStratum::KnowledgeUpdate,
        LongMemEvalQuestionType::MultiSession => CalibrationStratum::MultiSession,
        LongMemEvalQuestionType::SingleSessionAssistant => {
            CalibrationStratum::SingleSessionAssistant
        }
        LongMemEvalQuestionType::SingleSessionPreference => {
            CalibrationStratum::SingleSessionPreference
        }
        LongMemEvalQuestionType::SingleSessionUser => CalibrationStratum::SingleSessionUser,
        LongMemEvalQuestionType::TemporalReasoning => CalibrationStratum::TemporalReasoning,
    }
}

const fn failure_kind(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Timeout => "timeout",
        FailureKind::Budget => "budget",
        FailureKind::Provider => "provider",
        FailureKind::Parse => "parse",
        FailureKind::Backend => "backend",
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<CalibrationCommand> {
    let subcommand = args
        .next()
        .context("calibrate-external-memory-judge requires prepare or score")?;
    let mut values = BTreeMap::<String, String>::new();
    while let Some(flag) = args.next() {
        if !flag.starts_with("--") {
            bail!("unexpected calibration argument: {flag}");
        }
        let value = args
            .next()
            .with_context(|| format!("{flag} requires a value"))?;
        if values.insert(flag.clone(), value).is_some() {
            bail!("duplicate calibration argument: {flag}");
        }
    }
    let allowed = match subcommand.as_str() {
        "prepare" => [
            "--dataset",
            "--report",
            "--output-manifest",
            "--labeler-a-template",
            "--labeler-b-template",
        ]
        .as_slice(),
        "score" => [
            "--manifest",
            "--report",
            "--labeler-a",
            "--labeler-b",
            "--adjudication",
            "--output",
        ]
        .as_slice(),
        _ => bail!("unknown calibration subcommand: {subcommand}"),
    };
    let allowed = allowed.iter().copied().collect::<HashSet<_>>();
    if let Some(unknown) = values.keys().find(|flag| !allowed.contains(flag.as_str())) {
        bail!("unknown calibration argument: {unknown}");
    }
    let required = |flag: &str| -> Result<PathBuf> {
        values
            .get(flag)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .with_context(|| format!("missing required calibration argument {flag}"))
    };
    match subcommand.as_str() {
        "prepare" => Ok(CalibrationCommand::Prepare {
            dataset: required("--dataset")?,
            report: required("--report")?,
            output_manifest: required("--output-manifest")?,
            labeler_a_template: required("--labeler-a-template")?,
            labeler_b_template: required("--labeler-b-template")?,
        }),
        "score" => Ok(CalibrationCommand::Score {
            manifest: required("--manifest")?,
            report: required("--report")?,
            labeler_a: required("--labeler-a")?,
            labeler_b: required("--labeler-b")?,
            adjudication: required("--adjudication")?,
            output: required("--output")?,
        }),
        _ => unreachable!("subcommand was validated above"),
    }
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize calibration JSON")?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrate_external_memory_judge_prepare_requires_every_path() {
        // Pins: prepare cannot start package/report reads without all three explicit outputs.
        let error = parse_args(
            [
                "prepare",
                "--dataset",
                "dataset",
                "--report",
                "report.json",
                "--output-manifest",
                "manifest.json",
                "--labeler-a-template",
                "a.json",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect_err("missing labeler-B template must fail");
        assert!(error.to_string().contains("--labeler-b-template"));
    }

    #[test]
    fn calibrate_external_memory_judge_score_parses_explicit_report_and_inputs() {
        // Pins: score always receives the exact report plus both labels and adjudication.
        let parsed = parse_args(
            [
                "score",
                "--manifest",
                "manifest.json",
                "--report",
                "report.json",
                "--labeler-a",
                "a.json",
                "--labeler-b",
                "b.json",
                "--adjudication",
                "gold.json",
                "--output",
                "results.json",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("parse complete score command");
        assert_eq!(
            parsed,
            CalibrationCommand::Score {
                manifest: PathBuf::from("manifest.json"),
                report: PathBuf::from("report.json"),
                labeler_a: PathBuf::from("a.json"),
                labeler_b: PathBuf::from("b.json"),
                adjudication: PathBuf::from("gold.json"),
                output: PathBuf::from("results.json"),
            }
        );
    }

    #[test]
    fn calibrate_external_memory_judge_rejects_unknown_or_duplicate_flags() {
        // Pins: typoed and duplicate inputs cannot be silently ignored or overwritten.
        for args in [
            vec!["prepare", "--unknown", "value"],
            vec!["score", "--report", "first.json", "--report", "second.json"],
        ] {
            assert!(
                parse_args(args.into_iter().map(str::to_string)).is_err(),
                "unknown or duplicate calibration flags must fail"
            );
        }
    }
}

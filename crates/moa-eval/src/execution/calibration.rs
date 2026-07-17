//! Human-label and semantic-judge calibration for execution-eval prose metrics.

use std::collections::BTreeSet;

use moa_eval_core::{EvalError, Result};
use serde::{Deserialize, Serialize};

use super::report::ExecutionJudgeCalibrationStatusV1;

/// Required number of adjudicated calibration items.
pub const EXECUTION_CALIBRATION_ITEM_COUNT: usize = 100;

/// One two-labeler, adjudicated, and judge-scored calibration row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCalibrationItemV1 {
    /// Stable calibration case identifier.
    pub case_id: String,
    /// First independent human label.
    pub labeler_a: bool,
    /// Second independent human label.
    pub labeler_b: bool,
    /// Final adjudicated human label.
    pub adjudicated: bool,
    /// Semantic judge label evaluated against adjudication.
    pub judge: bool,
}

/// Strict checked calibration artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCalibrationArtifactV1 {
    /// Artifact schema version, fixed at `1`.
    pub schema_version: u8,
    /// Exact 100-item calibration set.
    pub items: Vec<ExecutionCalibrationItemV1>,
}

/// Deterministic calibration metrics and threshold verdict.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCalibrationReportV1 {
    /// Exact item count.
    pub item_count: u64,
    /// Raw agreement between the two human labelers.
    pub labeler_agreement: f64,
    /// Cohen's kappa between the two human labelers.
    pub cohens_kappa: f64,
    /// Judge accuracy against adjudicated human labels.
    pub judge_accuracy: f64,
    /// Closed calibration status used by execution reports.
    pub status: ExecutionJudgeCalibrationStatusV1,
}

/// Validates and scores one complete execution-judge calibration artifact.
pub fn score_execution_calibration(
    artifact: &ExecutionCalibrationArtifactV1,
) -> Result<ExecutionCalibrationReportV1> {
    if artifact.schema_version != 1 || artifact.items.len() != EXECUTION_CALIBRATION_ITEM_COUNT {
        return Err(invalid_config(format!(
            "execution calibration requires schema version 1 and exactly {EXECUTION_CALIBRATION_ITEM_COUNT} items"
        )));
    }
    let mut ids = BTreeSet::new();
    for item in &artifact.items {
        if item.case_id.trim().is_empty() || !ids.insert(item.case_id.as_str()) {
            return Err(invalid_config(
                "execution calibration case IDs must be non-empty and unique".to_string(),
            ));
        }
    }
    let count = artifact.items.len() as f64;
    let agreement = artifact
        .items
        .iter()
        .filter(|item| item.labeler_a == item.labeler_b)
        .count() as f64
        / count;
    let a_positive = artifact.items.iter().filter(|item| item.labeler_a).count() as f64 / count;
    let b_positive = artifact.items.iter().filter(|item| item.labeler_b).count() as f64 / count;
    let expected_agreement = a_positive * b_positive + (1.0 - a_positive) * (1.0 - b_positive);
    let cohens_kappa = if (1.0 - expected_agreement).abs() <= f64::EPSILON {
        if agreement == 1.0 { 1.0 } else { 0.0 }
    } else {
        (agreement - expected_agreement) / (1.0 - expected_agreement)
    };
    let judge_accuracy = artifact
        .items
        .iter()
        .filter(|item| item.judge == item.adjudicated)
        .count() as f64
        / count;
    let status = if agreement >= 0.90 && cohens_kappa >= 0.80 && judge_accuracy >= 0.85 {
        ExecutionJudgeCalibrationStatusV1::Calibrated
    } else {
        ExecutionJudgeCalibrationStatusV1::Rejected
    };
    Ok(ExecutionCalibrationReportV1 {
        item_count: EXECUTION_CALIBRATION_ITEM_COUNT as u64,
        labeler_agreement: agreement,
        cohens_kappa,
        judge_accuracy,
        status,
    })
}

fn invalid_config(message: String) -> EvalError {
    EvalError::InvalidConfig(message)
}

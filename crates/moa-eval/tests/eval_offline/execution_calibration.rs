//! Exact calibration thresholds for semantic execution-eval judges.

use moa_eval::execution::{
    EXECUTION_CALIBRATION_ITEM_COUNT, ExecutionCalibrationArtifact, ExecutionCalibrationItem,
    ExecutionJudgeCalibrationStatus, score_execution_calibration,
};

fn artifact(human_disagreements: usize, judge_errors: usize) -> ExecutionCalibrationArtifact {
    ExecutionCalibrationArtifact {
        schema_version: 1,
        items: (0..EXECUTION_CALIBRATION_ITEM_COUNT)
            .map(|index| {
                let labeler_a = index % 2 == 0;
                ExecutionCalibrationItem {
                    case_id: format!("calibration-{index:03}"),
                    labeler_a,
                    labeler_b: if index < human_disagreements {
                        !labeler_a
                    } else {
                        labeler_a
                    },
                    adjudicated: labeler_a,
                    judge: if index < judge_errors {
                        !labeler_a
                    } else {
                        labeler_a
                    },
                }
            })
            .collect(),
    }
}

#[test]
fn execution_calibration_accepts_exact_agreement_kappa_and_accuracy_thresholds_offline() {
    // Pins: 90% balanced labeler agreement yields kappa 0.80 and 85% judge accuracy.
    let report = score_execution_calibration(&artifact(10, 15))
        .expect("exact threshold calibration should score");

    assert_eq!(report.item_count, 100);
    assert!((report.labeler_agreement - 0.90).abs() < 1e-12);
    assert!((report.cohens_kappa - 0.80).abs() < 1e-12);
    assert!((report.judge_accuracy - 0.85).abs() < 1e-12);
    assert_eq!(report.status, ExecutionJudgeCalibrationStatus::Calibrated);
}

#[test]
fn execution_calibration_rejects_below_threshold_and_invalid_artifacts_offline() {
    // Pins: a supplied weak calibration is rejected, while cardinality and identity drift fail.
    let report = score_execution_calibration(&artifact(11, 16))
        .expect("below-threshold calibration remains a scored artifact");
    assert_eq!(report.status, ExecutionJudgeCalibrationStatus::Rejected);

    let mut wrong_count = artifact(0, 0);
    wrong_count.items.pop();
    assert!(score_execution_calibration(&wrong_count).is_err());

    let mut duplicate = artifact(0, 0);
    duplicate.items[99].case_id = duplicate.items[0].case_id.clone();
    assert!(score_execution_calibration(&duplicate).is_err());
}

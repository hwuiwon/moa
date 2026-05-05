//! Structural anomaly signal for task-resolution scoring.

use moa_core::SegmentBaseline;

/// Segment metrics compared against tenant and intent baselines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentMetrics {
    /// Turn count attributed to the segment.
    pub turn_count: u32,
    /// Token cost attributed to the segment.
    pub token_cost: u64,
    /// Segment duration in seconds.
    pub duration_secs: f64,
}

/// Scores segment metrics against historical baselines.
#[must_use]
pub fn score(
    metrics: SegmentMetrics,
    baseline: Option<&SegmentBaseline>,
    min_samples: usize,
) -> Option<f64> {
    let baseline = baseline?;
    if baseline.sample_count < min_samples {
        return None;
    }

    if is_high_outlier(
        f64::from(metrics.turn_count),
        baseline.avg_turns,
        baseline.stddev_turns,
    ) || is_high_outlier(
        metrics.token_cost as f64,
        baseline.avg_cost,
        baseline.stddev_cost,
    ) || is_high_outlier(
        metrics.duration_secs,
        baseline.avg_duration_secs,
        baseline.stddev_duration_secs,
    ) {
        Some(0.3)
    } else {
        Some(0.6)
    }
}

fn is_high_outlier(value: f64, mean: f64, stddev: Option<f64>) -> bool {
    let Some(stddev) = stddev.filter(|stddev| *stddev > 0.0) else {
        return false;
    };
    value > mean + (2.0 * stddev)
}

#[cfg(test)]
mod tests {
    use moa_core::SegmentBaseline;

    use super::{SegmentMetrics, score};

    fn baseline() -> SegmentBaseline {
        SegmentBaseline {
            sample_count: 20,
            avg_turns: 4.0,
            stddev_turns: Some(1.0),
            avg_cost: 100.0,
            stddev_cost: Some(25.0),
            avg_duration_secs: 60.0,
            stddev_duration_secs: Some(20.0),
        }
    }

    #[test]
    fn within_one_sigma_scores_normal() {
        let metrics = SegmentMetrics {
            turn_count: 5,
            token_cost: 120,
            duration_secs: 75.0,
        };

        assert_eq!(score(metrics, Some(&baseline()), 20), Some(0.6));
    }

    #[test]
    fn above_two_sigma_scores_anomalous() {
        let metrics = SegmentMetrics {
            turn_count: 7,
            token_cost: 120,
            duration_secs: 75.0,
        };

        assert_eq!(score(metrics, Some(&baseline()), 20), Some(0.3));
    }

    #[test]
    fn cold_start_returns_none() {
        let mut baseline = baseline();
        baseline.sample_count = 19;
        let metrics = SegmentMetrics {
            turn_count: 4,
            token_cost: 100,
            duration_secs: 60.0,
        };

        assert_eq!(score(metrics, Some(&baseline), 20), None);
    }
}

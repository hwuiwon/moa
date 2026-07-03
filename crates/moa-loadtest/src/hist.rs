//! HdrHistogram-backed latency recording with windowed interval snapshots.
//!
//! All histograms store microseconds. The recorder keeps three aggregate
//! views (coordinated-omission-corrected latency measured from the intended
//! arrival time, uncorrected service time measured from actual dispatch, and
//! dispatch delay) plus rotating per-window histograms so reports can show
//! degradation and recovery over time instead of one flat aggregate.

use std::time::Duration;

use hdrhistogram::Histogram;

use crate::*;

/// Upper bound for recorded latencies: one hour, in microseconds.
const MAX_LATENCY_US: u64 = 3_600_000_000;
/// Significant figures preserved by all latency histograms.
const SIGFIGS: u8 = 3;

/// Builds an empty latency histogram with the recorder's standard bounds.
fn new_histogram() -> Result<Histogram<u64>> {
    Histogram::new_with_bounds(1, MAX_LATENCY_US, SIGFIGS)
        .map_err(|error| MoaError::ValidationError(format!("histogram construction: {error}")))
}

/// Clamps a duration into the recordable range and records it.
fn record(histogram: &mut Histogram<u64>, value: Duration) {
    let micros = u64::try_from(value.as_micros())
        .unwrap_or(MAX_LATENCY_US)
        .clamp(1, MAX_LATENCY_US);
    if histogram.record(micros).is_err() {
        // Unreachable after clamping, but never panic in the collector.
        tracing::warn!(micros, "latency sample outside histogram bounds; dropped");
    }
}

/// One rotating measurement window.
struct Window {
    latency_corrected: Histogram<u64>,
    turns_completed: u64,
    turn_errors: u64,
}

/// Single-owner latency recorder driven by the collector task.
pub(crate) struct LatencyRecorder {
    corrected: Histogram<u64>,
    uncorrected: Histogram<u64>,
    dispatch_delay: Histogram<u64>,
    ttft: Histogram<u64>,
    windows: Vec<Window>,
    window_len: Duration,
    warmup: Duration,
}

impl LatencyRecorder {
    /// Creates a recorder with the given window length and warmup exclusion.
    pub(crate) fn new(window_len: Duration, warmup: Duration) -> Result<Self> {
        Ok(Self {
            corrected: new_histogram()?,
            uncorrected: new_histogram()?,
            dispatch_delay: new_histogram()?,
            ttft: new_histogram()?,
            windows: Vec::new(),
            window_len: window_len.max(Duration::from_secs(1)),
            warmup,
        })
    }

    fn window_at(&mut self, completed: Duration) -> Result<&mut Window> {
        let index = (completed.as_secs_f64() / self.window_len.as_secs_f64()) as usize;
        while self.windows.len() <= index {
            self.windows.push(Window {
                latency_corrected: new_histogram()?,
                turns_completed: 0,
                turn_errors: 0,
            });
        }
        Ok(&mut self.windows[index])
    }

    /// Records one completed turn. All offsets are measured from run start;
    /// `ttft` is measured from actual dispatch because time-to-first-token is
    /// a per-request service property, not an arrival-rate property.
    pub(crate) fn record_turn(
        &mut self,
        intended: Duration,
        dispatched: Duration,
        completed: Duration,
        ttft: Option<Duration>,
    ) -> Result<()> {
        let corrected = completed.saturating_sub(intended);
        let uncorrected = completed.saturating_sub(dispatched);
        let delay = dispatched.saturating_sub(intended);
        if completed >= self.warmup {
            record(&mut self.corrected, corrected);
            record(&mut self.uncorrected, uncorrected);
            record(&mut self.dispatch_delay, delay);
            if let Some(ttft) = ttft {
                record(&mut self.ttft, ttft);
            }
        }
        let window = self.window_at(completed)?;
        record(&mut window.latency_corrected, corrected);
        window.turns_completed += 1;
        Ok(())
    }

    /// Records one failed turn at its completion offset.
    pub(crate) fn record_turn_error(&mut self, completed: Duration) -> Result<()> {
        self.window_at(completed)?.turn_errors += 1;
        Ok(())
    }

    /// Summarizes the corrected-latency aggregate.
    pub(crate) fn corrected_summary(&self) -> PercentileSummary {
        histogram_summary(&self.corrected)
    }

    /// Summarizes the uncorrected (service-time) aggregate.
    pub(crate) fn uncorrected_summary(&self) -> PercentileSummary {
        histogram_summary(&self.uncorrected)
    }

    /// Summarizes dispatch delay (intended arrival to actual dispatch).
    pub(crate) fn dispatch_delay_summary(&self) -> PercentileSummary {
        histogram_summary(&self.dispatch_delay)
    }

    /// Summarizes TTFT samples.
    pub(crate) fn ttft_summary(&self) -> PercentileSummary {
        histogram_summary(&self.ttft)
    }

    /// Renders the per-window series for the final report.
    pub(crate) fn window_reports(&self) -> Vec<WindowReport> {
        self.windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                let start = self.window_len.as_secs_f64() * index as f64;
                let end = start + self.window_len.as_secs_f64();
                WindowReport {
                    start_ms: start * 1_000.0,
                    end_ms: end * 1_000.0,
                    warmup: Duration::from_secs_f64(end) <= self.warmup,
                    turns_completed: window.turns_completed,
                    turn_errors: window.turn_errors,
                    latency_corrected_ms: histogram_summary(&window.latency_corrected),
                }
            })
            .collect()
    }

    /// Number of post-warmup corrected samples recorded.
    #[cfg(test)]
    pub(crate) fn corrected_len(&self) -> u64 {
        self.corrected.len()
    }

    /// Warmup prefix excluded from aggregates.
    pub(crate) fn warmup(&self) -> Duration {
        self.warmup
    }
}

/// Base64 V2-serialized aggregate histograms, embedded in JSON reports so
/// multi-worker runs can merge losslessly (HdrHistogram addition is exact).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedHistograms {
    /// Corrected turn latency (from intended arrival).
    pub corrected: String,
    /// Uncorrected service time (from dispatch).
    pub uncorrected: String,
    /// Dispatch delay (intended arrival to dispatch).
    pub dispatch_delay: String,
    /// TTFT samples.
    pub ttft: String,
}

/// Serializes one histogram to base64 V2 wire format.
fn serialize_histogram(histogram: &Histogram<u64>) -> Result<String> {
    use base64::Engine as _;
    use hdrhistogram::serialization::{Serializer as _, V2Serializer};

    let mut buffer = Vec::new();
    V2Serializer::new()
        .serialize(histogram, &mut buffer)
        .map_err(|error| MoaError::SerializationError(format!("hdr serialize: {error}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buffer))
}

/// Deserializes one histogram from base64 V2 wire format.
pub(crate) fn deserialize_histogram(encoded: &str) -> Result<Histogram<u64>> {
    use base64::Engine as _;
    use hdrhistogram::serialization::Deserializer;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| MoaError::SerializationError(format!("hdr base64: {error}")))?;
    Deserializer::new()
        .deserialize(&mut bytes.as_slice())
        .map_err(|error| MoaError::SerializationError(format!("hdr deserialize: {error}")))
}

impl LatencyRecorder {
    /// Serializes the aggregate histograms for the report artifact.
    pub(crate) fn serialized(&self) -> Result<SerializedHistograms> {
        Ok(SerializedHistograms {
            corrected: serialize_histogram(&self.corrected)?,
            uncorrected: serialize_histogram(&self.uncorrected)?,
            dispatch_delay: serialize_histogram(&self.dispatch_delay)?,
            ttft: serialize_histogram(&self.ttft)?,
        })
    }
}

/// Converts a microsecond histogram into a millisecond percentile summary.
pub(crate) fn histogram_summary(histogram: &Histogram<u64>) -> PercentileSummary {
    if histogram.is_empty() {
        return PercentileSummary {
            min: 0.0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }
    let to_ms = |micros: u64| micros as f64 / 1_000.0;
    PercentileSummary {
        min: to_ms(histogram.min()),
        mean: histogram.mean() / 1_000.0,
        p50: to_ms(histogram.value_at_quantile(0.50)),
        p95: to_ms(histogram.value_at_quantile(0.95)),
        p99: to_ms(histogram.value_at_quantile(0.99)),
        max: to_ms(histogram.max()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrected_latency_includes_dispatch_delay_and_uncorrected_does_not() {
        // Pins: coordinated-omission correction measures from the intended
        // arrival, so a turn dispatched 900ms late with a 100ms service time
        // reports ~1000ms corrected and ~100ms uncorrected.
        let mut recorder =
            LatencyRecorder::new(Duration::from_secs(10), Duration::ZERO).expect("recorder");
        recorder
            .record_turn(
                Duration::from_secs(1),
                Duration::from_millis(1_900),
                Duration::from_secs(2),
                None,
            )
            .expect("record");

        let corrected = recorder.corrected_summary();
        let uncorrected = recorder.uncorrected_summary();
        let delay = recorder.dispatch_delay_summary();

        assert!(
            (corrected.p50 - 1_000.0).abs() < 5.0,
            "corrected {corrected:?}"
        );
        assert!(
            (uncorrected.p50 - 100.0).abs() < 5.0,
            "uncorrected {uncorrected:?}"
        );
        assert!((delay.p50 - 900.0).abs() < 5.0, "delay {delay:?}");
    }

    #[test]
    fn warmup_samples_are_excluded_from_aggregates_but_kept_in_windows() {
        // Pins: warmup turns never pollute the SLO aggregate, yet the window
        // series still shows them so ramp behavior stays visible.
        let mut recorder =
            LatencyRecorder::new(Duration::from_secs(5), Duration::from_secs(5)).expect("recorder");
        recorder
            .record_turn(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
                None,
            )
            .expect("warmup turn");
        recorder
            .record_turn(
                Duration::from_secs(6),
                Duration::from_secs(6),
                Duration::from_secs(7),
                None,
            )
            .expect("measured turn");

        assert_eq!(recorder.corrected_len(), 1);
        let windows = recorder.window_reports();
        assert_eq!(windows.len(), 2);
        assert!(windows[0].warmup);
        assert_eq!(windows[0].turns_completed, 1);
        assert!(!windows[1].warmup);
        assert_eq!(windows[1].turns_completed, 1);
    }

    #[test]
    fn turn_errors_are_counted_in_their_window() {
        // Pins: window error counts land in the window of the failure time.
        let mut recorder =
            LatencyRecorder::new(Duration::from_secs(5), Duration::ZERO).expect("recorder");
        recorder
            .record_turn_error(Duration::from_secs(7))
            .expect("record error");

        let windows = recorder.window_reports();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1].turn_errors, 1);
        assert_eq!(windows[0].turn_errors, 0);
    }
}

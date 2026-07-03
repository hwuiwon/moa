//! Lossless merging of multi-worker load-test reports.
//!
//! T3 scale-out runs shard the schedule across worker processes/hosts; each
//! worker writes its normal `--output json` report, which embeds base64
//! V2-serialized HdrHistograms. Merging adds the histograms (exact, unlike
//! merging percentiles) and sums the counters.

use std::collections::BTreeMap;
use std::path::Path;

use crate::*;

/// Aggregate view over several worker reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedSummary {
    /// Number of worker reports merged.
    pub workers: usize,
    /// Sum of offered rates across workers.
    pub requested_rate_qps: f64,
    /// Sum of achieved rates across workers.
    pub achieved_rate_qps: f64,
    /// Summed scheduled arrivals.
    pub turns_scheduled: u64,
    /// Summed completed turns.
    pub turns_completed: u64,
    /// Summed error taxonomy.
    pub errors: ErrorTaxonomy,
    /// Summed session counters.
    pub sessions_started: usize,
    /// Summed completed sessions.
    pub sessions_completed: usize,
    /// Summed failed sessions.
    pub sessions_failed: usize,
    /// Merged corrected turn latency.
    pub turn_latency_corrected_ms: PercentileSummary,
    /// Merged uncorrected service time.
    pub turn_latency_ms: PercentileSummary,
    /// Merged dispatch delay.
    pub dispatch_delay_ms: PercentileSummary,
    /// Merged TTFT.
    pub ttft_ms: PercentileSummary,
    /// Merged edge observation lag.
    pub edge_observation_wait_ms: PercentileSummary,
    /// Summed durable event-log resource bill.
    pub resource_bill: ResourceBillReport,
}

fn add_errors(total: &mut ErrorTaxonomy, part: &ErrorTaxonomy) {
    total.turn_start_failures += part.turn_start_failures;
    total.turn_timeouts += part.turn_timeouts;
    total.turn_failures += part.turn_failures;
    total.turn_cancellations += part.turn_cancellations;
    total.arrivals_dropped += part.arrivals_dropped;
    total.event_load_failures += part.event_load_failures;
    total.session_setup_failures += part.session_setup_failures;
    total.event_error_events += part.event_error_events;
    total.tool_error_events += part.tool_error_events;
}

/// Merges worker report JSON files into one summary.
pub fn merge_report_files(paths: &[impl AsRef<Path>]) -> Result<MergedSummary> {
    if paths.is_empty() {
        return Err(MoaError::ValidationError(
            "merge requires at least one report file".to_string(),
        ));
    }
    let mut corrected: Option<hdrhistogram::Histogram<u64>> = None;
    let mut uncorrected: Option<hdrhistogram::Histogram<u64>> = None;
    let mut dispatch: Option<hdrhistogram::Histogram<u64>> = None;
    let mut ttft: Option<hdrhistogram::Histogram<u64>> = None;
    let mut edge_observation_wait: Option<hdrhistogram::Histogram<u64>> = None;
    let mut event_rows_by_type = BTreeMap::new();
    let mut merged = MergedSummary {
        workers: 0,
        requested_rate_qps: 0.0,
        achieved_rate_qps: 0.0,
        turns_scheduled: 0,
        turns_completed: 0,
        errors: ErrorTaxonomy::default(),
        sessions_started: 0,
        sessions_completed: 0,
        sessions_failed: 0,
        turn_latency_corrected_ms: histogram_summary(&empty_histogram()?),
        turn_latency_ms: histogram_summary(&empty_histogram()?),
        dispatch_delay_ms: histogram_summary(&empty_histogram()?),
        ttft_ms: histogram_summary(&empty_histogram()?),
        edge_observation_wait_ms: histogram_summary(&empty_histogram()?),
        resource_bill: ResourceBillReport::default(),
    };

    for path in paths {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path).map_err(|error| {
            MoaError::ValidationError(format!("read report {}: {error}", path.display()))
        })?;
        let report: LoadTestReport = serde_json::from_str(&body).map_err(|error| {
            MoaError::SerializationError(format!("parse report {}: {error}", path.display()))
        })?;
        let hdr = report.hdr.as_ref().ok_or_else(|| {
            MoaError::ValidationError(format!(
                "report {} has no embedded histograms; regenerate with --output json",
                path.display()
            ))
        })?;
        merge_into(&mut corrected, &hdr.corrected)?;
        merge_into(&mut uncorrected, &hdr.uncorrected)?;
        merge_into(&mut dispatch, &hdr.dispatch_delay)?;
        merge_into(&mut ttft, &hdr.ttft)?;
        if !hdr.edge_observation_wait.is_empty() {
            merge_into(&mut edge_observation_wait, &hdr.edge_observation_wait)?;
        }

        merged.workers += 1;
        merged.requested_rate_qps += report.requested_rate_qps;
        merged.achieved_rate_qps += report.achieved_rate_qps;
        merged.turns_scheduled += report.turns_scheduled;
        merged.turns_completed += report.turns_completed;
        add_errors(&mut merged.errors, &report.errors);
        merged.sessions_started += report.sessions_started;
        merged.sessions_completed += report.sessions_completed;
        merged.sessions_failed += report.sessions_failed;
        for item in report.resource_bill.event_rows_by_type {
            *event_rows_by_type.entry(item.event_type).or_insert(0) += item.rows;
        }
    }

    if let Some(histogram) = &corrected {
        merged.turn_latency_corrected_ms = histogram_summary(histogram);
    }
    if let Some(histogram) = &uncorrected {
        merged.turn_latency_ms = histogram_summary(histogram);
    }
    if let Some(histogram) = &dispatch {
        merged.dispatch_delay_ms = histogram_summary(histogram);
    }
    if let Some(histogram) = &ttft {
        merged.ttft_ms = histogram_summary(histogram);
    }
    if let Some(histogram) = &edge_observation_wait {
        merged.edge_observation_wait_ms = histogram_summary(histogram);
    }
    merged.resource_bill = resource_bill_from_rows(event_rows_by_type, merged.turns_completed);
    Ok(merged)
}

fn resource_bill_from_rows(
    event_rows_by_type: BTreeMap<String, u64>,
    turns_completed: u64,
) -> ResourceBillReport {
    let event_rows_by_type = event_rows_by_type
        .into_iter()
        .filter(|(_, rows)| *rows > 0)
        .map(|(event_type, rows)| EventAppendTypeReport { event_type, rows })
        .collect::<Vec<_>>();
    let durable_event_rows = event_rows_by_type.iter().map(|item| item.rows).sum();
    let progress_update_rows = rows_for_event_type(&event_rows_by_type, "ProgressUpdate");
    let progress_narrated_rows = rows_for_event_type(&event_rows_by_type, "ProgressNarrated");
    ResourceBillReport {
        durable_event_rows,
        durable_event_rows_per_turn: merged_per_turn(durable_event_rows, turns_completed),
        progress_update_rows,
        progress_update_rows_per_turn: merged_per_turn(progress_update_rows, turns_completed),
        progress_narrated_rows,
        progress_narrated_rows_per_turn: merged_per_turn(progress_narrated_rows, turns_completed),
        event_rows_by_type,
    }
}

fn rows_for_event_type(event_rows_by_type: &[EventAppendTypeReport], event_type: &str) -> u64 {
    event_rows_by_type
        .iter()
        .find(|item| item.event_type == event_type)
        .map(|item| item.rows)
        .unwrap_or_default()
}

fn merged_per_turn(rows: u64, turns_completed: u64) -> f64 {
    if turns_completed == 0 {
        return 0.0;
    }
    rows as f64 / turns_completed as f64
}

fn empty_histogram() -> Result<hdrhistogram::Histogram<u64>> {
    hdrhistogram::Histogram::new(3)
        .map_err(|error| MoaError::ValidationError(format!("histogram construction: {error}")))
}

fn merge_into(accumulator: &mut Option<hdrhistogram::Histogram<u64>>, encoded: &str) -> Result<()> {
    let part = deserialize_histogram(encoded)?;
    match accumulator {
        Some(total) => total
            .add(&part)
            .map_err(|error| MoaError::SerializationError(format!("hdr histogram add: {error}")))?,
        None => *accumulator = Some(part),
    }
    Ok(())
}

/// Renders the merged summary as human-readable text.
pub fn render_merged_summary(summary: &MergedSummary) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "MOA Merged Load Test Summary");
    let _ = writeln!(&mut output, "============================");
    let _ = writeln!(
        &mut output,
        "Workers: {} | Rate: {:.1}/s requested, {:.1}/s achieved",
        summary.workers, summary.requested_rate_qps, summary.achieved_rate_qps
    );
    let _ = writeln!(
        &mut output,
        "Turns: {} scheduled, {} completed | failed: {}",
        summary.turns_scheduled,
        summary.turns_completed,
        summary.errors.failed_turns()
    );
    let _ = writeln!(
        &mut output,
        "Turn Latency (corrected):\n  p50: {}  p95: {}  p99: {}  max: {}",
        format_millis(summary.turn_latency_corrected_ms.p50),
        format_millis(summary.turn_latency_corrected_ms.p95),
        format_millis(summary.turn_latency_corrected_ms.p99),
        format_millis(summary.turn_latency_corrected_ms.max)
    );
    let _ = writeln!(
        &mut output,
        "Turn Service Time:\n  p50: {}  p95: {}  p99: {}",
        format_millis(summary.turn_latency_ms.p50),
        format_millis(summary.turn_latency_ms.p95),
        format_millis(summary.turn_latency_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "Dispatch Delay:\n  p50: {}  p95: {}  p99: {}",
        format_millis(summary.dispatch_delay_ms.p50),
        format_millis(summary.dispatch_delay_ms.p95),
        format_millis(summary.dispatch_delay_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "TTFT:\n  p50: {}  p95: {}  p99: {}",
        format_millis(summary.ttft_ms.p50),
        format_millis(summary.ttft_ms.p95),
        format_millis(summary.ttft_ms.p99)
    );
    if summary.edge_observation_wait_ms.max > 0.0 {
        let _ = writeln!(
            &mut output,
            "Edge Observation Wait:\n  p50: {}  p95: {}  p99: {}",
            format_millis(summary.edge_observation_wait_ms.p50),
            format_millis(summary.edge_observation_wait_ms.p95),
            format_millis(summary.edge_observation_wait_ms.p99)
        );
    }
    if summary.resource_bill.durable_event_rows > 0 {
        let _ = writeln!(
            &mut output,
            "Resource Bill:\n  durable event rows: {} ({:.2}/turn) | ProgressUpdate: {} ({:.2}/turn) | ProgressNarrated: {} ({:.2}/turn)",
            summary.resource_bill.durable_event_rows,
            summary.resource_bill.durable_event_rows_per_turn,
            summary.resource_bill.progress_update_rows,
            summary.resource_bill.progress_update_rows_per_turn,
            summary.resource_bill.progress_narrated_rows,
            summary.resource_bill.progress_narrated_rows_per_turn
        );
    }
    let _ = writeln!(
        &mut output,
        "Sessions: {} started, {} completed, {} failed",
        summary.sessions_started, summary.sessions_completed, summary.sessions_failed
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_histograms_are_exact_across_workers() {
        // Pins: merging two worker reports adds histograms losslessly — the
        // merged p99 sees the slow worker's tail (4 of 100 samples) while the
        // merged p95 still reflects the fast majority.
        let mut fast =
            LatencyRecorder::new(Duration::from_secs(10), Duration::ZERO).expect("fast recorder");
        for _ in 0..96 {
            fast.record_turn(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_millis(1_010),
                None,
                None,
            )
            .expect("fast turn");
        }
        let mut slow =
            LatencyRecorder::new(Duration::from_secs(10), Duration::ZERO).expect("slow recorder");
        for _ in 0..4 {
            slow.record_turn(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(3),
                None,
                None,
            )
            .expect("slow turn");
        }

        let dir = std::env::temp_dir().join(format!("moa-merge-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut paths = Vec::new();
        for (index, recorder) in [&fast, &slow].into_iter().enumerate() {
            let report = template_report(recorder);
            let path = dir.join(format!("worker-{index}.json"));
            std::fs::write(&path, serde_json::to_string(&report).expect("serialize"))
                .expect("write report");
            paths.push(path);
        }

        let merged = merge_report_files(&paths).expect("merge");

        assert_eq!(merged.workers, 2);
        assert_eq!(merged.turns_completed, 100);
        assert!(
            merged.turn_latency_corrected_ms.p99 > 1_500.0,
            "merged p99 must include the slow worker tail: {:?}",
            merged.turn_latency_corrected_ms
        );
        assert!(
            merged.turn_latency_corrected_ms.p95 < 100.0,
            "merged p95 must still reflect the fast majority: {:?}",
            merged.turn_latency_corrected_ms
        );
        assert_eq!(merged.resource_bill.durable_event_rows, 100);
        assert_eq!(merged.resource_bill.durable_event_rows_per_turn, 1.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn template_report(recorder: &LatencyRecorder) -> LoadTestReport {
        LoadTestReport {
            mode: LoadMode::Mock,
            endpoint: "http://localhost:10010".to_string(),
            profile: SessionProfileKind::Short,
            requested_rate_qps: 10.0,
            achieved_rate_qps: 10.0,
            sessions_started: 1,
            sessions_completed: 1,
            sessions_failed: 0,
            turns_scheduled: recorder.corrected_len(),
            turns_completed: recorder.corrected_len(),
            errors: ErrorTaxonomy::default(),
            total_tool_calls: 0,
            auto_denied_approvals: 0,
            duration_ms: 10_000.0,
            warmup_ms: 0.0,
            turn_latency_corrected_ms: recorder.corrected_summary(),
            turn_latency_ms: recorder.uncorrected_summary(),
            dispatch_delay_ms: recorder.dispatch_delay_summary(),
            ttft_ms: recorder.ttft_summary(),
            edge_observation_wait_ms: recorder.edge_observation_wait_summary(),
            step_latency_ms: Vec::new(),
            event_append_phase_latency_ms: Vec::new(),
            resource_bill: ResourceBillReport {
                durable_event_rows: recorder.corrected_len(),
                durable_event_rows_per_turn: 1.0,
                progress_update_rows: 0,
                progress_update_rows_per_turn: 0.0,
                progress_narrated_rows: 0,
                progress_narrated_rows_per_turn: 0.0,
                event_rows_by_type: vec![EventAppendTypeReport {
                    event_type: "BrainResponse".to_string(),
                    rows: recorder.corrected_len(),
                }],
            },
            cache_hit_rate: recorder.corrected_summary(),
            total_cost_cents: 0,
            windows: Vec::new(),
            tenant_ids: Vec::new(),
            hdr: Some(recorder.serialized().expect("serialize histograms")),
            sessions: Vec::new(),
        }
    }
}

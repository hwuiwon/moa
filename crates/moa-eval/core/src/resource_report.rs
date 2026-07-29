//! Adapters between eval results and the runtime resource contract.
//!
//! Two directions are needed, and both live here rather than in `moa-core` so
//! the runtime contract stays free of eval types:
//!
//! * [`usage_from_metrics`] turns an observed [`EvalMetrics`] reading into the
//!   [`ResourceAmounts`] handed back to [`moa_core::types::resource::ResourceLedger::reconcile`];
//! * [`RunResourceReport`] turns a ledger snapshot plus scheduling counters into
//!   a serializable record attached to a completed run.

use moa_core::types::resource::{
    ResourceAmounts, ResourceError, ResourceLedgerSnapshot, SharedResourceLedger,
};
use serde::{Deserialize, Serialize};

use crate::EvalMetrics;

/// Converts observed run metrics into reconcilable resource amounts.
///
/// Turns are counted as model calls when no distinct model-call counter exists:
/// the eval collector records one model response per turn, so the two are the
/// same observation at this layer.
pub fn usage_from_metrics(metrics: &EvalMetrics) -> Result<ResourceAmounts, ResourceError> {
    Ok(ResourceAmounts {
        cost_micro_usd: ResourceAmounts::cost_micro_usd_from_dollars(metrics.cost_dollars)?,
        tokens: metrics.total_tokens as u64,
        turns: metrics.turn_count as u64,
        model_calls: metrics.turn_count as u64,
        tool_calls: metrics.tool_call_count as u64,
    })
}

/// Resource accounting attached to a completed eval run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResourceReport {
    /// Version of the admission limits that admitted the run.
    pub admission_version: u32,
    /// Ledger state at the end of the run.
    pub ledger: ResourceLedgerSnapshot,
    /// Worst-case amounts reserved before each dispatched case.
    pub per_case_reservation: ResourceAmounts,
    /// `per_case_reservation * planned_cases`, proven not to overflow.
    pub worst_case_projection: ResourceAmounts,
    /// Bounded concurrency the run was admitted for.
    pub parallel: usize,
    /// `(config, case)` executions the run planned.
    pub planned_cases: usize,
    /// Cases that reserved capacity and were dispatched.
    pub dispatched_cases: usize,
    /// Cases never dispatched because the ledger refused a reservation.
    pub unreserved_cases: usize,
    /// Reason scheduling stopped early, when it did.
    pub stop_reason: Option<String>,
}

impl RunResourceReport {
    /// Builds a report from the run's ledger and scheduling counters.
    #[must_use]
    pub fn new(
        admission_version: u32,
        ledger: &SharedResourceLedger,
        per_case_reservation: ResourceAmounts,
        worst_case_projection: ResourceAmounts,
        parallel: usize,
        planned_cases: usize,
    ) -> Self {
        Self {
            admission_version,
            ledger: ledger.snapshot(),
            per_case_reservation,
            worst_case_projection,
            parallel,
            planned_cases,
            dispatched_cases: 0,
            unreserved_cases: 0,
            stop_reason: None,
        }
    }

    /// Records that a case reserved capacity and was dispatched.
    pub fn record_dispatched(&mut self) {
        self.dispatched_cases += 1;
    }

    /// Records that a case was refused capacity, with the reason scheduling
    /// stopped when this was the first refusal.
    pub fn record_unreserved(&mut self, reason: &str) {
        self.unreserved_cases += 1;
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason.to_string());
        }
    }

    /// Refreshes the ledger snapshot after all cases have reconciled.
    pub fn refresh(&mut self, ledger: &SharedResourceLedger) {
        self.ledger = ledger.snapshot();
    }
}

#[cfg(test)]
mod tests {
    use super::{RunResourceReport, usage_from_metrics};
    use crate::EvalMetrics;
    use moa_core::types::resource::{
        ResourceAmounts, ResourceEnvelope, ResourceError, SharedResourceLedger,
    };

    #[test]
    fn metrics_convert_to_reconcilable_amounts_with_conservative_cost() {
        let metrics = EvalMetrics {
            total_tokens: 1_234,
            cost_dollars: 0.0000151,
            turn_count: 3,
            tool_call_count: 7,
            ..EvalMetrics::default()
        };

        let usage = usage_from_metrics(&metrics).expect("valid metrics");
        assert_eq!(usage.tokens, 1_234);
        assert_eq!(usage.turns, 3);
        assert_eq!(usage.model_calls, 3);
        assert_eq!(usage.tool_calls, 7);
        // Fractional micro-dollars round up so real spend is never free.
        assert_eq!(usage.cost_micro_usd, 16);
    }

    #[test]
    fn negative_cost_metrics_are_rejected() {
        let metrics = EvalMetrics {
            cost_dollars: -1.0,
            ..EvalMetrics::default()
        };
        assert!(matches!(
            usage_from_metrics(&metrics),
            Err(ResourceError::InvalidCost { .. })
        ));
    }

    #[test]
    fn report_tracks_dispatch_and_refusal_counters() {
        let ledger = SharedResourceLedger::from_envelope(ResourceEnvelope::new(
            ResourceAmounts {
                cost_micro_usd: 100,
                ..ResourceAmounts::ZERO
            },
            None,
        ))
        .expect("ledger");
        let mut report = RunResourceReport::new(
            1,
            &ledger,
            ResourceAmounts::ZERO,
            ResourceAmounts::ZERO,
            2,
            4,
        );

        report.record_dispatched();
        report.record_unreserved("envelope exhausted");
        report.record_unreserved("scheduling stopped");

        assert_eq!(report.dispatched_cases, 1);
        assert_eq!(report.unreserved_cases, 2);
        assert_eq!(report.stop_reason.as_deref(), Some("envelope exhausted"));
        assert_eq!(report.planned_cases, 4);
    }
}

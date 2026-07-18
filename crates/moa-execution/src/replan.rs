//! Deterministic replan failure fingerprinting and stop evaluation.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{ExecutionBudgetLimit, PlanAmendment, PlanAmendmentOperation};
use moa_core::config::ExecutionConfig;
use serde::{Deserialize, Serialize};

use crate::{
    Result,
    budget::estimate_fits_limit,
    capability::{ExecutionEstimate, ExecutionHash, FAILURE_HASH_DOMAIN, hash_serializable},
    completion::CompletionStatus,
    state::FailureFingerprintInput,
};

const REPLAN_STOP_DETAIL_MAX_CHARS: usize = 512;

/// Complete pure input to replan-stop evaluation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplanEvaluationRequest {
    /// Deterministic evaluation time.
    pub now: DateTime<Utc>,
    /// Approved budget remaining for replacement work.
    pub remaining_budget: ExecutionBudgetLimit,
    /// Worst-case estimate of the proposed remaining plan.
    pub proposed_estimate: ExecutionEstimate,
    /// Canonical hash of the proposed plan.
    pub proposed_plan_hash: ExecutionHash,
    /// Canonical fingerprint of the proposed amendment's operation semantics.
    pub proposed_amendment_fingerprint: ExecutionHash,
    /// Plan hashes already observed by this run.
    pub seen_plan_hashes: BTreeSet<ExecutionHash>,
    /// Amendment-operation fingerprints already observed by this run.
    pub seen_amendment_fingerprints: BTreeSet<ExecutionHash>,
    /// Persisted occurrence counts keyed by normalized failure fingerprint.
    pub failure_fingerprint_counts: BTreeMap<ExecutionHash, u32>,
    /// Current failure that triggered replanning, when present.
    pub current_failure: Option<FailureFingerprintInput>,
    /// Goal requirements not yet satisfied.
    pub unresolved_requirement_ids: BTreeSet<String>,
    /// Exact restricted amendment being considered.
    pub amendment: PlanAmendment,
    /// Repeated-failure threshold and execution defaults.
    pub config: ExecutionConfig,
}

/// Pure loop-identity and progress input available before compiler validation succeeds.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplanLoopEvaluationRequest {
    /// Canonical fingerprint of the proposed amendment's operation semantics.
    pub proposed_amendment_fingerprint: ExecutionHash,
    /// Amendment-operation fingerprints already observed by this run.
    pub seen_amendment_fingerprints: BTreeSet<ExecutionHash>,
    /// Persisted occurrence counts keyed by normalized failure fingerprint.
    pub failure_fingerprint_counts: BTreeMap<ExecutionHash, u32>,
    /// Current failure that triggered replanning, when present.
    pub current_failure: Option<FailureFingerprintInput>,
    /// Goal requirements not yet satisfied.
    pub unresolved_requirement_ids: BTreeSet<String>,
    /// Exact restricted amendment being considered.
    pub amendment: PlanAmendment,
    /// Repeated-failure threshold and execution defaults.
    pub config: ExecutionConfig,
}

/// Decision returned by deterministic replan-stop evaluation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplanDecision {
    /// The amendment may proceed to compiler validation.
    Continue,
    /// Replanning must stop with the exact reason.
    Stop {
        /// Deterministic reason no further amendment may be attempted.
        reason: ReplanStopReason,
    },
}

/// Fixed reasons that terminate replanning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanStopReason {
    /// The proposed plan hash was already observed.
    DuplicatePlan,
    /// The proposed amendment-operation fingerprint was already observed.
    DuplicateAmendment,
    /// The same normalized failure reached the configured loop threshold.
    RepeatedFailure,
    /// The amendment does not measurably advance an unresolved requirement.
    NoProgress,
    /// The approved execution deadline elapsed.
    DeadlineExceeded,
    /// The proposed estimate does not fit remaining approved resources.
    BudgetExhausted,
}

impl ReplanStopReason {
    /// Returns the stable snake-case terminal evidence value for this stop reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicatePlan => "duplicate_plan",
            Self::DuplicateAmendment => "duplicate_amendment",
            Self::RepeatedFailure => "repeated_failure",
            Self::NoProgress => "no_progress",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// Hashes the normalized failure fields with the fixed failure domain.
pub fn failure_fingerprint(input: &FailureFingerprintInput) -> Result<ExecutionHash> {
    #[derive(Serialize)]
    struct NormalizedFailure<'a> {
        class: &'a moa_artifacts::execution_plan::ExecutionFailureClass,
        node_id: &'a str,
        capability_ref: &'a Option<moa_artifacts::execution_plan::CapabilityReference>,
        message: String,
    }

    hash_serializable(
        FAILURE_HASH_DOMAIN,
        &NormalizedFailure {
            class: &input.class,
            node_id: &input.node_id,
            capability_ref: &input.capability_ref,
            message: normalize_failure_message(&input.message),
        },
    )
}

/// Applies the fixed stop-condition precedence to one proposed amendment.
#[must_use]
pub fn evaluate_replan_stop(request: ReplanEvaluationRequest) -> ReplanDecision {
    if let Some(reason) = evaluate_replan_resource_stop(
        request.now,
        &request.remaining_budget,
        request.proposed_estimate,
    ) {
        return stop(reason);
    }
    if request
        .seen_plan_hashes
        .contains(&request.proposed_plan_hash)
    {
        return stop(ReplanStopReason::DuplicatePlan);
    }
    evaluate_replan_loop_stop(ReplanLoopEvaluationRequest {
        proposed_amendment_fingerprint: request.proposed_amendment_fingerprint,
        seen_amendment_fingerprints: request.seen_amendment_fingerprints,
        failure_fingerprint_counts: request.failure_fingerprint_counts,
        current_failure: request.current_failure,
        unresolved_requirement_ids: request.unresolved_requirement_ids,
        amendment: request.amendment,
        config: request.config,
    })
}

/// Applies the deadline and remaining-resource prefix of the fixed stop precedence.
#[must_use]
pub fn evaluate_replan_resource_stop(
    now: DateTime<Utc>,
    remaining_budget: &ExecutionBudgetLimit,
    proposed_estimate: ExecutionEstimate,
) -> Option<ReplanStopReason> {
    if remaining_budget
        .deadline_at
        .is_some_and(|deadline| now > deadline)
    {
        return Some(ReplanStopReason::DeadlineExceeded);
    }
    if estimate_fits_limit(proposed_estimate, remaining_budget).is_err() {
        return Some(ReplanStopReason::BudgetExhausted);
    }
    None
}

/// Applies operation-loop, repeated-failure, and measurable-progress stop precedence.
#[must_use]
pub fn evaluate_replan_loop_stop(request: ReplanLoopEvaluationRequest) -> ReplanDecision {
    if request
        .seen_amendment_fingerprints
        .contains(&request.proposed_amendment_fingerprint)
    {
        return stop(ReplanStopReason::DuplicateAmendment);
    }
    if let Some(failure) = &request.current_failure
        && let Ok(fingerprint) = failure_fingerprint(failure)
    {
        let prior = request
            .failure_fingerprint_counts
            .get(&fingerprint)
            .copied()
            .unwrap_or(0);
        if prior.saturating_add(1) >= request.config.repeated_failure_limit {
            return stop(ReplanStopReason::RepeatedFailure);
        }
    }
    if !amendment_advances_unresolved_requirements(
        &request.amendment,
        &request.unresolved_requirement_ids,
    ) {
        return stop(ReplanStopReason::NoProgress);
    }
    ReplanDecision::Continue
}

/// Returns whether an amendment adds or replaces work serving an unresolved requirement.
#[must_use]
pub fn amendment_advances_unresolved_requirements(
    amendment: &PlanAmendment,
    unresolved_requirement_ids: &BTreeSet<String>,
) -> bool {
    amendment.operations.iter().any(|operation| {
        let node = match operation {
            PlanAmendmentOperation::AddNode { node }
            | PlanAmendmentOperation::ReplacePendingNode { node, .. } => node,
            PlanAmendmentOperation::RemovePendingNode { .. } => return false,
        };
        node.requirement_ids
            .iter()
            .any(|id| unresolved_requirement_ids.contains(id))
    })
}

/// Selects the exact terminal status for a replan stop from useful completed work.
#[must_use]
pub const fn replan_stop_status(
    has_terminal_output: bool,
    satisfied_requirement_count: usize,
) -> CompletionStatus {
    if has_terminal_output || satisfied_requirement_count > 0 {
        CompletionStatus::Partial
    } else {
        CompletionStatus::Blocked
    }
}

/// Builds stable typed replan-stop gap evidence plus optional bounded diagnostic detail.
#[must_use]
pub fn replan_stop_gaps(reason: ReplanStopReason, detail: Option<&str>) -> Vec<String> {
    let mut gaps = vec![format!("replan stop reason: {}", reason.as_str())];
    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        let bounded = detail
            .chars()
            .take(REPLAN_STOP_DETAIL_MAX_CHARS)
            .collect::<String>();
        gaps.push(format!("replan stopped: {bounded}"));
    }
    gaps
}

fn normalize_failure_message(message: &str) -> String {
    message
        .trim()
        .to_ascii_lowercase()
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const fn stop(reason: ReplanStopReason) -> ReplanDecision {
    ReplanDecision::Stop { reason }
}

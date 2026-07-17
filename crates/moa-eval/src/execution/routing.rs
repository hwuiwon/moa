//! Deterministic scripted-provider scoring for the production execution-mode router.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use moa_brain::execution_planning::{
    EXECUTION_ROUTER_RESPONSE_MAX_BYTES, ExecutionRouteClassifierOutputV1, ExecutionRoutingInput,
    route_execution,
};
use moa_core::{
    traits::LLMProvider,
    types::{
        completion::{
            CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, StopReason,
            TokenUsage,
        },
        execution_planning::{
            ActEscalationSignal, ExecutionMode, ExecutionPlanningEvidence,
            ExecutionRouteClassifierOutcome, ExecutionRouteDecision, ExecutionRouteReason,
        },
        identifiers::ModelId,
        model::ModelCapabilities,
    },
};
use moa_eval_core::{EvalError, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::report::ExecutionEvalAggregateMetricsV1;

/// Expected closed route label for one corpus case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRoutingLabelV1 {
    /// Direct model response without tools.
    Respond,
    /// Bounded interactive agent work.
    Act,
    /// Durable compiled execution run.
    Run,
    /// Clarification is required before routing.
    NeedsInput,
}

/// Scripted provider behavior used to exercise the production classifier path offline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionRoutingClassifierFixtureV1 {
    /// Return one strict typed classifier response with fixed token usage and cost.
    Response {
        /// Strict classifier payload serialized by the scripted provider.
        output: ExecutionRouteClassifierOutputV1,
        /// Normalized provider usage returned with the response.
        usage: TokenUsage,
        /// Provider-owning boundary cost applied after routing.
        cost_microusd: u64,
    },
    /// Fail before a response stream is available.
    ProviderError,
    /// Return a stream whose terminal collection fails.
    StreamError,
    /// Return syntactically valid JSON that fails the strict response shape.
    Malformed,
    /// Return text exceeding the production classifier byte cap.
    Oversized,
    /// Assert that a trusted route bypasses the provider entirely.
    NotCalled,
}

impl ExecutionRoutingClassifierFixtureV1 {
    const fn cost_microusd(&self) -> u64 {
        match self {
            Self::Response { cost_microusd, .. } => *cost_microusd,
            Self::ProviderError
            | Self::StreamError
            | Self::Malformed
            | Self::Oversized
            | Self::NotCalled => 0,
        }
    }
}

/// One strict labeled routing case.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingCaseV1 {
    /// Case schema version, fixed at `1`.
    pub schema_version: u8,
    /// Stable unique case identifier.
    pub case_id: String,
    /// Exact user objective supplied to the production router.
    pub objective: String,
    /// Number of attachments supplied with the turn.
    pub attachment_count: usize,
    /// Whether recent bounded session metadata identifies a concrete target.
    pub has_recent_target: bool,
    /// Deterministic scripted classifier behavior.
    pub classifier: ExecutionRoutingClassifierFixtureV1,
    /// Exact classifier or trusted-bypass outcome expected from production routing.
    pub expected_classifier_outcome: ExecutionRouteClassifierOutcome,
    /// Human-adjudicated expected route label.
    pub expected_label: ExecutionRoutingLabelV1,
    /// Human-adjudicated expected closed route reason.
    pub expected_reason: ExecutionRouteReason,
    /// Whether this is a deliberately ambiguous Act-boundary example.
    pub near_boundary: bool,
    /// Optional typed Act-to-Run escalation input.
    pub escalation: Option<ActEscalationSignal>,
    /// Exact escalation evidence expected to survive corpus transport.
    pub expected_escalation_evidence: Option<Vec<ExecutionPlanningEvidence>>,
    /// Stable corpus grouping labels.
    pub tags: Vec<String>,
}

/// One production-router result for a labeled case.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingCaseResultV1 {
    /// Stable corpus case identifier.
    pub case_id: String,
    /// Expected route label.
    pub expected_label: ExecutionRoutingLabelV1,
    /// Observed production route label.
    pub observed_label: ExecutionRoutingLabelV1,
    /// Expected closed route reason.
    pub expected_reason: ExecutionRouteReason,
    /// Observed closed route reason.
    pub observed_reason: ExecutionRouteReason,
    /// Observed classifier or trusted-bypass outcome.
    pub observed_classifier_outcome: ExecutionRouteClassifierOutcome,
    /// Number of scripted provider calls made by the production router.
    pub classifier_calls: u64,
    /// Whether label and reason both match.
    pub passed: bool,
    /// Asymmetric matrix cost for mode-to-mode errors.
    pub weighted_cost: Option<u64>,
    /// Whether typed escalation evidence survived exactly.
    pub escalation_evidence_preserved: Option<bool>,
}

/// Aggregate routing metrics with exact denominators.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingMetricsV1 {
    /// Number of corpus cases.
    pub total_cases: u64,
    /// Number whose expected label and reason matched.
    pub passed_cases: u64,
    /// Total asymmetric mode-classification cost.
    pub weighted_cost_total: u64,
    /// Mean asymmetric cost across mode-to-mode cases.
    pub weighted_cost_mean: f64,
    /// True Run cases incorrectly predicted Respond.
    pub respond_on_run_count: u64,
    /// Respond-on-Run count divided by true Run cases.
    pub respond_on_run_rate: f64,
    /// Correct near-boundary Act cases divided by all near-boundary Act cases.
    pub near_boundary_act_recall: f64,
    /// Correct escalation-to-Run cases divided by all escalation cases.
    pub escalation_recall: f64,
    /// Exact preserved escalation evidence divided by escalation cases.
    pub escalation_evidence_preservation_rate: f64,
    /// True NeedsInput cases incorrectly accepted into a concrete mode.
    pub needs_input_false_accept_rate: f64,
    /// True concrete modes unnecessarily predicted NeedsInput.
    pub unnecessary_clarification_rate: f64,
    /// Non-accepted classifier routes divided by classifier-attempted routes.
    pub classifier_fallback_rate: f64,
    /// Exact non-accepted classifier outcomes by closed outcome label.
    pub classifier_fallback_counts: std::collections::BTreeMap<String, u64>,
    /// Mean classifier tokens per classifier-attempted route.
    pub classifier_tokens_per_routed_turn: f64,
    /// Mean classifier cost per classifier-attempted route.
    pub classifier_cost_microusd_per_routed_turn: f64,
    /// Mean classifier duration in milliseconds per classifier-attempted route.
    pub classifier_latency_ms_per_routed_turn: f64,
    /// Ordered case-level results.
    pub cases: Vec<ExecutionRoutingCaseResultV1>,
}

impl ExecutionRoutingMetricsV1 {
    /// Copies routing metrics into the common execution report dashboard fields.
    pub fn apply_to_report_metrics(&self, metrics: &mut ExecutionEvalAggregateMetricsV1) {
        metrics.weighted_routing_cost = Some(self.weighted_cost_mean);
        metrics.respond_on_run_rate = Some(self.respond_on_run_rate);
        metrics.near_boundary_act_recall = Some(self.near_boundary_act_recall);
        metrics.escalation_recall = Some(self.escalation_recall);
        metrics.escalation_evidence_preservation_rate =
            Some(self.escalation_evidence_preservation_rate);
        metrics.needs_input_false_accept_rate = Some(self.needs_input_false_accept_rate);
        metrics.unnecessary_clarification_rate = Some(self.unnecessary_clarification_rate);
        metrics.classifier_fallback_rate = Some(self.classifier_fallback_rate);
        metrics.classifier_fallback_counts = Some(self.classifier_fallback_counts.clone());
        metrics.classifier_tokens_per_routed_turn = Some(self.classifier_tokens_per_routed_turn);
        metrics.classifier_cost_microusd_per_routed_turn =
            Some(self.classifier_cost_microusd_per_routed_turn);
        metrics.classifier_latency_ms_per_routed_turn =
            Some(self.classifier_latency_ms_per_routed_turn);
    }
}

struct ScriptedRoutingProvider {
    fixture: ExecutionRoutingClassifierFixtureV1,
    calls: AtomicU64,
}

impl ScriptedRoutingProvider {
    fn new(fixture: ExecutionRoutingClassifierFixtureV1) -> Self {
        Self {
            fixture,
            calls: AtomicU64::new(0),
        }
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl LLMProvider for ScriptedRoutingProvider {
    fn name(&self) -> &str {
        "execution-routing-corpus"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match &self.fixture {
            ExecutionRoutingClassifierFixtureV1::Response { output, usage, .. } => {
                let text = serde_json::to_string(output).map_err(|error| {
                    moa_core::error::MoaError::SerializationError(error.to_string())
                })?;
                Ok(CompletionStream::from_response(scripted_response(
                    text, *usage,
                )))
            }
            ExecutionRoutingClassifierFixtureV1::ProviderError => {
                Err(moa_core::error::MoaError::ProviderError(
                    "scripted route provider failure".to_string(),
                ))
            }
            ExecutionRoutingClassifierFixtureV1::StreamError => {
                let (sender, receiver) = mpsc::channel(1);
                drop(sender);
                let completion = tokio::spawn(async {
                    Err(moa_core::error::MoaError::ProviderError(
                        "scripted route stream failure".to_string(),
                    ))
                });
                Ok(CompletionStream::new(receiver, completion))
            }
            ExecutionRoutingClassifierFixtureV1::Malformed => Ok(CompletionStream::from_response(
                scripted_response("{}".to_string(), TokenUsage::default()),
            )),
            ExecutionRoutingClassifierFixtureV1::Oversized => {
                Ok(CompletionStream::from_response(scripted_response(
                    "x".repeat(EXECUTION_ROUTER_RESPONSE_MAX_BYTES + 1),
                    TokenUsage::default(),
                )))
            }
            ExecutionRoutingClassifierFixtureV1::NotCalled => {
                Err(moa_core::error::MoaError::ProviderError(
                    "trusted route unexpectedly called the classifier".to_string(),
                ))
            }
        }
    }
}

fn scripted_response(text: String, usage: TokenUsage) -> CompletionResponse {
    CompletionResponse {
        content: vec![CompletionContent::Text(text.clone())],
        text,
        stop_reason: StopReason::EndTurn,
        model: ModelId::new("scripted-route-model"),
        usage,
        duration_ms: 1,
        thought_signature: None,
    }
}

/// Scores strict cases through the production async router and a scripted provider.
pub async fn score_routing_cases(
    cases: &[ExecutionRoutingCaseV1],
) -> Result<ExecutionRoutingMetricsV1> {
    let mut results = Vec::with_capacity(cases.len());
    let mut weighted_cost_total = 0_u64;
    let mut weighted_denominator = 0_u64;
    let mut true_run = 0_u64;
    let mut respond_on_run = 0_u64;
    let mut near_boundary_act = 0_u64;
    let mut near_boundary_act_correct = 0_u64;
    let mut escalation_cases = 0_u64;
    let mut escalation_correct = 0_u64;
    let mut escalation_evidence_preserved = 0_u64;
    let mut needs_input_cases = 0_u64;
    let mut needs_input_false_accepts = 0_u64;
    let mut concrete_mode_cases = 0_u64;
    let mut unnecessary_clarifications = 0_u64;
    let mut classifier_attempts = 0_u64;
    let mut classifier_fallbacks = 0_u64;
    let mut classifier_fallback_counts = BTreeMap::<String, u64>::new();
    let mut classifier_tokens = 0_u64;
    let mut classifier_cost_microusd = 0_u64;
    let mut classifier_duration_micros = 0_u64;

    for case in cases {
        validate_routing_case(case)?;
        let provider = ScriptedRoutingProvider::new(case.classifier.clone());
        let model = ModelId::new("scripted-route-model");
        let mut routed = route_execution(
            &provider,
            ExecutionRoutingInput {
                objective: &case.objective,
                execution_template: None,
                escalation: case.escalation.as_ref(),
                attachment_count: case.attachment_count,
                has_recent_target: case.has_recent_target,
                route_model: &model,
            },
        )
        .await
        .map_err(|error| invalid_config(format!("routing case `{}`: {error}", case.case_id)))?;
        routed.provenance.cost_microusd = case.classifier.cost_microusd();
        let classifier_calls = provider.call_count();
        let expected_calls = u64::from(
            routed.provenance.source
                == moa_core::types::execution_planning::ExecutionRouteSource::Classifier,
        );
        let (observed_label, observed_reason) = decision_label(&routed.decision);
        let observed_classifier_outcome = routed.provenance.classifier_outcome;
        if expected_calls == 1 {
            classifier_attempts = classifier_attempts.saturating_add(1);
            let usage = routed.provenance.usage;
            let route_tokens = usage
                .input_tokens_uncached
                .checked_add(usage.input_tokens_cache_write)
                .and_then(|value| value.checked_add(usage.input_tokens_cache_read))
                .and_then(|value| value.checked_add(usage.output_tokens))
                .ok_or_else(|| {
                    invalid_config("classifier token total overflowed u64".to_string())
                })?;
            classifier_tokens = classifier_tokens.checked_add(route_tokens).ok_or_else(|| {
                invalid_config("classifier token aggregate overflowed u64".to_string())
            })?;
            classifier_cost_microusd = classifier_cost_microusd
                .checked_add(routed.provenance.cost_microusd)
                .ok_or_else(|| {
                    invalid_config("classifier cost aggregate overflowed u64".to_string())
                })?;
            classifier_duration_micros = classifier_duration_micros
                .checked_add(routed.provenance.duration_micros)
                .ok_or_else(|| {
                    invalid_config("classifier latency aggregate overflowed u64".to_string())
                })?;
            if observed_classifier_outcome != ExecutionRouteClassifierOutcome::Accepted {
                classifier_fallbacks = classifier_fallbacks.saturating_add(1);
                *classifier_fallback_counts
                    .entry(classifier_outcome_label(observed_classifier_outcome).to_string())
                    .or_default() += 1;
            }
        }
        let weighted_cost = mode_cost(case.expected_label, observed_label);
        if let Some(cost) = weighted_cost {
            weighted_cost_total = weighted_cost_total.checked_add(cost).ok_or_else(|| {
                invalid_config("routing weighted cost overflowed u64".to_string())
            })?;
            weighted_denominator = weighted_denominator.saturating_add(1);
        }
        if case.expected_label == ExecutionRoutingLabelV1::Run {
            true_run = true_run.saturating_add(1);
            if observed_label == ExecutionRoutingLabelV1::Respond {
                respond_on_run = respond_on_run.saturating_add(1);
            }
        }
        if case.near_boundary {
            near_boundary_act = near_boundary_act.saturating_add(1);
            if observed_label == ExecutionRoutingLabelV1::Act {
                near_boundary_act_correct = near_boundary_act_correct.saturating_add(1);
            }
        }
        let evidence_preserved = case.escalation.as_ref().map(|escalation| {
            case.expected_escalation_evidence.as_ref() == Some(&escalation.evidence)
        });
        if case.escalation.is_some() {
            escalation_cases = escalation_cases.saturating_add(1);
            if observed_label == ExecutionRoutingLabelV1::Run
                && observed_reason == ExecutionRouteReason::ActEscalation
            {
                escalation_correct = escalation_correct.saturating_add(1);
            }
            if evidence_preserved == Some(true) {
                escalation_evidence_preserved = escalation_evidence_preserved.saturating_add(1);
            }
        }
        if case.expected_label == ExecutionRoutingLabelV1::NeedsInput {
            needs_input_cases = needs_input_cases.saturating_add(1);
            if observed_label != ExecutionRoutingLabelV1::NeedsInput {
                needs_input_false_accepts = needs_input_false_accepts.saturating_add(1);
            }
        } else {
            concrete_mode_cases = concrete_mode_cases.saturating_add(1);
            if observed_label == ExecutionRoutingLabelV1::NeedsInput {
                unnecessary_clarifications = unnecessary_clarifications.saturating_add(1);
            }
        }
        results.push(ExecutionRoutingCaseResultV1 {
            case_id: case.case_id.clone(),
            expected_label: case.expected_label,
            observed_label,
            expected_reason: case.expected_reason,
            observed_reason,
            observed_classifier_outcome,
            classifier_calls,
            passed: observed_label == case.expected_label
                && observed_reason == case.expected_reason
                && observed_classifier_outcome == case.expected_classifier_outcome
                && classifier_calls == expected_calls
                && evidence_preserved != Some(false),
            weighted_cost,
            escalation_evidence_preserved: evidence_preserved,
        });
    }

    let total_cases = usize_to_u64(cases.len(), "routing case count")?;
    let passed_cases = usize_to_u64(
        results.iter().filter(|result| result.passed).count(),
        "passing routing case count",
    )?;
    Ok(ExecutionRoutingMetricsV1 {
        total_cases,
        passed_cases,
        weighted_cost_total,
        weighted_cost_mean: ratio(weighted_cost_total, weighted_denominator),
        respond_on_run_count: respond_on_run,
        respond_on_run_rate: ratio(respond_on_run, true_run),
        near_boundary_act_recall: ratio(near_boundary_act_correct, near_boundary_act),
        escalation_recall: ratio(escalation_correct, escalation_cases),
        escalation_evidence_preservation_rate: ratio(
            escalation_evidence_preserved,
            escalation_cases,
        ),
        needs_input_false_accept_rate: ratio(needs_input_false_accepts, needs_input_cases),
        unnecessary_clarification_rate: ratio(unnecessary_clarifications, concrete_mode_cases),
        classifier_fallback_rate: ratio(classifier_fallbacks, classifier_attempts),
        classifier_fallback_counts,
        classifier_tokens_per_routed_turn: ratio(classifier_tokens, classifier_attempts),
        classifier_cost_microusd_per_routed_turn: ratio(
            classifier_cost_microusd,
            classifier_attempts,
        ),
        classifier_latency_ms_per_routed_turn: ratio(
            classifier_duration_micros,
            classifier_attempts,
        ) / 1_000.0,
        cases: results,
    })
}

/// Validates one routing case independently of corpus-level counts.
pub(crate) fn validate_routing_case(case: &ExecutionRoutingCaseV1) -> Result<()> {
    if case.schema_version != 1
        || case.case_id.trim().is_empty()
        || case.objective.trim().is_empty()
    {
        return Err(invalid_config(format!(
            "routing case `{}` has an invalid version, ID, or objective",
            case.case_id
        )));
    }
    if case.near_boundary && case.expected_label != ExecutionRoutingLabelV1::Act {
        return Err(invalid_config(format!(
            "routing case `{}` marks a non-Act case as near-boundary",
            case.case_id
        )));
    }
    let fixed_fixture_outcome = match &case.classifier {
        ExecutionRoutingClassifierFixtureV1::ProviderError => {
            Some(ExecutionRouteClassifierOutcome::ProviderError)
        }
        ExecutionRoutingClassifierFixtureV1::StreamError => {
            Some(ExecutionRouteClassifierOutcome::StreamError)
        }
        ExecutionRoutingClassifierFixtureV1::Malformed => {
            Some(ExecutionRouteClassifierOutcome::SchemaRejected)
        }
        ExecutionRoutingClassifierFixtureV1::Oversized => {
            Some(ExecutionRouteClassifierOutcome::Oversized)
        }
        ExecutionRoutingClassifierFixtureV1::NotCalled => {
            Some(ExecutionRouteClassifierOutcome::NotCalled)
        }
        ExecutionRoutingClassifierFixtureV1::Response { .. } => None,
    };
    if fixed_fixture_outcome.is_some_and(|outcome| outcome != case.expected_classifier_outcome) {
        return Err(invalid_config(format!(
            "routing case `{}` classifier fixture and expected outcome disagree",
            case.case_id
        )));
    }
    let reason_matches = match case.expected_label {
        ExecutionRoutingLabelV1::Respond => {
            case.expected_reason == ExecutionRouteReason::SimpleResponse
        }
        ExecutionRoutingLabelV1::Act => {
            case.expected_reason == ExecutionRouteReason::BoundedInteractiveWork
        }
        ExecutionRoutingLabelV1::Run => matches!(
            case.expected_reason,
            ExecutionRouteReason::ExplicitRun
                | ExecutionRouteReason::BulkCollection
                | ExecutionRouteReason::DurableOrResumable
                | ExecutionRouteReason::HighFanout
                | ExecutionRouteReason::ApprovalOrSignal
                | ExecutionRouteReason::SelectedExecutionTemplate
                | ExecutionRouteReason::ActEscalation
        ),
        ExecutionRoutingLabelV1::NeedsInput => {
            case.expected_reason == ExecutionRouteReason::PreflightInputMissing
        }
    };
    if !reason_matches {
        return Err(invalid_config(format!(
            "routing case `{}` has a reason inconsistent with its label",
            case.case_id
        )));
    }
    match (
        case.escalation.as_ref(),
        case.expected_escalation_evidence.as_ref(),
    ) {
        (Some(escalation), Some(expected))
            if case.expected_label == ExecutionRoutingLabelV1::Run
                && case.expected_reason == ExecutionRouteReason::ActEscalation
                && escalation.objective.as_bytes() == case.objective.as_bytes()
                && escalation.evidence == *expected
                && matches!(
                    &case.classifier,
                    ExecutionRoutingClassifierFixtureV1::NotCalled
                )
                && case.expected_classifier_outcome
                    == ExecutionRouteClassifierOutcome::NotCalled =>
        {
            escalation
                .validate()
                .map_err(|error| invalid_config(error.to_string()))?;
        }
        (None, None)
            if !matches!(
                &case.classifier,
                ExecutionRoutingClassifierFixtureV1::NotCalled
            ) => {}
        _ => {
            return Err(invalid_config(format!(
                "routing case `{}` has inconsistent escalation evidence",
                case.case_id
            )));
        }
    }
    Ok(())
}

fn decision_label(
    decision: &ExecutionRouteDecision,
) -> (ExecutionRoutingLabelV1, ExecutionRouteReason) {
    match *decision {
        ExecutionRouteDecision::NeedsInput { reason, .. } => {
            (ExecutionRoutingLabelV1::NeedsInput, reason)
        }
        ExecutionRouteDecision::Routed { mode, reason } => {
            let label = match mode {
                ExecutionMode::Respond => ExecutionRoutingLabelV1::Respond,
                ExecutionMode::Act => ExecutionRoutingLabelV1::Act,
                ExecutionMode::Run => ExecutionRoutingLabelV1::Run,
            };
            (label, reason)
        }
    }
}

const fn classifier_outcome_label(outcome: ExecutionRouteClassifierOutcome) -> &'static str {
    match outcome {
        ExecutionRouteClassifierOutcome::NotCalled => "not_called",
        ExecutionRouteClassifierOutcome::Accepted => "accepted",
        ExecutionRouteClassifierOutcome::ProviderError => "provider_error",
        ExecutionRouteClassifierOutcome::StreamError => "stream_error",
        ExecutionRouteClassifierOutcome::Oversized => "oversized",
        ExecutionRouteClassifierOutcome::SchemaRejected => "schema_rejected",
        ExecutionRouteClassifierOutcome::InvalidDecision => "invalid_decision",
        ExecutionRouteClassifierOutcome::LowConfidence => "low_confidence",
        ExecutionRouteClassifierOutcome::ContextForcedAct => "context_forced_act",
    }
}

fn mode_cost(expected: ExecutionRoutingLabelV1, observed: ExecutionRoutingLabelV1) -> Option<u64> {
    use ExecutionRoutingLabelV1::{Act, Respond, Run};
    Some(match (observed, expected) {
        (Respond, Respond) | (Act, Act) | (Run, Run) => 0,
        (Respond, Act) => 3,
        (Respond, Run) => 50,
        (Act, Respond) => 1,
        (Act, Run) => 8,
        (Run, Respond) => 6,
        (Run, Act) => 4,
        _ => return None,
    })
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn usize_to_u64(value: usize, context: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| invalid_config(format!("{context} exceeds u64")))
}

fn invalid_config(message: String) -> EvalError {
    EvalError::InvalidConfig(message)
}

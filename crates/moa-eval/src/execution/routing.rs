//! Deterministic scripted-provider scoring for the production execution router.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use moa_brain::execution_planning::{
    EXECUTION_ROUTER_RESPONSE_MAX_BYTES, ExecutionRouteClassifierOutput, ExecutionRoutingInput,
    route_execution,
};
use moa_core::{
    traits::LLMProvider,
    types::{
        completion::{
            CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
            SharedCompletionRequest, StopReason, TokenUsage,
        },
        execution_planning::{
            DurableUpgradeSignal, ExecutionPlanningEvidence, ExecutionRouteClassifierOutcome,
            ExecutionRouteDecision, ExecutionRouteSource, ExecutionStrategy,
            durable_upgrade_transition,
        },
        identifiers::ModelId,
        model::ModelCapabilities,
    },
};
use moa_eval_core::{Error, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::report::ExecutionEvalAggregateMetrics;

/// Expected closed route label for one corpus case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRoutingLabel {
    /// Direct model response without tools.
    Respond,
    /// Authorized work using a deterministic internal strategy.
    Execute,
    /// Clarification is required before routing.
    NeedsInput,
}

/// Scripted provider behavior used to exercise the production classifier path offline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionRoutingClassifierFixture {
    /// Return one strict typed classifier response with fixed token usage and cost.
    Response {
        /// Strict classifier payload serialized by the scripted provider.
        output: ExecutionRouteClassifierOutput,
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

impl ExecutionRoutingClassifierFixture {
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
pub struct ExecutionRoutingCase {
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
    /// Installed-skill names offered to the router as a coverage hint. Empty for
    /// cases that do not exercise skill coverage; omitted from serialized JSONL
    /// when empty so only skill-coverage cases carry the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_skills: Vec<String>,
    /// Deterministic scripted classifier behavior.
    pub classifier: ExecutionRoutingClassifierFixture,
    /// Exact classifier or trusted-bypass outcome expected from production routing.
    pub expected_classifier_outcome: ExecutionRouteClassifierOutcome,
    /// Human-adjudicated expected route label.
    pub expected_label: ExecutionRoutingLabel,
    /// Human-adjudicated strategy, present only for Execute.
    pub expected_strategy: Option<ExecutionStrategy>,
    /// Whether this is a deliberately ambiguous Execute/Inline boundary example.
    pub near_boundary: bool,
    /// Optional typed Inline-to-Durable upgrade input.
    pub durable_upgrade: Option<DurableUpgradeSignal>,
    /// Exact Durable-upgrade evidence expected after the production transition handoff.
    pub expected_durable_upgrade_evidence: Option<Vec<ExecutionPlanningEvidence>>,
    /// Stable corpus grouping labels.
    pub tags: Vec<String>,
}

/// One production-router result for a labeled case.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingCaseResult {
    /// Stable corpus case identifier.
    pub case_id: String,
    /// Expected route label.
    pub expected_label: ExecutionRoutingLabel,
    /// Observed production route label.
    pub observed_label: ExecutionRoutingLabel,
    /// Expected deterministic strategy, present only for Execute.
    pub expected_strategy: Option<ExecutionStrategy>,
    /// Observed deterministic strategy, present only for Execute.
    pub observed_strategy: Option<ExecutionStrategy>,
    /// Observed classifier or trusted-bypass outcome.
    pub observed_classifier_outcome: ExecutionRouteClassifierOutcome,
    /// Number of scripted provider calls made by the production router.
    pub classifier_calls: u64,
    /// Whether route, strategy, provenance, and upgrade evidence all match.
    pub passed: bool,
    /// Asymmetric cost for the public route-kind prediction.
    pub routing_cost: u64,
    /// Asymmetric cost for Inline versus Durable when both routes are Execute.
    pub strategy_cost: Option<u64>,
    /// Whether typed Durable-upgrade evidence survived exactly.
    pub durable_upgrade_evidence_preserved: Option<bool>,
}

/// Aggregate routing metrics with exact denominators.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRoutingMetrics {
    /// Number of corpus cases.
    pub total_cases: u64,
    /// Number whose expected route and strategy matched.
    pub passed_cases: u64,
    /// Total asymmetric public route cost.
    pub weighted_routing_cost_total: u64,
    /// Mean asymmetric public route cost across all cases.
    pub weighted_routing_cost_mean: f64,
    /// Total asymmetric Inline/Durable strategy cost.
    pub weighted_strategy_cost_total: u64,
    /// Mean asymmetric strategy cost across cases with expected and observed Execute routes.
    pub weighted_strategy_cost_mean: f64,
    /// True Execute cases incorrectly predicted Respond.
    pub respond_on_execute_count: u64,
    /// Respond-on-Execute count divided by true Execute cases.
    pub respond_on_execute_rate: f64,
    /// Correct near-boundary Execute/Inline cases divided by all near-boundary cases.
    pub near_boundary_inline_recall: f64,
    /// Correct Durable strategies divided by all expected Durable strategies.
    pub durable_strategy_recall: f64,
    /// Correct Durable-upgrade transitions divided by all upgrade cases.
    pub durable_upgrade_recall: f64,
    /// Exact preserved Durable-upgrade evidence divided by all upgrade cases.
    pub durable_upgrade_evidence_preservation_rate: f64,
    /// True NeedsInput cases incorrectly accepted into a concrete route.
    pub needs_input_false_accept_rate: f64,
    /// True concrete routes unnecessarily predicted NeedsInput.
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
    pub cases: Vec<ExecutionRoutingCaseResult>,
}

impl ExecutionRoutingMetrics {
    /// Copies routing metrics into the common execution report dashboard fields.
    pub fn apply_to_report_metrics(&self, metrics: &mut ExecutionEvalAggregateMetrics) {
        metrics.weighted_routing_cost = Some(self.weighted_routing_cost_mean);
        metrics.weighted_strategy_cost = Some(self.weighted_strategy_cost_mean);
        metrics.respond_on_execute_rate = Some(self.respond_on_execute_rate);
        metrics.near_boundary_inline_recall = Some(self.near_boundary_inline_recall);
        metrics.durable_strategy_recall = Some(self.durable_strategy_recall);
        metrics.durable_upgrade_recall = Some(self.durable_upgrade_recall);
        metrics.durable_upgrade_evidence_preservation_rate =
            Some(self.durable_upgrade_evidence_preservation_rate);
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
    fixture: ExecutionRoutingClassifierFixture,
    calls: AtomicU64,
}

impl ScriptedRoutingProvider {
    fn new(fixture: ExecutionRoutingClassifierFixture) -> Self {
        Self {
            fixture,
            calls: AtomicU64::new(0),
        }
    }

    fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn complete_fixture(&self) -> moa_core::error::Result<CompletionStream> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match &self.fixture {
            ExecutionRoutingClassifierFixture::Response { output, usage, .. } => {
                let text = serde_json::to_string(output).map_err(|error| {
                    moa_core::error::MoaError::SerializationError(error.to_string())
                })?;
                Ok(CompletionStream::from_response(scripted_response(
                    text, *usage,
                )))
            }
            ExecutionRoutingClassifierFixture::ProviderError => {
                Err(moa_core::error::MoaError::ProviderError(
                    "scripted route provider failure".to_string(),
                ))
            }
            ExecutionRoutingClassifierFixture::StreamError => {
                let (sender, receiver) = mpsc::channel(1);
                drop(sender);
                let completion = tokio::spawn(async {
                    Err(moa_core::error::MoaError::ProviderError(
                        "scripted route stream failure".to_string(),
                    ))
                });
                Ok(CompletionStream::new(receiver, completion))
            }
            ExecutionRoutingClassifierFixture::Malformed => Ok(CompletionStream::from_response(
                scripted_response("{}".to_string(), TokenUsage::default()),
            )),
            ExecutionRoutingClassifierFixture::Oversized => {
                Ok(CompletionStream::from_response(scripted_response(
                    "x".repeat(EXECUTION_ROUTER_RESPONSE_MAX_BYTES + 1),
                    TokenUsage::default(),
                )))
            }
            ExecutionRoutingClassifierFixture::NotCalled => {
                Err(moa_core::error::MoaError::ProviderError(
                    "trusted route unexpectedly called the classifier".to_string(),
                ))
            }
        }
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
        self.complete_fixture()
    }

    async fn complete_shared(
        &self,
        _request: SharedCompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        self.complete_fixture()
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
    cases: &[ExecutionRoutingCase],
) -> Result<ExecutionRoutingMetrics> {
    let mut results = Vec::with_capacity(cases.len());
    let mut weighted_routing_cost_total = 0_u64;
    let mut weighted_strategy_cost_total = 0_u64;
    let mut strategy_cost_cases = 0_u64;
    let mut execute_cases = 0_u64;
    let mut respond_on_execute = 0_u64;
    let mut durable_strategy_cases = 0_u64;
    let mut durable_strategy_correct = 0_u64;
    let mut near_boundary_inline = 0_u64;
    let mut near_boundary_inline_correct = 0_u64;
    let mut durable_upgrade_cases = 0_u64;
    let mut durable_upgrade_correct = 0_u64;
    let mut durable_upgrade_evidence_preserved = 0_u64;
    let mut needs_input_cases = 0_u64;
    let mut needs_input_false_accepts = 0_u64;
    let mut concrete_route_cases = 0_u64;
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
        let (mut routed, admitted_upgrade_evidence) = if let Some(upgrade) =
            case.durable_upgrade.as_ref()
        {
            let admitted = durable_upgrade_transition(
                &case.objective,
                &ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    rationale: "This request can begin in a bounded interactive loop.".to_string(),
                },
                true,
                false,
                upgrade.clone(),
            )
            .map_err(|error| {
                invalid_config(format!(
                    "routing case `{}` has an invalid Durable-upgrade transition: {error}",
                    case.case_id
                ))
            })?;
            (admitted.routing, Some(admitted.signal.evidence))
        } else {
            // The case's boolean recent-target dimension maps to a representative digest;
            // the digest no longer gates routing (only attachments do), so this exercises
            // the prompt path without changing corpus outcomes.
            let recent_target_digest = if case.has_recent_target {
                "user: continue the earlier request\ntool bash: {\"cmd\":\"cargo test\"}"
            } else {
                ""
            };
            let routed = route_execution(
                &provider,
                ExecutionRoutingInput {
                    objective: &case.objective,
                    execution_template: None,
                    attachment_count: case.attachment_count,
                    recent_target_digest,
                    available_skill_names: &case.available_skills,
                    classifier_model: &model,
                },
            )
            .await
            .map_err(|error| invalid_config(format!("routing case `{}`: {error}", case.case_id)))?;
            (routed, None)
        };
        routed.provenance.cost_microusd = case.classifier.cost_microusd();
        let classifier_calls = provider.call_count();
        let expected_calls = u64::from(
            routed.provenance.source
                == moa_core::types::execution_planning::ExecutionRouteSource::Classifier,
        );
        let (observed_label, observed_strategy) = decision_route(&routed.decision)?;
        let observed_classifier_outcome = routed.provenance.classifier_outcome;
        if expected_calls == 1 {
            classifier_attempts = classifier_attempts.saturating_add(1);
            let route_tokens =
                super::route_token_total(routed.provenance.usage).ok_or_else(|| {
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
        let routing_cost = routing_cost(case.expected_label, observed_label);
        weighted_routing_cost_total = weighted_routing_cost_total
            .checked_add(routing_cost)
            .ok_or_else(|| invalid_config("routing weighted cost overflowed u64".to_string()))?;
        let strategy_cost = strategy_cost(
            case.expected_label,
            case.expected_strategy,
            observed_label,
            observed_strategy,
        );
        if let Some(cost) = strategy_cost {
            weighted_strategy_cost_total = weighted_strategy_cost_total
                .checked_add(cost)
                .ok_or_else(|| {
                    invalid_config("strategy weighted cost overflowed u64".to_string())
                })?;
            strategy_cost_cases = strategy_cost_cases.saturating_add(1);
        }
        if case.expected_label == ExecutionRoutingLabel::Execute {
            execute_cases = execute_cases.saturating_add(1);
            if observed_label == ExecutionRoutingLabel::Respond {
                respond_on_execute = respond_on_execute.saturating_add(1);
            }
            if case.expected_strategy == Some(ExecutionStrategy::Durable) {
                durable_strategy_cases = durable_strategy_cases.saturating_add(1);
                if observed_label == ExecutionRoutingLabel::Execute
                    && observed_strategy == Some(ExecutionStrategy::Durable)
                {
                    durable_strategy_correct = durable_strategy_correct.saturating_add(1);
                }
            }
        }
        if case.near_boundary {
            near_boundary_inline = near_boundary_inline.saturating_add(1);
            if observed_label == ExecutionRoutingLabel::Execute
                && observed_strategy == Some(ExecutionStrategy::Inline)
            {
                near_boundary_inline_correct = near_boundary_inline_correct.saturating_add(1);
            }
        }
        let evidence_preserved = admitted_upgrade_evidence
            .as_ref()
            .zip(case.expected_durable_upgrade_evidence.as_ref())
            .map(|(observed, expected)| observed == expected);
        if case.durable_upgrade.is_some() {
            durable_upgrade_cases = durable_upgrade_cases.saturating_add(1);
            if observed_label == ExecutionRoutingLabel::Execute
                && observed_strategy == Some(ExecutionStrategy::Durable)
                && routed.provenance.source == ExecutionRouteSource::DurableUpgrade
                && classifier_calls == 0
            {
                durable_upgrade_correct = durable_upgrade_correct.saturating_add(1);
            }
            if evidence_preserved == Some(true) {
                durable_upgrade_evidence_preserved =
                    durable_upgrade_evidence_preserved.saturating_add(1);
            }
        }
        if case.expected_label == ExecutionRoutingLabel::NeedsInput {
            needs_input_cases = needs_input_cases.saturating_add(1);
            if observed_label != ExecutionRoutingLabel::NeedsInput {
                needs_input_false_accepts = needs_input_false_accepts.saturating_add(1);
            }
        } else {
            concrete_route_cases = concrete_route_cases.saturating_add(1);
            if observed_label == ExecutionRoutingLabel::NeedsInput {
                unnecessary_clarifications = unnecessary_clarifications.saturating_add(1);
            }
        }
        results.push(ExecutionRoutingCaseResult {
            case_id: case.case_id.clone(),
            expected_label: case.expected_label,
            observed_label,
            expected_strategy: case.expected_strategy,
            observed_strategy,
            observed_classifier_outcome,
            classifier_calls,
            passed: observed_label == case.expected_label
                && observed_strategy == case.expected_strategy
                && observed_classifier_outcome == case.expected_classifier_outcome
                && classifier_calls == expected_calls
                && evidence_preserved != Some(false),
            routing_cost,
            strategy_cost,
            durable_upgrade_evidence_preserved: evidence_preserved,
        });
    }

    let total_cases = usize_to_u64(cases.len(), "routing case count")?;
    let passed_cases = usize_to_u64(
        results.iter().filter(|result| result.passed).count(),
        "passing routing case count",
    )?;
    Ok(ExecutionRoutingMetrics {
        total_cases,
        passed_cases,
        weighted_routing_cost_total,
        weighted_routing_cost_mean: ratio(weighted_routing_cost_total, total_cases),
        weighted_strategy_cost_total,
        weighted_strategy_cost_mean: ratio(weighted_strategy_cost_total, strategy_cost_cases),
        respond_on_execute_count: respond_on_execute,
        respond_on_execute_rate: ratio(respond_on_execute, execute_cases),
        near_boundary_inline_recall: ratio(near_boundary_inline_correct, near_boundary_inline),
        durable_strategy_recall: ratio(durable_strategy_correct, durable_strategy_cases),
        durable_upgrade_recall: ratio(durable_upgrade_correct, durable_upgrade_cases),
        durable_upgrade_evidence_preservation_rate: ratio(
            durable_upgrade_evidence_preserved,
            durable_upgrade_cases,
        ),
        needs_input_false_accept_rate: ratio(needs_input_false_accepts, needs_input_cases),
        unnecessary_clarification_rate: ratio(unnecessary_clarifications, concrete_route_cases),
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
pub(crate) fn validate_routing_case(case: &ExecutionRoutingCase) -> Result<()> {
    if case.schema_version != 1
        || case.case_id.trim().is_empty()
        || case.objective.trim().is_empty()
    {
        return Err(invalid_config(format!(
            "routing case `{}` has an invalid version, ID, or objective",
            case.case_id
        )));
    }
    if case.near_boundary
        && (case.expected_label != ExecutionRoutingLabel::Execute
            || case.expected_strategy != Some(ExecutionStrategy::Inline))
    {
        return Err(invalid_config(format!(
            "routing case `{}` marks a non-Inline Execute case as near-boundary",
            case.case_id
        )));
    }
    let fixed_fixture_outcome = match &case.classifier {
        ExecutionRoutingClassifierFixture::ProviderError => {
            Some(ExecutionRouteClassifierOutcome::ProviderError)
        }
        ExecutionRoutingClassifierFixture::StreamError => {
            Some(ExecutionRouteClassifierOutcome::StreamError)
        }
        ExecutionRoutingClassifierFixture::Malformed => {
            Some(ExecutionRouteClassifierOutcome::SchemaRejected)
        }
        ExecutionRoutingClassifierFixture::Oversized => {
            Some(ExecutionRouteClassifierOutcome::Oversized)
        }
        ExecutionRoutingClassifierFixture::NotCalled => {
            Some(ExecutionRouteClassifierOutcome::NotCalled)
        }
        ExecutionRoutingClassifierFixture::Response { .. } => None,
    };
    if fixed_fixture_outcome.is_some_and(|outcome| outcome != case.expected_classifier_outcome) {
        return Err(invalid_config(format!(
            "routing case `{}` classifier fixture and expected outcome disagree",
            case.case_id
        )));
    }
    let strategy_matches = matches!(
        (case.expected_label, case.expected_strategy),
        (
            ExecutionRoutingLabel::Respond | ExecutionRoutingLabel::NeedsInput,
            None
        ) | (ExecutionRoutingLabel::Execute, Some(_))
    );
    if !strategy_matches {
        return Err(invalid_config(format!(
            "routing case `{}` has a strategy inconsistent with its label",
            case.case_id
        )));
    }
    match (
        case.durable_upgrade.as_ref(),
        case.expected_durable_upgrade_evidence.as_ref(),
    ) {
        (Some(upgrade), Some(_))
            if case.expected_label == ExecutionRoutingLabel::Execute
                && case.expected_strategy == Some(ExecutionStrategy::Durable)
                && upgrade.objective.as_bytes() == case.objective.as_bytes()
                && matches!(
                    &case.classifier,
                    ExecutionRoutingClassifierFixture::NotCalled
                )
                && case.expected_classifier_outcome
                    == ExecutionRouteClassifierOutcome::NotCalled =>
        {
            upgrade
                .validate()
                .map_err(|error| invalid_config(error.to_string()))?;
        }
        (None, None)
            if !matches!(
                &case.classifier,
                ExecutionRoutingClassifierFixture::NotCalled
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

fn decision_route(
    decision: &ExecutionRouteDecision,
) -> Result<(ExecutionRoutingLabel, Option<ExecutionStrategy>)> {
    let label = match decision.kind() {
        moa_core::types::execution_planning::ExecutionRouteKind::Respond => {
            ExecutionRoutingLabel::Respond
        }
        moa_core::types::execution_planning::ExecutionRouteKind::Execute => {
            ExecutionRoutingLabel::Execute
        }
        moa_core::types::execution_planning::ExecutionRouteKind::NeedsInput => {
            ExecutionRoutingLabel::NeedsInput
        }
    };
    let strategy = decision.strategy();
    if !matches!(
        (label, strategy),
        (
            ExecutionRoutingLabel::Respond | ExecutionRoutingLabel::NeedsInput,
            None
        ) | (
            ExecutionRoutingLabel::Execute,
            Some(ExecutionStrategy::Inline | ExecutionStrategy::Durable)
        )
    ) {
        return Err(invalid_config(
            "route kind and deterministic strategy are inconsistent".to_string(),
        ));
    }
    Ok((label, strategy))
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
        ExecutionRouteClassifierOutcome::ContextForcedInline => "context_forced_inline",
    }
}

pub(crate) const fn routing_cost(
    expected: ExecutionRoutingLabel,
    observed: ExecutionRoutingLabel,
) -> u64 {
    use ExecutionRoutingLabel::{Execute, NeedsInput, Respond};
    match (observed, expected) {
        (Respond, Respond) | (Execute, Execute) | (NeedsInput, NeedsInput) => 0,
        (Respond, Execute) => 50,
        (Execute, Respond) => 1,
        (Respond | Execute, NeedsInput) => 2,
        (NeedsInput, Respond | Execute) => 3,
    }
}

pub(crate) const fn strategy_cost(
    expected_route: ExecutionRoutingLabel,
    expected_strategy: Option<ExecutionStrategy>,
    observed_route: ExecutionRoutingLabel,
    observed_strategy: Option<ExecutionStrategy>,
) -> Option<u64> {
    use ExecutionRoutingLabel::Execute;
    use ExecutionStrategy::{Durable, Inline};
    match (
        expected_route,
        expected_strategy,
        observed_route,
        observed_strategy,
    ) {
        (Execute, Some(Inline), Execute, Some(Inline))
        | (Execute, Some(Durable), Execute, Some(Durable)) => Some(0),
        (Execute, Some(Inline), Execute, Some(Durable)) => Some(4),
        (Execute, Some(Durable), Execute, Some(Inline)) => Some(8),
        _ => None,
    }
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

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_route_decisions_expose_route_kind_and_deterministic_strategy() {
        // Pins: routing evals score the typed strategy without interpreting rationale prose.
        let rationale = "A specialist workflow outside the old taxonomy fits this strategy.";
        let cases = [
            (
                ExecutionRouteDecision::Respond {
                    rationale: rationale.to_string(),
                },
                ExecutionRoutingLabel::Respond,
                None,
            ),
            (
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    rationale: rationale.to_string(),
                },
                ExecutionRoutingLabel::Execute,
                Some(ExecutionStrategy::Inline),
            ),
            (
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Durable,
                    rationale: rationale.to_string(),
                },
                ExecutionRoutingLabel::Execute,
                Some(ExecutionStrategy::Durable),
            ),
            (
                ExecutionRouteDecision::NeedsInput {
                    rationale: rationale.to_string(),
                    missing_inputs: vec!["objective".to_string()],
                },
                ExecutionRoutingLabel::NeedsInput,
                None,
            ),
        ];
        for (decision, expected_label, expected_strategy) in cases {
            let (observed_label, observed_strategy) = decision_route(&decision)
                .expect("a valid direct route should have a deterministic strategy");
            assert_eq!(observed_label, expected_label);
            assert_eq!(observed_strategy, expected_strategy);
            assert_eq!(decision.rationale(), rationale);
        }
    }
}

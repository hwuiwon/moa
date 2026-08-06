//! Bounded model-assisted selection of respond, execute, or clarification.

use std::{collections::HashMap, time::Instant};

use moa_core::{
    error::{MoaError, Result},
    traits::LLMProvider,
    types::{
        completion::{CompletionRequest, JsonResponseFormat, NativeWebSearchPolicy, TokenUsage},
        context::ContextMessage,
        execution_planning::{
            ExecutionRouteClassifierOutcome, ExecutionRouteDecision, ExecutionRouteProvenance,
            ExecutionRouteSource, ExecutionRouteUsage, ExecutionRoutingResult, ExecutionStrategy,
            ExecutionTemplateInvocation, execution_planning_hash,
            execution_route_rationale_is_valid,
        },
        identifiers::ModelId,
    },
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Stable execution-route classifier prompt identifier.
pub const EXECUTION_ROUTER_PROMPT_VERSION: &str = "execution-router-v2";
/// Fixed maximum classifier output tokens.
pub const EXECUTION_ROUTER_MAX_OUTPUT_TOKENS: usize = 256;
/// Maximum collected classifier response bytes.
pub const EXECUTION_ROUTER_RESPONSE_MAX_BYTES: usize = 16_384;
/// Confidence required before a direct response or clarification is accepted.
pub const EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS: u16 = 9_000;
/// Confidence required before the Durable strategy is selected.
pub const EXECUTION_ROUTER_DURABLE_CONFIDENCE_BPS: u16 = 8_000;

const EXECUTION_ROUTER_MAX_MISSING_INPUTS: usize = 8;
const EXECUTION_ROUTER_MAX_MISSING_INPUT_BYTES: usize = 256;
/// Maximum installed-skill names serialized into the classifier prompt as a
/// coverage hint. Bounds the per-turn user message regardless of tenant size.
pub const EXECUTION_ROUTER_MAX_SKILL_NAMES: usize = 24;
const EXECUTION_ROUTER_PROMPT: &str = include_str!("../prompts/execution_router.txt");
const ROUTER_STAGE_METADATA_KEY: &str = "moa.pipeline.stage";
const OPENAI_REASONING_EFFORT_METADATA_KEY: &str = "_moa.openai.reasoning_effort";

/// Immutable inputs used by the execution router.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionRoutingInput<'a> {
    /// Exact current user objective.
    pub objective: &'a str,
    /// Exact explicit template invocation, when supplied by a trusted caller surface.
    pub execution_template: Option<&'a ExecutionTemplateInvocation>,
    /// Number of attachments supplied on the current user turn.
    pub attachment_count: usize,
    /// Compact, possibly empty digest of recent target-bearing conversation
    /// context. It is passed to the classifier prompt so the model — not a
    /// keyword heuristic — can resolve terse follow-ups against a concrete
    /// recent referent. It never gates routing on its own.
    pub recent_target_digest: &'a str,
    /// Names of the skills installed for the tenant, in a stable order and
    /// possibly empty. Serialized into the classifier prompt as a coverage
    /// hint so the model — not a keyword heuristic — can prefer execution when
    /// an installed skill plausibly covers the request. It never gates routing
    /// on its own, is bounded to [`EXECUTION_ROUTER_MAX_SKILL_NAMES`] before
    /// serialization, and is treated by the prompt as untrusted data.
    pub available_skill_names: &'a [String],
    /// Configured auxiliary model used for ordinary route classification.
    pub classifier_model: &'a ModelId,
}

/// Closed label emitted by the strict route classifier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRouteClassifierLabel {
    /// Produce one user-facing response without tools.
    Respond,
    /// Execute authorized work using a deterministic internal strategy.
    Execute,
    /// Ask for concrete missing caller input.
    NeedsInput,
}

/// Strict response produced by the bounded execution-route classifier.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRouteClassifierOutput {
    /// Selected closed route label.
    pub label: ExecutionRouteClassifierLabel,
    /// Authoritative strategy, present exactly for Execute.
    pub strategy: Option<ExecutionStrategy>,
    /// Bounded human-readable explanation that never controls execution.
    pub rationale: String,
    /// Model confidence in basis points.
    pub confidence_bps: u16,
    /// Concrete missing inputs, populated only for `needs_input`.
    pub missing_inputs: Vec<String>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenRoutingInput<'a> {
    schema_version: u8,
    objective: &'a str,
    attachment_count: usize,
    recent_target_digest: &'a str,
    available_skill_names: &'a [String],
}

/// Selects one stable execution route with at most one classifier call.
pub async fn route_execution(
    provider: &dyn LLMProvider,
    input: ExecutionRoutingInput<'_>,
) -> Result<ExecutionRoutingResult> {
    let objective_hash = objective_hash(input.objective);
    if input.execution_template.is_some() {
        return Ok(trusted_result(
            ExecutionRouteDecision::Execute {
                strategy: ExecutionStrategy::Durable,
                rationale: "A pinned execution template requires durable execution.".to_string(),
            },
            ExecutionRouteSource::SelectedExecutionTemplate,
            objective_hash,
        ));
    }
    if input.objective.trim().is_empty() {
        return Ok(trusted_result(
            ExecutionRouteDecision::NeedsInput {
                rationale: "The request does not include an objective.".to_string(),
                missing_inputs: vec!["objective".to_string()],
            },
            ExecutionRouteSource::BlankObjective,
            objective_hash,
        ));
    }

    let request = classifier_request(&input)?;
    let started = Instant::now();
    let stream = match provider.complete(request).await {
        Ok(stream) => stream,
        Err(MoaError::Cancelled) => return Err(MoaError::Cancelled),
        Err(_) => {
            return Ok(classifier_fallback(
                &input,
                objective_hash,
                ExecutionRouteClassifierOutcome::ProviderError,
                None,
                None,
                ExecutionRouteUsage::default(),
                duration_micros(started),
            ));
        }
    };
    let response = match stream.collect().await {
        Ok(response) => response,
        Err(MoaError::Cancelled) => return Err(MoaError::Cancelled),
        Err(_) => {
            return Ok(classifier_fallback(
                &input,
                objective_hash,
                ExecutionRouteClassifierOutcome::StreamError,
                None,
                None,
                ExecutionRouteUsage::default(),
                duration_micros(started),
            ));
        }
    };
    let duration_micros = duration_micros(started);
    let provider_model = response.model.to_string();
    let usage = route_usage(response.token_usage())?;
    let response_hash = execution_planning_hash(
        "moa.execution.route-classifier-response",
        response.text.as_bytes(),
    );
    if response.text.len() > EXECUTION_ROUTER_RESPONSE_MAX_BYTES {
        return Ok(classifier_fallback_with_response(
            objective_hash,
            ExecutionRouteClassifierOutcome::Oversized,
            provider_model,
            response_hash,
            None,
            usage,
            duration_micros,
        ));
    }
    let output = match serde_json::from_str::<ExecutionRouteClassifierOutput>(&response.text) {
        Ok(output) => output,
        Err(_) => {
            return Ok(classifier_fallback_with_response(
                objective_hash,
                ExecutionRouteClassifierOutcome::SchemaRejected,
                provider_model,
                response_hash,
                None,
                usage,
                duration_micros,
            ));
        }
    };
    if !valid_classifier_output(&output) {
        return Ok(classifier_fallback_with_response(
            objective_hash,
            ExecutionRouteClassifierOutcome::InvalidDecision,
            provider_model,
            response_hash,
            None,
            usage,
            duration_micros,
        ));
    }
    if below_confidence_threshold(&output) {
        return Ok(classifier_fallback_with_response(
            objective_hash,
            ExecutionRouteClassifierOutcome::LowConfidence,
            provider_model,
            response_hash,
            Some(output.confidence_bps),
            usage,
            duration_micros,
        ));
    }
    // Attachments are a structured fact the classifier cannot see in the objective
    // text, so a Respond/NeedsInput label is overridden to bounded Inline execution.
    // Recent conversation context is NOT a hard override: it reaches the classifier
    // as `recent_target_digest` in the prompt, and the model decides whether a terse
    // follow-up has a concrete referent, so its Respond/NeedsInput label survives.
    if matches!(
        output.label,
        ExecutionRouteClassifierLabel::Respond | ExecutionRouteClassifierLabel::NeedsInput
    ) && input.attachment_count > 0
    {
        return Ok(classifier_fallback_with_response(
            objective_hash,
            ExecutionRouteClassifierOutcome::ContextForcedInline,
            provider_model,
            response_hash,
            Some(output.confidence_bps),
            usage,
            duration_micros,
        ));
    }

    let confidence_bps = output.confidence_bps;
    let Some(decision) = decision_from_output(output) else {
        return Ok(classifier_fallback_with_response(
            objective_hash,
            ExecutionRouteClassifierOutcome::InvalidDecision,
            provider_model,
            response_hash,
            Some(confidence_bps),
            usage,
            duration_micros,
        ));
    };
    let missing_input_count = decision_missing_input_count(&decision)?;
    Ok(ExecutionRoutingResult {
        decision,
        provenance: ExecutionRouteProvenance {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
            provider_model: Some(provider_model),
            prompt_version: Some(EXECUTION_ROUTER_PROMPT_VERSION.to_string()),
            objective_hash,
            response_hash: Some(response_hash),
            confidence_bps: Some(confidence_bps),
            missing_input_count,
            usage,
            cost_microusd: 0,
            duration_micros,
        },
    })
}

fn classifier_request(input: &ExecutionRoutingInput<'_>) -> Result<CompletionRequest> {
    let frozen = FrozenRoutingInput {
        schema_version: 2,
        objective: input.objective,
        attachment_count: input.attachment_count,
        recent_target_digest: input.recent_target_digest,
        available_skill_names: bounded_skill_names(input.available_skill_names),
    };
    let schema = serde_json::to_value(schema_for!(ExecutionRouteClassifierOutput))
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let frozen_json = serde_json::to_string(&frozen)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(CompletionRequest {
        model: Some(input.classifier_model.clone()),
        messages: vec![
            ContextMessage::system(EXECUTION_ROUTER_PROMPT),
            ContextMessage::user(frozen_json),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(EXECUTION_ROUTER_MAX_OUTPUT_TOKENS),
        temperature: Some(0.0),
        response_format: Some(JsonResponseFormat::strict_json_schema(
            "execution_route_classifier",
            "Classify one user turn into respond, execute, or needs_input.",
            schema,
        )),
        native_web_search: NativeWebSearchPolicy::Disabled,
        metadata: HashMap::from([
            (
                ROUTER_STAGE_METADATA_KEY.to_string(),
                json!("execution_routing"),
            ),
            (
                OPENAI_REASONING_EFFORT_METADATA_KEY.to_string(),
                json!("none"),
            ),
        ]),
    })
}

fn trusted_result(
    decision: ExecutionRouteDecision,
    source: ExecutionRouteSource,
    objective_hash: String,
) -> ExecutionRoutingResult {
    let missing_input_count = match &decision {
        ExecutionRouteDecision::NeedsInput { missing_inputs, .. } => {
            u8::try_from(missing_inputs.len()).unwrap_or(u8::MAX)
        }
        ExecutionRouteDecision::Respond { .. } | ExecutionRouteDecision::Execute { .. } => 0,
    };
    ExecutionRoutingResult {
        decision,
        provenance: ExecutionRouteProvenance {
            source,
            classifier_outcome: ExecutionRouteClassifierOutcome::NotCalled,
            provider_model: None,
            prompt_version: None,
            objective_hash,
            response_hash: None,
            confidence_bps: None,
            missing_input_count,
            usage: ExecutionRouteUsage::default(),
            cost_microusd: 0,
            duration_micros: 0,
        },
    }
}

fn classifier_fallback(
    input: &ExecutionRoutingInput<'_>,
    objective_hash: String,
    outcome: ExecutionRouteClassifierOutcome,
    response_hash: Option<String>,
    confidence_bps: Option<u16>,
    usage: ExecutionRouteUsage,
    duration_micros: u64,
) -> ExecutionRoutingResult {
    ExecutionRoutingResult {
        decision: ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            rationale:
                "The request may require tools, so bounded inline execution is the safe fallback."
                    .to_string(),
        },
        provenance: ExecutionRouteProvenance {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: outcome,
            provider_model: Some(input.classifier_model.to_string()),
            prompt_version: Some(EXECUTION_ROUTER_PROMPT_VERSION.to_string()),
            objective_hash,
            response_hash,
            confidence_bps,
            missing_input_count: 0,
            usage,
            cost_microusd: 0,
            duration_micros,
        },
    }
}

fn classifier_fallback_with_response(
    objective_hash: String,
    outcome: ExecutionRouteClassifierOutcome,
    provider_model: String,
    response_hash: String,
    confidence_bps: Option<u16>,
    usage: ExecutionRouteUsage,
    duration_micros: u64,
) -> ExecutionRoutingResult {
    ExecutionRoutingResult {
        decision: ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            rationale:
                "The request may require tools, so bounded inline execution is the safe fallback."
                    .to_string(),
        },
        provenance: ExecutionRouteProvenance {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: outcome,
            provider_model: Some(provider_model),
            prompt_version: Some(EXECUTION_ROUTER_PROMPT_VERSION.to_string()),
            objective_hash,
            response_hash: Some(response_hash),
            confidence_bps,
            missing_input_count: 0,
            usage,
            cost_microusd: 0,
            duration_micros,
        },
    }
}

fn valid_classifier_output(output: &ExecutionRouteClassifierOutput) -> bool {
    if output.confidence_bps > 10_000 || !execution_route_rationale_is_valid(&output.rationale) {
        return false;
    }
    let valid_strategy = matches!(
        (output.label, output.strategy),
        (
            ExecutionRouteClassifierLabel::Respond | ExecutionRouteClassifierLabel::NeedsInput,
            None
        ) | (ExecutionRouteClassifierLabel::Execute, Some(_))
    );
    if !valid_strategy {
        return false;
    }
    match output.label {
        ExecutionRouteClassifierLabel::NeedsInput => {
            (1..=EXECUTION_ROUTER_MAX_MISSING_INPUTS).contains(&output.missing_inputs.len())
                && output.missing_inputs.iter().all(|value| {
                    !value.trim().is_empty()
                        && value.len() <= EXECUTION_ROUTER_MAX_MISSING_INPUT_BYTES
                })
        }
        ExecutionRouteClassifierLabel::Respond | ExecutionRouteClassifierLabel::Execute => {
            output.missing_inputs.is_empty()
        }
    }
}

fn below_confidence_threshold(output: &ExecutionRouteClassifierOutput) -> bool {
    match output.label {
        ExecutionRouteClassifierLabel::Respond | ExecutionRouteClassifierLabel::NeedsInput => {
            output.confidence_bps < EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS
        }
        ExecutionRouteClassifierLabel::Execute
            if output.strategy == Some(ExecutionStrategy::Durable) =>
        {
            output.confidence_bps < EXECUTION_ROUTER_DURABLE_CONFIDENCE_BPS
        }
        ExecutionRouteClassifierLabel::Execute => false,
    }
}

fn decision_from_output(output: ExecutionRouteClassifierOutput) -> Option<ExecutionRouteDecision> {
    match (output.label, output.strategy) {
        (ExecutionRouteClassifierLabel::NeedsInput, None) => {
            Some(ExecutionRouteDecision::NeedsInput {
                rationale: output.rationale,
                missing_inputs: output.missing_inputs,
            })
        }
        (ExecutionRouteClassifierLabel::Respond, None) => Some(ExecutionRouteDecision::Respond {
            rationale: output.rationale,
        }),
        (ExecutionRouteClassifierLabel::Execute, Some(strategy)) => {
            Some(ExecutionRouteDecision::Execute {
                strategy,
                rationale: output.rationale,
            })
        }
        _ => None,
    }
}

fn bounded_skill_names(names: &[String]) -> &[String] {
    &names[..names.len().min(EXECUTION_ROUTER_MAX_SKILL_NAMES)]
}

fn objective_hash(objective: &str) -> String {
    execution_planning_hash("moa.execution.route-objective", objective.as_bytes())
}

fn decision_missing_input_count(decision: &ExecutionRouteDecision) -> Result<u8> {
    match decision {
        ExecutionRouteDecision::NeedsInput { missing_inputs, .. } => {
            u8::try_from(missing_inputs.len()).map_err(|_| {
                MoaError::ValidationError(
                    "route missing-input count exceeds the audit representation".to_string(),
                )
            })
        }
        ExecutionRouteDecision::Respond { .. } | ExecutionRouteDecision::Execute { .. } => Ok(0),
    }
}

fn route_usage(usage: TokenUsage) -> Result<ExecutionRouteUsage> {
    Ok(ExecutionRouteUsage {
        input_tokens_uncached: u64::try_from(usage.input_tokens_uncached).map_err(|_| {
            MoaError::ValidationError("route uncached input usage exceeds u64".to_string())
        })?,
        input_tokens_cache_write: u64::try_from(usage.input_tokens_cache_write).map_err(|_| {
            MoaError::ValidationError("route cache-write usage exceeds u64".to_string())
        })?,
        input_tokens_cache_read: u64::try_from(usage.input_tokens_cache_read).map_err(|_| {
            MoaError::ValidationError("route cache-read usage exceeds u64".to_string())
        })?,
        output_tokens: u64::try_from(usage.output_tokens)
            .map_err(|_| MoaError::ValidationError("route output usage exceeds u64".to_string()))?,
    })
}

fn duration_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use async_trait::async_trait;
    use moa_core::{
        error::MoaError,
        types::{
            completion::{CompletionContent, CompletionResponse, CompletionStream, StopReason},
            execution_planning::PinnedExecutionTemplateRef,
            model::ModelCapabilities,
        },
    };
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;

    #[derive(Clone)]
    enum ProviderBehavior {
        Response(CompletionResponse),
        ProviderError,
        StreamError,
        CancelledProvider,
        CancelledStream,
    }

    struct ScriptedRouteProvider {
        behavior: ProviderBehavior,
        calls: AtomicU64,
        requests: Mutex<Vec<CompletionRequest>>,
    }

    impl ScriptedRouteProvider {
        fn new(behavior: ProviderBehavior) -> Self {
            Self {
                behavior,
                calls: AtomicU64::new(0),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests
                .lock()
                .expect("route request journal lock should remain available")
                .clone()
        }
    }

    #[async_trait]
    impl LLMProvider for ScriptedRouteProvider {
        fn name(&self) -> &str {
            "scripted-route"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.requests
                .lock()
                .expect("route request journal lock should remain available")
                .push(request);
            match &self.behavior {
                ProviderBehavior::Response(response) => {
                    Ok(CompletionStream::from_response(response.clone()))
                }
                ProviderBehavior::ProviderError => Err(MoaError::ProviderError(
                    "scripted provider failure".to_string(),
                )),
                ProviderBehavior::StreamError => {
                    let (_sender, receiver) = mpsc::channel(1);
                    let completion = tokio::spawn(async {
                        Err(MoaError::ProviderError(
                            "scripted stream failure".to_string(),
                        ))
                    });
                    Ok(CompletionStream::new(receiver, completion))
                }
                ProviderBehavior::CancelledProvider => Err(MoaError::Cancelled),
                ProviderBehavior::CancelledStream => {
                    let (_sender, receiver) = mpsc::channel(1);
                    let completion = tokio::spawn(async { Err(MoaError::Cancelled) });
                    Ok(CompletionStream::new(receiver, completion))
                }
            }
        }
    }

    fn classifier_model() -> ModelId {
        ModelId::new("route-model")
    }

    fn routing_input<'a>(objective: &'a str, model: &'a ModelId) -> ExecutionRoutingInput<'a> {
        ExecutionRoutingInput {
            objective,
            execution_template: None,
            attachment_count: 0,
            recent_target_digest: "",
            available_skill_names: &[],
            classifier_model: model,
        }
    }

    fn response(output: &ExecutionRouteClassifierOutput) -> CompletionResponse {
        response_text(
            serde_json::to_string(output).expect("route classifier output should serialize"),
        )
    }

    fn response_text(text: String) -> CompletionResponse {
        CompletionResponse {
            content: vec![CompletionContent::Text(text.clone())],
            text,
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("route-model-returned"),
            usage: TokenUsage {
                input_tokens_uncached: 11,
                input_tokens_cache_write: 2,
                input_tokens_cache_read: 3,
                output_tokens: 5,
            },
            duration_ms: 7,
            thought_signature: None,
        }
    }

    const TEST_RATIONALE: &str =
        "The requested workflow benefits from the selected execution strategy.";

    fn output(
        label: ExecutionRouteClassifierLabel,
        strategy: Option<ExecutionStrategy>,
        confidence_bps: u16,
    ) -> ExecutionRouteClassifierOutput {
        ExecutionRouteClassifierOutput {
            label,
            strategy,
            rationale: TEST_RATIONALE.to_string(),
            confidence_bps,
            missing_inputs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn execution_routing_uses_one_strict_bounded_classifier_call_offline() {
        // Pins: an ordinary user route is one no-tools/no-search strict auxiliary call.
        let model = classifier_model();
        let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(&output(
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Durable),
            9_500,
        ))));
        let result = route_execution(
            &provider,
            routing_input(
                "Screen all of the S&P 500 over five years for AI mentions",
                &model,
            ),
        )
        .await
        .expect("strict route classification should succeed");

        assert_eq!(provider.call_count(), 1);
        assert_eq!(
            result.decision,
            ExecutionRouteDecision::Execute {
                strategy: ExecutionStrategy::Durable,
                rationale: TEST_RATIONALE.to_string(),
            }
        );
        assert_eq!(
            result.provenance.classifier_outcome,
            ExecutionRouteClassifierOutcome::Accepted
        );
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.model.as_ref(), Some(&model));
        assert!(request.tools.is_empty());
        assert_eq!(request.max_output_tokens, Some(256));
        assert_eq!(request.temperature, Some(0.0));
        assert_eq!(request.native_web_search, NativeWebSearchPolicy::Disabled);
        assert_eq!(request.messages.len(), 2);
        assert!(!request.messages[1].content.contains("escalation"));
        assert!(!request.messages[1].content.contains("durable_upgrade"));
        let response_format = request
            .response_format
            .as_ref()
            .expect("route classifier should require a response format");
        assert!(response_format.strict);
        assert_eq!(
            response_format.schema.pointer("/properties/label/$ref"),
            Some(&json!("#/$defs/ExecutionRouteClassifierLabel"))
        );
        let label_variants = response_format
            .schema
            .pointer("/$defs/ExecutionRouteClassifierLabel/oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("route classifier label schema should be a closed oneOf");
        assert_eq!(label_variants.len(), 3);
        assert_eq!(
            label_variants
                .iter()
                .filter_map(|variant| variant.get("const").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>(),
            vec!["respond", "execute", "needs_input"]
        );
    }

    #[test]
    fn execution_router_schema_compiles_to_openai_strict_compatible_offline() {
        // Pins: the router response-format schema, after provider compilation,
        // satisfies OpenAI strict mode (no oneOf, no $ref siblings, explicit
        // types). A violation here 400s every live routing call and stalls
        // turns in restate retry instead of reaching the deterministic
        // fallback — scripted-provider lanes cannot catch it.
        let schema = serde_json::to_value(schemars::schema_for!(ExecutionRouteClassifierOutput))
            .expect("serialize route classifier schema");
        let compiled = moa_providers::compile_for_openai_strict(&schema);
        let violations = moa_providers::openai_strict_violations(&compiled);
        assert!(
            violations.is_empty(),
            "router schema violates OpenAI strict mode: {violations:?}"
        );
    }

    #[tokio::test]
    async fn execution_routing_template_and_blank_routes_make_zero_classifier_calls_offline() {
        // Pins: exact templates and blank objectives bypass probabilistic classification.
        let model = classifier_model();
        let provider = ScriptedRouteProvider::new(ProviderBehavior::ProviderError);
        let template = ExecutionTemplateInvocation {
            template: PinnedExecutionTemplateRef {
                skill_ref: "skill://research/example".to_string(),
                revision_uid: Uuid::now_v7(),
            },
            input: json!({}),
        };
        let template_result = route_execution(
            &provider,
            ExecutionRoutingInput {
                execution_template: Some(&template),
                ..routing_input("run the template", &model)
            },
        )
        .await
        .expect("trusted template route should succeed");
        assert_eq!(
            template_result.provenance.source,
            ExecutionRouteSource::SelectedExecutionTemplate
        );

        let blank_result = route_execution(&provider, routing_input("  ", &model))
            .await
            .expect("blank objective preflight should succeed");
        assert_eq!(
            blank_result.decision,
            ExecutionRouteDecision::NeedsInput {
                rationale: "The request does not include an objective.".to_string(),
                missing_inputs: vec!["objective".to_string()],
            }
        );
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn execution_routing_failures_and_low_confidence_fall_back_to_inline_offline() {
        // Pins: uncertain routing always falls back to Execute/Inline.
        let model = classifier_model();
        let cases = [
            (
                ProviderBehavior::ProviderError,
                ExecutionRouteClassifierOutcome::ProviderError,
            ),
            (
                ProviderBehavior::StreamError,
                ExecutionRouteClassifierOutcome::StreamError,
            ),
            (
                ProviderBehavior::Response(response_text("{}".to_string())),
                ExecutionRouteClassifierOutcome::SchemaRejected,
            ),
            (
                ProviderBehavior::Response(response_text(
                    "x".repeat(EXECUTION_ROUTER_RESPONSE_MAX_BYTES + 1),
                )),
                ExecutionRouteClassifierOutcome::Oversized,
            ),
            (
                ProviderBehavior::Response(response(&output(
                    ExecutionRouteClassifierLabel::Execute,
                    Some(ExecutionStrategy::Durable),
                    7_999,
                ))),
                ExecutionRouteClassifierOutcome::LowConfidence,
            ),
            (
                ProviderBehavior::Response(response(&output(
                    ExecutionRouteClassifierLabel::Respond,
                    Some(ExecutionStrategy::Durable),
                    9_500,
                ))),
                ExecutionRouteClassifierOutcome::InvalidDecision,
            ),
        ];
        for (behavior, expected_outcome) in cases {
            let provider = ScriptedRouteProvider::new(behavior);
            let result = route_execution(&provider, routing_input("investigate this", &model))
                .await
                .expect("classifier failures should degrade safely");
            assert_eq!(provider.call_count(), 1);
            assert!(matches!(
                result.decision,
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    ..
                }
            ));
            assert_eq!(result.provenance.classifier_outcome, expected_outcome);
        }
    }

    #[tokio::test]
    async fn execution_routing_preserves_cancellation_from_provider_and_stream_offline() {
        // Pins: workflow cancellation remains a typed terminal outcome instead of being
        // normalized into the router's execute-inline availability fallback.
        let model = classifier_model();
        for behavior in [
            ProviderBehavior::CancelledProvider,
            ProviderBehavior::CancelledStream,
        ] {
            let provider = ScriptedRouteProvider::new(behavior);
            let error = route_execution(&provider, routing_input("cancel this turn", &model))
                .await
                .expect_err("cancellation must escape routing");
            assert!(matches!(error, MoaError::Cancelled));
            assert_eq!(provider.call_count(), 1);
        }
    }

    #[tokio::test]
    async fn execution_routing_rejects_unbounded_rationales_without_interpreting_them_offline() {
        // Pins: free-form explanations may describe any domain, but malformed text falls back
        // and changing the sentence never changes an explicit strategy.
        let model = classifier_model();
        for rationale in [
            String::new(),
            " leading whitespace".to_string(),
            "multiple\nlines".to_string(),
            "x".repeat(241),
        ] {
            let mut classifier_output = output(
                ExecutionRouteClassifierLabel::Execute,
                Some(ExecutionStrategy::Durable),
                9_500,
            );
            classifier_output.rationale = rationale;
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(&provider, routing_input("inspect this asset", &model))
                .await
                .expect("invalid rationale should degrade safely");
            assert!(matches!(
                result.decision,
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    ..
                }
            ));
            assert_eq!(
                result.provenance.classifier_outcome,
                ExecutionRouteClassifierOutcome::InvalidDecision
            );
        }

        for strategy in [ExecutionStrategy::Inline, ExecutionStrategy::Durable] {
            let mut classifier_output = output(
                ExecutionRouteClassifierLabel::Execute,
                Some(strategy),
                9_500,
            );
            classifier_output.rationale =
                "The legal workflow uses a jurisdiction-specific filing sequence.".to_string();
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(&provider, routing_input("file the matter", &model))
                .await
                .expect("arbitrary bounded rationale should be accepted");
            assert_eq!(result.decision.strategy(), Some(strategy));
            assert_eq!(
                result.decision.rationale(),
                classifier_output.rationale.as_str()
            );
        }
    }

    #[tokio::test]
    async fn execution_routing_enforces_confidence_boundaries_exactly_offline() {
        // Pins: confidence basis points admit exact thresholds and reject values outside them.
        let model = classifier_model();
        let cases = [
            (
                output(
                    ExecutionRouteClassifierLabel::Execute,
                    Some(ExecutionStrategy::Durable),
                    10_000,
                ),
                ExecutionRouteClassifierOutcome::Accepted,
            ),
            (
                output(
                    ExecutionRouteClassifierLabel::Execute,
                    Some(ExecutionStrategy::Durable),
                    10_001,
                ),
                ExecutionRouteClassifierOutcome::InvalidDecision,
            ),
            (
                output(
                    ExecutionRouteClassifierLabel::Execute,
                    Some(ExecutionStrategy::Durable),
                    EXECUTION_ROUTER_DURABLE_CONFIDENCE_BPS,
                ),
                ExecutionRouteClassifierOutcome::Accepted,
            ),
            (
                output(
                    ExecutionRouteClassifierLabel::Respond,
                    None,
                    EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS - 1,
                ),
                ExecutionRouteClassifierOutcome::LowConfidence,
            ),
            (
                output(
                    ExecutionRouteClassifierLabel::Respond,
                    None,
                    EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS,
                ),
                ExecutionRouteClassifierOutcome::Accepted,
            ),
            (
                output(
                    ExecutionRouteClassifierLabel::Execute,
                    Some(ExecutionStrategy::Inline),
                    0,
                ),
                ExecutionRouteClassifierOutcome::Accepted,
            ),
        ];

        for (classifier_output, expected_outcome) in cases {
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(&provider, routing_input("classify this", &model))
                .await
                .expect("confidence boundary must produce a safe route");
            assert_eq!(
                result.provenance.classifier_outcome, expected_outcome,
                "unexpected result for {classifier_output:?}"
            );
        }
    }

    #[tokio::test]
    async fn execution_routing_rejects_blank_and_oversized_missing_inputs_offline() {
        // Pins: each classifier-requested input is concrete, nonblank, and byte bounded.
        let model = classifier_model();
        let invalid_missing_inputs = [
            "   ".to_string(),
            "x".repeat(EXECUTION_ROUTER_MAX_MISSING_INPUT_BYTES + 1),
        ];

        for missing_input in invalid_missing_inputs {
            let mut classifier_output = output(
                ExecutionRouteClassifierLabel::NeedsInput,
                None,
                EXECUTION_ROUTER_HIGH_RISK_CONFIDENCE_BPS,
            );
            classifier_output.missing_inputs = vec![missing_input];
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(&provider, routing_input("screen the universe", &model))
                .await
                .expect("invalid clarification must degrade safely");
            assert_eq!(
                result.provenance.classifier_outcome,
                ExecutionRouteClassifierOutcome::InvalidDecision
            );
            assert!(matches!(
                result.decision,
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn execution_routing_attachments_force_inline_offline() {
        // Pins: attachments are a structured fact the classifier cannot read in the
        // objective text, so a Respond or NeedsInput label is overridden to bounded
        // Inline execution.
        let model = classifier_model();
        let mut needs_input = output(ExecutionRouteClassifierLabel::NeedsInput, None, 9_500);
        needs_input.missing_inputs = vec!["target".to_string()];
        for classifier_output in [
            output(ExecutionRouteClassifierLabel::Respond, None, 9_500),
            needs_input,
        ] {
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(
                &provider,
                ExecutionRoutingInput {
                    attachment_count: 1,
                    ..routing_input("summarize this", &model)
                },
            )
            .await
            .expect("attachment-bearing route should classify");
            assert_eq!(
                result.provenance.classifier_outcome,
                ExecutionRouteClassifierOutcome::ContextForcedInline
            );
            assert!(matches!(
                result.decision,
                ExecutionRouteDecision::Execute {
                    strategy: ExecutionStrategy::Inline,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn execution_routing_recent_target_digest_does_not_force_inline_offline() {
        // Pins: a recent-target digest is NOT a hard override. Unlike the old keyword
        // bool, textual target context reaches the classifier as prompt content, so a
        // confident Respond or NeedsInput label survives instead of being forced to Act.
        // The digest travels in the dynamic user message, never the cached prompt prefix.
        let model = classifier_model();
        let digest =
            "user: the pricing page we discussed\ntool file_read: {\"path\":\"pricing.md\"}";
        let mut needs_input = output(ExecutionRouteClassifierLabel::NeedsInput, None, 9_500);
        needs_input.missing_inputs = vec!["target".to_string()];
        let cases = [
            (
                output(ExecutionRouteClassifierLabel::Respond, None, 9_500),
                ExecutionRouteDecision::Respond {
                    rationale: TEST_RATIONALE.to_string(),
                },
            ),
            (
                needs_input,
                ExecutionRouteDecision::NeedsInput {
                    rationale: TEST_RATIONALE.to_string(),
                    missing_inputs: vec!["target".to_string()],
                },
            ),
        ];
        for (classifier_output, expected_decision) in cases {
            let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(
                &classifier_output,
            )));
            let result = route_execution(
                &provider,
                ExecutionRoutingInput {
                    attachment_count: 0,
                    recent_target_digest: digest,
                    ..routing_input("continue", &model)
                },
            )
            .await
            .expect("digest-bearing route should classify");
            assert_eq!(
                result.provenance.classifier_outcome,
                ExecutionRouteClassifierOutcome::Accepted
            );
            assert_eq!(result.decision, expected_decision);
            let requests = provider.requests();
            assert!(
                requests[0].messages[1].content.contains("pricing.md"),
                "digest must reach the dynamic user message"
            );
            assert!(
                !requests[0].messages[0].content.contains("pricing.md"),
                "digest must not enter the cached system-prompt prefix"
            );
        }
    }

    #[tokio::test]
    async fn execution_routing_serializes_bounded_skill_names_into_dynamic_message_offline() {
        // Pins: installed-skill names reach the classifier as a coverage hint in the
        // dynamic user message (never the cached system prefix), preserve caller order,
        // and are capped at EXECUTION_ROUTER_MAX_SKILL_NAMES so tenant size cannot
        // unbound the prompt.
        let model = classifier_model();
        let names = (0..(EXECUTION_ROUTER_MAX_SKILL_NAMES + 6))
            .map(|index| format!("skill-{index:02}"))
            .collect::<Vec<_>>();
        let provider = ScriptedRouteProvider::new(ProviderBehavior::Response(response(&output(
            ExecutionRouteClassifierLabel::Execute,
            Some(ExecutionStrategy::Inline),
            9_500,
        ))));
        let result = route_execution(
            &provider,
            ExecutionRoutingInput {
                available_skill_names: &names,
                ..routing_input("summarize our refund policy", &model)
            },
        )
        .await
        .expect("skill-name-bearing route should classify");
        assert_eq!(
            result.provenance.classifier_outcome,
            ExecutionRouteClassifierOutcome::Accepted
        );
        let requests = provider.requests();
        let dynamic = &requests[0].messages[1].content;
        // First name is present, last kept name (index 23) is present, first dropped
        // name (index 24) is absent: exactly EXECUTION_ROUTER_MAX_SKILL_NAMES kept in order.
        assert!(dynamic.contains("\"skill-00\""));
        assert!(dynamic.contains(&format!(
            "\"skill-{:02}\"",
            EXECUTION_ROUTER_MAX_SKILL_NAMES - 1
        )));
        assert!(!dynamic.contains(&format!(
            "\"skill-{:02}\"",
            EXECUTION_ROUTER_MAX_SKILL_NAMES
        )));
        assert!(dynamic.contains("available_skill_names"));
        assert!(
            !requests[0].messages[0].content.contains("skill-00"),
            "skill names must not enter the cached system-prompt prefix"
        );
    }

    #[test]
    fn execution_router_prompt_carries_skill_coverage_guidance_offline() {
        // Pins: the router prompt keeps the additive skill-coverage guidance (prefer
        // execute when an installed skill covers the request) alongside the pre-existing
        // digest guidance, and still treats the names as untrusted data.
        assert!(EXECUTION_ROUTER_PROMPT.contains("available_skill_names"));
        assert!(
            EXECUTION_ROUTER_PROMPT
                .contains("prefer execute with strategy inline over needs_input or respond")
        );
        assert!(EXECUTION_ROUTER_PROMPT.contains("gathering any missing inputs"));
        assert!(EXECUTION_ROUTER_PROMPT.contains("recent_target_digest"));
    }

    #[test]
    fn execution_router_prompt_routes_bounded_worker_requests_to_inline_execution_offline() {
        // Pins: explicit same-turn worker/tool requests cannot be accepted as a
        // direct response that merely claims the requested side effects occurred.
        assert!(
            EXECUTION_ROUTER_PROMPT.contains(
                "spawn or delegate to conversational workers, or perform another action, choose execute rather than respond"
            )
        );
        assert!(EXECUTION_ROUTER_PROMPT.contains(
            "including a small parallel set of conversational workers or a follow-up worker"
        ));
        assert!(EXECUTION_ROUTER_PROMPT.contains(
            "A respond route must never claim that requested tool or worker calls occurred"
        ));
        assert!(
            EXECUTION_ROUTER_PROMPT.contains(
                "Parallelism alone does not require durable execution when the request is a bounded same-turn"
            )
        );
    }

    #[tokio::test]
    async fn execution_routing_needs_input_preserves_bounded_missing_fields_offline() {
        // Pins: classifier clarification carries concrete bounded inputs to the caller.
        let model = classifier_model();
        let mut needs_input = output(ExecutionRouteClassifierLabel::NeedsInput, None, 9_500);
        needs_input.missing_inputs = vec!["coverage universe".to_string()];
        let provider =
            ScriptedRouteProvider::new(ProviderBehavior::Response(response(&needs_input)));
        let result = route_execution(&provider, routing_input("screen the universe", &model))
            .await
            .expect("valid clarification should classify");
        assert_eq!(
            result.decision,
            ExecutionRouteDecision::NeedsInput {
                rationale: TEST_RATIONALE.to_string(),
                missing_inputs: vec!["coverage universe".to_string()],
            }
        );
        assert_eq!(result.provenance.missing_input_count, 1);
    }
}

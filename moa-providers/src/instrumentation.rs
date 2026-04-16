//! Shared tracing helpers for provider-level LLM completion spans.

use chrono::{DateTime, SecondsFormat, Utc};
use moa_core::{CompletionContent, CompletionRequest, CompletionResponse, TokenPricing};
use opentelemetry::trace::Status;
use serde::Serialize;
use serde_json::Value;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

const OPERATION_CHAT: &str = "chat";
const ATTRIBUTE_VALUE_LIMIT: usize = 32 * 1024;

/// Attributes recorded on one GenAI completion span.
#[derive(Debug, Clone, Default)]
pub(crate) struct LLMSpanAttributes {
    /// Provider system identifier.
    pub system: Option<&'static str>,
    /// GenAI operation name.
    pub operation: Option<&'static str>,
    /// Requested model identifier.
    pub request_model: Option<String>,
    /// Response model identifier.
    pub response_model: Option<String>,
    /// Requested temperature.
    pub temperature: Option<f64>,
    /// Requested max token budget.
    pub max_tokens: Option<usize>,
    /// Prompt token count.
    pub input_tokens: Option<usize>,
    /// Completion token count.
    pub output_tokens: Option<usize>,
    /// Total token count.
    pub total_tokens: Option<usize>,
    /// Request cost in dollars.
    pub cost: Option<f64>,
    /// Time the first output block arrived.
    pub completion_start_time: Option<DateTime<Utc>>,
    /// Serialized input content.
    pub input_content: Option<String>,
    /// Serialized output content.
    pub output_content: Option<String>,
    /// Session identifier for Langfuse session grouping.
    pub session_id: Option<String>,
    /// User identifier for Langfuse user analytics.
    pub user_id: Option<String>,
    /// Workspace identifier for filterable metadata.
    pub workspace_id: Option<String>,
    /// Originating platform name.
    pub platform: Option<String>,
    /// Human-readable trace name.
    pub trace_name: Option<String>,
    /// Total compiled context tokens for the request.
    pub context_tokens: Option<usize>,
    /// Prefix cache hit ratio for the request.
    pub cache_hit_ratio: Option<f64>,
    /// Explicit prompt cache read tokens reported by the provider.
    pub cache_read_tokens: Option<usize>,
    /// Explicit prompt cache creation tokens reported by the provider.
    pub cache_creation_tokens: Option<usize>,
    /// Actual provider-reported prompt cache hit rate.
    pub provider_cache_hit_rate: Option<f64>,
}

/// Per-request span recorder used by provider streaming tasks.
#[derive(Debug, Clone)]
pub(crate) struct LLMSpanRecorder {
    span: Span,
    pricing: TokenPricing,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
    first_output_at: Option<DateTime<Utc>>,
    streamed_output: Vec<CompletionContent>,
}

impl LLMSpanRecorder {
    /// Creates a new GenAI span recorder for one logical chat completion.
    pub(crate) fn new(
        system: &'static str,
        request_model: impl Into<String>,
        request: &CompletionRequest,
        max_tokens: Option<usize>,
        pricing: TokenPricing,
    ) -> Self {
        let request_model = request_model.into();
        let span_name = llm_span_name(OPERATION_CHAT, system, &request_model);
        let span = tracing::info_span!("llm_completion", otel.name = %span_name);

        record_llm_span_attributes(
            &span,
            &LLMSpanAttributes {
                system: Some(system),
                operation: Some(OPERATION_CHAT),
                request_model: Some(request_model),
                temperature: request.temperature.map(f64::from),
                max_tokens,
                input_content: serialize_input_content(request),
                session_id: metadata_string(request, "_moa.session_id"),
                user_id: metadata_string(request, "_moa.user_id"),
                workspace_id: metadata_string(request, "_moa.workspace_id"),
                platform: metadata_string(request, "_moa.platform"),
                trace_name: metadata_string(request, "_moa.trace_name"),
                context_tokens: metadata_usize(request, "_moa.context_tokens"),
                cache_hit_ratio: metadata_f64(request, "_moa.cache_ratio"),
                ..LLMSpanAttributes::default()
            },
        );

        Self {
            span,
            pricing,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            first_output_at: None,
            streamed_output: Vec::new(),
        }
    }

    /// Returns the owned tracing span so callers can instrument async tasks with it.
    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    /// Records the current internal provider phase on the active span.
    pub(crate) fn set_phase(&self, phase: &'static str) {
        self.span
            .set_attribute("langfuse.observation.metadata.provider_phase", phase);
    }

    /// Records the cached prompt token count used to price the request accurately.
    pub(crate) fn set_cached_input_tokens(&mut self, cached_input_tokens: usize) {
        self.cached_input_tokens = cached_input_tokens;
    }

    /// Records the prompt cache write tokens used to create or refresh a provider cache entry.
    pub(crate) fn set_cache_creation_input_tokens(&mut self, cache_creation_input_tokens: usize) {
        self.cache_creation_input_tokens = cache_creation_input_tokens;
    }

    /// Records a provider-private debug payload on the active span.
    pub(crate) fn record_raw_response<T>(&self, payload: &T)
    where
        T: Serialize,
    {
        if let Some(serialized) = serialize_provider_debug_payload(payload) {
            self.span.set_attribute(
                "langfuse.observation.metadata.provider_raw_response",
                serialized,
            );
        }
    }

    /// Observes one streamed output block, capturing TTFT and partial output.
    pub(crate) fn observe_block(&mut self, block: &CompletionContent) {
        if !has_meaningful_output(block) {
            return;
        }

        if self.first_output_at.is_none() {
            let now = Utc::now();
            self.first_output_at = Some(now);
            record_llm_span_attributes(
                &self.span,
                &LLMSpanAttributes {
                    completion_start_time: Some(now),
                    ..LLMSpanAttributes::default()
                },
            );
        }

        self.streamed_output.push(block.clone());
    }

    /// Finalizes the span with usage, cost, and response content.
    pub(crate) fn finish(&self, response: &CompletionResponse) {
        let total_tokens = response.input_tokens + response.output_tokens;
        let output_content = if response.content.is_empty() {
            serialize_output_text(&response.text)
        } else {
            serialize_output_content(&response.content)
        };
        let cost = calculate_cost_with_cached(
            response.input_tokens,
            self.cached_input_tokens.max(response.cached_input_tokens),
            response.output_tokens,
            &self.pricing,
        );
        let cached_input_tokens = self.cached_input_tokens.max(response.cached_input_tokens);
        let provider_cache_hit_rate = if response.input_tokens == 0 {
            0.0
        } else {
            cached_input_tokens as f64 / response.input_tokens as f64
        };

        record_llm_span_attributes(
            &self.span,
            &LLMSpanAttributes {
                response_model: Some(response.model.clone()),
                input_tokens: Some(response.input_tokens),
                output_tokens: Some(response.output_tokens),
                total_tokens: Some(total_tokens),
                cost: Some(cost),
                cache_read_tokens: Some(cached_input_tokens),
                cache_creation_tokens: Some(self.cache_creation_input_tokens),
                provider_cache_hit_rate: Some(provider_cache_hit_rate),
                output_content,
                ..LLMSpanAttributes::default()
            },
        );

        tracing::info!(
            cache_read = cached_input_tokens,
            cache_creation = self.cache_creation_input_tokens,
            cache_hit_rate = %format!("{:.1}%", provider_cache_hit_rate * 100.0),
            "provider cache metrics"
        );
    }

    /// Marks the span as failed and records any partial output that was seen.
    pub(crate) fn fail(&self, error: &impl std::fmt::Display) {
        self.span.set_status(Status::error(error.to_string()));
        if !self.streamed_output.is_empty() {
            record_llm_span_attributes(
                &self.span,
                &LLMSpanAttributes {
                    output_content: serialize_output_content(&self.streamed_output),
                    ..LLMSpanAttributes::default()
                },
            );
        }
    }

    /// Marks the span as failed while also recording the provider phase.
    pub(crate) fn fail_at_stage(&self, phase: &'static str, error: &impl std::fmt::Display) {
        self.set_phase(phase);
        self.fail(error);
    }
}

/// Records GenAI semantic-convention attributes on a tracing span.
pub(crate) fn record_llm_span_attributes(span: &Span, attrs: &LLMSpanAttributes) {
    if let Some(system) = attrs.system {
        span.set_attribute("gen_ai.system", system);
    }
    if let Some(operation) = attrs.operation {
        span.set_attribute("gen_ai.operation.name", operation);
    }
    if let Some(model) = attrs.request_model.as_ref() {
        span.set_attribute("gen_ai.request.model", model.clone());
    }
    if let Some(model) = attrs.response_model.as_ref() {
        span.set_attribute("gen_ai.response.model", model.clone());
    }
    if let Some(temperature) = attrs.temperature {
        span.set_attribute("gen_ai.request.temperature", temperature);
    }
    if let Some(max_tokens) = attrs.max_tokens {
        span.set_attribute("gen_ai.request.max_tokens", max_tokens as i64);
    }
    if let Some(input_tokens) = attrs.input_tokens {
        span.set_attribute("gen_ai.usage.prompt_tokens", input_tokens as i64);
    }
    if let Some(output_tokens) = attrs.output_tokens {
        span.set_attribute("gen_ai.usage.completion_tokens", output_tokens as i64);
    }
    if let Some(total_tokens) = attrs.total_tokens {
        span.set_attribute("gen_ai.usage.total_tokens", total_tokens as i64);
    }
    if let Some(cost) = attrs.cost {
        span.set_attribute("gen_ai.usage.cost", cost);
    }
    if let Some(start_time) = attrs.completion_start_time {
        span.set_attribute(
            "langfuse.observation.completion_start_time",
            start_time.to_rfc3339_opts(SecondsFormat::Millis, true),
        );
    }
    if let Some(input) = attrs.input_content.as_ref() {
        span.set_attribute("langfuse.observation.input", input.clone());
    }
    if let Some(output) = attrs.output_content.as_ref() {
        span.set_attribute("langfuse.observation.output", output.clone());
    }
    if let Some(session_id) = attrs.session_id.as_ref() {
        span.set_attribute("langfuse.session.id", session_id.clone());
    }
    if let Some(user_id) = attrs.user_id.as_ref() {
        span.set_attribute("langfuse.user.id", user_id.clone());
    }
    if let Some(workspace_id) = attrs.workspace_id.as_ref() {
        span.set_attribute("langfuse.trace.metadata.workspace_id", workspace_id.clone());
    }
    if let Some(platform) = attrs.platform.as_ref() {
        span.set_attribute("langfuse.trace.metadata.platform", platform.clone());
    }
    if let Some(trace_name) = attrs.trace_name.as_ref() {
        span.set_attribute("langfuse.trace.name", trace_name.clone());
    }
    if let Some(context_tokens) = attrs.context_tokens {
        span.set_attribute(
            "langfuse.observation.metadata.context_tokens",
            context_tokens as i64,
        );
    }
    if let Some(cache_hit_ratio) = attrs.cache_hit_ratio {
        span.set_attribute(
            "langfuse.observation.metadata.cache_hit_ratio",
            cache_hit_ratio,
        );
    }
    if let Some(cache_read_tokens) = attrs.cache_read_tokens {
        span.set_attribute("gen_ai.usage.cache_read_tokens", cache_read_tokens as i64);
    }
    if let Some(cache_creation_tokens) = attrs.cache_creation_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_creation_tokens",
            cache_creation_tokens as i64,
        );
    }
    if let Some(provider_cache_hit_rate) = attrs.provider_cache_hit_rate {
        span.set_attribute("moa.cache.hit_rate", provider_cache_hit_rate);
    }
}

/// Builds the exported span name for an LLM completion call.
pub(crate) fn llm_span_name(operation: &str, system: &str, model: &str) -> String {
    format!("{operation} {system}/{model}")
}

/// Calculates request cost in dollars using uncached pricing only.
pub(crate) fn calculate_cost(
    input_tokens: usize,
    output_tokens: usize,
    pricing: &TokenPricing,
) -> f64 {
    ((input_tokens as f64 * pricing.input_per_mtok)
        + (output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0
}

/// Calculates request cost in dollars, accounting for cached input tokens when available.
pub(crate) fn calculate_cost_with_cached(
    input_tokens: usize,
    cached_input_tokens: usize,
    output_tokens: usize,
    pricing: &TokenPricing,
) -> f64 {
    let cached_input_tokens = cached_input_tokens.min(input_tokens);
    let uncached_input_tokens = input_tokens.saturating_sub(cached_input_tokens);
    let cached_input_rate = pricing
        .cached_input_per_mtok
        .unwrap_or(pricing.input_per_mtok);

    calculate_cost(uncached_input_tokens, output_tokens, pricing)
        + ((cached_input_tokens as f64 * cached_input_rate) / 1_000_000.0)
}

fn has_meaningful_output(block: &CompletionContent) -> bool {
    match block {
        CompletionContent::Text(text) => !text.is_empty(),
        CompletionContent::ToolCall(_) => true,
        CompletionContent::ProviderToolResult { .. } => true,
    }
}

fn serialize_input_content(request: &CompletionRequest) -> Option<String> {
    serde_json::to_string(&request.messages)
        .ok()
        .map(truncate_attribute_value)
}

fn serialize_output_text(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(truncate_attribute_value(text.to_string()))
    }
}

fn metadata_string(request: &CompletionRequest, key: &str) -> Option<String> {
    request
        .metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn metadata_usize(request: &CompletionRequest, key: &str) -> Option<usize> {
    request
        .metadata
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn metadata_f64(request: &CompletionRequest, key: &str) -> Option<f64> {
    request.metadata.get(key).and_then(Value::as_f64)
}

fn serialize_output_content(content: &[CompletionContent]) -> Option<String> {
    serde_json::to_string(content)
        .ok()
        .map(truncate_attribute_value)
}

fn truncate_attribute_value(mut value: String) -> String {
    if value.len() <= ATTRIBUTE_VALUE_LIMIT {
        return value;
    }

    value.truncate(ATTRIBUTE_VALUE_LIMIT);
    value.push('…');
    value
}

fn serialize_provider_debug_payload<T>(payload: &T) -> Option<String>
where
    T: Serialize,
{
    serde_json::to_string(payload)
        .ok()
        .map(truncate_attribute_value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use moa_core::TokenPricing;

    use super::{calculate_cost, llm_span_name, serialize_provider_debug_payload};

    #[test]
    fn llm_span_name_format() {
        assert_eq!(
            llm_span_name("chat", "anthropic", "claude-sonnet-4-6"),
            "chat anthropic/claude-sonnet-4-6"
        );
    }

    #[test]
    fn cost_calculation_correct() {
        let pricing = TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.30),
        };

        let cost = calculate_cost(1_000, 500, &pricing);
        assert!((cost - 0.0105).abs() < 1e-10);
    }

    #[test]
    fn provider_debug_payload_is_serialized_and_truncated() {
        let payload = json!({
            "kind": "response",
            "body": "x".repeat(40_000),
        });

        let serialized =
            serialize_provider_debug_payload(&payload).expect("payload should serialize");

        assert!(serialized.starts_with('{'));
        assert!(serialized.len() > 100);
        assert!(serialized.ends_with('…'));
    }
}

//! Shared tracing helpers for provider-level LLM completion spans.

use moa_core::{
    types::completion::CompletionContent, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::TokenUsage,
    types::model::TokenPricing, types::observability::genai_operation_name,
    types::observability::genai_provider_name,
};
use moa_observability::{
    record_cache_hit_rate, record_genai_client_operation_duration,
    record_genai_client_time_to_first_chunk, record_genai_client_token_usage,
};
use opentelemetry::trace::Status;
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Attributes recorded on one `GenAI` completion span.
#[derive(Debug, Clone, Default)]
pub(crate) struct LLMSpanAttributes {
    /// OpenTelemetry GenAI provider name.
    pub provider: Option<&'static str>,
    /// OpenTelemetry GenAI operation name.
    pub operation: Option<&'static str>,
    /// Whether the provider request streams responses.
    pub stream: Option<bool>,
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
    /// Request cost in dollars.
    pub cost_usd: Option<f64>,
    /// Time to first streamed output block.
    pub time_to_first_chunk: Option<Duration>,
    /// Session identifier for standard GenAI conversation grouping.
    pub conversation_id: Option<String>,
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
    system: &'static str,
    request_model: String,
    pricing: TokenPricing,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
    started_at: Instant,
    first_output_elapsed: Option<Duration>,
}

impl LLMSpanRecorder {
    /// Creates a new `GenAI` span recorder for one logical chat completion.
    pub(crate) fn new(
        system: &'static str,
        request_model: impl Into<String>,
        request: &CompletionRequest,
        max_tokens: Option<usize>,
        pricing: TokenPricing,
    ) -> Self {
        let request_model = request_model.into();
        let provider = genai_provider_name(system);
        let operation = genai_operation_name(system);
        let span_name = llm_span_name(operation, &request_model);
        let span = tracing::info_span!("llm_completion", otel.name = %span_name);

        record_llm_span_attributes(
            &span,
            &LLMSpanAttributes {
                provider: Some(provider),
                operation: Some(operation),
                stream: Some(true),
                request_model: Some(request_model.clone()),
                temperature: request.temperature.map(f64::from),
                max_tokens,
                conversation_id: metadata_string(request, "_moa.session_id"),
                ..LLMSpanAttributes::default()
            },
        );

        Self {
            span,
            system,
            request_model,
            pricing,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            started_at: Instant::now(),
            first_output_elapsed: None,
        }
    }

    /// Returns the owned tracing span so callers can instrument async tasks with it.
    pub(crate) fn span(&self) -> &Span {
        &self.span
    }

    /// Records the current internal provider phase on the active span.
    pub(crate) fn set_phase(&self, phase: &'static str) {
        self.span.set_attribute("moa.provider.phase", phase);
    }

    /// Records the cached prompt token count used to price the request accurately.
    pub(crate) fn set_cached_input_tokens(&mut self, cached_input_tokens: usize) {
        self.cached_input_tokens = cached_input_tokens;
    }

    /// Records the prompt cache write tokens used to create or refresh a provider cache entry.
    pub(crate) fn set_cache_creation_input_tokens(&mut self, cache_creation_input_tokens: usize) {
        self.cache_creation_input_tokens = cache_creation_input_tokens;
    }

    /// Observes one streamed output block, capturing TTFT.
    pub(crate) fn observe_block(&mut self, block: &CompletionContent) {
        if !has_meaningful_output(block) {
            return;
        }

        if self.first_output_elapsed.is_none() {
            let elapsed = self.started_at.elapsed();
            self.first_output_elapsed = Some(elapsed);
            record_llm_span_attributes(
                &self.span,
                &LLMSpanAttributes {
                    time_to_first_chunk: Some(elapsed),
                    ..LLMSpanAttributes::default()
                },
            );
        }
    }

    /// Finalizes the span with usage, cost, and response content.
    pub(crate) fn finish(&self, response: &CompletionResponse) {
        let usage = self.merged_usage(response);
        let cost = calculate_cost_with_cached(
            usage.total_input_tokens(),
            usage.input_tokens_cache_read,
            usage.input_tokens_cache_write,
            usage.output_tokens,
            &self.pricing,
        );
        let provider_cache_hit_rate = usage.cache_hit_rate();

        record_llm_span_attributes(
            &self.span,
            &LLMSpanAttributes {
                response_model: Some(response.model.to_string()),
                input_tokens: Some(usage.total_input_tokens()),
                output_tokens: Some(usage.output_tokens),
                cost_usd: Some(cost),
                cache_read_tokens: Some(usage.input_tokens_cache_read),
                cache_creation_tokens: Some(usage.input_tokens_cache_write),
                provider_cache_hit_rate: Some(provider_cache_hit_rate),
                ..LLMSpanAttributes::default()
            },
        );

        tracing::info!(
            model = %response.model,
            input_uncached = usage.input_tokens_uncached,
            input_cache_read = usage.input_tokens_cache_read,
            input_cache_write = usage.input_tokens_cache_write,
            output = usage.output_tokens,
            cache_hit_rate = %format!("{:.1}%", provider_cache_hit_rate * 100.0),
            "completion received"
        );
        let provider = self.system;
        let request_model = self.request_model.as_str();
        let model = response.model.to_string();
        record_genai_client_operation_duration(
            provider,
            request_model,
            Some(&model),
            None,
            self.started_at.elapsed(),
        );
        record_genai_client_token_usage(
            provider,
            request_model,
            &model,
            "input",
            usage.total_input_tokens() as u64,
        );
        record_genai_client_token_usage(
            provider,
            request_model,
            &model,
            "output",
            usage.output_tokens as u64,
        );
        record_cache_hit_rate(provider, &model, provider_cache_hit_rate);
        if let Some(ttft) = self.first_output_elapsed {
            record_genai_client_time_to_first_chunk(provider, request_model, &model, ttft);
        }
    }

    fn fail_with_class(&self, class: &'static str, error: &impl std::fmt::Display) {
        self.span.set_status(Status::error(error.to_string()));
        self.span.set_attribute("error.type", class);
        record_genai_client_operation_duration(
            self.system,
            &self.request_model,
            None,
            Some(class),
            self.started_at.elapsed(),
        );
    }

    /// Marks the span as failed while also recording the provider phase.
    pub(crate) fn fail_at_stage(&self, phase: &'static str, error: &impl std::fmt::Display) {
        self.set_phase(phase);
        self.fail_with_class(phase, error);
    }

    fn merged_usage(&self, response: &CompletionResponse) -> TokenUsage {
        let mut usage = response.token_usage();
        if usage.input_tokens_cache_read == 0 && self.cached_input_tokens > 0 {
            usage.input_tokens_cache_read = self.cached_input_tokens;
        }
        if usage.input_tokens_cache_write == 0 && self.cache_creation_input_tokens > 0 {
            usage.input_tokens_cache_write = self.cache_creation_input_tokens;
        }
        usage
    }
}

/// Records `GenAI` semantic-convention attributes on a tracing span.
pub(crate) fn record_llm_span_attributes(span: &Span, attrs: &LLMSpanAttributes) {
    if let Some(provider) = attrs.provider {
        span.set_attribute("gen_ai.provider.name", provider);
    }
    if let Some(operation) = attrs.operation {
        span.set_attribute("gen_ai.operation.name", operation);
    }
    if let Some(stream) = attrs.stream {
        span.set_attribute("gen_ai.request.stream", stream);
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
        span.set_attribute("gen_ai.usage.input_tokens", input_tokens as i64);
    }
    if let Some(output_tokens) = attrs.output_tokens {
        span.set_attribute("gen_ai.usage.output_tokens", output_tokens as i64);
    }
    if let Some(cost) = attrs.cost_usd {
        span.set_attribute("moa.llm.cost_usd", cost);
    }
    if let Some(time_to_first_chunk) = attrs.time_to_first_chunk {
        span.set_attribute(
            "gen_ai.response.time_to_first_chunk",
            time_to_first_chunk.as_secs_f64(),
        );
    }
    if let Some(conversation_id) = attrs.conversation_id.as_ref() {
        span.set_attribute("gen_ai.conversation.id", conversation_id.clone());
    }
    if let Some(cache_read_tokens) = attrs.cache_read_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_read.input_tokens",
            cache_read_tokens as i64,
        );
    }
    if let Some(cache_creation_tokens) = attrs.cache_creation_tokens {
        span.set_attribute(
            "gen_ai.usage.cache_creation.input_tokens",
            cache_creation_tokens as i64,
        );
    }
    if let Some(provider_cache_hit_rate) = attrs.provider_cache_hit_rate {
        span.set_attribute("moa.cache.hit_rate", provider_cache_hit_rate);
    }
}

/// Builds the exported span name for an LLM completion call.
pub(crate) fn llm_span_name(operation: &str, model: &str) -> String {
    format!("{operation} {model}")
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
    cache_write_tokens: usize,
    output_tokens: usize,
    pricing: &TokenPricing,
) -> f64 {
    let cached_input_tokens = cached_input_tokens.min(input_tokens);
    let cache_write_tokens =
        cache_write_tokens.min(input_tokens.saturating_sub(cached_input_tokens));
    let uncached_input_tokens = input_tokens
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_write_tokens);
    let cached_input_rate = pricing
        .cached_input_per_mtok
        .unwrap_or(pricing.input_per_mtok);
    let cache_write_rate = pricing.cache_write_per_mtok();

    calculate_cost(uncached_input_tokens, output_tokens, pricing)
        + ((cached_input_tokens as f64 * cached_input_rate) / 1_000_000.0)
        + ((cache_write_tokens as f64 * cache_write_rate) / 1_000_000.0)
}

fn has_meaningful_output(block: &CompletionContent) -> bool {
    match block {
        CompletionContent::Text(text) => !text.is_empty(),
        CompletionContent::ToolCall(_) => true,
        CompletionContent::ProviderToolResult { .. } => true,
    }
}

fn metadata_string(request: &CompletionRequest, key: &str) -> Option<String> {
    request
        .metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use moa_core::types::model::TokenPricing;

    use super::{calculate_cost, calculate_cost_with_cached, llm_span_name};

    #[test]
    fn llm_span_name_format() {
        assert_eq!(
            llm_span_name("chat", "claude-sonnet-4-6"),
            "chat claude-sonnet-4-6"
        );
    }

    #[test]
    fn cost_calculation_correct() {
        let pricing = TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.30),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        };

        let cost = calculate_cost(1_000, 500, &pricing);
        assert!((cost - 0.0105).abs() < 1e-10);
    }

    #[test]
    fn cached_cost_calculation_uses_cache_write_rate() {
        // Pins: Anthropic-style cache creation is charged above the base input rate.
        let pricing = TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.30),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: Some(6.0),
        };

        let cost = calculate_cost_with_cached(1_000, 200, 300, 100, &pricing);

        assert!((cost - 0.004185).abs() < 1e-10);
    }
}

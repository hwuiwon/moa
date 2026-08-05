//! Shared tracing helpers for provider-level LLM completion spans.

use moa_core::{
    error::MoaError, types::completion::CompletionContent, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::StopReason,
    types::completion::TokenUsage, types::model::TokenPricing,
    types::observability::genai_operation_name, types::observability::genai_provider_name,
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
    /// Bounded GenAI finish-reason label for the completion (`stop`, `length`,
    /// `tool_calls`, `content_filter`, `cancelled`, or `error`/`other`).
    pub finish_reasons: Option<&'static str>,
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
                finish_reasons: Some(finish_reason_label(&response.stop_reason)),
                ..LLMSpanAttributes::default()
            },
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
    if let Some(finish_reason) = attrs.finish_reasons {
        span.set_attribute("gen_ai.response.finish_reasons", finish_reason);
    }
}

/// Maps a provider [`StopReason`] to the bounded `gen_ai.response.finish_reasons`
/// label. Provider-specific `Other(_)` values are bucketed into a small,
/// bounded-cardinality set rather than passed through verbatim, since a raw
/// provider string is not guaranteed to be low-cardinality.
fn finish_reason_label(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::Cancelled => "cancelled",
        StopReason::Other(raw) => classify_other_stop_reason(raw),
    }
}

/// Buckets a provider-specific stop-reason string into a bounded label.
fn classify_other_stop_reason(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    if [
        "safety",
        "recitation",
        "prohibited",
        "blocklist",
        "spii",
        "content_filter",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "content_filter"
    } else if lower.contains("fail") {
        "error"
    } else {
        "other"
    }
}

/// Builds a `GenAI` embeddings span for one embedding provider round trip.
///
/// The span carries only bounded-cardinality attribute values (provider name,
/// model id, and integer counts) — no raw input text.
pub(crate) fn embedding_span(provider: &'static str, model: &str, input_count: usize) -> Span {
    let span_name = format!("embeddings {model}");
    let span = tracing::info_span!("embeddings", otel.name = %span_name, otel.kind = "client");
    span.set_attribute("gen_ai.operation.name", "embeddings");
    span.set_attribute("gen_ai.provider.name", provider);
    span.set_attribute("gen_ai.request.model", model.to_string());
    span.set_attribute("moa.embedding.input_count", input_count as i64);
    span
}

/// Records success usage/cost attributes on an embeddings span. Only called
/// when the provider response actually parsed a token count — cost is never
/// fabricated from an input/document count alone.
pub(crate) fn finish_embedding_span(span: &Span, model: &str, input_tokens: usize) {
    span.set_attribute("gen_ai.usage.input_tokens", input_tokens as i64);
    if let Some(price) = super::models::embedding_price_per_mtok(model).filter(|price| *price > 0.0)
    {
        let cost = (input_tokens as f64 * price) / 1_000_000.0;
        span.set_attribute("moa.embedding.cost_usd", cost);
    }
}

/// Builds a `GenAI` rerank span for one rerank provider round trip.
pub(crate) fn rerank_span(provider: &'static str, model: &str, document_count: usize) -> Span {
    let span_name = format!("rerank {model}");
    let span = tracing::info_span!("rerank", otel.name = %span_name, otel.kind = "client");
    span.set_attribute("gen_ai.operation.name", "rerank");
    span.set_attribute("gen_ai.provider.name", provider);
    span.set_attribute("gen_ai.request.model", model.to_string());
    span.set_attribute("moa.rerank.document_count", document_count as i64);
    span
}

/// Records authoritative usage and cost on a successful rerank span when the
/// model and required billing inputs are known.
pub(crate) fn finish_rerank_span(span: &Span, model: &str, input_tokens: Option<usize>) {
    match super::models::rerank_billing(model) {
        Some(super::models::RerankBilling::PerThousandSearches(price)) if price > 0.0 => {
            span.set_attribute("moa.rerank.cost_usd", price / 1_000.0);
        }
        Some(super::models::RerankBilling::PerMillionTokens(price)) => {
            if let Some(tokens) = input_tokens {
                span.set_attribute("gen_ai.usage.input_tokens", tokens as i64);
                if price > 0.0 {
                    let cost = (tokens as f64 * price) / 1_000_000.0;
                    span.set_attribute("moa.rerank.cost_usd", cost);
                }
            }
        }
        _ => {}
    }
}

/// Marks a provider span as failed with a bounded error class, mirroring
/// [`LLMSpanRecorder::fail_at_stage`] for the non-streaming embedding/rerank
/// call sites that don't have a multi-phase recorder.
pub(crate) fn fail_provider_span(span: &Span, class: &'static str, error: &impl std::fmt::Display) {
    span.set_status(Status::error(error.to_string()));
    span.set_attribute("error.type", class);
}

/// Classifies a provider error into a small, bounded `error.type` label
/// suitable for a span attribute (never the raw error message, which may be
/// unbounded or carry provider-specific detail).
pub(crate) fn provider_error_class(error: &MoaError) -> &'static str {
    match error {
        MoaError::RateLimited { .. } => "rate_limited",
        MoaError::HttpStatus { status, .. } if *status >= 500 => "http_5xx",
        MoaError::HttpStatus { .. } => "http_4xx",
        MoaError::SerializationError(_) => "serialization_error",
        MoaError::ProviderQuirk(_) => "provider_quirk",
        MoaError::ProviderTransport(_) => "transport_error",
        MoaError::ProviderTimeout(_) => "timeout",
        _ => "provider_error",
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
    use moa_core::error::MoaError;
    use moa_core::types::completion::StopReason;
    use moa_core::types::model::TokenPricing;

    use crate::core::span_capture_test_support::{
        attr_f64, attr_i64, attr_string, capture_spans, find_span,
    };

    use super::{
        calculate_cost, calculate_cost_with_cached, embedding_span, fail_provider_span,
        finish_embedding_span, finish_reason_label, finish_rerank_span, llm_span_name,
        provider_error_class, rerank_span,
    };

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

    #[test]
    fn finish_reason_label_maps_known_stop_reasons() {
        // Pins: the well-known StopReason variants map to the exact GenAI
        // semantic-convention finish-reason labels.
        assert_eq!(finish_reason_label(&StopReason::EndTurn), "stop");
        assert_eq!(finish_reason_label(&StopReason::MaxTokens), "length");
        assert_eq!(finish_reason_label(&StopReason::ToolUse), "tool_calls");
        assert_eq!(finish_reason_label(&StopReason::Cancelled), "cancelled");
    }

    #[test]
    fn finish_reason_label_buckets_raw_provider_strings_into_bounded_classes() {
        // Pins: an arbitrary provider-specific stop reason (e.g. Gemini's raw
        // SAFETY/RECITATION finish reasons) is bucketed into a small, bounded
        // set instead of passed through verbatim as an unbounded span value.
        assert_eq!(
            finish_reason_label(&StopReason::Other("SAFETY".to_string())),
            "content_filter"
        );
        assert_eq!(
            finish_reason_label(&StopReason::Other("RECITATION".to_string())),
            "content_filter"
        );
        assert_eq!(
            finish_reason_label(&StopReason::Other("failed".to_string())),
            "error"
        );
        assert_eq!(
            finish_reason_label(&StopReason::Other("LANGUAGE".to_string())),
            "other"
        );
    }

    #[test]
    fn provider_error_class_is_bounded() {
        // Pins: every MoaError variant a provider call can return maps to one
        // of a small, fixed set of `error.type` span labels.
        assert_eq!(
            provider_error_class(&MoaError::RateLimited {
                retries: 3,
                message: "slow down".to_string()
            }),
            "rate_limited"
        );
        assert_eq!(
            provider_error_class(&MoaError::HttpStatus {
                status: 503,
                retry_after: None,
                message: "unavailable".to_string()
            }),
            "http_5xx"
        );
        assert_eq!(
            provider_error_class(&MoaError::HttpStatus {
                status: 401,
                retry_after: None,
                message: "bad key".to_string()
            }),
            "http_4xx"
        );
        assert_eq!(
            provider_error_class(&MoaError::SerializationError("bad json".to_string())),
            "serialization_error"
        );
        assert_eq!(
            provider_error_class(&MoaError::ProviderError("boom".to_string())),
            "provider_error"
        );
        assert_eq!(
            provider_error_class(&MoaError::ProviderTransport("reset".to_string())),
            "transport_error"
        );
        assert_eq!(
            provider_error_class(&MoaError::ProviderTimeout("deadline".to_string())),
            "timeout"
        );
    }

    #[test]
    fn embedding_span_records_bounded_attributes_and_token_scaled_cost() {
        // Pins: the embeddings span carries the GenAI provider/model/operation
        // fields, the input count, and — only once a token count is known —
        // usage tokens and a cost computed from the dedicated embedding
        // pricing catalog (text-embedding-3-small is $0.02/Mtok).
        let spans = capture_spans(|| {
            let span = embedding_span("openai", "text-embedding-3-small", 3);
            finish_embedding_span(&span, "text-embedding-3-small", 1_000);
        });

        let span = find_span(&spans, "embeddings text-embedding-3-small");
        assert_eq!(
            attr_string(span, "gen_ai.operation.name").as_deref(),
            Some("embeddings")
        );
        assert_eq!(
            attr_string(span, "gen_ai.provider.name").as_deref(),
            Some("openai")
        );
        assert_eq!(
            attr_string(span, "gen_ai.request.model").as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(attr_i64(span, "moa.embedding.input_count"), Some(3));
        assert_eq!(attr_i64(span, "gen_ai.usage.input_tokens"), Some(1_000));
        let cost = attr_f64(span, "moa.embedding.cost_usd")
            .expect("cost should be set once a token count and a non-zero price are known");
        assert!((cost - 0.00002).abs() < 1e-12);
    }

    #[test]
    fn embedding_span_omits_cost_when_price_is_zero() {
        // Pins: a catalogued-but-unverified embedding price (0.0) never
        // fabricates a cost attribute, even once a token count is known.
        let spans = capture_spans(|| {
            let span = embedding_span("zeroentropy", "not-a-priced-model", 1);
            finish_embedding_span(&span, "not-a-priced-model", 500);
        });

        let span = find_span(&spans, "embeddings not-a-priced-model");
        assert_eq!(attr_i64(span, "gen_ai.usage.input_tokens"), Some(500));
        assert_eq!(attr_f64(span, "moa.embedding.cost_usd"), None);
    }

    #[test]
    fn rerank_span_records_bounded_attributes_and_flat_per_call_cost() {
        // Pins: the rerank span carries document_count (not the raw documents)
        // and a flat per-call cost from the dedicated rerank pricing catalog
        // (rerank-v4.0-fast is $2.00/1K searches -> $0.002/call).
        let spans = capture_spans(|| {
            let span = rerank_span("cohere", "rerank-v4.0-fast", 12);
            finish_rerank_span(&span, "rerank-v4.0-fast", None);
        });

        let span = find_span(&spans, "rerank rerank-v4.0-fast");
        assert_eq!(
            attr_string(span, "gen_ai.operation.name").as_deref(),
            Some("rerank")
        );
        assert_eq!(
            attr_string(span, "gen_ai.provider.name").as_deref(),
            Some("cohere")
        );
        assert_eq!(attr_i64(span, "moa.rerank.document_count"), Some(12));
        let cost = attr_f64(span, "moa.rerank.cost_usd").expect("cost should be set");
        assert!((cost - 0.002).abs() < 1e-12);
    }

    #[test]
    fn rerank_span_records_authoritative_token_usage_and_exact_cost() {
        // Pins: zerank-2 uses the provider's authoritative token count and its
        // $0.025/1M-token catalog price; document count is never a proxy.
        let spans = capture_spans(|| {
            let span = rerank_span("zeroentropy", "zerank-2", 5);
            finish_rerank_span(&span, "zerank-2", Some(400));
        });

        let span = find_span(&spans, "rerank zerank-2");
        assert_eq!(attr_i64(span, "gen_ai.usage.input_tokens"), Some(400));
        let cost = attr_f64(span, "moa.rerank.cost_usd").expect("cost should be set");
        assert!((cost - 0.00001).abs() < 1e-12);
    }

    #[test]
    fn rerank_span_omits_token_cost_without_authoritative_usage() {
        // Pins: token-billed rerank models fail closed when the provider does
        // not supply usage; document count is never used as an estimate.
        let spans = capture_spans(|| {
            let span = rerank_span("zeroentropy", "zerank-2", 5);
            finish_rerank_span(&span, "zerank-2", None);
        });

        let span = find_span(&spans, "rerank zerank-2");
        assert_eq!(attr_i64(span, "gen_ai.usage.input_tokens"), None);
        assert_eq!(attr_f64(span, "moa.rerank.cost_usd"), None);
    }

    #[test]
    fn fail_provider_span_sets_status_and_bounded_error_type() {
        // Pins: a failed provider span records both an OTel error status and a
        // bounded `error.type`, matching what a collector needs to alert on
        // provider failure classes without parsing raw error text.
        let spans = capture_spans(|| {
            let span = embedding_span("cohere", "embed-v4.0", 2);
            let error = MoaError::HttpStatus {
                status: 429,
                retry_after: None,
                message: "rate limited".to_string(),
            };
            fail_provider_span(&span, provider_error_class(&error), &error);
        });

        let span = find_span(&spans, "embeddings embed-v4.0");
        assert_eq!(attr_string(span, "error.type").as_deref(), Some("http_4xx"));
        assert_eq!(
            span.status,
            opentelemetry::trace::Status::error("http status 429: rate limited")
        );
    }
}

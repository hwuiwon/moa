//! Observability and trace context helpers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    channel::Channel, completion::CompletionRequest, contact::ContactId, contact::SessionActorRef,
    context::MessageRole, context::estimate_text_tokens, context::sum_message_tokens,
    identifiers::ModelId, identifiers::SessionId, identifiers::TenantId, session::SessionMeta,
};

/// Durable summary of one provider request's cache plan and observed cache usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheReport {
    /// Provider identifier, for example `anthropic` or `openai`.
    pub provider: String,
    /// Model identifier used for the request.
    pub model: ModelId,
    /// Number of context messages sent to the provider.
    pub message_count: usize,
    /// Number of tool schemas sent to the provider.
    pub tool_count: usize,
    /// Estimated tokens contributed by tool schemas.
    pub tool_tokens_estimate: usize,
    /// Estimated tokens contributed by stable-prefix messages.
    pub stable_message_tokens_estimate: usize,
    /// Estimated tokens in the stable prefix, including tools.
    pub stable_total_tokens_estimate: usize,
    /// Estimated total request tokens, including tools and dynamic messages.
    pub total_tokens_estimate: usize,
    /// Estimated dynamic suffix tokens outside the stable prefix.
    pub dynamic_tokens_estimate: usize,
    /// Estimated stable-prefix ratio within the full request.
    pub cache_ratio_estimate: f64,
    /// Serialized JSON byte size of the stable provider-request prefix.
    #[serde(default)]
    pub stable_prefix_bytes: usize,
    /// Stable fingerprint of the cacheable prompt prefix.
    pub stable_prefix_fingerprint: u64,
    /// Stable fingerprint of the full request payload.
    pub full_request_fingerprint: u64,
    /// Estimated tokens through the frozen-history boundary (tools + system
    /// prefix + replayed history), when the request carries one.
    #[serde(default)]
    pub frozen_history_tokens_estimate: usize,
    /// Fingerprint of the request through the frozen-history boundary, when
    /// the request carries one. Equal values across turns mean the provider
    /// can cache-read the whole replayed-history span.
    #[serde(default)]
    pub frozen_history_fingerprint: u64,
    /// Whether the previous request in the same session reused the same stable prefix.
    pub stable_prefix_reused: bool,
    /// Provider-reported prompt input tokens.
    pub input_tokens: usize,
    /// Provider-reported cached input tokens.
    pub cached_input_tokens: usize,
    /// Provider-reported output tokens.
    pub output_tokens: usize,
    /// Ratio of cached provider tokens vs. the estimated stable prefix.
    pub cached_vs_stable_estimate_ratio: f64,
}

impl Default for CacheReport {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: ModelId::new(""),
            message_count: 0,
            tool_count: 0,
            tool_tokens_estimate: 0,
            stable_message_tokens_estimate: 0,
            stable_total_tokens_estimate: 0,
            total_tokens_estimate: 0,
            dynamic_tokens_estimate: 0,
            cache_ratio_estimate: 0.0,
            stable_prefix_bytes: 0,
            stable_prefix_fingerprint: 0,
            full_request_fingerprint: 0,
            frozen_history_tokens_estimate: 0,
            frozen_history_fingerprint: 0,
            stable_prefix_reused: false,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            cached_vs_stable_estimate_ratio: 0.0,
        }
    }
}

impl CacheReport {
    /// Builds a cache report from one completion request and its provider response metrics.
    pub fn from_request(
        request: &CompletionRequest,
        provider: impl Into<String>,
        model: impl Into<ModelId>,
        stable_prefix_reused: bool,
        input_tokens: usize,
        cached_input_tokens: usize,
        output_tokens: usize,
    ) -> Self {
        let stable_message_count = stable_prefix_message_count(request);
        let tool_tokens_estimate = request
            .tools
            .iter()
            .map(|tool| estimate_text_tokens(&tool.to_string()))
            .sum::<usize>();
        let stable_message_tokens_estimate =
            sum_message_tokens(&request.messages[..stable_message_count]);
        let total_message_tokens_estimate = sum_message_tokens(&request.messages);
        let stable_total_tokens_estimate = tool_tokens_estimate + stable_message_tokens_estimate;
        let total_tokens_estimate = tool_tokens_estimate + total_message_tokens_estimate;
        let dynamic_tokens_estimate =
            total_tokens_estimate.saturating_sub(stable_total_tokens_estimate);
        let cache_ratio_estimate = if total_tokens_estimate == 0 {
            0.0
        } else {
            stable_total_tokens_estimate as f64 / total_tokens_estimate as f64
        };
        let cached_vs_stable_estimate_ratio = if stable_total_tokens_estimate == 0 {
            0.0
        } else {
            cached_input_tokens as f64 / stable_total_tokens_estimate as f64
        };
        let frozen_history_end = frozen_history_end(request);
        let frozen_history_tokens_estimate = frozen_history_end
            .map(|end| tool_tokens_estimate + sum_message_tokens(&request.messages[..end]))
            .unwrap_or(0);
        let frozen_history_fingerprint = frozen_history_end
            .map(|end| fingerprint_json(&(&request.tools, &request.messages[..end])))
            .unwrap_or(0);

        Self {
            provider: provider.into(),
            model: model.into(),
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
            tool_tokens_estimate,
            stable_message_tokens_estimate,
            stable_total_tokens_estimate,
            total_tokens_estimate,
            dynamic_tokens_estimate,
            cache_ratio_estimate,
            stable_prefix_bytes: stable_prefix_byte_len(request),
            stable_prefix_fingerprint: stable_prefix_fingerprint(request),
            full_request_fingerprint: full_request_fingerprint(request),
            stable_prefix_reused,
            frozen_history_tokens_estimate,
            frozen_history_fingerprint,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cached_vs_stable_estimate_ratio,
        }
    }
}

/// Returns a stable fingerprint for the cacheable prefix of a completion request.
pub fn stable_prefix_fingerprint(request: &CompletionRequest) -> u64 {
    let stable_message_count = stable_prefix_message_count(request);
    fingerprint_json(&(&request.tools, &request.messages[..stable_message_count]))
}

/// Returns the frozen-history end index carried in request metadata, clamped
/// to the message list, when the context pipeline provided one.
fn frozen_history_end(request: &CompletionRequest) -> Option<usize> {
    request
        .metadata
        .get(super::completion::STABLE_HISTORY_END_METADATA_KEY)
        .and_then(Value::as_u64)
        .map(|end| (end as usize).min(request.messages.len()))
}

/// Returns a stable fingerprint for the full completion request payload.
pub fn full_request_fingerprint(request: &CompletionRequest) -> u64 {
    fingerprint_json(&(&request.tools, &request.messages))
}

fn fingerprint_json<T>(value: &T) -> u64
where
    T: Serialize,
{
    let Ok(value) = serde_json::to_value(value) else {
        return 0;
    };
    let Ok(serialized) = crate::canonical_json::canonical_json_bytes(&value) else {
        return 0;
    };
    let Ok(serialized) = String::from_utf8(serialized) else {
        return 0;
    };
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

fn stable_prefix_byte_len(request: &CompletionRequest) -> usize {
    let stable_message_count = stable_prefix_message_count(request);
    serde_json::to_vec(&(&request.tools, &request.messages[..stable_message_count]))
        .map(|bytes| bytes.len())
        .unwrap_or_default()
}

fn stable_prefix_message_count(request: &CompletionRequest) -> usize {
    request
        .messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count()
}

/// Context attributes propagated across spans in one logical turn trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// Session identifier used for trace grouping.
    pub session_id: SessionId,
    /// Tenant runtime boundary for filterable metadata.
    pub tenant_id: TenantId,
    /// Contact identifier for agent-facing contact sessions.
    pub contact_id: Option<ContactId>,
    /// Contact verification state for agent-facing contact sessions.
    pub contact_state: Option<String>,
    /// Actor that created the session, when recorded.
    pub created_by: Option<SessionActorRef>,
    /// Optional originating channel.
    pub channel: Option<Channel>,
    /// Active model identifier.
    pub model: ModelId,
    /// Human-readable trace name derived from the user prompt.
    pub trace_name: Option<String>,
    /// Optional deployment environment.
    pub environment: Option<String>,
}

impl TraceContext {
    /// Builds a trace context from persisted session metadata and the current user prompt.
    pub fn from_session_meta(session: &SessionMeta, prompt: Option<&str>) -> Self {
        Self {
            session_id: session.id,
            tenant_id: session.tenant_id,
            contact_id: session.contact.as_ref().map(|contact| contact.contact_id),
            contact_state: session
                .contact
                .as_ref()
                .map(|contact| contact.state.as_str().to_string()),
            created_by: session.created_by.clone(),
            channel: Some(session.channel),
            model: session.model.clone(),
            trace_name: prompt.map(trace_name_from_message),
            environment: None,
        }
    }

    /// Returns a clone of the trace context with an explicit environment override.
    #[must_use]
    pub fn with_environment(mut self, environment: Option<String>) -> Self {
        self.environment = environment
            .as_deref()
            .map(normalize_environment)
            .filter(|value| !value.is_empty());
        self
    }
}

/// Builds a human-readable trace name from the first line of a user-authored message.
pub fn trace_name_from_message(message: &str) -> String {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return "MOA turn".to_string();
    }

    truncate_with_ellipsis(trimmed.lines().next().unwrap_or(trimmed), 200)
}

/// Returns the OpenTelemetry GenAI provider name for a MOA provider key.
#[must_use]
pub fn genai_provider_name(provider: &str) -> &str {
    match provider {
        "google" | "gemini" => "gcp.gemini",
        other => other,
    }
}

/// Returns the OpenTelemetry GenAI operation name for a MOA provider key.
#[must_use]
pub fn genai_operation_name(provider: &str) -> &'static str {
    match genai_provider_name(provider) {
        "gcp.gemini" => "generate_content",
        _ => "chat",
    }
}

/// Truncates a string to the provided character limit with an ellipsis suffix.
pub fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    let keep = max_chars.saturating_sub(3);
    let truncated = value.chars().take(keep).collect::<String>();
    format!("{truncated}...")
}

/// Normalizes an environment label for trace attributes.
pub(crate) fn normalize_environment(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '-' | '_' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();

    truncate_with_ellipsis(&normalized, 40)
}

#[cfg(test)]
mod tests {
    use super::{
        TraceContext, full_request_fingerprint, stable_prefix_fingerprint, trace_name_from_message,
    };
    use crate::types::{
        channel::Channel, completion::CompletionRequest, context::ContextMessage,
        identifiers::SessionId, identifiers::TenantId, session::SessionMeta,
    };
    use serde_json::json;

    #[test]
    fn trace_name_truncates_at_200_chars() {
        let name = trace_name_from_message(&"a".repeat(300));
        assert!(name.len() <= 200);
        assert!(name.ends_with("..."));
    }

    #[test]
    fn trace_context_from_session_meta() {
        let meta = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::from(uuid::Uuid::from_u128(2)),
            channel: Channel::Slack,
            model: "claude-sonnet-4-20250514".into(),
            ..SessionMeta::default()
        };
        let ctx = TraceContext::from_session_meta(&meta, Some("Fix OAuth bug"));
        assert_eq!(ctx.trace_name.as_deref(), Some("Fix OAuth bug"));
        assert_eq!(ctx.tenant_id, TenantId::from(uuid::Uuid::from_u128(2)));
    }

    #[test]
    fn completion_request_fingerprints_are_json_object_order_insensitive() {
        // Pins: stable-prefix fingerprints canonicalize tool-schema object key order.
        let first = CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("Static instructions"),
                ContextMessage::user("Dynamic task A"),
            ],
            tools: vec![json!({
                "name": "search",
                "description": "Search indexed files.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query"
                        }
                    },
                    "required": ["query"]
                }
            })],
            max_output_tokens: Some(128),
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata: Default::default(),
        };
        let second = CompletionRequest {
            tools: vec![json!({
                "input_schema": {
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "description": "Search query",
                            "type": "string"
                        }
                    },
                    "type": "object"
                },
                "description": "Search indexed files.",
                "name": "search"
            })],
            ..first.clone()
        };

        assert_eq!(
            stable_prefix_fingerprint(&first),
            stable_prefix_fingerprint(&second),
            "stable prefix fingerprint should ignore JSON object key insertion order"
        );
        assert_eq!(
            full_request_fingerprint(&first),
            full_request_fingerprint(&second),
            "full request fingerprint should ignore JSON object key insertion order"
        );
    }
}

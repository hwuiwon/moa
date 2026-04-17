//! Observability and trace context helpers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tracing_opentelemetry::OpenTelemetrySpanExt;

use serde::{Deserialize, Serialize};

use super::{
    CacheTtl, CompletionRequest, ModelId, Platform, SessionId, SessionMeta, UserId, WorkspaceId,
    estimate_text_tokens,
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
    /// Explicit cache breakpoint indexes included in the request.
    pub cache_breakpoints: Vec<usize>,
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
    /// Stable fingerprint of the cacheable prompt prefix.
    pub stable_prefix_fingerprint: u64,
    /// Stable fingerprint of the full request payload.
    pub full_request_fingerprint: u64,
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
            cache_breakpoints: Vec::new(),
            tool_tokens_estimate: 0,
            stable_message_tokens_estimate: 0,
            stable_total_tokens_estimate: 0,
            total_tokens_estimate: 0,
            dynamic_tokens_estimate: 0,
            cache_ratio_estimate: 0.0,
            stable_prefix_fingerprint: 0,
            full_request_fingerprint: 0,
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
        let stable_message_tokens_estimate = request.messages[..stable_message_count]
            .iter()
            .map(|message| estimate_text_tokens(&message.content))
            .sum::<usize>();
        let total_message_tokens_estimate = request
            .messages
            .iter()
            .map(|message| estimate_text_tokens(&message.content))
            .sum::<usize>();
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

        Self {
            provider: provider.into(),
            model: model.into(),
            message_count: request.messages.len(),
            tool_count: request.tools.len(),
            cache_breakpoints: request.cache_breakpoints.clone(),
            tool_tokens_estimate,
            stable_message_tokens_estimate,
            stable_total_tokens_estimate,
            total_tokens_estimate,
            dynamic_tokens_estimate,
            cache_ratio_estimate,
            stable_prefix_fingerprint: stable_prefix_fingerprint(request),
            full_request_fingerprint: full_request_fingerprint(request),
            stable_prefix_reused,
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

/// Returns a stable fingerprint for the full completion request payload.
pub fn full_request_fingerprint(request: &CompletionRequest) -> u64 {
    fingerprint_json(&(&request.tools, &request.messages))
}

fn fingerprint_json<T>(value: &T) -> u64
where
    T: Serialize,
{
    let serialized = serde_json::to_string(value).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

fn stable_prefix_message_count(request: &CompletionRequest) -> usize {
    request
        .cache_controls
        .iter()
        .filter(|breakpoint| breakpoint.ttl == CacheTtl::OneHour)
        .filter_map(super::completion::CacheBreakpoint::message_index)
        .max()
        .or_else(|| request.cache_breakpoints.last().copied())
        .unwrap_or_default()
        .min(request.messages.len())
}

/// Context attributes propagated across spans in one logical turn trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// Session identifier used for Langfuse session grouping.
    pub session_id: SessionId,
    /// User identifier used for per-user analytics.
    pub user_id: UserId,
    /// Workspace identifier for filterable metadata.
    pub workspace_id: WorkspaceId,
    /// Optional originating platform.
    pub platform: Option<Platform>,
    /// Active model identifier.
    pub model: ModelId,
    /// Human-readable trace name derived from the user prompt.
    pub trace_name: Option<String>,
    /// Filterable Langfuse tags serialized on the root span.
    pub tags: Vec<String>,
    /// Optional deployment environment.
    pub environment: Option<String>,
}

impl TraceContext {
    /// Builds a trace context from persisted session metadata and the current user prompt.
    pub fn from_session_meta(session: &SessionMeta, prompt: Option<&str>) -> Self {
        Self {
            session_id: session.id,
            user_id: session.user_id.clone(),
            workspace_id: session.workspace_id.clone(),
            platform: Some(session.platform.clone()),
            model: session.model.clone(),
            trace_name: prompt.map(trace_name_from_message),
            tags: generate_trace_tags(Some(&session.platform), &session.workspace_id),
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

    /// Sets Langfuse and MOA span attributes on the provided tracing span.
    pub fn apply_to_span(&self, span: &tracing::Span) {
        span.set_attribute(
            "langfuse.session.id",
            sanitize_langfuse_id(&self.session_id.to_string()),
        );
        span.set_attribute(
            "langfuse.user.id",
            sanitize_langfuse_id(self.user_id.as_str()),
        );
        span.set_attribute(
            "langfuse.trace.metadata.workspace_id",
            self.workspace_id.to_string(),
        );
        let model = self.model.to_string();
        span.set_attribute("langfuse.trace.metadata.model", model.clone());
        span.set_attribute("moa.session.id", self.session_id.to_string());
        span.set_attribute("moa.user.id", self.user_id.to_string());
        span.set_attribute("moa.workspace.id", self.workspace_id.to_string());
        span.set_attribute("moa.model", model);

        if let Some(platform) = self.platform.as_ref() {
            let value = platform.to_string();
            span.set_attribute("langfuse.trace.metadata.platform", value.clone());
            span.set_attribute("moa.platform", value);
        }

        if let Some(trace_name) = self.trace_name.as_ref() {
            span.set_attribute("langfuse.trace.name", trace_name.clone());
        }

        if !self.tags.is_empty()
            && let Ok(tags) = serde_json::to_string(&self.tags)
        {
            span.set_attribute("langfuse.trace.tags", tags);
        }

        if let Some(environment) = self.environment.as_ref() {
            span.set_attribute("langfuse.environment", environment.clone());
        }
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

/// Derives Langfuse tags from platform and workspace identifiers.
pub fn generate_trace_tags(platform: Option<&Platform>, workspace_id: &WorkspaceId) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(platform) = platform {
        tags.push(truncate_with_ellipsis(&platform.to_string(), 200));
    }
    tags.push(truncate_with_ellipsis(
        &format!("workspace:{workspace_id}"),
        200,
    ));
    tags
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

/// Replaces non-ASCII characters and enforces Langfuse identifier length limits.
pub fn sanitize_langfuse_id(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '_' })
        .collect::<String>();
    truncate_with_ellipsis(&sanitized, 200)
}

/// Normalizes an environment label for Langfuse compatibility.
pub fn normalize_environment(value: &str) -> String {
    let mut normalized = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '-' | '_' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();

    if normalized.starts_with("langfuse") {
        normalized = format!("env-{normalized}");
    }

    truncate_with_ellipsis(&normalized, 40)
}

#[cfg(test)]
mod tests {
    use super::{TraceContext, generate_trace_tags, trace_name_from_message};
    use crate::types::{Platform, SessionId, SessionMeta, UserId, WorkspaceId};

    #[test]
    fn trace_name_truncates_at_200_chars() {
        let name = trace_name_from_message(&"a".repeat(300));
        assert!(name.len() <= 200);
        assert!(name.ends_with("..."));
    }

    #[test]
    fn tags_include_platform_and_workspace() {
        let tags = generate_trace_tags(Some(&Platform::Slack), &WorkspaceId::new("myproject"));
        assert!(tags.contains(&"slack".to_string()));
        assert!(tags.contains(&"workspace:myproject".to_string()));
    }

    #[test]
    fn trace_context_from_session_meta() {
        let meta = SessionMeta {
            id: SessionId::new(),
            user_id: UserId::new("user-456"),
            workspace_id: WorkspaceId::new("webapp"),
            platform: Platform::Telegram,
            model: "claude-sonnet-4-20250514".into(),
            ..SessionMeta::default()
        };
        let ctx = TraceContext::from_session_meta(&meta, Some("Fix OAuth bug"));
        assert_eq!(ctx.trace_name.as_deref(), Some("Fix OAuth bug"));
        assert!(ctx.tags.contains(&"telegram".to_string()));
        assert_eq!(ctx.workspace_id, WorkspaceId::new("webapp"));
    }
}

//! Observability and trace context helpers.

use tracing_opentelemetry::OpenTelemetrySpanExt;

use serde::{Deserialize, Serialize};

use super::{Platform, SessionId, SessionMeta, UserId, WorkspaceId};

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
    pub model: String,
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
            session_id: session.id.clone(),
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
        span.set_attribute("langfuse.trace.metadata.model", self.model.clone());
        span.set_attribute("moa.session.id", self.session_id.to_string());
        span.set_attribute("moa.user.id", self.user_id.to_string());
        span.set_attribute("moa.workspace.id", self.workspace_id.to_string());
        span.set_attribute("moa.model", self.model.clone());

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

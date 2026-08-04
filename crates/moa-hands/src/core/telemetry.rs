//! Tracing helpers for tool execution spans and result metadata.

use std::time::Duration;

use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionPolicyEffect,
    types::completion::ToolInvocation, types::hands::SandboxTier,
    types::observability::TraceContext, types::session::SessionMeta,
    types::tools::SecuredToolOutput,
};
use moa_observability::{apply_trace_context_to_span, current_turn_root_span, record_tool_call};
use opentelemetry::trace::Status;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use super::ToolExecution;

const TOOL_ERROR_OUTPUT_STATUS: &str = "tool returned error output";
const TOOL_EXECUTION_FAILED_STATUS: &str = "tool execution failed";

pub(super) fn tool_execution_span(
    session: &SessionMeta,
    invocation: &ToolInvocation,
) -> tracing::Span {
    let span_name = format!("execute_tool {}", invocation.name);
    // `moa.session.id` is not declared here as a span field: it is already set
    // below via `apply_trace_context_to_span`, which is the shared path every
    // session-scoped span in this crate uses to attach session/tenant identity.
    let span = match current_turn_root_span() {
        Some(parent) => {
            tracing::info_span!(parent: &parent, "tool_execution", otel.name = %span_name)
        }
        None => tracing::info_span!("tool_execution", otel.name = %span_name),
    };
    let trace_context = TraceContext::from_session_meta(session, None);
    apply_trace_context_to_span(&trace_context, &span);
    span.set_attribute("gen_ai.tool.name", invocation.name.clone());
    if let Some(tool_call_id) = invocation.id.as_ref() {
        span.set_attribute("gen_ai.tool.call.id", tool_call_id.clone());
    }
    if let Ok(serialized_input) = serde_json::to_string(&invocation.input) {
        record_tool_input_fields(&span, &serialized_input, trace_tool_output_enabled());
    }
    span.set_attribute("moa.tool.denied", false);
    span
}

pub(super) fn record_tool_invocation_metadata(
    span: &tracing::Span,
    session: &SessionMeta,
    execution: &ToolExecution,
    effect: &ActionPolicyEffect,
) {
    let trace_context = TraceContext::from_session_meta(session, None);
    apply_trace_context_to_span(&trace_context, span);

    let (category, sandbox_tier) = match execution {
        ToolExecution::BuiltIn(_) => ("builtin", "none"),
        ToolExecution::Hand { routes } => (
            "hand",
            routes
                .first()
                .map(|route| sandbox_tier_label(&route.tier))
                .unwrap_or("unknown"),
        ),
        ToolExecution::Mcp { .. } => ("mcp", "external"),
        ToolExecution::InstalledConnectorAction { .. } => ("installed_connector", "external"),
    };

    span.set_attribute("moa.tool.category", category);
    span.set_attribute("moa.sandbox.tier", sandbox_tier);
    span.set_attribute(
        "moa.tool.action_review_required",
        matches!(effect, ActionPolicyEffect::AdminReview),
    );
}

/// Records the span outcome for one executed tool call.
///
/// It takes the [`SecuredToolOutput`], not a bare output, so the text that can
/// reach a trace exporter is by construction the post-classification text. The
/// assessment class and capability are recorded as closed-vocabulary attributes;
/// neither can carry attacker-controlled bytes or unbounded cardinality.
pub(super) fn record_tool_execution_result(
    span: &tracing::Span,
    tool_name: &str,
    duration: Duration,
    result: &Result<SecuredToolOutput>,
) {
    span.set_attribute("moa.tool.duration_ms", duration.as_millis() as i64);

    match result {
        Ok(secured) => {
            let output = &secured.safe_output;
            let succeeded = !output.is_error;
            span.set_attribute("moa.tool.success", succeeded);
            span.set_attribute("moa.tool.security.class", secured.assessment.class.as_str());
            span.set_attribute("moa.tool.security.capability", secured.capability.render());
            record_tool_output_fields(span, &output.to_text(), trace_tool_output_enabled());
            record_tool_call(
                tool_name,
                if succeeded { "success" } else { "error" },
                duration,
            );
            if output.is_error {
                span.set_status(Status::error(TOOL_ERROR_OUTPUT_STATUS));
            }
        }
        Err(MoaError::PermissionDenied(_)) => {
            span.set_attribute("moa.tool.success", false);
            record_tool_call(tool_name, "error", duration);
        }
        Err(MoaError::Cancelled) => {
            span.set_attribute("moa.tool.success", false);
            record_tool_call(tool_name, "error", duration);
        }
        Err(_) => {
            span.set_attribute("moa.tool.success", false);
            span.set_status(Status::error(TOOL_EXECUTION_FAILED_STATUS));
            record_tool_call(tool_name, "error", duration);
        }
    }
}

fn sandbox_tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

/// Env flag that opts execution spans back into carrying tool-output bodies.
const TRACE_TOOL_OUTPUT_ENV: &str = "MOA_TRACE_TOOL_OUTPUT";

/// Records the tool-output telemetry fields on `span`.
///
/// The output byte length and a short content hash are always recorded so spans
/// stay useful for correlation and size analysis. The (capped) body is attached
/// only when `attach_body` is set, so normal operation does not ship up to 8 KiB
/// of tenant tool output on every call.
fn record_tool_output_fields(span: &tracing::Span, output_text: &str, attach_body: bool) {
    span.set_attribute("moa.tool.output.bytes", output_text.len() as i64);
    span.set_attribute("moa.tool.output.hash", short_content_hash(output_text));
    if let Some(body) = tool_output_body_field(output_text, attach_body) {
        span.set_attribute("moa.tool.output", body);
    }
}

/// Records the tool-input telemetry fields on `span`.
///
/// Spans always carry input size and a stable short hash for correlation. The
/// raw serialized input body is attached only when body tracing is explicitly
/// enabled.
fn record_tool_input_fields(span: &tracing::Span, serialized_input: &str, attach_body: bool) {
    span.set_attribute("moa.tool.input.bytes", serialized_input.len() as i64);
    span.set_attribute("moa.tool.input.hash", short_content_hash(serialized_input));
    if let Some(body) = tool_body_field(serialized_input, attach_body) {
        span.set_attribute("moa.tool.input", body);
    }
}

/// Returns the capped tool-output body to attach, or `None` when body tracing is
/// disabled.
fn tool_output_body_field(output_text: &str, attach_body: bool) -> Option<String> {
    tool_body_field(output_text, attach_body)
}

fn tool_body_field(value: &str, attach_body: bool) -> Option<String> {
    attach_body.then(|| truncate_tool_span_text(value.to_string()))
}

/// Returns whether tool-output bodies should be attached to execution spans.
///
/// Off by default; enabled when [`TRACE_TOOL_OUTPUT_ENV`] is set to `1`/`true`.
/// Read per call so an operator (or test) can toggle it without a process restart.
fn trace_tool_output_enabled() -> bool {
    parse_trace_flag(std::env::var(TRACE_TOOL_OUTPUT_ENV).ok().as_deref())
}

/// Parses a `MOA_TRACE_TOOL_OUTPUT` value into an enabled/disabled decision.
fn parse_trace_flag(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1" | "true"))
}

/// Computes a short, stable hex digest of tool output for span correlation.
///
/// This is a non-cryptographic content fingerprint used only to correlate or
/// deduplicate outputs across spans, not a security primitive.
fn short_content_hash(value: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Caps text attached to telemetry while preserving UTF-8 character boundaries.
#[must_use]
pub fn truncate_tool_span_text(mut value: String) -> String {
    const LIMIT: usize = 8 * 1024;
    if value.len() <= LIMIT {
        return value;
    }

    let mut truncate_at = LIMIT;
    while !value.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    value.truncate(truncate_at);
    value.push('…');
    value
}

#[cfg(test)]
mod tests {
    use super::{
        parse_trace_flag, short_content_hash, tool_body_field, tool_output_body_field,
        truncate_tool_span_text,
    };

    #[test]
    fn trace_flag_only_enables_on_explicit_truthy_values() {
        // Pins: body tracing is opt-in; only "1"/"true" (trimmed) enable it, and a
        // missing or falsey value keeps bodies off.
        assert!(parse_trace_flag(Some("1")));
        assert!(parse_trace_flag(Some("true")));
        assert!(parse_trace_flag(Some(" 1 ")));
        assert!(!parse_trace_flag(Some("0")));
        assert!(!parse_trace_flag(Some("yes")));
        assert!(!parse_trace_flag(Some("")));
        assert!(!parse_trace_flag(None));
    }

    #[test]
    fn tool_output_body_is_gated_and_capped() {
        // Pins: the body is attached only when tracing is enabled, and even then it
        // stays within the 8 KiB span cap.
        assert_eq!(tool_output_body_field("hello", false), None);
        assert_eq!(
            tool_output_body_field("hello", true),
            Some("hello".to_string())
        );

        let large = "x".repeat(16 * 1024);
        let body = tool_output_body_field(&large, true).expect("enabled body should be present");
        assert!(body.len() <= 8 * 1024 + '…'.len_utf8());
        assert!(body.ends_with('…'));
    }

    #[test]
    fn tool_body_helper_is_shared_by_input_and_output_fields() {
        // Pins: raw body attachment uses one opt-in path for both tool input and
        // output so input telemetry cannot bypass the output tracing gate.
        assert_eq!(tool_body_field("secret", false), None);
        assert_eq!(tool_body_field("secret", true), Some("secret".to_string()));
    }

    #[test]
    fn telemetry_text_truncation_preserves_unicode_boundaries() {
        // Pins: bounded error and tool-body telemetry remains valid UTF-8 when
        // the byte cap falls inside a multi-byte character.
        let value = format!("{}🦀", "x".repeat(8 * 1024 - 1));

        let truncated = truncate_tool_span_text(value);

        assert_eq!(truncated, format!("{}…", "x".repeat(8 * 1024 - 1)));
        assert_eq!(truncated.len(), 8 * 1024 - 1 + '…'.len_utf8());
    }

    #[test]
    fn short_content_hash_is_stable_and_content_sensitive() {
        // Pins: the hash is deterministic per content (usable for correlation) and
        // distinguishes different outputs.
        assert_eq!(short_content_hash("same"), short_content_hash("same"));
        assert_ne!(short_content_hash("a"), short_content_hash("b"));
        assert_eq!(short_content_hash("a").len(), 16);
    }
}

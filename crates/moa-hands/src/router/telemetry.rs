//! Tracing helpers for tool execution spans and result metadata.

use std::time::Duration;

use moa_core::{
    MoaError, Result, SandboxTier, SessionMeta, ToolInvocation, ToolOutput, TraceContext,
    record_tool_call, record_tool_output_truncated_metric,
};
use opentelemetry::trace::Status;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use super::ToolExecution;

pub(super) fn tool_execution_span(
    session: &SessionMeta,
    invocation: &ToolInvocation,
) -> tracing::Span {
    let span_name = format!("execute_tool {}", invocation.name);
    let span = tracing::info_span!("tool_execution", otel.name = %span_name);
    TraceContext::from_session_meta(session, None).apply_to_span(&span);
    span.set_attribute("gen_ai.tool.name", invocation.name.clone());
    if let Some(tool_call_id) = invocation.id.as_ref() {
        span.set_attribute("gen_ai.tool.call.id", tool_call_id.clone());
    }
    if let Ok(serialized_input) = serde_json::to_string(&invocation.input) {
        span.set_attribute("moa.tool.input", truncate_tool_span_text(serialized_input));
    }
    span.set_attribute("moa.tool.denied", false);
    span
}

pub(super) fn record_tool_invocation_metadata(
    span: &tracing::Span,
    session: &SessionMeta,
    execution: &ToolExecution,
    action: &moa_core::PolicyAction,
) {
    TraceContext::from_session_meta(session, None).apply_to_span(span);

    let (category, sandbox_tier) = match execution {
        ToolExecution::BuiltIn(_) => ("builtin", "none"),
        ToolExecution::Hand { tier, .. } => ("hand", sandbox_tier_label(tier)),
        ToolExecution::Mcp { .. } => ("mcp", "external"),
    };

    span.set_attribute("langfuse.observation.metadata.tool_category", category);
    span.set_attribute("langfuse.observation.metadata.sandbox_tier", sandbox_tier);
    span.set_attribute(
        "langfuse.observation.metadata.approval_required",
        matches!(action, moa_core::PolicyAction::RequireApproval),
    );
}

pub(super) fn record_tool_execution_result(
    span: &tracing::Span,
    tool_name: &str,
    duration: Duration,
    result: &Result<(Option<String>, ToolOutput)>,
) {
    span.set_attribute("moa.tool.duration_ms", duration.as_millis() as i64);

    match result {
        Ok((_, output)) => {
            let succeeded = !output.is_error;
            span.set_attribute("moa.tool.success", succeeded);
            span.set_attribute("moa.tool.output", truncate_tool_span_text(output.to_text()));
            record_tool_call(
                tool_name,
                if succeeded { "success" } else { "error" },
                duration,
            );
            if output.is_error {
                span.set_status(Status::error(output.to_text()));
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
        Err(error) => {
            span.set_attribute("moa.tool.success", false);
            span.set_status(Status::error(error.to_string()));
            record_tool_call(tool_name, "error", duration);
        }
    }
}

pub(super) fn record_tool_output_truncated(tool_name: &str) {
    record_tool_output_truncated_metric(tool_name);
}

fn sandbox_tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

fn truncate_tool_span_text(mut value: String) -> String {
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

//! Trace-context span attribute helpers.

use moa_core::{SessionActorRef, TraceContext};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Sets MOA trace-context attributes on the provided tracing span.
pub fn apply_trace_context_to_span(context: &TraceContext, span: &tracing::Span) {
    let model = context.model.to_string();
    span.set_attribute("moa.session.id", context.session_id.to_string());
    span.set_attribute("moa.tenant.id", context.tenant_id.to_string());
    span.set_attribute("moa.model", model);
    if let Some(contact_id) = context.contact_id {
        span.set_attribute("moa.contact.id", contact_id.to_string());
    }
    if let Some(created_by) = context.created_by.as_ref() {
        match created_by {
            SessionActorRef::Identity { id } => {
                span.set_attribute("moa.actor.type", "identity");
                span.set_attribute("moa.actor.id", id.to_string());
            }
            SessionActorRef::Contact { id } => {
                span.set_attribute("moa.actor.type", "contact");
                span.set_attribute("moa.actor.id", id.to_string());
            }
            SessionActorRef::Anonymous => {
                span.set_attribute("moa.actor.type", "anonymous");
            }
        }
    }
    if let Some(contact_state) = context.contact_state.as_ref() {
        span.set_attribute("moa.contact.state", contact_state.clone());
    }
    if let Some(channel) = context.channel.as_ref() {
        span.set_attribute("moa.channel", channel.to_string());
    }
    if let Some(trace_name) = context.trace_name.as_ref() {
        span.set_attribute("moa.trace.name", trace_name.clone());
    }
    if let Some(environment) = context.environment.as_ref() {
        span.set_attribute("moa.environment", environment.clone());
    }
}

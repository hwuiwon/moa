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

#[cfg(test)]
mod tests {
    use moa_core::{
        Channel, ContactId, ModelId, SessionActorRef, SessionId, TenantId, TraceContext,
    };

    use super::apply_trace_context_to_span;
    use crate::test_capture::{attr_string, capture_spans, find_span};

    #[test]
    fn apply_trace_context_round_trips_every_field_onto_span() {
        // Pins: each TraceContext field maps to its documented span attribute key/value, so
        // cross-service trace metadata cannot silently drift or drop.
        let session_id = SessionId::new();
        let tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let context = TraceContext {
            session_id,
            tenant_id,
            contact_id: Some(contact_id),
            contact_state: Some("verified".to_string()),
            created_by: Some(SessionActorRef::Contact { id: contact_id }),
            channel: Some(Channel::Slack),
            model: ModelId::new("gpt-5.4"),
            trace_name: Some("Reset my password".to_string()),
            environment: Some("production".to_string()),
        };

        let spans = capture_spans(|| {
            let span = tracing::info_span!("trace_ctx_probe");
            apply_trace_context_to_span(&context, &span);
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "trace_ctx_probe");
        let session = session_id.to_string();
        let tenant = tenant_id.to_string();
        let contact = contact_id.to_string();
        assert_eq!(
            attr_string(span, "moa.session.id").as_deref(),
            Some(session.as_str())
        );
        assert_eq!(
            attr_string(span, "moa.tenant.id").as_deref(),
            Some(tenant.as_str())
        );
        assert_eq!(attr_string(span, "moa.model").as_deref(), Some("gpt-5.4"));
        assert_eq!(
            attr_string(span, "moa.contact.id").as_deref(),
            Some(contact.as_str())
        );
        assert_eq!(
            attr_string(span, "moa.actor.type").as_deref(),
            Some("contact")
        );
        assert_eq!(
            attr_string(span, "moa.actor.id").as_deref(),
            Some(contact.as_str())
        );
        assert_eq!(
            attr_string(span, "moa.contact.state").as_deref(),
            Some("verified")
        );
        assert_eq!(attr_string(span, "moa.channel").as_deref(), Some("slack"));
        assert_eq!(
            attr_string(span, "moa.trace.name").as_deref(),
            Some("Reset my password")
        );
        assert_eq!(
            attr_string(span, "moa.environment").as_deref(),
            Some("production")
        );
    }

    #[test]
    fn apply_trace_context_maps_each_actor_kind() {
        // Pins: actor kind drives moa.actor.type, and only identity/contact actors emit an
        // actor id; an absent actor emits no actor attributes at all.
        // SessionId wraps a Uuid in a public field, which yields the raw Uuid the
        // Identity actor variant needs without taking a direct `uuid` dependency.
        let identity_id = SessionId::new().0;
        let base = TraceContext {
            session_id: SessionId::new(),
            tenant_id: TenantId::new(),
            contact_id: None,
            contact_state: None,
            created_by: None,
            channel: None,
            model: ModelId::new("model"),
            trace_name: None,
            environment: None,
        };

        let spans = capture_spans(|| {
            let identity = TraceContext {
                created_by: Some(SessionActorRef::Identity { id: identity_id }),
                ..base.clone()
            };
            let span = tracing::info_span!("identity_actor");
            apply_trace_context_to_span(&identity, &span);
            span.in_scope(|| {});

            let anonymous = TraceContext {
                created_by: Some(SessionActorRef::Anonymous),
                ..base.clone()
            };
            let span = tracing::info_span!("anonymous_actor");
            apply_trace_context_to_span(&anonymous, &span);
            span.in_scope(|| {});

            let span = tracing::info_span!("no_actor");
            apply_trace_context_to_span(&base, &span);
            span.in_scope(|| {});
        });

        let identity = find_span(&spans, "identity_actor");
        let identity_actor = identity_id.to_string();
        assert_eq!(
            attr_string(identity, "moa.actor.type").as_deref(),
            Some("identity")
        );
        assert_eq!(
            attr_string(identity, "moa.actor.id").as_deref(),
            Some(identity_actor.as_str())
        );

        let anonymous = find_span(&spans, "anonymous_actor");
        assert_eq!(
            attr_string(anonymous, "moa.actor.type").as_deref(),
            Some("anonymous")
        );
        assert_eq!(attr_string(anonymous, "moa.actor.id"), None);

        let no_actor = find_span(&spans, "no_actor");
        assert_eq!(attr_string(no_actor, "moa.actor.type"), None);
        assert_eq!(attr_string(no_actor, "moa.actor.id"), None);
    }
}

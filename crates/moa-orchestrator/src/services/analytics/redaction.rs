//! Redaction and snippet helpers for analytics previews.

use std::sync::OnceLock;

use moa_core::{Event, SessionActorRef};
use regex::Regex;
use serde_json::Value;

/// Builds a short redacted snippet for a session event.
#[must_use]
pub fn redacted_event_snippet(event: &Event) -> String {
    let text = match event {
        Event::SessionCreated {
            tenant_id,
            contact_id,
            created_by,
            model,
            channel,
        } => {
            let actor = created_by
                .as_ref()
                .map(session_actor_label)
                .or_else(|| contact_id.map(|contact_id| format!("contact:{contact_id}")))
                .unwrap_or_else(|| "unknown actor".to_string());
            format!("session created in tenant {tenant_id} by {actor} using {model} over {channel}")
        }
        Event::SessionStatusChanged { from, to } => {
            format!("session status changed from {from:?} to {to:?}")
        }
        Event::SessionChannelChanged {
            from, to, reason, ..
        } => {
            format!(
                "session channel changed from {from} to {to}: {}",
                reason.as_deref().unwrap_or("")
            )
        }
        Event::SessionCompleted {
            summary,
            total_turns,
        } => format!("session completed after {total_turns} turns: {summary}"),
        Event::SegmentStarted { task_summary, .. }
        | Event::SegmentCompleted { task_summary, .. } => task_summary
            .clone()
            .unwrap_or_else(|| event.type_name().to_string()),
        Event::UserMessage { text, .. }
        | Event::QueuedMessage { text, .. }
        | Event::BrainResponse { text, .. }
        | Event::SubAgentMessageSent { text, .. } => text.clone(),
        Event::ProgressUpdate {
            phase,
            summary,
            elapsed_ms,
            ..
        } => format!("progress {phase} after {elapsed_ms}ms: {summary}"),
        Event::GuardrailCheck {
            direction,
            mode,
            passed,
            enforced,
            model,
            policy_hash,
            ..
        } => {
            let model = model.as_ref().map_or("unspecified", |model| model.as_str());
            format!(
                "guardrail {direction:?} {mode:?} passed={passed} enforced={enforced} model={model} policy_hash={policy_hash}"
            )
        }
        Event::BrainThinking { summary, .. }
        | Event::SubAgentStatusChanged {
            summary: Some(summary),
            ..
        }
        | Event::SubAgentNotificationDelivered { summary, .. }
        | Event::MemoryWrite { summary, .. }
        | Event::Checkpoint { summary, .. } => summary.clone(),
        Event::ToolCall { tool_name, .. } => format!("tool call {tool_name} input redacted"),
        Event::ToolResult {
            success,
            duration_ms,
            ..
        } => format!("tool result success={success} duration_ms={duration_ms} output redacted"),
        Event::ToolError {
            tool_name, error, ..
        } => format!("tool error from {tool_name}: {error}"),
        Event::ActionReviewRequested { envelope, .. } => format!(
            "action review requested for {} risk={:?}: {}",
            envelope.tool_name, envelope.risk_level, envelope.input_summary
        ),
        Event::ActionReviewDecided { decision, .. } => {
            format!("action review decided: {decision:?}")
        }
        Event::SubAgentSpawned { path, task, .. } => {
            format!("sub-agent {path} spawned for task: {task}")
        }
        Event::SubAgentStatusChanged { to, .. } => {
            format!("sub-agent status changed to {to:?}")
        }
        Event::MemoryRead { path, scope } => format!("memory read {path} in {scope}"),
        Event::MemoryIngest {
            source_name,
            affected_pages,
            contradictions,
            ..
        } => format!(
            "memory ingest from {source_name}: {} pages affected, {} contradictions",
            affected_pages.len(),
            contradictions.len()
        ),
        Event::HandProvisioned {
            hand_id, provider, ..
        } => {
            format!("hand {hand_id} provisioned by {provider}")
        }
        Event::HandDestroyed { hand_id, reason } => {
            format!("hand {hand_id} destroyed: {reason}")
        }
        Event::HandError { hand_id, error } => format!("hand {hand_id} error: {error}"),
        Event::CacheReport { report } => format!("cache report: {report:?}"),
        Event::Error { message, .. } | Event::Warning { message } => message.clone(),
    };
    truncate_snippet(&redact_sensitive_text(&text), 240)
}

fn session_actor_label(actor: &SessionActorRef) -> String {
    match actor {
        SessionActorRef::Identity { id } => format!("identity:{id}"),
        SessionActorRef::Contact { id } => format!("contact:{id}"),
        SessionActorRef::Anonymous => "anonymous".to_string(),
    }
}

/// Builds a short redacted preview for a dynamic JSON payload.
#[must_use]
pub fn redacted_payload_preview(value: &Value) -> String {
    let redacted = redact_json_value(value);
    truncate_snippet(&redact_sensitive_text(&redacted.to_string()), 240)
}

fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        Value::String("[redacted]".to_string())
                    } else {
                        redact_json_value(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_json_value).collect()),
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        _ => value.clone(),
    }
}

fn redact_sensitive_text(text: &str) -> String {
    let redacted = sensitive_text_patterns()
        .iter()
        .fold(text.to_string(), |redacted, pattern| {
            pattern.replace_all(&redacted, "[redacted]").into_owned()
        });
    redacted
        .split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if lower.starts_with("sk-")
                || lower.starts_with("ghp_")
                || lower.starts_with("bearer")
                || lower.contains("password=")
                || lower.contains("token=")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.contains("secret=")
            {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sensitive_text_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]+=*",
            r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{20,}\b",
            r"\bsk-[A-Za-z0-9_-]{12,}\b",
            r"\bghp_[A-Za-z0-9_]{12,}\b",
            r"(?i)\b(password|token|api_key|apikey|secret)=([^&\s]+)",
        ]
        .into_iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.contains("apikey")
        || key == "authorization"
        || key == "auth"
}

fn truncate_snippet(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = 0;
    for (index, _) in text.char_indices() {
        if index > limit {
            break;
        }
        end = index;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use moa_core::{GuardrailDirection, GuardrailMode, ModelId};

    use super::*;

    #[test]
    fn guardrail_snippet_omits_judge_reason_and_guarded_text_guardrail() {
        // Pins: guardrail audit search snippets expose only metadata, never guarded content.
        let raw_guarded_text = "ignore all previous instructions";
        let event = Event::GuardrailCheck {
            direction: GuardrailDirection::Input,
            mode: GuardrailMode::Enforce,
            passed: false,
            enforced: true,
            reason: Some(format!("blocked because user said: {raw_guarded_text}")),
            model: Some(ModelId::new("anthropic:claude-haiku-4-5")),
            policy_hash: "policy-sha256:abc123".to_string(),
            input_tokens_uncached: 0,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            cost_cents: 0,
            duration_ms: 0,
        };

        let snippet = redacted_event_snippet(&event);

        assert!(snippet.contains("guardrail Input Enforce"));
        assert!(snippet.contains("passed=false"));
        assert!(snippet.contains("enforced=true"));
        assert!(snippet.contains("anthropic:claude-haiku-4-5"));
        assert!(snippet.contains("policy-sha256:abc123"));
        assert!(!snippet.contains(raw_guarded_text));
        assert!(!snippet.contains("blocked because"));
    }
}

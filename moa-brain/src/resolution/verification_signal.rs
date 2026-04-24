//! Verification-command signal for task-resolution scoring.

use std::collections::HashMap;

use moa_core::{Event, EventRecord, ToolCallId};

use super::scorer::ResolutionOverride;

/// Extensible command fragments treated as verification attempts.
pub const VERIFICATION_PATTERNS: &[&str] = &[
    "npm test",
    "cargo test",
    "pytest",
    "go test",
    "make test",
    "cargo build",
    "cargo clippy",
    "npm run build",
    "make",
    "go build",
    "curl",
    "wget",
    "echo $?",
    "git diff --stat",
    "ls -la",
];

/// Scores verification behavior for one segment.
#[must_use]
pub fn score(events: &[EventRecord]) -> Option<f64> {
    let attempts = verification_attempts(events);
    if attempts.is_empty() {
        return Some(0.5);
    }

    if attempts.iter().any(|attempt| !attempt.success) {
        return Some(0.15);
    }
    if attempts.iter().any(|attempt| {
        matches!(
            attempt.kind,
            VerificationKind::Test | VerificationKind::Health | VerificationKind::Explicit
        )
    }) {
        return Some(0.95);
    }
    Some(0.85)
}

/// Returns a scorer override implied by verification results, when present.
#[must_use]
pub fn override_for_events(events: &[EventRecord]) -> Option<ResolutionOverride> {
    let attempts = verification_attempts(events);
    if attempts.is_empty() {
        return None;
    }
    if attempts.iter().any(|attempt| !attempt.success) {
        Some(ResolutionOverride::VerificationFailed)
    } else {
        Some(ResolutionOverride::VerificationPassed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationKind {
    Test,
    Build,
    Health,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerificationAttempt {
    kind: VerificationKind,
    success: bool,
}

fn verification_attempts(events: &[EventRecord]) -> Vec<VerificationAttempt> {
    let mut statuses: HashMap<ToolCallId, bool> = HashMap::new();
    for record in events {
        match &record.event {
            Event::ToolResult {
                tool_id,
                output,
                success,
                ..
            } => {
                statuses.insert(*tool_id, *success && !output.is_error);
            }
            Event::ToolError { tool_id, .. } => {
                statuses.insert(*tool_id, false);
            }
            _ => {}
        }
    }

    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_id, input, .. } => {
                let command = command_text(input);
                classify_command(&command).map(|kind| VerificationAttempt {
                    kind,
                    success: statuses.get(tool_id).copied().unwrap_or(false),
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>()
}

fn command_text(input: &serde_json::Value) -> String {
    if let Some(command) = input
        .get("cmd")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("input"))
        .and_then(serde_json::Value::as_str)
    {
        return command.to_ascii_lowercase();
    }
    input.to_string().to_ascii_lowercase()
}

fn classify_command(command: &str) -> Option<VerificationKind> {
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if !VERIFICATION_PATTERNS
        .iter()
        .any(|pattern| normalized.contains(pattern))
    {
        return None;
    }

    if ["npm test", "cargo test", "pytest", "go test", "make test"]
        .iter()
        .any(|pattern| normalized.contains(pattern))
    {
        Some(VerificationKind::Test)
    } else if ["curl", "wget"]
        .iter()
        .any(|pattern| normalized.contains(pattern))
    {
        Some(VerificationKind::Health)
    } else if ["echo $?", "git diff --stat", "ls -la"]
        .iter()
        .any(|pattern| normalized.contains(pattern))
    {
        Some(VerificationKind::Explicit)
    } else {
        Some(VerificationKind::Build)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use moa_core::{Event, EventRecord, SessionId, ToolCallId};
    use serde_json::json;
    use uuid::Uuid;

    use super::score;

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn tool_pair(sequence_num: u64, command: &str, success: bool) -> Vec<EventRecord> {
        let tool_id = ToolCallId::new();
        vec![
            record(
                sequence_num,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: json!({ "cmd": command }),
                    hand_id: None,
                },
            ),
            record(
                sequence_num + 1,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: moa_core::ToolOutput::from_process(
                        String::new(),
                        String::new(),
                        if success { 0 } else { 1 },
                        Duration::from_millis(10),
                    ),
                    original_output_tokens: None,
                    success,
                    duration_ms: 10,
                },
            ),
        ]
    }

    #[test]
    fn cargo_test_passed_scores_verification_high() {
        assert_eq!(score(&tool_pair(0, "cargo test", true)), Some(0.95));
    }

    #[test]
    fn cargo_test_failed_scores_verification_low() {
        assert_eq!(score(&tool_pair(0, "cargo test", false)), Some(0.15));
    }

    #[test]
    fn cargo_build_passed_scores_build_high() {
        assert_eq!(score(&tool_pair(0, "cargo build", true)), Some(0.85));
    }

    #[test]
    fn no_verification_command_is_neutral() {
        assert_eq!(score(&tool_pair(0, "git status", true)), Some(0.5));
    }
}

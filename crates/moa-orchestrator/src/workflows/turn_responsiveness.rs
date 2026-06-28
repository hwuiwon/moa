//! Deterministic turn responsiveness classification and cap policy.

use moa_core::config::SessionLimitsConfig;
use moa_core::wire::turn::TurnComplexityClass;
use moa_core::{Event, EventRecord, ToolContent, ToolInvocation, ToolOutput};

/// Cheap, deterministic inputs used to classify one turn request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnResponsivenessInput<'a> {
    /// Current user-visible request text.
    pub(crate) user_text: &'a str,
    /// Number of attachments on the current request.
    pub(crate) attachment_count: usize,
    /// Explicit turn cap supplied by the caller, if any.
    pub(crate) request_max_turns: Option<u32>,
    /// Whether cheap existing metadata already points at a recent target.
    pub(crate) has_recent_target: bool,
    /// Whether this turn is running inside an explicit workflow adapter context.
    pub(crate) is_workflow_context: bool,
    /// Whether this turn is running inside a delegated sub-agent context.
    pub(crate) is_sub_agent_context: bool,
    /// Count of tool schemas known to be available without compiling context.
    pub(crate) available_tool_count: usize,
}

impl<'a> TurnResponsivenessInput<'a> {
    /// Creates a root-turn classifier input with no optional context signals.
    #[cfg(test)]
    pub(crate) fn root(user_text: &'a str) -> Self {
        Self {
            user_text,
            attachment_count: 0,
            request_max_turns: None,
            has_recent_target: false,
            is_workflow_context: false,
            is_sub_agent_context: false,
            available_tool_count: 0,
        }
    }

    fn has_attachments(self) -> bool {
        self.attachment_count > 0
    }
}

/// Selects a deterministic responsiveness class for one turn request.
pub(crate) fn classify_turn_request(input: TurnResponsivenessInput<'_>) -> TurnComplexityClass {
    let normalized = normalize_text(input.user_text);
    let text = normalized.as_str();
    if input.is_workflow_context || input.is_sub_agent_context {
        return TurnComplexityClass::Complex;
    }

    let direct_question = is_direct_question(input.user_text, text);
    if !direct_question
        && !input.has_attachments()
        && !input.has_recent_target
        && (text.is_empty() || is_vague_deictic_action(text))
    {
        return TurnComplexityClass::Clarification;
    }

    if is_complex_or_workflow_shaped(text) {
        return TurnComplexityClass::Complex;
    }

    if input.has_attachments() || input.has_recent_target {
        return TurnComplexityClass::Standard;
    }

    if is_tool_work_request(text, input.available_tool_count) {
        return TurnComplexityClass::Standard;
    }

    if direct_question && input.available_tool_count > 0 && !is_short_prompt(text) {
        return TurnComplexityClass::Standard;
    }

    if input
        .request_max_turns
        .is_some_and(|max_turns| max_turns > 1)
    {
        return TurnComplexityClass::Standard;
    }

    if direct_question || is_short_prompt(text) {
        return TurnComplexityClass::Simple;
    }

    TurnComplexityClass::Standard
}

/// Returns the effective model-loop cap for a selected turn class.
pub(crate) fn effective_turn_cap(
    request_max_turns: Option<u32>,
    selected_class: TurnComplexityClass,
    session_limits: &SessionLimitsConfig,
) -> usize {
    let hard_cap = session_limits.max_turns as usize;
    if let Some(request_cap) = request_max_turns {
        let request_cap = (request_cap as usize).max(1);
        return if hard_cap == 0 {
            request_cap
        } else {
            request_cap.min(hard_cap)
        };
    }

    if hard_cap == 0 {
        return usize::MAX;
    }

    let class_cap = match selected_class {
        TurnComplexityClass::Clarification | TurnComplexityClass::Simple => {
            session_limits.simple_max_turns as usize
        }
        TurnComplexityClass::Standard => session_limits.standard_max_turns as usize,
        TurnComplexityClass::Complex => hard_cap,
    };
    class_cap.max(1).min(hard_cap)
}

/// Returns the effective tool-call cap for a selected turn class.
pub(crate) fn effective_tool_cap(
    selected_class: TurnComplexityClass,
    session_limits: &SessionLimitsConfig,
) -> usize {
    match selected_class {
        TurnComplexityClass::Clarification | TurnComplexityClass::Simple => 0,
        TurnComplexityClass::Standard | TurnComplexityClass::Complex => {
            session_limits.max_tool_calls as usize
        }
    }
}

/// Converts an internal cap into the progress DTO representation.
pub(crate) fn progress_cap(cap: usize) -> Option<u32> {
    if cap == usize::MAX {
        None
    } else {
        Some(cap.min(u32::MAX as usize) as u32)
    }
}

/// Converts an internal counter into the progress DTO representation.
pub(crate) fn progress_count(count: usize) -> u32 {
    count.min(u32::MAX as usize) as u32
}

/// Returns whether recent persisted events contain a concrete target for a vague follow-up.
pub(crate) fn has_recent_target(
    recent_events: &[EventRecord],
    current_user_sequence_num: u64,
) -> bool {
    recent_events
        .iter()
        .filter(|record| record.sequence_num < current_user_sequence_num)
        .any(|record| event_has_target_signal(&record.event))
}

/// Per-turn state for enforcing tool dispatch caps and repeated-call loops.
#[derive(Clone, Debug)]
pub(crate) struct ToolBudgetState {
    max_tool_calls: usize,
    loop_detection_threshold: usize,
    attempted_tool_calls: usize,
    last_fingerprint: Option<ToolFingerprint>,
    consecutive_repeats: usize,
}

impl ToolBudgetState {
    /// Creates a per-turn tool budget state machine.
    pub(crate) fn new(max_tool_calls: usize, loop_detection_threshold: u32) -> Self {
        Self {
            max_tool_calls,
            loop_detection_threshold: loop_detection_threshold as usize,
            attempted_tool_calls: 0,
            last_fingerprint: None,
            consecutive_repeats: 0,
        }
    }

    /// Records one attempted tool call and returns whether it may dispatch.
    pub(crate) fn before_tool_dispatch(
        &mut self,
        invocation: &ToolInvocation,
    ) -> ToolBudgetDecision {
        self.attempted_tool_calls = self.attempted_tool_calls.saturating_add(1);
        let fingerprint = ToolFingerprint::from_invocation(invocation);
        if self.last_fingerprint.as_ref() == Some(&fingerprint) {
            self.consecutive_repeats = self.consecutive_repeats.saturating_add(1);
        } else {
            self.last_fingerprint = Some(fingerprint.clone());
            self.consecutive_repeats = 1;
        }

        if self.max_tool_calls == 0 || self.attempted_tool_calls > self.max_tool_calls {
            return ToolBudgetDecision::Stop(ToolBudgetExhausted {
                attempted_tool_calls: self.attempted_tool_calls,
                max_tool_calls: self.max_tool_calls,
                tool_name: fingerprint.tool_name,
                consecutive_repeats: self.consecutive_repeats,
                reason: ToolBudgetExhaustedReason::MaxToolCallsExceeded,
            });
        }

        if self.loop_detection_threshold > 0
            && self.consecutive_repeats >= self.loop_detection_threshold
        {
            return ToolBudgetDecision::Stop(ToolBudgetExhausted {
                attempted_tool_calls: self.attempted_tool_calls,
                max_tool_calls: self.max_tool_calls,
                tool_name: fingerprint.tool_name,
                consecutive_repeats: self.consecutive_repeats,
                reason: ToolBudgetExhaustedReason::RepeatedToolCall {
                    threshold: self.loop_detection_threshold,
                },
            });
        }

        ToolBudgetDecision::Allow {
            attempted_tool_calls: self.attempted_tool_calls,
        }
    }

    /// Returns the number of tool calls attempted during this turn.
    pub(crate) fn attempted_tool_calls(&self) -> usize {
        self.attempted_tool_calls
    }
}

/// Decision returned by [`ToolBudgetState`] before a tool dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToolBudgetDecision {
    /// The tool call may dispatch.
    Allow {
        /// Number of attempted tool calls after recording this call.
        attempted_tool_calls: usize,
    },
    /// Tool execution must stop before dispatch.
    Stop(ToolBudgetExhausted),
}

/// Details for an auditable tool-budget stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolBudgetExhausted {
    /// Number of attempted tool calls after recording the blocked call.
    pub(crate) attempted_tool_calls: usize,
    /// Selected maximum tool calls for the turn.
    pub(crate) max_tool_calls: usize,
    /// Tool name from the blocked call.
    pub(crate) tool_name: String,
    /// Number of consecutive matching tool fingerprints.
    pub(crate) consecutive_repeats: usize,
    /// Stop reason.
    pub(crate) reason: ToolBudgetExhaustedReason,
}

impl ToolBudgetExhausted {
    /// Returns a short, user-visible stop response.
    pub(crate) fn assistant_message(&self) -> String {
        match self.reason {
            ToolBudgetExhaustedReason::MaxToolCallsExceeded => format!(
                "MOA stopped before running another tool because this turn reached the tool-call budget ({}). Narrow the scope or ask MOA to continue.",
                self.max_tool_calls
            ),
            ToolBudgetExhaustedReason::RepeatedToolCall { .. } => format!(
                "MOA stopped before running another tool because the model repeatedly requested the same `{}` call. Narrow the scope or ask MOA to continue.",
                self.tool_name
            ),
        }
    }

    /// Returns an audit-oriented error message.
    pub(crate) fn audit_message(&self) -> String {
        match self.reason {
            ToolBudgetExhaustedReason::MaxToolCallsExceeded => format!(
                "tool budget exceeded: attempted {} tool calls with max {}",
                self.attempted_tool_calls, self.max_tool_calls
            ),
            ToolBudgetExhaustedReason::RepeatedToolCall { threshold } => format!(
                "tool loop detected: `{}` repeated {} consecutive times with threshold {}",
                self.tool_name, self.consecutive_repeats, threshold
            ),
        }
    }
}

/// Reason a tool-budget decision stopped dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolBudgetExhaustedReason {
    /// The turn selected no more tool-call capacity.
    MaxToolCallsExceeded,
    /// The same tool fingerprint repeated consecutively past the loop threshold.
    RepeatedToolCall {
        /// Configured repeat threshold.
        threshold: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolFingerprint {
    tool_name: String,
    canonical_input: String,
}

impl ToolFingerprint {
    fn from_invocation(invocation: &ToolInvocation) -> Self {
        Self {
            tool_name: invocation.name.clone(),
            canonical_input: canonical_json(&invocation.input),
        }
    }
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => {
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
        }
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{values}]")
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    format!("{key}:{}", canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
    }
}

fn event_has_target_signal(event: &Event) -> bool {
    match event {
        Event::UserMessage { text, attachments } => {
            !attachments.is_empty() || text_has_target_signal(text)
        }
        Event::BrainResponse { text, .. } | Event::SubAgentMessageSent { text, .. } => {
            text_has_target_signal(text)
        }
        Event::ToolCall {
            tool_name, input, ..
        } => tool_name_implies_target(tool_name) || json_has_target_signal(input, 0),
        Event::ToolResult { output, .. } => tool_output_has_target(output),
        Event::ToolError {
            tool_name, error, ..
        } => tool_name_implies_target(tool_name) || text_has_target_signal(error),
        Event::SubAgentSpawned { task, .. } => text_has_target_signal(task),
        Event::SegmentStarted { task_summary, .. }
        | Event::SegmentCompleted { task_summary, .. } => {
            task_summary.as_deref().is_some_and(text_has_target_signal)
        }
        Event::MemoryRead { path, .. } | Event::MemoryWrite { path, .. } => !path.trim().is_empty(),
        Event::MemoryIngest {
            source_path,
            affected_pages,
            ..
        } => {
            !source_path.trim().is_empty()
                || affected_pages.iter().any(|page| !page.trim().is_empty())
        }
        _ => false,
    }
}

fn tool_name_implies_target(tool_name: &str) -> bool {
    let tool_name = tool_name.to_ascii_lowercase();
    contains_any(
        &tool_name,
        &[
            "bash",
            "edit",
            "file",
            "git",
            "patch",
            "repo",
            "shell",
            "sub_agent",
            "tool",
        ],
    )
}

fn tool_output_has_target(output: &ToolOutput) -> bool {
    output.artifact.is_some()
        || output.content.iter().any(|content| match content {
            ToolContent::Text { text } => text_has_target_signal(text),
            ToolContent::Json { data } => json_has_target_signal(data, 0),
        })
        || output
            .structured
            .as_ref()
            .is_some_and(|data| json_has_target_signal(data, 0))
}

fn json_has_target_signal(value: &serde_json::Value, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    match value {
        serde_json::Value::String(text) => text_has_target_signal(text),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_has_target_signal(value, depth + 1)),
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            targetish_json_key(key) || json_has_target_signal(value, depth + 1)
        }),
        _ => false,
    }
}

fn targetish_json_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "branch"
            | "file"
            | "filename"
            | "files"
            | "message_id"
            | "object"
            | "path"
            | "paths"
            | "pr"
            | "repo"
            | "repository"
            | "resource"
            | "target"
            | "uri"
            | "url"
    )
}

fn text_has_target_signal(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && (text.contains("```")
            || text.contains("http://")
            || text.contains("https://")
            || text.split_whitespace().any(token_has_target_signal))
}

fn token_has_target_signal(raw_token: &str) -> bool {
    let token = raw_token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\''
                | '`'
                | ','
                | ';'
                | ':'
                | '.'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
        )
    });
    let token = token
        .split_once(':')
        .map_or(token, |(before_line_suffix, line_suffix)| {
            if line_suffix.chars().all(|ch| ch.is_ascii_digit()) {
                before_line_suffix
            } else {
                token
            }
        });
    if token.len() < 3 {
        return false;
    }

    let lower = token.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "cargo.toml"
            | "dockerfile"
            | "go.mod"
            | "makefile"
            | "package.json"
            | "pyproject.toml"
            | "readme"
            | "tsconfig.json"
    ) {
        return true;
    }
    if lower.contains("::") {
        return true;
    }
    if lower.contains('/') || lower.contains('\\') {
        return lower != "and/or";
    }

    let Some((_, extension)) = lower.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension,
        "c" | "cc"
            | "cpp"
            | "cs"
            | "css"
            | "go"
            | "h"
            | "hpp"
            | "html"
            | "java"
            | "js"
            | "json"
            | "jsx"
            | "kt"
            | "lock"
            | "md"
            | "php"
            | "proto"
            | "py"
            | "rb"
            | "rs"
            | "sh"
            | "sql"
            | "swift"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "yaml"
            | "yml"
    )
}

fn normalize_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_direct_question(original: &str, normalized: &str) -> bool {
    let trimmed = original.trim();
    trimmed.ends_with('?')
        || starts_with_any(
            normalized,
            &[
                "what ",
                "why ",
                "how ",
                "when ",
                "where ",
                "who ",
                "which ",
                "can you ",
                "could you ",
                "should ",
                "is ",
                "are ",
                "does ",
                "did ",
            ],
        )
}

fn is_vague_deictic_action(text: &str) -> bool {
    matches!(
        text,
        "fix this"
            | "fix it"
            | "fix that"
            | "change this"
            | "change it"
            | "change that"
            | "update this"
            | "update it"
            | "update that"
            | "do this"
            | "do it"
            | "do that"
            | "handle this"
            | "handle it"
            | "handle that"
            | "make this work"
            | "make it work"
            | "clean this up"
            | "clean it up"
            | "improve this"
            | "improve it"
            | "review this"
            | "review it"
            | "check this"
            | "check it"
            | "take care of this"
            | "take care of it"
    ) || starts_with_any(
        text,
        &["fix the above", "change the above", "update the above"],
    )
}

fn is_complex_or_workflow_shaped(text: &str) -> bool {
    if text.len() > 600 {
        return true;
    }

    contains_any(
        text,
        &[
            "acceptance criteria",
            "all scenarios",
            "architecture",
            "audit",
            "break this into",
            "codebase",
            "cross-crate",
            "end to end",
            "end-to-end",
            "execute the plan",
            "final report",
            "full coverage",
            "implement",
            "milestone",
            "refactor",
            "release gate",
            "run the plan",
            "task goal",
            "workflow",
            "write scope",
        ],
    )
}

fn is_tool_work_request(text: &str, available_tool_count: usize) -> bool {
    contains_any(
        text,
        &[
            "cargo ", "clippy", "compile", "git ", "test ", "tests", "tool ", "tools",
        ],
    ) || starts_with_any(
        text,
        &[
            "add ", "check ", "edit ", "execute ", "find ", "fix ", "inspect ", "list ",
            "look up ", "modify ", "open ", "read ", "run ", "search ", "show ", "update ",
            "write ",
        ],
    ) || (available_tool_count > 0
        && contains_any(
            text,
            &[
                "file",
                "files",
                "repo",
                "repository",
                "search",
                "run",
                "command",
            ],
        ))
}

fn is_short_prompt(text: &str) -> bool {
    text.split_whitespace().count() <= 12
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn starts_with_any(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::config::SessionLimitsConfig;
    use moa_core::wire::turn::TurnComplexityClass;
    use moa_core::{Attachment, Event, EventRecord, SessionId, ToolCallId, ToolInvocation};
    use uuid::Uuid;

    use super::{
        ToolBudgetDecision, ToolBudgetExhaustedReason, ToolBudgetState, ToolFingerprint,
        TurnResponsivenessInput, classify_turn_request, effective_tool_cap, effective_turn_cap,
        has_recent_target, progress_cap,
    };

    fn limits() -> SessionLimitsConfig {
        SessionLimitsConfig {
            max_turns: 8,
            simple_max_turns: 1,
            standard_max_turns: 4,
            max_tool_calls: 12,
            ..SessionLimitsConfig::default()
        }
    }

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        let event_type = event.event_type();
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn attachment(name: &str) -> Attachment {
        Attachment {
            id: None,
            name: name.to_string(),
            mime_type: None,
            sha256: None,
            url: None,
            path: None,
            size_bytes: None,
        }
    }

    fn invocation(id: &str, name: &str, input: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            id: Some(id.to_string()),
            name: name.to_string(),
            input,
        }
    }

    #[test]
    fn direct_simple_request_is_simple() {
        // Pins: direct questions are answerable requests, not clarification prompts.
        let input = TurnResponsivenessInput::root("What is the configured model?");
        assert_eq!(classify_turn_request(input), TurnComplexityClass::Simple);
    }

    #[test]
    fn vague_without_target_is_clarification() {
        // Pins: high-confidence deictic edits without target context ask before doing work.
        for text in ["fix this", "do this"] {
            let input = TurnResponsivenessInput::root(text);
            assert_eq!(
                classify_turn_request(input),
                TurnComplexityClass::Clarification,
                "{text:?} should ask for clarification"
            );
        }
    }

    #[test]
    fn vague_with_recent_target_is_standard() {
        // Pins: a recent target keeps vague follow-up edits runnable instead of asking first.
        let input = TurnResponsivenessInput {
            has_recent_target: true,
            ..TurnResponsivenessInput::root("fix this")
        };
        assert_eq!(classify_turn_request(input), TurnComplexityClass::Standard);
    }

    #[test]
    fn direct_question_without_question_mark_is_simple() {
        // Pins: direct "what is X" requests do not get mistaken for vague edits.
        let input = TurnResponsivenessInput::root("what is X");
        assert_eq!(classify_turn_request(input), TurnComplexityClass::Simple);
    }

    #[test]
    fn detailed_question_with_tools_is_standard() {
        // Pins: detailed support requests phrased as questions still get enough budget to use selected skills/tools.
        let input = TurnResponsivenessInput {
            available_tool_count: 3,
            ..TurnResponsivenessInput::root(
                "A customer says their ramen order arrived spilled all over the bag. They uploaded a clear photo and want a refund or replacement. Can you handle this?",
            )
        };

        assert_eq!(classify_turn_request(input), TurnComplexityClass::Standard);
    }

    #[test]
    fn standard_tool_work_is_standard() {
        // Pins: focused tool-shaped work gets normal bounded tool/model loops.
        let input = TurnResponsivenessInput {
            available_tool_count: 3,
            ..TurnResponsivenessInput::root("run cargo test -p moa-orchestrator")
        };
        assert_eq!(classify_turn_request(input), TurnComplexityClass::Standard);
    }

    #[test]
    fn workflow_shaped_request_is_complex() {
        // Pins: explicit workflow contexts and broad implementation prompts get the hard cap.
        let workflow_input = TurnResponsivenessInput {
            is_workflow_context: true,
            ..TurnResponsivenessInput::root("")
        };
        assert_eq!(
            classify_turn_request(workflow_input),
            TurnComplexityClass::Complex
        );

        let task_input = TurnResponsivenessInput::root(
            "Task Goal: implement the new workflow policy. Acceptance Criteria: run verification.",
        );
        assert_eq!(
            classify_turn_request(task_input),
            TurnComplexityClass::Complex
        );
    }

    #[test]
    fn recent_target_detector_ignores_current_sequence() {
        // Pins: a vague current request cannot count as its own target after append.
        let events = vec![
            record(
                10,
                Event::UserMessage {
                    text: "hello".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                11,
                Event::UserMessage {
                    text: "fix crates/moa-core/src/lib.rs".to_string(),
                    attachments: vec![attachment("lib.rs")],
                },
            ),
        ];

        assert!(!has_recent_target(&events, 11));
    }

    #[test]
    fn recent_target_detector_finds_prior_attachment_and_path() {
        // Pins: concrete prior files keep follow-up edits on the normal execution path.
        let attachment_events = vec![record(
            7,
            Event::UserMessage {
                text: "please inspect this".to_string(),
                attachments: vec![attachment("report.md")],
            },
        )];
        assert!(has_recent_target(&attachment_events, 8));

        let path_events = vec![record(
            7,
            Event::UserMessage {
                text: "please inspect crates/moa-orchestrator/src/workflows/turn_execution.rs"
                    .to_string(),
                attachments: Vec::new(),
            },
        )];
        assert!(has_recent_target(&path_events, 8));
    }

    #[test]
    fn recent_target_detector_finds_prior_tool_target() {
        // Pins: recent tool work is a target-bearing context for terse follow-ups.
        let events = vec![record(
            4,
            Event::ToolCall {
                tool_id: ToolCallId(Uuid::now_v7()),
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input: serde_json::json!({
                    "path": "crates/moa-orchestrator/src/workflows/turn_execution.rs"
                }),
                hand_id: None,
            },
        )];

        assert!(has_recent_target(&events, 5));
    }

    #[test]
    fn effective_turn_cap_applies_class_and_hard_limits() {
        // Pins: class defaults are bounded by the global hard cap and never return zero turns.
        let limits = limits();
        assert_eq!(
            effective_turn_cap(None, TurnComplexityClass::Simple, &limits),
            1
        );
        assert_eq!(
            effective_turn_cap(None, TurnComplexityClass::Standard, &limits),
            4
        );
        assert_eq!(
            effective_turn_cap(None, TurnComplexityClass::Complex, &limits),
            8
        );

        let tiny_hard_cap = SessionLimitsConfig {
            max_turns: 1,
            standard_max_turns: 4,
            ..limits
        };
        assert_eq!(
            effective_turn_cap(None, TurnComplexityClass::Standard, &tiny_hard_cap),
            1
        );
    }

    #[test]
    fn effective_turn_cap_bounds_explicit_request_caps() {
        // Pins: explicit workflow/request caps continue to work but cannot exceed the hard cap.
        let limits = limits();
        assert_eq!(
            effective_turn_cap(Some(3), TurnComplexityClass::Complex, &limits),
            3
        );
        assert_eq!(
            effective_turn_cap(Some(30), TurnComplexityClass::Complex, &limits),
            8
        );
        assert_eq!(
            effective_turn_cap(Some(0), TurnComplexityClass::Complex, &limits),
            1
        );
    }

    #[test]
    fn effective_turn_cap_preserves_unlimited_global_semantics() {
        // Pins: max_turns=0 keeps existing unlimited semantics for uncapped requests.
        let limits = SessionLimitsConfig {
            max_turns: 0,
            simple_max_turns: 1,
            standard_max_turns: 4,
            ..limits()
        };
        assert_eq!(
            effective_turn_cap(None, TurnComplexityClass::Standard, &limits),
            usize::MAX
        );
        assert_eq!(
            effective_turn_cap(Some(5), TurnComplexityClass::Standard, &limits),
            5
        );
        assert_eq!(progress_cap(usize::MAX), None);
    }

    #[test]
    fn effective_tool_cap_uses_selected_class() {
        // Pins: direct/clarification turns do not receive tool budget; work turns do.
        let limits = limits();
        assert_eq!(
            effective_tool_cap(TurnComplexityClass::Clarification, &limits),
            0
        );
        assert_eq!(effective_tool_cap(TurnComplexityClass::Simple, &limits), 0);
        assert_eq!(
            effective_tool_cap(TurnComplexityClass::Standard, &limits),
            12
        );
        assert_eq!(
            effective_tool_cap(TurnComplexityClass::Complex, &limits),
            12
        );
    }

    #[test]
    fn tool_budget_stops_before_dispatch_beyond_max_cap() {
        // Pins: the selected max_tool_calls cap is checked before dispatching a tool.
        let mut budget = ToolBudgetState::new(2, 0);

        assert_eq!(
            budget.before_tool_dispatch(&invocation(
                "tool-1",
                "bash",
                serde_json::json!({"cmd": "one"})
            )),
            ToolBudgetDecision::Allow {
                attempted_tool_calls: 1
            }
        );
        assert_eq!(
            budget.before_tool_dispatch(&invocation(
                "tool-2",
                "bash",
                serde_json::json!({"cmd": "two"})
            )),
            ToolBudgetDecision::Allow {
                attempted_tool_calls: 2
            }
        );
        let ToolBudgetDecision::Stop(stop) = budget.before_tool_dispatch(&invocation(
            "tool-3",
            "bash",
            serde_json::json!({"cmd": "three"}),
        )) else {
            panic!("third call should stop before dispatch");
        };

        assert_eq!(stop.attempted_tool_calls, 3);
        assert_eq!(stop.max_tool_calls, 2);
        assert_eq!(stop.reason, ToolBudgetExhaustedReason::MaxToolCallsExceeded);
        assert_eq!(budget.attempted_tool_calls(), 3);
    }

    #[test]
    fn tool_budget_zero_stops_first_tool_before_dispatch() {
        // Pins: clarification/simple classes select a zero tool budget and cannot dispatch tools.
        let mut budget = ToolBudgetState::new(0, 3);

        let ToolBudgetDecision::Stop(stop) = budget.before_tool_dispatch(&invocation(
            "tool-1",
            "bash",
            serde_json::json!({"cmd": "pwd"}),
        )) else {
            panic!("zero tool budget should stop the first tool call");
        };

        assert_eq!(stop.attempted_tool_calls, 1);
        assert_eq!(stop.max_tool_calls, 0);
        assert_eq!(stop.reason, ToolBudgetExhaustedReason::MaxToolCallsExceeded);
    }

    #[test]
    fn repeated_tool_fingerprint_stops_at_loop_threshold() {
        // Pins: repeated tool detection keys on tool name plus canonical JSON input, not provider id.
        let mut budget = ToolBudgetState::new(10, 3);

        for index in 1..=2 {
            assert_eq!(
                budget.before_tool_dispatch(&invocation(
                    &format!("scripted-{index}"),
                    "bash",
                    serde_json::json!({"cmd": "cargo test"})
                )),
                ToolBudgetDecision::Allow {
                    attempted_tool_calls: index
                }
            );
        }
        let ToolBudgetDecision::Stop(stop) = budget.before_tool_dispatch(&invocation(
            "scripted-3",
            "bash",
            serde_json::json!({"cmd": "cargo test"}),
        )) else {
            panic!("third identical call should stop before dispatch");
        };

        assert_eq!(
            stop.reason,
            ToolBudgetExhaustedReason::RepeatedToolCall { threshold: 3 }
        );
        assert_eq!(stop.consecutive_repeats, 3);
        assert_eq!(stop.tool_name, "bash");
    }

    #[test]
    fn canonical_tool_input_ordering_is_stable_for_repeat_detection() {
        // Pins: serde_json object insertion order cannot affect tool loop fingerprints.
        let left = invocation(
            "left",
            "file_write",
            serde_json::json!({
                "path": "notes.md",
                "content": {
                    "b": [2, {"z": true, "a": null}],
                    "a": 1
                }
            }),
        );
        let right = invocation(
            "right",
            "file_write",
            serde_json::json!({
                "content": {
                    "a": 1,
                    "b": [2, {"a": null, "z": true}]
                },
                "path": "notes.md"
            }),
        );

        assert_eq!(
            ToolFingerprint::from_invocation(&left),
            ToolFingerprint::from_invocation(&right)
        );
    }

    #[test]
    fn scripted_repeated_tool_sequence_stops_before_unbounded_dispatch() {
        // Pins: scripted providers that keep asking for the same tool stop deterministically.
        let mut budget = ToolBudgetState::new(10, 3);
        let scripted_calls = (0..5).map(|index| {
            invocation(
                &format!("scripted-tool-{index}"),
                "bash",
                serde_json::json!({"cmd": "cargo test -p moa-orchestrator"}),
            )
        });
        let mut dispatched = 0;

        for call in scripted_calls {
            match budget.before_tool_dispatch(&call) {
                ToolBudgetDecision::Allow { .. } => dispatched += 1,
                ToolBudgetDecision::Stop(stop) => {
                    assert_eq!(dispatched, 2);
                    assert_eq!(stop.attempted_tool_calls, 3);
                    assert_eq!(
                        stop.reason,
                        ToolBudgetExhaustedReason::RepeatedToolCall { threshold: 3 }
                    );
                    assert!(
                        stop.assistant_message()
                            .contains("MOA stopped before running another tool")
                    );
                    return;
                }
            }
        }

        panic!("scripted repeated calls should have stopped");
    }
}

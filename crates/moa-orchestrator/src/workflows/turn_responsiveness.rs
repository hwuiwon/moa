//! Deterministic turn cap and recent-target policy around model-assisted routing.

use moa_core::config::SessionLimitsConfig;
use moa_core::{
    events::Event, types::completion::ToolInvocation, types::events_stream::EventRecord,
};

/// Closed model-loop classes that receive turn and tool budgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelLoopClass {
    /// One root response with no tools.
    Respond,
    /// One bounded root Execute/Inline loop.
    InlineExecute,
    /// One bounded delegated worker Inline loop.
    WorkerInline,
}

/// Returns the effective model-loop cap for a selected turn class.
pub(crate) fn effective_turn_cap(
    request_max_turns: Option<u32>,
    class: ModelLoopClass,
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

    let class_cap = match class {
        ModelLoopClass::Respond => session_limits.simple_max_turns as usize,
        ModelLoopClass::InlineExecute | ModelLoopClass::WorkerInline => {
            session_limits.standard_max_turns as usize
        }
    };
    class_cap.max(1).min(hard_cap)
}

/// Returns the effective model-loop cap for a turn that has delegated to a worker.
///
/// Delegation escalation is one-way: it raises the per-turn loop budget to
/// `max_model_turns_delegation` (bounded by the global hard cap) so a coordinator
/// can spawn workers, wait for them, and synthesize their results within one turn.
/// It never lowers the cap, so a larger explicit request cap or class cap is
/// preserved.
pub(crate) fn effective_delegation_turn_cap(
    request_max_turns: Option<u32>,
    class: ModelLoopClass,
    session_limits: &SessionLimitsConfig,
) -> usize {
    let base = effective_turn_cap(request_max_turns, class, session_limits);
    let hard_cap = session_limits.max_turns as usize;
    let delegation = (session_limits.max_model_turns_delegation as usize).max(1);
    let delegation = if hard_cap == 0 {
        delegation
    } else {
        delegation.min(hard_cap)
    };
    base.max(delegation)
}

/// Returns the effective tool-call cap for a selected turn class.
pub(crate) fn effective_tool_cap(
    class: ModelLoopClass,
    session_limits: &SessionLimitsConfig,
) -> usize {
    match class {
        ModelLoopClass::Respond => 0,
        ModelLoopClass::InlineExecute | ModelLoopClass::WorkerInline => {
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

/// Maximum bytes for the recent-target digest handed to the execution router.
const RECENT_TARGET_DIGEST_MAX_BYTES: usize = 640;
/// Maximum number of lines included in the recent-target digest.
const RECENT_TARGET_DIGEST_MAX_ENTRIES: usize = 12;
/// Maximum bytes for one rendered field (snippet, path, or tool arguments).
const RECENT_TARGET_FIELD_MAX_BYTES: usize = 120;
/// Maximum attachment names listed for one user message.
const RECENT_TARGET_MAX_ATTACHMENTS: usize = 4;

/// Builds a compact, deterministic digest of recent conversation context.
///
/// This digest is the only channel by which prior conversation reaches the
/// execution router, so the router LLM — not a keyword heuristic — can judge
/// whether a terse follow-up ("fix it", "the pricing page we discussed") has a
/// concrete recent referent. It renders the most recent relevant events as a
/// bounded, newline-joined mini-transcript: user and assistant message snippets,
/// tool names with a compact rendering of their arguments, worker task text,
/// segment summaries, and memory paths. Deliberately plain — it hands the model
/// the recent context verbatim (bounded) and lets it decide what is a referent,
/// rather than pre-judging targets with the kind of keyword allowlist this router
/// change exists to remove. It runs on every turn with no LLM calls, omits tool
/// result bodies so it never dumps raw tool output, and is bounded in both entry
/// count and total bytes. Events at or after `current_user_sequence_num` are
/// excluded so the current request cannot count as its own referent.
pub(crate) fn recent_target_digest(
    recent_events: &[EventRecord],
    current_user_sequence_num: u64,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used_bytes = 0usize;
    // Walk newest-first so byte/entry truncation keeps the freshest context,
    // then present chronologically for the reader.
    for record in recent_events
        .iter()
        .rev()
        .filter(|record| record.sequence_num < current_user_sequence_num)
    {
        if lines.len() >= RECENT_TARGET_DIGEST_MAX_ENTRIES {
            break;
        }
        let Some(line) = digest_line(&record.event) else {
            continue;
        };
        let separator = usize::from(!lines.is_empty());
        if used_bytes + separator + line.len() > RECENT_TARGET_DIGEST_MAX_BYTES {
            break;
        }
        used_bytes += separator + line.len();
        lines.push(line);
    }
    lines.reverse();
    lines.join("\n")
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

    /// Records a tool call that a per-turn cache will serve instead of dispatching.
    ///
    /// Loop detection exists to stop unbounded identical *dispatches* (cost and hang
    /// protection). A cache-served repeat is not a dispatch: it costs nothing and
    /// cannot hang, so it must not advance the consecutive-repeat counter that trips
    /// [`ToolBudgetExhaustedReason::RepeatedToolCall`]. It still counts against
    /// `max_tool_calls` so total attempted work stays bounded, and it breaks the
    /// real-dispatch streak (an intervening cache serve means the next real dispatch
    /// is not consecutive with whatever preceded the serve). [`Self::before_tool_dispatch`]
    /// is intentionally left byte-identical for genuine dispatches.
    pub(crate) fn record_cached_serve(
        &mut self,
        invocation: &ToolInvocation,
    ) -> ToolBudgetDecision {
        self.attempted_tool_calls = self.attempted_tool_calls.saturating_add(1);
        // A cache serve is not a dispatch, so it neither extends nor is counted by the
        // consecutive-dispatch streak; clear it so later real dispatches start fresh.
        self.last_fingerprint = None;
        self.consecutive_repeats = 0;

        if self.max_tool_calls == 0 || self.attempted_tool_calls > self.max_tool_calls {
            let fingerprint = ToolFingerprint::from_invocation(invocation);
            return ToolBudgetDecision::Stop(ToolBudgetExhausted {
                attempted_tool_calls: self.attempted_tool_calls,
                max_tool_calls: self.max_tool_calls,
                tool_name: fingerprint.tool_name,
                consecutive_repeats: self.consecutive_repeats,
                reason: ToolBudgetExhaustedReason::MaxToolCallsExceeded,
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

/// Renders one recent event as a compact digest line, or `None` when the event
/// carries nothing worth surfacing.
///
/// Only the event types loaded by the recent-target event query are handled
/// explicitly; every other variant is elided by the catch-all arm. Tool result
/// bodies are omitted on purpose so the digest never dumps raw tool output.
fn digest_line(event: &Event) -> Option<String> {
    match event {
        Event::UserMessage { text, attachments } => {
            let mut line = format!(
                "user: {}",
                field(text).unwrap_or_else(|| "(no text)".to_string())
            );
            let names = attachments
                .iter()
                .filter_map(|attachment| field(&attachment.name))
                .take(RECENT_TARGET_MAX_ATTACHMENTS)
                .collect::<Vec<_>>();
            if !names.is_empty() {
                line.push_str(&format!(" [attachments: {}]", names.join(", ")));
            }
            Some(line)
        }
        Event::BrainResponse { text, .. } => {
            field(text).map(|snippet| format!("assistant: {snippet}"))
        }
        Event::WorkerMessageSent { text, .. } => {
            field(text).map(|snippet| format!("worker message: {snippet}"))
        }
        Event::WorkerSpawned { task, .. } => {
            field(task).map(|snippet| format!("worker task: {snippet}"))
        }
        Event::ToolCall {
            tool_name, input, ..
        } => {
            let name = field(tool_name).unwrap_or_else(|| "tool".to_string());
            Some(format!("tool {name}: {}", compact_json(input)))
        }
        Event::ToolError { tool_name, .. } => Some(format!(
            "tool {} failed",
            field(tool_name).unwrap_or_else(|| "tool".to_string())
        )),
        // The tool call already names what was operated on, and the result body is
        // the raw-dump risk, so tool results contribute nothing to the digest.
        Event::ToolResult { .. } => None,
        Event::SegmentStarted { task_summary, .. }
        | Event::SegmentCompleted { task_summary, .. } => task_summary
            .as_deref()
            .and_then(field)
            .map(|summary| format!("segment: {summary}")),
        Event::MemoryRead { path, .. } => field(path).map(|path| format!("memory read: {path}")),
        Event::MemoryWrite { path, .. } => field(path).map(|path| format!("memory write: {path}")),
        Event::MemoryIngest { source_path, .. } => {
            field(source_path).map(|path| format!("memory ingest: {path}"))
        }
        _ => None,
    }
}

/// Collapses interior whitespace and truncates a string to one bounded field,
/// returning `None` when the string is blank.
fn field(text: &str) -> Option<String> {
    let collapsed = collapse_whitespace(text);
    (!collapsed.is_empty()).then(|| truncate_field(&collapsed))
}

/// Renders a JSON value (a tool's arguments) as compact, bounded text.
fn compact_json(value: &serde_json::Value) -> String {
    truncate_field(&collapse_whitespace(&value.to_string()))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates on a UTF-8 char boundary, appending an ellipsis when text is cut.
fn truncate_field(text: &str) -> String {
    if text.len() <= RECENT_TARGET_FIELD_MAX_BYTES {
        return text.to_string();
    }
    let mut end = RECENT_TARGET_FIELD_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_string();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::config::SessionLimitsConfig;
    use moa_core::{
        events::Event, types::channel::Attachment, types::completion::ToolInvocation,
        types::events_stream::EventRecord, types::identifiers::SessionId,
        types::identifiers::ToolCallId,
    };
    use uuid::Uuid;

    use super::{
        ModelLoopClass, RECENT_TARGET_DIGEST_MAX_BYTES, RECENT_TARGET_DIGEST_MAX_ENTRIES,
        ToolBudgetDecision, ToolBudgetExhaustedReason, ToolBudgetState, ToolFingerprint,
        effective_delegation_turn_cap, effective_tool_cap, effective_turn_cap, progress_cap,
        recent_target_digest,
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
    fn recent_target_digest_excludes_current_and_orders_chronologically() {
        // Pins: the current request is never its own referent, and prior events render
        // oldest-first so the router reads them in conversation order.
        let events = vec![
            record(
                10,
                Event::UserMessage {
                    text: "start from crates/moa-core/src/lib.rs".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                11,
                Event::ToolCall {
                    tool_id: ToolCallId(Uuid::now_v7()),
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "cargo test" }),
                    hand_id: None,
                },
            ),
            record(
                12,
                Event::UserMessage {
                    text: "now fix it".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];

        let digest = recent_target_digest(&events, 12);
        assert_eq!(
            digest.lines().collect::<Vec<_>>(),
            vec![
                "user: start from crates/moa-core/src/lib.rs",
                "tool bash: {\"cmd\":\"cargo test\"}",
            ]
        );
        assert!(
            !digest.contains("now fix it"),
            "the current request must not be part of its own digest"
        );
    }

    #[test]
    fn recent_target_digest_renders_referents_without_manufacturing_from_a_bare_fence() {
        // Pins: tool arguments carry concrete file paths and URLs to the router verbatim,
        // a bare code fence in user prose manufactures no target (it stays inside the
        // snippet the router judges), attachments are named, and the builder is deterministic.
        let events = vec![
            record(
                1,
                Event::UserMessage {
                    text: "review this ```code``` and the pricing page".to_string(),
                    attachments: vec![attachment("report.md")],
                },
            ),
            record(
                2,
                Event::ToolCall {
                    tool_id: ToolCallId(Uuid::now_v7()),
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "file_read".to_string(),
                    input: serde_json::json!({ "path": "crates/moa-brain/src/lib.rs" }),
                    hand_id: None,
                },
            ),
            record(
                3,
                Event::ToolCall {
                    tool_id: ToolCallId(Uuid::now_v7()),
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "http_get".to_string(),
                    input: serde_json::json!({ "url": "https://example.com/pricing" }),
                    hand_id: None,
                },
            ),
            record(
                4,
                Event::WorkerSpawned {
                    worker_id: "child-1".to_string(),
                    path: "/root/research".to_string(),
                    task: "summarize the findings".to_string(),
                    budget_tokens: 512,
                },
            ),
        ];

        let digest = recent_target_digest(&events, 100);
        assert_eq!(
            digest,
            recent_target_digest(&events, 100),
            "digest must be deterministic for a fixed event list"
        );
        assert!(
            digest.contains(
                "user: review this ```code``` and the pricing page [attachments: report.md]"
            ),
            "user snippet and attachment names must render without a manufactured target: {digest}"
        );
        assert!(
            digest.contains("tool file_read: {\"path\":\"crates/moa-brain/src/lib.rs\"}"),
            "tool arguments carry the concrete path verbatim: {digest}"
        );
        assert!(
            digest.contains("tool http_get: {\"url\":\"https://example.com/pricing\"}"),
            "tool arguments carry the concrete URL verbatim: {digest}"
        );
        assert!(
            digest.contains("worker task: summarize the findings"),
            "worker task text surfaces as a snippet: {digest}"
        );
        assert!(digest.len() <= RECENT_TARGET_DIGEST_MAX_BYTES);
        assert!(digest.lines().count() <= RECENT_TARGET_DIGEST_MAX_ENTRIES);
    }

    #[test]
    fn recent_target_digest_keeps_newest_events_within_bounds() {
        // Pins: with more relevant events than fit, the digest retains the newest ones
        // and stays within the entry and byte caps.
        let events = (1..=40u64)
            .map(|seq| {
                record(
                    seq,
                    Event::UserMessage {
                        text: format!("message number {seq} about crates/moa-core/src/f{seq}.rs"),
                        attachments: Vec::new(),
                    },
                )
            })
            .collect::<Vec<_>>();

        let digest = recent_target_digest(&events, 1000);
        assert!(digest.lines().count() <= RECENT_TARGET_DIGEST_MAX_ENTRIES);
        assert!(digest.len() <= RECENT_TARGET_DIGEST_MAX_BYTES);
        assert!(
            digest.contains("message number 40"),
            "the newest event must be retained: {digest}"
        );
        assert!(
            !digest.contains("message number 1 "),
            "the oldest events must be dropped once the cap is reached: {digest}"
        );
    }

    #[test]
    fn effective_turn_cap_applies_class_and_hard_limits() {
        // Pins: Respond, Inline Execute, and Worker Inline defaults are bounded by the
        // global hard cap and never encode Durable or NeedsInput loop classes.
        let limits = limits();
        assert_eq!(
            effective_turn_cap(None, ModelLoopClass::Respond, &limits),
            1
        );
        assert_eq!(
            effective_turn_cap(None, ModelLoopClass::InlineExecute, &limits),
            4
        );
        assert_eq!(
            effective_turn_cap(None, ModelLoopClass::WorkerInline, &limits),
            4
        );

        let tiny_hard_cap = SessionLimitsConfig {
            max_turns: 1,
            standard_max_turns: 4,
            ..limits
        };
        assert_eq!(
            effective_turn_cap(None, ModelLoopClass::InlineExecute, &tiny_hard_cap),
            1
        );
    }

    #[test]
    fn effective_turn_cap_bounds_explicit_request_caps() {
        // Pins: explicit workflow/request caps continue to work but cannot exceed the hard cap.
        let limits = limits();
        assert_eq!(
            effective_turn_cap(Some(3), ModelLoopClass::InlineExecute, &limits),
            3
        );
        assert_eq!(
            effective_turn_cap(Some(30), ModelLoopClass::InlineExecute, &limits),
            8
        );
        assert_eq!(
            effective_turn_cap(Some(0), ModelLoopClass::InlineExecute, &limits),
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
            effective_turn_cap(None, ModelLoopClass::InlineExecute, &limits),
            usize::MAX
        );
        assert_eq!(
            effective_turn_cap(Some(5), ModelLoopClass::InlineExecute, &limits),
            5
        );
        assert_eq!(progress_cap(usize::MAX), None);
    }

    #[test]
    fn delegation_turn_cap_raises_base_without_lowering() {
        // Pins: once a turn delegates, the loop budget escalates to
        // max_model_turns_delegation (bounded by the hard cap), never drops below a
        // larger base cap, and preserves unlimited global semantics.
        let limits = SessionLimitsConfig {
            max_turns: 50,
            simple_max_turns: 1,
            standard_max_turns: 6,
            max_model_turns_delegation: 12,
            max_tool_calls: 30,
            ..SessionLimitsConfig::default()
        };
        // The sweep drives an explicit request cap of 6; delegation escalates it to 12.
        assert_eq!(
            effective_turn_cap(Some(6), ModelLoopClass::InlineExecute, &limits),
            6
        );
        assert_eq!(
            effective_delegation_turn_cap(Some(6), ModelLoopClass::InlineExecute, &limits),
            12
        );
        // A class-default standard turn escalates from 6 to 12 as well.
        assert_eq!(
            effective_delegation_turn_cap(None, ModelLoopClass::InlineExecute, &limits),
            12
        );
        // Escalation is bounded by the global hard cap.
        let tight = SessionLimitsConfig {
            max_turns: 8,
            ..limits.clone()
        };
        assert_eq!(
            effective_delegation_turn_cap(None, ModelLoopClass::InlineExecute, &tight),
            8
        );
        // A larger explicit request cap is never lowered to the delegation cap.
        assert_eq!(
            effective_delegation_turn_cap(Some(20), ModelLoopClass::InlineExecute, &limits),
            20
        );
        // Unlimited global semantics survive delegation.
        let unlimited = SessionLimitsConfig {
            max_turns: 0,
            ..limits
        };
        assert_eq!(
            effective_delegation_turn_cap(None, ModelLoopClass::InlineExecute, &unlimited),
            usize::MAX
        );
    }

    #[test]
    fn effective_tool_cap_uses_selected_class() {
        // Pins: Respond receives no tool budget while both root and worker Inline loops
        // retain the existing bounded work budget.
        let limits = limits();
        assert_eq!(effective_tool_cap(ModelLoopClass::Respond, &limits), 0);
        assert_eq!(
            effective_tool_cap(ModelLoopClass::InlineExecute, &limits),
            12
        );
        assert_eq!(
            effective_tool_cap(ModelLoopClass::WorkerInline, &limits),
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
    fn cached_serves_never_trip_the_loop_detector_but_still_count_toward_the_cap() {
        // Pins: a cache-served repeat is not a dispatch, so identical cached serves never
        // trip RepeatedToolCall regardless of count; they still increment the attempted
        // count and stop the turn at max_tool_calls with MaxToolCallsExceeded.
        let mut budget = ToolBudgetState::new(4, 3);
        let call = invocation(
            "cached",
            "file_read",
            serde_json::json!({"path": ".moa/skills/x/SKILL.md"}),
        );

        for attempted in 1..=4 {
            assert_eq!(
                budget.record_cached_serve(&call),
                ToolBudgetDecision::Allow {
                    attempted_tool_calls: attempted
                },
                "cached serve {attempted} must be allowed without a loop stop"
            );
        }
        let ToolBudgetDecision::Stop(stop) = budget.record_cached_serve(&call) else {
            panic!("the attempt beyond max_tool_calls must stop");
        };
        assert_eq!(stop.reason, ToolBudgetExhaustedReason::MaxToolCallsExceeded);
        assert_eq!(stop.attempted_tool_calls, 5);
        assert_eq!(budget.attempted_tool_calls(), 5);
    }

    #[test]
    fn a_cached_serve_breaks_the_consecutive_dispatch_streak() {
        // Pins: two identical real dispatches build a streak of 2; an intervening cached
        // serve clears it, so the next identical dispatch restarts at 1 instead of tripping
        // the loop detector as a third consecutive call.
        let mut budget = ToolBudgetState::new(10, 3);
        let dispatched = invocation("d", "bash", serde_json::json!({"cmd": "cargo test"}));
        let cached = invocation("c", "file_read", serde_json::json!({"path": "a/SKILL.md"}));

        assert!(matches!(
            budget.before_tool_dispatch(&dispatched),
            ToolBudgetDecision::Allow { .. }
        ));
        assert!(matches!(
            budget.before_tool_dispatch(&dispatched),
            ToolBudgetDecision::Allow { .. }
        ));
        assert!(matches!(
            budget.record_cached_serve(&cached),
            ToolBudgetDecision::Allow { .. }
        ));

        assert_eq!(
            budget.before_tool_dispatch(&dispatched),
            ToolBudgetDecision::Allow {
                attempted_tool_calls: 4
            },
            "the dispatch after a cached serve is consecutive #1 again, not the tripping #3"
        );
    }

    #[test]
    fn identical_failing_file_reads_still_trip_the_loop_detector() {
        // Pins: only successful reads are cached, so a miss-path file_read loop takes the
        // real-dispatch path (before_tool_dispatch) and still trips at the threshold.
        let mut budget = ToolBudgetState::new(10, 3);
        let miss = invocation(
            "m",
            "file_read",
            serde_json::json!({"path": ".moa/skills/x.md"}),
        );

        for _ in 1..=2 {
            assert!(matches!(
                budget.before_tool_dispatch(&miss),
                ToolBudgetDecision::Allow { .. }
            ));
        }
        let ToolBudgetDecision::Stop(stop) = budget.before_tool_dispatch(&miss) else {
            panic!("third identical failing read should stop before dispatch");
        };
        assert_eq!(
            stop.reason,
            ToolBudgetExhaustedReason::RepeatedToolCall { threshold: 3 }
        );
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

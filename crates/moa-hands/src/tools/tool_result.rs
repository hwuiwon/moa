//! Built-in tools for reading and searching persisted tool-result output.

use std::time::Instant;

use async_trait::async_trait;
use moa_core::{
    BuiltInTool, Event, EventRange, IdempotencyClass, MoaError, Result, ToolArtifactStream,
    ToolCallId, ToolContext, ToolInputShape, ToolOutput, ToolPolicySpec, read_tool_policy,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 400;
const MAX_MATCHES: usize = 100;
const MAX_CONTEXT_LINES: usize = 5;
const MAX_LINE_LENGTH: usize = 500;

/// Built-in tool that reads a line range from a prior tool result.
pub struct ToolResultReadTool;

/// Built-in tool that searches a prior tool result with ripgrep-style semantics.
pub struct ToolResultSearchTool;

#[async_trait]
impl BuiltInTool for ToolResultReadTool {
    fn name(&self) -> &'static str {
        "tool_result_read"
    }

    fn description(&self) -> &'static str {
        "Read a specific line range from an earlier tool result in this session. Use this after tool_result_search has identified the right line numbers, or when you need a specific stream such as stdout or stderr from a stored large output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool_id": {
                    "type": "string",
                    "description": "The MOA tool result id shown in the prior <tool_result id=\"...\"> context block."
                },
                "stream": {
                    "type": "string",
                    "enum": ["combined", "stdout", "stderr"],
                    "description": "Optional stream to read. Default: combined."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based first line to read. Default: 1."
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based last line to read. If omitted, returns a bounded chunk starting at start_line."
                }
            },
            "required": ["tool_id"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        read_tool_policy(ToolInputShape::Json)
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::Idempotent
    }

    fn max_output_tokens(&self) -> u32 {
        8_000
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let params: ToolResultReadInput = serde_json::from_value(input.clone())?;
        let Some(session_store) = ctx.session_store else {
            return Err(MoaError::Unsupported(
                "tool_result_read requires a session-backed tool router".to_string(),
            ));
        };

        let started_at = Instant::now();
        let tool_id = parse_tool_id(&params.tool_id)?;
        let stream = params.stream.unwrap_or(ToolArtifactStream::Combined);
        let text = load_tool_result_text(session_store, ctx.session.id, tool_id, stream).await?;
        let lines = text.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start_line = params.start_line.unwrap_or(1).max(1);
        let default_end_line = start_line.saturating_add(DEFAULT_READ_LINES - 1);
        let mut end_line = params.end_line.unwrap_or(default_end_line).max(start_line);
        let mut notes = Vec::new();

        if total_lines == 0 {
            return Ok(ToolOutput::json(
                format!(
                    "tool_result_read {tool_id} ({}) returned no text output.",
                    stream.as_str()
                ),
                json!({
                    "tool_id": tool_id,
                    "stream": stream.as_str(),
                    "total_lines": 0,
                    "start_line": 0,
                    "end_line": 0,
                    "lines": [],
                }),
                started_at.elapsed(),
            ));
        }

        if start_line > total_lines {
            return Err(MoaError::ToolError(format!(
                "tool_result_read start_line {start_line} exceeds total line count {total_lines}"
            )));
        }

        if end_line > total_lines {
            end_line = total_lines;
            notes.push(format!("end_line clamped to {total_lines}"));
        }
        if end_line - start_line + 1 > MAX_READ_LINES {
            end_line = start_line + MAX_READ_LINES - 1;
            notes.push(format!(
                "range capped at {MAX_READ_LINES} lines; request a narrower span for more detail"
            ));
        }

        let selected = lines[(start_line - 1)..end_line]
            .iter()
            .enumerate()
            .map(|(offset, line)| (start_line + offset, truncate_line(line)))
            .collect::<Vec<_>>();
        let width = end_line.to_string().len().max(2);
        let mut summary = format!(
            "tool_result_read {tool_id} ({}) lines {}-{} of {}",
            stream.as_str(),
            start_line,
            end_line,
            total_lines
        );
        if !notes.is_empty() {
            summary.push_str(&format!("\n[{}]", notes.join("; ")));
        }
        summary.push_str("\n\n");
        summary.push_str(
            &selected
                .iter()
                .map(|(line_number, line)| format!("{line_number:>width$} | {line}", width = width))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        Ok(ToolOutput::json(
            summary,
            json!({
                "tool_id": tool_id,
                "stream": stream.as_str(),
                "total_lines": total_lines,
                "start_line": start_line,
                "end_line": end_line,
                "notes": notes,
                "lines": selected.iter().map(|(line_number, line)| {
                    json!({
                        "line": line_number,
                        "text": line,
                    })
                }).collect::<Vec<_>>(),
            }),
            started_at.elapsed(),
        ))
    }
}

#[async_trait]
impl BuiltInTool for ToolResultSearchTool {
    fn name(&self) -> &'static str {
        "tool_result_search"
    }

    fn description(&self) -> &'static str {
        "Search an earlier tool result from this session using a regex or literal pattern, similar to ripgrep over persisted tool output. Prefer this before tool_result_read when you need to locate an exact line, error, warning, symbol, or test name inside a stored large output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool_id": {
                    "type": "string",
                    "description": "The MOA tool result id shown in the prior <tool_result id=\"...\"> context block."
                },
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search. Use literal=true for exact string matching."
                },
                "stream": {
                    "type": "string",
                    "enum": ["combined", "stdout", "stderr"],
                    "description": "Optional stream to search. Default: combined."
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 5,
                    "description": "Optional number of surrounding lines to include for each match."
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as a literal string instead of a regex. Default: false."
                }
            },
            "required": ["tool_id", "pattern"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        read_tool_policy(ToolInputShape::Pattern)
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::Idempotent
    }

    fn max_output_tokens(&self) -> u32 {
        4_000
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let params: ToolResultSearchInput = serde_json::from_value(input.clone())?;
        if params.pattern.trim().is_empty() {
            return Err(MoaError::ToolError(
                "tool_result_search pattern must not be empty".to_string(),
            ));
        }
        let Some(session_store) = ctx.session_store else {
            return Err(MoaError::Unsupported(
                "tool_result_search requires a session-backed tool router".to_string(),
            ));
        };

        let started_at = Instant::now();
        let tool_id = parse_tool_id(&params.tool_id)?;
        let stream = params.stream.unwrap_or(ToolArtifactStream::Combined);
        let text = load_tool_result_text(session_store, ctx.session.id, tool_id, stream).await?;
        let pattern = if params.literal.unwrap_or(false) {
            regex::escape(&params.pattern)
        } else {
            params.pattern
        };
        let regex =
            Regex::new(&pattern).map_err(|error| MoaError::ValidationError(error.to_string()))?;
        let context_lines = params.context_lines.unwrap_or(0).min(MAX_CONTEXT_LINES);
        let outcome = search_tool_result(&text, &regex, context_lines);

        Ok(ToolOutput::json(
            render_search_summary(tool_id, stream, &outcome),
            json!({
                "tool_id": tool_id,
                "stream": stream.as_str(),
                "match_count": outcome.matches.len(),
                "truncated": outcome.truncated,
                "matches": outcome.matches.iter().map(|entry| {
                    json!({
                        "line": entry.line,
                        "text": entry.text,
                        "context": entry.context.iter().map(|context| {
                            json!({
                                "line": context.line,
                                "text": context.text,
                                "is_match": context.is_match,
                            })
                        }).collect::<Vec<_>>(),
                    })
                }).collect::<Vec<_>>(),
            }),
            started_at.elapsed(),
        ))
    }
}

#[derive(Debug, Deserialize)]
struct ToolResultReadInput {
    tool_id: String,
    #[serde(default)]
    stream: Option<ToolArtifactStream>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ToolResultSearchInput {
    tool_id: String,
    pattern: String,
    #[serde(default)]
    stream: Option<ToolArtifactStream>,
    #[serde(default)]
    context_lines: Option<usize>,
    #[serde(default)]
    literal: Option<bool>,
}

#[derive(Debug)]
struct SearchOutcome {
    matches: Vec<SearchMatch>,
    truncated: bool,
}

#[derive(Debug)]
struct SearchMatch {
    line: usize,
    text: String,
    context: Vec<SearchContextLine>,
}

#[derive(Debug)]
struct SearchContextLine {
    line: usize,
    text: String,
    is_match: bool,
}

async fn load_tool_result_text(
    session_store: &dyn moa_core::SessionStore,
    session_id: moa_core::SessionId,
    tool_id: ToolCallId,
    stream: ToolArtifactStream,
) -> Result<String> {
    let events = session_store
        .get_events(session_id, EventRange::all())
        .await?;
    let output = events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::ToolResult {
                tool_id: candidate,
                output,
                ..
            } if *candidate == tool_id => Some(output),
            _ => None,
        })
        .ok_or_else(|| MoaError::ToolError(format!("tool result {tool_id} was not found")))?;

    if let Some(artifact) = output.artifact.as_ref() {
        let claim_check = artifact.claim_check(stream).ok_or_else(|| {
            MoaError::ToolError(format!(
                "tool result {tool_id} does not have a {} stream",
                stream.as_str()
            ))
        })?;
        return session_store
            .load_text_artifact(session_id, claim_check)
            .await;
    }

    match stream {
        ToolArtifactStream::Combined => Ok(output.to_text()),
        ToolArtifactStream::Stdout => {
            output
                .process_stdout()
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    MoaError::ToolError(format!(
                        "tool result {tool_id} does not have a {} stream",
                        stream.as_str()
                    ))
                })
        }
        ToolArtifactStream::Stderr => {
            output
                .process_stderr()
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    MoaError::ToolError(format!(
                        "tool result {tool_id} does not have a {} stream",
                        stream.as_str()
                    ))
                })
        }
    }
}

fn parse_tool_id(raw: &str) -> Result<ToolCallId> {
    Uuid::parse_str(raw)
        .map(ToolCallId::from)
        .map_err(|error| MoaError::ValidationError(format!("invalid tool_id `{raw}`: {error}")))
}

fn search_tool_result(text: &str, regex: &Regex, context_lines: usize) -> SearchOutcome {
    let lines = text.lines().collect::<Vec<_>>();
    let mut matches = Vec::new();
    let mut truncated = false;

    for (line_index, line) in lines.iter().enumerate() {
        if matches.len() >= MAX_MATCHES {
            truncated = true;
            break;
        }
        if !regex.is_match(line) {
            continue;
        }

        matches.push(SearchMatch {
            line: line_index + 1,
            text: truncate_line(line),
            context: collect_context(&lines, line_index, context_lines),
        });
    }

    SearchOutcome { matches, truncated }
}

fn collect_context(
    lines: &[&str],
    line_index: usize,
    context_lines: usize,
) -> Vec<SearchContextLine> {
    if context_lines == 0 {
        return Vec::new();
    }

    let start = line_index.saturating_sub(context_lines);
    let end = (line_index + context_lines + 1).min(lines.len());
    (start..end)
        .map(|context_index| SearchContextLine {
            line: context_index + 1,
            text: truncate_line(lines[context_index]),
            is_match: context_index == line_index,
        })
        .collect()
}

fn render_search_summary(
    tool_id: ToolCallId,
    stream: ToolArtifactStream,
    outcome: &SearchOutcome,
) -> String {
    let mut summary = if outcome.matches.is_empty() {
        format!(
            "No matching lines found in tool result {tool_id} ({})",
            stream.as_str()
        )
    } else {
        outcome
            .matches
            .iter()
            .map(render_search_match)
            .collect::<Vec<_>>()
            .join("\n")
    };

    if outcome.truncated {
        summary.push_str(&format!(
            "\n\n[search truncated at {} matches; narrow the pattern or search a specific stream]",
            MAX_MATCHES
        ));
    }

    summary
}

fn render_search_match(entry: &SearchMatch) -> String {
    if entry.context.is_empty() {
        return format!("{}:{}", entry.line, entry.text);
    }

    let width = entry
        .context
        .last()
        .map(|context| context.line.to_string().len())
        .unwrap_or(2)
        .max(2);
    let mut block = format!("line {}\n", entry.line);
    for context in &entry.context {
        let marker = if context.is_match { ">" } else { " " };
        block.push_str(&format!(
            "{} {:>width$} | {}\n",
            marker,
            context.line,
            context.text,
            width = width
        ));
    }
    block.trim_end().to_string()
}

fn truncate_line(line: &str) -> String {
    let char_count = line.chars().count();
    if char_count <= MAX_LINE_LENGTH {
        return line.to_string();
    }

    let prefix = line.chars().take(MAX_LINE_LENGTH).collect::<String>();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        ClaimCheck, EventFilter, PendingSignal, PendingSignalId, SessionFilter, SessionId,
        SessionMeta, SessionStatus, SessionSummary, UserId, WorkspaceId,
    };

    use super::*;

    #[derive(Clone, Default)]
    struct MockSessionStore {
        events: Vec<moa_core::EventRecord>,
        artifacts: std::collections::HashMap<String, String>,
    }

    #[async_trait]
    impl moa_core::SessionStore for MockSessionStore {
        async fn create_session(&self, _meta: SessionMeta) -> Result<SessionId> {
            Ok(SessionId::new())
        }

        async fn emit_event(&self, _session_id: SessionId, _event: Event) -> Result<u64> {
            Ok(0)
        }

        async fn store_text_artifact(
            &self,
            _session_id: SessionId,
            _text: &str,
        ) -> Result<ClaimCheck> {
            unreachable!("not used in test")
        }

        async fn load_text_artifact(
            &self,
            _session_id: SessionId,
            claim_check: &ClaimCheck,
        ) -> Result<String> {
            self.artifacts
                .get(&claim_check.blob_id)
                .cloned()
                .ok_or_else(|| MoaError::BlobNotFound(claim_check.blob_id.clone()))
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<moa_core::EventRecord>> {
            Ok(self.events.clone())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(SessionMeta {
                workspace_id: WorkspaceId::new("workspace"),
                user_id: UserId::new("user"),
                status: SessionStatus::Running,
                ..SessionMeta::default()
            })
        }

        async fn update_status(
            &self,
            _session_id: SessionId,
            _status: SessionStatus,
        ) -> Result<()> {
            Ok(())
        }

        async fn store_pending_signal(
            &self,
            _session_id: SessionId,
            _signal: PendingSignal,
        ) -> Result<PendingSignalId> {
            unreachable!("not used in test")
        }

        async fn get_pending_signals(&self, _session_id: SessionId) -> Result<Vec<PendingSignal>> {
            Ok(Vec::new())
        }

        async fn resolve_pending_signal(&self, _signal_id: PendingSignalId) -> Result<()> {
            Ok(())
        }

        async fn search_events(
            &self,
            _query: &str,
            _filter: EventFilter,
        ) -> Result<Vec<moa_core::EventRecord>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        async fn workspace_cost_since(
            &self,
            _workspace_id: &WorkspaceId,
            _since: DateTime<Utc>,
        ) -> Result<u32> {
            Ok(0)
        }

        async fn delete_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }
    }

    fn event_record(session_id: SessionId, event: Event) -> moa_core::EventRecord {
        let event_type = event.event_type();
        moa_core::EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num: 0,
            event_type,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    #[tokio::test]
    async fn tool_result_read_reads_from_persisted_artifact() {
        let session = SessionMeta::default();
        let tool_id = ToolCallId::new();
        let store = MockSessionStore {
            events: vec![event_record(
                session.id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text("stored separately", Duration::from_millis(1))
                        .with_artifact(Some(moa_core::ToolOutputArtifact {
                            combined: ClaimCheck {
                                blob_id: "blob-1".to_string(),
                                size: 24,
                                preview: "line 1".to_string(),
                            },
                            estimated_tokens: 20,
                            line_count: 3,
                            stdout: None,
                            stderr: None,
                        })),
                    original_output_tokens: Some(20),
                    success: true,
                    duration_ms: 1,
                },
            )],
            artifacts: std::collections::HashMap::from([(
                "blob-1".to_string(),
                "line 1\nline 2\nline 3\n".to_string(),
            )]),
        };
        let ctx = ToolContext {
            session: &session,
            session_store: Some(&store),
            cancel_token: None,
        };

        let output = ToolResultReadTool
            .execute(
                &json!({
                    "tool_id": tool_id.to_string(),
                    "start_line": 2,
                    "end_line": 3
                }),
                &ctx,
            )
            .await
            .expect("tool_result_read");

        assert!(output.to_text().contains("2 | line 2"));
        assert!(output.to_text().contains("3 | line 3"));
    }

    #[tokio::test]
    async fn tool_result_search_matches_inline_small_output() {
        let session = SessionMeta::default();
        let tool_id = ToolCallId::new();
        let store = MockSessionStore {
            events: vec![event_record(
                session.id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text(
                        "alpha\nbeta\nneedle\nomega\n",
                        Duration::from_millis(1),
                    ),
                    original_output_tokens: None,
                    success: true,
                    duration_ms: 1,
                },
            )],
            artifacts: std::collections::HashMap::new(),
        };
        let ctx = ToolContext {
            session: &session,
            session_store: Some(&store),
            cancel_token: None,
        };

        let output = ToolResultSearchTool
            .execute(
                &json!({
                    "tool_id": tool_id.to_string(),
                    "pattern": "needle",
                    "context_lines": 1
                }),
                &ctx,
            )
            .await
            .expect("tool_result_search");

        assert!(output.to_text().contains("line 3"));
        assert!(output.to_text().contains(">  3 | needle"));
    }
}

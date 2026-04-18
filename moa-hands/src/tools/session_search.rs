//! Built-in session-history search tool backed by `SessionStore::search_events`.

use std::time::Instant;

use async_trait::async_trait;
use moa_core::{
    BuiltInTool, Event, EventFilter, EventRecord, EventType, MoaError, Result, ToolContext,
    ToolInputShape, ToolOutput, ToolPolicySpec, read_tool_policy,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Built-in session history search tool.
pub struct SessionSearchTool;

#[async_trait]
impl BuiltInTool for SessionSearchTool {
    fn name(&self) -> &'static str {
        "session_search"
    }

    fn description(&self) -> &'static str {
        "Search the current session history for earlier tool calls, tool results, errors, and prior assistant responses that may no longer fit in the active context window."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms to match against prior session events." },
                "event_type": {
                    "type": "string",
                    "enum": ["tool_call", "tool_result", "brain_response", "error", "all"],
                    "description": "Optional event type filter."
                },
                "last_n": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Maximum number of matching events to return."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        read_tool_policy(ToolInputShape::Query)
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let params: SessionSearchInput = serde_json::from_value(input.clone())?;
        if params.query.trim().is_empty() {
            return Err(MoaError::ToolError(
                "session_search query must not be empty".to_string(),
            ));
        }

        let Some(session_store) = ctx.session_store else {
            return Err(MoaError::Unsupported(
                "session_search requires a session-backed tool router".to_string(),
            ));
        };

        let started_at = Instant::now();
        let limit = params.last_n.unwrap_or(5).clamp(1, 20);
        let filter = EventFilter {
            session_id: Some(ctx.session.id),
            workspace_id: None,
            user_id: None,
            event_types: params.event_type.event_types(),
            from_time: None,
            to_time: None,
            limit: Some(limit),
        };
        let results = session_store.search_events(&params.query, filter).await?;
        let rendered = render_results(&results);
        let structured = results
            .iter()
            .map(|record| {
                json!({
                    "sequence_num": record.sequence_num,
                    "timestamp": record.timestamp,
                    "event_type": record.event_type,
                    "tool_id": event_tool_id(record),
                    "snippet": event_snippet(record),
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolOutput::json(
            rendered,
            Value::Array(structured),
            started_at.elapsed(),
        ))
    }
}

#[derive(Debug, Deserialize)]
struct SessionSearchInput {
    query: String,
    #[serde(default)]
    event_type: SessionSearchEventType,
    #[serde(default)]
    last_n: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionSearchEventType {
    ToolCall,
    ToolResult,
    BrainResponse,
    Error,
    #[default]
    All,
}

impl SessionSearchEventType {
    fn event_types(&self) -> Option<Vec<EventType>> {
        match self {
            Self::ToolCall => Some(vec![EventType::ToolCall]),
            Self::ToolResult => Some(vec![EventType::ToolResult]),
            Self::BrainResponse => Some(vec![EventType::BrainResponse]),
            Self::Error => Some(vec![EventType::ToolError, EventType::Error]),
            Self::All => None,
        }
    }
}

fn render_results(results: &[EventRecord]) -> String {
    if results.is_empty() {
        return "No matching session events found.".to_string();
    }

    results
        .iter()
        .map(|record| {
            format!(
                "## #{sequence} {event_type} @ {timestamp}\n{snippet}\n",
                sequence = record.sequence_num,
                event_type = record.event.type_name(),
                timestamp = record.timestamp.to_rfc3339(),
                snippet = event_snippet(record),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn event_snippet(record: &EventRecord) -> String {
    truncate(match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => text.clone(),
        Event::BrainResponse { text, .. } => text.clone(),
        Event::ToolCall {
            tool_id,
            tool_name,
            input,
            ..
        } => format!("tool_id={tool_id} {tool_name}: {}", input),
        Event::ToolResult {
            tool_id,
            output,
            success,
            ..
        } => {
            format!("tool_id={tool_id} success={success}: {}", output.to_text())
        }
        Event::ToolError { tool_id, error, .. } => format!("tool_id={tool_id} {error}"),
        Event::Error { message, .. } | Event::Warning { message } => message.clone(),
        Event::Checkpoint { summary, .. } => summary.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{other:?}")),
    })
}

fn event_tool_id(record: &EventRecord) -> Option<String> {
    match &record.event {
        Event::ToolCall { tool_id, .. }
        | Event::ToolResult { tool_id, .. }
        | Event::ToolError { tool_id, .. } => Some(tool_id.to_string()),
        _ => None,
    }
}

fn truncate(text: String) -> String {
    const LIMIT: usize = 600;
    if text.chars().count() <= LIMIT {
        return text;
    }

    let prefix = text.chars().take(LIMIT - 3).collect::<String>();
    format!("{prefix}...")
}

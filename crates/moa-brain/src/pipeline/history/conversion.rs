//! Event-record to `ContextMessage` conversion for history replay.

use std::collections::HashMap;

use moa_core::{
    ContextMessage, Event, EventRecord, Result, ToolCallId, ToolContent, ToolOutput,
    ToolOutputConfig, truncate_head_tail,
};
use moa_security::wrap_untrusted_tool_output;

pub(super) fn compile_records(
    records: &[&EventRecord],
    tool_output: &ToolOutputConfig,
    file_read_paths: &HashMap<ToolCallId, String>,
) -> Result<Vec<CompiledRecordMessage>> {
    records
        .iter()
        .filter_map(|record| event_to_context_message(record, tool_output, file_read_paths))
        .collect::<Result<Vec<_>>>()
}

fn event_to_context_message(
    record: &EventRecord,
    tool_output: &ToolOutputConfig,
    file_read_paths: &HashMap<ToolCallId, String>,
) -> Option<Result<CompiledRecordMessage>> {
    match &record.event {
        Event::UserMessage { text, .. } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::user(text.clone()),
        ))),
        Event::QueuedMessage { text, .. } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::user(text.clone()),
        ))),
        Event::BrainResponse {
            text,
            thought_signature,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::assistant_with_thought_signature(
                text.clone(),
                thought_signature.clone(),
            ),
        ))),
        Event::ToolCall {
            tool_id,
            provider_tool_use_id,
            provider_thought_signature,
            tool_name,
            input,
            ..
        } => Some(
            serde_json::to_string(input)
                .map(|serialized| {
                    CompiledRecordMessage::plain(
                        ContextMessage::assistant_tool_call_with_thought_signature(
                            moa_core::ToolInvocation {
                                id: Some(
                                    provider_tool_use_id
                                        .clone()
                                        .unwrap_or_else(|| tool_id.to_string()),
                                ),
                                name: tool_name.clone(),
                                input: input.clone(),
                            },
                            format!("<tool_call name=\"{tool_name}\">{serialized}</tool_call>"),
                            provider_thought_signature.clone(),
                        ),
                    )
                })
                .map_err(Into::into),
        ),
        Event::ToolResult {
            output,
            success,
            tool_id,
            provider_tool_use_id,
            ..
        } => Some(Ok(tool_result_context_message(
            provider_tool_use_id
                .clone()
                .unwrap_or_else(|| tool_id.to_string()),
            *tool_id,
            *success,
            output,
            tool_output,
            file_read_paths.get(tool_id).cloned(),
        ))),
        Event::ToolError {
            error,
            tool_id,
            provider_tool_use_id,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(
            match provider_tool_use_id.as_ref() {
                Some(call_id) => {
                    let replayable_error = truncate_tool_result_text(error, tool_output);
                    ContextMessage::tool_result(
                        call_id.clone(),
                        format!("<tool_error id=\"{tool_id}\">{replayable_error}</tool_error>"),
                        Some(vec![ToolContent::Text {
                            text: replayable_error,
                        }]),
                    )
                }
                None => ContextMessage::tool(format!(
                    "<tool_error id=\"{tool_id}\">{error}</tool_error>"
                )),
            },
        ))),
        Event::Warning { message } => Some(Ok(CompiledRecordMessage::plain(
            ContextMessage::system(format!("<warning>{message}</warning>")),
        ))),
        Event::MemoryRead { path, scope } => {
            Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
                format!("<memory_event kind=\"read\" scope=\"{scope}\">{path}</memory_event>"),
            ))))
        }
        Event::MemoryWrite { path, summary, .. } => {
            Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
                format!("<memory_write path=\"{path}\">{summary}</memory_write>"),
            ))))
        }
        Event::MemoryIngest {
            source_name,
            source_path,
            ..
        } => Some(Ok(CompiledRecordMessage::plain(ContextMessage::system(
            format!(
                "<memory_ingest source_name=\"{source_name}\" source_path=\"{source_path}\" />"
            ),
        )))),
        _ => None,
    }
}

fn tool_result_context_message(
    tool_use_id: String,
    tool_id: ToolCallId,
    success: bool,
    output: &ToolOutput,
    tool_output: &ToolOutputConfig,
    file_read_path: Option<String>,
) -> CompiledRecordMessage {
    let replayable_text = truncate_tool_result_text(&output.to_text(), tool_output);
    let artifact_attrs = output
        .artifact
        .as_ref()
        .map(|artifact| {
            format!(
                " artifact=\"stored\" artifact_tokens=\"{}\" artifact_lines=\"{}\" artifact_streams=\"{}\"",
                artifact.estimated_tokens,
                artifact.line_count,
                artifact.available_streams().join(",")
            )
        })
        .unwrap_or_default();
    CompiledRecordMessage {
        message: ContextMessage::tool_result(
            tool_use_id.clone(),
            format!(
                "<tool_result id=\"{tool_id}\" success=\"{success}\"{artifact_attrs}>\n{}\n</tool_result>",
                wrap_untrusted_tool_output(&replayable_text)
            ),
            replayable_tool_content_blocks(output, &replayable_text, tool_output),
        ),
        tool_result: file_read_path.as_ref().map(|path| ToolResultReplayMeta {
            tool_use_id,
            tool_id,
            success,
            file_read_path: path.clone(),
        }),
    }
}

fn replayable_tool_content_blocks(
    output: &ToolOutput,
    replayable_text: &str,
    tool_output: &ToolOutputConfig,
) -> Option<Vec<ToolContent>> {
    let total_chars = output
        .content
        .iter()
        .map(tool_content_char_len)
        .sum::<usize>();

    if total_chars <= tool_output.max_replay_chars {
        return Some(output.content.clone());
    }

    Some(vec![ToolContent::Text {
        text: replayable_text.to_string(),
    }])
}

fn tool_content_char_len(content: &ToolContent) -> usize {
    match content {
        ToolContent::Text { text } => text.chars().count(),
        ToolContent::Json { data } => data.to_string().chars().count(),
    }
}

fn truncate_tool_result_text(text: &str, tool_output: &ToolOutputConfig) -> String {
    truncate_head_tail(text, tool_output.max_replay_chars, tool_output.head_ratio).0
}

#[derive(Debug, Clone)]
pub(super) struct CompiledRecordMessage {
    pub(super) message: ContextMessage,
    pub(super) tool_result: Option<ToolResultReplayMeta>,
}

impl CompiledRecordMessage {
    pub(super) fn plain(message: ContextMessage) -> Self {
        Self {
            message,
            tool_result: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ToolResultReplayMeta {
    pub(super) tool_use_id: String,
    pub(super) tool_id: ToolCallId,
    pub(super) success: bool,
    pub(super) file_read_path: String,
}

#[cfg(test)]
mod tests {
    use crate::pipeline::history::test_support::prelude::*;

    #[test]
    fn history_compiler_formats_user_and_assistant_turns() {
        let session = session();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "Hello".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::BrainResponse {
                    text: "Hi there".to_string(),
                    model: ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: 10,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 4,
                    cost_cents: 1,
                    duration_ms: 100,
                    thought_signature: None,
                },
            ),
        ];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, tokens_added) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, moa_core::MessageRole::User);
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[1].role, moa_core::MessageRole::Assistant);
        assert_eq!(messages[1].content, "Hi there");
        assert!(tokens_added > 0);
    }

    #[test]
    fn history_compiler_preserves_structured_tool_result_blocks() {
        let session = session();
        let tool_id = ToolCallId::new();
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_history".to_string()),
                output: moa_core::ToolOutput::json(
                    "1 result",
                    serde_json::json!({ "matches": ["notes/today.md"] }),
                    Duration::from_millis(7),
                ),
                original_output_tokens: None,
                success: true,
                duration_ms: 7,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_use_id.as_deref(), Some("toolu_history"));
        assert!(messages[0].content.contains("<tool_result"));
        assert_eq!(messages[0].content_blocks.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn history_compiler_truncates_oversized_tool_results_for_replay() {
        let session = session();
        let tool_id = ToolCallId::new();
        let giant = (1..=15_000)
            .map(|index| format!("src/lib.rs:{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_large".to_string()),
                output: ToolOutput {
                    content: vec![ToolContent::Text {
                        text: giant.clone(),
                    }],
                    is_error: false,
                    structured: None,
                    duration: Duration::from_millis(7),
                    truncated: false,
                    original_output_tokens: None,
                    artifact: None,
                },
                original_output_tokens: None,
                success: true,
                duration_ms: 7,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000_000).unwrap();

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert!(message.content.contains("[... ~"));
        let blocks = message
            .content_blocks
            .as_ref()
            .expect("bounded content blocks");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ToolContent::Text { text } => {
                assert!(text.contains("src/lib.rs:1"));
                assert!(text.contains("src/lib.rs:15000"));
                assert!(text.contains("[... ~"));
                assert!(text.chars().count() <= ToolOutputConfig::default().max_replay_chars);
            }
            ToolContent::Json { .. } => panic!("oversized replay should collapse to a text block"),
        }
    }

    #[test]
    fn history_compiler_preserves_structured_tool_call_invocation() {
        let session = session();
        let tool_id = ToolCallId::new();
        let events = vec![event_record(
            &session.id,
            0,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some("toolu_history_call".to_string()),
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "pwd" }),
                hand_id: None,
            },
        )];
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, _) = compiler.compile_messages(&events, 1_000).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]
                .tool_invocation
                .as_ref()
                .and_then(|invocation| invocation.id.as_deref()),
            Some("toolu_history_call")
        );
        assert_eq!(
            messages[0]
                .tool_invocation
                .as_ref()
                .map(|invocation| invocation.name.as_str()),
            Some("bash")
        );
        assert!(messages[0].content.contains("<tool_call"));
    }
}

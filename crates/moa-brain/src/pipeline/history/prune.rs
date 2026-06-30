//! Pruning and file-read deduplication for budgeted history replay.

use std::collections::HashMap;

use moa_core::{
    ContextMessage, Event, EventRecord, FileReadDedupState, SnapshotFileReadState, ToolCallId,
    ToolContent,
};
use moa_security::wrap_untrusted_tool_output;

use moa_core::estimate_text_tokens;

use super::conversion::ToolResultReplayMeta;
use super::{FILE_READ_DEDUP_PLACEHOLDER, conversion::CompiledRecordMessage};

pub(super) fn build_full_file_read_path_map(
    events: &[&EventRecord],
) -> HashMap<ToolCallId, String> {
    let mut file_reads = HashMap::new();

    for record in events {
        let Event::ToolCall {
            tool_id,
            tool_name,
            input,
            ..
        } = &record.event
        else {
            continue;
        };

        if tool_name != "file_read" {
            continue;
        }

        let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };

        if input.get("start_line").is_some() || input.get("end_line").is_some() {
            continue;
        }

        file_reads.insert(*tool_id, path.to_string());
    }

    file_reads
}

pub(super) fn latest_full_file_read_results(
    events: &[&EventRecord],
    file_read_paths: &HashMap<ToolCallId, String>,
) -> HashMap<String, ToolCallId> {
    let mut latest_results = HashMap::new();

    for record in events {
        let Event::ToolResult { tool_id, .. } = &record.event else {
            continue;
        };

        let Some(path) = file_read_paths.get(tool_id) else {
            continue;
        };

        latest_results.insert(path.clone(), *tool_id);
    }

    latest_results
}

pub(super) fn deduplicate_file_reads(
    messages: &mut [CompiledRecordMessage],
    latest_file_reads: &HashMap<String, ToolCallId>,
) -> DeduplicationStats {
    let mut stats = DeduplicationStats::default();

    for compiled in messages {
        let Some(tool_result) = compiled.tool_result.as_ref() else {
            continue;
        };
        let Some(latest_tool_id) = latest_file_reads.get(&tool_result.file_read_path) else {
            continue;
        };
        if tool_result.tool_id == *latest_tool_id {
            continue;
        }

        let previous_tokens = estimate_text_tokens(&compiled.message.content);
        compiled.message = placeholder_tool_result_message(tool_result);
        let placeholder_tokens = estimate_text_tokens(&compiled.message.content);
        stats.deduplicated_count += 1;
        stats.tokens_saved += previous_tokens.saturating_sub(placeholder_tokens);
    }

    stats
}

pub(super) fn build_file_read_dedup_state(
    messages: &[CompiledRecordMessage],
) -> FileReadDedupState {
    let mut latest_reads = HashMap::new();

    for (index, compiled) in messages.iter().enumerate() {
        let Some(tool_result) = compiled.tool_result.as_ref() else {
            continue;
        };
        if compiled
            .message
            .content
            .contains(FILE_READ_DEDUP_PLACEHOLDER)
        {
            continue;
        }

        latest_reads.insert(
            tool_result.file_read_path.clone(),
            SnapshotFileReadState {
                message_index: index,
                tool_use_id: tool_result.tool_use_id.clone(),
                tool_id: tool_result.tool_id,
                success: tool_result.success,
            },
        );
    }

    FileReadDedupState { latest_reads }
}

pub(super) fn placeholder_tool_result_message(
    tool_result: &ToolResultReplayMeta,
) -> ContextMessage {
    let placeholder = FILE_READ_DEDUP_PLACEHOLDER.to_string();

    ContextMessage::tool_result(
        tool_result.tool_use_id.clone(),
        format!(
            "<tool_result id=\"{}\" success=\"{}\">\n{}\n</tool_result>",
            tool_result.tool_id,
            tool_result.success,
            wrap_untrusted_tool_output(&placeholder)
        ),
        Some(vec![ToolContent::Text { text: placeholder }]),
    )
}

pub(super) fn placeholder_tool_result_from_snapshot(
    file_read_path: &str,
    tool_result: &SnapshotFileReadState,
) -> ContextMessage {
    let replay_meta = ToolResultReplayMeta {
        tool_use_id: tool_result.tool_use_id.clone(),
        tool_id: tool_result.tool_id,
        success: tool_result.success,
        file_read_path: file_read_path.to_string(),
    };
    placeholder_tool_result_message(&replay_meta)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DeduplicationStats {
    pub(super) deduplicated_count: usize,
    pub(super) tokens_saved: usize,
}

#[cfg(test)]
mod tests {
    use crate::pipeline::history::test_support::prelude::*;

    #[test]
    fn history_compiler_deduplicates_repeated_full_file_reads() {
        let session = session();
        let foo_first = ToolCallId::new();
        let bar = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let first_read = (1..=80)
            .map(|line| format!("fn first_version_{line}() {{}}\n"))
            .collect::<String>();
        let second_read = (1..=80)
            .map(|line| format!("fn latest_version_{line}() {{}}\n"))
            .collect::<String>();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 2, foo_first, "toolu_foo_first", &first_read),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "bar read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                bar,
                "toolu_bar",
                json!({ "path": "src/bar.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                5,
                bar,
                "toolu_bar",
                "fn bar() {\n    keep_me();\n}",
            ),
            event_record(
                &session.id,
                6,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                7,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 8, foo_second, "toolu_foo_second", &second_read),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        let first_foo_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_first"))
            .expect("first foo result present");
        let second_foo_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_second"))
            .expect("second foo result present");
        let bar_result = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_bar"))
            .expect("bar result present");

        assert_eq!(
            first_foo_result.content_blocks,
            Some(vec![ToolContent::Text {
                text: FILE_READ_DEDUP_PLACEHOLDER.to_string(),
            }])
        );
        assert_eq!(
            first_foo_result.tool_use_id.as_deref(),
            Some("toolu_foo_first")
        );
        assert!(second_foo_result.content.contains("latest_version_80"));
        assert!(bar_result.content.contains("keep_me"));
        assert_eq!(compiled.deduplication.deduplicated_count, 1);
        assert!(compiled.deduplication.tokens_saved > 0);
    }

    #[test]
    fn history_compiler_does_not_deduplicate_recent_turn_file_reads() {
        let session = session();
        let foo_first = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "setup".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "first foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                2,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                3,
                foo_first,
                "toolu_foo_first",
                "fn foo() {\n    first_recent();\n}",
            ),
            event_record(
                &session.id,
                4,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                5,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                6,
                foo_second,
                "toolu_foo_second",
                "fn foo() {\n    second_recent();\n}",
            ),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 2);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        assert_eq!(compiled.deduplication.deduplicated_count, 0);
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("first_recent"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("second_recent"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .all(|message| !message.content.contains(FILE_READ_DEDUP_PLACEHOLDER))
        );
    }

    #[test]
    fn history_compiler_does_not_deduplicate_partial_file_reads() {
        let session = session();
        let partial_one = ToolCallId::new();
        let partial_two = ToolCallId::new();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first partial".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                partial_one,
                "toolu_partial_one",
                json!({ "path": "src/foo.rs", "start_line": 1, "end_line": 40 }),
            ),
            file_read_tool_result(
                &session.id,
                2,
                partial_one,
                "toolu_partial_one",
                "[showing lines 1-40 of 200 total in src/foo.rs]\n     1\tfn foo() {}",
            ),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second partial".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                partial_two,
                "toolu_partial_two",
                json!({ "path": "src/foo.rs", "start_line": 41, "end_line": 80 }),
            ),
            file_read_tool_result(
                &session.id,
                5,
                partial_two,
                "toolu_partial_two",
                "[showing lines 41-80 of 200 total in src/foo.rs]\n    41\tfn bar() {}",
            ),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        assert_eq!(compiled.deduplication.deduplicated_count, 0);
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("showing lines 1-40"))
        );
        assert!(
            compiled
                .messages
                .iter()
                .any(|message| message.content.contains("showing lines 41-80"))
        );
    }

    #[tokio::test]
    async fn history_processor_reports_file_read_deduplication_metadata() {
        let session = session();
        let foo_first = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let first_read = (1..=80)
            .map(|line| format!("fn first_version_{line}() {{}}\n"))
            .collect::<String>();
        let second_read = (1..=80)
            .map(|line| format!("fn latest_version_{line}() {{}}\n"))
            .collect::<String>();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "first foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                foo_first,
                "toolu_foo_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 2, foo_first, "toolu_foo_first", &first_read),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second foo read".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                foo_second,
                "toolu_foo_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 5, foo_second, "toolu_foo_second", &second_read),
        ];
        let mut ctx = WorkingContext::new(&session, capabilities());
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let output = compiler
            .process(&mut ctx)
            .await
            .expect("history should process");

        assert_eq!(
            output.metadata.get("file_reads_deduplicated"),
            Some(&json!(1))
        );
        assert!(
            output
                .metadata
                .get("tokens_saved_by_dedup")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|value| value > 0)
        );
        assert!(
            ctx.messages
                .iter()
                .any(|message| message.content.contains(FILE_READ_DEDUP_PLACEHOLDER))
        );
    }
}

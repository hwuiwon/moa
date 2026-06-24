//! Context snapshot loading and incremental history replay.
//!
//! This remains one module because snapshot loading, replay, and migration
//! helpers share compiled-history state and must preserve replay ordering.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use moa_core::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, ContextMessage, ContextSnapshot, Event, EventRecord,
    FileReadDedupState, Result, SequenceNum, SnapshotFileReadState, WorkingContext,
};
use moa_observability::record_turn_snapshot_load;

use crate::compaction::recent_turn_boundary;
use crate::pipeline::estimate_tokens;

use super::budgeting::keep_budgeted_older_messages;
use super::conversion::{CompiledRecordMessage, compile_records};
use super::prune::{
    build_file_read_dedup_state, build_full_file_read_path_map, deduplicate_file_reads,
    latest_full_file_read_results, placeholder_tool_result_from_snapshot,
};
use super::{CompiledHistory, FILE_READ_DEDUP_PLACEHOLDER, HistoryCompiler};

const MAX_INCREMENTAL_DELTA_EVENTS: usize = 50;

impl HistoryCompiler {
    pub(super) async fn load_snapshot(
        &self,
        ctx: &WorkingContext,
        stage_inputs_hash: u64,
    ) -> Result<Option<ContextSnapshot>> {
        if !self.snapshot_config.enabled {
            return Ok(None);
        }

        let started_at = Instant::now();
        let snapshot = self.session_store.get_snapshot(ctx.session_id).await;
        match snapshot {
            Ok(Some(snapshot))
                if snapshot.is_current_version()
                    && snapshot.stage_inputs_hash == stage_inputs_hash =>
            {
                record_turn_snapshot_load(started_at.elapsed(), true);
                Ok(Some(snapshot))
            }
            Ok(Some(snapshot)) => {
                record_turn_snapshot_load(started_at.elapsed(), false);
                tracing::warn!(
                    session_id = %ctx.session_id,
                    snapshot_version = snapshot.format_version,
                    expected_version = CONTEXT_SNAPSHOT_FORMAT_VERSION,
                    stored_hash = snapshot.stage_inputs_hash,
                    expected_hash = stage_inputs_hash,
                    "context snapshot drift detected; falling back to full replay"
                );
                Ok(None)
            }
            Ok(None) => {
                record_turn_snapshot_load(started_at.elapsed(), false);
                Ok(None)
            }
            Err(error) => {
                record_turn_snapshot_load(started_at.elapsed(), false);
                tracing::warn!(
                    session_id = %ctx.session_id,
                    error = %error,
                    "context snapshot load failed; falling back to full replay"
                );
                Ok(None)
            }
        }
    }

    pub(super) fn compile_messages_from_snapshot(
        &self,
        snapshot: &ContextSnapshot,
        delta_events: &[EventRecord],
        remaining_budget: usize,
    ) -> Option<Result<CompiledHistory>> {
        if delta_events.len() > MAX_INCREMENTAL_DELTA_EVENTS {
            tracing::warn!(
                delta_events = delta_events.len(),
                max_delta_events = MAX_INCREMENTAL_DELTA_EVENTS,
                "incremental history delta too large; falling back to full replay"
            );
            return None;
        }

        if delta_events
            .iter()
            .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        {
            return None;
        }

        let delta_refs = delta_events.iter().collect::<Vec<_>>();
        let recent_start = recent_turn_boundary(&delta_refs, self.compaction.recent_turns_verbatim);
        let (older_events, recent_events) = delta_refs.split_at(recent_start);
        let file_read_paths = build_full_file_read_path_map(&delta_refs);
        let replay_latest_reads = latest_full_file_read_results(&delta_refs, &file_read_paths);

        let recent_messages =
            match compile_records(recent_events, &self.tool_output, &file_read_paths) {
                Ok(records) => records,
                Err(error) => return Some(Err(error)),
            };
        let mut older_messages =
            match compile_records(older_events, &self.tool_output, &file_read_paths) {
                Ok(records) => records,
                Err(error) => return Some(Err(error)),
            };

        let mut latest_tool_ids = snapshot
            .file_read_dedup_state
            .latest_reads
            .iter()
            .map(|(path, state)| (path.clone(), state.tool_id))
            .collect::<HashMap<_, _>>();
        latest_tool_ids.extend(
            replay_latest_reads
                .iter()
                .map(|(path, tool_id)| (path.clone(), *tool_id)),
        );

        let mut deduplication = deduplicate_file_reads(&mut older_messages, &latest_tool_ids);
        let mut snapshotted_messages = snapshot.messages.clone();
        let mut next_snapshot_state = snapshot.file_read_dedup_state.clone();

        for path in replay_latest_reads.keys() {
            let Some(previous) = next_snapshot_state.latest_reads.remove(path) else {
                continue;
            };
            if previous.message_index >= snapshotted_messages.len() {
                continue;
            }

            let previous_tokens =
                estimate_tokens(&snapshotted_messages[previous.message_index].content);
            snapshotted_messages[previous.message_index] =
                placeholder_tool_result_from_snapshot(path, &previous);
            let placeholder_tokens =
                estimate_tokens(&snapshotted_messages[previous.message_index].content);
            deduplication.deduplicated_count += 1;
            deduplication.tokens_saved += previous_tokens.saturating_sub(placeholder_tokens);
        }

        let snapshotted_tokens = snapshotted_messages
            .iter()
            .map(|message| estimate_tokens(&message.content))
            .sum::<usize>();
        let recent_tokens = recent_messages
            .iter()
            .map(|compiled| estimate_tokens(&compiled.message.content))
            .sum::<usize>();
        let (kept_older, tokens_used) = keep_budgeted_older_messages(
            snapshotted_tokens,
            &older_messages,
            &recent_messages,
            recent_tokens,
            remaining_budget,
        );

        let mut next_snapshot_messages = snapshotted_messages.clone();
        for compiled in &kept_older {
            let message_index = next_snapshot_messages.len();
            if let Some(tool_result) = compiled.tool_result.as_ref()
                && !compiled
                    .message
                    .content
                    .contains(FILE_READ_DEDUP_PLACEHOLDER)
            {
                next_snapshot_state.latest_reads.insert(
                    tool_result.file_read_path.clone(),
                    SnapshotFileReadState {
                        message_index,
                        tool_use_id: tool_result.tool_use_id.clone(),
                        tool_id: tool_result.tool_id,
                        success: tool_result.success,
                    },
                );
            }
            next_snapshot_messages.push(compiled.message.clone());
        }

        let mut messages = next_snapshot_messages.clone();
        messages.extend(recent_messages.into_iter().map(|compiled| compiled.message));

        let snapshot = if next_snapshot_messages.is_empty() {
            None
        } else {
            Some(SnapshotHistory {
                token_count: next_snapshot_messages
                    .iter()
                    .map(|message| estimate_tokens(&message.content))
                    .sum::<usize>(),
                messages: next_snapshot_messages,
                last_sequence_num: older_events
                    .last()
                    .map(|record| record.sequence_num)
                    .unwrap_or(snapshot.last_sequence_num),
                file_read_dedup_state: next_snapshot_state,
            })
        };

        Some(Ok(CompiledHistory {
            messages,
            tokens_used,
            deduplication,
            snapshot,
        }))
    }
}

pub(super) fn snapshot_stage_inputs_hash(ctx: &WorkingContext) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(messages) = serde_json::to_string(&ctx.messages) {
        messages.hash(&mut hasher);
    }
    if let Ok(tools) = serde_json::to_string(ctx.tools()) {
        tools.hash(&mut hasher);
    }
    ctx.model_capabilities.model_id.hash(&mut hasher);
    ctx.token_budget.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn build_snapshot_state(
    records: &[CompiledRecordMessage],
    last_sequence_num: SequenceNum,
) -> SnapshotHistory {
    let messages = records
        .iter()
        .map(|compiled| compiled.message.clone())
        .collect::<Vec<_>>();
    let token_count = messages
        .iter()
        .map(|message| estimate_tokens(&message.content))
        .sum::<usize>();

    SnapshotHistory {
        last_sequence_num,
        messages,
        token_count,
        file_read_dedup_state: build_file_read_dedup_state(records),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SnapshotHistory {
    pub(super) messages: Vec<ContextMessage>,
    pub(super) last_sequence_num: SequenceNum,
    pub(super) token_count: usize,
    pub(super) file_read_dedup_state: FileReadDedupState,
}

#[cfg(test)]
mod tests {
    use crate::pipeline::history::test_support::prelude::*;

    #[test]
    fn incremental_history_replaces_prior_full_file_reads_across_turns() {
        let session = session();
        let foo_first = ToolCallId(uuid::Uuid::from_u128(1));
        let foo_second = ToolCallId(uuid::Uuid::from_u128(2));
        let prefix_events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "read foo".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                1,
                foo_first,
                "toolu_first",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                2,
                foo_first,
                "toolu_first",
                "fn foo() {\n    first_version();\n}",
            ),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "think".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                4,
                Event::BrainResponse {
                    text: "noted".to_string(),
                    thought_signature: None,
                    model: ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: 1,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 1,
                    cost_cents: 0,
                    duration_ms: 1,
                },
            ),
        ];
        let mut events = prefix_events.clone();
        events.extend([
            event_record(
                &session.id,
                5,
                Event::UserMessage {
                    text: "read foo again".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                6,
                foo_second,
                "toolu_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(
                &session.id,
                7,
                foo_second,
                "toolu_second",
                "fn foo() {\n    second_version();\n}",
            ),
        ]);
        let compiler = compiler_with_recent_turns(&session, &events, 1);
        let full = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("full replay should compile");
        let prefix = compiler
            .compile_messages_with_stats(&prefix_events, 100_000)
            .expect("prefix replay should compile");
        let snapshot = compiled_snapshot(&session, &prefix).expect("prefix should yield snapshot");
        let replay_events = events
            .iter()
            .filter(|record| record.sequence_num > snapshot.last_sequence_num)
            .cloned()
            .collect::<Vec<_>>();

        let incremental = compiler
            .compile_messages_from_snapshot(&snapshot, &replay_events, 100_000)
            .expect("incremental replay should remain active")
            .expect("incremental replay should compile");

        assert_eq!(incremental.messages, full.messages);
        let first_foo_result = incremental
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_first"))
            .expect("first foo read should still exist");
        assert_eq!(
            first_foo_result.content_blocks,
            Some(vec![ToolContent::Text {
                text: FILE_READ_DEDUP_PLACEHOLDER.to_string(),
            }])
        );
    }

    #[test]
    fn incremental_history_falls_back_when_delta_grows_too_large() {
        let session = session();
        let compiler = compiler_with_recent_turns(&session, &[], 1);
        let snapshot = ContextSnapshot {
            format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
            session_id: session.id,
            last_sequence_num: 0,
            created_at: Utc::now(),
            messages: vec![ContextMessage::user("stable")],
            file_read_dedup_state: FileReadDedupState::default(),
            token_count: 1,
            stage_inputs_hash: 1,
        };
        let delta_events = (1..=51)
            .map(|sequence_num| {
                event_record(
                    &session.id,
                    sequence_num,
                    Event::UserMessage {
                        text: format!("turn {sequence_num}"),
                        attachments: Vec::new(),
                    },
                )
            })
            .collect::<Vec<_>>();

        assert!(
            compiler
                .compile_messages_from_snapshot(&snapshot, &delta_events, 100_000)
                .is_none(),
            "large deltas should force a full replay"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        #[test]
        fn incremental_history_matches_full_replay(turns in prop::collection::vec(turn_spec_strategy(), 4..8)) {
            let session = session();
            let events = build_events_from_turn_specs(&session, &turns);
            let compiler = compiler_with_recent_turns(&session, &events, 2);
            let full = compiler
                .compile_messages_with_stats(&events, 100_000)
                .expect("full replay should compile");

            let prefix_turn_count = turns.len() - 1;
            let prefix_event_count = event_count_for_turns(&turns[..prefix_turn_count]);
            let prefix_events = &events[..prefix_event_count];
            let prefix = compiler
                .compile_messages_with_stats(prefix_events, 100_000)
                .expect("prefix replay should compile");
            let snapshot = compiled_snapshot(&session, &prefix)
                .expect("prefix should produce a reusable snapshot");
            let replay_events = events
                .iter()
                .filter(|record| record.sequence_num > snapshot.last_sequence_num)
                .cloned()
                .collect::<Vec<_>>();

            let incremental = compiler
                .compile_messages_from_snapshot(&snapshot, &replay_events, 100_000)
                .expect("incremental replay should stay active")
                .expect("incremental replay should compile");

            prop_assert_eq!(incremental.messages, full.messages);
            prop_assert_eq!(incremental.snapshot, full.snapshot);
        }
    }

    #[derive(Debug, Clone)]
    enum TestAction {
        Assistant(u8),
        FullRead { path_index: u8, version: u8 },
        PartialRead { path_index: u8, start_line: u8 },
        Bash(u8),
    }

    #[derive(Debug, Clone)]
    struct TestTurn {
        prompt_seed: u8,
        actions: Vec<TestAction>,
    }

    fn turn_spec_strategy() -> impl Strategy<Value = TestTurn> {
        (
            any::<u8>(),
            prop::collection::vec(test_action_strategy(), 0..4),
        )
            .prop_map(|(prompt_seed, actions)| TestTurn {
                prompt_seed,
                actions,
            })
    }

    fn test_action_strategy() -> impl Strategy<Value = TestAction> {
        prop_oneof![
            any::<u8>().prop_map(TestAction::Assistant),
            (0u8..3, any::<u8>()).prop_map(|(path_index, version)| TestAction::FullRead {
                path_index,
                version,
            }),
            (0u8..3, 1u8..120).prop_map(|(path_index, start_line)| TestAction::PartialRead {
                path_index,
                start_line,
            }),
            any::<u8>().prop_map(TestAction::Bash),
        ]
    }

    fn build_events_from_turn_specs(session: &SessionMeta, turns: &[TestTurn]) -> Vec<EventRecord> {
        let mut events = Vec::new();
        let mut sequence_num = 0u64;
        let mut next_tool_id = 1u128;

        for (turn_index, turn) in turns.iter().enumerate() {
            events.push(event_record(
                &session.id,
                sequence_num,
                Event::UserMessage {
                    text: format!("turn-{turn_index}-{}", turn.prompt_seed),
                    attachments: Vec::new(),
                },
            ));
            sequence_num += 1;

            for action in &turn.actions {
                match action {
                    TestAction::Assistant(seed) => {
                        events.push(event_record(
                            &session.id,
                            sequence_num,
                            Event::BrainResponse {
                                text: format!("assistant-{turn_index}-{seed}"),
                                thought_signature: None,
                                model: ModelId::new("claude-sonnet-4-6"),
                                model_tier: moa_core::ModelTier::Main,
                                input_tokens_uncached: 1,
                                input_tokens_cache_write: 0,
                                input_tokens_cache_read: 0,
                                output_tokens: 1,
                                cost_cents: 0,
                                duration_ms: 1,
                            },
                        ));
                        sequence_num += 1;
                    }
                    TestAction::FullRead {
                        path_index,
                        version,
                    } => {
                        let tool_id = ToolCallId(uuid::Uuid::from_u128(next_tool_id));
                        next_tool_id += 1;
                        let provider_id = format!("toolu_{tool_id}");
                        let path = test_path(*path_index);
                        events.push(file_read_tool_call(
                            &session.id,
                            sequence_num,
                            tool_id,
                            &provider_id,
                            json!({ "path": path }),
                        ));
                        sequence_num += 1;
                        events.push(file_read_tool_result(
                            &session.id,
                            sequence_num,
                            tool_id,
                            &provider_id,
                            &full_read_fixture(path, *version),
                        ));
                        sequence_num += 1;
                    }
                    TestAction::PartialRead {
                        path_index,
                        start_line,
                    } => {
                        let tool_id = ToolCallId(uuid::Uuid::from_u128(next_tool_id));
                        next_tool_id += 1;
                        let provider_id = format!("toolu_{tool_id}");
                        let path = test_path(*path_index);
                        let start_line = (*start_line as usize).max(1);
                        let end_line = start_line + 4;
                        events.push(file_read_tool_call(
                            &session.id,
                            sequence_num,
                            tool_id,
                            &provider_id,
                            json!({ "path": path, "start_line": start_line, "end_line": end_line }),
                        ));
                        sequence_num += 1;
                        events.push(file_read_tool_result(
                        &session.id,
                        sequence_num,
                        tool_id,
                        &provider_id,
                        &format!(
                            "[showing lines {start_line}-{end_line} of 200 total in {path}]\n{start_line}\tpartial-{turn_index}-{start_line}"
                        ),
                    ));
                        sequence_num += 1;
                    }
                    TestAction::Bash(seed) => {
                        let tool_id = ToolCallId(uuid::Uuid::from_u128(next_tool_id));
                        next_tool_id += 1;
                        let provider_id = format!("toolu_{tool_id}");
                        events.push(event_record(
                        &session.id,
                        sequence_num,
                        Event::ToolCall {
                            tool_id,
                            provider_tool_use_id: Some(provider_id.clone()),
                            provider_thought_signature: None,
                            tool_name: "bash".to_string(),
                            input: json!({ "command": format!("echo bash-{turn_index}-{seed}") }),
                            hand_id: None,
                        },
                    ));
                        sequence_num += 1;
                        events.push(event_record(
                            &session.id,
                            sequence_num,
                            Event::ToolResult {
                                tool_id,
                                provider_tool_use_id: Some(provider_id),
                                output: ToolOutput::text(
                                    format!("bash-output-{turn_index}-{seed}"),
                                    Duration::default(),
                                ),
                                original_output_tokens: None,
                                success: true,
                                duration_ms: 1,
                            },
                        ));
                        sequence_num += 1;
                    }
                }
            }
        }

        events
    }

    fn event_count_for_turns(turns: &[TestTurn]) -> usize {
        turns
            .iter()
            .map(|turn| {
                1 + turn
                    .actions
                    .iter()
                    .map(test_action_event_count)
                    .sum::<usize>()
            })
            .sum()
    }

    fn test_action_event_count(action: &TestAction) -> usize {
        match action {
            TestAction::Assistant(_) => 1,
            TestAction::FullRead { .. } | TestAction::PartialRead { .. } | TestAction::Bash(_) => 2,
        }
    }

    fn test_path(index: u8) -> &'static str {
        match index % 3 {
            0 => "src/foo.rs",
            1 => "src/bar.rs",
            _ => "src/baz.rs",
        }
    }

    fn full_read_fixture(path: &str, version: u8) -> String {
        (1..=12)
            .map(|line| format!("{path}-v{version}-line{line}\n"))
            .collect()
    }
}

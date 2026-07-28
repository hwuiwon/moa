//! Cache-stable file-read deduplication planning for budgeted history replay.
//!
//! History replay never rewrites already-compiled messages between
//! checkpoints, so provider prompt caches keep matching the frozen prefix.
//! Deduplication decisions are made once, deterministically, from the event
//! stream:
//!
//! - a full re-read whose replayed text is byte-identical to the previous
//!   content-bearing read of the same path renders as a short pointer on the
//!   **new** side (`FILE_READ_UNCHANGED_PLACEHOLDER`), leaving old bytes
//!   untouched;
//! - a full re-read with changed content renders in full and marks the older
//!   read stale; the stale copy is replaced with
//!   `FILE_READ_DEDUP_PLACEHOLDER` only once a `Checkpoint` event lands after
//!   the superseding read — the same moment the compiled history head changes
//!   and the provider cache is invalidated anyway.

use std::collections::{HashMap, HashSet};

use moa_core::{
    events::Event, types::events_stream::EventRecord, types::events_stream::SequenceNum,
    types::identifiers::ToolCallId, types::snapshot::FileReadDedupState,
    types::snapshot::SnapshotFileReadState,
};

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

/// Maps tool calls to a `(tool name, canonical input)` supersession key.
///
/// A later successful result with the same key makes the earlier result
/// stale (a re-run of the same invocation is authoritative). Full file reads
/// are excluded — they have their own content-hash-aware path — as are
/// child-report tools, whose "results" are coordination signals rather than
/// re-runnable outputs. `serde_json` maps are key-sorted, so serializing the
/// input is deterministic.
pub(super) fn build_tool_invocation_key_map(
    events: &[&EventRecord],
    file_read_paths: &HashMap<ToolCallId, String>,
) -> HashMap<ToolCallId, String> {
    let mut keys = HashMap::new();

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
        if file_read_paths.contains_key(tool_id)
            || moa_core::types::worker::tool_schema::is_child_report_tool_name(tool_name)
        {
            continue;
        }
        let Ok(serialized_input) = serde_json::to_string(input) else {
            continue;
        };
        keys.insert(*tool_id, format!("{tool_name}\u{1f}{serialized_input}"));
    }

    keys
}

/// Hashes the replayed tool output text for content-identity comparison.
///
/// Dedup compares the bytes the model would see again on replay, so hashing
/// the persisted rendered text (post router budgeting) is exact: two results
/// with identical replayed text are interchangeable by definition.
pub(super) fn replayed_content_hash(rendered: &str) -> String {
    blake3::hash(rendered.as_bytes()).to_hex().to_string()
}

/// Per-tool-result replay decisions computed once from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FileReadRenderPlan {
    /// Results replayed as an unchanged-content pointer (new-side dedup).
    pub(super) pointer_results: HashSet<ToolCallId>,
    /// Full results that supersede an earlier stale read of the same path.
    pub(super) superseding_results: HashSet<ToolCallId>,
    /// Stale full-read results replayed as a placeholder (checkpoint-gated).
    pub(super) stale_results: HashSet<ToolCallId>,
    /// Non-file-read results superseded by a newer successful run of the same
    /// invocation, demoted to a placeholder (checkpoint-gated).
    pub(super) demoted_results: HashSet<ToolCallId>,
}

/// Builds the render plan and the dedup walk state at the snapshot boundary.
///
/// The walk is deterministic over `(seed_state, visible events)` so full and
/// incremental replay reach identical decisions. `snapshot_boundary_seq` marks
/// the last event covered by the next snapshot; the returned state reflects the
/// walk at that point (events past the boundary still influence the plan, not
/// the persisted state).
pub(super) fn build_file_read_render_plan(
    visible_events: &[&EventRecord],
    file_read_paths: &HashMap<ToolCallId, String>,
    invocation_keys: &HashMap<ToolCallId, String>,
    latest_checkpoint_seq: Option<SequenceNum>,
    seed_state: &FileReadDedupState,
    snapshot_boundary_seq: Option<SequenceNum>,
) -> (FileReadRenderPlan, FileReadDedupState) {
    let mut plan = FileReadRenderPlan::default();
    let mut walk: HashMap<String, SnapshotFileReadState> = seed_state.latest_reads.clone();
    let mut boundary_state: Option<HashMap<String, SnapshotFileReadState>> = None;
    // Stale candidates: (stale tool id, sequence of the superseding result).
    let mut stale_candidates: Vec<(ToolCallId, SequenceNum)> = Vec::new();
    // Latest successful result per non-file-read invocation key. Demotion
    // needs no cross-snapshot seed: it only fires once a checkpoint lands
    // after the superseding result, and checkpoint deltas always fall back to
    // full replay, so incremental compiles never apply new demotions.
    let mut latest_invocations: HashMap<&str, ToolCallId> = HashMap::new();
    let mut demotion_candidates: Vec<(ToolCallId, SequenceNum)> = Vec::new();

    for record in visible_events {
        if boundary_state.is_none()
            && snapshot_boundary_seq.is_some_and(|boundary| record.sequence_num > boundary)
        {
            boundary_state = Some(walk.clone());
        }

        let Event::ToolResult {
            tool_id,
            output,
            success,
            ..
        } = &record.event
        else {
            continue;
        };
        if *success && let Some(key) = invocation_keys.get(tool_id) {
            if let Some(previous) = latest_invocations.insert(key.as_str(), *tool_id) {
                demotion_candidates.push((previous, record.sequence_num));
            }
            continue;
        }
        let Some(path) = file_read_paths.get(tool_id) else {
            continue;
        };

        let content_hash = replayed_content_hash(&output.to_text());
        match walk.get(path) {
            Some(previous) if previous.content_hash == content_hash => {
                plan.pointer_results.insert(*tool_id);
            }
            Some(previous) => {
                plan.superseding_results.insert(*tool_id);
                stale_candidates.push((previous.tool_id, record.sequence_num));
                walk.insert(
                    path.clone(),
                    SnapshotFileReadState {
                        tool_id: *tool_id,
                        content_hash,
                    },
                );
            }
            None => {
                walk.insert(
                    path.clone(),
                    SnapshotFileReadState {
                        tool_id: *tool_id,
                        content_hash,
                    },
                );
            }
        }
    }

    for (stale_tool_id, superseding_seq) in stale_candidates {
        if latest_checkpoint_seq.is_some_and(|checkpoint_seq| checkpoint_seq > superseding_seq) {
            plan.stale_results.insert(stale_tool_id);
        }
    }
    for (demoted_tool_id, superseding_seq) in demotion_candidates {
        if latest_checkpoint_seq.is_some_and(|checkpoint_seq| checkpoint_seq > superseding_seq) {
            plan.demoted_results.insert(demoted_tool_id);
        }
    }

    let boundary_state = match (boundary_state, snapshot_boundary_seq) {
        (Some(state), _) => state,
        // No visible event lay past the boundary: the walk itself is the state.
        (None, Some(_)) => walk,
        // No snapshot boundary requested (no older events survive).
        (None, None) => seed_state.latest_reads.clone(),
    };

    (
        plan,
        FileReadDedupState {
            latest_reads: boundary_state,
        },
    )
}

/// Token savings recorded when render-plan placeholders replace full content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DeduplicationStats {
    pub(super) deduplicated_count: usize,
    /// Non-file-read results demoted as superseded invocations.
    pub(super) demoted_count: usize,
    pub(super) tokens_saved: usize,
}

impl DeduplicationStats {
    pub(super) fn absorb(&mut self, other: DeduplicationStats) {
        self.deduplicated_count += other.deduplicated_count;
        self.demoted_count += other.demoted_count;
        self.tokens_saved += other.tokens_saved;
    }
}

#[cfg(test)]
mod tests {
    use crate::pipeline::history::test_support::prelude::*;

    #[test]
    fn history_compiler_pointers_identical_full_file_rereads_on_the_new_side() {
        // Pins: a byte-identical full re-read replays as a pointer on the NEW
        // side while the earlier content-bearing read keeps its bytes, so the
        // frozen history prefix stays cache-stable.
        let session = session();
        let foo_first = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let content = (1..=80)
            .map(|line| format!("fn same_version_{line}() {{}}\n"))
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
            file_read_tool_result(&session.id, 2, foo_first, "toolu_foo_first", &content),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second read".to_string(),
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
            file_read_tool_result(&session.id, 5, foo_second, "toolu_foo_second", &content),
        ];
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let compiled = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");

        let first = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_first"))
            .expect("first read present");
        let second = compiled
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_second"))
            .expect("second read present");

        assert!(
            first.content.contains("same_version_80"),
            "content-bearing first read keeps its bytes"
        );
        assert!(
            second.content.contains(FILE_READ_UNCHANGED_PLACEHOLDER),
            "identical re-read replays as a pointer: {}",
            second.content
        );
        assert_eq!(compiled.deduplication.deduplicated_count, 1);
        assert!(compiled.deduplication.tokens_saved > 0);
    }

    #[test]
    fn history_compiler_defers_stale_read_rewrite_until_a_checkpoint_lands() {
        // Pins: a changed-content re-read never rewrites the older read's
        // bytes between checkpoints; the older read is replaced with the stale
        // placeholder only once a Checkpoint event lands after the superseding
        // read.
        let session = session();
        let foo_first = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let first_read = (1..=80)
            .map(|line| format!("fn first_version_{line}() {{}}\n"))
            .collect::<String>();
        let second_read = (1..=80)
            .map(|line| format!("fn latest_version_{line}() {{}}\n"))
            .collect::<String>();
        let mut events = vec![
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
                    text: "second read".to_string(),
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
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let before_checkpoint = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");
        let first_before = before_checkpoint
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_first"))
            .expect("first read present");
        let second_before = before_checkpoint
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_second"))
            .expect("second read present");
        assert!(
            first_before.content.contains("first_version_80"),
            "older read keeps its bytes before a checkpoint"
        );
        assert!(
            second_before.content.contains("latest_version_80"),
            "changed re-read replays in full"
        );
        assert!(
            second_before.content.contains("supersedes_stale_read"),
            "changed re-read carries the supersession marker: {}",
            second_before.content
        );

        // A checkpoint that summarizes only the first user message leaves both
        // reads visible while opening the old-side rewrite gate.
        events.push(event_record(
            &session.id,
            6,
            Event::Checkpoint {
                summary: "summary".to_string(),
                events_summarized: 1,
                token_count: 2,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens: 10,
                output_tokens: 2,
                cost_cents: 0,
            },
        ));
        let after_checkpoint = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile after checkpoint");
        let first_after = after_checkpoint
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some("toolu_foo_first"))
            .expect("first read still present");
        assert!(
            first_after.content.contains(FILE_READ_DEDUP_PLACEHOLDER),
            "stale read collapses once a checkpoint lands: {}",
            first_after.content
        );
    }

    #[test]
    fn history_compiler_demotes_superseded_invocations_only_after_a_checkpoint() {
        // Pins: a repeated successful run of the same tool invocation demotes
        // the OLDER result to a placeholder only once a Checkpoint event lands
        // after the superseding run — never between checkpoints, so the frozen
        // prefix stays byte-stable. Different inputs never demote.
        let session = session();
        let first = ToolCallId::new();
        let second = ToolCallId::new();
        let other_input = ToolCallId::new();
        let mut events = Vec::new();
        let mut seq = 0u64;
        fn push(session: &SessionMeta, events: &mut Vec<EventRecord>, seq: &mut u64, event: Event) {
            events.push(event_record(&session.id, *seq, event));
            *seq += 1;
        }
        fn bash_exchange(
            session: &SessionMeta,
            events: &mut Vec<EventRecord>,
            seq: &mut u64,
            tool_id: ToolCallId,
            cmd: &str,
            out: &str,
        ) {
            push(
                session,
                events,
                seq,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: Some(format!("toolu_{tool_id}")),
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: json!({ "cmd": cmd }),
                    hand_id: None,
                },
            );
            push(
                session,
                events,
                seq,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: Some(format!("toolu_{tool_id}")),
                    output: ToolOutput::text(out.repeat(40), Duration::default()),
                    original_output_tokens: None,
                    success: true,
                    duration_ms: 1,
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
                },
            );
        }
        push(
            &session,
            &mut events,
            &mut seq,
            Event::UserMessage {
                text: "first status".to_string(),
                attachments: Vec::new(),
            },
        );
        bash_exchange(
            &session,
            &mut events,
            &mut seq,
            first,
            "git status",
            "dirty-tree ",
        );
        bash_exchange(
            &session,
            &mut events,
            &mut seq,
            other_input,
            "git log -1",
            "latest-commit ",
        );
        push(
            &session,
            &mut events,
            &mut seq,
            Event::UserMessage {
                text: "second status".to_string(),
                attachments: Vec::new(),
            },
        );
        bash_exchange(
            &session,
            &mut events,
            &mut seq,
            second,
            "git status",
            "clean-tree ",
        );
        let compiler = compiler_with_recent_turns(&session, &events, 0);

        let before = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile");
        assert!(
            before
                .messages
                .iter()
                .all(|message| !message.content.contains(SUPERSEDED_TOOL_RESULT_PLACEHOLDER)),
            "no checkpoint fired, so nothing is demoted"
        );

        events.push(event_record(
            &session.id,
            seq,
            Event::Checkpoint {
                summary: "summary".to_string(),
                events_summarized: 1,
                token_count: 2,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens: 10,
                output_tokens: 2,
                cost_cents: 0,
            },
        ));
        let after = compiler
            .compile_messages_with_stats(&events, 100_000)
            .expect("history should compile after checkpoint");
        let first_message = after
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some(&format!("toolu_{first}")))
            .expect("first status result present");
        let second_message = after
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some(&format!("toolu_{second}")))
            .expect("second status result present");
        let other_message = after
            .messages
            .iter()
            .find(|message| message.tool_use_id.as_deref() == Some(&format!("toolu_{other_input}")))
            .expect("other invocation present");
        assert!(
            first_message
                .content
                .contains(SUPERSEDED_TOOL_RESULT_PLACEHOLDER),
            "older run of the same invocation demotes after a checkpoint: {}",
            first_message.content
        );
        assert!(
            second_message.content.contains("clean-tree"),
            "the newest run stays verbatim"
        );
        assert!(
            other_message.content.contains("latest-commit"),
            "a different invocation is never demoted"
        );
        assert_eq!(after.deduplication.demoted_count, 1);
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

    #[test]
    fn recompiling_after_identical_reread_keeps_frozen_prefix_byte_identical() {
        // Pins: compiling turn N and then turn N+1 (which re-reads a file with
        // identical content) leaves every turn-N message byte-identical, so
        // the provider prompt cache keeps matching the frozen prefix.
        let session = session();
        let foo_first = ToolCallId::new();
        let foo_second = ToolCallId::new();
        let content = (1..=40)
            .map(|line| format!("fn stable_{line}() {{}}\n"))
            .collect::<String>();
        let turn_one = vec![
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
            file_read_tool_result(&session.id, 2, foo_first, "toolu_first", &content),
        ];
        let mut both_turns = turn_one.clone();
        both_turns.extend([
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "read foo again".to_string(),
                    attachments: Vec::new(),
                },
            ),
            file_read_tool_call(
                &session.id,
                4,
                foo_second,
                "toolu_second",
                json!({ "path": "src/foo.rs" }),
            ),
            file_read_tool_result(&session.id, 5, foo_second, "toolu_second", &content),
        ]);
        let compiler = compiler_with_recent_turns(&session, &both_turns, 0);

        let first_compile = compiler
            .compile_messages_with_stats(&turn_one, 100_000)
            .expect("turn one should compile");
        let second_compile = compiler
            .compile_messages_with_stats(&both_turns, 100_000)
            .expect("both turns should compile");

        assert_eq!(
            &second_compile.messages[..first_compile.messages.len()],
            &first_compile.messages[..],
            "turn-one messages must stay byte-identical after the re-read"
        );
    }
}

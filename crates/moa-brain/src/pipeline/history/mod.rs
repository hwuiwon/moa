//! Stage 8: compiles session history into context messages and owns checkpoint compaction.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    config::CompactionConfig, config::ContextSnapshotConfig, config::ToolOutputConfig,
    error::Result, events::Event, traits::ContextProcessor, traits::LLMProvider,
    traits::SessionStore, types::context::ContextMessage, types::context::ProcessorOutput,
    types::context::WorkingContext, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::events_stream::SequenceNum,
    types::snapshot::CONTEXT_SNAPSHOT_FORMAT_VERSION, types::snapshot::ContextSnapshot,
    types::snapshot::FileReadDedupState,
};
use serde_json::json;

use crate::compaction::{
    latest_checkpoint_state, non_checkpoint_events, recent_turn_boundary, unsummarized_events,
};

use moa_core::{types::context::estimate_text_tokens, types::context::sum_message_tokens};

mod budgeting;
mod checkpoint;
mod compaction;
mod conversion;
mod errors;
mod prune;

#[cfg(test)]
mod test_support;

use budgeting::keep_budgeted_older_messages;
use checkpoint::{SnapshotHistory, build_snapshot_state, snapshot_stage_inputs_hash};
use conversion::{answered_worker_inputs, child_report_tool_ids, compile_records};
pub(crate) use errors::preserved_error_messages;
use prune::{
    DeduplicationStats, build_file_read_render_plan, build_full_file_read_path_map,
    build_tool_invocation_key_map,
};

pub(crate) const FILE_READ_DEDUP_PLACEHOLDER: &str =
    "[file previously read — see latest version below]";
pub(crate) const FILE_READ_UNCHANGED_PLACEHOLDER: &str = "[file content unchanged since the \
     earlier read of this path above; re-read the file if that copy is no longer visible]";
pub(crate) const SUPERSEDED_TOOL_RESULT_PLACEHOLDER: &str = "[superseded tool result — a newer \
     run of the same invocation appears later in this session; use session_search if the old \
     output is needed]";
pub(crate) const HISTORY_START_INDEX_METADATA_KEY: &str = "_moa.history.start_index";
pub(crate) const HISTORY_END_INDEX_METADATA_KEY: &str = "_moa.history.end_index";
/// Metadata key used by the history stage to expose the latest reusable context snapshot.
pub const HISTORY_SNAPSHOT_METADATA_KEY: &str = "_moa.history.snapshot";

pub struct HistoryCompiler {
    session_store: Arc<dyn SessionStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
    compaction: CompactionConfig,
    tool_output: ToolOutputConfig,
    snapshot_config: ContextSnapshotConfig,
}

impl HistoryCompiler {
    /// Creates a history compiler without automatic checkpoint generation.
    pub fn new(session_store: Arc<dyn SessionStore>) -> Self {
        Self {
            session_store,
            llm_provider: None,
            compaction: CompactionConfig::default(),
            tool_output: ToolOutputConfig::default(),
            snapshot_config: ContextSnapshotConfig::default(),
        }
    }

    /// Overrides the compaction and replay-window settings used during history compilation.
    pub fn with_compaction_config(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = compaction;
        self
    }

    /// Creates a history compiler that can emit reversible checkpoint summaries.
    pub fn with_compaction(
        session_store: Arc<dyn SessionStore>,
        llm_provider: Arc<dyn LLMProvider>,
        compaction: CompactionConfig,
    ) -> Self {
        Self {
            session_store,
            llm_provider: Some(llm_provider),
            compaction,
            tool_output: ToolOutputConfig::default(),
            snapshot_config: ContextSnapshotConfig::default(),
        }
    }

    /// Overrides the tool-output truncation settings used during history replay.
    pub fn with_tool_output_config(mut self, tool_output: ToolOutputConfig) -> Self {
        self.tool_output = tool_output;
        self
    }

    /// Overrides the snapshot settings used for incremental history replay.
    pub fn with_snapshot_config(mut self, snapshot_config: ContextSnapshotConfig) -> Self {
        self.snapshot_config = snapshot_config;
        self
    }

    /// Converts event records into context messages subject to the available budget.
    pub fn compile_messages(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<(Vec<ContextMessage>, usize)> {
        let compiled = self.compile_messages_with_stats(events, remaining_budget)?;
        Ok((compiled.messages, compiled.tokens_used))
    }

    fn compile_messages_with_stats(
        &self,
        events: &[EventRecord],
        remaining_budget: usize,
    ) -> Result<CompiledHistory> {
        let checkpoint = latest_checkpoint_state(events);
        let latest_checkpoint_seq = latest_checkpoint_sequence(events);
        let all_non_checkpoint = non_checkpoint_events(events);
        let visible_events = unsummarized_events(events);
        let recent_start =
            recent_turn_boundary(&visible_events, self.compaction.recent_turns_verbatim);
        let (older_events, recent_events) = visible_events.split_at(recent_start);
        let file_read_paths = build_full_file_read_path_map(&visible_events);
        let invocation_keys = build_tool_invocation_key_map(&visible_events, &file_read_paths);
        let snapshot_boundary_seq = older_events.last().map(|record| record.sequence_num);
        let (render_plan, dedup_state) = build_file_read_render_plan(
            &visible_events,
            &file_read_paths,
            &invocation_keys,
            latest_checkpoint_seq,
            &FileReadDedupState::default(),
            snapshot_boundary_seq,
        );

        let mut stable_prefix: Vec<ContextMessage> = Vec::new();
        let mut stable_prefix_tokens = 0usize;

        if self.compaction.preserve_errors {
            let summarized_end = checkpoint
                .as_ref()
                .map(|state| state.events_summarized.min(all_non_checkpoint.len()))
                .unwrap_or(0);
            for message in preserved_error_messages(&all_non_checkpoint[..summarized_end]) {
                stable_prefix_tokens += estimate_text_tokens(&message.content);
                stable_prefix.push(message);
            }
        }

        if let Some(checkpoint) = checkpoint {
            // The checkpoint summary is compaction-derived conversation context, not
            // part of the byte-stable cache prefix (identity/instructions/tools). It is
            // injected as a `user` message — the same role used for the runtime reminder
            // — so it does not extend the leading System block. Rendering it as a System
            // message would push it into the cacheable prefix and invalidate prompt-cache
            // reuse every time compaction produces a new checkpoint.
            let checkpoint_message = ContextMessage::user(format!(
                "<session_checkpoint summarized_events=\"{}\">\n{}\n</session_checkpoint>",
                checkpoint.events_summarized, checkpoint.summary
            ));
            stable_prefix_tokens += estimate_text_tokens(&checkpoint_message.content);
            stable_prefix.push(checkpoint_message);
        }

        // Compute child-report tool ids and answered worker-input requests over the FULL visible
        // window (both slices) so a tool call/result pair — or a NeedsInput signal and its answer
        // — split across the older/recent boundary is still paired/suppressed. Computing them
        // per-slice would emit a dangling provider `tool_result` with no matching `tool_use`.
        let answered_input_requests = answered_worker_inputs(&visible_events);
        let child_report_ids = child_report_tool_ids(&visible_events);
        let (recent_messages, recent_stats) = compile_records(
            recent_events,
            &self.tool_output,
            &render_plan,
            &answered_input_requests,
            &child_report_ids,
        )?;
        let (older_messages, older_stats) = compile_records(
            older_events,
            &self.tool_output,
            &render_plan,
            &answered_input_requests,
            &child_report_ids,
        )?;
        let mut deduplication = older_stats;
        deduplication.absorb(recent_stats);
        let recent_tokens = sum_message_tokens(&recent_messages);
        let (kept_older, tokens_used) = keep_budgeted_older_messages(
            stable_prefix_tokens,
            &older_messages,
            &recent_messages,
            recent_tokens,
            remaining_budget,
        );

        let mut snapshot_messages = stable_prefix;
        snapshot_messages.extend(kept_older);
        let snapshot = older_events.last().map(|record| {
            build_snapshot_state(
                snapshot_messages.clone(),
                record.sequence_num,
                dedup_state.clone(),
            )
        });

        let mut messages = snapshot_messages;
        messages.extend(recent_messages);

        Ok(CompiledHistory {
            messages,
            tokens_used,
            deduplication,
            snapshot,
            compaction: Default::default(),
        })
    }
}

/// Returns the sequence number of the newest `Checkpoint` event in the log.
fn latest_checkpoint_sequence(events: &[EventRecord]) -> Option<SequenceNum> {
    events.iter().rev().find_map(|record| {
        matches!(record.event, Event::Checkpoint { .. }).then_some(record.sequence_num)
    })
}

/// Where and why this turn's compiled history diverged from the prior turn's.
///
/// The prior context snapshot covers the stable-prefix and kept-older region
/// only, so byte churn inside the recent window is not measured here; the
/// snapshot region is exactly the span provider prompt caches can reuse.
struct HistoryDivergenceReport {
    cause: &'static str,
    first_divergent_index: Option<usize>,
    /// Estimated tokens of previously compiled history past the divergence
    /// point — the span a provider prompt cache can no longer serve.
    tokens_invalidated_downstream: usize,
}

fn divergence_report(
    prior: Option<&ContextSnapshot>,
    current: &[ContextMessage],
    checkpoint_emitted: bool,
) -> HistoryDivergenceReport {
    let Some(prior) = prior else {
        return HistoryDivergenceReport {
            cause: "no_prior_snapshot",
            first_divergent_index: None,
            tokens_invalidated_downstream: 0,
        };
    };

    let shared = prior
        .messages
        .iter()
        .zip(current.iter())
        .take_while(|(previous, next)| previous == next)
        .count();
    if shared == prior.messages.len() {
        return HistoryDivergenceReport {
            cause: "append_only",
            first_divergent_index: None,
            tokens_invalidated_downstream: 0,
        };
    }

    let cause = if checkpoint_emitted {
        "checkpoint"
    } else if current.get(shared).is_some_and(|message| {
        message.content.contains(FILE_READ_DEDUP_PLACEHOLDER)
            || message.content.contains(FILE_READ_UNCHANGED_PLACEHOLDER)
    }) {
        "dedup_rewrite"
    } else if current.get(shared).is_some_and(|next| {
        // A head drop removes prior messages, so the current message at the
        // divergence point reappears further along the prior sequence.
        prior.messages[shared..]
            .iter()
            .skip(1)
            .take(16)
            .any(|previous| previous == next)
    }) {
        "budget_head_drop"
    } else {
        "unknown"
    };

    HistoryDivergenceReport {
        cause,
        first_divergent_index: Some(shared),
        tokens_invalidated_downstream: sum_message_tokens(&prior.messages[shared..]),
    }
}

#[async_trait]
impl ContextProcessor for HistoryCompiler {
    fn name(&self) -> &str {
        "history"
    }

    fn stage(&self) -> u8 {
        8
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let history_start_index = ctx.messages.len();
        let remaining_budget = ctx.token_budget.saturating_sub(ctx.token_count);
        let stage_inputs_hash = snapshot_stage_inputs_hash(ctx);
        let gate_open = self.compaction_gate_open(ctx).await?;
        // Loaded unconditionally: gate-open turns full-replay, but the prior
        // snapshot is still the comparison base for divergence attribution.
        let snapshot_load = self.load_snapshot(ctx, stage_inputs_hash).await?;
        let mut loaded_snapshot = None;

        let (compiled, delete_snapshot_if_empty) = if gate_open {
            // Compaction might fire this turn: read the full log once, compact,
            // and compile from that same read (folding in any new checkpoint).
            (
                self.compile_full_messages_compacting(ctx, remaining_budget)
                    .await?,
                self.snapshot_config.enabled,
            )
        } else {
            let stored_snapshot_present = snapshot_load.stored_snapshot_present;
            if let Some(snapshot) = snapshot_load.snapshot.clone() {
                loaded_snapshot = Some(snapshot.clone());
                // Fast path: the gate proved compaction cannot fire, so replay only
                // the bounded delta on top of the reusable snapshot — no full read.
                let delta_events = self
                    .session_store
                    .get_events(
                        ctx.session_id,
                        EventRange {
                            from_seq: Some(snapshot.last_sequence_num.saturating_add(1)),
                            ..EventRange::default()
                        },
                    )
                    .await?;
                (
                    match self.compile_messages_from_snapshot(
                        &snapshot,
                        &delta_events,
                        remaining_budget,
                    ) {
                        Some(result) => result?,
                        None => self.compile_full_messages(ctx, remaining_budget).await?,
                    },
                    stored_snapshot_present,
                )
            } else {
                (
                    self.compile_full_messages(ctx, remaining_budget).await?,
                    stored_snapshot_present,
                )
            }
        };
        let divergence = divergence_report(
            snapshot_load.snapshot.as_ref(),
            &compiled.messages,
            compiled.compaction.checkpoint_emitted,
        );

        if compiled.deduplication.deduplicated_count > 0 {
            tracing::info!(
                deduplicated = compiled.deduplication.deduplicated_count,
                tokens_saved = compiled.deduplication.tokens_saved,
                "deduplicated file read results in history compilation"
            );
        }
        let messages = compiled.messages;
        let tokens_added = compiled.tokens_used;
        let items_included = messages
            .iter()
            .map(|message| format!("{:?}", message.role))
            .collect::<Vec<_>>();

        ctx.extend_messages(messages);
        ctx.insert_metadata(HISTORY_START_INDEX_METADATA_KEY, json!(history_start_index));
        ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(ctx.messages.len()));
        // Frozen-history boundary for provider cache breakpoints: everything
        // before the trailing user run replays byte-identically next turn.
        // Later stages insert per-turn sections at (not before) this index,
        // so the boundary stays valid in the final request message order.
        ctx.insert_metadata(
            moa_core::types::completion::STABLE_HISTORY_END_METADATA_KEY,
            json!(crate::pipeline::trailing_user_insertion_index(
                &ctx.messages
            )),
        );
        if self.snapshot_config.enabled {
            if let Some(snapshot) = compiled.snapshot.as_ref() {
                let next_snapshot = ContextSnapshot {
                    format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
                    session_id: ctx.session_id,
                    last_sequence_num: snapshot.last_sequence_num,
                    created_at: chrono::Utc::now(),
                    messages: snapshot.messages.clone(),
                    file_read_dedup_state: snapshot.file_read_dedup_state.clone(),
                    token_count: snapshot.token_count,
                    stage_inputs_hash,
                };
                if loaded_snapshot
                    .as_ref()
                    .is_none_or(|loaded| !snapshot_payload_matches(loaded, &next_snapshot))
                {
                    ctx.insert_metadata(
                        HISTORY_SNAPSHOT_METADATA_KEY,
                        serde_json::to_value(next_snapshot)?,
                    );
                }
            } else if delete_snapshot_if_empty {
                ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, serde_json::Value::Null);
            }
        }

        let mut metadata = HashMap::new();
        metadata.insert(
            "file_reads_deduplicated".to_string(),
            json!(compiled.deduplication.deduplicated_count),
        );
        metadata.insert(
            "tokens_saved_by_dedup".to_string(),
            json!(compiled.deduplication.tokens_saved),
        );
        metadata.insert(
            "tool_results_demoted".to_string(),
            json!(compiled.deduplication.demoted_count),
        );
        metadata.insert(
            "history_divergence_cause".to_string(),
            json!(divergence.cause),
        );
        if let Some(index) = divergence.first_divergent_index {
            metadata.insert("history_divergence_index".to_string(), json!(index));
        }
        metadata.insert(
            "tokens_invalidated_downstream".to_string(),
            json!(divergence.tokens_invalidated_downstream),
        );
        metadata.insert(
            "checkpoint_emitted".to_string(),
            json!(compiled.compaction.checkpoint_emitted),
        );
        metadata.insert("tier1_applied".to_string(), json!(false));
        metadata.insert("tier2_applied".to_string(), json!(false));
        metadata.insert(
            "tier3_applied".to_string(),
            json!(compiled.compaction.checkpoint_emitted),
        );
        metadata.insert("tokens_reclaimed".to_string(), json!(0));
        metadata.insert(
            "messages_elided".to_string(),
            json!(compiled.compaction.events_summarized_delta.unwrap_or(0)),
        );
        if let Some(events_summarized) = compiled.compaction.events_summarized {
            metadata.insert("events_summarized".to_string(), json!(events_summarized));
        }
        if let Some(events_summarized_delta) = compiled.compaction.events_summarized_delta {
            metadata.insert(
                "events_summarized_delta".to_string(),
                json!(events_summarized_delta),
            );
        }
        if let Some(summary_tokens) = compiled.compaction.summary_tokens {
            metadata.insert("summary_tokens".to_string(), json!(summary_tokens));
        }

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            metadata,
            ..ProcessorOutput::default()
        })
    }
}

fn snapshot_payload_matches(left: &ContextSnapshot, right: &ContextSnapshot) -> bool {
    left.format_version == right.format_version
        && left.session_id == right.session_id
        && left.last_sequence_num == right.last_sequence_num
        && left.messages == right.messages
        && left.file_read_dedup_state == right.file_read_dedup_state
        && left.token_count == right.token_count
        && left.stage_inputs_hash == right.stage_inputs_hash
}

struct CompiledHistory {
    messages: Vec<ContextMessage>,
    tokens_used: usize,
    deduplication: DeduplicationStats,
    snapshot: Option<SnapshotHistory>,
    compaction: HistoryCompactionStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HistoryCompactionStats {
    checkpoint_emitted: bool,
    events_summarized: Option<u64>,
    events_summarized_delta: Option<u64>,
    summary_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{HISTORY_SNAPSHOT_METADATA_KEY, checkpoint::snapshot_stage_inputs_hash};
    use crate::pipeline::history::test_support::prelude::*;

    #[tokio::test]
    async fn history_processor_loads_events_directly_from_session_store() {
        let session = session();
        let events = vec![event_record(
            &session.id,
            0,
            Event::UserMessage {
                text: "Hello".to_string(),
                attachments: Vec::new(),
            },
        )];
        let mut ctx = WorkingContext::new(&session, capabilities());
        let compiler =
            HistoryCompiler::new(Arc::new(MockSessionStore::new(session.clone(), events)));

        let output = compiler.process(&mut ctx).await.unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].content, "Hello");
        assert!(output.tokens_added > 0);
    }

    #[tokio::test]
    async fn history_processor_reports_append_only_divergence_against_prior_snapshot() {
        // Pins: with a prior snapshot whose messages are a byte-prefix of this
        // turn's compiled history, the divergence report is append_only with
        // no invalidated downstream tokens — the signal that provider prompt
        // caches keep matching.
        let session = session();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "turn one".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::BrainResponse {
                    text: "answer one".to_string(),
                    thought_signature: None,
                    model: ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::types::provider::ModelTier::Main,
                    input_tokens_uncached: 1,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 1,
                    cost_cents: 0,
                    duration_ms: 1,
                    llm_ttft_ms: None,
                },
            ),
            event_record(
                &session.id,
                2,
                Event::UserMessage {
                    text: "turn two".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                3,
                Event::BrainResponse {
                    text: "answer two".to_string(),
                    thought_signature: None,
                    model: ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::types::provider::ModelTier::Main,
                    input_tokens_uncached: 1,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 1,
                    cost_cents: 0,
                    duration_ms: 1,
                    llm_ttft_ms: None,
                },
            ),
            event_record(
                &session.id,
                4,
                Event::UserMessage {
                    text: "turn three".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), events.clone()));
        let compiler = compiler_with_recent_turns(&session, &events, 1);
        let mut ctx = WorkingContext::new(&session, capabilities());
        let prefix = compiler
            .compile_messages_with_stats(&events[..4], 100_000)
            .expect("prefix should compile");
        let mut snapshot = compiled_snapshot(&session, &prefix).expect("prefix yields snapshot");
        snapshot.stage_inputs_hash = snapshot_stage_inputs_hash(&ctx);
        store
            .put_snapshot(session.id, snapshot)
            .await
            .expect("store snapshot");
        let compiler = HistoryCompiler::new(store).with_compaction_config(CompactionConfig {
            recent_turns_verbatim: 1,
            ..CompactionConfig::default()
        });

        let output = compiler.process(&mut ctx).await.expect("history compiles");

        assert_eq!(
            output.metadata.get("history_divergence_cause"),
            Some(&json!("append_only"))
        );
        assert_eq!(
            output.metadata.get("tokens_invalidated_downstream"),
            Some(&json!(0))
        );
    }

    #[tokio::test]
    async fn history_processor_skips_snapshot_delete_when_no_snapshot_exists() {
        let session = session();
        let events = vec![event_record(
            &session.id,
            0,
            Event::UserMessage {
                text: "Hello".to_string(),
                attachments: Vec::new(),
            },
        )];
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::new(store.clone());
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();

        assert!(
            !ctx.metadata().contains_key(HISTORY_SNAPSHOT_METADATA_KEY),
            "no snapshot metadata means the bridge has no snapshot write or delete to perform"
        );
        assert_eq!(
            store.snapshot_delete_count().await,
            0,
            "a missing snapshot should not be deleted on every short-history turn"
        );
    }

    #[tokio::test]
    async fn history_processor_skips_snapshot_write_when_loaded_snapshot_is_unchanged() {
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
                Event::UserMessage {
                    text: "Follow-up".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let mut ctx = WorkingContext::new(&session, capabilities());
        let snapshot_messages = vec![ContextMessage::user("Hello".to_string())];
        store
            .put_snapshot(
                session.id,
                ContextSnapshot {
                    format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
                    session_id: session.id,
                    last_sequence_num: 0,
                    created_at: Utc::now(),
                    token_count: moa_core::types::context::sum_message_tokens(&snapshot_messages),
                    messages: snapshot_messages,
                    file_read_dedup_state: FileReadDedupState::default(),
                    stage_inputs_hash: snapshot_stage_inputs_hash(&ctx),
                },
            )
            .await
            .unwrap();
        let compiler = HistoryCompiler::new(store.clone());

        compiler.process(&mut ctx).await.unwrap();

        assert!(
            !ctx.metadata().contains_key(HISTORY_SNAPSHOT_METADATA_KEY),
            "unchanged loaded snapshot should not ask the bridge to upsert it again"
        );
        assert_eq!(
            store.snapshot_write_count().await,
            1,
            "only the test setup should write the snapshot"
        );
    }
}

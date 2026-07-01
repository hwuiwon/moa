//! Stage 7: compiles session history into context messages.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, CompactionConfig, ContextMessage, ContextProcessor,
    ContextSnapshot, ContextSnapshotConfig, EventRange, EventRecord, LLMProvider, ProcessorOutput,
    Result, SessionStore, ToolOutputConfig, WorkingContext,
};
use serde_json::json;

use crate::compaction::{
    latest_checkpoint_state, non_checkpoint_events, recent_turn_boundary, unsummarized_events,
};

use moa_core::estimate_text_tokens;

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
use conversion::{
    CompiledRecordMessage, answered_worker_inputs, child_report_tool_ids, compile_records,
};
pub(crate) use errors::preserved_error_messages;
use prune::{
    DeduplicationStats, build_full_file_read_path_map, deduplicate_file_reads,
    latest_full_file_read_results,
};

pub(crate) const FILE_READ_DEDUP_PLACEHOLDER: &str =
    "[file previously read — see latest version below]";
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
        let all_non_checkpoint = non_checkpoint_events(events);
        let visible_events = unsummarized_events(events);
        let recent_start =
            recent_turn_boundary(&visible_events, self.compaction.recent_turns_verbatim);
        let (older_events, recent_events) = visible_events.split_at(recent_start);
        let file_read_paths = build_full_file_read_path_map(&visible_events);
        let latest_file_reads = latest_full_file_read_results(&visible_events, &file_read_paths);

        let mut stable_prefix = Vec::new();
        let mut stable_prefix_tokens = 0usize;

        if self.compaction.preserve_errors {
            let summarized_end = checkpoint
                .as_ref()
                .map(|state| state.events_summarized.min(all_non_checkpoint.len()))
                .unwrap_or(0);
            for message in preserved_error_messages(&all_non_checkpoint[..summarized_end]) {
                stable_prefix_tokens += estimate_text_tokens(&message.content);
                stable_prefix.push(CompiledRecordMessage::plain(message));
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
            stable_prefix.push(CompiledRecordMessage::plain(checkpoint_message));
        }

        // Compute child-report tool ids and answered worker-input requests over the FULL visible
        // window (both slices) so a tool call/result pair — or a NeedsInput signal and its answer
        // — split across the older/recent boundary is still paired/suppressed. Computing them
        // per-slice would emit a dangling provider `tool_result` with no matching `tool_use`.
        let answered_input_requests = answered_worker_inputs(&visible_events);
        let child_report_ids = child_report_tool_ids(&visible_events);
        let recent_messages = compile_records(
            recent_events,
            &self.tool_output,
            &file_read_paths,
            &answered_input_requests,
            &child_report_ids,
        )?;
        let mut older_messages = compile_records(
            older_events,
            &self.tool_output,
            &file_read_paths,
            &answered_input_requests,
            &child_report_ids,
        )?;
        let deduplication = deduplicate_file_reads(&mut older_messages, &latest_file_reads);
        let recent_tokens = recent_messages
            .iter()
            .map(|compiled| estimate_text_tokens(&compiled.message.content))
            .sum::<usize>();
        let (kept_older, tokens_used) = keep_budgeted_older_messages(
            stable_prefix_tokens,
            &older_messages,
            &recent_messages,
            recent_tokens,
            remaining_budget,
        );

        let mut snapshot_records = stable_prefix.clone();
        snapshot_records.extend(kept_older.iter().cloned());
        let snapshot = older_events
            .last()
            .map(|record| build_snapshot_state(&snapshot_records, record.sequence_num));

        let mut final_records = snapshot_records.clone();
        final_records.extend(recent_messages);
        let messages = final_records
            .iter()
            .map(|compiled| compiled.message.clone())
            .collect();

        Ok(CompiledHistory {
            messages,
            tokens_used,
            deduplication,
            snapshot,
        })
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
        let checkpoint_emitted = self.maybe_emit_checkpoint(ctx).await?;

        let compiled = if !checkpoint_emitted
            && let Some(snapshot) = self.load_snapshot(ctx, stage_inputs_hash).await?
        {
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
            match self.compile_messages_from_snapshot(&snapshot, &delta_events, remaining_budget) {
                Some(result) => result?,
                None => self.compile_full_messages(ctx, remaining_budget).await?,
            }
        } else {
            self.compile_full_messages(ctx, remaining_budget).await?
        };

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
        if let Some(snapshot) = compiled.snapshot.as_ref() {
            ctx.insert_metadata(
                HISTORY_SNAPSHOT_METADATA_KEY,
                serde_json::to_value(ContextSnapshot {
                    format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
                    session_id: ctx.session_id,
                    last_sequence_num: snapshot.last_sequence_num,
                    created_at: chrono::Utc::now(),
                    messages: snapshot.messages.clone(),
                    file_read_dedup_state: snapshot.file_read_dedup_state.clone(),
                    token_count: snapshot.token_count,
                    stage_inputs_hash,
                })?,
            );
        } else {
            ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, serde_json::Value::Null);
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

        Ok(ProcessorOutput {
            tokens_added,
            items_included,
            metadata,
            ..ProcessorOutput::default()
        })
    }
}

struct CompiledHistory {
    messages: Vec<ContextMessage>,
    tokens_used: usize,
    deduplication: DeduplicationStats,
    snapshot: Option<SnapshotHistory>,
}

#[cfg(test)]
mod tests {
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
}

//! Stage 5: compiles session history into context messages.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use moa_core::{
    CONTEXT_SNAPSHOT_FORMAT_VERSION, CompactionConfig, ContextMessage, ContextProcessor,
    ContextSnapshot, ContextSnapshotConfig, Event, EventRange, EventRecord, FileReadDedupState,
    LLMProvider, ModelTask, ProcessorOutput, Result, SequenceNum, SessionStore,
    SnapshotFileReadState, ToolCallId, ToolContent, ToolOutput, ToolOutputConfig, WorkingContext,
    record_turn_snapshot_load, truncate_head_tail,
};
use moa_security::wrap_untrusted_tool_output;
use serde_json::json;

use crate::compaction::{
    latest_checkpoint_state, maybe_compact_events, non_checkpoint_events, recent_turn_boundary,
    unsummarized_events,
};

use super::estimate_tokens;

const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";
const MAX_INCREMENTAL_DELTA_EVENTS: usize = 50;
pub(crate) const HISTORY_START_INDEX_METADATA_KEY: &str = "_moa.history.start_index";
pub(crate) const HISTORY_END_INDEX_METADATA_KEY: &str = "_moa.history.end_index";
pub(crate) const HISTORY_SNAPSHOT_METADATA_KEY: &str = "_moa.history.snapshot";

/// Compiles session events into conversational context.
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
                stable_prefix_tokens += estimate_tokens(&message.content);
                stable_prefix.push(CompiledRecordMessage::plain(message));
            }
        }

        if let Some(checkpoint) = checkpoint {
            let checkpoint_message = ContextMessage::system(format!(
                "<session_checkpoint summarized_events=\"{}\">\n{}\n</session_checkpoint>",
                checkpoint.events_summarized, checkpoint.summary
            ));
            stable_prefix_tokens += estimate_tokens(&checkpoint_message.content);
            stable_prefix.push(CompiledRecordMessage::plain(checkpoint_message));
        }

        let recent_messages = compile_records(recent_events, &self.tool_output, &file_read_paths)?;
        let mut older_messages =
            compile_records(older_events, &self.tool_output, &file_read_paths)?;
        let deduplication = deduplicate_file_reads(&mut older_messages, &latest_file_reads);
        let recent_tokens = recent_messages
            .iter()
            .map(|compiled| estimate_tokens(&compiled.message.content))
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

    async fn load_snapshot(
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

    fn compile_messages_from_snapshot(
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

#[async_trait]
impl ContextProcessor for HistoryCompiler {
    fn name(&self) -> &str {
        "history"
    }

    fn stage(&self) -> u8 {
        5
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let history_start_index = ctx.messages.len();
        let remaining_budget = ctx.token_budget.saturating_sub(ctx.token_count);
        let stage_inputs_hash = snapshot_stage_inputs_hash(ctx);

        let compiled = if let Some(snapshot) = self.load_snapshot(ctx, stage_inputs_hash).await? {
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
                    cache_controls: Vec::new(),
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

impl HistoryCompiler {
    async fn compile_full_messages(
        &self,
        ctx: &WorkingContext,
        remaining_budget: usize,
    ) -> Result<CompiledHistory> {
        let mut events = self
            .session_store
            .get_events(ctx.session_id, EventRange::all())
            .await?;

        if let Some(llm_provider) = &self.llm_provider
            && maybe_compact_events(
                &self.compaction,
                &*self.session_store,
                &**llm_provider,
                ModelTask::Summarization.tier(),
                ctx.session_id,
                ctx.token_budget,
                &events,
            )
            .await?
        {
            events = self
                .session_store
                .get_events(ctx.session_id, EventRange::all())
                .await?;
        }

        self.compile_messages_with_stats(&events, remaining_budget)
    }
}

fn snapshot_stage_inputs_hash(ctx: &WorkingContext) -> u64 {
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

fn keep_budgeted_older_messages(
    stable_prefix_tokens: usize,
    older_messages: &[CompiledRecordMessage],
    recent_messages: &[CompiledRecordMessage],
    recent_tokens: usize,
    remaining_budget: usize,
) -> (Vec<CompiledRecordMessage>, usize) {
    let mut tokens_used = stable_prefix_tokens + recent_tokens;
    let mut kept_older_reversed = Vec::new();

    for compiled in older_messages.iter().rev() {
        let message_tokens = estimate_tokens(&compiled.message.content);
        if tokens_used + message_tokens > remaining_budget {
            break;
        }

        tokens_used += message_tokens;
        kept_older_reversed.push(compiled.clone());
    }

    kept_older_reversed.reverse();

    let tokens_used = if older_messages.is_empty() && recent_messages.is_empty() {
        stable_prefix_tokens
    } else {
        tokens_used
    };

    (kept_older_reversed, tokens_used)
}

fn build_snapshot_state(
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

fn compile_records(
    records: &[&EventRecord],
    tool_output: &ToolOutputConfig,
    file_read_paths: &HashMap<ToolCallId, String>,
) -> Result<Vec<CompiledRecordMessage>> {
    records
        .iter()
        .filter_map(|record| event_to_context_message(record, tool_output, file_read_paths))
        .collect::<Result<Vec<_>>>()
}

pub(crate) fn preserved_error_messages(events: &[&EventRecord]) -> Vec<ContextMessage> {
    let mut messages = Vec::new();
    for record in events {
        match &record.event {
            Event::Error { message, .. } => messages.push(ContextMessage::system(format!(
                "<previous_error>{message}</previous_error>"
            ))),
            Event::ToolError { error, tool_id, .. } => messages.push(ContextMessage::tool(
                format!("<tool_error id=\"{tool_id}\">{error}</tool_error>"),
            )),
            _ => {}
        }
    }
    messages
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
                format!("<memory_read scope=\"{scope}\">{path}</memory_read>"),
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

fn build_full_file_read_path_map(events: &[&EventRecord]) -> HashMap<ToolCallId, String> {
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

fn latest_full_file_read_results(
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

fn deduplicate_file_reads(
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

        let previous_tokens = estimate_tokens(&compiled.message.content);
        compiled.message = placeholder_tool_result_message(tool_result);
        let placeholder_tokens = estimate_tokens(&compiled.message.content);
        stats.deduplicated_count += 1;
        stats.tokens_saved += previous_tokens.saturating_sub(placeholder_tokens);
    }

    stats
}

fn build_file_read_dedup_state(messages: &[CompiledRecordMessage]) -> FileReadDedupState {
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

fn placeholder_tool_result_message(tool_result: &ToolResultReplayMeta) -> ContextMessage {
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

fn placeholder_tool_result_from_snapshot(
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

struct CompiledHistory {
    messages: Vec<ContextMessage>,
    tokens_used: usize,
    deduplication: DeduplicationStats,
    snapshot: Option<SnapshotHistory>,
}

#[derive(Debug, Clone, PartialEq)]
struct SnapshotHistory {
    messages: Vec<ContextMessage>,
    last_sequence_num: SequenceNum,
    token_count: usize,
    file_read_dedup_state: FileReadDedupState,
}

#[derive(Debug, Clone)]
struct CompiledRecordMessage {
    message: ContextMessage,
    tool_result: Option<ToolResultReplayMeta>,
}

impl CompiledRecordMessage {
    fn plain(message: ContextMessage) -> Self {
        Self {
            message,
            tool_result: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ToolResultReplayMeta {
    tool_use_id: String,
    tool_id: ToolCallId,
    success: bool,
    file_read_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeduplicationStats {
    deduplicated_count: usize,
    tokens_saved: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        BrainId, CompactionConfig, CompletionContent, CompletionRequest, CompletionResponse,
        CompletionStream, EventFilter, EventRecord, ModelId, PendingSignal, PendingSignalId,
        Platform, SequenceNum, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore,
        SessionSummary, StopReason, TokenPricing, TokenUsage, ToolCallFormat, ToolCallId,
        ToolOutputConfig, UserId, WorkspaceId,
    };
    use proptest::prelude::*;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::*;

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

    #[derive(Clone)]
    struct MockSessionStore {
        session: Arc<Mutex<SessionMeta>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
        snapshot: Arc<Mutex<Option<ContextSnapshot>>>,
    }

    impl MockSessionStore {
        fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
            Self {
                session: Arc::new(Mutex::new(session)),
                events: Arc::new(Mutex::new(events)),
                snapshot: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
            let id = meta.id;
            *self.session.lock().await = meta;
            Ok(id)
        }

        async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<SequenceNum> {
            let mut events = self.events.lock().await;
            let sequence_num = events.len() as SequenceNum;
            events.push(EventRecord {
                id: uuid::Uuid::now_v7(),
                session_id,
                sequence_num,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            });
            Ok(sequence_num)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(self.events.lock().await.clone())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(self.session.lock().await.clone())
        }

        async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
            self.session.lock().await.status = status;
            Ok(())
        }

        async fn put_snapshot(
            &self,
            _session_id: SessionId,
            snapshot: ContextSnapshot,
        ) -> Result<()> {
            *self.snapshot.lock().await = Some(snapshot);
            Ok(())
        }

        async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
            Ok(self.snapshot.lock().await.clone())
        }

        async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
            *self.snapshot.lock().await = None;
            Ok(())
        }

        async fn store_pending_signal(
            &self,
            _session_id: SessionId,
            signal: PendingSignal,
        ) -> Result<PendingSignalId> {
            Ok(signal.id)
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
        ) -> Result<Vec<EventRecord>> {
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

    #[derive(Clone)]
    struct MockLlmProvider;

    #[async_trait]
    impl LLMProvider for MockLlmProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> moa_core::ModelCapabilities {
            capabilities()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "## Key Facts\n- compacted history\n\n## Decisions\n- keep the recent tail verbatim\n".to_string(),
                content: vec![CompletionContent::Text(
                    "## Key Facts\n- compacted history\n\n## Decisions\n- keep the recent tail verbatim\n"
                        .to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("claude-sonnet-4-6"),
                input_tokens: 120,
                output_tokens: 40,
                cached_input_tokens: 0,
                usage: token_usage(120, 40),
                duration_ms: 25,
                thought_signature: None,
            }))
        }
    }

    fn capabilities() -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        }
    }

    fn event_record(session_id: &SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: uuid::Uuid::now_v7(),
            session_id: *session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: Option::<BrainId>::None,
            hand_id: None,
            token_count: None,
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        }
    }

    fn compiler_with_recent_turns(
        session: &SessionMeta,
        events: &[EventRecord],
        recent_turns_verbatim: usize,
    ) -> HistoryCompiler {
        HistoryCompiler {
            session_store: Arc::new(MockSessionStore::new(session.clone(), events.to_vec())),
            llm_provider: None,
            compaction: CompactionConfig {
                recent_turns_verbatim,
                ..CompactionConfig::default()
            },
            tool_output: ToolOutputConfig::default(),
            snapshot_config: ContextSnapshotConfig::default(),
        }
    }

    fn file_read_tool_call(
        session_id: &SessionId,
        sequence_num: u64,
        tool_id: ToolCallId,
        provider_tool_use_id: &str,
        input: serde_json::Value,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: Some(provider_tool_use_id.to_string()),
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input,
                hand_id: None,
            },
        )
    }

    fn file_read_tool_result(
        session_id: &SessionId,
        sequence_num: u64,
        tool_id: ToolCallId,
        provider_tool_use_id: &str,
        text: &str,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some(provider_tool_use_id.to_string()),
                output: ToolOutput::text(text, Duration::from_millis(5)),
                original_output_tokens: None,
                success: true,
                duration_ms: 5,
            },
        )
    }

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
    async fn compaction_triggers_at_threshold_and_keeps_full_log() {
        let session = session();
        let mut events = Vec::new();
        for index in 0..7 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("event {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 4,
                recent_turns_verbatim: 2,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id, EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 8);
        assert!(matches!(
            stored_events.last().map(|record| &record.event),
            Some(Event::Checkpoint { events_summarized, .. }) if *events_summarized == 5
        ));
    }

    #[tokio::test]
    async fn compacted_view_preserves_old_errors_and_respects_budget() {
        let session = session();
        let mut events = vec![event_record(
            &session.id,
            0,
            Event::Error {
                message: "deploy failed on port binding".to_string(),
                recoverable: true,
            },
        )];
        for index in 1..12 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("turn {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        events.push(event_record(
            &session.id,
            12,
            Event::Checkpoint {
                summary: "## Key Facts\n- earlier turns were compacted".to_string(),
                events_summarized: 8,
                token_count: 12,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::ModelTier::Auxiliary,
                input_tokens: 60,
                output_tokens: 20,
                cost_cents: 1,
            },
        ));
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, tokens_used) = compiler.compile_messages(&events, 80).unwrap();
        let rendered = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("deploy failed on port binding"));
        assert!(rendered.contains("<session_checkpoint"));
        assert!(tokens_used <= 120);
    }

    #[tokio::test]
    async fn no_compaction_below_threshold() {
        let session = session();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "one".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "two".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 10,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id, EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 2);
        assert!(
            !stored_events
                .iter()
                .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        );
    }

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
            cache_controls: Vec::new(),
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

    fn compiled_snapshot(
        session: &SessionMeta,
        compiled: &CompiledHistory,
    ) -> Option<ContextSnapshot> {
        compiled.snapshot.as_ref().map(|snapshot| ContextSnapshot {
            format_version: CONTEXT_SNAPSHOT_FORMAT_VERSION,
            session_id: session.id,
            last_sequence_num: snapshot.last_sequence_num,
            created_at: Utc::now(),
            messages: snapshot.messages.clone(),
            file_read_dedup_state: snapshot.file_read_dedup_state.clone(),
            token_count: snapshot.token_count,
            cache_controls: Vec::new(),
            stage_inputs_hash: 1,
        })
    }
}

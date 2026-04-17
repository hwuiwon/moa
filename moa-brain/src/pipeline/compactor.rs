//! Stage 8: applies tiered context compaction to compiled history.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CompactionConfig, ContextMessage, ContextProcessor, ContextSnapshot, LLMProvider, MessageRole,
    ProcessorOutput, Result, SessionStore, ToolContent, WorkingContext,
};
use serde::Serialize;
use serde_json::json;

use crate::compaction::{latest_checkpoint_state, maybe_compact_events, non_checkpoint_events};
use crate::pipeline::estimate_tokens;
use crate::pipeline::history::{
    HISTORY_END_INDEX_METADATA_KEY, HISTORY_SNAPSHOT_METADATA_KEY,
    HISTORY_START_INDEX_METADATA_KEY, preserved_error_messages,
};

const TOOL_RESULT_ELIDED_PLACEHOLDER: &str = "[tool result elided by compaction]";
const DUPLICATE_BASH_PLACEHOLDER: &str = "[duplicate bash output elided by compaction]";
const CACHE_COMPACTION_PLACEHOLDER: &str =
    "[earlier history elided for cache compaction — see session log for full history]";
const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";
const FILE_READ_RANGE_HEADER_PREFIX: &str = "[showing lines ";

/// Tiered message compaction stage.
pub struct Compactor {
    config: CompactionConfig,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
}

impl Compactor {
    /// Creates a compactor that operates on compiled history messages.
    pub fn new(
        config: CompactionConfig,
        session_store: Arc<dyn SessionStore>,
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> Self {
        Self {
            config,
            session_store,
            llm_provider,
        }
    }

    fn should_apply_tier3(&self, ctx: &WorkingContext) -> bool {
        let model_limit = ctx.model_capabilities.context_window.max(1);
        let configured_ceiling = self.config.max_input_tokens_per_turn.max(1);
        let fraction_ceiling = ((model_limit as f64)
            * self.config.tier3_trigger_fraction.clamp(0.0, 1.0))
        .round() as usize;
        let effective_ceiling = configured_ceiling.min(fraction_ceiling.max(1));
        ctx.token_count >= effective_ceiling
    }
}

#[async_trait]
impl ContextProcessor for Compactor {
    fn name(&self) -> &str {
        "compactor"
    }

    fn stage(&self) -> u8 {
        8
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if !self.config.enabled {
            return Ok(ProcessorOutput::default());
        }

        let Some((history_start, history_end)) = history_bounds(ctx) else {
            return Ok(ProcessorOutput::default());
        };
        if history_start >= history_end || history_end > ctx.messages.len() {
            return Ok(ProcessorOutput::default());
        }

        let tokens_before = ctx.token_count;
        let mut report = CompactionReport {
            tokens_before,
            ..CompactionReport::default()
        };
        report
            .tiers_applied
            .push(CompactionTier::Tier1Deterministic);

        let mut history_messages = ctx.messages[history_start..history_end].to_vec();
        let mut snapshot = load_snapshot(ctx);

        let snapshot_protected = snapshot
            .as_ref()
            .map(protected_snapshot_tool_use_ids)
            .unwrap_or_default();
        let tier1_elided = apply_tier1(
            &mut history_messages,
            self.config.recent_turns_verbatim,
            &HashSet::new(),
        );
        report.messages_elided += tier1_elided;

        if let Some(snapshot) = snapshot.as_mut() {
            report.messages_elided += apply_tier1(&mut snapshot.messages, 0, &snapshot_protected);
            snapshot.token_count = token_count(&snapshot.messages);
        }

        let recent_start =
            recent_turn_boundary_messages(&history_messages, self.config.recent_turns_verbatim);
        if should_apply_tier2(&history_messages, recent_start, &self.config) {
            report.tiers_applied.push(CompactionTier::Tier2CacheAware);
            report.messages_elided += apply_tier2(&mut history_messages);
            if let Some(snapshot) = snapshot.as_mut() {
                collapse_snapshot_for_tier2(snapshot);
            }
        }

        if self.should_apply_tier3(ctx)
            && let Some(llm_provider) = &self.llm_provider
            && let Some(summary) = apply_tier3(
                ctx,
                &history_messages,
                &self.config,
                &*self.session_store,
                &**llm_provider,
            )
            .await?
        {
            report
                .tiers_applied
                .push(CompactionTier::Tier3Summarization);
            report.messages_elided += history_messages.len();
            history_messages = summary.messages;
            report.summary_text = Some(summary.summary);
            report.events_summarized = Some(summary.events_summarized);
            snapshot = None;
        }

        ctx.messages
            .splice(history_start..history_end, history_messages.clone());
        ctx.insert_metadata(
            HISTORY_END_INDEX_METADATA_KEY,
            json!(history_start + history_messages.len()),
        );
        store_snapshot(ctx, snapshot)?;

        ctx.token_count = token_count(&ctx.messages);
        report.tokens_after = ctx.token_count;

        let mut metadata = HashMap::new();
        metadata.insert(
            "tiers_applied".to_string(),
            serde_json::to_value(&report.tiers_applied)?,
        );
        metadata.insert("tokens_before".to_string(), json!(report.tokens_before));
        metadata.insert("tokens_after".to_string(), json!(report.tokens_after));
        metadata.insert(
            "tokens_reclaimed".to_string(),
            json!(report.tokens_reclaimed()),
        );
        metadata.insert("messages_elided".to_string(), json!(report.messages_elided));
        metadata.insert(
            "tier1_applied".to_string(),
            json!(
                report
                    .tiers_applied
                    .contains(&CompactionTier::Tier1Deterministic)
            ),
        );
        metadata.insert(
            "tier2_applied".to_string(),
            json!(
                report
                    .tiers_applied
                    .contains(&CompactionTier::Tier2CacheAware)
            ),
        );
        metadata.insert(
            "tier3_applied".to_string(),
            json!(
                report
                    .tiers_applied
                    .contains(&CompactionTier::Tier3Summarization)
            ),
        );
        if let Some(summary_text) = report.summary_text.as_ref() {
            metadata.insert("summary_text".to_string(), json!(summary_text));
        }
        if let Some(events_summarized) = report.events_summarized {
            metadata.insert("events_summarized".to_string(), json!(events_summarized));
        }

        Ok(ProcessorOutput {
            tokens_added: 0,
            tokens_removed: report.tokens_reclaimed(),
            metadata,
            ..ProcessorOutput::default()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CompactionTier {
    Tier1Deterministic,
    Tier2CacheAware,
    Tier3Summarization,
}

#[derive(Debug, Clone, Default)]
struct CompactionReport {
    tiers_applied: Vec<CompactionTier>,
    tokens_before: usize,
    tokens_after: usize,
    messages_elided: usize,
    summary_text: Option<String>,
    events_summarized: Option<usize>,
}

impl CompactionReport {
    fn tokens_reclaimed(&self) -> usize {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

struct Tier3Summary {
    messages: Vec<ContextMessage>,
    summary: String,
    events_summarized: usize,
}

fn history_bounds(ctx: &WorkingContext) -> Option<(usize, usize)> {
    let start = ctx
        .metadata()
        .get(HISTORY_START_INDEX_METADATA_KEY)?
        .as_u64()? as usize;
    let end = ctx
        .metadata()
        .get(HISTORY_END_INDEX_METADATA_KEY)?
        .as_u64()? as usize;
    Some((start, end))
}

fn load_snapshot(ctx: &WorkingContext) -> Option<ContextSnapshot> {
    let value = ctx.metadata().get(HISTORY_SNAPSHOT_METADATA_KEY)?;
    if value.is_null() {
        return None;
    }

    serde_json::from_value(value.clone()).ok()
}

fn store_snapshot(ctx: &mut WorkingContext, snapshot: Option<ContextSnapshot>) -> Result<()> {
    let value = match snapshot {
        Some(snapshot) => serde_json::to_value(snapshot)?,
        None => serde_json::Value::Null,
    };
    ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, value);
    Ok(())
}

fn protected_snapshot_tool_use_ids(snapshot: &ContextSnapshot) -> HashSet<String> {
    snapshot
        .file_read_dedup_state
        .latest_reads
        .values()
        .map(|state| state.tool_use_id.clone())
        .collect()
}

fn apply_tier1(
    messages: &mut [ContextMessage],
    recent_turns_verbatim: usize,
    protected_tool_use_ids: &HashSet<String>,
) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let recent_boundary = recent_turn_boundary_messages(messages, recent_turns_verbatim);
    let tool_names = tool_names_by_use_id(messages);
    let referenced_ids = referenced_tool_use_ids(messages);
    let mut seen_bash_outputs = HashSet::new();
    let mut elided = 0usize;

    for message in messages.iter_mut().take(recent_boundary) {
        let Some(tool_use_id) = message.tool_use_id.clone() else {
            continue;
        };
        if is_compacted_tool_result(message) {
            continue;
        }
        if is_file_read_result_message(message) {
            continue;
        }
        if protected_tool_use_ids.contains(&tool_use_id) {
            continue;
        }
        if referenced_ids.contains(&tool_use_id) {
            continue;
        }

        let is_bash = tool_names
            .get(&tool_use_id)
            .map(|tool_name| tool_name == "bash")
            .unwrap_or(false);
        if is_bash && !seen_bash_outputs.insert(message.content.clone()) {
            *message = compacted_tool_result(message, DUPLICATE_BASH_PLACEHOLDER);
            elided += 1;
            continue;
        }

        *message = compacted_tool_result(message, TOOL_RESULT_ELIDED_PLACEHOLDER);
        elided += 1;
    }

    elided
}

fn is_compacted_tool_result(message: &ContextMessage) -> bool {
    message.content.contains(TOOL_RESULT_ELIDED_PLACEHOLDER)
        || message.content.contains(DUPLICATE_BASH_PLACEHOLDER)
        || message.content.contains(CACHE_COMPACTION_PLACEHOLDER)
}

fn is_file_read_result_message(message: &ContextMessage) -> bool {
    message.content.contains(FILE_READ_DEDUP_PLACEHOLDER)
        || message.content.contains(FILE_READ_RANGE_HEADER_PREFIX)
}

fn tool_names_by_use_id(messages: &[ContextMessage]) -> HashMap<String, String> {
    messages
        .iter()
        .filter_map(|message| {
            let invocation = message.tool_invocation.as_ref()?;
            let id = invocation.id.as_ref()?;
            Some((id.clone(), invocation.name.clone()))
        })
        .collect()
}

fn referenced_tool_use_ids(messages: &[ContextMessage]) -> HashSet<String> {
    let candidate_ids = messages
        .iter()
        .filter_map(|message| message.tool_use_id.clone())
        .collect::<Vec<_>>();

    let mut referenced = HashSet::new();
    for (index, tool_use_id) in candidate_ids.iter().enumerate() {
        if messages.iter().skip(index + 1).any(|message| {
            message.content.contains(tool_use_id)
                || message
                    .tool_invocation
                    .as_ref()
                    .and_then(|invocation| invocation.id.as_ref())
                    .map(|id| id == tool_use_id)
                    .unwrap_or(false)
        }) {
            referenced.insert(tool_use_id.clone());
        }
    }

    referenced
}

fn compacted_tool_result(message: &ContextMessage, placeholder: &str) -> ContextMessage {
    ContextMessage::tool_result(
        message
            .tool_use_id
            .clone()
            .unwrap_or_else(|| "compacted".to_string()),
        placeholder,
        Some(vec![ToolContent::Text {
            text: placeholder.to_string(),
        }]),
    )
}

fn recent_turn_boundary_messages(messages: &[ContextMessage], recent_turns: usize) -> usize {
    if recent_turns == 0 || messages.is_empty() {
        return messages.len();
    }

    let mut turns_seen = 0usize;
    for index in (0..messages.len()).rev() {
        if messages[index].role == MessageRole::User {
            turns_seen += 1;
            if turns_seen == recent_turns {
                return index;
            }
        }
    }

    0
}

fn should_apply_tier2(
    history_messages: &[ContextMessage],
    recent_boundary: usize,
    config: &CompactionConfig,
) -> bool {
    if recent_boundary == 0 {
        return false;
    }

    let token_pressure = token_count(history_messages);
    recent_boundary > config.tier2_trigger_blocks_past_bp4
        && token_pressure > config.max_input_tokens_per_turn / 2
}

fn apply_tier2(messages: &mut Vec<ContextMessage>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let dropped = messages.len();
    messages.clear();
    messages.push(ContextMessage::system(CACHE_COMPACTION_PLACEHOLDER));
    dropped
}

fn collapse_snapshot_for_tier2(snapshot: &mut ContextSnapshot) {
    snapshot.messages = vec![ContextMessage::system(CACHE_COMPACTION_PLACEHOLDER)];
    snapshot.file_read_dedup_state.latest_reads.clear();
    snapshot.token_count = token_count(&snapshot.messages);
}

async fn apply_tier3(
    ctx: &WorkingContext,
    history_messages: &[ContextMessage],
    config: &CompactionConfig,
    session_store: &dyn SessionStore,
    llm_provider: &dyn LLMProvider,
) -> Result<Option<Tier3Summary>> {
    let events = session_store
        .get_events(ctx.session_id, moa_core::EventRange::all())
        .await?;
    let mut forced_config = config.clone();
    forced_config.enabled = true;
    forced_config.event_threshold = 1;
    forced_config.token_ratio_threshold = 0.0;
    if !maybe_compact_events(
        &forced_config,
        session_store,
        llm_provider,
        ctx.session_id,
        ctx.model_capabilities.context_window,
        &events,
    )
    .await?
    {
        return Ok(None);
    }

    let refreshed_events = session_store
        .get_events(ctx.session_id, moa_core::EventRange::all())
        .await?;
    let Some(checkpoint) = latest_checkpoint_state(&refreshed_events) else {
        return Ok(None);
    };
    let non_checkpoint = non_checkpoint_events(&refreshed_events);
    let summarized = checkpoint.events_summarized.min(non_checkpoint.len());
    let preserved_errors = preserved_error_messages(&non_checkpoint[..summarized]);
    let recent_boundary =
        recent_turn_boundary_messages(history_messages, config.recent_turns_verbatim);
    let recent_tail = history_messages[recent_boundary.min(history_messages.len())..].to_vec();

    let mut messages = preserved_errors;
    messages.push(ContextMessage::system(format!(
        "<session_checkpoint summarized_events=\"{}\">\n{}\n</session_checkpoint>",
        checkpoint.events_summarized, checkpoint.summary
    )));
    messages.extend(recent_tail);

    Ok(Some(Tier3Summary {
        messages,
        summary: checkpoint.summary,
        events_summarized: checkpoint.events_summarized,
    }))
}

fn token_count(messages: &[ContextMessage]) -> usize {
    messages
        .iter()
        .map(|message| estimate_tokens(&message.content))
        .sum()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        BrainId, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, Event,
        EventFilter, EventRange, EventRecord, ModelCapabilities, ModelId, PendingSignal,
        PendingSignalId, Platform, Result, SessionFilter, SessionId, SessionMeta, SessionStatus,
        SessionStore, SessionSummary, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
        WorkspaceId,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone)]
    struct MockSessionStore {
        session: Arc<Mutex<SessionMeta>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
    }

    impl MockSessionStore {
        fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
            Self {
                session: Arc::new(Mutex::new(session)),
                events: Arc::new(Mutex::new(events)),
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

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(self.session.lock().await.clone())
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
            let mut events = self.events.lock().await;
            let sequence_num = events.len() as u64 + 1;
            let record = EventRecord {
                id: uuid::Uuid::now_v7(),
                session_id,
                sequence_num,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: Option::<BrainId>::None,
                hand_id: None,
                token_count: None,
            };
            events.push(record);
            Ok(sequence_num)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            let events = self.events.lock().await.clone();
            Ok(events
                .into_iter()
                .filter(|record| {
                    range
                        .from_seq
                        .map(|from_seq| record.sequence_num >= from_seq)
                        .unwrap_or(true)
                        && range
                            .to_seq
                            .map(|to_seq| record.sequence_num <= to_seq)
                            .unwrap_or(true)
                })
                .collect())
        }

        async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
            self.session.lock().await.status = status;
            Ok(())
        }

        async fn put_snapshot(
            &self,
            _session_id: SessionId,
            _snapshot: ContextSnapshot,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_snapshot(&self, _session_id: SessionId) -> Result<Option<ContextSnapshot>> {
            Ok(None)
        }

        async fn delete_snapshot(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }

        async fn store_pending_signal(
            &self,
            _session_id: SessionId,
            _signal: PendingSignal,
        ) -> Result<PendingSignalId> {
            unimplemented!("not needed for compactor tests")
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

        fn capabilities(&self) -> ModelCapabilities {
            capabilities()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            Ok(CompletionStream::from_response(CompletionResponse {
                text: "## Goal\n- compact the older turns\n".to_string(),
                content: vec![CompletionContent::Text(
                    "## Goal\n- compact the older turns\n".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("claude-sonnet-4-6"),
                input_tokens: 120,
                output_tokens: 40,
                cached_input_tokens: 0,
                usage: TokenUsage {
                    input_tokens_uncached: 120,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 40,
                },
                duration_ms: 25,
                thought_signature: None,
            }))
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: moa_core::UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
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

    #[test]
    fn tier1_is_idempotent_for_compacted_messages() {
        let mut messages = vec![
            ContextMessage::assistant_tool_call(
                moa_core::ToolInvocation {
                    id: Some("toolu_1".to_string()),
                    name: "bash".to_string(),
                    input: json!({"cmd": "pwd"}),
                },
                "call",
            ),
            ContextMessage::tool_result("toolu_1", "output", None),
            ContextMessage::user("latest"),
        ];

        let once = apply_tier1(&mut messages, 1, &HashSet::new());
        let snapshot = messages.clone();
        let twice = apply_tier1(&mut messages, 1, &HashSet::new());

        assert_eq!(once, 1);
        assert_eq!(twice, 0);
        assert_eq!(messages, snapshot);
    }

    #[test]
    fn tier2_replaces_old_history_with_placeholder() {
        let mut messages = vec![
            ContextMessage::user("turn 1"),
            ContextMessage::assistant("answer 1"),
            ContextMessage::user("turn 2"),
            ContextMessage::assistant("answer 2"),
        ];

        let dropped = apply_tier2(&mut messages);

        assert_eq!(dropped, 4);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("cache compaction"));
    }

    #[tokio::test]
    async fn tier3_emits_checkpoint_and_replaces_history_with_summary() {
        let session = session();
        let history = vec![
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "first request".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                2,
                Event::BrainResponse {
                    text: "first response".to_string(),
                    model: ModelId::new("claude-sonnet-4-6"),
                    input_tokens_uncached: 10,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 5,
                    cost_cents: 1,
                    duration_ms: 10,
                    thought_signature: None,
                },
            ),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second request".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), history.clone()));
        let llm = Arc::new(MockLlmProvider);
        let compactor = Compactor::new(
            CompactionConfig {
                max_input_tokens_per_turn: 1,
                recent_turns_verbatim: 1,
                ..CompactionConfig::default()
            },
            store.clone(),
            Some(llm),
        );
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.extend_messages(vec![
            ContextMessage::user("first request"),
            ContextMessage::assistant("first response"),
            ContextMessage::user("second request"),
        ]);
        ctx.insert_metadata(HISTORY_START_INDEX_METADATA_KEY, json!(0));
        ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(3));
        ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, serde_json::Value::Null);
        ctx.token_count = 10;

        let output = compactor.process(&mut ctx).await.unwrap();
        let events = store
            .get_events(session.id, EventRange::all())
            .await
            .unwrap();

        assert!(
            output
                .metadata
                .get("tier3_applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        );
        assert!(
            events
                .iter()
                .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        );
        assert!(ctx.messages.iter().any(|message| {
            message.content.contains("<session_checkpoint")
                && message.content.contains("summarized_events")
        }));
        assert_eq!(
            ctx.metadata().get(HISTORY_SNAPSHOT_METADATA_KEY),
            Some(&serde_json::Value::Null)
        );
    }
}

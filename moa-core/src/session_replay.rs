//! Per-turn session event replay instrumentation utilities.

use std::future::Future;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    ApprovalDecision, ApprovalPrompt, Event, EventFilter, EventRange, EventRecord, PendingSignal,
    PendingSignalId, Result, SessionFilter, SessionId, SessionMeta, SessionStatus, SessionStore,
    SessionSummary, ToolContent, ToolOutput, WorkspaceId,
};

tokio::task_local! {
    static TURN_REPLAY_COUNTERS: Arc<TurnReplayCounters>;
}

/// Snapshot of per-turn event replay counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnReplaySnapshot {
    /// Number of `get_events` calls made during the turn.
    pub get_events_calls: u64,
    /// Total number of event records returned across all `get_events` calls.
    pub events_replayed: u64,
    /// Approximate number of bytes deserialized across all returned events.
    pub events_bytes: u64,
    /// Aggregate wall-clock time spent inside `get_events`.
    pub get_events_total_duration: Duration,
    /// Aggregate wall-clock time spent compiling pipeline context for the turn.
    pub pipeline_compile_duration: Duration,
}

impl TurnReplaySnapshot {
    /// Returns total `get_events` time in whole milliseconds.
    pub fn get_events_total_ms(&self) -> u64 {
        display_duration_ms(self.get_events_total_duration)
    }

    /// Returns total pipeline compile time in whole milliseconds.
    pub fn pipeline_compile_ms(&self) -> u64 {
        display_duration_ms(self.pipeline_compile_duration)
    }
}

/// Mutable per-turn counters stored in task-local scope.
#[derive(Debug, Default)]
pub struct TurnReplayCounters {
    get_events_calls: AtomicU64,
    events_replayed: AtomicU64,
    events_bytes: AtomicU64,
    get_events_total_us: AtomicU64,
    pipeline_compile_us: AtomicU64,
}

impl TurnReplayCounters {
    /// Returns a read-only snapshot of the current counter values.
    pub fn snapshot(&self) -> TurnReplaySnapshot {
        TurnReplaySnapshot {
            get_events_calls: self.get_events_calls.load(Ordering::Relaxed),
            events_replayed: self.events_replayed.load(Ordering::Relaxed),
            events_bytes: self.events_bytes.load(Ordering::Relaxed),
            get_events_total_duration: Duration::from_micros(
                self.get_events_total_us.load(Ordering::Relaxed),
            ),
            pipeline_compile_duration: Duration::from_micros(
                self.pipeline_compile_us.load(Ordering::Relaxed),
            ),
        }
    }

    fn record_get_events(&self, event_count: usize, bytes: u64, duration: Duration) {
        self.get_events_calls.fetch_add(1, Ordering::Relaxed);
        self.events_replayed
            .fetch_add(event_count as u64, Ordering::Relaxed);
        self.events_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.get_events_total_us
            .fetch_add(recorded_duration_micros(duration), Ordering::Relaxed);
    }

    fn record_pipeline_compile_duration(&self, duration: Duration) {
        self.pipeline_compile_us
            .fetch_add(recorded_duration_micros(duration), Ordering::Relaxed);
    }
}

/// Runs a future inside a fresh per-turn replay-counter scope.
pub async fn scope_turn_replay_counters<F, T>(counters: Arc<TurnReplayCounters>, future: F) -> T
where
    F: Future<Output = T>,
{
    TURN_REPLAY_COUNTERS.scope(counters, future).await
}

/// Records pipeline compilation time for the current turn when instrumentation is active.
pub fn record_pipeline_compile_duration(duration: Duration) {
    let _ = TURN_REPLAY_COUNTERS.try_with(|counters| {
        counters.record_pipeline_compile_duration(duration);
    });
}

/// Wraps a session store and records per-turn `get_events` usage in task-local counters.
#[derive(Clone)]
pub struct CountedSessionStore {
    inner: Arc<dyn SessionStore>,
}

impl CountedSessionStore {
    /// Creates a counted wrapper around an existing session store.
    pub fn new(inner: Arc<dyn SessionStore>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl SessionStore for CountedSessionStore {
    async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        self.inner.create_session(meta).await
    }

    async fn emit_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
        self.inner.emit_event(session_id, event).await
    }

    async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        let started_at = Instant::now();
        let events = self
            .inner
            .get_events(session_id.clone(), range.clone())
            .await?;
        let duration = started_at.elapsed();
        let bytes = approx_event_bytes(&events);

        let _ = TURN_REPLAY_COUNTERS.try_with(|counters| {
            counters.record_get_events(events.len(), bytes, duration);
        });

        tracing::debug!(
            session_id = %session_id,
            range = ?range,
            returned_events = events.len(),
            approx_bytes = bytes,
            duration_ms = duration.as_millis() as u64,
            "loaded session events"
        );

        Ok(events)
    }

    async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.inner.get_session(session_id).await
    }

    async fn update_status(&self, session_id: SessionId, status: SessionStatus) -> Result<()> {
        self.inner.update_status(session_id, status).await
    }

    async fn store_pending_signal(
        &self,
        session_id: SessionId,
        signal: PendingSignal,
    ) -> Result<PendingSignalId> {
        self.inner.store_pending_signal(session_id, signal).await
    }

    async fn get_pending_signals(&self, session_id: SessionId) -> Result<Vec<PendingSignal>> {
        self.inner.get_pending_signals(session_id).await
    }

    async fn resolve_pending_signal(&self, signal_id: PendingSignalId) -> Result<()> {
        self.inner.resolve_pending_signal(signal_id).await
    }

    async fn search_events(&self, query: &str, filter: EventFilter) -> Result<Vec<EventRecord>> {
        self.inner.search_events(query, filter).await
    }

    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        self.inner.list_sessions(filter).await
    }

    async fn workspace_cost_since(
        &self,
        workspace_id: &WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32> {
        self.inner.workspace_cost_since(workspace_id, since).await
    }

    async fn delete_session(&self, session_id: SessionId) -> Result<()> {
        self.inner.delete_session(session_id).await
    }
}

fn recorded_duration_micros(duration: Duration) -> u64 {
    duration.as_micros().max(1) as u64
}

fn display_duration_ms(duration: Duration) -> u64 {
    let millis = duration.as_millis() as u64;
    if millis == 0 && duration > Duration::ZERO {
        1
    } else {
        millis
    }
}

fn approx_event_bytes(events: &[EventRecord]) -> u64 {
    events
        .iter()
        .map(|record| {
            mem::size_of::<EventRecord>() as u64 + event_payload_size(&record.event) as u64
        })
        .sum()
}

fn event_payload_size(event: &Event) -> usize {
    match event {
        Event::SessionCreated {
            workspace_id,
            user_id,
            model,
        } => workspace_id.len() + user_id.len() + model.len(),
        Event::SessionStatusChanged { .. } => 32,
        Event::SessionCompleted { summary, .. } => summary.len(),
        Event::UserMessage { text, attachments } => {
            text.len() + attachments.iter().map(attachment_size).sum::<usize>()
        }
        Event::QueuedMessage { text, .. } => text.len(),
        Event::BrainThinking { summary, .. } => summary.len(),
        Event::BrainResponse {
            text,
            thought_signature,
            model,
            ..
        } => text.len() + model.len() + thought_signature.as_ref().map_or(0, String::len),
        Event::ToolCall {
            tool_name, input, ..
        } => tool_name.len() + json_size(input),
        Event::ToolResult { output, .. } => tool_output_size(output),
        Event::ToolError {
            tool_name, error, ..
        } => tool_name.len() + error.len(),
        Event::ApprovalRequested {
            tool_name,
            input_summary,
            prompt,
            ..
        } => tool_name.len() + input_summary.len() + approval_prompt_size(prompt),
        Event::ApprovalDecided {
            decided_by,
            decision,
            ..
        } => decided_by.len() + approval_decision_size(decision),
        Event::MemoryRead { path, scope } => path.len() + scope.len(),
        Event::MemoryWrite {
            path,
            scope,
            summary,
        } => path.len() + scope.len() + summary.len(),
        Event::MemoryIngest {
            source_name,
            source_path,
            affected_pages,
            contradictions,
        } => {
            source_name.len()
                + source_path.len()
                + affected_pages.iter().map(String::len).sum::<usize>()
                + contradictions.iter().map(String::len).sum::<usize>()
        }
        Event::HandProvisioned {
            hand_id,
            provider,
            tier,
        } => hand_id.len() + provider.len() + tier.len(),
        Event::HandDestroyed { hand_id, reason } => hand_id.len() + reason.len(),
        Event::HandError { hand_id, error } => hand_id.len() + error.len(),
        Event::Checkpoint { summary, model, .. } => summary.len() + model.len(),
        Event::CacheReport { report } => json_size(&serde_json::json!(report)),
        Event::Error { message, .. } | Event::Warning { message } => message.len(),
    }
}

fn attachment_size(attachment: &crate::Attachment) -> usize {
    attachment.name.len()
        + attachment.mime_type.as_ref().map_or(0, String::len)
        + attachment
            .path
            .as_ref()
            .map_or(0, |path| path.as_os_str().len())
        + attachment.url.as_ref().map_or(0, String::len)
}

fn approval_prompt_size(prompt: &ApprovalPrompt) -> usize {
    prompt.pattern.len()
        + prompt.request.tool_name.len()
        + prompt.request.input_summary.len()
        + prompt
            .parameters
            .iter()
            .map(|field| field.label.len() + field.value.len())
            .sum::<usize>()
        + prompt
            .file_diffs
            .iter()
            .map(|diff| {
                diff.path.len()
                    + diff.before.len()
                    + diff.after.len()
                    + diff.language_hint.as_ref().map_or(0, String::len)
            })
            .sum::<usize>()
}

fn approval_decision_size(decision: &ApprovalDecision) -> usize {
    match decision {
        ApprovalDecision::AllowOnce => 16,
        ApprovalDecision::AlwaysAllow { pattern } => pattern.len(),
        ApprovalDecision::Deny { reason } => reason.as_ref().map_or(16, String::len),
    }
}

fn tool_output_size(output: &ToolOutput) -> usize {
    let content_bytes = output
        .content
        .iter()
        .map(|block| match block {
            ToolContent::Text { text } => text.len(),
            ToolContent::Json { data } => json_size(data),
        })
        .sum::<usize>();
    let structured_bytes = output.structured.as_ref().map_or(0, json_size);

    content_bytes + structured_bytes
}

fn json_size(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |serialized| serialized.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use chrono::Utc;

    use crate::{SessionStatus, UserId};

    #[derive(Clone)]
    struct MockSessionStore {
        events: Vec<EventRecord>,
    }

    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn create_session(&self, _meta: SessionMeta) -> Result<SessionId> {
            Ok(SessionId::new())
        }

        async fn emit_event(&self, _session_id: SessionId, _event: Event) -> Result<u64> {
            Ok(0)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
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

    fn event_record(event: Event) -> EventRecord {
        let event_type = event.event_type();
        EventRecord {
            id: uuid::Uuid::now_v7(),
            session_id: SessionId::new(),
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
    async fn counted_store_records_get_events_within_scope() {
        let inner: Arc<dyn SessionStore> = Arc::new(MockSessionStore {
            events: vec![event_record(Event::UserMessage {
                text: "hello".to_string(),
                attachments: Vec::new(),
            })],
        });
        let store = CountedSessionStore::new(inner);
        let counters = Arc::new(TurnReplayCounters::default());

        scope_turn_replay_counters(counters.clone(), async {
            let events = store
                .get_events(SessionId::new(), EventRange::all())
                .await
                .expect("get_events should succeed");
            assert_eq!(events.len(), 1);
            record_pipeline_compile_duration(Duration::from_millis(12));
        })
        .await;

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.get_events_calls, 1);
        assert_eq!(snapshot.events_replayed, 1);
        assert!(snapshot.events_bytes > 0);
        assert!(snapshot.get_events_total_duration > Duration::ZERO);
        assert_eq!(snapshot.pipeline_compile_ms(), 12);
    }

    #[tokio::test]
    async fn counted_store_is_noop_outside_scope() {
        let inner: Arc<dyn SessionStore> = Arc::new(MockSessionStore {
            events: vec![event_record(Event::Warning {
                message: "warn".to_string(),
            })],
        });
        let store = CountedSessionStore::new(inner);

        let events = store
            .get_events(SessionId::new(), EventRange::all())
            .await
            .expect("get_events should succeed");
        assert_eq!(events.len(), 1);
    }
}

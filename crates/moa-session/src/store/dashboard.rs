//! Tenant-scoped dashboard read model for persisted sessions.

use chrono::{DateTime, Utc};
use moa_core::{
    error::Result, events::Event, events::EventType, types::channel::Channel,
    types::contact::ContactId, types::contact::SessionActorRef, types::events_stream::EventRecord,
    types::identifiers::BrainId, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::session::SessionStatus, types::session::SessionSummary,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder, postgres::PgRow};
use uuid::Uuid;

use super::PostgresSessionStore;
use crate::queries::{
    EVENT_COLUMNS, RowExt, SESSION_SELECT_COLUMNS, SESSION_SUMMARY_COLUMNS, map_sqlx_error,
    session_meta_from_row, session_summary_from_row,
};

const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 200;

/// Request for a tenant-scoped dashboard session list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSessionListRequest {
    /// Maximum number of sessions to return.
    pub limit: Option<usize>,
    /// Cursor returned by a previous list page.
    pub cursor: Option<DashboardSessionListCursor>,
    /// Optional session status filter.
    pub status: Option<SessionStatus>,
    /// Optional delivery channel filter.
    pub channel: Option<Channel>,
    /// Optional contact filter.
    pub contact_id: Option<ContactId>,
}

/// Stable keyset cursor for dashboard session lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSessionListCursor {
    /// Last seen session update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last seen session id for deterministic timestamp ties.
    pub session_id: SessionId,
}

/// One tenant-scoped dashboard session list page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSessionListPage {
    /// Sessions in deterministic `(updated_at, session_id)` descending order.
    pub sessions: Vec<SessionSummary>,
    /// Cursor for the next page when more rows are available.
    pub next_cursor: Option<DashboardSessionListCursor>,
}

/// Dashboard-safe session detail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSessionDetail {
    /// Session identifier.
    pub session_id: SessionId,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Optional operator-facing title.
    pub title: Option<String>,
    /// Current lifecycle status.
    pub status: SessionStatus,
    /// Active delivery channel.
    pub channel: Channel,
    /// Model used by the session.
    pub model: ModelId,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Completion timestamp for terminal sessions.
    pub completed_at: Option<DateTime<Utc>>,
    /// Parent session identifier, when this is a child session.
    pub parent_session_id: Option<SessionId>,
    /// Contact attached to the session, when present.
    pub contact_id: Option<ContactId>,
    /// Actor that created the session, when recorded.
    pub created_by: Option<SessionActorRef>,
    /// Aggregate input tokens.
    pub total_input_tokens: usize,
    /// Aggregate output tokens.
    pub total_output_tokens: usize,
    /// Aggregate cost in cents.
    pub total_cost_cents: u32,
    /// Number of persisted session events.
    pub event_count: usize,
    /// Fraction of input tokens served from cache.
    pub cache_hit_rate: f64,
}

/// Request for one dashboard event page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardEventPageRequest {
    /// Maximum number of events to return.
    pub limit: Option<usize>,
    /// Cursor returned by a previous event page.
    pub cursor: Option<DashboardEventCursor>,
    /// Optional event type filter.
    pub event_types: Option<Vec<EventType>>,
}

/// Stable sequence cursor for dashboard event pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardEventCursor {
    /// Last seen event sequence number.
    pub sequence_num: u64,
}

/// One tenant-scoped dashboard event page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardEventPage {
    /// Events in ascending sequence order.
    pub events: Vec<DashboardEventTimelineItem>,
    /// Cursor for the next page when more rows are available.
    pub next_cursor: Option<DashboardEventCursor>,
}

/// A redacted event timeline item for dashboard display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardEventTimelineItem {
    /// Event identifier.
    pub event_id: Uuid,
    /// Session identifier.
    pub session_id: SessionId,
    /// Event sequence number.
    pub sequence_num: u64,
    /// Event type discriminator.
    pub event_type: EventType,
    /// Event emission timestamp.
    pub timestamp: DateTime<Utc>,
    /// Brain identifier, when recorded.
    pub brain_id: Option<BrainId>,
    /// Hand identifier, when recorded.
    pub hand_id: Option<String>,
    /// Token count attributed to this event, when recorded.
    pub token_count: Option<usize>,
    /// Redacted operator-facing summary.
    pub summary: String,
}

impl PostgresSessionStore {
    /// Lists dashboard sessions for one tenant using `(updated_at, session_id)` keyset pagination.
    pub async fn list_dashboard_sessions(
        &self,
        tenant_id: TenantId,
        request: DashboardSessionListRequest,
    ) -> Result<DashboardSessionListPage> {
        let limit = page_limit(request.limit);
        let sessions = self.table_name("sessions");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {SESSION_SUMMARY_COLUMNS} FROM {sessions} WHERE tenant_id = "
        ));
        query.push_bind(tenant_id.0);

        if let Some(status) = request.status {
            query.push(" AND status = ");
            query.push_bind(status.as_str());
        }
        if let Some(channel) = request.channel {
            query.push(" AND channel = ");
            query.push_bind(channel.as_str());
        }
        if let Some(contact_id) = request.contact_id {
            query.push(" AND contact_id = ");
            query.push_bind(contact_id.0);
        }
        if let Some(cursor) = request.cursor {
            query.push(" AND (updated_at < ");
            query.push_bind(cursor.updated_at);
            query.push(" OR (updated_at = ");
            query.push_bind(cursor.updated_at);
            query.push(" AND id < ");
            query.push_bind(cursor.session_id.0);
            query.push("))");
        }

        query.push(" ORDER BY updated_at DESC, id DESC LIMIT ");
        query.push_bind((limit + 1) as i64);

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut sessions = rows
            .iter()
            .map(session_summary_from_row)
            .collect::<Result<Vec<_>>>()?;
        let has_next = sessions.len() > limit;
        if has_next {
            sessions.truncate(limit);
        }
        let next_cursor = has_next.then(|| {
            let last = sessions
                .last()
                .expect("non-empty page should have a last row when has_next is true");
            DashboardSessionListCursor {
                updated_at: last.updated_at,
                session_id: last.session_id,
            }
        });

        Ok(DashboardSessionListPage {
            sessions,
            next_cursor,
        })
    }

    /// Loads dashboard-safe detail for one tenant-owned session.
    pub async fn get_dashboard_session_detail(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
    ) -> Result<Option<DashboardSessionDetail>> {
        let sessions = self.table_name("sessions");
        let row = sqlx::query(&format!(
            "SELECT {SESSION_SELECT_COLUMNS} FROM {sessions} WHERE tenant_id = $1 AND id = $2"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| session_meta_from_row(&row).map(dashboard_detail_from_meta))
            .transpose()
    }

    /// Lists tenant-scoped session events using event sequence keyset pagination.
    pub async fn list_dashboard_session_events(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        request: DashboardEventPageRequest,
    ) -> Result<DashboardEventPage> {
        let limit = page_limit(request.limit);
        let event_types = request.event_types.as_deref();
        let cursor = request.cursor.map(|cursor| cursor.sequence_num);
        let mut rows = self
            .load_dashboard_event_rows(tenant_id, session_id, cursor, event_types, limit + 1)
            .await?;
        let has_next = rows.len() > limit;
        if has_next {
            rows.truncate(limit);
        }
        let records = self.event_records_from_rows(session_id, &rows).await?;
        let events = records
            .iter()
            .map(dashboard_timeline_item_from_record)
            .collect();
        let next_cursor = has_next.then(|| DashboardEventCursor {
            sequence_num: records
                .last()
                .expect("non-empty event page should have a last row when has_next is true")
                .sequence_num,
        });

        Ok(DashboardEventPage {
            events,
            next_cursor,
        })
    }

    async fn load_dashboard_event_rows(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        after_sequence: Option<u64>,
        event_types: Option<&[EventType]>,
        limit: usize,
    ) -> Result<Vec<PgRow>> {
        if matches!(event_types, Some(types) if types.is_empty()) {
            return Ok(Vec::new());
        }

        let events = self.table_name("events");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {EVENT_COLUMNS} FROM {events} WHERE tenant_id = "
        ));
        query.push_bind(tenant_id.0);
        query.push(" AND session_id = ");
        query.push_bind(session_id.0);

        if let Some(after_sequence) = after_sequence {
            query.push(" AND sequence_num > ");
            query.push_bind(after_sequence as i64);
        }
        if let Some(event_types) = event_types {
            query.push(" AND event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type.as_str());
            }
            separated.push_unseparated(")");
        }
        query.push(" ORDER BY sequence_num ASC");
        query.push(" LIMIT ");
        query.push_bind(limit as i64);

        query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)
    }

    async fn event_records_from_rows(
        &self,
        session_id: SessionId,
        rows: &[PgRow],
    ) -> Result<Vec<EventRecord>> {
        let mut payloads = Vec::with_capacity(rows.len());
        let mut blob_ids = Vec::new();
        let mut seen_blob_ids = std::collections::HashSet::new();
        for row in rows {
            let payload = row.col::<serde_json::Value>("payload")?;
            let mut row_blob_ids = Vec::new();
            crate::blob::collect_claim_check_blob_ids(&payload, &mut row_blob_ids)?;
            for blob_id in row_blob_ids {
                if seen_blob_ids.insert(blob_id.clone()) {
                    blob_ids.push(blob_id);
                }
            }
            payloads.push(payload);
        }

        let blob_cache = self.blob_store.get_many(&session_id, &blob_ids).await?;
        rows.iter()
            .zip(payloads)
            .map(|(row, payload)| {
                let event = crate::blob::decode_event_from_cache(payload, &blob_cache)?;
                Self::event_record_from_row_parts(row, event)
            })
            .collect()
    }
}

fn page_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn dashboard_detail_from_meta(
    meta: moa_core::types::session::SessionMeta,
) -> DashboardSessionDetail {
    let cache_hit_rate = meta.cache_hit_rate();
    DashboardSessionDetail {
        session_id: meta.id,
        tenant_id: meta.tenant_id,
        title: meta.title,
        status: meta.status,
        channel: meta.channel,
        model: meta.model,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        completed_at: meta.completed_at,
        parent_session_id: meta.parent_session_id,
        contact_id: meta.contact.as_ref().map(|contact| contact.contact_id),
        created_by: meta.created_by,
        total_input_tokens: meta.total_input_tokens,
        total_output_tokens: meta.total_output_tokens,
        total_cost_cents: meta.total_cost_cents,
        event_count: meta.event_count,
        cache_hit_rate,
    }
}

fn dashboard_timeline_item_from_record(record: &EventRecord) -> DashboardEventTimelineItem {
    DashboardEventTimelineItem {
        event_id: record.id,
        session_id: record.session_id,
        sequence_num: record.sequence_num,
        event_type: record.event_type,
        timestamp: record.timestamp,
        brain_id: record.brain_id,
        hand_id: record.hand_id.clone(),
        token_count: record.token_count,
        summary: redacted_event_summary(&record.event),
    }
}

fn redacted_event_summary(event: &Event) -> String {
    match event {
        Event::SessionCreated { channel, .. } => {
            format!("session created via {}", channel.as_str())
        }
        Event::SessionStatusChanged { from, to } => {
            format!("status changed from {} to {}", from.as_str(), to.as_str())
        }
        Event::SessionChannelChanged { from, to, .. } => {
            format!("channel changed from {} to {}", from.as_str(), to.as_str())
        }
        Event::SegmentStarted { segment_index, .. } => {
            format!("segment {segment_index} started")
        }
        Event::SegmentCompleted {
            segment_index,
            turn_count,
            ..
        } => format!("segment {segment_index} completed after {turn_count} turns"),
        Event::UserMessage { attachments, .. } => {
            format!("user message with {} attachments", attachments.len())
        }
        Event::QueuedMessage { attachments, .. } => {
            format!("queued user message with {} attachments", attachments.len())
        }
        Event::QueuedMessageRejected {
            queue_index,
            rejection,
            ..
        } => format!("queued message {queue_index} rejected: {rejection:?}"),
        Event::ExecutionRunStarted(started) => {
            format!("execution run {} started", started.run_uid)
        }
        Event::ExecutionProgress(progress) => format!(
            "execution run {} progress {}/{} ready={} active={} parked={} blocker={:?} status={}",
            progress.run_uid,
            progress.completed,
            progress.total,
            progress.ready_tasks,
            progress.active_tasks,
            progress.parked_tasks,
            progress.blocker_audience,
            progress.status
        ),
        Event::ExecutionInputRequired(required) => {
            format!("execution run {} requires user input", required.run_uid)
        }
        Event::ExecutionCompleted(summary) => {
            format!("execution run {} completed", summary.run_uid)
        }
        Event::ExecutionFailed {
            disposition,
            summary,
        } => format!(
            "execution run {} failed disposition={disposition:?}",
            summary.run_uid
        ),
        Event::ExecutionCancelled(summary) => {
            format!("execution run {} cancelled", summary.run_uid)
        }
        Event::ExecutionSynthesisRequested(requested) => format!(
            "execution run {} synthesis requested for turn {}",
            requested.run_uid, requested.turn_id
        ),
        Event::BrainThinking { token_count, .. } => {
            format!("assistant thinking summary used {token_count} tokens")
        }
        Event::BrainResponse {
            model,
            output_tokens,
            duration_ms,
            ..
        } => format!(
            "assistant response from {model} used {output_tokens} output tokens in {duration_ms}ms"
        ),
        Event::ProgressUpdate { elapsed_ms, .. } => {
            format!("progress update after {elapsed_ms}ms")
        }
        Event::GuardrailCheck {
            direction,
            passed,
            enforced,
            ..
        } => format!("guardrail check direction={direction:?} passed={passed} enforced={enforced}"),
        Event::ToolCall { tool_name, .. } => format!("tool call: {tool_name}"),
        Event::ToolResult {
            tool_id,
            success,
            duration_ms,
            ..
        } => format!("tool result for {tool_id} success={success} duration_ms={duration_ms}"),
        Event::ToolError {
            tool_name,
            retryable,
            ..
        } => format!("tool error from {tool_name} retryable={retryable}"),
        // Safe by construction: the transition carries only closed vocabulary —
        // a class, a capability identity, and two stages — never output bytes.
        Event::PromptInjectionCircuitTransition { transition, .. } => format!(
            "prompt-injection circuit {} -> {} for {} ({})",
            transition.prior_stage.as_str(),
            transition.reached_stage.as_str(),
            transition.capability.render(),
            transition.class.as_str()
        ),
        Event::ActionReviewRequested { review_id, .. } => {
            format!("action review requested: {review_id}")
        }
        Event::ActionReviewDecided { review_id, .. } => {
            format!("action review decided: {review_id}")
        }
        Event::ActionReviewTimedOut { review_id, .. } => {
            format!("action review timed out: {review_id}")
        }
        // Redacted by construction: only the review and continuation turn identifiers
        // reach the dashboard summary. The receipt's outcome summary can quote tool
        // output, so it stays out of this operator-facing projection.
        Event::ActionReviewContinuationRequested {
            review_id, turn_id, ..
        } => {
            format!("action review continuation requested: {review_id} turn={turn_id}")
        }
        Event::WorkerSpawned {
            worker_id,
            budget_tokens,
            ..
        } => format!("worker {worker_id} spawned with {budget_tokens} budget tokens"),
        Event::WorkerMessageSent {
            worker_id,
            input_request_id,
            ..
        } => format!(
            "worker {worker_id} message sent input_request_id={}",
            input_request_id.as_deref().unwrap_or("none")
        ),
        Event::WorkerStatusChanged { worker_id, to, .. } => {
            format!("worker {worker_id} status changed to {to:?}")
        }
        Event::WorkerNotificationDelivered {
            worker_id, state, ..
        } => format!("worker {worker_id} terminal notification delivered: {state:?}"),
        Event::TurnMetrics {
            turn_id,
            durable_appends,
            ..
        } => format!("turn metrics for {turn_id} with {durable_appends} durable appends"),
        // The class is already the coarse, secret-free attribution, so the
        // dashboard renders it directly instead of re-deriving a summary.
        Event::TurnFailed {
            actor,
            turn_id,
            class,
            ..
        } => format!(
            "{} turn {turn_id} failed during {class:?}",
            actor.actor_key()
        ),
        Event::WorkerSignalReceived {
            signal_id,
            worker_id,
            kind,
            ..
        } => format!("worker {worker_id} signal {signal_id} received: {kind:?}"),
        Event::WorkerParentResumeRequested {
            signal_id, turn_id, ..
        } => format!("parent resume requested for signal {signal_id} on turn {turn_id}"),
        Event::WorkerHeartbeatStale {
            worker_id,
            threshold_ms,
            ..
        } => format!("worker {worker_id} heartbeat stale after {threshold_ms}ms"),
        Event::MemoryRead { .. } => "memory read".to_string(),
        Event::MemoryWrite { .. } => "memory write".to_string(),
        Event::MemoryIngest { affected_pages, .. } => {
            format!("memory ingest affected {} pages", affected_pages.len())
        }
        Event::Checkpoint {
            events_summarized,
            token_count,
            ..
        } => format!("checkpoint summarized {events_summarized} events into {token_count} tokens"),
        Event::CacheReport { .. } => "cache report recorded".to_string(),
        Event::Error { recoverable, .. } => format!("error recorded recoverable={recoverable}"),
        Event::Warning { .. } => "warning recorded".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::events::{Event, ExecutionTaskResultsRef, ExecutionTerminalSummary};
    use uuid::Uuid;

    use super::redacted_event_summary;

    #[test]
    fn execution_dashboard_summary_never_copies_terminal_evidence() {
        // Pins: operator summaries identify the run/status while keeping aggregate output,
        // citations, failure/gap bodies, and the task table out of dashboard rows.
        let run_uid = Uuid::from_u128(111);
        let summary =
            redacted_event_summary(&Event::ExecutionCompleted(ExecutionTerminalSummary {
                run_uid,
                originating_user_sequence_num: 8,
                output: Some(serde_json::json!({
                    "private": "aggregate-output-sentinel"
                })),
                output_hash: [8; 32],
                citation_ids: vec!["citation-sentinel".to_string()],
                failures: vec!["failure-sentinel".to_string()],
                gaps: vec!["gap-sentinel".to_string()],
                task_results: ExecutionTaskResultsRef::ExecutionTaskTable { run_uid },
            }));

        assert_eq!(summary, format!("execution run {run_uid} completed"));
        for forbidden in [
            "aggregate-output-sentinel",
            "citation-sentinel",
            "failure-sentinel",
            "gap-sentinel",
            "execution_task_table",
        ] {
            assert!(!summary.contains(forbidden));
        }
    }
}

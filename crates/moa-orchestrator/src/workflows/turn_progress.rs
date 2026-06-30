//! Durable progress projection helpers for turn workflows.

use chrono::{DateTime, Utc};
use moa_core::wire::session_store::AppendEventRequest;
use moa_core::wire::turn::TurnPhase;
use moa_core::{Event, SessionId};
use moa_observability::record_turn_event_persist_duration;
use moa_observability::restate_observability::event_persist_span;
use restate_sdk::prelude::*;
use std::time::Instant;
use tracing::Instrument;

use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::progress_delivery;

const K_PROGRESS_STARTED_AT: &str = "progress_started_at";
const K_PROGRESS_LAST_EMITTED_AT: &str = "progress_last_emitted_at";
const K_PROGRESS_ELAPSED_MS: &str = "progress_elapsed_ms";
const K_PROGRESS_LAST_SUMMARY: &str = "progress_last_summary";

/// Generic progress summary used before context or request compilation waits.
pub(crate) const SUMMARY_WORKING: &str = "Working on it";
/// Generic progress summary used before model calls.
pub(crate) const SUMMARY_CALLING_MODEL: &str = "Calling the model";
/// Generic progress summary used before validation or guardrail checks.
pub(crate) const SUMMARY_CHECKING_RESULTS: &str = "Checking results";

/// Snapshot returned to workflow progress handlers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProgressSnapshot {
    /// Elapsed runtime in milliseconds.
    pub(crate) elapsed_ms: u64,
    /// Last durable progress summary emitted for this turn.
    pub(crate) last_summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProgressState {
    started_at: Option<DateTime<Utc>>,
    last_emitted_at: Option<DateTime<Utc>>,
    elapsed_ms: u64,
    last_summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProgressAttempt {
    emit: Option<String>,
}

impl ProgressState {
    fn initialized(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at: Some(started_at),
            last_emitted_at: None,
            elapsed_ms: 0,
            last_summary: None,
        }
    }

    fn attempt(
        &mut self,
        now: DateTime<Utc>,
        summary: String,
        first_delay_ms: u64,
        interval_ms: u64,
    ) -> ProgressAttempt {
        let started_at = *self.started_at.get_or_insert(now);
        self.elapsed_ms = elapsed_ms(started_at, now);
        if self.elapsed_ms < first_delay_ms {
            return ProgressAttempt { emit: None };
        }

        if self.last_summary.as_deref() == Some(summary.as_str()) {
            return ProgressAttempt { emit: None };
        }

        if let Some(last_emitted_at) = self.last_emitted_at {
            let since_last_ms = elapsed_ms(last_emitted_at, now);
            if since_last_ms < interval_ms {
                return ProgressAttempt { emit: None };
            }
        }

        self.last_emitted_at = Some(now);
        self.last_summary = Some(summary.clone());
        ProgressAttempt {
            emit: Some(summary),
        }
    }

    fn finish(&mut self, now: DateTime<Utc>) {
        if let Some(started_at) = self.started_at {
            self.elapsed_ms = elapsed_ms(started_at, now);
        }
    }

    fn snapshot(&self, now: Option<DateTime<Utc>>) -> ProgressSnapshot {
        let elapsed_ms = match (self.started_at, now) {
            (Some(started_at), Some(now)) => self.elapsed_ms.max(elapsed_ms(started_at, now)),
            _ => self.elapsed_ms,
        };
        ProgressSnapshot {
            elapsed_ms,
            last_summary: self.last_summary.clone(),
        }
    }
}

/// Initializes helper-owned durable progress state for a turn workflow.
pub(crate) async fn initialize(ctx: &WorkflowContext<'_>) -> Result<(), HandlerError> {
    let now = workflow_utc_now(ctx).await?;
    store_state(ctx, &ProgressState::initialized(now));
    Ok(())
}

/// Enables live channel status delivery for progress emitted by this workflow.
pub(crate) fn enable_live_delivery(ctx: &WorkflowContext<'_>) {
    progress_delivery::enable_live_delivery(ctx);
}

/// Records final elapsed time without appending a progress event.
pub(crate) async fn finish(ctx: &WorkflowContext<'_>) -> Result<(), HandlerError> {
    let mut state = load_workflow_state(ctx).await?;
    if state.started_at.is_none() {
        return Ok(());
    }
    let now = workflow_utc_now(ctx).await?;
    state.finish(now);
    store_state(ctx, &state);
    Ok(())
}

/// Records final elapsed time and updates any live root-turn status message.
pub(crate) async fn finish_with_live_delivery(
    ctx: &WorkflowContext<'_>,
    session_id: moa_core::SessionId,
    phase: TurnPhase,
) -> Result<(), HandlerError> {
    finish(ctx).await?;
    progress_delivery::maybe_deliver_terminal(ctx, session_id, phase).await?;
    Ok(())
}

/// Attempts to append a durable progress event while respecting delay and cadence limits.
pub(crate) async fn maybe_emit(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    phase: TurnPhase,
    summary: impl Into<String>,
    first_delay_ms: u64,
    interval_ms: u64,
) -> Result<(), HandlerError> {
    let mut state = load_workflow_state(ctx).await?;
    let now = workflow_utc_now(ctx).await?;
    let attempt = state.attempt(now, summary.into(), first_delay_ms, interval_ms);
    if let Some(summary) = attempt.emit {
        append_progress_event(ctx, session_id, turn_id, &phase, &summary, state.elapsed_ms).await?;
        progress_delivery::maybe_deliver(ctx, session_id, &summary).await?;
    }
    store_state(ctx, &state);
    Ok(())
}

/// Returns helper-owned progress state for shared workflow progress handlers.
pub(crate) async fn snapshot(
    ctx: &SharedWorkflowContext<'_>,
    terminal: bool,
) -> Result<ProgressSnapshot, HandlerError> {
    let state = load_shared_state(ctx).await?;
    let now = if terminal || state.started_at.is_none() {
        None
    } else {
        Some(shared_utc_now(ctx).await?)
    };
    Ok(state.snapshot(now))
}

/// Returns a short safe tool progress summary.
pub(crate) fn running_tool_summary(tool_name: &str) -> String {
    format!("Running tool: {}", safe_tool_name(tool_name))
}

async fn append_progress_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    phase: &TurnPhase,
    summary: &str,
    elapsed_ms: u64,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::ProgressUpdate {
                turn_id: turn_id.to_string(),
                phase: format!("{phase:?}"),
                summary: summary.to_string(),
                elapsed_ms,
            },
            dedupe_key: None,
        }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
}

async fn load_workflow_state(ctx: &WorkflowContext<'_>) -> Result<ProgressState, HandlerError> {
    Ok(ProgressState {
        started_at: ctx
            .get::<Json<DateTime<Utc>>>(K_PROGRESS_STARTED_AT)
            .await?
            .map(Json::into_inner),
        last_emitted_at: ctx
            .get::<Json<DateTime<Utc>>>(K_PROGRESS_LAST_EMITTED_AT)
            .await?
            .map(Json::into_inner),
        elapsed_ms: ctx
            .get::<Json<u64>>(K_PROGRESS_ELAPSED_MS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_summary: ctx
            .get::<Json<Option<String>>>(K_PROGRESS_LAST_SUMMARY)
            .await?
            .map(Json::into_inner)
            .unwrap_or(None),
    })
}

async fn load_shared_state(ctx: &SharedWorkflowContext<'_>) -> Result<ProgressState, HandlerError> {
    Ok(ProgressState {
        started_at: ctx
            .get::<Json<DateTime<Utc>>>(K_PROGRESS_STARTED_AT)
            .await?
            .map(Json::into_inner),
        last_emitted_at: ctx
            .get::<Json<DateTime<Utc>>>(K_PROGRESS_LAST_EMITTED_AT)
            .await?
            .map(Json::into_inner),
        elapsed_ms: ctx
            .get::<Json<u64>>(K_PROGRESS_ELAPSED_MS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_summary: ctx
            .get::<Json<Option<String>>>(K_PROGRESS_LAST_SUMMARY)
            .await?
            .map(Json::into_inner)
            .unwrap_or(None),
    })
}

fn store_state(ctx: &WorkflowContext<'_>, state: &ProgressState) {
    if let Some(started_at) = state.started_at {
        ctx.set(K_PROGRESS_STARTED_AT, Json::from(started_at));
    }
    if let Some(last_emitted_at) = state.last_emitted_at {
        ctx.set(K_PROGRESS_LAST_EMITTED_AT, Json::from(last_emitted_at));
    }
    ctx.set(K_PROGRESS_ELAPSED_MS, Json::from(state.elapsed_ms));
    ctx.set(
        K_PROGRESS_LAST_SUMMARY,
        Json::from(state.last_summary.clone()),
    );
}

async fn workflow_utc_now(ctx: &WorkflowContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("turn_progress_utc_now")
        .await?
        .into_inner())
}

async fn shared_utc_now(ctx: &SharedWorkflowContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("turn_progress_snapshot_utc_now")
        .await?
        .into_inner())
}

fn elapsed_ms(started_at: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    now.signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as u64
}

fn safe_tool_name(tool_name: &str) -> &str {
    let name = tool_name.trim();
    if name.is_empty()
        || name.len() > 64
        || name
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':')))
    {
        return "tool";
    }
    name
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::{
        ProgressState, SUMMARY_CALLING_MODEL, SUMMARY_CHECKING_RESULTS, elapsed_ms,
        running_tool_summary,
    };

    fn at(ms: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .timestamp_millis_opt(ms)
            .single()
            .expect("test timestamp should be valid")
    }

    #[test]
    fn first_delay_gates_initial_progress_emit() {
        // Pins: fast turns do not emit durable progress before the first-delay threshold.
        let mut state = ProgressState::initialized(at(0));
        let attempt = state.attempt(at(7_999), SUMMARY_CALLING_MODEL.to_string(), 8_000, 8_000);

        assert_eq!(attempt.emit, None);
        assert_eq!(state.elapsed_ms, 7_999);
        assert_eq!(state.last_summary, None);
        assert_eq!(state.last_emitted_at, None);
    }

    #[test]
    fn progress_emits_after_first_delay() {
        // Pins: the first eligible long-call boundary produces a replayable summary.
        let mut state = ProgressState::initialized(at(0));
        let attempt = state.attempt(at(8_000), SUMMARY_CALLING_MODEL.to_string(), 8_000, 8_000);

        assert_eq!(attempt.emit, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(state.elapsed_ms, 8_000);
        assert_eq!(state.last_summary, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(state.last_emitted_at, Some(at(8_000)));
    }

    #[test]
    fn interval_gates_follow_up_progress_emit() {
        // Pins: different progress summaries still obey the configured global interval.
        let mut state = ProgressState::initialized(at(0));
        let first = state.attempt(at(8_000), SUMMARY_CALLING_MODEL.to_string(), 8_000, 8_000);
        let second = state.attempt(
            at(12_000),
            SUMMARY_CHECKING_RESULTS.to_string(),
            8_000,
            8_000,
        );
        let third = state.attempt(
            at(16_000),
            SUMMARY_CHECKING_RESULTS.to_string(),
            8_000,
            8_000,
        );

        assert_eq!(first.emit, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(second.emit, None);
        assert_eq!(third.emit, Some(SUMMARY_CHECKING_RESULTS.to_string()));
        assert_eq!(
            state.last_summary,
            Some(SUMMARY_CHECKING_RESULTS.to_string())
        );
    }

    #[test]
    fn duplicate_progress_summary_does_not_spam_events() {
        // Pins: repeated identical boundaries do not append duplicate progress events.
        let mut state = ProgressState::initialized(at(0));
        let first = state.attempt(at(8_000), SUMMARY_CALLING_MODEL.to_string(), 8_000, 1_000);
        let second = state.attempt(at(20_000), SUMMARY_CALLING_MODEL.to_string(), 8_000, 1_000);

        assert_eq!(first.emit, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(second.emit, None);
        assert_eq!(state.last_summary, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(state.last_emitted_at, Some(at(8_000)));
        assert_eq!(state.elapsed_ms, 20_000);
    }

    #[test]
    fn running_tool_summary_uses_safe_tool_names() {
        // Pins: progress summaries reveal only bounded tool names, never inputs.
        assert_eq!(running_tool_summary("bash"), "Running tool: bash");
        assert_eq!(
            running_tool_summary("worker.spawn"),
            "Running tool: worker.spawn"
        );
        assert_eq!(running_tool_summary("bash\nsecret"), "Running tool: tool");
        assert_eq!(running_tool_summary(""), "Running tool: tool");
    }

    #[test]
    fn snapshot_projects_elapsed_and_last_summary() {
        // Pins: progress handlers can recover elapsed time and the last emitted summary from helper state.
        let mut state = ProgressState::initialized(at(0));
        let _ = state.attempt(at(8_000), SUMMARY_CALLING_MODEL.to_string(), 8_000, 8_000);
        let live = state.snapshot(Some(at(12_500)));
        state.finish(at(15_000));
        let terminal = state.snapshot(None);

        assert_eq!(elapsed_ms(at(0), at(12_500)), 12_500);
        assert_eq!(live.elapsed_ms, 12_500);
        assert_eq!(live.last_summary, Some(SUMMARY_CALLING_MODEL.to_string()));
        assert_eq!(terminal.elapsed_ms, 15_000);
        assert_eq!(
            terminal.last_summary,
            Some(SUMMARY_CALLING_MODEL.to_string())
        );
    }
}

//! Contact session message SSE stream helpers.

use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream;
use moa_core::wire::turn::SessionProgress;
use moa_core::{
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    Event, EventRange, EventRecord, SequenceNum, SessionId, TenantId, WorkerId,
    WorkerProgressSummary, WorkerState,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::{Duration, Instant};

/// Period of durable-event silence after which the stream emits a templated
/// `working` liveness frame while a descendant worker is still active.
///
/// This is a transient UI hint (never a persisted event): it keeps the screen
/// visibly alive between durable narrations, covering both a long *unchanged*
/// coordinator/child step (where narration correctly skips the model call) and
/// the narration-disabled case. `KeepAlive` still runs underneath for transport
/// liveness; this frame is a named, renderable event distinct from keep-alive
/// comments.
const LIVENESS_FRAME_INTERVAL: Duration = Duration::from_secs(10);

use super::{AppState, EdgeJsonError, call_contacts_handler};

pub(super) fn session_message_stream_response(
    app: AppState,
    message: ContactSessionMessageRequest,
    accepted: ContactSessionMessageResponse,
    next_sequence_num: SequenceNum,
) -> Response {
    let accepted_frame = SessionMessageAccepted {
        session_id: accepted.session_id,
        queued: accepted.queued,
        started_turn_id: accepted.started_turn_id,
        next_sequence_num,
    };
    let terminal_turn_id = accepted_frame.started_turn_id.clone();
    let mut pending_events = VecDeque::new();
    pending_events.push_back(json_sse_event("accepted", &accepted_frame));

    let stream_state = SessionMessageStreamState {
        app,
        tenant_id: message.tenant_id,
        session_id: accepted_frame.session_id,
        contact_token: message.contact_token,
        next_sequence_num,
        terminal_turn_id,
        pending_events,
        closed: false,
        last_frame_at: Instant::now(),
    };
    Sse::new(stream::unfold(stream_state, next_session_message_event))
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(super) async fn initial_stream_sequence(
    state: &AppState,
    message: &ContactSessionMessageRequest,
    reconnect_after: Option<SequenceNum>,
) -> Result<SequenceNum, EdgeJsonError> {
    if let Some(sequence_num) = reconnect_after {
        return Ok(sequence_num.saturating_add(1));
    }
    let progress = call_contacts_handler::<_, SessionProgress>(
        state,
        "progress",
        &ContactSessionProgressRequest {
            tenant_id: message.tenant_id,
            session_id: message.session_id,
            contact_token: message.contact_token.clone(),
            event_range: EventRange::recent(1),
        },
    )
    .await?;
    Ok(next_sequence_after(&progress.events))
}

pub(super) fn last_event_id_sequence(headers: &HeaderMap) -> Option<SequenceNum> {
    headers
        .get("last-event-id")?
        .to_str()
        .ok()?
        .parse::<SequenceNum>()
        .ok()
}

struct SessionMessageStreamState {
    app: AppState,
    tenant_id: TenantId,
    session_id: SessionId,
    contact_token: String,
    next_sequence_num: SequenceNum,
    terminal_turn_id: Option<String>,
    pending_events: VecDeque<SseEvent>,
    closed: bool,
    /// Wall-clock instant the last SSE frame was emitted, used to gate the
    /// silence-filler `working` liveness frame.
    last_frame_at: Instant,
}

#[derive(Debug, Serialize)]
struct SessionMessageAccepted {
    session_id: SessionId,
    queued: bool,
    started_turn_id: Option<String>,
    next_sequence_num: SequenceNum,
}

#[derive(Debug, Serialize)]
struct SessionMessageDone {
    session_id: SessionId,
    status: &'static str,
    last_turn_id: Option<String>,
}

/// Transient silence-filler liveness frame payload (never a persisted event).
///
/// Built from the fan-in summary of an active descendant so the UI can render a
/// templated "still working" line between durable narrations.
#[derive(Debug, Serialize)]
struct SessionMessageWorking {
    session_id: SessionId,
    /// Active descendant the liveness frame describes.
    worker_id: WorkerId,
    /// Templated, assistant-voice liveness line for direct rendering. It already carries the
    /// active descendant's summary and the elapsed silent window, so neither is duplicated as a
    /// separate wire field.
    message: String,
}

async fn next_session_message_event(
    mut state: SessionMessageStreamState,
) -> Option<(Result<SseEvent, Infallible>, SessionMessageStreamState)> {
    if state.closed {
        return None;
    }
    if let Some(event) = state.pending_events.pop_front() {
        state.last_frame_at = Instant::now();
        return Some((Ok(event), state));
    }

    loop {
        match fetch_stream_progress(&state).await {
            Ok(progress) => {
                let done_event = session_message_stream_done(&state, &progress)
                    .then(|| done_sse_event(&state, &progress));
                enqueue_progress_events(&mut state, progress.events);
                if let Some(event) = state.pending_events.pop_front() {
                    state.last_frame_at = Instant::now();
                    return Some((Ok(event), state));
                }
                if let Some(event) = done_event {
                    state.closed = true;
                    return Some((Ok(event), state));
                }
                // No new durable events and the task tree is still active: keep
                // the screen visibly alive with a templated `working` frame after
                // a window of silence. This persists no event.
                if let Some(frame) = liveness_working_frame(&state, &progress.child_progress) {
                    state.last_frame_at = Instant::now();
                    return Some((Ok(frame), state));
                }
            }
            Err(error) => {
                state.closed = true;
                return Some((Ok(error_sse_event(error.summary())), state));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn fetch_stream_progress(
    state: &SessionMessageStreamState,
) -> Result<SessionProgress, EdgeJsonError> {
    call_contacts_handler(
        &state.app,
        "progress",
        &ContactSessionProgressRequest {
            tenant_id: state.tenant_id,
            session_id: state.session_id,
            contact_token: state.contact_token.clone(),
            event_range: EventRange {
                from_seq: Some(state.next_sequence_num),
                to_seq: None,
                event_types: None,
                limit: Some(100),
            },
        },
    )
    .await
}

fn enqueue_progress_events(state: &mut SessionMessageStreamState, records: Vec<EventRecord>) {
    for record in records {
        state.next_sequence_num = state
            .next_sequence_num
            .max(record.sequence_num.saturating_add(1));
        // A follow-on coordinator turn (auto-delegation synthesis or a guarded child-signal
        // resume) runs under a NEW turn id after the initial turn completed. Re-target the
        // stream's terminal turn to it so the stream neither closes early — before the
        // follow-on answer streams — nor leaks waiting on a turn id that never completes again.
        // `pending_events` (this event) is drained before the done-check, so the poll that
        // observes the follow-on event never closes on the stale terminal turn.
        if let Some(turn_id) = follow_on_terminal_turn(&record.event) {
            state.terminal_turn_id = Some(turn_id.to_string());
        }
        state.pending_events.push_back(record_sse_event(&record));
    }
}

fn next_sequence_after(records: &[EventRecord]) -> SequenceNum {
    records
        .iter()
        .map(|record| record.sequence_num)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn session_message_stream_done(
    state: &SessionMessageStreamState,
    progress: &SessionProgress,
) -> bool {
    session_message_terminal_done(state.terminal_turn_id.as_deref(), progress)
}

fn session_message_terminal_done(
    terminal_turn_id: Option<&str>,
    progress: &SessionProgress,
) -> bool {
    if let Some(turn_id) = terminal_turn_id {
        let turn_completed = progress
            .snapshot
            .last_outcome
            .as_ref()
            .is_some_and(|outcome| outcome.turn_id == turn_id);
        // Keep the stream open across the detached window: a coordinator turn can
        // return and go idle while spawned children keep running. Only close once
        // the started turn completed AND no descendant is still doing work. With
        // no children this collapses to the prior turn-completion check, so the
        // no-delegation case is unchanged.
        return turn_completed && !child_progress_blocks_close(&progress.child_progress);
    }
    progress.snapshot.active_turn_id.is_none()
        && progress.snapshot.pending_message_count == 0
        && !child_progress_blocks_close(&progress.child_progress)
}

/// Whether a child lifecycle state is terminal (no further work expected).
fn is_terminal_child_state(state: WorkerState) -> bool {
    matches!(
        state,
        WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
    )
}

/// First non-terminal descendant in a fan-in summary, when one is still active.
fn active_child_summary(
    child_progress: &[WorkerProgressSummary],
) -> Option<&WorkerProgressSummary> {
    child_progress
        .iter()
        .find(|child| !is_terminal_child_state(child.state))
}

/// The follow-on coordinator turn id introduced by a synthesis/resume event, if any.
///
/// After the initial turn completes, auto-delegation synthesis and guarded child-signal resume
/// each dispatch a NEW coordinator turn. The SSE stream re-targets its terminal turn to this id
/// so it closes on the follow-on turn's completion rather than the original turn's.
fn follow_on_terminal_turn(event: &Event) -> Option<&str> {
    match event {
        Event::WorkerResultSynthesisRequested { turn_id, .. }
        | Event::WorkerParentResumeRequested { turn_id, .. } => Some(turn_id),
        _ => None,
    }
}

/// Whether any descendant is still doing work that should hold the SSE stream open.
///
/// A non-terminal child normally blocks close, EXCEPT one whose heartbeat has gone stale
/// without awaiting input: the liveness watchdog has flagged it as stuck, so it must not hold
/// the stream open (and its 1s poll) indefinitely. An `awaiting_input` child is never treated
/// as stuck — it is legitimately parked on a question.
fn child_progress_blocks_close(child_progress: &[WorkerProgressSummary]) -> bool {
    child_progress.iter().any(|child| {
        !is_terminal_child_state(child.state) && (!child.stale || child.awaiting_input)
    })
}

fn done_sse_event(state: &SessionMessageStreamState, progress: &SessionProgress) -> SseEvent {
    let last_turn_id = progress
        .snapshot
        .last_outcome
        .as_ref()
        .map(|outcome| outcome.turn_id.clone());
    json_sse_event(
        "done",
        &SessionMessageDone {
            session_id: state.session_id,
            status: done_status(progress),
            last_turn_id,
        },
    )
}

fn done_status(progress: &SessionProgress) -> &'static str {
    let Some(outcome) = progress.snapshot.last_outcome.as_ref() else {
        return "idle";
    };
    match outcome.kind {
        moa_core::wire::turn::TurnOutcomeKind::Completed => "completed",
        moa_core::wire::turn::TurnOutcomeKind::Cancelled => "cancelled",
        moa_core::wire::turn::TurnOutcomeKind::Failed => "failed",
    }
}

fn liveness_working_frame(
    state: &SessionMessageStreamState,
    child_progress: &[WorkerProgressSummary],
) -> Option<SseEvent> {
    if state.last_frame_at.elapsed() < LIVENESS_FRAME_INTERVAL {
        return None;
    }
    let active = active_child_summary(child_progress)?;
    let payload = working_frame_payload(state.session_id, active, state.last_frame_at.elapsed());
    Some(json_sse_event("working", &payload))
}

fn working_frame_payload(
    session_id: SessionId,
    child: &WorkerProgressSummary,
    elapsed: Duration,
) -> SessionMessageWorking {
    let elapsed_secs = elapsed.as_secs();
    let message = match child.last_summary.as_deref() {
        Some(summary) => {
            format!("Still working — {summary} ({elapsed_secs}s since the last update)")
        }
        None => format!("Still working… ({elapsed_secs}s since the last update)"),
    };
    SessionMessageWorking {
        session_id,
        worker_id: child.worker_id.clone(),
        message,
    }
}

/// SSE frame name a durable session event maps to. Child progress, signals, and
/// liveness events get distinct names; everything else falls back to the generic
/// `session_event` frame, which clients can ignore when unknown.
fn sse_event_name(event: &Event) -> &'static str {
    match event {
        Event::ProgressUpdate { .. } => "progress",
        Event::ProgressNarrated { .. } => "progress_narration",
        Event::WorkerSignalReceived {
            kind: moa_core::ChildSignalKind::NeedsInput,
            input_audience: Some(moa_core::InputAudience::User),
            ..
        } => "worker_input_request",
        Event::WorkerSignalReceived { .. } => "worker_signal",
        Event::WorkerParentResumeRequested { .. } => "worker_resume",
        Event::WorkerHeartbeatStale { .. } => "worker_stale",
        Event::BrainResponse { .. } => "response",
        Event::ToolCall { .. } | Event::ToolResult { .. } | Event::ToolError { .. } => "tool",
        _ => "session_event",
    }
}

fn record_sse_event(record: &EventRecord) -> SseEvent {
    match SseEvent::default()
        .id(record.sequence_num.to_string())
        .event(sse_event_name(&record.event))
        .json_data(record)
    {
        Ok(event) => event,
        Err(error) => error_sse_event(format!("failed to serialize session event: {error}")),
    }
}

fn json_sse_event<T: Serialize>(event_name: &'static str, data: &T) -> SseEvent {
    match SseEvent::default().event(event_name).json_data(data) {
        Ok(event) => event,
        Err(error) => error_sse_event(format!("failed to serialize SSE event: {error}")),
    }
}

fn error_sse_event(message: String) -> SseEvent {
    let data = serde_json::json!({ "message": message });
    SseEvent::default().event("error").data(data.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::wire::turn::{SessionSnapshot, TurnOutcome, TurnOutcomeKind};
    use moa_core::{EventRecord, EventType};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn session_message_stream_cursor_starts_after_latest_event() {
        // Pins: a fresh browser message stream does not replay old session history.
        let records = vec![event_record(4), event_record(9), event_record(7)];

        assert_eq!(next_sequence_after(&records), 10);
        assert_eq!(next_sequence_after(&[]), 1);
    }

    #[test]
    fn session_message_stream_cursor_honors_last_event_id() {
        // Pins: reconnecting streams resume after the browser's Last-Event-ID cursor.
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "41".parse().expect("valid header value"));

        assert_eq!(last_event_id_sequence(&headers), Some(41));
        headers.insert(
            "last-event-id",
            "not-a-number".parse().expect("valid header value"),
        );
        assert_eq!(last_event_id_sequence(&headers), None);
    }

    #[test]
    fn session_message_stream_finishes_when_started_turn_reports_outcome() {
        // Pins: a no-delegation stream still closes on its started turn, not on later session idle.
        let progress = session_progress(
            Some("next-turn".to_string()),
            1,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "done".to_string(),
            }),
        );

        assert!(session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));
        assert!(!session_message_terminal_done(
            Some("other-turn"),
            &progress
        ));
    }

    #[test]
    fn session_message_stream_stays_open_while_a_child_is_active() {
        // Pins: a coordinator turn that completed but left an active descendant keeps the stream open.
        let mut progress = session_progress(
            None,
            0,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "spawned a child".to_string(),
            }),
        );
        progress.child_progress = vec![
            child_summary(WorkerState::Completed, Some("first child done")),
            child_summary(WorkerState::Running, Some("still searching")),
        ];

        assert!(!session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));
    }

    #[test]
    fn session_message_stream_closes_when_started_turn_done_and_tree_terminal() {
        // Pins: the stream closes only once the started turn completed and every descendant is terminal.
        let mut progress = session_progress(
            None,
            0,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "all children reported".to_string(),
            }),
        );
        progress.child_progress = vec![
            child_summary(WorkerState::Completed, Some("child a done")),
            child_summary(WorkerState::Failed, Some("child b failed")),
            child_summary(WorkerState::Cancelled, None),
        ];

        assert!(session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));
    }

    #[test]
    fn no_delegation_stream_closes_on_turn_completion_unchanged() {
        // Pins: with no children the close condition is exactly the started-turn outcome check.
        let done = session_progress(
            None,
            0,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "done".to_string(),
            }),
        );
        let still_running = session_progress(Some("started-turn".to_string()), 0, None);

        assert!(done.child_progress.is_empty());
        assert!(session_message_terminal_done(Some("started-turn"), &done));
        assert!(!session_message_terminal_done(
            Some("started-turn"),
            &still_running
        ));
    }

    #[test]
    fn record_sse_event_maps_worker_and_narration_events_to_frame_names() {
        // Pins: child progress/signal/liveness events stream as distinct, named SSE frames.
        assert_eq!(
            sse_event_name(&Event::ProgressNarrated {
                source: moa_core::NarrationSource::Coordinator,
                text: "Searching the pricing docs".to_string(),
                segments: Vec::new(),
                model: "none".to_string(),
                tokens_used: 0,
            }),
            "progress_narration"
        );
        assert_eq!(
            sse_event_name(&Event::WorkerSignalReceived {
                signal_id: moa_core::AgentSignalId::new(),
                worker_id: "child-1".to_string(),
                kind: moa_core::ChildSignalKind::Blocked,
                severity: moa_core::SignalSeverity::Warning,
                summary: "blocked on input".to_string(),
                input_request_id: None,
                input_audience: None,
            }),
            "worker_signal"
        );
        assert_eq!(
            sse_event_name(&Event::WorkerSignalReceived {
                signal_id: moa_core::AgentSignalId::new(),
                worker_id: "child-1".to_string(),
                kind: moa_core::ChildSignalKind::NeedsInput,
                severity: moa_core::SignalSeverity::Warning,
                summary: "needs user input".to_string(),
                input_request_id: Some("req-1".to_string()),
                input_audience: Some(moa_core::InputAudience::User),
            }),
            "worker_input_request"
        );
        assert_eq!(
            sse_event_name(&Event::WorkerParentResumeRequested {
                signal_id: moa_core::AgentSignalId::new(),
                worker_id: "child-1".to_string(),
                turn_id: "turn-1".to_string(),
                reason: "child blocked".to_string(),
            }),
            "worker_resume"
        );
        assert_eq!(
            sse_event_name(&Event::WorkerHeartbeatStale {
                worker_id: "child-1".to_string(),
                last_heartbeat_at: Utc::now(),
                threshold_ms: 30_000,
            }),
            "worker_stale"
        );
        // Terminal lifecycle events stay on the generic frame, unchanged.
        assert_eq!(
            sse_event_name(&Event::WorkerNotificationDelivered {
                worker_id: "child-1".to_string(),
                state: WorkerState::Completed,
                summary: "ok".to_string(),
            }),
            "session_event"
        );
    }

    #[test]
    fn working_frame_payload_renders_active_child_summary() {
        // Pins: the silence-filler frame echoes the active child's summary and the silent window
        // inside the pre-rendered `message`, and does not duplicate them as separate wire fields.
        let child = child_summary(WorkerState::Running, Some("indexing the corpus"));
        let payload =
            working_frame_payload(SessionId(Uuid::nil()), &child, Duration::from_secs(12));

        assert_eq!(payload.worker_id, "child-1");
        assert!(payload.message.contains("indexing the corpus"));
        assert!(payload.message.contains("12s"));

        let json = serde_json::to_value(&payload).expect("serialize working frame");
        assert_eq!(json["worker_id"], "child-1");
        assert!(json.get("summary").is_none());
        assert!(json.get("elapsed_ms").is_none());
    }

    #[test]
    fn session_progress_round_trips_child_progress_and_tolerates_missing_field() {
        // Pins: child_progress round-trips, and a payload that omits it decodes to an empty list.
        let mut progress = session_progress(Some("turn-1".to_string()), 0, None);
        progress.child_progress = vec![child_summary(WorkerState::Running, Some("running"))];

        let json = serde_json::to_string(&progress).expect("serialize session progress");
        assert!(json.contains("\"child_progress\""));
        let decoded: SessionProgress =
            serde_json::from_str(&json).expect("round-trip session progress");
        assert_eq!(decoded.child_progress.len(), 1);

        let without_child_progress: SessionProgress = serde_json::from_str(
            r#"{"snapshot":{"session_id":"s","active_turn_id":null,"pending_message_count":0,"last_outcome":null},"events":[]}"#,
        )
        .expect("deserialize payload without child_progress");
        assert!(without_child_progress.child_progress.is_empty());
    }

    #[test]
    fn queued_session_message_stream_finishes_when_session_is_idle() {
        // Pins: queued messages have no accepted turn id, so their stream waits for the queue to drain.
        let running = session_progress(Some("active-turn".to_string()), 1, None);
        let mut idle = session_progress(None, 0, None);
        let mut child_active = session_progress(None, 0, None);
        child_active.child_progress = vec![child_summary(WorkerState::Running, Some("running"))];

        assert!(!session_message_terminal_done(None, &running));
        assert!(!session_message_terminal_done(None, &child_active));
        assert!(session_message_terminal_done(None, &idle));
        idle.child_progress = vec![child_summary(WorkerState::Completed, Some("done"))];
        assert!(session_message_terminal_done(None, &idle));
    }

    #[test]
    fn follow_on_terminal_turn_targets_synthesis_and_resume_turns() {
        // Pins (B3): synthesis and guarded-resume events carry the follow-on coordinator turn id
        // the stream must re-target to; other events do not move the terminal turn.
        assert_eq!(
            follow_on_terminal_turn(&Event::WorkerResultSynthesisRequested {
                user_sequence_num: 1,
                turn_id: "synth-turn".to_string(),
                reason: "synthesize".to_string(),
            }),
            Some("synth-turn")
        );
        assert_eq!(
            follow_on_terminal_turn(&Event::WorkerParentResumeRequested {
                signal_id: moa_core::AgentSignalId::new(),
                worker_id: "child-1".to_string(),
                turn_id: "resume-turn".to_string(),
                reason: "child needs attention".to_string(),
            }),
            Some("resume-turn")
        );
        assert_eq!(
            follow_on_terminal_turn(&Event::Warning {
                message: "x".to_string()
            }),
            None
        );
    }

    #[test]
    fn stream_tracks_follow_on_synthesis_turn_completion() {
        // Pins (B3): after re-targeting to the synthesis turn, the stream stays open until THAT
        // turn completes (not the original turn), then closes — so it neither closes early nor
        // leaks.
        let mut running = session_progress(
            Some("synth-turn".to_string()),
            0,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "spawned".to_string(),
            }),
        );
        running.child_progress = vec![child_summary(WorkerState::Completed, Some("done"))];
        // Original turn's completion + terminal children no longer closes the re-targeted stream.
        assert!(!session_message_terminal_done(Some("synth-turn"), &running));

        let done = session_progress(
            None,
            0,
            Some(TurnOutcome {
                turn_id: "synth-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "final answer".to_string(),
            }),
        );
        assert!(session_message_terminal_done(Some("synth-turn"), &done));
    }

    #[test]
    fn stream_closes_when_only_remaining_child_is_stale_and_not_awaiting_input() {
        // Pins (B11): a stuck (stale, not awaiting input) worker must not hold the stream — and
        // its 1s poll — open forever.
        let mut progress = session_progress(
            None,
            0,
            Some(TurnOutcome {
                turn_id: "started-turn".to_string(),
                kind: TurnOutcomeKind::Completed,
                message: "done".to_string(),
            }),
        );
        let stale_stuck = WorkerProgressSummary {
            stale: true,
            ..child_summary(WorkerState::Running, Some("stuck"))
        };
        progress.child_progress = vec![stale_stuck];
        assert!(session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));

        // A stale child that is awaiting input is legitimately parked → stream stays open.
        let stale_awaiting = WorkerProgressSummary {
            stale: true,
            awaiting_input: true,
            ..child_summary(WorkerState::Running, Some("parked"))
        };
        progress.child_progress = vec![stale_awaiting];
        assert!(!session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));

        // A healthy (non-stale) running child → stream stays open.
        progress.child_progress = vec![child_summary(WorkerState::Running, Some("working"))];
        assert!(!session_message_terminal_done(
            Some("started-turn"),
            &progress
        ));
    }

    fn event_record(sequence_num: SequenceNum) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId(Uuid::nil()),
            sequence_num,
            event_type: EventType::Warning,
            event: Event::Warning {
                message: "test".to_string(),
            },
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn session_progress(
        active_turn_id: Option<String>,
        pending_message_count: u64,
        last_outcome: Option<TurnOutcome>,
    ) -> SessionProgress {
        SessionProgress {
            snapshot: SessionSnapshot {
                session_id: Uuid::nil().to_string(),
                active_turn_id,
                pending_message_count,
                last_outcome,
            },
            active_turn_progress: None,
            events: Vec::new(),
            child_progress: Vec::new(),
        }
    }

    fn child_summary(state: WorkerState, last_summary: Option<&str>) -> WorkerProgressSummary {
        WorkerProgressSummary {
            worker_id: "child-1".to_string(),
            state,
            active_turn_id: None,
            last_summary: last_summary.map(str::to_string),
            tokens_used: 0,
            budget_remaining: 1_000,
            last_heartbeat_at: None,
            stale: false,
            awaiting_input: false,
        }
    }
}

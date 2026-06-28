//! Contact session message SSE stream helpers.

use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream;
use moa_core::wire::turn::SessionProgress;
use moa_core::{
    ContactSessionMessageRequest, ContactSessionMessageResponse, ContactSessionProgressRequest,
    Event, EventRange, EventRecord, SequenceNum, SessionId, TenantId,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

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
    };
    Sse::new(stream::unfold(stream_state, next_session_message_event))
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(super) async fn initial_stream_sequence(
    state: &AppState,
    message: &ContactSessionMessageRequest,
) -> Result<SequenceNum, EdgeJsonError> {
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

struct SessionMessageStreamState {
    app: AppState,
    tenant_id: TenantId,
    session_id: SessionId,
    contact_token: String,
    next_sequence_num: SequenceNum,
    terminal_turn_id: Option<String>,
    pending_events: VecDeque<SseEvent>,
    closed: bool,
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

async fn next_session_message_event(
    mut state: SessionMessageStreamState,
) -> Option<(Result<SseEvent, Infallible>, SessionMessageStreamState)> {
    if state.closed {
        return None;
    }
    if let Some(event) = state.pending_events.pop_front() {
        return Some((Ok(event), state));
    }

    loop {
        match fetch_stream_progress(&state).await {
            Ok(progress) => {
                let done_event = session_message_stream_done(&state, &progress)
                    .then(|| done_sse_event(&state, &progress));
                enqueue_progress_events(&mut state, progress.events);
                if let Some(event) = state.pending_events.pop_front() {
                    return Some((Ok(event), state));
                }
                if let Some(event) = done_event {
                    state.closed = true;
                    return Some((Ok(event), state));
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
        return progress
            .snapshot
            .last_outcome
            .as_ref()
            .is_some_and(|outcome| outcome.turn_id == turn_id);
    }
    progress.snapshot.active_turn_id.is_none() && progress.snapshot.pending_message_count == 0
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

fn record_sse_event(record: &EventRecord) -> SseEvent {
    let event_name = match &record.event {
        Event::ProgressUpdate { .. } => "progress",
        Event::BrainResponse { .. } => "response",
        Event::ToolCall { .. } | Event::ToolResult { .. } | Event::ToolError { .. } => "tool",
        _ => "session_event",
    };
    match SseEvent::default()
        .id(record.sequence_num.to_string())
        .event(event_name)
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
    fn session_message_stream_finishes_when_started_turn_reports_outcome() {
        // Pins: a stream for an immediately-started message closes on that turn, not on later session idle.
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
    fn queued_session_message_stream_finishes_when_session_is_idle() {
        // Pins: queued messages have no accepted turn id, so their stream waits for the queue to drain.
        let running = session_progress(Some("active-turn".to_string()), 1, None);
        let idle = session_progress(None, 0, None);

        assert!(!session_message_terminal_done(None, &running));
        assert!(session_message_terminal_done(None, &idle));
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
        }
    }
}

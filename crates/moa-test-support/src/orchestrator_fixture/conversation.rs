//! Reusable multi-turn conversation driver for Restate `service_e2e` tests.
//!
//! Drives a real Session virtual object through several user turns
//! deterministically — sending each user message only after the previous
//! message has fully resolved — and then returns the full durable event log so
//! tests can inspect what actually happened. The driver reuses [`TestApiClient`]
//! and the existing `Session/start_turn`, `Session/status`, and
//! `Session/progress` handlers rather than reimplementing
//! HTTP.
//!
//! Settling follows the current lifecycle plus the active turn, pending message,
//! detached execution, and conversational worker projections. A message is
//! settled only when the lifecycle is idle or terminal and every active-work
//! projection is empty or terminal.

use super::*;
use moa_core::types::contact::ClientMessageId;
use moa_core::types::worker::state::{WorkerProgressSummary, WorkerState};
use moa_wire::turn::{SessionProgress, SessionProgressRequest};

/// `Session/progress` clamps every response to `MAX_SESSION_PROGRESS_EVENT_LIMIT`
/// events, so [`fetch_all_events`] pages forward by sequence number using this
/// size to reconstruct the full log.
const PROGRESS_PAGE_SIZE: usize = 500;

/// Options controlling how [`drive_conversation`] paces and bounds each turn.
#[derive(Debug, Clone)]
pub struct ConversationOptions {
    /// Maximum time to wait for one user message to have no active work.
    pub turn_timeout: Duration,
    /// Delay between lifecycle/progress polls while waiting for a message to settle.
    pub poll_interval: Duration,
}

impl Default for ConversationOptions {
    fn default() -> Self {
        Self {
            turn_timeout: Duration::from_secs(90),
            poll_interval: Duration::from_millis(250),
        }
    }
}

/// Drives `session_id` through `turns` one user message at a time and returns
/// the complete durable event log once every turn has settled.
///
/// Every turn is delivered through `Session/start_turn`. Because the driver
/// blocks until the session has no active work between messages, each message
/// starts a fresh turn immediately, keeping the resulting event log deterministic.
pub async fn drive_conversation(
    client: &TestApiClient,
    session_id: SessionId,
    turns: &[&str],
    opts: ConversationOptions,
) -> Result<Vec<EventRecord>> {
    for (index, message) in turns.iter().enumerate() {
        start_turn(client, session_id, message, index as u64).await?;
        await_conversation_settled(client, session_id, &opts)
            .await
            .with_context(|| format!("await settled session after conversation turn {index}"))?;
    }
    fetch_all_events(client, session_id).await
}

/// Fetches the complete durable event log for `session_id` through
/// `Session/progress`, paging forward by sequence number because the handler
/// clamps each response to at most [`PROGRESS_PAGE_SIZE`] events.
async fn fetch_all_events(
    client: &TestApiClient,
    session_id: SessionId,
) -> Result<Vec<EventRecord>> {
    let mut events: Vec<EventRecord> = Vec::new();
    let mut next_from_seq: u64 = 0;
    loop {
        let request = SessionProgressRequest {
            event_range: EventRange {
                from_seq: Some(next_from_seq),
                to_seq: None,
                event_types: None,
                limit: Some(PROGRESS_PAGE_SIZE),
            },
        };
        let progress: SessionProgress = client
            .post_call(&format!("/Session/{session_id}/progress"), &request)
            .await
            .context("fetch session progress page")?;
        let page_len = progress.events.len();
        if let Some(last) = progress.events.last() {
            next_from_seq = last.sequence_num + 1;
        }
        events.extend(progress.events);
        if page_len < PROGRESS_PAGE_SIZE {
            break;
        }
    }
    Ok(events)
}

/// Starts one conversation turn on an idle session.
///
/// `start_turn` on a `Created` session begins a turn immediately and commits
/// [`SessionStatus::Running`] before returning.
async fn start_turn(
    client: &TestApiClient,
    session_id: SessionId,
    message: &str,
    ordinal: u64,
) -> Result<()> {
    let request = start_turn_request(message, session_id, ordinal)?;
    let response = client
        .session(session_id.to_string())
        .start_turn(request, None)
        .await
        .context("start conversation turn")?;
    response
        .turn_id
        .context("start_turn on an idle session should begin a turn immediately")?;
    Ok(())
}

/// Polls the session lifecycle and active-work projections until the just-sent
/// user message has fully resolved.
async fn await_conversation_settled(
    client: &TestApiClient,
    session_id: SessionId,
    opts: &ConversationOptions,
) -> Result<SessionStatus> {
    let deadline = Instant::now() + opts.turn_timeout;
    let mut last_status = None;
    let mut last_progress = None;
    while Instant::now() < deadline {
        let status = client
            .session(session_id.to_string())
            .status()
            .await
            .context("poll session status for conversation-settled state")?;
        let progress: SessionProgress = client
            .post_call(
                &format!("/Session/{session_id}/progress"),
                &SessionProgressRequest {
                    event_range: EventRange {
                        from_seq: None,
                        to_seq: None,
                        event_types: None,
                        limit: Some(1),
                    },
                },
            )
            .await
            .context("poll session progress for conversation-settled state")?;
        if conversation_is_settled(&status, &progress) {
            return Ok(status);
        }
        last_status = Some(status);
        last_progress = Some(progress);
        tokio::time::sleep(opts.poll_interval).await;
    }

    let active_workers = last_progress
        .as_ref()
        .map(|progress| active_worker_ids(&progress.child_progress))
        .unwrap_or_default();
    bail!(
        "session {session_id} did not settle within {:?}; last status: {last_status:?}, active turn: {:?}, pending messages: {}, active executions: {}, active workers: {active_workers:?}",
        opts.turn_timeout,
        last_progress
            .as_ref()
            .and_then(|progress| progress.snapshot.active_turn_id.as_deref()),
        last_progress
            .as_ref()
            .map_or(0, |progress| progress.snapshot.pending_message_count),
        last_progress.as_ref().map_or(0, |progress| progress
            .snapshot
            .active_execution_run_uids
            .len()),
    )
}

/// Returns whether the lifecycle and every active-work projection are settled.
fn conversation_is_settled(status: &SessionStatus, progress: &SessionProgress) -> bool {
    !matches!(status, SessionStatus::Created | SessionStatus::Running)
        && progress.snapshot.active_turn_id.is_none()
        && progress.snapshot.pending_message_count == 0
        && progress.snapshot.active_execution_run_uids.is_empty()
        && progress
            .child_progress
            .iter()
            .all(|worker| worker_is_terminal(worker.state))
}

/// Returns the identifiers of workers that can still make progress.
fn active_worker_ids(workers: &[WorkerProgressSummary]) -> Vec<&str> {
    workers
        .iter()
        .filter(|worker| !worker_is_terminal(worker.state))
        .map(|worker| worker.worker_id.as_str())
        .collect()
}

/// Returns whether a worker has reached a terminal lifecycle state.
fn worker_is_terminal(state: WorkerState) -> bool {
    matches!(
        state,
        WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
    )
}

/// Builds a minimal `Session/start_turn` request carrying only the user text.
///
/// The client message id is derived from the driven session and the message's position
/// in the script, so a driver retry submits the same identity the Session admission
/// fence already recorded instead of duplicating the conversation turn.
fn start_turn_request(
    message: &str,
    session_id: SessionId,
    ordinal: u64,
) -> Result<StartTurnRequest> {
    Ok(StartTurnRequest {
        client_message_id: conversation_message_id(session_id, ordinal)?,
        reply_to: None,
        stream_cursor: None,
        user_message: message.to_string(),
        attachments: Vec::new(),
        model: None,
        contact: None,
        max_turns: None,
        resource_budget: Default::default(),
        execution_template: None,
    })
}

/// Derives the driver's deterministic client message id for one scripted turn.
fn conversation_message_id(session_id: SessionId, ordinal: u64) -> Result<ClientMessageId> {
    ClientMessageId::internal("fixture-conversation", session_id.0, ordinal)
        .context("derive conversation client message id")
}

#[cfg(test)]
mod tests {
    //! Unit coverage for conversation settling projections.

    use super::*;
    use moa_wire::turn::SessionSnapshot;

    fn progress() -> SessionProgress {
        SessionProgress {
            snapshot: SessionSnapshot {
                session_id: "session-test".to_string(),
                active_turn_id: None,
                pending_message_count: 0,
                last_outcome: None,
                active_execution_run_uids: Vec::new(),
            },
            active_turn_progress: None,
            active_execution_progress: Vec::new(),
            events: Vec::new(),
            child_progress: Vec::new(),
        }
    }

    fn worker(state: WorkerState) -> WorkerProgressSummary {
        WorkerProgressSummary {
            worker_id: format!("worker-{state:?}"),
            state,
            active_turn_id: None,
            last_summary: None,
            tokens_used: 0,
            budget_remaining: 100,
            last_heartbeat_at: None,
            stale: false,
            awaiting_input: false,
        }
    }

    #[test]
    fn conversation_settling_requires_no_active_turn_execution_or_worker() {
        // Pins: the reusable driver cannot send the next message while any current turn,
        // queued message, detached execution, or conversational worker remains active.
        let settled = progress();
        assert!(conversation_is_settled(&SessionStatus::Idle, &settled));
        assert!(!conversation_is_settled(&SessionStatus::Running, &settled));

        let mut active_turn = progress();
        active_turn.snapshot.active_turn_id = Some("turn-active".to_string());
        assert!(!conversation_is_settled(&SessionStatus::Idle, &active_turn));

        let mut pending_message = progress();
        pending_message.snapshot.pending_message_count = 1;
        assert!(!conversation_is_settled(
            &SessionStatus::Idle,
            &pending_message
        ));

        let mut active_execution = progress();
        active_execution
            .snapshot
            .active_execution_run_uids
            .push(uuid::Uuid::nil());
        assert!(!conversation_is_settled(
            &SessionStatus::Idle,
            &active_execution
        ));

        for state in [WorkerState::Uninitialized, WorkerState::Running] {
            let mut active_worker = progress();
            active_worker.child_progress.push(worker(state));
            assert!(
                !conversation_is_settled(&SessionStatus::Idle, &active_worker),
                "{state:?} worker must keep the conversation active"
            );
        }

        let mut terminal_workers = progress();
        terminal_workers.child_progress = vec![
            worker(WorkerState::Completed),
            worker(WorkerState::Failed),
            worker(WorkerState::Cancelled),
        ];
        assert!(conversation_is_settled(
            &SessionStatus::Idle,
            &terminal_workers
        ));
    }
}

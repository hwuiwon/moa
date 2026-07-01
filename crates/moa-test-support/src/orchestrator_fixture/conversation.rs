//! Reusable multi-turn conversation driver for Restate `service_e2e` tests.
//!
//! Drives a real Session virtual object through several user turns
//! deterministically — sending each user message only after the previous
//! message has fully resolved — and then returns the full durable event log so
//! coordination and cost-analysis tests can inspect what actually happened. The
//! driver reuses [`TestApiClient`] and the existing `Session/start_turn`,
//! `Session/queue_message`, `Session/status`, and `Session/progress` handlers
//! rather than reimplementing HTTP.
//!
//! Settling is keyed on the durable session **status**, not on any single
//! turn's ID. One user message can spawn several turns — an auto-delegation
//! coordinator schedule/wait turn plus a separate synthesis turn with a *new*
//! turn ID — and there are transient windows where no turn is active while
//! child workers are still running. Throughout all of that the session status
//! stays [`SessionStatus::Running`]; it flips to `Paused` (or a terminal
//! `Cancelled`/`Failed`) only once the whole user message is resolved. Because
//! `start_turn`/`queue_message` commit `Running` synchronously before
//! returning, a status poll after a send can never observe a stale `Paused`
//! from the previous turn, so status alone is an authoritative settle signal.

use super::*;
use moa_core::ConversationCost;
use moa_core::wire::turn::{
    QueueMessageRequest, QueueMessageResponse, SessionProgress, SessionProgressRequest,
};

/// `Session/progress` clamps every response to `MAX_SESSION_PROGRESS_EVENT_LIMIT`
/// events, so [`fetch_all_events`] pages forward by sequence number using this
/// size to reconstruct the full log.
const PROGRESS_PAGE_SIZE: usize = 500;

/// Options controlling how [`drive_conversation`] paces and bounds each turn.
#[derive(Debug, Clone)]
pub struct ConversationOptions {
    /// Maximum time to wait for one user message to settle (status leaves `Running`).
    pub turn_timeout: Duration,
    /// Delay between `Session/status` polls while waiting for a message to settle.
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
/// Turn `0` is delivered through `Session/start_turn`; every later turn is
/// delivered through `Session/queue_message`. Because the driver blocks until
/// the session settles (status leaves `Running`) between messages, each queued
/// message starts a fresh turn immediately instead of waiting behind an active
/// one, keeping the resulting event log deterministic for cost and coordination
/// analysis.
pub async fn drive_conversation(
    client: &TestApiClient,
    session_id: SessionId,
    turns: &[&str],
    opts: ConversationOptions,
) -> Result<Vec<EventRecord>> {
    for (index, message) in turns.iter().enumerate() {
        if index == 0 {
            start_first_turn(client, session_id, message).await?;
        } else {
            queue_followup_turn(client, session_id, message).await?;
        }
        await_conversation_settled(client, session_id, &opts)
            .await
            .with_context(|| {
                format!("await settled session status after conversation turn {index}")
            })?;
    }
    fetch_all_events(client, session_id).await
}

/// Drives `session_id` through `turns` and hands the resulting durable event
/// log to the Phase-1c cost analyzer, returning both the reconstructed
/// [`ConversationCost`] and the raw events it was derived from.
///
/// This is the integrated path used by deterministic coordination tests: drive
/// the conversation, then assert on model-side and coordination KPIs. The raw
/// events are returned alongside so callers can also assert on individual
/// [`EventRecord`]s. Coordination KPIs are only populated when the run enabled
/// `MOA_PERSIST_TURN_METRICS`; see [`ConversationCost`] for details.
pub async fn drive_conversation_cost(
    client: &TestApiClient,
    session_id: SessionId,
    turns: &[&str],
    opts: ConversationOptions,
) -> Result<(ConversationCost, Vec<EventRecord>)> {
    let events = drive_conversation(client, session_id, turns, opts).await?;
    let cost = ConversationCost::from_events(&events);
    Ok((cost, events))
}

/// Fetches the complete durable event log for `session_id` through
/// `Session/progress`, paging forward by sequence number because the handler
/// clamps each response to at most [`PROGRESS_PAGE_SIZE`] events.
pub async fn fetch_all_events(
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

/// Starts the first conversation turn on an idle session.
///
/// `start_turn` on a `Created` session begins a turn immediately and commits
/// [`SessionStatus::Running`] before returning.
async fn start_first_turn(
    client: &TestApiClient,
    session_id: SessionId,
    message: &str,
) -> Result<()> {
    let response = client
        .session(session_id.to_string())
        .start_turn(start_turn_request(message), None)
        .await
        .context("start first conversation turn")?;
    response
        .turn_id
        .context("start_turn on an idle session should begin a turn immediately")?;
    Ok(())
}

/// Queues a follow-up message on a settled session, which starts a fresh turn.
///
/// The driver only calls this once the prior message has settled (status left
/// `Running`), so `Session/queue_message` starts the turn immediately rather
/// than enqueueing it, and commits `Running` before returning.
async fn queue_followup_turn(
    client: &TestApiClient,
    session_id: SessionId,
    message: &str,
) -> Result<()> {
    let response: QueueMessageResponse = client
        .post_call(
            &format!("/Session/{session_id}/queue_message"),
            &queue_message_request(message),
        )
        .await
        .context("queue follow-up conversation turn")?;
    response.started_turn_id.context(
        "queue_message on a settled session should start a turn immediately; \
         the driver waits for the session to settle between messages",
    )?;
    Ok(())
}

/// Polls `Session/status` until the durable session status leaves `Running`,
/// i.e. the just-sent user message (including any auto-delegation coordinator,
/// worker, and synthesis turns) has fully resolved.
async fn await_conversation_settled(
    client: &TestApiClient,
    session_id: SessionId,
    opts: &ConversationOptions,
) -> Result<SessionStatus> {
    let deadline = Instant::now() + opts.turn_timeout;
    let mut last_status = None;
    while Instant::now() < deadline {
        let status = client
            .session(session_id.to_string())
            .status()
            .await
            .context("poll session status for conversation-settled state")?;
        if status_is_settled(&status) {
            return Ok(status);
        }
        last_status = Some(status);
        tokio::time::sleep(opts.poll_interval).await;
    }

    bail!(
        "session {session_id} did not settle (status still Running/Created) within {:?}; last status: {last_status:?}",
        opts.turn_timeout
    )
}

/// Returns whether `status` means the current user message has fully resolved.
///
/// `Running` is the sole in-flight state that spans every turn of a message,
/// including auto-delegation worker waits and the separate synthesis turn;
/// `Created` only precedes the first turn. Every other status — `Paused` (idle,
/// awaiting the next message) or a terminal `Completed`/`Cancelled`/`Failed` —
/// means the message settled.
fn status_is_settled(status: &SessionStatus) -> bool {
    !matches!(status, SessionStatus::Created | SessionStatus::Running)
}

/// Builds a minimal `Session/start_turn` request carrying only the user text.
fn start_turn_request(message: &str) -> StartTurnRequest {
    StartTurnRequest {
        user_message: message.to_string(),
        attachments: Vec::new(),
        model: None,
        contact: None,
        max_turns: None,
    }
}

/// Builds a minimal `Session/queue_message` request carrying only the user text.
fn queue_message_request(message: &str) -> QueueMessageRequest {
    QueueMessageRequest {
        user_message: message.to_string(),
        attachments: Vec::new(),
        model: None,
        contact: None,
        max_turns: None,
    }
}

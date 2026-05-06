//! Shared contract assertions for orchestrator lifecycle behavior.

use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    CompletionRequest, Event, EventRecord, ModelId, Platform, Result, SessionHandle, SessionId,
    SessionSignal, SessionStatus, StartSessionRequest, UserId, UserMessage, WorkspaceId,
};
use tokio::time::{Instant, sleep};
use uuid::Uuid;

const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const BLANK_SESSION_SETTLE_DELAY: Duration = Duration::from_millis(400);

/// Minimal harness API required to run the shared orchestrator contract tests.
#[async_trait]
pub trait OrchestratorContractHarness: Send + Sync {
    /// Returns a short stable name for diagnostics.
    fn harness_name(&self) -> &'static str;

    /// Returns the default model used by the harness.
    fn default_model(&self) -> ModelId;

    /// Returns the platform to use when creating contract-test sessions.
    fn platform(&self) -> Platform;

    /// Starts a session using the underlying orchestrator.
    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle>;

    /// Delivers a signal to the underlying orchestrator.
    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()>;

    /// Returns the current persisted session status when the session is visible.
    async fn session_status(&self, session_id: SessionId) -> Result<Option<SessionStatus>>;

    /// Returns the persisted event log for the session.
    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>>;

    /// Returns cloned provider requests when the harness tracks them.
    fn recorded_requests(&self) -> Option<Vec<CompletionRequest>> {
        None
    }
}

/// Verifies that a blank session stays idle until its first queued message arrives.
pub async fn assert_blank_session_waits_for_first_message<H>(
    harness: &H,
    workspace: &str,
    user: &str,
    first_message: &str,
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let session = start_session_with_timeout(harness, workspace, user, None).await?;

    sleep(BLANK_SESSION_SETTLE_DELAY).await;
    let events = harness.session_events(session.session_id).await?;
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::BrainResponse { .. })),
        "{} blank session emitted a response before the first user message.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );
    if let Some(status) = harness.session_status(session.session_id).await? {
        assert!(
            status != SessionStatus::Completed,
            "{} blank session completed before the first user message.\n{}",
            harness.harness_name(),
            diagnostic_snapshot(harness, session.session_id).await?
        );
    }
    if let Some(requests) = harness.recorded_requests() {
        assert!(
            requests.is_empty(),
            "{} issued provider requests before the first user message: {:?}",
            harness.harness_name(),
            request_user_messages(&requests)
        );
    }

    harness
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(user_message(first_message)),
        )
        .await?;

    wait_for_status(harness, session.session_id, SessionStatus::Completed).await?;
    let events = wait_for_brain_response_count(harness, session.session_id, 1).await?;
    assert_eq!(
        brain_response_texts(&events),
        vec![assistant_text(first_message)],
        "{} blank session did not respond to the first user message.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );

    Ok(())
}

/// Verifies that two sessions can make progress independently.
pub async fn assert_processes_two_sessions_independently<H>(
    harness: &H,
    left_message: &str,
    right_message: &str,
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let left = start_session_with_timeout(harness, "ws-left", "u-left", Some(left_message)).await?;
    let right =
        start_session_with_timeout(harness, "ws-right", "u-right", Some(right_message)).await?;

    wait_for_status(harness, left.session_id, SessionStatus::Completed).await?;
    wait_for_status(harness, right.session_id, SessionStatus::Completed).await?;

    let left_events = wait_for_brain_response_count(harness, left.session_id, 1).await?;
    let right_events = wait_for_brain_response_count(harness, right.session_id, 1).await?;
    assert_eq!(
        brain_response_texts(&left_events),
        vec![assistant_text(left_message)],
        "{} left session produced the wrong response.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, left.session_id).await?
    );
    assert_eq!(
        brain_response_texts(&right_events),
        vec![assistant_text(right_message)],
        "{} right session produced the wrong response.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, right.session_id).await?
    );

    Ok(())
}

/// Verifies that multiple queued messages are processed one turn at a time in FIFO order.
pub async fn assert_processes_multiple_queued_messages_fifo<H>(
    harness: &H,
    first: &str,
    queued: &[&str],
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let session = start_session_with_timeout(harness, "ws-fifo", "u-fifo", Some(first)).await?;

    sleep(Duration::from_millis(40)).await;
    for message in queued {
        harness
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(user_message(message)),
            )
            .await?;
    }

    wait_for_status(harness, session.session_id, SessionStatus::Completed).await?;
    let events =
        wait_for_brain_response_count(harness, session.session_id, queued.len() + 1).await?;
    let expected = std::iter::once(first)
        .chain(queued.iter().copied())
        .map(assistant_text)
        .collect::<Vec<_>>();
    assert_eq!(
        brain_response_texts(&events),
        expected,
        "{} did not preserve FIFO order for queued messages.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );

    if let Some(requests) = harness.recorded_requests() {
        let expected_requests = std::iter::once(first)
            .chain(queued.iter().copied())
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            request_user_messages(&requests),
            expected_requests,
            "{} issued provider requests out of order.",
            harness.harness_name()
        );
    }

    Ok(())
}

/// Verifies that a queued message buffered during approval runs after the approved turn completes.
pub async fn assert_queued_message_waiting_for_approval_runs_after_allowed_turn<H>(
    harness: &H,
    first_message: &str,
    queued_message: &str,
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let session =
        start_session_with_timeout(harness, "ws-approval", "u-approval", Some(first_message))
            .await?;

    wait_for_status(harness, session.session_id, SessionStatus::WaitingApproval).await?;
    let request_id = wait_for_approval_request(harness, session.session_id).await?;
    harness
        .signal(
            session.session_id,
            SessionSignal::QueueMessage(user_message(queued_message)),
        )
        .await?;
    harness
        .signal(
            session.session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: moa_core::ApprovalDecision::AllowOnce,
            },
        )
        .await?;

    wait_for_status(harness, session.session_id, SessionStatus::Completed).await?;
    let events = wait_for_brain_response_count(harness, session.session_id, 2).await?;
    assert!(
        events.iter().any(|record| matches!(
            &record.event,
            Event::QueuedMessage { text, .. } if text == queued_message
        )),
        "{} did not persist the queued follow-up message.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );
    assert_eq!(
        brain_response_texts(&events),
        vec![
            assistant_text(first_message),
            assistant_text(queued_message)
        ],
        "{} approval flow did not resume with the queued follow-up.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );

    if let Some(requests) = harness.recorded_requests() {
        assert_eq!(
            request_user_messages(&requests),
            vec![first_message.to_string(), queued_message.to_string()],
            "{} issued provider requests out of order across approval resume.",
            harness.harness_name()
        );
    }

    Ok(())
}

/// Verifies that a soft cancel issued while waiting for approval cancels the
/// session without recording an approval decision or tool result.
pub async fn assert_soft_cancel_waiting_for_approval_cancels_cleanly<H>(
    harness: &H,
    first_message: &str,
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let session =
        start_session_with_timeout(harness, "ws-cancel", "u-cancel", Some(first_message)).await?;

    wait_for_status(harness, session.session_id, SessionStatus::WaitingApproval).await?;
    harness
        .signal(session.session_id, SessionSignal::SoftCancel)
        .await?;
    wait_for_status(harness, session.session_id, SessionStatus::Cancelled).await?;

    let events = wait_for_status_transition(
        harness,
        session.session_id,
        SessionStatus::WaitingApproval,
        SessionStatus::Cancelled,
    )
    .await?;
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::ApprovalDecided { .. })),
        "{} persisted an approval decision after a soft cancel.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );
    assert!(
        !events
            .iter()
            .any(|record| matches!(record.event, Event::ToolResult { .. })),
        "{} persisted a tool result after cancelling while approval was pending.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );

    let transitions = status_transitions(&events);
    assert!(
        transitions.contains(&(SessionStatus::Running, SessionStatus::WaitingApproval)),
        "{} never recorded Running -> WaitingApproval.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );
    assert!(
        transitions.contains(&(SessionStatus::WaitingApproval, SessionStatus::Cancelled)),
        "{} never recorded WaitingApproval -> Cancelled.\n{}",
        harness.harness_name(),
        diagnostic_snapshot(harness, session.session_id).await?
    );

    Ok(())
}

/// Collects all visible assistant response texts from a session event stream.
pub fn brain_response_texts(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn start_request<H>(
    harness: &H,
    workspace: &str,
    user: &str,
    initial_message: Option<&str>,
) -> StartSessionRequest
where
    H: OrchestratorContractHarness,
{
    StartSessionRequest {
        workspace_id: WorkspaceId::new(workspace),
        user_id: UserId::new(user),
        platform: harness.platform(),
        model: harness.default_model(),
        initial_message: initial_message.map(user_message),
        title: None,
        parent_session_id: None,
    }
}

fn user_message(text: &str) -> UserMessage {
    UserMessage {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}

fn assistant_text(prompt: &str) -> String {
    format!("assistant:{prompt}")
}

fn request_user_messages(requests: &[CompletionRequest]) -> Vec<String> {
    requests
        .iter()
        .filter_map(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|message| {
                    matches!(message.role, moa_core::MessageRole::User)
                        && !message.content.starts_with("<system-reminder>")
                        && !message.content.starts_with("<memory-reminder>")
                })
                .or_else(|| {
                    request
                        .messages
                        .iter()
                        .rev()
                        .find(|message| matches!(message.role, moa_core::MessageRole::User))
                })
                .map(|message| message.content.clone())
        })
        .collect()
}

async fn start_session_with_timeout<H>(
    harness: &H,
    workspace: &str,
    user: &str,
    initial_message: Option<&str>,
) -> Result<SessionHandle>
where
    H: OrchestratorContractHarness,
{
    match tokio::time::timeout(
        DEFAULT_WAIT_TIMEOUT,
        harness.start_session(start_request(harness, workspace, user, initial_message)),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => panic!(
            "{} timed out starting a session for workspace={workspace:?} user={user:?} initial_message={initial_message:?}",
            harness.harness_name()
        ),
    }
}

async fn wait_for_status<H>(
    harness: &H,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<()>
where
    H: OrchestratorContractHarness,
{
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        if let Some(status) = harness.session_status(session_id).await?
            && status == expected
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            panic!(
                "{} timed out waiting for status {expected:?}.\n{}",
                harness.harness_name(),
                diagnostic_snapshot(harness, session_id)
                    .await
                    .expect("diagnostic snapshot")
            );
        }

        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_status_transition<H>(
    harness: &H,
    session_id: SessionId,
    from: SessionStatus,
    to: SessionStatus,
) -> Result<Vec<EventRecord>>
where
    H: OrchestratorContractHarness,
{
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        if status_transitions(&events).contains(&(from.clone(), to.clone())) {
            return Ok(events);
        }

        if Instant::now() >= deadline {
            panic!(
                "{} timed out waiting for status transition {from:?} -> {to:?}.\n{}",
                harness.harness_name(),
                diagnostic_snapshot(harness, session_id)
                    .await
                    .expect("diagnostic snapshot")
            );
        }

        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_approval_request<H>(harness: &H, session_id: SessionId) -> Result<Uuid>
where
    H: OrchestratorContractHarness,
{
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        if let Some(request_id) = events.iter().find_map(|record| match record.event {
            Event::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        }) {
            return Ok(request_id);
        }

        if Instant::now() >= deadline {
            panic!(
                "{} timed out waiting for an approval request.\n{}",
                harness.harness_name(),
                diagnostic_snapshot(harness, session_id)
                    .await
                    .expect("diagnostic snapshot")
            );
        }

        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_brain_response_count<H>(
    harness: &H,
    session_id: SessionId,
    expected: usize,
) -> Result<Vec<EventRecord>>
where
    H: OrchestratorContractHarness,
{
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        let count = events
            .iter()
            .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
            .count();
        if count >= expected {
            return Ok(events);
        }

        if Instant::now() >= deadline {
            panic!(
                "{} timed out waiting for {expected} brain responses.\n{}",
                harness.harness_name(),
                diagnostic_snapshot(harness, session_id)
                    .await
                    .expect("diagnostic snapshot")
            );
        }

        sleep(Duration::from_millis(50)).await;
    }
}

async fn diagnostic_snapshot<H>(harness: &H, session_id: SessionId) -> Result<String>
where
    H: OrchestratorContractHarness,
{
    let status = harness.session_status(session_id).await?;
    let events = harness.session_events(session_id).await?;
    let mut tail = events
        .iter()
        .rev()
        .take(12)
        .map(event_label)
        .collect::<Vec<_>>();
    tail.reverse();
    Ok(format!(
        "session_id={session_id}\nstatus={status:?}\nevent_count={}\nevent_tail={tail:#?}",
        events.len()
    ))
}

fn event_label(record: &EventRecord) -> String {
    match &record.event {
        Event::SessionCreated { .. } => format!("{}: SessionCreated", record.sequence_num),
        Event::SessionStatusChanged { from, to } => {
            format!(
                "{}: SessionStatusChanged({from:?}->{to:?})",
                record.sequence_num
            )
        }
        Event::UserMessage { text, .. } => {
            format!("{}: UserMessage({text:?})", record.sequence_num)
        }
        Event::QueuedMessage { text, .. } => {
            format!("{}: QueuedMessage({text:?})", record.sequence_num)
        }
        Event::BrainResponse { text, .. } => {
            format!("{}: BrainResponse({text:?})", record.sequence_num)
        }
        Event::ToolCall { tool_name, .. } => {
            format!("{}: ToolCall({tool_name})", record.sequence_num)
        }
        Event::ToolResult { success, .. } => {
            format!("{}: ToolResult(success={success})", record.sequence_num)
        }
        Event::ToolError { error, .. } => {
            format!("{}: ToolError({error:?})", record.sequence_num)
        }
        Event::ApprovalRequested {
            request_id,
            tool_name,
            ..
        } => format!(
            "{}: ApprovalRequested({}, {})",
            record.sequence_num, tool_name, request_id
        ),
        Event::ApprovalDecided {
            request_id,
            decision,
            ..
        } => format!(
            "{}: ApprovalDecided({request_id}, {decision:?})",
            record.sequence_num
        ),
        Event::Warning { message } => format!("{}: Warning({message:?})", record.sequence_num),
        Event::Error { message, .. } => format!("{}: Error({message:?})", record.sequence_num),
        other => format!("{}: {}", record.sequence_num, other.type_name()),
    }
}

fn status_transitions(events: &[EventRecord]) -> Vec<(SessionStatus, SessionStatus)> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::SessionStatusChanged { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        })
        .collect()
}

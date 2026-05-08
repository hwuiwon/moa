//! Session-scoped client helpers for the orchestrator wire surface.

use std::time::{Duration, Instant};

use moa_core::{ApprovalDecision, CancelMode};

use crate::client::OrchestratorClient;
use crate::error::{Error, Result};
use crate::types::{
    CancelResponse, QueueMessageRequest, QueueMessageResponse, SessionSnapshot, StartTurnRequest,
    StartTurnResponse, TurnOutcome,
};

/// Client handle scoped to one `Session` virtual object key.
pub struct SessionHandle<'a> {
    pub(crate) client: &'a OrchestratorClient,
    pub(crate) session_id: String,
}

impl SessionHandle<'_> {
    /// Returns the Session virtual object key.
    pub fn id(&self) -> &str {
        &self.session_id
    }

    /// Starts a new turn and returns immediately with its assigned turn ID.
    pub async fn start_turn(
        &self,
        request: StartTurnRequest,
        idempotency_key: Option<&str>,
    ) -> Result<StartTurnResponse> {
        let path = format!("/Session/{}/start_turn", self.session_id);
        self.client
            .post_call_with_idempotency(&path, &request, idempotency_key)
            .await
    }

    /// Queues a message or starts a turn immediately when no turn is active.
    pub async fn queue_message(
        &self,
        request: QueueMessageRequest,
        idempotency_key: Option<&str>,
    ) -> Result<QueueMessageResponse> {
        let path = format!("/Session/{}/queue_message", self.session_id);
        self.client
            .post_call_with_idempotency(&path, &request, idempotency_key)
            .await
    }

    /// Forwards cancellation to the active `TurnExecution` workflow.
    pub async fn request_cancel(&self, reason: impl Into<String>) -> Result<CancelResponse> {
        let path = format!("/Session/{}/request_cancel", self.session_id);
        self.client.post_call(&path, &reason.into()).await
    }

    /// Applies a cooperative cancellation mode to the Session virtual object.
    pub async fn cancel(&self, mode: CancelMode) -> Result<()> {
        let path = format!("/Session/{}/cancel", self.session_id);
        self.client.post_void(&path, &mode).await
    }

    /// Resolves the currently pending approval for the Session virtual object.
    pub async fn approve(&self, decision: ApprovalDecision) -> Result<()> {
        let path = format!("/Session/{}/approve", self.session_id);
        self.client.post_void(&path, &decision).await
    }

    /// Reads a non-blocking snapshot of the Session virtual object's turn state.
    pub async fn snapshot(&self) -> Result<SessionSnapshot> {
        let path = format!("/Session/{}/snapshot", self.session_id);
        self.client.post_call(&path, &serde_json::Value::Null).await
    }

    /// Returns a polling-based event subscription placeholder.
    ///
    /// The orchestrator does not expose a Session SSE stream yet, so this
    /// helper yields snapshots at the requested interval until a later wire
    /// surface can replace it with server-sent events.
    pub fn subscribe_events(&self, poll_interval: Duration) -> SnapshotPoller<'_> {
        SnapshotPoller {
            client: self.client,
            session_id: self.session_id.clone(),
            poll_interval,
        }
    }

    /// Polls snapshots until the requested turn's terminal outcome is visible.
    pub async fn await_turn_outcome(
        &self,
        turn_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<TurnOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot().await?;
            if let Some(outcome) = snapshot.last_outcome
                && outcome.turn_id == turn_id
            {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(timeout));
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

/// Polling snapshot stream used until the orchestrator exposes SSE events.
pub struct SnapshotPoller<'a> {
    client: &'a OrchestratorClient,
    session_id: String,
    poll_interval: Duration,
}

impl SnapshotPoller<'_> {
    /// Sleeps for the configured interval, then returns the latest snapshot.
    pub async fn next_snapshot(&self) -> Result<SessionSnapshot> {
        tokio::time::sleep(self.poll_interval).await;
        self.client
            .session(self.session_id.clone())
            .snapshot()
            .await
    }
}

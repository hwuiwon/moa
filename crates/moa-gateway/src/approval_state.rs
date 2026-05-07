//! Approval request lifecycle tracking for gateway button interactions.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::{ApprovalDecision, ApprovalRequest, MessageContent, OutboundMessage, SessionSignal};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::approval::ApprovalCallbackAction;

/// Current gateway-visible approval lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalLifecycleState {
    /// The approval is waiting for a user decision.
    Pending {
        /// Expiration timestamp for this approval.
        expires_at: DateTime<Utc>,
    },
    /// The approval was decided.
    Decided {
        /// User decision.
        decision: ApprovalDecision,
        /// Platform user that clicked the button.
        actor: String,
        /// Decision timestamp.
        decided_at: DateTime<Utc>,
    },
    /// The approval timed out before a decision was received.
    Expired {
        /// Expiration timestamp.
        expired_at: DateTime<Utc>,
    },
}

/// Result of processing one approval button click.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalClickOutcome {
    /// Orchestrator signal emitted by a winning click.
    pub signal: Option<SessionSignal>,
    /// User-visible acknowledgement for the clicker.
    pub acknowledgement: OutboundMessage,
    /// State after this click was processed.
    pub state: ApprovalLifecycleState,
}

#[derive(Debug, Clone)]
struct ApprovalRecord {
    request: ApprovalRequest,
    state: ApprovalLifecycleState,
}

/// In-memory approval state machine used by gateway adapters.
#[derive(Debug, Clone, Default)]
pub struct ApprovalStateTracker {
    records: Arc<Mutex<HashMap<Uuid, ApprovalRecord>>>,
}

impl ApprovalStateTracker {
    /// Creates an empty approval state tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a pending approval request.
    pub async fn insert_pending(&self, request: ApprovalRequest, expires_at: DateTime<Utc>) {
        self.records.lock().await.insert(
            request.request_id,
            ApprovalRecord {
                request,
                state: ApprovalLifecycleState::Pending { expires_at },
            },
        );
    }

    /// Returns the current state for a request id.
    pub async fn state(&self, request_id: Uuid) -> Option<ApprovalLifecycleState> {
        self.records
            .lock()
            .await
            .get(&request_id)
            .map(|record| record.state.clone())
    }

    /// Handles a button callback, emitting at most one approval decision signal.
    pub async fn handle_callback(
        &self,
        callback_data: &str,
        actor: &str,
        now: DateTime<Utc>,
    ) -> ApprovalClickOutcome {
        let Some((request_id, decision)) = parse_approval_decision(callback_data) else {
            return stale_outcome("This approval has expired or already been decided.");
        };

        let mut records = self.records.lock().await;
        let Some(record) = records.get_mut(&request_id) else {
            return stale_outcome("This approval has expired or already been decided.");
        };

        match &record.state {
            ApprovalLifecycleState::Pending { expires_at } if *expires_at <= now => {
                record.state = ApprovalLifecycleState::Expired {
                    expired_at: *expires_at,
                };
                ApprovalClickOutcome {
                    signal: None,
                    acknowledgement: text_ack("This approval has expired."),
                    state: record.state.clone(),
                }
            }
            ApprovalLifecycleState::Pending { .. } => {
                record.state = ApprovalLifecycleState::Decided {
                    decision: decision.clone(),
                    actor: actor.to_string(),
                    decided_at: now,
                };
                ApprovalClickOutcome {
                    signal: Some(SessionSignal::ApprovalDecided {
                        request_id: record.request.request_id,
                        decision,
                    }),
                    acknowledgement: text_ack("Approval recorded."),
                    state: record.state.clone(),
                }
            }
            ApprovalLifecycleState::Decided { .. } | ApprovalLifecycleState::Expired { .. } => {
                ApprovalClickOutcome {
                    signal: None,
                    acknowledgement: text_ack("This approval has expired or already been decided."),
                    state: record.state.clone(),
                }
            }
        }
    }
}

/// Builds the state marker text rendered near approval controls.
pub fn approval_state_marker(state: &ApprovalLifecycleState) -> String {
    match state {
        ApprovalLifecycleState::Pending { .. } => "Pending".to_string(),
        ApprovalLifecycleState::Decided {
            decision,
            actor,
            decided_at,
        } => match decision {
            ApprovalDecision::AllowOnce => {
                format!("✓ Allowed by {actor} at {}", decided_at.format("%H:%M"))
            }
            ApprovalDecision::AlwaysAllow { .. } => {
                format!(
                    "✓ Always allowed by {actor} at {}",
                    decided_at.format("%H:%M")
                )
            }
            ApprovalDecision::Deny { .. } => {
                format!("✕ Denied by {actor} at {}", decided_at.format("%H:%M"))
            }
        },
        ApprovalLifecycleState::Expired { .. } => "Expired".to_string(),
    }
}

/// Parses compact or verbose approval callback payloads into decisions.
pub fn parse_approval_decision(callback_data: &str) -> Option<(Uuid, ApprovalDecision)> {
    if let Some(action) = ApprovalCallbackAction::decode(callback_data) {
        return match action {
            ApprovalCallbackAction::AllowOnce { request_id } => {
                Some((request_id, ApprovalDecision::AllowOnce))
            }
            ApprovalCallbackAction::AlwaysAllow { request_id } => Some((
                request_id,
                ApprovalDecision::AlwaysAllow {
                    pattern: String::new(),
                },
            )),
            ApprovalCallbackAction::Deny { request_id } => {
                Some((request_id, ApprovalDecision::Deny { reason: None }))
            }
        };
    }

    let mut parts = callback_data.split(':');
    let prefix = parts.next()?;
    let request_id = Uuid::parse_str(parts.next()?).ok()?;
    let decision = parts.next()?;
    if prefix != "approve" || parts.next().is_some() {
        return None;
    }

    let decision = match decision {
        "allow_once" => ApprovalDecision::AllowOnce,
        "always_allow" => ApprovalDecision::AlwaysAllow {
            pattern: String::new(),
        },
        "deny" => ApprovalDecision::Deny { reason: None },
        _ => return None,
    };
    Some((request_id, decision))
}

fn stale_outcome(message: &str) -> ApprovalClickOutcome {
    ApprovalClickOutcome {
        signal: None,
        acknowledgement: text_ack(message),
        state: ApprovalLifecycleState::Expired {
            expired_at: Utc::now(),
        },
    }
}

fn text_ack(text: &str) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::Text(text.to_string()),
        buttons: Vec::new(),
        reply_to: None,
        ephemeral: true,
    }
}

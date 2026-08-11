//! Worker terminal-result waiter and parent-delivery DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::identifiers::AgentSignalId;
use super::state::{WorkerId, WorkerTerminalResult};

/// Input for registering an awakeable that should resolve when a child terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachWorkerResultWaiterInput {
    /// Awakeable id owned by the waiting workflow.
    pub awakeable_id: String,
}

/// Output returned when registering a terminal result waiter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachWorkerResultWaiterOutput {
    /// Already available terminal result, if the child had finished before registration.
    pub terminal: Option<WorkerTerminalResult>,
}

/// Input for removing a terminal result waiter after timeout or cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveWorkerResultWaiterInput {
    /// Awakeable id that should no longer be resolved by the child.
    pub awakeable_id: String,
}

/// Input for durably recording a child's terminal transition on its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordWorkerChildTerminalInput {
    /// Child worker id.
    pub worker_id: WorkerId,
    /// Worker admission generation that produced the terminal result.
    pub generation: u64,
    /// Terminal state and result to record and cache.
    pub terminal: WorkerTerminalResult,
    /// Replay-stable identifier journaled by the worker for parent-side deduplication.
    pub signal_id: AgentSignalId,
    /// Replay-stable terminal timestamp journaled by the worker.
    pub created_at: DateTime<Utc>,
}

/// Input for consuming a cached child result from a parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeWorkerChildResultInput {
    /// Child worker id.
    pub worker_id: WorkerId,
}

/// Output returned when consuming a cached child result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumeWorkerChildResultOutput {
    /// Terminal result, if one was cached and consumed.
    pub terminal: Option<WorkerTerminalResult>,
}

#[cfg(test)]
mod contract_tests {
    use chrono::{TimeZone, Utc};

    use super::RecordWorkerChildTerminalInput;
    use crate::types::{
        identifiers::AgentSignalId,
        worker::state::{WorkerResult, WorkerState, WorkerTerminalResult},
    };

    #[test]
    fn record_worker_child_terminal_input_round_trips_every_durable_coordinate() {
        // Pins: terminal delivery names one worker admission and carries the
        // worker-journaled signal identity and timestamp across the wire.
        let input = RecordWorkerChildTerminalInput {
            worker_id: "worker-contract-1".to_string(),
            generation: 7,
            terminal: WorkerTerminalResult {
                state: WorkerState::Completed,
                result: WorkerResult {
                    worker_id: "worker-contract-1".to_string(),
                    success: true,
                    output: "done".to_string(),
                    tokens_used: 42,
                    tools_invoked: 2,
                    error: None,
                },
            },
            signal_id: AgentSignalId(uuid::Uuid::from_u128(0x51_9a1)),
            created_at: Utc
                .with_ymd_and_hms(2026, 8, 10, 14, 30, 0)
                .single()
                .expect("fixture timestamp should be unambiguous"),
        };

        let encoded = serde_json::to_value(&input).expect("serialize terminal input");
        assert_eq!(encoded["worker_id"], "worker-contract-1");
        assert_eq!(encoded["generation"], 7);
        assert_eq!(encoded["terminal"]["state"], "completed");
        assert_eq!(encoded["signal_id"], "00000000-0000-0000-0000-0000000519a1");
        assert_eq!(encoded["created_at"], "2026-08-10T14:30:00Z");
        assert_eq!(
            serde_json::from_value::<RecordWorkerChildTerminalInput>(encoded)
                .expect("deserialize terminal input"),
            input
        );
    }

    #[test]
    fn record_worker_child_terminal_input_rejects_missing_or_unknown_coordinates() {
        // Pins: the hard-cutover terminal protocol cannot silently accept an
        // old payload that omits its generation or journaled delivery identity.
        let old_payload = serde_json::json!({
            "worker_id": "worker-contract-1",
            "terminal": {
                "state": "completed",
                "result": {
                    "worker_id": "worker-contract-1",
                    "success": true,
                    "output": "done",
                    "tokens_used": 42,
                    "tools_invoked": 2,
                    "error": null
                }
            }
        });
        assert!(serde_json::from_value::<RecordWorkerChildTerminalInput>(old_payload).is_err());

        let unknown_field_payload = serde_json::json!({
            "worker_id": "worker-contract-1",
            "generation": 7,
            "terminal": {
                "state": "completed",
                "result": {
                    "worker_id": "worker-contract-1",
                    "success": true,
                    "output": "done",
                    "tokens_used": 42,
                    "tools_invoked": 2,
                    "error": null
                }
            },
            "signal_id": "00000000-0000-0000-0000-0000000519a1",
            "created_at": "2026-08-10T14:30:00Z",
            "notification_id": "retired"
        });
        assert!(
            serde_json::from_value::<RecordWorkerChildTerminalInput>(unknown_field_payload)
                .is_err()
        );
    }
}

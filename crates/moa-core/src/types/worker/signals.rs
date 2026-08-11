//! Child-to-parent worker attention signals and resume policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::super::identifiers::{AgentSignalId, SessionId};
use super::state::{WorkerId, WorkerInputRequest};

/// Attention-requiring child-to-parent signal kind.
///
/// Excludes high-frequency telemetry (progress/heartbeat); these are the only
/// kinds routed to the owning coordinator on the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildSignalKind {
    /// The child surfaced a noteworthy intermediate finding.
    Finding,
    /// The child is blocked and cannot make progress without intervention.
    Blocked,
    /// The child needs input before it can continue.
    NeedsInput,
    /// The child failed terminally and is reporting the failure.
    Failed,
    /// The child's heartbeat went stale (raised by the watchdog).
    HeartbeatStale,
    /// The current child fan-in generation settled while the coordinator was idle.
    FanInSettled,
}

/// Whether a signal may wake an idle coordinator. Conservative by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentResumePolicy {
    /// Never wake the coordinator; the signal waits for the next user turn.
    Never,
    /// Wake the coordinator only when it is currently idle.
    IfIdle,
}

/// Relative urgency of one control-plane signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSeverity {
    /// Informational; no action implied.
    Info,
    /// Warrants attention but is not terminal.
    Warning,
    /// Critical condition requiring prompt coordinator attention.
    Critical,
}

/// Narrow child-to-parent attention signal routed to the owning coordinator.
///
/// Idempotent at the event log via a dedupe key derived from `signal_id`. This is
/// the control plane: low-frequency, model-driven attention events (not per-tick
/// telemetry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerSignal {
    /// Stable identifier for this attention signal.
    pub signal_id: AgentSignalId,
    /// Child worker that raised the signal.
    pub worker_id: WorkerId,
    /// Owning root session coordinator that should receive the signal.
    pub parent_session: SessionId,
    /// Kind of attention being requested.
    pub kind: ChildSignalKind,
    /// Relative urgency of the signal.
    pub severity: SignalSeverity,
    /// Short, safe human-readable summary of the signal.
    pub summary: String,
    /// Structured payload carrying signal-specific detail.
    #[serde(default)]
    pub payload: serde_json::Value,
    /// When the signal was created (Restate-journaled at the child).
    pub created_at: DateTime<Utc>,
    /// Whether this signal may wake an idle coordinator.
    pub resume_policy: ParentResumePolicy,
    /// Exact in-flight input request; `Some` only for `NeedsInput`.
    ///
    /// One field rather than parallel optionals: the coordinates and the audience
    /// are meaningless apart, and the coordinator session advertises the reply
    /// target from exactly these coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request: Option<WorkerInputRequest>,
}

/// Compact, persisted projection of one unread child→parent control-plane signal.
///
/// Stored on the owning coordinator `Session` VO so a later resume/drain turn can
/// surface the signal's content without re-reading the event log. Carries content
/// rather than only ids, and is capped to a small recent window on the VO so it
/// never bloats parent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadChildSignal {
    /// Stable identifier of the recorded signal.
    pub signal_id: AgentSignalId,
    /// Child worker that raised the signal.
    pub worker_id: WorkerId,
    /// Kind of attention requested.
    pub kind: ChildSignalKind,
    /// Short, safe human-readable summary carried for the resume/drain turn.
    pub summary: String,
    /// Exact in-flight input request; `Some` only for `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request: Option<WorkerInputRequest>,
}

#[cfg(test)]
mod contract_tests {
    use super::ChildSignalKind;

    #[test]
    fn fan_in_settled_has_one_exact_wire_value() {
        // Pins: successful child fan-in has a first-class control-plane signal;
        // it cannot be encoded as a finding or accepted under a legacy spelling.
        let encoded = serde_json::to_string(&ChildSignalKind::FanInSettled)
            .expect("serialize fan-in settled signal kind");

        assert_eq!(encoded, "\"fan_in_settled\"");
        assert_eq!(
            serde_json::from_str::<ChildSignalKind>(&encoded)
                .expect("deserialize fan-in settled signal kind"),
            ChildSignalKind::FanInSettled
        );
        assert!(serde_json::from_str::<ChildSignalKind>("\"fan_in_complete\"").is_err());
    }
}

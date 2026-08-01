//! Typed terminal evidence produced by a Behavior Lab trial target.
//!
//! Both target paths — the agent-loop session and the pinned execution-template
//! run — reduce to one of these values before any score is derived. The
//! evidence carries the token and cost observations that were previously only
//! emitted as telemetry, so a deterministic budget evaluator has something
//! durable to read.

use moa_core::types::identifiers::SessionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::ExperimentTrialStopReason;
use crate::simulator_policy::{registry::SimulatorPolicyBinding, runtime::SimulatorDecision};

/// Namespace for evidence-reference hashing.
const EVIDENCE_HASH_DOMAIN: &str = "moa.experiment.trial-evidence";

/// How a trial's target stopped, classified for evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialTerminalOutcome {
    /// The target reached a terminal state without a provider or runtime error.
    Completed,
    /// A model provider call failed.
    ProviderFailure,
    /// The orchestration runtime or the target itself failed.
    RuntimeFailure,
    /// The trial was cancelled before the target reached a terminal state.
    Cancelled,
}

impl TrialTerminalOutcome {
    /// Returns true when the target finished without a provider or runtime error.
    #[must_use]
    pub const fn is_clean_completion(self) -> bool {
        matches!(self, Self::Completed)
    }

    /// Returns the persisted representation for this outcome.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ProviderFailure => "provider_failure",
            Self::RuntimeFailure => "runtime_failure",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The exact target a trial's scores attach to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrialScoreTarget {
    /// The trial drove an agent-loop session.
    Session {
        /// Exact target session.
        session_id: SessionId,
    },
    /// The trial drove a typed execution run.
    ExecutionRun {
        /// Exact target execution run.
        execution_run_uid: Uuid,
    },
}

impl TrialScoreTarget {
    /// Returns the target session when this trial drove one.
    #[must_use]
    pub const fn session_id(self) -> Option<SessionId> {
        match self {
            Self::Session { session_id } => Some(session_id),
            Self::ExecutionRun { .. } => None,
        }
    }

    /// Returns the target execution run when this trial drove one.
    #[must_use]
    pub const fn execution_run_uid(self) -> Option<Uuid> {
        match self {
            Self::ExecutionRun { execution_run_uid } => Some(execution_run_uid),
            Self::Session { .. } => None,
        }
    }

    /// Returns the stable identity fragment used in score-id derivation.
    #[must_use]
    pub fn identity_fragment(self) -> String {
        match self {
            Self::Session { session_id } => format!("session:{session_id}"),
            Self::ExecutionRun { execution_run_uid } => {
                format!("execution_run:{execution_run_uid}")
            }
        }
    }
}

/// Terminal observations one trial produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialTerminalEvidence {
    /// The exact target the trial ran against.
    pub target: TrialScoreTarget,
    /// Session the trial ran in.
    ///
    /// Both target shapes have one: an agent-loop trial drives it directly, and
    /// an execution-template trial runs its typed execution inside one. The
    /// score row names this session; provenance names the exact target.
    pub session_id: SessionId,
    /// Classified terminal outcome.
    pub outcome: TrialTerminalOutcome,
    /// Durable stop reason persisted on the trial row.
    pub stop_reason: ExperimentTrialStopReason,
    /// Simulator-target turns the trial actually drove.
    pub turn_count: u32,
    /// Total target-side tokens observed across the trial.
    pub total_tokens: u64,
    /// Total target-side cost in cents observed across the trial.
    pub total_cost_cents: u64,
    /// Latest durable event-log sequence covered by this evidence.
    pub latest_sequence_num: u64,
    /// The last user-visible output the target produced, when it produced one.
    ///
    /// This is the text the privacy evaluator classifies. It is never persisted
    /// into the provenance table; only its digest enters the evidence hash.
    pub visible_output: Option<String>,
    /// Stable failure code when the target failed.
    pub failure_code: Option<String>,
    /// Certified simulator identity used by agent-loop trials.
    pub simulator_policy: Option<SimulatorPolicyBinding>,
    /// Last structured simulator decision, when a simulator ran.
    pub simulator_decision: Option<SimulatorDecision>,
    /// Last bounded simulator decision reason, when a simulator ran.
    pub simulator_reason: Option<String>,
}

impl TrialTerminalEvidence {
    /// Returns true when the target produced a non-empty visible result.
    #[must_use]
    pub fn produced_result(&self) -> bool {
        self.visible_output
            .as_deref()
            .is_some_and(|output| !output.trim().is_empty())
    }

    /// Returns a bounded, privacy-safe reference describing this evidence.
    ///
    /// The reference names where the evidence lives rather than reproducing it,
    /// so provenance rows stay bounded and carry no target output text.
    #[must_use]
    pub fn reference(&self) -> String {
        format!(
            "{}#seq={}&turns={}&outcome={}",
            self.target.identity_fragment(),
            self.latest_sequence_num,
            self.turn_count,
            self.outcome.as_str()
        )
    }

    /// Returns the BLAKE3 digest binding this evidence to its scores.
    ///
    /// The visible output enters as its own digest rather than as text, so the
    /// hash distinguishes different target outputs without storing any.
    #[must_use]
    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(EVIDENCE_HASH_DOMAIN.as_bytes());
        hasher.update(self.reference().as_bytes());
        hasher.update(self.stop_reason.as_str().as_bytes());
        hasher.update(&self.total_tokens.to_be_bytes());
        hasher.update(&self.total_cost_cents.to_be_bytes());
        match self.visible_output.as_deref() {
            Some(output) => {
                hasher.update(b"\x01");
                hasher.update(blake3::hash(output.as_bytes()).as_bytes());
            }
            None => {
                hasher.update(b"\x00");
            }
        }
        match self.failure_code.as_deref() {
            Some(code) => {
                hasher.update(b"\x01");
                hasher.update(code.as_bytes());
            }
            None => {
                hasher.update(b"\x00");
            }
        }
        match self.simulator_policy {
            Some(binding) => {
                hasher.update(b"\x01");
                hasher.update(binding.policy_uid.as_bytes());
                hasher.update(&binding.revision.to_be_bytes());
                hasher.update(&binding.policy_hash.0);
                hasher.update(binding.study_uid.as_bytes());
                hasher.update(&binding.study_artifact_hash.0);
                hasher.update(&binding.evaluator_version.to_be_bytes());
                hasher.update(&binding.certified_until.timestamp_millis().to_be_bytes());
            }
            None => {
                hasher.update(b"\x00");
            }
        }
        match self.simulator_decision {
            Some(decision) => {
                hasher.update(b"\x01");
                hasher.update(decision.as_str().as_bytes());
            }
            None => {
                hasher.update(b"\x00");
            }
        }
        match self.simulator_reason.as_deref() {
            Some(reason) => {
                hasher.update(b"\x01");
                hasher.update(blake3::hash(reason.as_bytes()).as_bytes());
            }
            None => {
                hasher.update(b"\x00");
            }
        }
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> TrialTerminalEvidence {
        TrialTerminalEvidence {
            target: TrialScoreTarget::Session {
                session_id: SessionId(Uuid::from_u128(7)),
            },
            session_id: SessionId(Uuid::from_u128(7)),
            outcome: TrialTerminalOutcome::Completed,
            stop_reason: ExperimentTrialStopReason::SimulatorDone,
            turn_count: 3,
            total_tokens: 1200,
            total_cost_cents: 4,
            latest_sequence_num: 42,
            visible_output: Some("the order shipped on tuesday".to_string()),
            failure_code: None,
            simulator_policy: None,
            simulator_decision: None,
            simulator_reason: None,
        }
    }

    #[test]
    fn evidence_reference_is_bounded_and_carries_no_target_output_offline() {
        // Pins: the provenance reference names where evidence lives instead of
        // reproducing it, so a long or sensitive target response cannot leak into
        // the provenance row through the reference field.
        let mut long = evidence();
        long.visible_output = Some("x".repeat(100_000));

        let reference = long.reference();

        assert_eq!(
            reference,
            "session:00000000-0000-0000-0000-000000000007#seq=42&turns=3&outcome=completed"
        );
        assert!(!reference.contains('x'), "reference leaked target output");
    }

    #[test]
    fn evidence_hash_distinguishes_different_target_output_offline() {
        // Pins: evidence hash is identity. Two trials that differ only in what the
        // target said must not share an evidence hash, or replay acceptance would
        // accept one trial's scores as another's.
        let first = evidence();
        let mut second = evidence();
        second.visible_output = Some("the order was cancelled".to_string());

        assert_eq!(first.hash(), evidence().hash(), "hash must be stable");
        assert_ne!(first.hash(), second.hash());
    }

    #[test]
    fn evidence_hash_separates_absent_output_from_empty_output_offline() {
        // Pins: "the target said nothing" and "the target said the empty string"
        // are different facts, and the framing bytes keep them different hashes.
        let mut absent = evidence();
        absent.visible_output = None;
        let mut empty = evidence();
        empty.visible_output = Some(String::new());

        assert_ne!(absent.hash(), empty.hash());
        assert!(!absent.produced_result());
        assert!(!empty.produced_result());
        assert!(evidence().produced_result());
    }
}

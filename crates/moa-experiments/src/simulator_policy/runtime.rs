//! Shared production simulator request and response contract.
//!
//! The experiment workflow and the certification registry use these same schema
//! and context-contract digests. That keeps the runtime from silently serving a
//! protocol different from the one a fidelity study certified.

use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::release::Digest32;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::simulator_policy::SimulatorPolicyError;
use crate::simulator_policy::registry::{SimulatorPolicyComponents, SimulatorProtocol};

/// Stable simulator response protocol identifier.
pub const SIMULATOR_PROTOCOL_ID: &str = "moa.behavior_lab.simulator_turn";

/// Current simulator response protocol version.
pub const SIMULATOR_PROTOCOL_VERSION: u32 = 1;

/// Default policy prompt used by repository fixtures and the live smoke.
pub const DEFAULT_SIMULATOR_SYSTEM_PROMPT: &str = "You are the simulated user in a Behavior Lab trial. Follow only the supplied persona, profile, scenario, and data-bundle context. Never call tools or claim to have changed external state. Return one structured simulator-turn response. Set message to the next user turn only for continue; set it to an empty string for terminal decisions.";

/// Highest accepted simulated user message length.
pub const MAX_SIMULATOR_MESSAGE_LEN: usize = 8_192;

/// Highest accepted simulator decision-reason length.
pub const MAX_SIMULATOR_REASON_LEN: usize = 2_048;

/// What the simulated user decided after observing the target transcript.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulatorDecision {
    /// Send the supplied user-visible message to the target.
    Continue,
    /// The simulated user's goal is satisfied; stop without another target turn.
    GoalSatisfied,
    /// The scenario requires transfer to another channel or actor.
    Transfer,
    /// The target interaction has left the certified scenario scope.
    OutOfScope,
}

impl SimulatorDecision {
    /// Returns whether this decision ends simulator-target turns.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Continue)
    }

    /// Returns the stable serialized decision name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::GoalSatisfied => "goal_satisfied",
            Self::Transfer => "transfer",
            Self::OutOfScope => "out_of_scope",
        }
    }
}

/// One structured response emitted by the certified simulator policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorResponse {
    /// Exact response protocol version.
    pub schema_version: u32,
    /// Simulator outcome signal.
    pub decision: SimulatorDecision,
    /// Next user-visible message for `continue`; canonicalized empty for terminal decisions.
    pub message: String,
    /// Bounded audit reason for the decision.
    pub reason: String,
}

/// Why a provider response could not be admitted as a simulator turn.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SimulatorResponseError {
    /// The response was not valid structured JSON.
    #[error("simulator response is not valid structured JSON: {detail}")]
    NotStructured {
        /// Parser detail.
        detail: String,
    },
    /// The response used another protocol version.
    #[error(
        "simulator response schema version {actual} does not match required version {expected}"
    )]
    WrongSchemaVersion {
        /// Required version.
        expected: u32,
        /// Returned version.
        actual: u32,
    },
    /// A continuing response omitted its target-visible message.
    #[error("continuing simulator response must include a nonblank message")]
    MissingMessage,
    /// The message exceeded the runtime bound.
    #[error("simulator response message is {actual} bytes, above the {limit}-byte limit")]
    MessageTooLong {
        /// Observed bytes.
        actual: usize,
        /// Accepted bytes.
        limit: usize,
    },
    /// The reason was empty or exceeded the runtime bound.
    #[error("simulator response reason must be 1..={limit} bytes")]
    InvalidReason {
        /// Accepted bytes.
        limit: usize,
    },
}

/// Parses and validates one provider response against the production protocol.
///
/// # Errors
///
/// Returns [`SimulatorResponseError`] for malformed JSON, protocol drift, or an
/// invalid decision payload.
pub fn parse_simulator_response(raw: &str) -> Result<SimulatorResponse, SimulatorResponseError> {
    let mut response: SimulatorResponse = serde_json::from_str(raw.trim()).map_err(|error| {
        SimulatorResponseError::NotStructured {
            detail: error.to_string(),
        }
    })?;
    if response.schema_version != SIMULATOR_PROTOCOL_VERSION {
        return Err(SimulatorResponseError::WrongSchemaVersion {
            expected: SIMULATOR_PROTOCOL_VERSION,
            actual: response.schema_version,
        });
    }
    response.message = response.message.trim().to_string();
    response.reason = response.reason.trim().to_string();
    if response.message.len() > MAX_SIMULATOR_MESSAGE_LEN {
        return Err(SimulatorResponseError::MessageTooLong {
            actual: response.message.len(),
            limit: MAX_SIMULATOR_MESSAGE_LEN,
        });
    }
    if response.reason.is_empty() || response.reason.len() > MAX_SIMULATOR_REASON_LEN {
        return Err(SimulatorResponseError::InvalidReason {
            limit: MAX_SIMULATOR_REASON_LEN,
        });
    }
    if response.decision == SimulatorDecision::Continue && response.message.is_empty() {
        return Err(SimulatorResponseError::MissingMessage);
    }
    if response.decision.is_terminal() {
        // The certified schema cannot portably express a decision-dependent
        // empty-string constraint across every supported provider. Terminal
        // decisions never enqueue a target turn, so erase any unused model text
        // at this typed boundary instead of applying an unhashed validation rule
        // that is stricter than the certified schema.
        response.message.clear();
    }
    Ok(response)
}

/// Returns the strict provider-facing JSON schema for simulator turns.
#[must_use]
pub fn simulator_response_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["schema_version", "decision", "message", "reason"],
        "properties": {
            "schema_version": { "const": SIMULATOR_PROTOCOL_VERSION },
            "decision": {
                "enum": ["continue", "goal_satisfied", "transfer", "out_of_scope"]
            },
            "message": { "type": "string", "maxLength": MAX_SIMULATOR_MESSAGE_LEN },
            "reason": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_SIMULATOR_REASON_LEN
            }
        }
    })
}

/// Returns the protocol identity this server executes.
///
/// # Errors
///
/// Returns [`SimulatorPolicyError::NotCanonicalizable`] if the schema cannot be
/// canonically hashed.
pub fn production_protocol() -> Result<SimulatorProtocol, SimulatorPolicyError> {
    Ok(SimulatorProtocol {
        id: SIMULATOR_PROTOCOL_ID.to_string(),
        version: SIMULATOR_PROTOCOL_VERSION,
        schema_hash: canonical_hash(&simulator_response_schema())
            .map(Digest32)
            .map_err(|error| SimulatorPolicyError::NotCanonicalizable {
                detail: error.to_string(),
            })?,
    })
}

/// Returns the digest over the server-owned context compilation contract.
///
/// # Errors
///
/// Returns [`SimulatorPolicyError::NotCanonicalizable`] if the contract cannot
/// be canonically hashed.
pub fn production_context_contract_hash() -> Result<Digest32, SimulatorPolicyError> {
    let contract = json!({
        "id": "moa.behavior_lab.simulator_context",
        "version": 1,
        "fields": ["deterministic_seed", "persona", "profile", "scenario", "data_bundles"],
        "serialization": "canonical_plan_selection_json",
        "message_order": ["system_policy", "canonical_context", "durable_target_transcript", "turn_instruction"]
    });
    canonical_hash(&contract).map(Digest32).map_err(|error| {
        SimulatorPolicyError::NotCanonicalizable {
            detail: error.to_string(),
        }
    })
}

/// Verifies that a certified policy can be executed by this server build.
///
/// # Errors
///
/// Returns [`SimulatorPolicyError::RuntimeContractMismatch`] when protocol or
/// context compilation has drifted since certification.
pub fn validate_runtime_contract(
    components: &SimulatorPolicyComponents,
) -> Result<(), SimulatorPolicyError> {
    let served_protocol = production_protocol()?;
    if components.protocol != served_protocol {
        return Err(SimulatorPolicyError::RuntimeContractMismatch {
            detail: format!(
                "policy protocol {}@{} does not match served {}@{}",
                components.protocol.id,
                components.protocol.version,
                served_protocol.id,
                served_protocol.version
            ),
        });
    }
    let served_context = production_context_contract_hash()?;
    if components.context_contract_hash != served_context {
        return Err(SimulatorPolicyError::RuntimeContractMismatch {
            detail: "policy context compiler hash does not match this server build".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator_policy::test_support::components;

    #[test]
    fn structured_terminal_decision_canonicalizes_unused_message_offline() {
        // Pins: stopping is a typed decision, and text returned alongside it can
        // never be mistaken for a normal user message sent to the target.
        let raw = r#"{"schema_version":1,"decision":"goal_satisfied","message":"DONE","reason":"goal met"}"#;
        let response = parse_simulator_response(raw).expect("terminal response should parse");
        assert_eq!(response.decision, SimulatorDecision::GoalSatisfied);
        assert!(response.message.is_empty());
        assert_eq!(response.reason, "goal met");
    }

    #[test]
    fn policy_contract_drift_fails_closed_offline() {
        // Pins: a certified schema hash cannot be executed by a different server
        // protocol under the same policy identity.
        let mut drifted = components();
        drifted.protocol.schema_hash = Digest32([0xFF; 32]);
        assert!(matches!(
            validate_runtime_contract(&drifted),
            Err(SimulatorPolicyError::RuntimeContractMismatch { .. })
        ));
    }
}

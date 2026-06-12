//! Approval payload helpers shared by approval-resolution handlers.

use moa_core::ApprovalDecision;
use restate_sdk::prelude::*;

/// Serializes an approval decision for a Restate awakeable payload.
pub(crate) fn serialize_awakeable_decision(
    decision: &ApprovalDecision,
) -> Result<String, TerminalError> {
    serde_json::to_string(decision).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize approval decision for awakeable: {error}"
        ))
    })
}

/// Parses an approval decision from a Restate awakeable payload.
pub(crate) fn parse_awakeable_decision(raw: &str) -> Result<ApprovalDecision, TerminalError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize approval decision from awakeable: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use moa_core::ApprovalDecision;

    use super::{parse_awakeable_decision, serialize_awakeable_decision};

    #[test]
    fn awakeable_decision_round_trips_through_json_payload() {
        // Pins: approval handlers resolve Restate awakeables with JSON decisions.
        let encoded = serialize_awakeable_decision(&ApprovalDecision::AlwaysAllow {
            pattern: "bash:npm test".to_string(),
        })
        .expect("serialize approval decision");

        let decoded = parse_awakeable_decision(&encoded).expect("deserialize approval decision");
        assert_eq!(
            decoded,
            ApprovalDecision::AlwaysAllow {
                pattern: "bash:npm test".to_string()
            }
        );
    }
}

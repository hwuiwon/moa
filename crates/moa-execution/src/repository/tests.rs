//! Unit tests for execution repository value reconstruction.

use super::audit_codec::*;
use super::*;

#[test]
fn execution_route_repository_parsers_accept_exact_current_values() {
    // Pins: repository reconstruction accepts every value in the normalized
    // decision, strategy, source, stage, and classifier-outcome contract.
    for (value, expected) in [
        ("respond", ExecutionRouteKind::Respond),
        ("execute", ExecutionRouteKind::Execute),
        ("needs_input", ExecutionRouteKind::NeedsInput),
    ] {
        assert_eq!(
            route_decision_from_str(value).expect("current decision should parse"),
            expected
        );
    }
    for (value, expected) in [
        ("inline", ExecutionStrategy::Inline),
        ("durable", ExecutionStrategy::Durable),
    ] {
        assert_eq!(
            execution_strategy_from_str(value).expect("current strategy should parse"),
            expected
        );
    }
    for (value, expected) in [
        ("initial", ExecutionRouteStage::Initial),
        ("durable_upgrade", ExecutionRouteStage::DurableUpgrade),
    ] {
        assert_eq!(
            route_stage_from_str(value).expect("current stage should parse"),
            expected
        );
    }
    for (value, expected) in [
        ("classifier", ExecutionRouteSource::Classifier),
        ("blank_objective", ExecutionRouteSource::BlankObjective),
        (
            "selected_execution_template",
            ExecutionRouteSource::SelectedExecutionTemplate,
        ),
        ("durable_upgrade", ExecutionRouteSource::DurableUpgrade),
    ] {
        assert_eq!(
            route_source_from_str(value).expect("current source should parse"),
            expected
        );
    }
    for (value, expected) in [
        ("not_called", ExecutionRouteClassifierOutcome::NotCalled),
        ("accepted", ExecutionRouteClassifierOutcome::Accepted),
        (
            "provider_error",
            ExecutionRouteClassifierOutcome::ProviderError,
        ),
        ("stream_error", ExecutionRouteClassifierOutcome::StreamError),
        ("oversized", ExecutionRouteClassifierOutcome::Oversized),
        (
            "schema_rejected",
            ExecutionRouteClassifierOutcome::SchemaRejected,
        ),
        (
            "invalid_decision",
            ExecutionRouteClassifierOutcome::InvalidDecision,
        ),
        (
            "low_confidence",
            ExecutionRouteClassifierOutcome::LowConfidence,
        ),
        (
            "context_forced_inline",
            ExecutionRouteClassifierOutcome::ContextForcedInline,
        ),
    ] {
        assert_eq!(
            route_classifier_outcome_from_str(value)
                .expect("current classifier outcome should parse"),
            expected
        );
    }
}

#[test]
fn execution_route_repository_parsers_reject_removed_values() {
    // Pins: the breaking cutover does not translate any removed route value.
    for value in ["routed", "act", "run"] {
        assert!(route_decision_from_str(value).is_err(), "accepted {value}");
    }
    for value in ["respond", "act", "run"] {
        assert!(
            execution_strategy_from_str(value).is_err(),
            "accepted {value}"
        );
    }
    let removed_upgrade = ["act", "_escalation"].concat();
    assert!(route_stage_from_str(&removed_upgrade).is_err());
    assert!(route_source_from_str(&removed_upgrade).is_err());
    assert!(route_classifier_outcome_from_str(&["context_forced_", "act"].concat()).is_err());
}

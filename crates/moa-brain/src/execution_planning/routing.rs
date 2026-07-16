//! Cheap deterministic selection of respond, act, run, or preflight clarification.

use moa_core::types::execution_planning::{
    ActEscalationSignal, ExecutionMode, ExecutionRouteDecision, ExecutionRouteDecisionKind,
    ExecutionRouteReason, ExecutionTemplateInvocation,
};
use moa_execution::repository::RouteAuditWriteOutcome;
use moa_observability::record_execution_route;

/// Immutable inputs used by the execution-mode router.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionRoutingInput<'a> {
    /// Exact current user objective.
    pub objective: &'a str,
    /// Exact explicit template invocation, when supplied by a trusted caller surface.
    pub execution_template: Option<&'a ExecutionTemplateInvocation>,
    /// Bounded evidence emitted by an already-running Act turn.
    pub escalation: Option<&'a ActEscalationSignal>,
}

/// Selects one stable execution route without a model or retrieval call.
#[must_use]
pub fn route_execution(input: ExecutionRoutingInput<'_>) -> ExecutionRouteDecision {
    if input.execution_template.is_some() {
        return routed(
            ExecutionMode::Run,
            ExecutionRouteReason::SelectedExecutionTemplate,
        );
    }
    if input.escalation.is_some() {
        return routed(ExecutionMode::Run, ExecutionRouteReason::ActEscalation);
    }

    let normalized = normalize(input.objective);
    if preflight_input_missing(&normalized) {
        return ExecutionRouteDecision::NeedsInput {
            reason: ExecutionRouteReason::PreflightInputMissing,
        };
    }
    if contains_any(
        &normalized,
        &[
            "await approval",
            "approval step",
            "human approval",
            "human review before",
            "wait for approval",
            "wait for a signal",
            "wait for signal",
        ],
    ) {
        return routed(ExecutionMode::Run, ExecutionRouteReason::ApprovalOrSignal);
    }
    if contains_any(
        &normalized,
        &[
            "for every ",
            "for each ",
            "across all ",
            "bulk ",
            "entire collection",
            "all records",
            "all customers",
            "all accounts",
        ],
    ) {
        return routed(ExecutionMode::Run, ExecutionRouteReason::BulkCollection);
    }
    if contains_any(
        &normalized,
        &[
            "across restarts",
            "in the background",
            "long-running",
            "long running",
            "resume later",
            "resumable",
            "survive restart",
        ],
    ) {
        return routed(ExecutionMode::Run, ExecutionRouteReason::DurableOrResumable);
    }
    if contains_any(
        &normalized,
        &[
            "fan out",
            "high fanout",
            "high fan-out",
            "hundreds of",
            "thousands of",
            "many parallel",
            "parallel workers",
        ],
    ) {
        return routed(ExecutionMode::Run, ExecutionRouteReason::HighFanout);
    }
    if contains_any(
        &normalized,
        &[
            "execute as a run",
            "run as a durable",
            "start a durable run",
            "start an execution run",
            "use an execution run",
        ],
    ) {
        return routed(ExecutionMode::Run, ExecutionRouteReason::ExplicitRun);
    }
    if simple_response(&normalized) {
        return routed(ExecutionMode::Respond, ExecutionRouteReason::SimpleResponse);
    }
    routed(
        ExecutionMode::Act,
        ExecutionRouteReason::BoundedInteractiveWork,
    )
}

/// Emits a route metric only when the durable audit boundary inserted first evidence.
pub fn record_applied_route_audit(result: &RouteAuditWriteOutcome) {
    let RouteAuditWriteOutcome::Applied(evidence) = result else {
        return;
    };
    let decision = match (evidence.decision, evidence.mode) {
        (ExecutionRouteDecisionKind::NeedsInput, None) => ExecutionRouteDecision::NeedsInput {
            reason: evidence.reason,
        },
        (ExecutionRouteDecisionKind::Routed, Some(mode)) => ExecutionRouteDecision::Routed {
            mode,
            reason: evidence.reason,
        },
        (ExecutionRouteDecisionKind::NeedsInput, Some(_))
        | (ExecutionRouteDecisionKind::Routed, None) => return,
    };
    record_execution_route(&decision);
}

fn routed(mode: ExecutionMode, reason: ExecutionRouteReason) -> ExecutionRouteDecision {
    ExecutionRouteDecision::Routed { mode, reason }
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn preflight_input_missing(value: &str) -> bool {
    value.is_empty()
        || matches!(
            value,
            "do it"
                | "do this"
                | "fix it"
                | "fix this"
                | "handle it"
                | "handle this"
                | "make it"
                | "run it"
                | "start it"
        )
        || value.ends_with("[todo]")
        || value.contains("without specifying which")
}

fn simple_response(value: &str) -> bool {
    let word_count = value.split_whitespace().count();
    (value.ends_with('?') && word_count <= 24)
        || (word_count <= 12
            && starts_with_any(
                value,
                &[
                    "explain ",
                    "what is ",
                    "what are ",
                    "who is ",
                    "why ",
                    "how does ",
                ],
            ))
        || matches!(value, "hello" | "hi" | "thanks" | "thank you")
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn starts_with_any(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| value.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use metrics::{
        Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };
    use moa_execution::repository::RouteAuditEvidence;
    use uuid::Uuid;

    use super::*;

    struct CounterRecorder {
        count: Arc<AtomicU64>,
    }

    impl Recorder for CounterRecorder {
        fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        }

        fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

        fn describe_histogram(
            &self,
            _key: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn register_counter(&self, _key: &Key, _metadata: &Metadata<'_>) -> Counter {
            Counter::from_arc(Arc::clone(&self.count))
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }

    #[test]
    fn execution_planning_routing_pins_all_stable_reasons() {
        // Pins: execution shape, not generic difficulty, chooses a durable run.
        let cases = [
            (
                "What is a DAG?",
                ExecutionMode::Respond,
                ExecutionRouteReason::SimpleResponse,
            ),
            (
                "Investigate the unusual failure and explain it",
                ExecutionMode::Act,
                ExecutionRouteReason::BoundedInteractiveWork,
            ),
            (
                "Start an execution run for this investigation",
                ExecutionMode::Run,
                ExecutionRouteReason::ExplicitRun,
            ),
            (
                "Process all records in the collection",
                ExecutionMode::Run,
                ExecutionRouteReason::BulkCollection,
            ),
            (
                "Run this in the background and resume later",
                ExecutionMode::Run,
                ExecutionRouteReason::DurableOrResumable,
            ),
            (
                "Use many parallel workers to inspect the accounts",
                ExecutionMode::Run,
                ExecutionRouteReason::HighFanout,
            ),
            (
                "Prepare the report and wait for approval",
                ExecutionMode::Run,
                ExecutionRouteReason::ApprovalOrSignal,
            ),
        ];
        for (objective, mode, reason) in cases {
            assert_eq!(
                route_execution(ExecutionRoutingInput {
                    objective,
                    execution_template: None,
                    escalation: None,
                }),
                ExecutionRouteDecision::Routed { mode, reason },
                "{objective}"
            );
        }
    }

    #[test]
    fn execution_planning_routing_preserves_preflight_and_open_ended_act() {
        // Pins: unresolved deictic requests clarify, while difficult exploration stays Act.
        assert_eq!(
            route_execution(ExecutionRoutingInput {
                objective: "do it",
                execution_template: None,
                escalation: None,
            }),
            ExecutionRouteDecision::NeedsInput {
                reason: ExecutionRouteReason::PreflightInputMissing,
            }
        );
        assert_eq!(
            route_execution(ExecutionRoutingInput {
                objective: "Deeply investigate why this production behavior is inconsistent",
                execution_template: None,
                escalation: None,
            }),
            ExecutionRouteDecision::Routed {
                mode: ExecutionMode::Act,
                reason: ExecutionRouteReason::BoundedInteractiveWork,
            }
        );
    }

    #[test]
    fn durable_route_metric_suppresses_exact_replay() {
        // Pins: mutation-checking the durable gate to treat Replayed as Applied would
        // duplicate the route counter and fail this exact count.
        let evidence = RouteAuditEvidence {
            audit_uid: Uuid::now_v7(),
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Run),
            reason: ExecutionRouteReason::ExplicitRun,
            accepted_at: chrono::Utc::now(),
        };
        let count = Arc::new(AtomicU64::new(0));
        let recorder = CounterRecorder {
            count: Arc::clone(&count),
        };
        metrics::with_local_recorder(&recorder, || {
            record_applied_route_audit(&RouteAuditWriteOutcome::Applied(evidence));
            record_applied_route_audit(&RouteAuditWriteOutcome::Replayed(evidence));
        });
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}

//! Shared deterministic adapters for execution-run and task workflow node actions.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::ExecutionFailureClass;
use moa_execution::{
    completion::{CompletionEvaluation, CompletionStatus},
    repository::{
        ExecutionMutationMetricEvidence, ExecutionRunRecord, ExecutionRunTransitionEvidence,
        ExecutionTaskRecord, ExecutionTaskTransitionEvidence,
    },
    state::{
        ExecutionRunStatus, ExecutionTaskFailure, ExecutionTaskStatus, ExecutionTerminalReason,
        LogicalTaskKind, TerminalProjection,
    },
};
use moa_observability::{
    ExecutionMetricFailureClass, ExecutionMetricRunState, ExecutionMetricTaskKind,
    ExecutionMetricTaskOutcome, ExecutionMetricTaskState, ExecutionMetricTerminalReason,
    ExecutionMetricTerminalStatus, ExecutionMetricUsage, ExecutionMetricUsageValues,
    record_execution_run_coverage, record_execution_run_queue_to_start,
    record_execution_run_state_transition, record_execution_run_terminal,
    record_execution_run_usage, record_execution_task_duration, record_execution_task_retry,
    record_execution_task_state_transition,
};
use serde_json::Value;

/// Converts deterministic completion evidence into the matching terminal projection.
pub(crate) fn terminal_projection_from_evaluation(
    evaluation: &CompletionEvaluation,
    output: Option<Value>,
    additional_gap: Option<String>,
) -> TerminalProjection {
    let mut gaps = evaluation.gaps.clone();
    if let Some(gap) = additional_gap {
        gaps.push(gap);
        gaps.sort();
        gaps.dedup();
    }
    match evaluation.status {
        CompletionStatus::Completed => TerminalProjection::Completed {
            output: output.unwrap_or(Value::Null),
        },
        CompletionStatus::Partial => TerminalProjection::Partial { output, gaps },
        CompletionStatus::Blocked => TerminalProjection::Blocked { output, gaps },
        CompletionStatus::Unsupported => TerminalProjection::Unsupported {
            reason: gaps
                .first()
                .cloned()
                .unwrap_or_else(|| "required execution path is unsupported".to_string()),
            gaps,
        },
        CompletionStatus::Failed => TerminalProjection::Failed {
            failure: ExecutionTaskFailure {
                class: ExecutionFailureClass::Terminal,
                message: gaps
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "execution produced no required result".to_string()),
                capability_ref: None,
            },
        },
    }
}

pub(crate) fn record_applied_run_transition(
    prior: Option<ExecutionRunStatus>,
    run: &ExecutionRunRecord,
) {
    let Some(prior_status) = prior else {
        record_execution_run_state_transition(metric_run_state(run.status));
        if run.status.is_terminal() {
            record_applied_terminal_run(run);
        }
        return;
    };
    record_run_transition_evidence(&ExecutionRunTransitionEvidence {
        prior_status,
        status: run.status,
        queued_at: run.queued_at,
        started_at: run.started_at,
        reserved: run.reserved,
        consumed: run.consumed,
        terminal_evidence: run.terminal_evidence.clone(),
        terminal_reason: run.terminal_reason,
    });
}

/// Emits every run and task metric transition committed by one first-applied transaction.
pub(crate) fn record_applied_execution_mutation(metrics: &ExecutionMutationMetricEvidence) {
    record_run_transition_evidence(&metrics.run);
    for task in &metrics.tasks {
        record_task_transition_evidence(task);
    }
}

fn record_run_transition_evidence(evidence: &ExecutionRunTransitionEvidence) {
    if evidence.prior_status == evidence.status {
        return;
    }
    record_execution_run_state_transition(metric_run_state(evidence.status));
    if evidence.prior_status == ExecutionRunStatus::Queued
        && evidence.status == ExecutionRunStatus::Running
        && let (Some(queued_at), Some(started_at)) = (evidence.queued_at, evidence.started_at)
    {
        record_execution_run_queue_to_start(nonnegative_duration(queued_at, started_at));
    }
    if evidence.status.is_terminal() {
        record_terminal_run_metrics(
            evidence.status,
            evidence.reserved,
            evidence.consumed,
            evidence.terminal_evidence.as_ref(),
            evidence.terminal_reason,
        );
    }
}

pub(crate) fn record_applied_task_transition(
    prior: Option<ExecutionTaskStatus>,
    task: &ExecutionTaskRecord,
) {
    let Some(prior_status) = prior else {
        let kind = metric_task_kind(&task.kind);
        record_execution_task_state_transition(metric_task_state(task.status), kind);
        record_terminal_task_duration(
            task.status,
            kind,
            task.created_at,
            task.updated_at,
            task.started_at,
            task.completed_at,
        );
        return;
    };
    record_task_transition_evidence(&ExecutionTaskTransitionEvidence {
        prior_status,
        status: task.status,
        kind: task.kind.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        started_at: task.started_at,
        completed_at: task.completed_at,
    });
}

fn record_task_transition_evidence(evidence: &ExecutionTaskTransitionEvidence) {
    if evidence.prior_status == evidence.status {
        return;
    }
    let kind = metric_task_kind(&evidence.kind);
    record_execution_task_state_transition(metric_task_state(evidence.status), kind);
    record_terminal_task_duration(
        evidence.status,
        kind,
        evidence.created_at,
        evidence.updated_at,
        evidence.started_at,
        evidence.completed_at,
    );
}

pub(crate) fn record_applied_task_retry(task: &ExecutionTaskRecord) {
    let Some(failure_class) =
        task.current_outcome
            .as_ref()
            .and_then(|outcome| match &outcome.result {
                moa_artifacts::execution_plan::ExecutionTaskResult::Failed { class, .. } => {
                    Some(class)
                }
                moa_artifacts::execution_plan::ExecutionTaskResult::Completed { .. }
                | moa_artifacts::execution_plan::ExecutionTaskResult::NeedsInput { .. }
                | moa_artifacts::execution_plan::ExecutionTaskResult::NeedsReplan { .. }
                | moa_artifacts::execution_plan::ExecutionTaskResult::Cancelled { .. } => None,
            })
    else {
        return;
    };
    record_execution_task_retry(
        metric_task_kind(&task.kind),
        metric_failure_class(failure_class),
    );
}

fn record_applied_terminal_run(run: &ExecutionRunRecord) {
    record_terminal_run_metrics(
        run.status,
        run.reserved,
        run.consumed,
        run.terminal_evidence.as_ref(),
        run.terminal_reason,
    );
}

fn record_terminal_run_metrics(
    status: ExecutionRunStatus,
    reserved: moa_execution::capability::ExecutionEstimate,
    consumed: moa_execution::capability::ExecutionEstimate,
    evidence: Option<&moa_execution::state::ExecutionTerminalEvidence>,
    reason: Option<ExecutionTerminalReason>,
) {
    let (Some(evidence), Some(reason)) = (evidence, reason) else {
        return;
    };
    let status = metric_terminal_status(status);
    record_execution_run_usage(
        ExecutionMetricUsage::Reserved,
        ExecutionMetricUsageValues {
            cost_microusd: reserved.cost_microusd,
            tokens: reserved.tokens,
            tasks: reserved.tasks,
            tool_calls: reserved.tool_calls,
            retrieved_bytes: reserved.retrieved_bytes,
        },
    );
    record_execution_run_usage(
        ExecutionMetricUsage::Actual,
        ExecutionMetricUsageValues {
            cost_microusd: consumed.cost_microusd,
            tokens: consumed.tokens,
            tasks: consumed.tasks,
            tool_calls: consumed.tool_calls,
            retrieved_bytes: consumed.retrieved_bytes,
        },
    );
    record_execution_run_coverage(
        status,
        evidence.satisfied_requirement_count,
        evidence.requirement_count,
    );
    record_execution_run_terminal(status, metric_terminal_reason(reason));
}

fn record_terminal_task_duration(
    status: ExecutionTaskStatus,
    kind: ExecutionMetricTaskKind,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
) {
    let Some(outcome) = metric_terminal_task_outcome(status) else {
        return;
    };
    let completed_at = completed_at.unwrap_or(updated_at);
    let started_at = started_at.unwrap_or(created_at);
    record_execution_task_duration(
        kind,
        outcome,
        nonnegative_duration(started_at, completed_at),
    );
}

const fn metric_run_state(status: ExecutionRunStatus) -> ExecutionMetricRunState {
    match status {
        ExecutionRunStatus::AwaitingConfirmation => ExecutionMetricRunState::AwaitingConfirmation,
        ExecutionRunStatus::Queued => ExecutionMetricRunState::Queued,
        ExecutionRunStatus::Running => ExecutionMetricRunState::Running,
        ExecutionRunStatus::WaitingInput => ExecutionMetricRunState::WaitingInput,
        ExecutionRunStatus::WaitingReview => ExecutionMetricRunState::WaitingReview,
        ExecutionRunStatus::WaitingReplan => ExecutionMetricRunState::WaitingReplan,
        ExecutionRunStatus::Completed => ExecutionMetricRunState::Completed,
        ExecutionRunStatus::Partial => ExecutionMetricRunState::Partial,
        ExecutionRunStatus::Blocked => ExecutionMetricRunState::Blocked,
        ExecutionRunStatus::Unsupported => ExecutionMetricRunState::Unsupported,
        ExecutionRunStatus::Failed => ExecutionMetricRunState::Failed,
        ExecutionRunStatus::Cancelled => ExecutionMetricRunState::Cancelled,
    }
}

const fn metric_task_state(status: ExecutionTaskStatus) -> ExecutionMetricTaskState {
    match status {
        ExecutionTaskStatus::Pending => ExecutionMetricTaskState::Pending,
        ExecutionTaskStatus::Reserved => ExecutionMetricTaskState::Reserved,
        ExecutionTaskStatus::Running => ExecutionMetricTaskState::Running,
        ExecutionTaskStatus::WaitingInput => ExecutionMetricTaskState::WaitingInput,
        ExecutionTaskStatus::WaitingReplan => ExecutionMetricTaskState::WaitingReplan,
        ExecutionTaskStatus::Completed => ExecutionMetricTaskState::Completed,
        ExecutionTaskStatus::Skipped => ExecutionMetricTaskState::Skipped,
        ExecutionTaskStatus::Failed => ExecutionMetricTaskState::Failed,
        ExecutionTaskStatus::Cancelled => ExecutionMetricTaskState::Cancelled,
    }
}

const fn metric_task_kind(kind: &LogicalTaskKind) -> ExecutionMetricTaskKind {
    match kind {
        LogicalTaskKind::Capability { .. } => ExecutionMetricTaskKind::Capability,
        LogicalTaskKind::Agent { .. } => ExecutionMetricTaskKind::Agent,
        LogicalTaskKind::Review { .. } => ExecutionMetricTaskKind::Review,
        LogicalTaskKind::WaitSignal { .. } => ExecutionMetricTaskKind::WaitSignal,
        LogicalTaskKind::Output { .. } => ExecutionMetricTaskKind::Output,
        LogicalTaskKind::CompletionVerifier { .. } => ExecutionMetricTaskKind::CompletionVerifier,
    }
}

const fn metric_terminal_task_outcome(
    status: ExecutionTaskStatus,
) -> Option<ExecutionMetricTaskOutcome> {
    match status {
        ExecutionTaskStatus::Completed => Some(ExecutionMetricTaskOutcome::Completed),
        ExecutionTaskStatus::Skipped => Some(ExecutionMetricTaskOutcome::Skipped),
        ExecutionTaskStatus::Failed => Some(ExecutionMetricTaskOutcome::Failed),
        ExecutionTaskStatus::Cancelled => Some(ExecutionMetricTaskOutcome::Cancelled),
        ExecutionTaskStatus::Pending
        | ExecutionTaskStatus::Reserved
        | ExecutionTaskStatus::Running
        | ExecutionTaskStatus::WaitingInput
        | ExecutionTaskStatus::WaitingReplan => None,
    }
}

const fn metric_failure_class(class: &ExecutionFailureClass) -> ExecutionMetricFailureClass {
    match class {
        ExecutionFailureClass::Retryable => ExecutionMetricFailureClass::Retryable,
        ExecutionFailureClass::DependencyFailed => ExecutionMetricFailureClass::DependencyFailed,
        ExecutionFailureClass::InvalidInput => ExecutionMetricFailureClass::InvalidInput,
        ExecutionFailureClass::InvalidOutput => ExecutionMetricFailureClass::InvalidOutput,
        ExecutionFailureClass::AuthorizationDenied => {
            ExecutionMetricFailureClass::AuthorizationDenied
        }
        ExecutionFailureClass::BudgetExceeded => ExecutionMetricFailureClass::BudgetExceeded,
        ExecutionFailureClass::DeadlineExceeded => ExecutionMetricFailureClass::DeadlineExceeded,
        ExecutionFailureClass::Cancelled => ExecutionMetricFailureClass::Cancelled,
        ExecutionFailureClass::Unsupported => ExecutionMetricFailureClass::Unsupported,
        ExecutionFailureClass::Terminal => ExecutionMetricFailureClass::Terminal,
    }
}

fn metric_terminal_status(status: ExecutionRunStatus) -> ExecutionMetricTerminalStatus {
    match status {
        ExecutionRunStatus::Completed => ExecutionMetricTerminalStatus::Completed,
        ExecutionRunStatus::Partial => ExecutionMetricTerminalStatus::Partial,
        ExecutionRunStatus::Blocked => ExecutionMetricTerminalStatus::Blocked,
        ExecutionRunStatus::Unsupported => ExecutionMetricTerminalStatus::Unsupported,
        ExecutionRunStatus::Failed => ExecutionMetricTerminalStatus::Failed,
        ExecutionRunStatus::Cancelled => ExecutionMetricTerminalStatus::Cancelled,
        ExecutionRunStatus::AwaitingConfirmation
        | ExecutionRunStatus::Queued
        | ExecutionRunStatus::Running
        | ExecutionRunStatus::WaitingInput
        | ExecutionRunStatus::WaitingReview
        | ExecutionRunStatus::WaitingReplan => {
            unreachable!("terminal metric requires a terminal execution run")
        }
    }
}

const fn metric_terminal_reason(reason: ExecutionTerminalReason) -> ExecutionMetricTerminalReason {
    match reason {
        ExecutionTerminalReason::Completed => ExecutionMetricTerminalReason::Completed,
        ExecutionTerminalReason::GoalIncomplete => ExecutionMetricTerminalReason::GoalIncomplete,
        ExecutionTerminalReason::BudgetExceeded => ExecutionMetricTerminalReason::BudgetExceeded,
        ExecutionTerminalReason::DeadlineExceeded => {
            ExecutionMetricTerminalReason::DeadlineExceeded
        }
        ExecutionTerminalReason::Cancelled => ExecutionMetricTerminalReason::Cancelled,
        ExecutionTerminalReason::NoProgress => ExecutionMetricTerminalReason::NoProgress,
        ExecutionTerminalReason::DuplicatePlan => ExecutionMetricTerminalReason::DuplicatePlan,
        ExecutionTerminalReason::DuplicateAmendment => {
            ExecutionMetricTerminalReason::DuplicateAmendment
        }
        ExecutionTerminalReason::RepeatedFailure => ExecutionMetricTerminalReason::RepeatedFailure,
        ExecutionTerminalReason::BudgetExhausted => ExecutionMetricTerminalReason::BudgetExhausted,
        ExecutionTerminalReason::TaskFailure => ExecutionMetricTerminalReason::TaskFailure,
        ExecutionTerminalReason::UnsupportedPlan => ExecutionMetricTerminalReason::UnsupportedPlan,
        ExecutionTerminalReason::Blocked => ExecutionMetricTerminalReason::Blocked,
        ExecutionTerminalReason::InternalFailure => ExecutionMetricTerminalReason::InternalFailure,
    }
}

fn nonnegative_duration(start: DateTime<Utc>, end: DateTime<Utc>) -> Duration {
    (end - start).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use chrono::{DateTime, TimeZone, Utc};
    use metrics::{
        Counter, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder, SharedString,
        Unit,
    };
    use moa_execution::{
        capability::ExecutionEstimate,
        repository::{
            ExecutionMutationMetricEvidence, ExecutionRunTransitionEvidence,
            ExecutionTaskTransitionEvidence,
        },
        state::{
            ExecutionRunStatus, ExecutionTaskStatus, ExecutionTerminalCause,
            ExecutionTerminalEvidence, ExecutionTerminalReason, LogicalTaskKind,
        },
    };
    use serde_json::json;

    use super::record_applied_execution_mutation;

    struct DurationSamples(Arc<Mutex<Vec<f64>>>);

    impl HistogramFn for DurationSamples {
        fn record(&self, value: f64) {
            self.0
                .lock()
                .expect("duration sample recorder lock should remain available")
                .push(value);
        }
    }

    struct ExecutionTransitionRecorder {
        run_transitions: Arc<AtomicU64>,
        task_transitions: Arc<AtomicU64>,
        terminal_runs: Arc<AtomicU64>,
        task_durations: Arc<Mutex<Vec<f64>>>,
    }

    impl Recorder for ExecutionTransitionRecorder {
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

        fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
            match key.name() {
                "moa_execution_run_state_transitions_total" => {
                    Counter::from_arc(Arc::clone(&self.run_transitions))
                }
                "moa_execution_task_state_transitions_total" => {
                    Counter::from_arc(Arc::clone(&self.task_transitions))
                }
                "moa_execution_runs_terminal_total" => {
                    Counter::from_arc(Arc::clone(&self.terminal_runs))
                }
                _ => Counter::noop(),
            }
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            if key.name() == "moa_execution_task_duration_seconds" {
                Histogram::from_arc(Arc::new(DurationSamples(Arc::clone(&self.task_durations))))
            } else {
                Histogram::noop()
            }
        }
    }

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("test timestamp should be valid")
    }

    fn task_transition(
        prior_status: ExecutionTaskStatus,
        created_at: i64,
        started_at: Option<i64>,
        completed_at: i64,
    ) -> ExecutionTaskTransitionEvidence {
        ExecutionTaskTransitionEvidence {
            prior_status,
            status: ExecutionTaskStatus::Cancelled,
            kind: LogicalTaskKind::Output {
                value: json!({"ok": true}),
            },
            created_at: timestamp(created_at),
            updated_at: timestamp(completed_at),
            started_at: started_at.map(timestamp),
            completed_at: Some(timestamp(completed_at)),
        }
    }

    #[test]
    fn applied_execution_mutation_metrics_emit_every_committed_transition_once() {
        // Pins: amendment and terminal cancellation emit the exact run transition plus every
        // persisted task transition/duration carried by their first-applied repository result.
        let amendment = ExecutionMutationMetricEvidence {
            run: ExecutionRunTransitionEvidence {
                prior_status: ExecutionRunStatus::WaitingReplan,
                status: ExecutionRunStatus::Running,
                queued_at: Some(timestamp(1)),
                started_at: Some(timestamp(2)),
                reserved: ExecutionEstimate::default(),
                consumed: ExecutionEstimate::default(),
                terminal_evidence: None,
                terminal_reason: None,
            },
            tasks: vec![task_transition(
                ExecutionTaskStatus::WaitingReplan,
                10,
                Some(10),
                15,
            )],
        };
        let cancellation = ExecutionMutationMetricEvidence {
            run: ExecutionRunTransitionEvidence {
                prior_status: ExecutionRunStatus::Running,
                status: ExecutionRunStatus::Cancelled,
                queued_at: Some(timestamp(1)),
                started_at: Some(timestamp(2)),
                reserved: ExecutionEstimate::default(),
                consumed: ExecutionEstimate {
                    tasks: 2,
                    ..ExecutionEstimate::default()
                },
                terminal_evidence: Some(ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::Cancellation,
                    satisfied_requirement_count: 0,
                    requirement_count: 1,
                }),
                terminal_reason: Some(ExecutionTerminalReason::Cancelled),
            },
            tasks: vec![
                task_transition(ExecutionTaskStatus::Pending, 20, None, 23),
                task_transition(ExecutionTaskStatus::Running, 20, Some(21), 28),
            ],
        };
        let run_transitions = Arc::new(AtomicU64::new(0));
        let task_transitions = Arc::new(AtomicU64::new(0));
        let terminal_runs = Arc::new(AtomicU64::new(0));
        let task_durations = Arc::new(Mutex::new(Vec::new()));
        let recorder = ExecutionTransitionRecorder {
            run_transitions: Arc::clone(&run_transitions),
            task_transitions: Arc::clone(&task_transitions),
            terminal_runs: Arc::clone(&terminal_runs),
            task_durations: Arc::clone(&task_durations),
        };

        metrics::with_local_recorder(&recorder, || {
            record_applied_execution_mutation(&amendment);
            record_applied_execution_mutation(&cancellation);
        });

        assert_eq!(run_transitions.load(Ordering::Relaxed), 2);
        assert_eq!(task_transitions.load(Ordering::Relaxed), 3);
        assert_eq!(terminal_runs.load(Ordering::Relaxed), 1);
        let mut durations = task_durations
            .lock()
            .expect("duration samples should remain readable")
            .clone();
        durations.sort_by(f64::total_cmp);
        assert_eq!(durations, vec![3.0, 5.0, 7.0]);
    }
}

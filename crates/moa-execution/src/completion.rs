//! Deterministic goal, deliverable, coverage, citation, and verifier completion checks.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionFailureClass, ExecutionGoalContract,
    ExecutionOperation, ExecutionTaskResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Error, Result,
    bindings::{BindingContext, extract_map_key, resolve_bindings},
    budget::BudgetLedger,
    compiler::CanonicalExecutionPlan,
    schema::validate_instance,
    state::{
        ExecutionLimitStop, ExecutionMapItem, ExecutionMapItemStatus, ExecutionMapOutput,
        ExecutionNodeStatus, ExecutionProjection, ExecutionRunStatus, ExecutionTaskFailure,
        ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
        ExecutionTerminalEvidence, ExecutionTerminalReason, TerminalProjection,
    },
};

/// Complete pure input to deterministic completion evaluation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvaluationRequest {
    /// Immutable execution goal contract.
    pub goal: ExecutionGoalContract,
    /// Active canonical execution plan.
    pub plan: CanonicalExecutionPlan,
    /// Immutable run input.
    pub run_input: Value,
    /// Current durable node and task projection.
    pub projection: ExecutionProjection,
    /// Resolved terminal output, when available.
    pub terminal_output: Option<Value>,
    /// Current run-level budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Deterministic evaluation time.
    pub now: DateTime<Utc>,
}

/// Persistable deterministic completion result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionEvaluation {
    /// Terminal status selected by the fixed completion precedence.
    pub status: CompletionStatus,
    /// Exact typed resource limit that stopped completion, if any.
    pub limit_stop: Option<ExecutionLimitStop>,
    /// One persisted result for each declared completion check.
    pub checks: Vec<CompletionCheckResult>,
    /// Sorted goal requirement IDs that are fully satisfied.
    pub satisfied_requirement_ids: Vec<String>,
    /// Sorted goal requirement IDs that remain unsatisfied.
    pub unsatisfied_requirement_ids: Vec<String>,
    /// Deterministic human-readable completion gaps.
    pub gaps: Vec<String>,
}

/// Fixed terminal completion statuses.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    /// Every required completion gate passed within budget and deadline.
    Completed,
    /// Some useful result exists but required scope remains incomplete.
    Partial,
    /// A live input, review, signal, or authorization gate prevents progress.
    Blocked,
    /// Every available path for required work ended unsupported.
    Unsupported,
    /// No useful required result could be produced.
    Failed,
}

/// Returns the durable run status represented by one completion evaluation status.
#[must_use]
pub const fn run_status_from_completion(status: CompletionStatus) -> ExecutionRunStatus {
    match status {
        CompletionStatus::Completed => ExecutionRunStatus::Completed,
        CompletionStatus::Partial => ExecutionRunStatus::Partial,
        CompletionStatus::Blocked => ExecutionRunStatus::Blocked,
        CompletionStatus::Unsupported => ExecutionRunStatus::Unsupported,
        CompletionStatus::Failed => ExecutionRunStatus::Failed,
    }
}

/// Converts deterministic completion evidence into the matching terminal projection.
pub fn terminal_projection_from_evaluation(
    evaluation: &CompletionEvaluation,
    output: Option<Value>,
    additional_gap: Option<String>,
    failure: Option<ExecutionTaskFailure>,
    unsupported_reason: Option<String>,
) -> Result<TerminalProjection> {
    let mut gaps = evaluation.gaps.clone();
    if let Some(gap) = additional_gap {
        gaps.push(gap);
        gaps.sort();
        gaps.dedup();
    }
    Ok(match evaluation.status {
        CompletionStatus::Completed => TerminalProjection::Completed {
            output: output.ok_or_else(|| Error::InvalidProjection {
                message: "completed evaluation has no terminal output".to_string(),
            })?,
        },
        CompletionStatus::Partial => TerminalProjection::Partial { output, gaps },
        CompletionStatus::Blocked => TerminalProjection::Blocked { output, gaps },
        CompletionStatus::Unsupported => TerminalProjection::Unsupported {
            reason: unsupported_reason.unwrap_or_else(|| {
                gaps.first()
                    .cloned()
                    .unwrap_or_else(|| "required execution path is unsupported".to_string())
            }),
            gaps,
        },
        CompletionStatus::Failed => TerminalProjection::Failed {
            failure: failure.unwrap_or_else(|| ExecutionTaskFailure {
                class: ExecutionFailureClass::Terminal,
                message: gaps
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "execution produced no required result".to_string()),
                capability_ref: None,
            }),
        },
    })
}

/// Returns whether a terminal projection represents one completion evaluation status.
#[must_use]
pub const fn terminal_projection_matches_completion(
    projection: &TerminalProjection,
    status: CompletionStatus,
) -> bool {
    matches!(
        (projection, status),
        (
            TerminalProjection::Completed { .. },
            CompletionStatus::Completed
        ) | (
            TerminalProjection::Partial { .. },
            CompletionStatus::Partial
        ) | (
            TerminalProjection::Blocked { .. },
            CompletionStatus::Blocked
        ) | (
            TerminalProjection::Unsupported { .. },
            CompletionStatus::Unsupported
        ) | (TerminalProjection::Failed { .. }, CompletionStatus::Failed)
            | (TerminalProjection::Cancelled { .. }, _)
    )
}

/// Selects the typed terminal cause from a pure execution projection and budget ledger.
#[must_use]
pub fn terminal_cause(
    projection: &ExecutionProjection,
    budget_ledger: &BudgetLedger,
    terminal: &TerminalProjection,
    now: DateTime<Utc>,
) -> ExecutionTerminalCause {
    let deadline_exceeded = budget_ledger
        .limit
        .deadline_at
        .is_some_and(|deadline| now > deadline);
    let unfinished_work = projection.node_statuses.values().any(|status| {
        !matches!(
            status,
            ExecutionNodeStatus::Completed
                | ExecutionNodeStatus::Skipped
                | ExecutionNodeStatus::Failed
                | ExecutionNodeStatus::Cancelled
        )
    });
    if deadline_exceeded && unfinished_work {
        return ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::DeadlineExceeded,
        };
    }
    if let Some(class) = projection.tasks.iter().find_map(|task| {
        task.outcome
            .as_ref()
            .and_then(|outcome| match &outcome.result {
                ExecutionTaskResult::Failed { class, .. } => Some(class.clone()),
                ExecutionTaskResult::UnknownOutcome { .. } => Some(ExecutionFailureClass::Terminal),
                _ => None,
            })
    }) {
        return ExecutionTerminalCause::TaskFailure { class };
    }
    let budget_stopped_dispatch = matches!(
        terminal,
        TerminalProjection::Failed { failure }
            if failure.class == ExecutionFailureClass::BudgetExceeded
    ) || matches!(
        terminal,
        TerminalProjection::Partial { gaps, .. }
            if gaps.iter().any(|gap| gap == "execution budget cannot reserve required work")
    );
    if budget_stopped_dispatch {
        return ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::BudgetExceeded,
        };
    }
    if matches!(terminal, TerminalProjection::Cancelled { .. }) {
        return ExecutionTerminalCause::Cancellation;
    }
    if let TerminalProjection::Failed { failure } = terminal {
        return ExecutionTerminalCause::TaskFailure {
            class: failure.class.clone(),
        };
    }
    let limit_stop = if deadline_exceeded {
        Some(ExecutionLimitStop::DeadlineExceeded)
    } else if budget_ledger.overrun {
        Some(ExecutionLimitStop::BudgetExceeded)
    } else {
        None
    };
    ExecutionTerminalCause::Completion { limit_stop }
}

/// Persisted result of one declared completion check.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheckResult {
    /// Stable completion-check ID.
    pub check_id: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Structured deterministic evidence.
    pub evidence: Value,
}

/// Builds the immutable terminal replay identity from one deterministic completion evaluation.
pub fn terminal_evidence_from_evaluation(
    cause: ExecutionTerminalCause,
    evaluation: &CompletionEvaluation,
) -> Result<ExecutionTerminalEvidence> {
    let satisfied_requirement_count = u64::try_from(evaluation.satisfied_requirement_ids.len())
        .map_err(|_| Error::ArithmeticOverflow {
            context: "satisfied execution requirement count".to_string(),
        })?;
    let unsatisfied_requirement_count = u64::try_from(evaluation.unsatisfied_requirement_ids.len())
        .map_err(|_| Error::ArithmeticOverflow {
            context: "unsatisfied execution requirement count".to_string(),
        })?;
    let requirement_count = satisfied_requirement_count
        .checked_add(unsatisfied_requirement_count)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "total execution requirement count".to_string(),
        })?;
    Ok(ExecutionTerminalEvidence {
        cause,
        satisfied_requirement_count,
        requirement_count,
    })
}

/// Selects and validates the exact normalized terminal reason from typed evidence.
pub fn execution_terminal_reason(
    cause: &ExecutionTerminalCause,
    projection: &TerminalProjection,
    evaluation: &CompletionEvaluation,
) -> Result<ExecutionTerminalReason> {
    let projection_status = completion_status_from_projection(projection);
    if !matches!(
        (cause, projection),
        (
            ExecutionTerminalCause::Cancellation,
            TerminalProjection::Cancelled { .. }
        )
    ) && projection_status != Some(evaluation.status)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "terminal projection and completion evaluation status disagree".to_string(),
        });
    }

    let reason = match cause {
        ExecutionTerminalCause::Cancellation => {
            if !matches!(projection, TerminalProjection::Cancelled { .. }) {
                return invalid_terminal_combination(cause, projection);
            }
            ExecutionTerminalReason::Cancelled
        }
        ExecutionTerminalCause::InternalFailure => {
            if !matches!(projection, TerminalProjection::Failed { .. }) {
                return invalid_terminal_combination(cause, projection);
            }
            ExecutionTerminalReason::InternalFailure
        }
        ExecutionTerminalCause::CompensationFailure { .. } => {
            if !matches!(projection, TerminalProjection::Failed { .. }) {
                return invalid_terminal_combination(cause, projection);
            }
            ExecutionTerminalReason::CompensationFailed
        }
        ExecutionTerminalCause::ReplanStop { reason } => {
            if !matches!(
                projection,
                TerminalProjection::Partial { .. } | TerminalProjection::Blocked { .. }
            ) {
                return invalid_terminal_combination(cause, projection);
            }
            match reason {
                crate::replan::ReplanStopReason::DuplicatePlan => {
                    ExecutionTerminalReason::DuplicatePlan
                }
                crate::replan::ReplanStopReason::DuplicateAmendment => {
                    ExecutionTerminalReason::DuplicateAmendment
                }
                crate::replan::ReplanStopReason::RepeatedFailure => {
                    ExecutionTerminalReason::RepeatedFailure
                }
                crate::replan::ReplanStopReason::NoProgress => ExecutionTerminalReason::NoProgress,
                crate::replan::ReplanStopReason::DeadlineExceeded => {
                    ExecutionTerminalReason::DeadlineExceeded
                }
                crate::replan::ReplanStopReason::BudgetExhausted => {
                    ExecutionTerminalReason::BudgetExhausted
                }
            }
        }
        ExecutionTerminalCause::LimitStop { reason } => {
            if !matches!(
                projection,
                TerminalProjection::Partial { .. } | TerminalProjection::Failed { .. }
            ) {
                return invalid_terminal_combination(cause, projection);
            }
            terminal_reason_from_limit(*reason)
        }
        ExecutionTerminalCause::SchedulerNoProgress => match projection {
            TerminalProjection::Unsupported { .. } => ExecutionTerminalReason::UnsupportedPlan,
            TerminalProjection::Partial { .. }
            | TerminalProjection::Blocked { .. }
            | TerminalProjection::Failed { .. } => ExecutionTerminalReason::NoProgress,
            TerminalProjection::Completed { .. } | TerminalProjection::Cancelled { .. } => {
                return invalid_terminal_combination(cause, projection);
            }
        },
        ExecutionTerminalCause::TaskFailure { class } => match projection {
            TerminalProjection::Unsupported { .. } => ExecutionTerminalReason::UnsupportedPlan,
            TerminalProjection::Blocked { .. } => ExecutionTerminalReason::Blocked,
            TerminalProjection::Partial { .. } => match class {
                ExecutionFailureClass::DeadlineExceeded => {
                    ExecutionTerminalReason::DeadlineExceeded
                }
                ExecutionFailureClass::BudgetExceeded => ExecutionTerminalReason::BudgetExceeded,
                ExecutionFailureClass::Retryable
                | ExecutionFailureClass::DependencyFailed
                | ExecutionFailureClass::InvalidInput
                | ExecutionFailureClass::InvalidOutput
                | ExecutionFailureClass::AuthorizationDenied
                | ExecutionFailureClass::Cancelled
                | ExecutionFailureClass::Unsupported
                | ExecutionFailureClass::Terminal => ExecutionTerminalReason::GoalIncomplete,
            },
            TerminalProjection::Failed { .. } => match class {
                ExecutionFailureClass::DeadlineExceeded => {
                    ExecutionTerminalReason::DeadlineExceeded
                }
                ExecutionFailureClass::BudgetExceeded => ExecutionTerminalReason::BudgetExceeded,
                ExecutionFailureClass::Retryable
                | ExecutionFailureClass::DependencyFailed
                | ExecutionFailureClass::InvalidInput
                | ExecutionFailureClass::InvalidOutput
                | ExecutionFailureClass::AuthorizationDenied
                | ExecutionFailureClass::Cancelled
                | ExecutionFailureClass::Unsupported
                | ExecutionFailureClass::Terminal => ExecutionTerminalReason::TaskFailure,
            },
            TerminalProjection::Completed { .. } | TerminalProjection::Cancelled { .. } => {
                return invalid_terminal_combination(cause, projection);
            }
        },
        ExecutionTerminalCause::Completion { limit_stop } => {
            if *limit_stop != evaluation.limit_stop {
                return Err(Error::InvalidRepositoryInput {
                    message: "completion terminal cause and evaluation limit stop disagree"
                        .to_string(),
                });
            }
            match projection {
                TerminalProjection::Completed { .. } => ExecutionTerminalReason::Completed,
                TerminalProjection::Blocked { .. } => ExecutionTerminalReason::Blocked,
                TerminalProjection::Unsupported { .. } => ExecutionTerminalReason::UnsupportedPlan,
                TerminalProjection::Partial { .. } | TerminalProjection::Failed { .. } => {
                    limit_stop.map_or(ExecutionTerminalReason::GoalIncomplete, |reason| {
                        terminal_reason_from_limit(reason)
                    })
                }
                TerminalProjection::Cancelled { .. } => {
                    return invalid_terminal_combination(cause, projection);
                }
            }
        }
    };
    Ok(reason)
}

fn completion_status_from_projection(projection: &TerminalProjection) -> Option<CompletionStatus> {
    match projection {
        TerminalProjection::Completed { .. } => Some(CompletionStatus::Completed),
        TerminalProjection::Partial { .. } => Some(CompletionStatus::Partial),
        TerminalProjection::Blocked { .. } => Some(CompletionStatus::Blocked),
        TerminalProjection::Unsupported { .. } => Some(CompletionStatus::Unsupported),
        TerminalProjection::Failed { .. } => Some(CompletionStatus::Failed),
        TerminalProjection::Cancelled { .. } => None,
    }
}

const fn terminal_reason_from_limit(reason: ExecutionLimitStop) -> ExecutionTerminalReason {
    match reason {
        ExecutionLimitStop::DeadlineExceeded => ExecutionTerminalReason::DeadlineExceeded,
        ExecutionLimitStop::BudgetExceeded => ExecutionTerminalReason::BudgetExceeded,
    }
}

fn invalid_terminal_combination<T>(
    cause: &ExecutionTerminalCause,
    projection: &TerminalProjection,
) -> Result<T> {
    Err(Error::InvalidRepositoryInput {
        message: format!("terminal cause {cause:?} is invalid for projection {projection:?}"),
    })
}

/// Counts cancellation coverage only from already completed task outcomes.
pub fn cancellation_terminal_evidence(
    goal: &ExecutionGoalContract,
    plan: &CanonicalExecutionPlan,
    projection: &ExecutionProjection,
) -> Result<ExecutionTerminalEvidence> {
    let declared_requirement_ids = goal
        .requirements
        .iter()
        .map(|requirement| requirement.id.as_str())
        .collect::<BTreeSet<_>>();
    let completed_node_ids = projection
        .tasks
        .iter()
        .filter(|task| {
            task.status == ExecutionTaskStatus::Completed
                && task.outcome.as_ref().is_some_and(|outcome| {
                    matches!(outcome.result, ExecutionTaskResult::Completed { .. })
                })
        })
        .map(|task| task.node_id.as_str())
        .collect::<BTreeSet<_>>();
    let evidenced_requirement_ids = plan
        .definition
        .nodes
        .iter()
        .filter(|node| completed_node_ids.contains(node.id.as_str()))
        .flat_map(|node| node.requirement_ids.iter().map(String::as_str))
        .filter(|requirement_id| declared_requirement_ids.contains(requirement_id))
        .collect::<BTreeSet<_>>();
    Ok(ExecutionTerminalEvidence {
        cause: ExecutionTerminalCause::Cancellation,
        satisfied_requirement_count: u64::try_from(evidenced_requirement_ids.len()).map_err(
            |_| Error::ArithmeticOverflow {
                context: "cancelled execution satisfied requirement count".to_string(),
            },
        )?,
        requirement_count: u64::try_from(declared_requirement_ids.len()).map_err(|_| {
            Error::ArithmeticOverflow {
                context: "cancelled execution total requirement count".to_string(),
            }
        })?,
    })
}

/// Evaluates every deterministic and persisted-verifier completion gate.
pub fn evaluate_completion(request: CompletionEvaluationRequest) -> Result<CompletionEvaluation> {
    let outputs = node_outputs(&request.plan, &request.projection)?;
    let mut checks = Vec::with_capacity(request.goal.completion_checks.len());
    for check in &request.goal.completion_checks {
        checks.push(evaluate_check(check, &request, &outputs)?);
    }

    let coverage = evaluate_all_coverage(&request, &outputs)?;
    let mut coverage_by_node = BTreeMap::new();
    for result in &coverage {
        coverage_by_node
            .entry(result.map_node_id.as_str())
            .and_modify(|passed| *passed &= result.passed)
            .or_insert(result.passed);
    }
    let (mut satisfied, mut unsatisfied) = evaluate_requirements(&request, &coverage_by_node);
    satisfied.sort();
    unsatisfied.sort();

    let mut gaps = Vec::new();
    for check in &checks {
        if !check.passed {
            gaps.push(format!("completion check {} failed", check.check_id));
        }
    }
    for requirement_id in &unsatisfied {
        gaps.push(format!("requirement {requirement_id} is unsatisfied"));
    }
    for result in &coverage {
        if !result.passed {
            gaps.push(format!("coverage {} failed", result.coverage_id));
        }
    }

    let mut deliverables_pass = true;
    for deliverable in &request.goal.deliverables {
        let Some(output) = request.terminal_output.as_ref() else {
            deliverables_pass = false;
            gaps.push(format!(
                "deliverable {} has no terminal output",
                deliverable.id
            ));
            continue;
        };
        let Some(value) = output.pointer(&deliverable.output_pointer) else {
            deliverables_pass = false;
            gaps.push(format!("deliverable {} is missing", deliverable.id));
            continue;
        };
        if validate_instance(
            &deliverable.schema,
            value,
            &format!("goal.deliverables.{}", deliverable.id),
        )
        .is_err()
        {
            deliverables_pass = false;
            gaps.push(format!("deliverable {} has invalid schema", deliverable.id));
        }
    }

    let explicit_checks_pass = checks.iter().all(|check| check.passed);
    let coverage_pass = coverage.iter().all(|result| result.passed);
    let requirements_pass = unsatisfied.is_empty();
    let terminal_schemas_pass = request.terminal_output.as_ref().is_some_and(|output| {
        validate_instance(
            &request.plan.definition.output_schema,
            output,
            "plan.output",
        )
        .is_ok()
            && output_node(&request.plan).is_some_and(|node| {
                validate_instance(&node.output_schema, output, "output_node.output").is_ok()
            })
    });
    if !terminal_schemas_pass {
        gaps.push("terminal output is missing or violates its declared schemas".to_string());
    }
    let constraints_pass = request.goal.constraints.iter().all(|constraint| {
        request
            .goal
            .completion_checks
            .iter()
            .enumerate()
            .filter(|(_, check)| check.constraint_ids.contains(&constraint.id))
            .all(|(index, _)| checks.get(index).is_some_and(|result| result.passed))
    });
    if !constraints_pass {
        gaps.push("one or more constraint-linked checks failed".to_string());
    }

    let deadline_exceeded = request
        .budget_ledger
        .limit
        .deadline_at
        .is_some_and(|deadline| request.now > deadline);
    if request.budget_ledger.overrun {
        gaps.push("execution budget overrun".to_string());
    }
    if deadline_exceeded {
        gaps.push("execution deadline exceeded".to_string());
    }
    gaps.sort();
    gaps.dedup();

    let all_pass = explicit_checks_pass
        && coverage_pass
        && requirements_pass
        && deliverables_pass
        && constraints_pass
        && terminal_schemas_pass;
    let useful = request.terminal_output.is_some() || !satisfied.is_empty();
    let limit_stop = if deadline_exceeded {
        Some(ExecutionLimitStop::DeadlineExceeded)
    } else if request.budget_ledger.overrun {
        Some(ExecutionLimitStop::BudgetExceeded)
    } else {
        None
    };
    let status = if all_pass && limit_stop.is_none() {
        CompletionStatus::Completed
    } else if is_blocked(&request) {
        CompletionStatus::Blocked
    } else if has_fully_unsupported_requirement(&request, &unsatisfied) {
        CompletionStatus::Unsupported
    } else if limit_stop.is_some() {
        if useful {
            CompletionStatus::Partial
        } else {
            CompletionStatus::Failed
        }
    } else if useful {
        CompletionStatus::Partial
    } else {
        CompletionStatus::Failed
    };

    Ok(CompletionEvaluation {
        status,
        limit_stop,
        checks,
        satisfied_requirement_ids: satisfied,
        unsatisfied_requirement_ids: unsatisfied,
        gaps,
    })
}

fn evaluate_check(
    check: &CompletionCheck,
    request: &CompletionEvaluationRequest,
    outputs: &BTreeMap<String, Value>,
) -> Result<CompletionCheckResult> {
    let (passed, evidence) = match &check.kind {
        CompletionCheckKind::OutputSchema => {
            let passed = request.terminal_output.as_ref().is_some_and(|output| {
                validate_instance(
                    &request.plan.definition.output_schema,
                    output,
                    "plan.output",
                )
                .is_ok()
                    && output_node(&request.plan).is_some_and(|node| {
                        validate_instance(&node.output_schema, output, "output_node.output").is_ok()
                    })
            });
            (
                passed,
                json!({ "terminal_output_present": request.terminal_output.is_some() }),
            )
        }
        CompletionCheckKind::RequiredNodes { node_ids } => {
            let failed = node_ids
                .iter()
                .filter(|id| {
                    request.projection.node_statuses.get(*id)
                        != Some(&ExecutionNodeStatus::Completed)
                        || request
                            .projection
                            .tasks
                            .iter()
                            .filter(|task| task.node_id == id.as_str())
                            .filter(|task| task.status != ExecutionTaskStatus::Skipped)
                            .any(|task| task.status != ExecutionTaskStatus::Completed)
                })
                .cloned()
                .collect::<Vec<_>>();
            (failed.is_empty(), json!({ "incomplete_node_ids": failed }))
        }
        CompletionCheckKind::MapCoverage { map_node_id } => {
            let matching = request
                .goal
                .coverage
                .iter()
                .filter(|coverage| coverage.map_node_id == *map_node_id)
                .map(|coverage| evaluate_coverage(coverage, request, outputs))
                .collect::<Result<Vec<_>>>()?;
            let passed = !matching.is_empty() && matching.iter().all(|result| result.passed);
            (passed, serde_json::to_value(matching)?)
        }
        CompletionCheckKind::Citations {
            node_ids,
            min_per_task,
        } => {
            let mut failures = Vec::new();
            for node_id in node_ids {
                let tasks = request
                    .projection
                    .tasks
                    .iter()
                    .filter(|task| {
                        task.node_id == *node_id && task.status != ExecutionTaskStatus::Skipped
                    })
                    .collect::<Vec<_>>();
                if tasks.is_empty() {
                    failures.push(json!({ "node_id": node_id, "item_key": null, "count": 0 }));
                    continue;
                }
                for task in tasks {
                    let count = task
                        .outcome
                        .as_ref()
                        .and_then(|outcome| match &outcome.result {
                            ExecutionTaskResult::Completed { citations, .. } => Some(
                                citations
                                    .iter()
                                    .filter(|citation| !citation.source_id.trim().is_empty())
                                    .count(),
                            ),
                            _ => None,
                        })
                        .unwrap_or(0);
                    if count < usize::try_from(*min_per_task).unwrap_or(usize::MAX) {
                        failures.push(json!({
                            "node_id": task.node_id,
                            "item_key": task.item_key,
                            "count": count,
                        }));
                    }
                }
            }
            (
                failures.is_empty(),
                json!({ "insufficient_tasks": failures }),
            )
        }
        CompletionCheckKind::AgentVerifier { .. } => {
            let node_id = format!("@check/{}", check.id);
            let verifier = request
                .projection
                .tasks
                .iter()
                .find(|task| {
                    task.node_id == node_id && task.status == ExecutionTaskStatus::Completed
                })
                .and_then(completed_output);
            let Some(Value::Object(object)) = verifier else {
                return Ok(check_result(check, false, json!({ "verdict": "missing" })));
            };
            let passed = object.len() == 2
                && object.get("passed").and_then(Value::as_bool).is_some()
                && object.contains_key("evidence");
            let verdict = object
                .get("passed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (
                passed && verdict,
                json!({
                    "verdict": verdict,
                    "evidence": object.get("evidence").cloned().unwrap_or(Value::Null),
                    "valid_shape": passed,
                }),
            )
        }
    };
    Ok(check_result(check, passed, evidence))
}

fn check_result(check: &CompletionCheck, passed: bool, evidence: Value) -> CompletionCheckResult {
    CompletionCheckResult {
        check_id: check.id.clone(),
        passed,
        evidence,
    }
}

#[derive(Clone, Debug, Serialize)]
struct CoverageEvaluation {
    coverage_id: String,
    map_node_id: String,
    passed: bool,
    missing_keys: Vec<String>,
    extra_keys: Vec<String>,
    failed_keys: Vec<String>,
    completed_keys: Vec<String>,
}

fn evaluate_all_coverage(
    request: &CompletionEvaluationRequest,
    outputs: &BTreeMap<String, Value>,
) -> Result<Vec<CoverageEvaluation>> {
    request
        .goal
        .coverage
        .iter()
        .map(|coverage| evaluate_coverage(coverage, request, outputs))
        .collect()
}

fn evaluate_coverage(
    coverage: &moa_artifacts::execution_plan::CoverageRequirement,
    request: &CompletionEvaluationRequest,
    outputs: &BTreeMap<String, Value>,
) -> Result<CoverageEvaluation> {
    let node = request
        .plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == coverage.map_node_id)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!("coverage {} references a missing map node", coverage.id),
        })?;
    let ExecutionOperation::Map { item_key, .. } = &node.operation else {
        return Err(Error::InvalidProjection {
            message: format!("coverage {} does not reference a map node", coverage.id),
        });
    };
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let expected = resolve_bindings(
        &coverage.expected_items,
        &BindingContext {
            run_input: &request.run_input,
            node_outputs: outputs,
            dependencies: &dependencies,
            item: None,
            item_key: None,
        },
    )?;
    let expected = expected
        .as_array()
        .ok_or_else(|| Error::InvalidProjection {
            message: format!(
                "coverage {} expected_items did not resolve to an array",
                coverage.id
            ),
        })?;
    let mut expected_keys = BTreeSet::new();
    for item in expected {
        let key = extract_map_key(item, item_key)?;
        if !expected_keys.insert(key) {
            return Err(Error::InvalidProjection {
                message: format!("coverage {} contains duplicate expected keys", coverage.id),
            });
        }
    }

    let mut completed_keys = BTreeSet::new();
    let mut failed_keys = BTreeSet::new();
    for task in request
        .projection
        .tasks
        .iter()
        .filter(|task| task.node_id == coverage.map_node_id)
    {
        match task.status {
            ExecutionTaskStatus::Completed => {
                completed_keys.insert(task.item_key.clone());
            }
            ExecutionTaskStatus::Failed | ExecutionTaskStatus::Cancelled => {
                failed_keys.insert(task.item_key.clone());
            }
            ExecutionTaskStatus::Pending
            | ExecutionTaskStatus::Reserved
            | ExecutionTaskStatus::Running
            | ExecutionTaskStatus::WaitingInput
            | ExecutionTaskStatus::WaitingReplan
            | ExecutionTaskStatus::Skipped => {}
        }
    }
    let observed = completed_keys
        .union(&failed_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing = expected_keys
        .difference(&observed)
        .cloned()
        .collect::<Vec<_>>();
    let extra = observed
        .difference(&expected_keys)
        .cloned()
        .collect::<Vec<_>>();
    let failed = failed_keys.iter().cloned().collect::<Vec<_>>();
    let completed = completed_keys.iter().cloned().collect::<Vec<_>>();
    let passed = extra.is_empty()
        && failed.is_empty()
        && if coverage.require_all {
            missing.is_empty()
        } else {
            expected_keys.is_empty() || completed_keys.intersection(&expected_keys).next().is_some()
        };
    Ok(CoverageEvaluation {
        coverage_id: coverage.id.clone(),
        map_node_id: coverage.map_node_id.clone(),
        passed,
        missing_keys: missing,
        extra_keys: extra,
        failed_keys: failed,
        completed_keys: completed,
    })
}

fn evaluate_requirements(
    request: &CompletionEvaluationRequest,
    coverage_by_node: &BTreeMap<&str, bool>,
) -> (Vec<String>, Vec<String>) {
    let mut satisfied = Vec::new();
    let mut unsatisfied = Vec::new();
    for requirement in &request.goal.requirements {
        let declaring = request
            .plan
            .definition
            .nodes
            .iter()
            .filter(|node| node.requirement_ids.contains(&requirement.id))
            .filter(|node| {
                request.projection.node_statuses.get(&node.id)
                    != Some(&ExecutionNodeStatus::Skipped)
            })
            .collect::<Vec<_>>();
        let passed = !declaring.is_empty()
            && declaring.iter().all(|node| {
                request.projection.node_statuses.get(&node.id)
                    == Some(&ExecutionNodeStatus::Completed)
                    && request
                        .projection
                        .tasks
                        .iter()
                        .filter(|task| task.node_id == node.id)
                        .filter(|task| task.status != ExecutionTaskStatus::Skipped)
                        .all(|task| task.status == ExecutionTaskStatus::Completed)
                    && (!matches!(node.operation, ExecutionOperation::Map { .. })
                        || coverage_by_node
                            .get(node.id.as_str())
                            .copied()
                            .unwrap_or(true))
            });
        if passed {
            satisfied.push(requirement.id.clone());
        } else {
            unsatisfied.push(requirement.id.clone());
        }
    }
    (satisfied, unsatisfied)
}

fn is_blocked(request: &CompletionEvaluationRequest) -> bool {
    request.projection.tasks.iter().any(|task| {
        task.status == ExecutionTaskStatus::WaitingInput
            || task.outcome.as_ref().is_some_and(|outcome| {
                matches!(
                    outcome.result,
                    ExecutionTaskResult::Failed {
                        class: ExecutionFailureClass::AuthorizationDenied,
                        ..
                    }
                )
            })
    }) || request.plan.definition.nodes.iter().any(|node| {
        request.projection.node_statuses.get(&node.id) == Some(&ExecutionNodeStatus::Waiting)
            && matches!(
                node.operation,
                ExecutionOperation::Review { .. } | ExecutionOperation::WaitSignal { .. }
            )
    })
}

fn has_fully_unsupported_requirement(
    request: &CompletionEvaluationRequest,
    unsatisfied: &[String],
) -> bool {
    unsatisfied.iter().any(|requirement_id| {
        let paths = request
            .plan
            .definition
            .nodes
            .iter()
            .filter(|node| node.requirement_ids.contains(requirement_id))
            .filter(|node| {
                request.projection.node_statuses.get(&node.id)
                    != Some(&ExecutionNodeStatus::Skipped)
            })
            .collect::<Vec<_>>();
        !paths.is_empty()
            && paths.iter().all(|node| {
                let tasks = request
                    .projection
                    .tasks
                    .iter()
                    .filter(|task| task.node_id == node.id)
                    .collect::<Vec<_>>();
                !tasks.is_empty()
                    && tasks.iter().all(|task| {
                        task.outcome.as_ref().is_some_and(|outcome| {
                            matches!(
                                outcome.result,
                                ExecutionTaskResult::Failed {
                                    class: ExecutionFailureClass::Unsupported,
                                    ..
                                }
                            )
                        })
                    })
            })
    })
}

fn output_node(
    plan: &CanonicalExecutionPlan,
) -> Option<&moa_artifacts::execution_plan::ExecutionNode> {
    plan.definition
        .nodes
        .iter()
        .find(|node| matches!(node.operation, ExecutionOperation::Output { .. }))
}

pub(crate) fn node_outputs(
    plan: &CanonicalExecutionPlan,
    projection: &ExecutionProjection,
) -> Result<BTreeMap<String, Value>> {
    let mut outputs = BTreeMap::new();
    for node in &plan.definition.nodes {
        if matches!(node.operation, ExecutionOperation::Map { .. }) {
            if projection.node_statuses.get(&node.id) != Some(&ExecutionNodeStatus::Completed) {
                continue;
            }
            let aggregate = map_output(node, projection)?;
            let value = serde_json::to_value(aggregate)?;
            validate_instance(
                &node.output_schema,
                &value,
                &format!("node.{}.output", node.id),
            )?;
            outputs.insert(node.id.clone(), value);
            continue;
        }
        if let Some(output) = projection
            .tasks
            .iter()
            .filter(|task| task.node_id == node.id && task.item_key.is_empty())
            .find_map(completed_output)
        {
            validate_instance(
                &node.output_schema,
                &output,
                &format!("node.{}.output", node.id),
            )?;
            outputs.insert(node.id.clone(), output);
        }
    }
    Ok(outputs)
}

pub(crate) fn map_output(
    node: &moa_artifacts::execution_plan::ExecutionNode,
    projection: &ExecutionProjection,
) -> Result<ExecutionMapOutput> {
    let ExecutionOperation::Map {
        item_output_schema, ..
    } = &node.operation
    else {
        return Err(Error::InvalidProjection {
            message: format!("node {} is not a map", node.id),
        });
    };
    let mut items = projection
        .tasks
        .iter()
        .filter(|task| task.node_id == node.id)
        .filter_map(|task| map_item(node, task, item_output_schema).transpose())
        .collect::<Result<Vec<_>>>()?;
    items.sort_by(|left, right| left.item_key.cmp(&right.item_key));
    Ok(ExecutionMapOutput { items })
}

fn map_item(
    node: &moa_artifacts::execution_plan::ExecutionNode,
    task: &ExecutionTaskProjection,
    item_schema: &Value,
) -> Result<Option<ExecutionMapItem>> {
    if task.status == ExecutionTaskStatus::Skipped {
        return Ok(Some(ExecutionMapItem {
            item_key: task.item_key.clone(),
            status: ExecutionMapItemStatus::Skipped,
            output: None,
            failure: None,
            usage: zero_usage(),
            citations: Vec::new(),
        }));
    }
    let Some(outcome) = &task.outcome else {
        return Ok(None);
    };
    let item = match &outcome.result {
        ExecutionTaskResult::Completed { output, citations } => {
            validate_instance(item_schema, output, "map.item_output")?;
            ExecutionMapItem {
                item_key: task.item_key.clone(),
                status: ExecutionMapItemStatus::Completed,
                output: Some(output.clone()),
                failure: None,
                usage: outcome.usage.clone(),
                citations: citations.clone(),
            }
        }
        ExecutionTaskResult::Failed { class, message } => ExecutionMapItem {
            item_key: task.item_key.clone(),
            status: ExecutionMapItemStatus::Failed,
            output: None,
            failure: Some(ExecutionTaskFailure {
                class: class.clone(),
                message: message.clone(),
                capability_ref: map_capability(node),
            }),
            usage: outcome.usage.clone(),
            citations: Vec::new(),
        },
        ExecutionTaskResult::Cancelled { reason } => ExecutionMapItem {
            item_key: task.item_key.clone(),
            status: ExecutionMapItemStatus::Cancelled,
            output: None,
            failure: Some(ExecutionTaskFailure {
                class: ExecutionFailureClass::Cancelled,
                message: reason.clone(),
                capability_ref: None,
            }),
            usage: outcome.usage.clone(),
            citations: Vec::new(),
        },
        ExecutionTaskResult::UnknownOutcome { message } => ExecutionMapItem {
            item_key: task.item_key.clone(),
            status: ExecutionMapItemStatus::Failed,
            output: None,
            failure: Some(ExecutionTaskFailure {
                class: ExecutionFailureClass::Terminal,
                message: message.clone(),
                capability_ref: map_capability(node),
            }),
            usage: outcome.usage.clone(),
            citations: Vec::new(),
        },
        ExecutionTaskResult::NeedsInput { .. } | ExecutionTaskResult::NeedsReplan { .. } => {
            return Ok(None);
        }
    };
    Ok(Some(item))
}

fn map_capability(
    node: &moa_artifacts::execution_plan::ExecutionNode,
) -> Option<moa_artifacts::execution_plan::CapabilityReference> {
    match &node.operation {
        ExecutionOperation::Map {
            task: moa_artifacts::execution_plan::MapTask::Capability { reference },
            ..
        } => Some(reference.clone()),
        ExecutionOperation::Map { .. }
        | ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Reduce { .. }
        | ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => None,
    }
}

pub(crate) fn completed_output(task: &ExecutionTaskProjection) -> Option<Value> {
    task.outcome
        .as_ref()
        .and_then(|outcome| match &outcome.result {
            ExecutionTaskResult::Completed { output, .. } => Some(output.clone()),
            ExecutionTaskResult::NeedsInput { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Cancelled { .. }
            | ExecutionTaskResult::UnknownOutcome { .. }
            | ExecutionTaskResult::Failed { .. } => None,
        })
}

const fn zero_usage() -> moa_artifacts::execution_plan::ExecutionUsage {
    moa_artifacts::execution_plan::ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluation(
        status: CompletionStatus,
        limit_stop: Option<ExecutionLimitStop>,
    ) -> CompletionEvaluation {
        CompletionEvaluation {
            status,
            limit_stop,
            checks: Vec::new(),
            satisfied_requirement_ids: Vec::new(),
            unsatisfied_requirement_ids: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn failed_projection() -> TerminalProjection {
        TerminalProjection::Failed {
            failure: ExecutionTaskFailure {
                class: ExecutionFailureClass::Terminal,
                message: "failed".to_string(),
                capability_ref: None,
            },
        }
    }

    #[test]
    fn completion_projection_conversion_is_strict_and_preserves_overrides() {
        // Pins: every scheduler and workflow uses one conversion, completed output cannot be
        // invented, and interpreter-specific failure evidence remains intact.
        assert!(
            terminal_projection_from_evaluation(
                &evaluation(CompletionStatus::Completed, None),
                None,
                None,
                None,
                None,
            )
            .is_err()
        );
        let failure = ExecutionTaskFailure {
            class: ExecutionFailureClass::InvalidOutput,
            message: "invalid result".to_string(),
            capability_ref: None,
        };
        assert_eq!(
            terminal_projection_from_evaluation(
                &evaluation(CompletionStatus::Failed, None),
                None,
                Some("diagnostic gap".to_string()),
                Some(failure.clone()),
                None,
            )
            .expect("failed projection"),
            TerminalProjection::Failed { failure }
        );
    }

    #[test]
    fn terminal_reason_preserves_partial_and_failed_limit_precedence() {
        // Pins: deadline wins over budget before terminal reason selection, and the same
        // typed limit maps identically for partial and failed projections.
        for (projection, status) in [
            (
                TerminalProjection::Partial {
                    output: Some(Value::Null),
                    gaps: Vec::new(),
                },
                CompletionStatus::Partial,
            ),
            (failed_projection(), CompletionStatus::Failed),
        ] {
            let evaluation = evaluation(status, Some(ExecutionLimitStop::DeadlineExceeded));
            assert_eq!(
                execution_terminal_reason(
                    &ExecutionTerminalCause::Completion {
                        limit_stop: Some(ExecutionLimitStop::DeadlineExceeded),
                    },
                    &projection,
                    &evaluation,
                )
                .expect("typed deadline reason should be valid"),
                ExecutionTerminalReason::DeadlineExceeded
            );
        }
    }

    #[test]
    fn ordinary_failed_completion_is_goal_incomplete() {
        // Pins: Completion without a typed limit never falls back to task-failure inference.
        assert_eq!(
            execution_terminal_reason(
                &ExecutionTerminalCause::Completion { limit_stop: None },
                &failed_projection(),
                &evaluation(CompletionStatus::Failed, None),
            )
            .expect("ordinary failed completion should be valid"),
            ExecutionTerminalReason::GoalIncomplete
        );
    }

    #[test]
    fn task_failure_mapping_is_projection_aware() {
        // Pins: the original task failure class remains diagnostic evidence while the
        // normalized reason follows the exact terminal projection matrix.
        let partial = TerminalProjection::Partial {
            output: Some(Value::Null),
            gaps: Vec::new(),
        };
        assert_eq!(
            execution_terminal_reason(
                &ExecutionTerminalCause::TaskFailure {
                    class: ExecutionFailureClass::InvalidOutput,
                },
                &partial,
                &evaluation(CompletionStatus::Partial, None),
            )
            .expect("partial task failure should be valid"),
            ExecutionTerminalReason::GoalIncomplete
        );
        assert_eq!(
            execution_terminal_reason(
                &ExecutionTerminalCause::TaskFailure {
                    class: ExecutionFailureClass::InvalidOutput,
                },
                &failed_projection(),
                &evaluation(CompletionStatus::Failed, None),
            )
            .expect("failed task failure should be valid"),
            ExecutionTerminalReason::TaskFailure
        );
    }

    #[test]
    fn invalid_terminal_cause_projection_pair_is_rejected() {
        // Pins: terminal reasons are never inferred from status or prose after typed
        // cause/projection validation fails.
        let error = execution_terminal_reason(
            &ExecutionTerminalCause::Cancellation,
            &failed_projection(),
            &evaluation(CompletionStatus::Failed, None),
        )
        .expect_err("cancellation cannot finalize a failed projection");
        assert!(matches!(error, Error::InvalidRepositoryInput { .. }));
    }
}

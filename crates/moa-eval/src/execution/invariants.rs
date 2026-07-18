//! Deterministic predicates over redacted execution evaluation snapshots.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::CapabilityReference;
use moa_execution::state::{ExecutionRunStatus, ExecutionTaskStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::snapshot::{ExecutionEvalSnapshot, total_accounted_resources};

const MAX_KEY_SAMPLE: usize = 25;

/// One deterministic assertion over an execution snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionInvariantSpec {
    /// Require the run status to be one of the listed statuses.
    TerminalStatusIn {
        /// Allowed durable run statuses.
        statuses: Vec<ExecutionRunStatus>,
    },
    /// Declare that this case must never report completed.
    MustNotComplete,
    /// Require an exact task count for one plan node.
    TaskCount {
        /// Stable plan node identifier.
        node_id: String,
        /// Exact expected task count.
        exact: u64,
    },
    /// Require an independent expected item universe for one map node.
    MapCoverage {
        /// Stable map node identifier.
        node_id: String,
        /// Independently supplied expected item keys.
        expected_keys: Vec<String>,
        /// Make complete status conditional on exact successful coverage.
        require_all_when_completed: bool,
    },
    /// Require one persisted completion check to pass.
    CompletionCheckPassed {
        /// Stable completion-check identifier.
        check_id: String,
    },
    /// Require one persisted completion check to fail.
    CompletionCheckFailed {
        /// Stable completion-check identifier.
        check_id: String,
    },
    /// Require one terminal gap to contain the supplied free-text fragment.
    TerminalGapContains {
        /// Expected free-text fragment.
        text: String,
    },
    /// Require approved, reserved, and consumed budget accounting to agree.
    BudgetWithinApproved,
    /// Require persisted progress counters to match task rows exactly.
    ProgressMatchesTasks,
    /// Require at most one non-replayed effect per logical invocation ID.
    NoDuplicateLogicalEffects,
    /// Require every observed capability call to stay inside the allowlist.
    AllowedCapabilitiesOnly {
        /// Exact capability references allowed for this case.
        references: Vec<CapabilityReference>,
    },
    /// Require previously completed task keys to remain completed.
    CompletedTaskKeysPreserved {
        /// Stable plan node identifier.
        node_id: String,
        /// Stable item keys that must remain completed.
        item_keys: Vec<String>,
    },
    /// Bound one stable session-event category.
    SessionEventCountAtMost {
        /// Stable event category label.
        event_kind: String,
        /// Maximum allowed count.
        max: u64,
    },
    /// Require zero session events containing raw task outputs.
    NoRawTaskOutputEvents,
}

/// Stable result of evaluating one execution invariant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInvariantResult {
    /// Stable invariant identity, including scoped node or check identifiers.
    pub invariant_id: String,
    /// Whether the assertion passed for the observed run.
    pub passed: bool,
    /// Bounded structured expected value.
    pub expected: Value,
    /// Bounded structured observed value.
    pub observed: Value,
    /// Short deterministic diagnostic.
    pub diagnostic: String,
    /// Underlying successful-completion guard, when this invariant defines one.
    pub completion_guard_passed: Option<bool>,
}

impl ExecutionInvariantSpec {
    /// Evaluates this invariant against one redacted snapshot.
    #[must_use]
    pub fn evaluate(&self, snapshot: &ExecutionEvalSnapshot) -> ExecutionInvariantResult {
        match self {
            Self::TerminalStatusIn { statuses } => {
                let passed = statuses.contains(&snapshot.run.status);
                result(
                    "terminal_status_in",
                    passed,
                    json!({ "statuses": statuses }),
                    json!({ "status": snapshot.run.status }),
                    if passed {
                        "run status is allowed"
                    } else {
                        "run status is outside the allowed set"
                    },
                    None,
                )
            }
            Self::MustNotComplete => {
                let passed = snapshot.run.status != ExecutionRunStatus::Completed;
                result(
                    "must_not_complete",
                    passed,
                    json!({ "completed": false }),
                    json!({ "status": snapshot.run.status }),
                    if passed {
                        "deliberately impossible case did not complete"
                    } else {
                        "deliberately impossible case reported completed"
                    },
                    Some(false),
                )
            }
            Self::TaskCount { node_id, exact } => {
                let observed = snapshot
                    .tasks
                    .iter()
                    .filter(|task| task.node_id == *node_id)
                    .count();
                let observed = u64::try_from(observed).unwrap_or(u64::MAX);
                let passed = observed == *exact;
                result(
                    &format!("task_count:{node_id}"),
                    passed,
                    json!({ "node_id": node_id, "exact": exact }),
                    json!({ "node_id": node_id, "count": observed }),
                    if passed {
                        "node task count matches"
                    } else {
                        "node task count differs"
                    },
                    None,
                )
            }
            Self::MapCoverage {
                node_id,
                expected_keys,
                require_all_when_completed,
            } => evaluate_map_coverage(
                snapshot,
                node_id,
                expected_keys,
                *require_all_when_completed,
            ),
            Self::CompletionCheckPassed { check_id } => {
                evaluate_completion_check(snapshot, check_id, true)
            }
            Self::CompletionCheckFailed { check_id } => {
                evaluate_completion_check(snapshot, check_id, false)
            }
            Self::TerminalGapContains { text } => {
                let matching = snapshot
                    .run
                    .terminal_gaps
                    .iter()
                    .position(|gap| gap.contains(text));
                let passed = matching.is_some();
                result(
                    &format!("terminal_gap_contains:{}", stable_label(text)),
                    passed,
                    json!({ "text": text }),
                    json!({
                        "gap_count": snapshot.run.terminal_gaps.len(),
                        "matching_gap_index": matching,
                    }),
                    if passed {
                        "terminal gap contains the expected text"
                    } else {
                        "no terminal gap contains the expected text"
                    },
                    None,
                )
            }
            Self::BudgetWithinApproved => evaluate_budget(snapshot),
            Self::ProgressMatchesTasks => evaluate_progress(snapshot),
            Self::NoDuplicateLogicalEffects => evaluate_logical_effects(snapshot),
            Self::AllowedCapabilitiesOnly { references } => {
                evaluate_capability_allowlist(snapshot, references)
            }
            Self::CompletedTaskKeysPreserved { node_id, item_keys } => {
                evaluate_completed_keys(snapshot, node_id, item_keys)
            }
            Self::SessionEventCountAtMost { event_kind, max } => {
                let observed = snapshot.harness.session_events.count(event_kind);
                let passed = observed.is_some_and(|count| count <= *max);
                result(
                    &format!("session_event_count_at_most:{event_kind}"),
                    passed,
                    json!({ "event_kind": event_kind, "max": max }),
                    json!({ "count": observed }),
                    if observed.is_none() {
                        "event category is not recognized"
                    } else if passed {
                        "session event count is within the bound"
                    } else {
                        "session event count exceeds the bound"
                    },
                    None,
                )
            }
            Self::NoRawTaskOutputEvents => {
                let observed = snapshot.harness.session_events.raw_task_output;
                let passed = observed == 0;
                result(
                    "no_raw_task_output_events",
                    passed,
                    json!({ "count": 0 }),
                    json!({ "count": observed }),
                    if passed {
                        "session events contain no raw task outputs"
                    } else {
                        "session events exposed raw task outputs"
                    },
                    None,
                )
            }
        }
    }
}

/// Evaluates an ordered list of deterministic invariant specifications.
#[must_use]
pub fn evaluate_invariants(
    snapshot: &ExecutionEvalSnapshot,
    specs: &[ExecutionInvariantSpec],
) -> Vec<ExecutionInvariantResult> {
    specs.iter().map(|spec| spec.evaluate(snapshot)).collect()
}

fn evaluate_map_coverage(
    snapshot: &ExecutionEvalSnapshot,
    node_id: &str,
    expected_keys: &[String],
    require_all_when_completed: bool,
) -> ExecutionInvariantResult {
    let expected = expected_keys.iter().cloned().collect::<BTreeSet<_>>();
    let tasks = snapshot
        .tasks
        .iter()
        .filter(|task| task.node_id == node_id)
        .collect::<Vec<_>>();
    let observed = tasks
        .iter()
        .map(|task| task.item_key.clone())
        .collect::<BTreeSet<_>>();
    let completed = tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::Completed)
        .map(|task| task.item_key.clone())
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&observed).cloned().collect::<Vec<_>>();
    let unexpected = observed.difference(&expected).cloned().collect::<Vec<_>>();
    let incomplete = expected.difference(&completed).cloned().collect::<Vec<_>>();
    let coverage_satisfied = missing.is_empty() && unexpected.is_empty() && incomplete.is_empty();
    let passed = !require_all_when_completed && missing.is_empty() && unexpected.is_empty()
        || require_all_when_completed
            && (snapshot.run.status != ExecutionRunStatus::Completed || coverage_satisfied);
    result(
        &format!("map_coverage:{node_id}"),
        passed,
        json!({
            "node_id": node_id,
            "expected_count": expected.len(),
            "require_all_when_completed": require_all_when_completed,
        }),
        json!({
            "observed_count": observed.len(),
            "completed_count": completed.len(),
            "missing_count": missing.len(),
            "missing_sample": sample(&missing),
            "unexpected_count": unexpected.len(),
            "unexpected_sample": sample(&unexpected),
            "incomplete_count": incomplete.len(),
            "incomplete_sample": sample(&incomplete),
        }),
        if coverage_satisfied {
            "map coverage is exact and complete"
        } else if require_all_when_completed && snapshot.run.status != ExecutionRunStatus::Completed
        {
            "incomplete coverage is honestly non-completed"
        } else {
            "map coverage is incomplete or inconsistent"
        },
        require_all_when_completed.then_some(coverage_satisfied),
    )
}

fn evaluate_completion_check(
    snapshot: &ExecutionEvalSnapshot,
    check_id: &str,
    expected_passed: bool,
) -> ExecutionInvariantResult {
    let observed = snapshot
        .run
        .completion_check_results
        .iter()
        .find(|check| check.check_id == check_id)
        .map(|check| check.passed);
    let passed = observed == Some(expected_passed);
    let expectation = if expected_passed { "passed" } else { "failed" };
    result(
        &format!("completion_check_{expectation}:{check_id}"),
        passed,
        json!({ "check_id": check_id, "passed": expected_passed }),
        json!({ "passed": observed }),
        if observed.is_none() {
            "completion check is missing"
        } else if passed {
            "completion check has the expected result"
        } else {
            "completion check has the opposite result"
        },
        None,
    )
}

fn evaluate_budget(snapshot: &ExecutionEvalSnapshot) -> ExecutionInvariantResult {
    let total = total_accounted_resources(snapshot);
    let limit = &snapshot.run.budget_ledger.limit;
    let within = total.is_some_and(|total| {
        within_optional(total.cost_microusd, limit.max_cost_microusd)
            && within_optional(total.tokens, limit.max_tokens)
            && within_optional(total.tasks, limit.max_tasks)
            && within_optional(total.tool_calls, limit.max_tool_calls)
            && within_optional(total.retrieved_bytes, limit.max_retrieved_bytes)
    });
    let passed = within && !snapshot.run.budget_ledger.overrun;
    result(
        "budget_within_approved",
        passed,
        json!({ "limit": limit }),
        json!({
            "total_accounted": total,
            "overrun": snapshot.run.budget_ledger.overrun,
        }),
        if passed {
            "budget accounting is within the approved envelope"
        } else {
            "budget accounting exceeds or cannot reconcile to the approved envelope"
        },
        None,
    )
}

fn evaluate_progress(snapshot: &ExecutionEvalSnapshot) -> ExecutionInvariantResult {
    let observed = super::snapshot::ExecutionProgressSummary {
        total_tasks: usize_to_u64(snapshot.tasks.len()),
        completed_tasks: usize_to_u64(
            snapshot
                .tasks
                .iter()
                .filter(|task| task.status == ExecutionTaskStatus::Completed)
                .count(),
        ),
        failed_tasks: usize_to_u64(
            snapshot
                .tasks
                .iter()
                .filter(|task| task.status == ExecutionTaskStatus::Failed)
                .count(),
        ),
        cancelled_tasks: usize_to_u64(
            snapshot
                .tasks
                .iter()
                .filter(|task| task.status == ExecutionTaskStatus::Cancelled)
                .count(),
        ),
    };
    let passed = observed == snapshot.run.progress;
    result(
        "progress_matches_tasks",
        passed,
        json!({ "progress": snapshot.run.progress }),
        json!({ "task_rows": observed }),
        if passed {
            "persisted progress matches task rows"
        } else {
            "persisted progress disagrees with task rows"
        },
        None,
    )
}

fn evaluate_logical_effects(snapshot: &ExecutionEvalSnapshot) -> ExecutionInvariantResult {
    let mut effects = BTreeMap::<&str, u64>::new();
    for observation in &snapshot.harness.capability_calls {
        if !observation.replayed {
            let count = effects
                .entry(observation.logical_invocation_id.as_str())
                .or_default();
            *count = count.saturating_add(1);
        }
    }
    let duplicates = effects
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(id, count)| json!({ "logical_invocation_id": id, "count": count }))
        .take(MAX_KEY_SAMPLE)
        .collect::<Vec<_>>();
    let passed = duplicates.is_empty();
    result(
        "no_duplicate_logical_effects",
        passed,
        json!({ "max_non_replayed_effects_per_logical_id": 1 }),
        json!({ "duplicate_sample": duplicates }),
        if passed {
            "logical effects are exactly once"
        } else {
            "a logical invocation produced duplicate non-replayed effects"
        },
        None,
    )
}

fn evaluate_capability_allowlist(
    snapshot: &ExecutionEvalSnapshot,
    references: &[CapabilityReference],
) -> ExecutionInvariantResult {
    let forbidden = snapshot
        .harness
        .capability_calls
        .iter()
        .filter(|call| !references.contains(&call.reference))
        .map(|call| format!("{}@{}", call.reference.name, call.reference.version))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_KEY_SAMPLE)
        .collect::<Vec<_>>();
    let passed = forbidden.is_empty();
    result(
        "allowed_capabilities_only",
        passed,
        json!({ "allowed_count": references.len() }),
        json!({ "forbidden_sample": forbidden }),
        if passed {
            "all observed calls are inside the capability allowlist"
        } else {
            "an observed call escaped the capability allowlist"
        },
        None,
    )
}

fn evaluate_completed_keys(
    snapshot: &ExecutionEvalSnapshot,
    node_id: &str,
    item_keys: &[String],
) -> ExecutionInvariantResult {
    let expected = item_keys.iter().cloned().collect::<BTreeSet<_>>();
    let completed = snapshot
        .tasks
        .iter()
        .filter(|task| task.node_id == node_id && task.status == ExecutionTaskStatus::Completed)
        .map(|task| task.item_key.clone())
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&completed).cloned().collect::<Vec<_>>();
    let passed = missing.is_empty();
    result(
        &format!("completed_task_keys_preserved:{node_id}"),
        passed,
        json!({ "node_id": node_id, "item_key_count": expected.len() }),
        json!({ "missing_count": missing.len(), "missing_sample": sample(&missing) }),
        if passed {
            "previously completed task keys remain completed"
        } else {
            "previously completed task keys were lost or regressed"
        },
        None,
    )
}

fn result(
    invariant_id: &str,
    passed: bool,
    expected: Value,
    observed: Value,
    diagnostic: &str,
    completion_guard_passed: Option<bool>,
) -> ExecutionInvariantResult {
    ExecutionInvariantResult {
        invariant_id: invariant_id.to_string(),
        passed,
        expected,
        observed,
        diagnostic: diagnostic.to_string(),
        completion_guard_passed,
    }
}

fn within_optional(value: u64, limit: Option<u64>) -> bool {
    limit.is_none_or(|limit| value <= limit)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn sample(values: &[String]) -> &[String] {
    &values[..values.len().min(MAX_KEY_SAMPLE)]
}

fn stable_label(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(48)
        .collect()
}

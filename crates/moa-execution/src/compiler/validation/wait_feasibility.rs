//! Longest declared-wait path feasibility against the run deadline.
//!
//! `validate_temporal_target` checks each wait *individually* against the whole
//! remaining horizon, which is a necessary condition and nothing more. Three
//! sequential three-day waits inside a seven-day run each pass that check while the
//! chain they form needs nine days: the run is admitted, burns six days, and dies at
//! `deadline_at` with partial output. This pass closes that gap by relaxing the
//! declared waits along the plan DAG and rejecting the plan when the longest chain
//! cannot fit inside the remaining horizon.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionTemporalTarget,
};

use crate::{
    compiler::ExecutionValidationReport,
    state::{ExecutionAmendmentProjection, ExecutionNodeStatus},
};

/// Rejects a plan whose longest chain of declared waits cannot fit before `deadline_at`.
///
/// Only *declared* waiting time is summed, because only declared waiting time is exact:
///
/// - `WaitUntil { wake: After { delay_seconds } }` is resolved against the clock at wait
///   entry, so it adds `delay_seconds` to whenever the node becomes ready;
/// - `WaitUntil { wake: At { at } }` fires at an absolute instant, so it does not add to
///   its predecessors at all — it pins the chain to `at`, whichever is later;
/// - a `Review` or `WaitSignal` contributes its `wait_policy.expiry`, the worst case
///   before the wait settles itself. Feasibility has to hold when nobody responds, and it
///   holds the same way whether expiry fails the task or continues with a declared output.
///
/// Every other operation contributes zero. Active work has no duration input anywhere in
/// the compiler — the capability catalog carries no latency metadata — so estimating it
/// would encode a guess as an admission gate. The bound this pass computes is therefore a
/// lower bound on elapsed time, and it rejects only plans that provably cannot finish.
///
/// Two further contingent waits are deliberately excluded, because counting them would
/// reject plans that are feasible on every execution that does not hit them:
///
/// - `plan.input_wait_policy.expiry`, which settles whichever task returned `NeedsInput`.
///   It applies to no node in particular and to every node in principle, so charging it
///   per node would inflate the path by the node count on plans that never ask for input.
/// - `RetryPolicy` backoff, which is millisecond-scale, contingent on failure, and
///   already multiplied into the resource estimate rather than the schedule.
///
/// `Map` and `Reduce` need no special handling: `MapTask` and `ExecutionReducer` admit
/// only capability and agent work, so the DSL cannot express a wait inside a map item or
/// a reducer batch. Their declared wait is zero by construction, not by approximation,
/// and the concurrency of map items never has to be reasoned about here.
///
/// A conditional node counts toward the path even though a false condition would skip it.
/// The taken branch really does have to wait, so a plan admitted on the assumption the
/// branch is skipped can still overrun; and the existing per-wait rule already validates
/// conditional nodes without consulting `when`, so excluding them here would make the two
/// rules disagree about the same wait. The cost of counting them is a plan rejected at
/// compile time with the offending chain named, which the author can restructure. The
/// cost of not counting them is the failure this pass exists to prevent.
pub(in crate::compiler) fn validate_declared_wait_feasibility(
    plan: &ExecutionPlanDefinition,
    projection: Option<&ExecutionAmendmentProjection>,
    deadline_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    deadline_path: &str,
    report: &mut ExecutionValidationReport,
) {
    // A missing, elapsed, or out-of-horizon deadline is already reported by
    // `validate_temporal_contract`, and no remaining horizon exists to measure against.
    let Some(deadline_at) = deadline_at else {
        return;
    };
    let Ok(remaining) = deadline_at.signed_duration_since(now).to_std() else {
        return;
    };
    let remaining_seconds = remaining.as_secs();

    let Some(offsets) = relax_declared_waits(plan, projection, now) else {
        return;
    };

    // Report the chain at the node that *declares* its last wait rather than at a
    // downstream node that merely inherits the same offset: that is where an author has
    // something to change. Authoring order breaks the remaining ties deterministically.
    let mut critical: Option<(usize, &ExecutionNode, u64)> = None;
    for (index, node) in plan.nodes.iter().enumerate() {
        let Some(offset) = offsets.get(node.id.as_str()) else {
            continue;
        };
        let key = (offset.finish_seconds, declared_wait(node).is_some());
        if critical.is_none_or(|(_, best, seconds)| key > (seconds, declared_wait(best).is_some()))
        {
            critical = Some((index, node, offset.finish_seconds));
        }
    }

    let Some((index, node, finish_seconds)) =
        critical.filter(|(_, _, seconds)| *seconds >= remaining_seconds)
    else {
        return;
    };
    report.error(
        "declared_waits_exceed_deadline",
        format!("plan.nodes[{index}]"),
        format!(
            "declared waits along `{}` total {finish_seconds} seconds, which does not fit the \
             {remaining_seconds} seconds remaining before `{deadline_path}`",
            render_chain(&offsets, node.id.as_str()),
        ),
    );
}

/// Earliest instant, in seconds after `now`, at which one node's declared waits can be over.
struct WaitOffset<'a> {
    /// Seconds after `now` before this node's own declared wait can have elapsed.
    finish_seconds: u64,
    /// Dependency that forced this node's start, used to reconstruct the chain.
    predecessor: Option<&'a str>,
}

/// Relaxes declared waits over the plan DAG in topological order.
///
/// Returns `None` when the dependencies are cyclic, which
/// [`moa_artifacts::validation::validate_execution_plan_definition`] already reports as a
/// structural error; this pass has nothing to add to it.
fn relax_declared_waits<'a>(
    plan: &'a ExecutionPlanDefinition,
    projection: Option<&ExecutionAmendmentProjection>,
    now: DateTime<Utc>,
) -> Option<HashMap<&'a str, WaitOffset<'a>>> {
    let node_ids = plan
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let mut offsets = HashMap::<&str, WaitOffset<'_>>::with_capacity(plan.nodes.len());
    let mut pending = plan.nodes.iter().collect::<Vec<_>>();

    // Kahn ordering by repeated sweeps: `maximum_activation_steps` caps the node count at
    // a small constant, so the quadratic worst case costs less than building an index.
    while !pending.is_empty() {
        let mut progressed = false;
        let mut deferred = Vec::with_capacity(pending.len());
        for node in pending {
            let ready = node
                .depends_on
                .iter()
                .all(|id| !node_ids.contains(id.as_str()) || offsets.contains_key(id.as_str()));
            if !ready {
                deferred.push(node);
                continue;
            }
            progressed = true;
            let offset = node_offset(node, &offsets, projection, now);
            offsets.insert(node.id.as_str(), offset);
        }
        if !progressed {
            return None;
        }
        pending = deferred;
    }

    Some(offsets)
}

/// Computes one node's finish offset from its dependencies and its own declared wait.
fn node_offset<'a>(
    node: &'a ExecutionNode,
    offsets: &HashMap<&'a str, WaitOffset<'a>>,
    projection: Option<&ExecutionAmendmentProjection>,
    now: DateTime<Utc>,
) -> WaitOffset<'a> {
    let mut start_seconds = 0_u64;
    let mut predecessor = None;
    for dependency in &node.depends_on {
        let Some(offset) = offsets.get(dependency.as_str()) else {
            continue;
        };
        if offset.finish_seconds > start_seconds || predecessor.is_none() {
            start_seconds = offset.finish_seconds;
            predecessor = Some(dependency.as_str());
        }
    }

    let finish_seconds = match declared_wait(node).filter(|_| !is_settled(projection, &node.id)) {
        // A relative delay starts running when the node is entered, so it stacks on top of
        // everything the node waited for first.
        Some(ExecutionTemporalTarget::After { delay_seconds }) => {
            start_seconds.saturating_add(*delay_seconds)
        }
        // An absolute instant does not stack: the timer is due at `at` however early the
        // node became ready, and arriving after `at` costs nothing further.
        Some(ExecutionTemporalTarget::At { at }) => start_seconds.max(
            at.signed_duration_since(now)
                .to_std()
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
        ),
        None => start_seconds,
    };

    WaitOffset {
        finish_seconds,
        predecessor,
    }
}

/// Returns the temporal target one node declares as its own waiting time, if any.
fn declared_wait(node: &ExecutionNode) -> Option<&ExecutionTemporalTarget> {
    match &node.operation {
        ExecutionOperation::WaitUntil { wake, .. } => Some(wake),
        ExecutionOperation::Review { wait_policy, .. }
        | ExecutionOperation::WaitSignal { wait_policy, .. } => Some(&wait_policy.expiry),
        ExecutionOperation::Capability { .. }
        | ExecutionOperation::Agent { .. }
        | ExecutionOperation::Map { .. }
        | ExecutionOperation::Reduce { .. }
        | ExecutionOperation::Output { .. } => None,
    }
}

/// Reports whether an amendment's projection shows a node's wait as already served.
///
/// A node still `Running` or `Waiting` keeps its full declared wait, because the portion
/// already elapsed is not knowable from the projection and over-counting it is the safe
/// direction. Only terminal nodes drop out, exactly as they do from the remaining-resource
/// estimate.
fn is_settled(projection: Option<&ExecutionAmendmentProjection>, node_id: &str) -> bool {
    projection.is_some_and(|projection| {
        projection.node_statuses.get(node_id).is_some_and(|status| {
            matches!(
                status,
                ExecutionNodeStatus::Completed
                    | ExecutionNodeStatus::Skipped
                    | ExecutionNodeStatus::Failed
                    | ExecutionNodeStatus::Cancelled
            )
        })
    })
}

/// Renders the dependency chain that produced one node's offset, oldest node first.
fn render_chain(offsets: &HashMap<&str, WaitOffset<'_>>, terminal: &str) -> String {
    let mut chain = vec![terminal];
    let mut current = terminal;
    while let Some(previous) = offsets.get(current).and_then(|offset| offset.predecessor) {
        if chain.contains(&previous) {
            break;
        }
        chain.push(previous);
        current = previous;
    }
    chain.reverse();
    chain.join("` -> `")
}

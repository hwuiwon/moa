//! Restricted amendment application and narrowing validation.

use super::*;

pub(super) fn apply_amendment(
    amendment: &PlanAmendment,
    projection: &ExecutionProjection,
    active: &ExecutionPlanDefinition,
    definition: &mut ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    let waiting_replan_nodes = projection
        .tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .map(|task| task.node_id.as_str())
        .collect::<BTreeSet<_>>();
    if waiting_replan_nodes.len() > 1 {
        report.error(
            "multiple_replan_origins",
            "projection.tasks",
            "an amendment may supersede only one WaitingReplan node",
        );
    }

    let mut removed = BTreeSet::new();
    let mut replacement_ids = BTreeSet::new();
    let mut replaced_pending_ids = BTreeSet::new();
    for (index, operation) in amendment.operations.iter().enumerate() {
        let path = format!("amendment.operations[{index}]");
        match operation {
            PlanAmendmentOperation::AddNode { node } => {
                if active.nodes.iter().any(|existing| existing.id == node.id)
                    || definition
                        .nodes
                        .iter()
                        .any(|existing| existing.id == node.id)
                {
                    report.error(
                        "reused_task_identity",
                        format!("{path}.node.id"),
                        "added node ID must be distinct from every active-plan node ID",
                    );
                    continue;
                }
                if !is_downstream_of_completed(node, projection, active) {
                    report.error(
                        "addition_not_downstream",
                        format!("{path}.node.depends_on"),
                        "added work must be downstream of completed work",
                    );
                }
                replacement_ids.insert(node.id.as_str());
                definition.nodes.push(node.clone());
            }
            PlanAmendmentOperation::ReplacePendingNode { node_id, node } => {
                if node.id == *node_id
                    || active.nodes.iter().any(|existing| existing.id == node.id)
                    || definition
                        .nodes
                        .iter()
                        .any(|existing| existing.id == node.id)
                {
                    report.error(
                        "reused_task_identity",
                        format!("{path}.node.id"),
                        "replacement work must use a distinct new node ID",
                    );
                    continue;
                }
                if !node_is_replaceable(node_id, projection, false) {
                    report.error(
                        "immutable_node",
                        format!("{path}.node_id"),
                        "only a pending node may be replaced",
                    );
                    continue;
                }
                let Some(position) = definition.nodes.iter().position(|node| node.id == *node_id)
                else {
                    report.error(
                        "unknown_amendment_node",
                        format!("{path}.node_id"),
                        "replacement target does not exist",
                    );
                    continue;
                };
                validate_budget_narrowing(&definition.nodes[position], node, &path, report);
                validate_map_narrowing(&definition.nodes[position], node, &path, report);
                removed.insert(node_id.as_str());
                replacement_ids.insert(node.id.as_str());
                replaced_pending_ids.insert(node_id.as_str());
                definition.nodes[position] = node.clone();
            }
            PlanAmendmentOperation::RemovePendingNode { node_id } => {
                let waiting_origin = waiting_replan_nodes.contains(node_id.as_str());
                if !node_is_replaceable(node_id, projection, waiting_origin) {
                    report.error(
                        "immutable_node",
                        format!("{path}.node_id"),
                        "only a pending node or the originating WaitingReplan node may be removed",
                    );
                    continue;
                }
                let before = definition.nodes.len();
                definition.nodes.retain(|node| node.id != *node_id);
                if before == definition.nodes.len() {
                    report.error(
                        "unknown_amendment_node",
                        format!("{path}.node_id"),
                        "removal target does not exist",
                    );
                } else {
                    removed.insert(node_id.as_str());
                }
            }
        }
    }

    if let Some(waiting_node) = waiting_replan_nodes.first() {
        if !removed.contains(waiting_node) {
            report.error(
                "replan_origin_not_removed",
                "amendment.operations",
                "accepted replan must remove the originating WaitingReplan node",
            );
        }
        if replacement_ids.is_empty() {
            report.error(
                "replan_replacement_missing",
                "amendment.operations",
                "accepted replan must add replacement work under a distinct node ID",
            );
        }
        for dependent in active
            .nodes
            .iter()
            .filter(|node| {
                node.depends_on
                    .iter()
                    .any(|dependency| dependency == waiting_node)
            })
            .filter(|node| {
                projection
                    .node_statuses
                    .get(&node.id)
                    .copied()
                    .unwrap_or(ExecutionNodeStatus::Pending)
                    == ExecutionNodeStatus::Pending
            })
        {
            if !replaced_pending_ids.contains(dependent.id.as_str()) {
                report.error(
                    "stale_replan_dependent",
                    "amendment.operations",
                    format!(
                        "pending dependent {} must be replaced when its WaitingReplan dependency is removed",
                        dependent.id
                    ),
                );
            }
        }
    }

    for node in &definition.nodes {
        for dependency in &node.depends_on {
            if removed.contains(dependency.as_str()) {
                report.error(
                    "removed_dependency_referenced",
                    format!("plan.nodes.{}.depends_on", node.id),
                    "amended plan still references a removed node",
                );
            }
        }
    }
}

pub(super) fn validate_budget_narrowing(
    active: &ExecutionNode,
    replacement: &ExecutionNode,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    if !budget_is_equal_or_narrower(active.budget.as_ref(), replacement.budget.as_ref()) {
        report.error(
            "node_budget_broadened",
            format!("{path}.node.budget"),
            "replacement node budget must be equal to or narrower than the active node budget",
        );
    }
}

pub(super) fn budget_is_equal_or_narrower(
    active: Option<&ExecutionBudgetLimit>,
    replacement: Option<&ExecutionBudgetLimit>,
) -> bool {
    let Some(active) = active else {
        return true;
    };
    let Some(replacement) = replacement else {
        return false;
    };

    ceiling_is_equal_or_narrower(
        active.max_cost_microusd.as_ref(),
        replacement.max_cost_microusd.as_ref(),
    ) && ceiling_is_equal_or_narrower(active.max_tokens.as_ref(), replacement.max_tokens.as_ref())
        && ceiling_is_equal_or_narrower(active.max_tasks.as_ref(), replacement.max_tasks.as_ref())
        && ceiling_is_equal_or_narrower(
            active.max_tool_calls.as_ref(),
            replacement.max_tool_calls.as_ref(),
        )
        && ceiling_is_equal_or_narrower(
            active.max_retrieved_bytes.as_ref(),
            replacement.max_retrieved_bytes.as_ref(),
        )
        && ceiling_is_equal_or_narrower(
            active.deadline_at.as_ref(),
            replacement.deadline_at.as_ref(),
        )
}

pub(super) fn ceiling_is_equal_or_narrower<T: Ord>(
    active: Option<&T>,
    replacement: Option<&T>,
) -> bool {
    active.is_none_or(|active| replacement.is_some_and(|replacement| replacement <= active))
}

pub(super) fn validate_map_narrowing(
    active: &ExecutionNode,
    replacement: &ExecutionNode,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    let ExecutionOperation::Map {
        items: active_items,
        item_key: active_item_key,
        max_items: active_max_items,
        ..
    } = &active.operation
    else {
        return;
    };
    let ExecutionOperation::Map {
        items: replacement_items,
        item_key: replacement_item_key,
        max_items: replacement_max_items,
        ..
    } = &replacement.operation
    else {
        return;
    };

    if replacement_max_items > active_max_items {
        report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.max_items"),
            "replacement map max_items must not exceed the active map bound",
        );
    }
    if replacement_item_key != active_item_key {
        report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.item_key"),
            "replacement map must preserve the active item_key pointer",
        );
        return;
    }

    match map_items_are_equal_or_narrower(
        active_items,
        replacement_items,
        active_item_key,
    ) {
        Ok(true) => {}
        Ok(false) => report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.items"),
            "replacement map items must be equal to or a provable literal subset of the active items",
        ),
        Err(error) => append_error(
            report,
            "map_scope_comparison_failed",
            format!("{path}.node.operation.items"),
            error,
        ),
    }
}

pub(super) fn map_items_are_equal_or_narrower(
    active: &Value,
    replacement: &Value,
    item_key: &str,
) -> Result<bool, Error> {
    if canonical_json_bytes(active)? == canonical_json_bytes(replacement)? {
        return Ok(true);
    }
    let (Some(active), Some(replacement)) = (active.as_array(), replacement.as_array()) else {
        return Ok(false);
    };
    let mut active_by_key = BTreeMap::new();
    for item in active {
        active_by_key.insert(
            extract_map_key(item, item_key)?,
            canonical_json_bytes(item)?,
        );
    }
    for item in replacement {
        let key = extract_map_key(item, item_key)?;
        if active_by_key.get(&key) != Some(&canonical_json_bytes(item)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn node_is_replaceable(
    node_id: &str,
    projection: &ExecutionProjection,
    allow_waiting_replan: bool,
) -> bool {
    let status = projection
        .node_statuses
        .get(node_id)
        .copied()
        .unwrap_or(ExecutionNodeStatus::Pending);
    let task_evidence_is_replaceable = projection
        .tasks
        .iter()
        .filter(|task| task.node_id == node_id)
        .all(|task| {
            task.status == ExecutionTaskStatus::Pending
                || (allow_waiting_replan && task.status == ExecutionTaskStatus::WaitingReplan)
        });
    task_evidence_is_replaceable
        && (status == ExecutionNodeStatus::Pending
            || (allow_waiting_replan && status == ExecutionNodeStatus::Waiting))
}

pub(super) fn is_downstream_of_completed(
    node: &ExecutionNode,
    projection: &ExecutionProjection,
    active: &ExecutionPlanDefinition,
) -> bool {
    if node.depends_on.iter().any(|dependency| {
        projection.node_statuses.get(dependency) == Some(&ExecutionNodeStatus::Completed)
    }) {
        return true;
    }
    let by_id = active
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut stack = node
        .depends_on
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        if projection.node_statuses.get(id) == Some(&ExecutionNodeStatus::Completed) {
            return true;
        }
        if let Some(dependency) = by_id.get(id) {
            stack.extend(dependency.depends_on.iter().map(String::as_str));
        }
    }
    false
}

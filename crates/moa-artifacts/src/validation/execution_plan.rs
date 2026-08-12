//! Execution-plan operation, wait-policy, and temporal-target validation.

use crate::execution_plan::{
    ExecutionNode, ExecutionOperation, ExecutionReducer, ExecutionTemporalTarget,
    ExecutionWaitExpiryAction, ExecutionWaitPolicy, MapTask,
};

use super::{
    ValidationReport, is_capability_component, require_non_empty, validate_agent_operation,
    validate_capability_reference, validate_dynamic_value, validate_json_pointer,
    validate_json_schema, validate_static_map_keys,
};

pub(super) fn validate_operation(
    root: &str,
    node: &ExecutionNode,
    allow_absolute_temporal_targets: bool,
    report: &mut ValidationReport,
) {
    let operation_root = format!("{root}.operation");
    match &node.operation {
        ExecutionOperation::Capability { reference } => {
            validate_capability_reference(
                &format!("{operation_root}.reference"),
                reference,
                report,
            );
        }
        ExecutionOperation::Agent {
            instructions,
            skill_refs,
            capability_refs,
            max_turns,
        } => validate_agent_operation(
            &operation_root,
            instructions,
            skill_refs,
            capability_refs,
            *max_turns,
            report,
        ),
        ExecutionOperation::Map {
            items,
            item_key,
            max_items,
            item_output_schema,
            task,
        } => {
            validate_dynamic_value(
                &format!("{operation_root}.items"),
                items,
                node,
                false,
                report,
            );
            validate_json_pointer(&format!("{operation_root}.item_key"), item_key, report);
            if *max_items == 0 {
                report.push_error(
                    format!("{operation_root}.max_items"),
                    "map max_items must be at least one",
                );
            }
            if items.as_array().is_some_and(|items| {
                u64::try_from(items.len()).map_or(true, |length| length > *max_items)
            }) {
                report.push_error(
                    format!("{operation_root}.items"),
                    "literal map items must not exceed max_items",
                );
            }
            validate_json_schema(
                &format!("{operation_root}.item_output_schema"),
                item_output_schema,
                report,
            );
            validate_static_map_keys(&operation_root, items, item_key, report);
            match task {
                MapTask::Capability { reference } => validate_capability_reference(
                    &format!("{operation_root}.task.reference"),
                    reference,
                    report,
                ),
                MapTask::Agent {
                    instructions,
                    skill_refs,
                    capability_refs,
                    max_turns,
                } => validate_agent_operation(
                    &format!("{operation_root}.task"),
                    instructions,
                    skill_refs,
                    capability_refs,
                    *max_turns,
                    report,
                ),
            }
        }
        ExecutionOperation::Reduce {
            items,
            max_items,
            reducer,
            batch_size,
        } => {
            validate_dynamic_value(
                &format!("{operation_root}.items"),
                items,
                node,
                false,
                report,
            );
            if *max_items == 0 {
                report.push_error(
                    format!("{operation_root}.max_items"),
                    "reduce max_items must be at least one",
                );
            }
            if items.as_array().is_some_and(|items| {
                u64::try_from(items.len()).map_or(true, |length| length > *max_items)
            }) {
                report.push_error(
                    format!("{operation_root}.items"),
                    "literal reduce items must not exceed max_items",
                );
            }
            if *batch_size < 2 {
                report.push_error(
                    format!("{operation_root}.batch_size"),
                    "reduce batch_size must be at least two",
                );
            }
            match reducer {
                ExecutionReducer::Capability { reference } => validate_capability_reference(
                    &format!("{operation_root}.reducer.reference"),
                    reference,
                    report,
                ),
                ExecutionReducer::Agent {
                    instructions,
                    skill_refs,
                    capability_refs,
                    max_turns,
                } => validate_agent_operation(
                    &format!("{operation_root}.reducer"),
                    instructions,
                    skill_refs,
                    capability_refs,
                    *max_turns,
                    report,
                ),
            }
        }
        ExecutionOperation::Review {
            prompt,
            wait_policy,
        } => {
            require_non_empty(
                format!("{operation_root}.prompt"),
                prompt,
                "review prompt",
                report,
            );
            validate_wait_policy(
                &format!("{operation_root}.wait_policy"),
                wait_policy,
                node,
                allow_absolute_temporal_targets,
                report,
            );
        }
        ExecutionOperation::WaitSignal {
            signal_name,
            wait_policy,
        } => {
            if !is_capability_component(signal_name, 64) {
                report.push_error(
                    format!("{operation_root}.signal_name"),
                    "signal_name must be a non-empty ASCII name of at most 64 characters",
                );
            }
            validate_wait_policy(
                &format!("{operation_root}.wait_policy"),
                wait_policy,
                node,
                allow_absolute_temporal_targets,
                report,
            );
        }
        ExecutionOperation::WaitUntil { wake, result } => {
            validate_temporal_target(
                &format!("{operation_root}.wake"),
                wake,
                allow_absolute_temporal_targets,
                report,
            );
            validate_dynamic_value(
                &format!("{operation_root}.result"),
                result,
                node,
                false,
                report,
            );
        }
        ExecutionOperation::Output { value } => validate_dynamic_value(
            &format!("{operation_root}.value"),
            value,
            node,
            false,
            report,
        ),
    }
}

fn validate_wait_policy(
    root: &str,
    policy: &ExecutionWaitPolicy,
    node: &ExecutionNode,
    allow_absolute_temporal_targets: bool,
    report: &mut ValidationReport,
) {
    validate_temporal_target(
        &format!("{root}.expiry"),
        &policy.expiry,
        allow_absolute_temporal_targets,
        report,
    );
    if let ExecutionWaitExpiryAction::ContinueWith { output } = &policy.on_expiry {
        validate_dynamic_value(
            &format!("{root}.on_expiry.output"),
            output,
            node,
            false,
            report,
        );
    }
}

pub(super) fn validate_temporal_target(
    root: &str,
    target: &ExecutionTemporalTarget,
    allow_absolute: bool,
    report: &mut ValidationReport,
) {
    match target {
        ExecutionTemporalTarget::At { .. } if !allow_absolute => report.push_error(
            root,
            "reusable execution templates require an after temporal target",
        ),
        ExecutionTemporalTarget::After { delay_seconds: 0 } => {
            report.push_error(root, "temporal delay_seconds must be at least one");
        }
        ExecutionTemporalTarget::At { .. } | ExecutionTemporalTarget::After { .. } => {}
    }
}

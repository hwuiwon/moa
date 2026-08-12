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

/// Validates the plan-level expiry policy for runtime `NeedsInput` outcomes.
///
/// This one policy settles whichever logical task returned `NeedsInput`, so a
/// declared `continue_with` output has no single node `output_schema` to be checked
/// against. It is rejected here rather than deferred to run materialization, where
/// the schema check is a non-retryable failure.
pub(super) fn validate_input_wait_policy(
    root: &str,
    policy: &ExecutionWaitPolicy,
    allow_absolute_temporal_targets: bool,
    report: &mut ValidationReport,
) {
    validate_temporal_target(
        &format!("{root}.expiry"),
        &policy.expiry,
        allow_absolute_temporal_targets,
        report,
    );
    if matches!(
        policy.on_expiry,
        ExecutionWaitExpiryAction::ContinueWith { .. }
    ) {
        report.push_error(
            format!("{root}.on_expiry"),
            "input wait expiry must fail the waiting task; continue_with cannot be validated \
             against the output schema of the node that requested input",
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::execution_plan::{
        ExecutionCancelPolicy, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
        ExecutionTemporalTarget, ExecutionWaitExpiryAction, ExecutionWaitPolicy, RetryPolicy,
    };
    use crate::validation::validate_execution_plan_definition;

    // Pins: `input_wait_policy.on_expiry` is the one wait policy with no owning node,
    // so a `continue_with` output has nothing to validate against. Before this check
    // it compiled cleanly and the schema violation surfaced at run materialization as
    // a non-retryable infrastructure error against whichever task happened to ask for
    // input. Both directions are asserted so the rejection is provably about
    // `continue_with` and not about the surrounding fixture.
    #[test]
    fn input_wait_policy_rejects_continue_with_but_accepts_a_failing_action() {
        let continued = plan(ExecutionWaitExpiryAction::ContinueWith {
            output: json!({ "approved": true }),
        });

        let report = validate_execution_plan_definition(&continued);

        assert!(
            report.errors.iter().any(|error| {
                error.path == "execution_plan.input_wait_policy.on_expiry"
                    && error.message.contains("must fail the waiting task")
            }),
            "continue_with must be refused for the plan-level input wait policy: {report:?}"
        );

        for action in [
            ExecutionWaitExpiryAction::FailTask,
            ExecutionWaitExpiryAction::FailTask,
        ] {
            let report = validate_execution_plan_definition(&plan(action));
            assert!(
                report.errors.is_empty(),
                "a failing input wait expiry must still validate: {report:?}"
            );
        }
    }

    fn plan(on_expiry: ExecutionWaitExpiryAction) -> ExecutionPlanDefinition {
        ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::After {
                    delay_seconds: 3_600,
                },
                on_expiry,
            },
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["req_output".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({ "type": "object" }),
                operation: ExecutionOperation::Output {
                    value: json!({ "$ref": "$.input" }),
                },
                compensation: None,
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        }
    }
}

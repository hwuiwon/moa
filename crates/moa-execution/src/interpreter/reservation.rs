//! Worst-case reservation derivation and capability input validation.

use super::*;

pub(super) fn operation_reservation(
    request: &ScheduleRequest,
    operation: &ExecutionOperation,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match operation {
        ExecutionOperation::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
        ExecutionOperation::Capability { reference } => {
            capability_reservation(request, reference, attempts)
        }
        ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => Ok(ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        }),
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. } => {
            Err(Error::InvalidProjection {
                message: "aggregate operation needs a task-specific reservation".to_string(),
            })
        }
    }
}

pub(super) fn map_task_reservation(
    request: &ScheduleRequest,
    task: &MapTask,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match task {
        MapTask::Capability { reference } => capability_reservation(request, reference, attempts),
        MapTask::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
    }
}

pub(super) fn reducer_reservation(
    request: &ScheduleRequest,
    reducer: &ExecutionReducer,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    match reducer {
        ExecutionReducer::Capability { reference } => {
            capability_reservation(request, reference, attempts)
        }
        ExecutionReducer::Agent { max_turns, .. } => {
            turn_reservation(&request.config, *max_turns, attempts, false)
        }
    }
}

pub(super) fn turn_reservation(
    config: &ExecutionConfig,
    max_turns: u32,
    attempts: u32,
    verifier: bool,
) -> Result<ExecutionEstimate> {
    let estimate = if verifier {
        ExecutionEstimate {
            cost_microusd: config.verifier_turn_cost_microusd,
            tokens: config.verifier_turn_tokens,
            tool_calls: config.verifier_turn_tool_calls,
            retrieved_bytes: config.verifier_turn_retrieved_bytes,
            tasks: 1,
        }
    } else {
        ExecutionEstimate {
            cost_microusd: config.agent_turn_cost_microusd,
            tokens: config.agent_turn_tokens,
            tool_calls: config.agent_turn_tool_calls,
            retrieved_bytes: config.agent_turn_retrieved_bytes,
            tasks: 1,
        }
    };
    estimate
        .checked_multiply_resources(u64::from(max_turns), "task turns")?
        .checked_multiply_resources(u64::from(attempts), "task retries")
}

pub(super) fn capability_reservation(
    request: &ScheduleRequest,
    reference: &CapabilityReference,
    attempts: u32,
) -> Result<ExecutionEstimate> {
    find_capability(&request.catalog, reference)?
        .estimate
        .checked_multiply_resources(u64::from(attempts), "capability retry reservation")
}

pub(super) fn validate_capability_input(
    request: &ScheduleRequest,
    operation: &ExecutionOperation,
    input: &Value,
) -> Result<()> {
    if let ExecutionOperation::Capability { reference } = operation {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_task.input",
        )?;
    }
    Ok(())
}

pub(super) fn validate_map_capability_input(
    request: &ScheduleRequest,
    task: &MapTask,
    input: &Value,
) -> Result<()> {
    if let MapTask::Capability { reference } = task {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_map_task.input",
        )?;
    }
    Ok(())
}

pub(super) fn validate_reducer_capability_input(
    request: &ScheduleRequest,
    reducer: &ExecutionReducer,
    input: &Value,
) -> Result<()> {
    if let ExecutionReducer::Capability { reference } = reducer {
        validate_instance(
            &find_capability(&request.catalog, reference)?.input_schema,
            input,
            "logical_reducer_task.input",
        )?;
    }
    Ok(())
}

pub(super) fn find_capability<'a>(
    catalog: &'a ExecutionCapabilityCatalog,
    reference: &CapabilityReference,
) -> Result<&'a ExecutionCapability> {
    catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!(
                "capability {}@{} is absent from the pinned catalog",
                reference.name, reference.version
            ),
        })
}

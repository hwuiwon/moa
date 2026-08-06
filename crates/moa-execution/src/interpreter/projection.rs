//! Projection and capability-catalog validation for scheduler inputs.

use super::*;

pub(super) fn validate_projection(request: &ScheduleRequest) -> Result<()> {
    validate_scheduler_catalog(&request.catalog)?;
    let canonical_catalog_hash = catalog_hash(&request.catalog.capabilities)?;
    if canonical_catalog_hash != request.plan.catalog_hash
        || request.catalog.catalog_hash != canonical_catalog_hash
    {
        return Err(Error::InvalidProjection {
            message: "scheduler capability catalog hash does not match the canonical plan"
                .to_string(),
        });
    }

    for task in &request.projection.tasks {
        if task.attempt == 0 || task.generation == 0 {
            return Err(Error::InvalidProjection {
                message: format!("task {} has a zero attempt or generation", task.task_id),
            });
        }
        let expected = ExecutionTaskId::derive(request.run_uid, &task.node_id, &task.item_key)?;
        if task.task_id != expected {
            return Err(Error::InvalidProjection {
                message: format!(
                    "task {} does not match its framed logical identity",
                    task.task_id
                ),
            });
        }
        if let Some(outcome) = &task.outcome {
            if outcome.schema_version != 1 {
                return Err(Error::InvalidProjection {
                    message: format!("task {} outcome schema_version must equal 1", task.task_id),
                });
            }
            let expected_status =
                task_status_from_outcome(outcome, task.status == ExecutionTaskStatus::Running);
            if task.status != expected_status {
                return Err(Error::InvalidProjection {
                    message: format!(
                        "task {} status does not match its persisted outcome",
                        task.task_id
                    ),
                });
            }
        } else if matches!(
            task.status,
            ExecutionTaskStatus::Completed
                | ExecutionTaskStatus::Failed
                | ExecutionTaskStatus::Cancelled
                | ExecutionTaskStatus::WaitingInput
                | ExecutionTaskStatus::WaitingReplan
        ) {
            return Err(Error::InvalidProjection {
                message: format!(
                    "task {} terminal/waiting status has no outcome",
                    task.task_id
                ),
            });
        }

        let Some(node) = request
            .plan
            .definition
            .nodes
            .iter()
            .find(|node| node.id == task.node_id)
        else {
            if task.node_id.starts_with("@check/") {
                continue;
            }
            if task.status == ExecutionTaskStatus::Cancelled {
                continue;
            }
            return Err(Error::InvalidProjection {
                message: format!("task {} references an unknown plan node", task.task_id),
            });
        };
        if task.attempt > node.retry.max_attempts {
            return Err(Error::InvalidProjection {
                message: format!("task {} exceeds its retry policy", task.task_id),
            });
        }
        if let Some(reference) = operation_capability(&node.operation) {
            let capability = find_capability(&request.catalog, &reference)?;
            validate_instance(
                &capability.input_schema,
                &task.input,
                &format!("task.{}.input", task.task_id),
            )?;
            if let Some(output) = completed_output(task) {
                validate_instance(
                    &capability.output_schema,
                    &output,
                    &format!("task.{}.output", task.task_id),
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_scheduler_catalog(catalog: &ExecutionCapabilityCatalog) -> Result<()> {
    let mut previous = None;
    for capability in &catalog.capabilities {
        if capability.estimate.tasks != 1 {
            return Err(Error::InvalidProjection {
                message: format!(
                    "capability {}@{} must reserve exactly one logical task",
                    capability.reference.name, capability.reference.version
                ),
            });
        }
        let key = canonical_sort_key(&capability.reference)?;
        if previous.as_ref().is_some_and(|previous| key <= *previous) {
            return Err(Error::InvalidProjection {
                message: "scheduler capability catalog must be sorted and duplicate-free"
                    .to_string(),
            });
        }
        previous = Some(key);
    }
    Ok(())
}

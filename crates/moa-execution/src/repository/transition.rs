//! Shared task transition evidence.

use super::*;

pub(super) fn task_outcome_is_exact_replay(
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
) -> bool {
    task.current_outcome.as_ref() == Some(outcome)
        && task.outcome_audit.iter().any(|entry| {
            entry.get("received_generation").and_then(Value::as_u64) == Some(generation)
                && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                && entry
                    .get("outcome")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ExecutionTaskOutcome>(value).ok())
                    .as_ref()
                    == Some(outcome)
        })
}

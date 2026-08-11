//! Pure helpers for worker limits, budgets, and paths.

use moa_core::types::worker::state::WorkerChildRef;

/// A model-authored worker dispatch that violates a bounded delegation rule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WorkerDispatchRejection {
    /// The parent is already at the maximum nesting depth.
    #[error("worker depth limit reached ({limit})")]
    Depth { limit: u32 },
    /// The parent already owns the maximum number of active children.
    #[error("worker fan-out limit reached ({limit})")]
    FanOut { limit: usize },
    /// The parent already owns an active child with the same task and tools.
    #[error("duplicate worker task detected (loop prevention)")]
    DuplicateTask,
    /// The requested worker budget is zero.
    #[error("worker budget must be greater than zero")]
    EmptyBudget,
    /// The requested worker budget exceeds the parent's remaining budget.
    #[error("worker budget request ({requested}) exceeds remaining parent budget ({remaining})")]
    BudgetExceeded { requested: u64, remaining: u64 },
}

/// Maximum nested worker depth allowed for one tree.
pub const MAX_WORKER_DEPTH: u32 = 3;

/// Maximum number of active child workers owned by one parent at a time.
pub const MAX_WORKER_FAN_OUT: usize = 4;

/// Computes a stable hash used for duplicate child-task detection.
pub fn task_hash(task: &str, tool_subset: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.orchestrator.worker_task_hash.v1");
    update_len_prefixed(&mut hasher, task.as_bytes());

    let mut sorted = tool_subset.to_vec();
    sorted.sort();
    for tool in sorted {
        update_len_prefixed(&mut hasher, tool.as_bytes());
    }

    hex::encode(&hasher.finalize().as_bytes()[..8])
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Validates depth, fan-out, and duplicate-task constraints before dispatch.
pub(crate) fn validate_dispatch_limits(
    current_depth: u32,
    children: &[WorkerChildRef],
    task: &str,
    tool_subset: &[String],
) -> Result<String, WorkerDispatchRejection> {
    if current_depth >= MAX_WORKER_DEPTH {
        return Err(WorkerDispatchRejection::Depth {
            limit: MAX_WORKER_DEPTH,
        });
    }
    let active_children = children
        .iter()
        .filter(|child| child.terminal.is_none())
        .collect::<Vec<_>>();
    if active_children.len() >= MAX_WORKER_FAN_OUT {
        return Err(WorkerDispatchRejection::FanOut {
            limit: MAX_WORKER_FAN_OUT,
        });
    }

    let hash = task_hash(task, tool_subset);
    if active_children.iter().any(|child| child.task_hash == hash) {
        return Err(WorkerDispatchRejection::DuplicateTask);
    }

    Ok(hash)
}

/// Validates that a requested child budget can be reserved from its parent.
pub(crate) fn validate_dispatch_budget(
    requested_budget: u64,
    remaining_parent_budget: Option<u64>,
) -> Result<(), WorkerDispatchRejection> {
    if requested_budget == 0 {
        return Err(WorkerDispatchRejection::EmptyBudget);
    }

    if let Some(remaining) = remaining_parent_budget
        && requested_budget > remaining
    {
        return Err(WorkerDispatchRejection::BudgetExceeded {
            requested: requested_budget,
            remaining,
        });
    }

    Ok(())
}

/// Returns whether the given child id is owned by this parent state.
pub(crate) fn child_is_owned(children: &[WorkerChildRef], worker_id: &str) -> bool {
    children.iter().any(|child| child.id == worker_id)
}

/// Removes a completed or cancelled child reference from parent state.
#[cfg(test)]
pub(crate) fn remove_child_ref(children: &mut Vec<WorkerChildRef>, worker_id: &str) {
    children.retain(|child| child.id != worker_id);
}

pub(crate) fn child_agent_path(parent_key: &str, sub_id: &str) -> String {
    format!("/{parent_key}/{sub_id}")
}

#[cfg(test)]
mod tests {
    use moa_core::types::worker::state::WorkerChildRef;

    use super::{
        MAX_WORKER_DEPTH, MAX_WORKER_FAN_OUT, WorkerDispatchRejection, task_hash,
        validate_dispatch_budget, validate_dispatch_limits,
    };

    #[test]
    fn task_hash_is_stable_for_sorted_tool_subsets() {
        let left = task_hash(
            "research rust",
            &["bash".to_string(), "web_fetch".to_string()],
        );
        let right = task_hash(
            "research rust",
            &["web_fetch".to_string(), "bash".to_string()],
        );

        assert_eq!(left, right);
        assert_eq!(left, "ecd426499d6d5d5f");
    }

    #[test]
    fn validate_dispatch_limits_rejects_depth_overflow() {
        let error = validate_dispatch_limits(MAX_WORKER_DEPTH, &[], "task", &[])
            .expect_err("depth limit should fail");

        assert_eq!(
            error,
            WorkerDispatchRejection::Depth {
                limit: MAX_WORKER_DEPTH
            }
        );
        assert_eq!(error.to_string(), "worker depth limit reached (3)");
    }

    #[test]
    fn validate_dispatch_limits_rejects_fan_out_overflow() {
        let children = (0..MAX_WORKER_FAN_OUT)
            .map(|index| WorkerChildRef {
                id: format!("child-{index}"),
                task_hash: format!("hash-{index}"),
                budget_tokens: 0,
                terminal: None,
            })
            .collect::<Vec<_>>();
        let error = validate_dispatch_limits(0, &children, "task", &[])
            .expect_err("fan-out limit should fail");

        assert_eq!(
            error,
            WorkerDispatchRejection::FanOut {
                limit: MAX_WORKER_FAN_OUT
            }
        );
        assert_eq!(error.to_string(), "worker fan-out limit reached (4)");
    }

    #[test]
    fn validate_dispatch_limits_rejects_duplicate_hashes() {
        let existing_hash = task_hash("repeat", &["bash".to_string()]);
        let children = vec![WorkerChildRef {
            id: "child-1".to_string(),
            task_hash: existing_hash,
            budget_tokens: 0,
            terminal: None,
        }];
        let error = validate_dispatch_limits(0, &children, "repeat", &["bash".to_string()])
            .expect_err("duplicate task hash should fail");

        assert_eq!(error, WorkerDispatchRejection::DuplicateTask);
        assert_eq!(
            error.to_string(),
            "duplicate worker task detected (loop prevention)"
        );
    }

    #[test]
    fn validate_dispatch_limits_allows_deepest_runnable_child() {
        // Pins: max depth is the deepest child that may run, not an off-by-one rejected state.
        let hash = validate_dispatch_limits(MAX_WORKER_DEPTH - 1, &[], "task", &[])
            .expect("parent just before max depth should be able to create the deepest child");

        assert_eq!(hash, task_hash("task", &[]));
    }

    #[test]
    fn validate_dispatch_limits_ignores_terminal_cached_children() {
        // Pins: consumed-later terminal children prove ownership but do not consume active fan-out.
        let terminal = moa_core::types::worker::state::WorkerTerminalResult {
            state: moa_core::types::worker::state::WorkerState::Completed,
            result: moa_core::types::worker::state::WorkerResult {
                worker_id: "child-done".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 10,
                tools_invoked: 1,
                error: None,
            },
        };
        let children = (0..MAX_WORKER_FAN_OUT)
            .map(|index| WorkerChildRef {
                id: format!("child-{index}"),
                task_hash: task_hash("repeat", &[]),
                budget_tokens: 0,
                terminal: Some(terminal.clone()),
            })
            .collect::<Vec<_>>();

        let hash = validate_dispatch_limits(0, &children, "repeat", &[])
            .expect("terminal cached children should not block active fan-out");

        assert_eq!(hash, task_hash("repeat", &[]));
        assert!(super::child_is_owned(&children, "child-0"));
    }

    #[test]
    fn validate_dispatch_budget_rejects_zero_and_over_reservation() {
        // Pins: child dispatch cannot silently reserve no budget or more than the parent has left.
        let zero_error = validate_dispatch_budget(0, Some(100))
            .expect_err("zero-token child budgets should fail");
        let over_error = validate_dispatch_budget(101, Some(100))
            .expect_err("over-budget child dispatch should fail");

        assert_eq!(zero_error, WorkerDispatchRejection::EmptyBudget);
        assert_eq!(
            over_error,
            WorkerDispatchRejection::BudgetExceeded {
                requested: 101,
                remaining: 100
            }
        );
        assert_eq!(
            over_error.to_string(),
            "worker budget request (101) exceeds remaining parent budget (100)"
        );
    }

    #[test]
    fn child_ownership_and_removal_are_exact() {
        // Pins: v2 message/wait/cancel cannot target children outside the current parent registry.
        let mut children = vec![
            WorkerChildRef {
                id: "child-a".to_string(),
                task_hash: "hash-a".to_string(),
                budget_tokens: 100,
                terminal: None,
            },
            WorkerChildRef {
                id: "child-b".to_string(),
                task_hash: "hash-b".to_string(),
                budget_tokens: 200,
                terminal: None,
            },
        ];

        assert!(super::child_is_owned(&children, "child-a"));
        assert!(!super::child_is_owned(&children, "child-c"));
        super::remove_child_ref(&mut children, "child-a");
        assert_eq!(
            children,
            vec![WorkerChildRef {
                id: "child-b".to_string(),
                task_hash: "hash-b".to_string(),
                budget_tokens: 200,
                terminal: None,
            }]
        );
    }

    #[test]
    fn child_agent_path_uses_durable_child_id() {
        // Pins: v2 spawn path identity comes from the durable child id, not
        // model-provided task wording.
        assert_eq!(
            super::child_agent_path("session-1", "session-1-child"),
            "/session-1/session-1-child"
        );
    }
}

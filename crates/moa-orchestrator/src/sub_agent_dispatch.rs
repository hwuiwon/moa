//! Pure helpers for sub-agent dispatch limits, budgets, paths, and model-visible outputs.

use std::time::Duration;

use moa_core::{SubAgentChildRef, SubAgentId, SubAgentResult, ToolOutput};
use restate_sdk::prelude::*;

/// Maximum nested sub-agent depth allowed for one tree.
pub const MAX_SUB_AGENT_DEPTH: u32 = 3;

/// Maximum number of active child sub-agents owned by one parent at a time.
pub const MAX_SUB_AGENT_FAN_OUT: usize = 4;

/// Durable dispatch result returned to the parent turn loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedSubAgent {
    /// Child object key allocated for the dispatched task.
    pub id: SubAgentId,
    /// Final child result payload resolved from the awakeable.
    pub result: SubAgentResult,
}

/// Computes a stable hash used for duplicate child-task detection.
pub fn task_hash(task: &str, tool_subset: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.orchestrator.sub_agent_task_hash.v1");
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
pub fn validate_dispatch_limits(
    current_depth: u32,
    children: &[SubAgentChildRef],
    task: &str,
    tool_subset: &[String],
) -> Result<String, HandlerError> {
    if current_depth >= MAX_SUB_AGENT_DEPTH {
        return Err(TerminalError::new(format!(
            "sub-agent depth limit reached ({MAX_SUB_AGENT_DEPTH})"
        ))
        .into());
    }
    if children.len() >= MAX_SUB_AGENT_FAN_OUT {
        return Err(TerminalError::new(format!(
            "sub-agent fan-out limit reached ({MAX_SUB_AGENT_FAN_OUT})"
        ))
        .into());
    }

    let hash = task_hash(task, tool_subset);
    if children.iter().any(|child| child.task_hash == hash) {
        return Err(TerminalError::new(
            "duplicate sub-agent task detected (loop prevention)".to_string(),
        )
        .into());
    }

    Ok(hash)
}

/// Validates that a requested child budget can be reserved from its parent.
pub(crate) fn validate_dispatch_budget(
    requested_budget: u64,
    remaining_parent_budget: Option<u64>,
) -> Result<(), HandlerError> {
    if requested_budget == 0 {
        return Err(
            TerminalError::new("sub-agent budget must be greater than zero".to_string()).into(),
        );
    }

    if let Some(remaining) = remaining_parent_budget
        && requested_budget > remaining
    {
        return Err(TerminalError::new(format!(
            "sub-agent budget request ({requested_budget}) exceeds remaining parent budget ({remaining})"
        ))
        .into());
    }

    Ok(())
}

/// Returns the parent budget remaining after reserving the requested child budget.
pub(crate) fn reserve_child_budget(
    remaining_parent_budget: u64,
    requested_budget: u64,
) -> Result<u64, HandlerError> {
    validate_dispatch_budget(requested_budget, Some(remaining_parent_budget))?;
    Ok(remaining_parent_budget - requested_budget)
}

/// Returns the parent budget after refunding any unused child reservation.
#[must_use]
pub(crate) fn refund_child_budget(
    current_parent_budget: u64,
    requested_budget: u64,
    child_tokens_used: u64,
) -> u64 {
    current_parent_budget.saturating_add(requested_budget.saturating_sub(child_tokens_used))
}

/// Returns whether the given child id is owned by this parent state.
pub(crate) fn child_is_owned(children: &[SubAgentChildRef], sub_agent_id: &str) -> bool {
    children.iter().any(|child| child.id == sub_agent_id)
}

/// Removes a completed or cancelled child reference from parent state.
pub(crate) fn remove_child_ref(children: &mut Vec<SubAgentChildRef>, sub_agent_id: &str) {
    children.retain(|child| child.id != sub_agent_id);
}

pub(crate) fn child_agent_path(parent_key: &str, sub_id: &str, task_name: Option<&str>) -> String {
    let segment = task_name
        .map(sanitize_path_segment)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| sub_id.to_string());
    format!("/{parent_key}/{segment}")
}

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch == '-' || ch == '_' {
                Some(ch)
            } else if ch.is_whitespace() || ch == '/' {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Converts a completed child result into the synthetic tool output returned to the parent LLM.
#[must_use]
pub fn sub_agent_result_tool_output(result: &SubAgentResult) -> ToolOutput {
    if result.success {
        return ToolOutput::text(
            format!(
                "Sub-agent {} completed successfully.\n{}",
                result.sub_agent_id,
                truncate_result_text(&result.output)
            ),
            Duration::ZERO,
        );
    }

    let detail = result
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(result.output.as_str());
    ToolOutput::error(
        format!(
            "Sub-agent {} failed: {}",
            result.sub_agent_id,
            truncate_result_text(detail)
        ),
        Duration::ZERO,
    )
}

const MAX_SUB_AGENT_RESULT_CHARS: usize = 12_000;

fn truncate_result_text(value: &str) -> String {
    let Some((cutoff, _)) = value.char_indices().nth(MAX_SUB_AGENT_RESULT_CHARS) else {
        return value.to_string();
    };

    let mut truncated = value[..cutoff].to_string();
    truncated.push_str("\n\n[truncated sub-agent result]");
    truncated
}

#[cfg(test)]
mod tests {
    use moa_core::SubAgentChildRef;

    use super::{
        MAX_SUB_AGENT_DEPTH, MAX_SUB_AGENT_FAN_OUT, MAX_SUB_AGENT_RESULT_CHARS,
        refund_child_budget, reserve_child_budget, sub_agent_result_tool_output, task_hash,
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
        assert_eq!(left, "926925371cadf8cf");
    }

    #[test]
    fn validate_dispatch_limits_rejects_depth_overflow() {
        let error = validate_dispatch_limits(MAX_SUB_AGENT_DEPTH, &[], "task", &[])
            .expect_err("depth limit should fail");

        assert!(format!("{error:?}").contains("depth limit"));
    }

    #[test]
    fn validate_dispatch_limits_rejects_fan_out_overflow() {
        let children = (0..MAX_SUB_AGENT_FAN_OUT)
            .map(|index| SubAgentChildRef {
                id: format!("child-{index}"),
                task_hash: format!("hash-{index}"),
                budget_tokens: 0,
            })
            .collect::<Vec<_>>();
        let error = validate_dispatch_limits(0, &children, "task", &[])
            .expect_err("fan-out limit should fail");

        assert!(format!("{error:?}").contains("fan-out limit"));
    }

    #[test]
    fn validate_dispatch_limits_rejects_duplicate_hashes() {
        let existing_hash = task_hash("repeat", &["bash".to_string()]);
        let children = vec![SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: existing_hash,
            budget_tokens: 0,
        }];
        let error = validate_dispatch_limits(0, &children, "repeat", &["bash".to_string()])
            .expect_err("duplicate task hash should fail");

        assert!(format!("{error:?}").contains("duplicate sub-agent task"));
    }

    #[test]
    fn validate_dispatch_limits_allows_deepest_runnable_child() {
        // Pins: max depth is the deepest child that may run, not an off-by-one rejected state.
        let hash = validate_dispatch_limits(MAX_SUB_AGENT_DEPTH - 1, &[], "task", &[])
            .expect("parent just before max depth should be able to create the deepest child");

        assert_eq!(hash, task_hash("task", &[]));
    }

    #[test]
    fn validate_dispatch_budget_rejects_zero_and_over_reservation() {
        // Pins: child dispatch cannot silently reserve no budget or more than the parent has left.
        let zero_error = validate_dispatch_budget(0, Some(100))
            .expect_err("zero-token child budgets should fail");
        let over_error = validate_dispatch_budget(101, Some(100))
            .expect_err("over-budget child dispatch should fail");

        assert!(format!("{zero_error:?}").contains("greater than zero"));
        assert!(format!("{over_error:?}").contains("exceeds remaining parent budget"));
    }

    #[test]
    fn child_budget_reservation_and_refund_are_zero_sum() {
        // Pins: parent budgets reserve requested child tokens and refund only the unused amount.
        let after_reserve =
            reserve_child_budget(1_000, 400).expect("reservation within budget should succeed");
        let after_refund = refund_child_budget(after_reserve, 400, 125);

        assert_eq!(after_reserve, 600);
        assert_eq!(after_refund, 875);
    }

    #[test]
    fn sub_agent_result_tool_output_truncates_oversized_success_payloads() {
        // Pins: child results cannot return unbounded synthetic tool output to the parent turn.
        let result = moa_core::SubAgentResult {
            sub_agent_id: "child-1".to_string(),
            success: true,
            output: "a".repeat(MAX_SUB_AGENT_RESULT_CHARS + 10),
            tokens_used: 42,
            tools_invoked: 1,
            error: None,
        };

        let output = sub_agent_result_tool_output(&result);
        let rendered = output.to_text();

        assert!(!output.is_error);
        let payload = rendered
            .strip_prefix("Sub-agent child-1 completed successfully.\n")
            .expect("successful output should include the sub-agent result header");
        let (visible_payload, marker) = payload
            .split_once("\n\n[truncated sub-agent result]")
            .expect("oversized output should include the truncation marker");
        assert_eq!(visible_payload.len(), MAX_SUB_AGENT_RESULT_CHARS);
        assert_eq!(marker, "");
    }

    #[test]
    fn child_ownership_and_removal_are_exact() {
        // Pins: v2 message/wait/cancel cannot target children outside the current parent registry.
        let mut children = vec![
            SubAgentChildRef {
                id: "child-a".to_string(),
                task_hash: "hash-a".to_string(),
                budget_tokens: 100,
            },
            SubAgentChildRef {
                id: "child-b".to_string(),
                task_hash: "hash-b".to_string(),
                budget_tokens: 200,
            },
        ];

        assert!(super::child_is_owned(&children, "child-a"));
        assert!(!super::child_is_owned(&children, "child-c"));
        super::remove_child_ref(&mut children, "child-a");
        assert_eq!(
            children,
            vec![SubAgentChildRef {
                id: "child-b".to_string(),
                task_hash: "hash-b".to_string(),
                budget_tokens: 200,
            }]
        );
    }

    #[test]
    fn child_agent_path_uses_sanitized_task_name_when_available() {
        // Pins: v2 spawn returns a stable model-visible path independent of raw UUID formatting.
        assert_eq!(
            super::child_agent_path(
                "session-1",
                "session-1-child",
                Some("Research Vendors/Cloud")
            ),
            "/session-1/research-vendors-cloud"
        );
        assert_eq!(
            super::child_agent_path("session-1", "session-1-child", Some("!!!")),
            "/session-1/session-1-child"
        );
    }
}

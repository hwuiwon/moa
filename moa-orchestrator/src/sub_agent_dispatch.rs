//! Helper functions for spawning sub-agent virtual objects and awaiting their results.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use moa_core::{
    DispatchSubAgentInput, ModelId, SessionId, SubAgentChildRef, SubAgentId, SubAgentResult,
    ToolOutput, UserId, WorkspaceId,
};
use restate_sdk::prelude::*;

use crate::objects::sub_agent::SubAgentClient;

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
    let mut hasher = DefaultHasher::new();
    task.hash(&mut hasher);

    let mut sorted = tool_subset.to_vec();
    sorted.sort();
    sorted.hash(&mut hasher);

    format!("{:016x}", hasher.finish())
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

/// Spawns a child sub-agent, waits durably for its result, and removes it from the parent state.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_sub_agent(
    ctx: &mut ObjectContext<'_>,
    children_key: &str,
    budget_key: Option<&str>,
    parent_session: SessionId,
    parent_sub_agent: Option<SubAgentId>,
    current_depth: u32,
    request: DispatchSubAgentInput,
    workspace_id: WorkspaceId,
    user_id: UserId,
    model: ModelId,
) -> Result<DispatchedSubAgent, HandlerError> {
    let mut children = ctx
        .get::<Json<Vec<SubAgentChildRef>>>(children_key)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    let hash = validate_dispatch_limits(
        current_depth,
        &children,
        request.task.as_str(),
        &request.tool_subset,
    )?;

    let parent_key = ctx.key().to_string();
    let sub_id = format!("{parent_key}-{}", ctx.rand_uuid());
    children.push(SubAgentChildRef {
        id: sub_id.clone(),
        task_hash: hash,
    });
    ctx.set(children_key, Json::from(children));

    if let Some(budget_key) = budget_key {
        let remaining = ctx
            .get::<Json<u64>>(budget_key)
            .await?
            .map(Json::into_inner)
            .unwrap_or(u64::MAX);
        ctx.set(
            budget_key,
            Json::from(remaining.saturating_sub(request.budget_tokens)),
        );
    }

    let (awakeable_id, result_future) = ctx.awakeable::<String>();
    let initial = request.into_initial_message(
        parent_session,
        parent_sub_agent,
        current_depth + 1,
        awakeable_id,
        workspace_id,
        user_id,
        model,
    );

    ctx.object_client::<SubAgentClient>(sub_id.clone())
        .post_message(Json::from(initial))
        .send();

    let result = parse_sub_agent_result(&result_future.await?)?;

    let mut children = ctx
        .get::<Json<Vec<SubAgentChildRef>>>(children_key)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    children.retain(|child| child.id != sub_id);
    if children.is_empty() {
        ctx.clear(children_key);
    } else {
        ctx.set(children_key, Json::from(children));
    }

    Ok(DispatchedSubAgent { id: sub_id, result })
}

/// Converts a completed child result into the synthetic tool output returned to the parent LLM.
#[must_use]
pub fn sub_agent_result_tool_output(result: &SubAgentResult) -> ToolOutput {
    if result.success {
        return ToolOutput::text(
            format!(
                "Sub-agent {} completed successfully.\n{}",
                result.sub_agent_id, result.output
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
        format!("Sub-agent {} failed: {detail}", result.sub_agent_id),
        Duration::ZERO,
    )
}

fn parse_sub_agent_result(raw: &str) -> Result<SubAgentResult, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize sub-agent result from awakeable: {error}"
        ))
        .into()
    })
}

#[cfg(test)]
mod tests {
    use moa_core::SubAgentChildRef;

    use super::{MAX_SUB_AGENT_DEPTH, MAX_SUB_AGENT_FAN_OUT, task_hash, validate_dispatch_limits};

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
        }];
        let error = validate_dispatch_limits(0, &children, "repeat", &["bash".to_string()])
            .expect_err("duplicate task hash should fail");

        assert!(format!("{error:?}").contains("duplicate sub-agent task"));
    }
}

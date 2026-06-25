//! Shared runtime for MOA's sub-agent delegation tool surface.

use std::time::Duration;

use moa_core::wire::AppendEventRequest;
use moa_core::{
    AttachSubAgentResultWaiterInput, CancelSubAgentInput, ConsumeSubAgentChildResultInput,
    DelegationTool, Event, ListSubAgentsInput, ListSubAgentsOutput, ListedSubAgent,
    MessageSubAgentInput, RemoveSubAgentResultWaiterInput, ReserveSubAgentInput, ReservedSubAgent,
    SessionId, SessionMeta, SpawnSubAgentInput, SpawnSubAgentOutput, SubAgentChildRef,
    SubAgentChildRequest, SubAgentId, SubAgentMessage, SubAgentState, SubAgentStatus,
    SubAgentTerminalResult, ToolOutput, UserId, WaitSubAgentInput, WaitSubAgentOutput,
};
use restate_sdk::prelude::*;
use serde::Serialize;
use tracing::Instrument;

use crate::objects::session::SessionClient;
use crate::objects::sub_agent::SubAgentClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::sub_agent_dispatch::{
    child_agent_path, child_is_owned, validate_dispatch_budget, validate_dispatch_limits,
};

/// Maximum wait accepted by the v2 wait tool.
pub(crate) const MAX_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Parent context for a delegation tool execution.
#[derive(Clone, Copy)]
pub(crate) enum DelegationParent<'a> {
    /// Top-level session turn workflow is executing the tool.
    RootSession {
        /// Root session receiving events.
        session_id: SessionId,
        /// Session metadata used to initialize root children.
        meta: &'a SessionMeta,
    },
    /// Sub-agent turn workflow is executing the tool.
    SubAgent {
        /// Parent sub-agent that owns the child registry.
        sub_agent_id: &'a str,
        /// Root session receiving events.
        session_id: SessionId,
    },
}

impl DelegationParent<'_> {
    fn session_id(self) -> SessionId {
        match self {
            Self::RootSession { session_id, .. } | Self::SubAgent { session_id, .. } => session_id,
        }
    }

    fn parent_sub_agent_id(&self) -> Option<&str> {
        match self {
            Self::RootSession { .. } => None,
            Self::SubAgent { sub_agent_id, .. } => Some(sub_agent_id),
        }
    }
}

/// Executes one typed delegation tool call for either a root session or sub-agent parent.
pub(crate) async fn execute_delegation_tool(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    tool: DelegationTool,
) -> Result<ToolOutput, HandlerError> {
    let output = match tool {
        DelegationTool::Spawn(input) => {
            spawn_output(spawn_child_detached(ctx, parent, input).await?)
        }
        DelegationTool::Wait(input) => wait_output(wait_child(ctx, parent, input).await?),
        DelegationTool::Message(input) => {
            let sub_agent_id = input.sub_agent_id.clone();
            message_child(ctx, parent, input).await?;
            message_output(&sub_agent_id)
        }
        DelegationTool::List(input) => list_output(list_children(ctx, parent, input).await?),
        DelegationTool::Cancel(input) => {
            let sub_agent_id = input.sub_agent_id.clone();
            cancel_child(ctx, parent, input).await?;
            cancel_output(&sub_agent_id)
        }
    };
    Ok(output)
}

/// Returns whether a sub-agent state is terminal.
#[must_use]
pub(crate) fn is_terminal_sub_agent_state(state: SubAgentState) -> bool {
    matches!(
        state,
        SubAgentState::Completed | SubAgentState::Failed | SubAgentState::Cancelled
    )
}

/// Clamps a model-requested wait timeout to the supported bound.
#[must_use]
pub(crate) fn clamp_wait_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.min(MAX_WAIT_TIMEOUT_MS)
}

/// Converts a status projection into the v2 list entry shape.
#[must_use]
pub(crate) fn listed_sub_agent(sub_agent_id: SubAgentId, status: SubAgentStatus) -> ListedSubAgent {
    ListedSubAgent {
        sub_agent_id,
        state: status.state,
        depth: status.depth,
        tokens_used: status.tokens_used,
        budget_remaining: status.budget_remaining,
    }
}

/// Builds a structured success output for `spawn_sub_agent`.
pub(crate) fn spawn_output(output: SpawnSubAgentOutput) -> ToolOutput {
    json_tool_output(
        format!(
            "Spawned sub-agent {} at {} with status {:?}.",
            output.sub_agent_id, output.path, output.status
        ),
        output,
    )
}

/// Builds a structured success output for `list_sub_agents`.
pub(crate) fn list_output(output: ListSubAgentsOutput) -> ToolOutput {
    let count = output.sub_agents.len();
    json_tool_output(format!("Found {count} child sub-agent(s)."), output)
}

/// Builds a structured success output for `wait_sub_agent`.
pub(crate) fn wait_output(output: WaitSubAgentOutput) -> ToolOutput {
    let summary = if output.timed_out {
        format!(
            "Sub-agent {} is still {:?}; wait timed out.",
            output.sub_agent_id, output.state
        )
    } else {
        format!(
            "Sub-agent {} reached {:?}.",
            output.sub_agent_id, output.state
        )
    };
    json_tool_output(summary, output)
}

/// Builds a structured success output for `message_sub_agent`.
pub(crate) fn message_output(sub_agent_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Sent follow-up message to sub-agent {sub_agent_id}."),
        Duration::ZERO,
    )
}

/// Builds a structured success output for `cancel_sub_agent`.
pub(crate) fn cancel_output(sub_agent_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Cancellation requested for sub-agent {sub_agent_id}."),
        Duration::ZERO,
    )
}

async fn spawn_child_detached(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    request: SpawnSubAgentInput,
) -> Result<SpawnSubAgentOutput, HandlerError> {
    let task_name = request.task_name.clone();
    let child_request = SubAgentChildRequest {
        task: request.task,
        tool_subset: request.tool_subset,
        budget_tokens: request.budget_tokens,
        max_turns: request.max_turns,
    };
    let reservation =
        reserve_and_start_child(ctx, parent, child_request, task_name, "spawn_sub_agent_id")
            .await?;

    Ok(SpawnSubAgentOutput {
        sub_agent_id: reservation.child_ref.id,
        path: reservation.path,
        status: SubAgentState::Running,
    })
}

async fn reserve_and_start_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    request: SubAgentChildRequest,
    task_name: Option<String>,
    idempotency_step: &'static str,
) -> Result<ReservedSubAgent, HandlerError> {
    let task = request.task.clone();
    let budget_tokens = request.budget_tokens;
    let reservation = match parent {
        DelegationParent::RootSession { session_id, meta } => {
            reserve_root_child(
                ctx,
                session_id,
                meta,
                request,
                task_name.clone(),
                idempotency_step,
            )
            .await?
        }
        DelegationParent::SubAgent { sub_agent_id, .. } => ctx
            .object_client::<SubAgentClient>(sub_agent_id.to_string())
            .reserve_child(Json::from(ReserveSubAgentInput {
                request,
                task_name: task_name.clone(),
            }))
            .call()
            .await?
            .into_inner(),
    };

    ctx.object_client::<SubAgentClient>(reservation.child_ref.id.clone())
        .post_message(Json::from(reservation.initial_message.clone()))
        .send();
    append_child_spawned_event(ctx, parent, &reservation, task, budget_tokens).await?;
    Ok(reservation)
}

#[allow(clippy::too_many_arguments)]
async fn reserve_root_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    request: SubAgentChildRequest,
    task_name: Option<String>,
    idempotency_step: &'static str,
) -> Result<ReservedSubAgent, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    let hash = validate_dispatch_limits(0, &children, &request.task, &request.tool_subset)?;
    validate_dispatch_budget(request.budget_tokens, None)?;
    let sub_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(uuid::Uuid::now_v7().to_string())) })
        .name(idempotency_step)
        .await?
        .into_inner();
    let sub_id = format!("{}-{sub_id}", ctx.key());
    let child_ref = SubAgentChildRef {
        id: sub_id.clone(),
        task_hash: hash,
        budget_tokens: request.budget_tokens,
        terminal: None,
    };
    register_session_child(ctx, session_id, child_ref.clone()).await?;

    let path = child_agent_path(ctx.key(), &sub_id, task_name.as_deref());
    let task = request.task.clone();
    let budget_tokens = request.budget_tokens;
    let initial_message = request.into_initial_message(
        session_id,
        None,
        1,
        meta.tenant_id,
        storage_user_id(meta),
        meta.model.clone(),
    );

    Ok(ReservedSubAgent {
        child_ref,
        initial_message,
        path,
        task,
        budget_tokens,
    })
}

fn storage_user_id(meta: &SessionMeta) -> UserId {
    let id = meta
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .unwrap_or_else(|| format!("tenant:{}", meta.tenant_id));
    UserId::new(id)
}

async fn wait_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: WaitSubAgentInput,
) -> Result<WaitSubAgentOutput, HandlerError> {
    let timeout_ms = clamp_wait_timeout_ms(input.timeout_ms);
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.sub_agent_id).await?
    {
        return Ok(wait_terminal_output(input.sub_agent_id, terminal));
    }

    ensure_child_owned(ctx, parent, &input.sub_agent_id).await?;
    let (awakeable_id, terminal_future) = ctx.awakeable::<String>();
    let attached = ctx
        .object_client::<SubAgentClient>(input.sub_agent_id.clone())
        .attach_result_waiter(Json::from(AttachSubAgentResultWaiterInput {
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?
        .into_inner();
    if let Some(terminal) = attached.terminal {
        let _ = consume_parent_cached_terminal(ctx, parent, &input.sub_agent_id).await?;
        return Ok(wait_terminal_output(input.sub_agent_id, terminal));
    }

    if timeout_ms == 0 {
        return wait_timed_out(ctx, parent, input, awakeable_id).await;
    }

    restate_sdk::select! {
        terminal = terminal_future => {
            let terminal = parse_terminal_result(&terminal?)?;
            let _ = consume_parent_cached_terminal(ctx, parent, &input.sub_agent_id).await?;
            Ok(wait_terminal_output(input.sub_agent_id, terminal))
        },
        _ = ctx.sleep(Duration::from_millis(timeout_ms)) => {
            wait_timed_out(ctx, parent, input, awakeable_id).await
        }
    }
}

async fn wait_timed_out(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: WaitSubAgentInput,
    awakeable_id: String,
) -> Result<WaitSubAgentOutput, HandlerError> {
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.sub_agent_id).await?
    {
        return Ok(wait_terminal_output(input.sub_agent_id, terminal));
    }
    ctx.object_client::<SubAgentClient>(input.sub_agent_id.clone())
        .remove_result_waiter(Json::from(RemoveSubAgentResultWaiterInput { awakeable_id }))
        .call()
        .await?;
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.sub_agent_id).await?
    {
        return Ok(wait_terminal_output(input.sub_agent_id, terminal));
    }
    let status = ctx
        .object_client::<SubAgentClient>(input.sub_agent_id.clone())
        .status()
        .call()
        .await?
        .into_inner();
    Ok(WaitSubAgentOutput {
        sub_agent_id: input.sub_agent_id,
        state: status.state,
        result: None,
        timed_out: true,
    })
}

fn wait_terminal_output(
    sub_agent_id: SubAgentId,
    terminal: SubAgentTerminalResult,
) -> WaitSubAgentOutput {
    WaitSubAgentOutput {
        sub_agent_id,
        state: terminal.state,
        result: Some(terminal.result),
        timed_out: false,
    }
}

async fn message_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: MessageSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(sub_agent_id.clone())
        .post_message(Json::from(SubAgentMessage::FollowUp {
            text: input.text.clone(),
        }))
        .send();
    append_session_event(
        ctx,
        parent.session_id(),
        Event::SubAgentMessageSent {
            sub_agent_id,
            parent_sub_agent_id: parent.parent_sub_agent_id().map(ToOwned::to_owned),
            text: input.text,
        },
    )
    .await?;
    Ok(())
}

async fn list_children(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    _input: ListSubAgentsInput,
) -> Result<ListSubAgentsOutput, HandlerError> {
    let children = child_refs(ctx, parent).await?;
    let mut sub_agents = Vec::with_capacity(children.len());
    for child in children {
        let status = ctx
            .object_client::<SubAgentClient>(child.id.clone())
            .status()
            .call()
            .await?
            .into_inner();
        sub_agents.push(listed_sub_agent(child.id, status));
    }
    Ok(ListSubAgentsOutput { sub_agents })
}

async fn cancel_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: CancelSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(input.sub_agent_id)
        .cancel(input.reason)
        .send();
    append_session_event(
        ctx,
        parent.session_id(),
        Event::SubAgentStatusChanged {
            sub_agent_id,
            from: None,
            to: SubAgentState::Cancelled,
            summary: Some(match parent.parent_sub_agent_id() {
                Some(_) => "cancel requested by parent sub-agent".to_string(),
                None => "cancel requested by parent".to_string(),
            }),
        },
    )
    .await?;
    Ok(())
}

async fn consume_parent_cached_terminal(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    sub_agent_id: &str,
) -> Result<Option<SubAgentTerminalResult>, HandlerError> {
    let input = Json::from(ConsumeSubAgentChildResultInput {
        sub_agent_id: sub_agent_id.to_string(),
    });
    let terminal = match parent {
        DelegationParent::RootSession { session_id, .. } => {
            ctx.object_client::<SessionClient>(session_id.to_string())
                .consume_child_result(input)
                .call()
                .await?
                .into_inner()
                .terminal
        }
        DelegationParent::SubAgent { sub_agent_id, .. } => {
            ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
                .consume_child_result(input)
                .call()
                .await?
                .into_inner()
                .terminal
        }
    };
    Ok(terminal)
}

async fn ensure_child_owned(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    sub_agent_id: &str,
) -> Result<(), HandlerError> {
    let children = child_refs(ctx, parent).await?;
    if child_is_owned(&children, sub_agent_id) {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "sub-agent {sub_agent_id} is not owned by this parent"
    ))
    .into())
}

async fn child_refs(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
) -> Result<Vec<SubAgentChildRef>, HandlerError> {
    match parent {
        DelegationParent::RootSession { session_id, .. } => {
            session_child_refs(ctx, session_id).await
        }
        DelegationParent::SubAgent { sub_agent_id, .. } => Ok(ctx
            .object_client::<SubAgentClient>(sub_agent_id.to_string())
            .child_refs()
            .call()
            .await?
            .into_inner()),
    }
}

async fn session_child_refs(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<SubAgentChildRef>, HandlerError> {
    Ok(ctx
        .object_client::<SessionClient>(session_id.to_string())
        .child_refs()
        .call()
        .await?
        .into_inner())
}

async fn register_session_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    child: SubAgentChildRef,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .register_child(Json::from(child))
        .call()
        .await?;
    Ok(())
}

async fn append_child_spawned_event(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    reservation: &ReservedSubAgent,
    task: String,
    budget_tokens: u64,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        parent.session_id(),
        Event::SubAgentSpawned {
            sub_agent_id: reservation.child_ref.id.clone(),
            parent_sub_agent_id: parent.parent_sub_agent_id().map(ToOwned::to_owned),
            path: reservation.path.clone(),
            task,
            budget_tokens,
        },
    )
    .await?;
    Ok(())
}

async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, HandlerError> {
    let persist_span = moa_observability::restate_observability::event_persist_span(1);
    let sequence_num = ctx
        .service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    Ok(sequence_num)
}

fn parse_terminal_result(raw: &str) -> Result<SubAgentTerminalResult, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize sub-agent terminal result from awakeable: {error}"
        ))
        .into()
    })
}

fn json_tool_output(summary: impl Into<String>, value: impl Serialize) -> ToolOutput {
    let data = serde_json::to_value(value).unwrap_or_else(|error| {
        serde_json::json!({
            "serialization_error": error.to_string()
        })
    });
    ToolOutput::json(summary, data, Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use moa_core::{
        SpawnSubAgentOutput, SubAgentResult, SubAgentState, SubAgentStatus, ToolContent,
        WaitSubAgentOutput,
    };

    use super::{
        MAX_WAIT_TIMEOUT_MS, clamp_wait_timeout_ms, is_terminal_sub_agent_state, listed_sub_agent,
        spawn_output, wait_output,
    };

    #[test]
    fn wait_timeout_is_clamped_to_supported_bound() {
        // Pins: model-requested waits cannot block a turn longer than the supported max.
        assert_eq!(clamp_wait_timeout_ms(0), 0);
        assert_eq!(clamp_wait_timeout_ms(1_000), 1_000);
        assert_eq!(
            clamp_wait_timeout_ms(MAX_WAIT_TIMEOUT_MS + 1),
            MAX_WAIT_TIMEOUT_MS
        );
    }

    #[test]
    fn terminal_state_detection_matches_sub_agent_lifecycle() {
        // Pins: v2 wait/list behavior agrees on which sub-agent statuses are terminal.
        assert!(!is_terminal_sub_agent_state(SubAgentState::Uninitialized));
        assert!(!is_terminal_sub_agent_state(SubAgentState::Running));
        assert!(is_terminal_sub_agent_state(SubAgentState::Completed));
        assert!(is_terminal_sub_agent_state(SubAgentState::Failed));
        assert!(is_terminal_sub_agent_state(SubAgentState::Cancelled));
    }

    #[test]
    fn listed_sub_agent_preserves_status_fields() {
        // Pins: list output is a stable projection of child status, not a lossy text summary.
        let listed = listed_sub_agent(
            "child-1".to_string(),
            SubAgentStatus {
                state: SubAgentState::Running,
                depth: 2,
                tokens_used: 11,
                budget_remaining: 22,
                active_children: vec!["grandchild".to_string()],
            },
        );

        assert_eq!(listed.sub_agent_id, "child-1");
        assert_eq!(listed.state, SubAgentState::Running);
        assert_eq!(listed.depth, 2);
        assert_eq!(listed.tokens_used, 11);
        assert_eq!(listed.budget_remaining, 22);
    }

    #[test]
    fn delegation_json_outputs_preserve_structured_payloads() {
        // Pins: delegation helpers return machine-readable payloads, not only text summaries.
        let spawn = spawn_output(SpawnSubAgentOutput {
            sub_agent_id: "child-1".to_string(),
            path: "/session/child-1".to_string(),
            status: SubAgentState::Running,
        });

        assert!(!spawn.is_error);
        assert_eq!(
            spawn
                .structured
                .as_ref()
                .and_then(|value| value.get("sub_agent_id"))
                .and_then(serde_json::Value::as_str),
            Some("child-1")
        );
        assert!(matches!(
            spawn.content.as_slice(),
            [ToolContent::Text { .. }, ToolContent::Json { .. }]
        ));

        let wait = wait_output(WaitSubAgentOutput {
            sub_agent_id: "child-1".to_string(),
            state: SubAgentState::Completed,
            result: Some(SubAgentResult {
                sub_agent_id: "child-1".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 17,
                tools_invoked: 2,
                error: None,
            }),
            timed_out: false,
        });

        assert!(!wait.is_error);
        assert_eq!(
            wait.structured
                .as_ref()
                .and_then(|value| value.get("result"))
                .and_then(|value| value.get("tokens_used"))
                .and_then(serde_json::Value::as_u64),
            Some(17)
        );
    }
}

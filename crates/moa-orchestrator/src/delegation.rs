//! Shared runtime for MOA's worker delegation tool surface.

use std::time::Duration;

use moa_core::wire::session_store::AppendEventRequest;
use moa_core::{
    AttachWorkerResultWaiterInput, CancelWorkerInput, ConsumeWorkerChildResultInput,
    DelegationTool, Event, ListWorkersInput, ListWorkersOutput, MessageWorkerInput,
    ProvideWorkerInputInput, RemoveWorkerResultWaiterInput, ReserveWorkerInput, ReservedWorker,
    SessionId, SessionMeta, SpawnWorkerInput, SpawnWorkerOutput, ToolOutput,
    TrustedSandboxFileManifestRef, UserId, WaitWorkerInput, WaitWorkerOutput, WorkerChildRef,
    WorkerChildRequest, WorkerId, WorkerMessage, WorkerProgressSummary, WorkerState,
    WorkerTerminalResult,
};
use restate_sdk::prelude::*;
use serde::Serialize;
use tracing::Instrument;

use crate::objects::session::{
    ChildProgressFetch, SessionClient, plan_child_progress_fan_in, terminal_result_summary,
};
use crate::objects::worker::WorkerClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::worker_dispatch::{
    MAX_WORKER_FAN_OUT, child_agent_path, child_is_owned, validate_dispatch_budget,
    validate_dispatch_limits,
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
    /// Worker turn workflow is executing the tool.
    Worker {
        /// Parent worker that owns the child registry.
        worker_id: &'a str,
        /// Root session receiving events.
        session_id: SessionId,
    },
}

impl DelegationParent<'_> {
    fn session_id(self) -> SessionId {
        match self {
            Self::RootSession { session_id, .. } | Self::Worker { session_id, .. } => session_id,
        }
    }

    fn parent_worker_id(&self) -> Option<&str> {
        match self {
            Self::RootSession { .. } => None,
            Self::Worker { worker_id, .. } => Some(worker_id),
        }
    }
}

/// Executes one typed delegation tool call for either a root session or worker parent.
pub(crate) async fn execute_delegation_tool(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    tool: DelegationTool,
    trusted_sandbox_manifest: Option<&TrustedSandboxFileManifestRef>,
) -> Result<ToolOutput, HandlerError> {
    let output = match tool {
        DelegationTool::Spawn(input) => {
            spawn_output(spawn_child_detached(ctx, parent, input, trusted_sandbox_manifest).await?)
        }
        DelegationTool::Wait(input) => wait_output(wait_child(ctx, parent, input).await?),
        DelegationTool::Message(input) => {
            let worker_id = input.worker_id.clone();
            message_child(ctx, parent, input).await?;
            message_output(&worker_id)
        }
        DelegationTool::List(input) => list_output(list_children(ctx, parent, input).await?),
        DelegationTool::Cancel(input) => {
            let worker_id = input.worker_id.clone();
            cancel_child(ctx, parent, input).await?;
            cancel_output(&worker_id)
        }
        DelegationTool::ProvideInput(input) => {
            let worker_id = input.worker_id.clone();
            provide_input_child(ctx, parent, input).await?;
            provide_input_output(&worker_id)
        }
    };
    Ok(output)
}

/// Returns whether a worker state is terminal.
#[must_use]
pub(crate) fn is_terminal_worker_state(state: WorkerState) -> bool {
    matches!(
        state,
        WorkerState::Completed | WorkerState::Failed | WorkerState::Cancelled
    )
}

/// Clamps a model-requested wait timeout to the supported bound.
#[must_use]
pub(crate) fn clamp_wait_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.min(MAX_WAIT_TIMEOUT_MS)
}

/// Builds a structured success output for `spawn_worker`.
pub(crate) fn spawn_output(output: SpawnWorkerOutput) -> ToolOutput {
    json_tool_output(
        format!(
            "Spawned worker {} at {} with status {:?}.",
            output.worker_id, output.path, output.status
        ),
        output,
    )
}

/// Builds a structured success output for `list_workers`.
pub(crate) fn list_output(output: ListWorkersOutput) -> ToolOutput {
    let count = output.child_progress.len();
    json_tool_output(format!("Found {count} child worker(s)."), output)
}

/// Builds a structured success output for `wait_worker`.
pub(crate) fn wait_output(output: WaitWorkerOutput) -> ToolOutput {
    let summary = if output.timed_out {
        format!(
            "Worker {} is still {:?}; wait timed out.",
            output.worker_id, output.state
        )
    } else {
        format!("Worker {} reached {:?}.", output.worker_id, output.state)
    };
    json_tool_output(summary, output)
}

/// Builds a structured success output for `message_worker`.
pub(crate) fn message_output(worker_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Sent follow-up message to worker {worker_id}."),
        Duration::ZERO,
    )
}

/// Builds a structured success output for `cancel_worker`.
pub(crate) fn cancel_output(worker_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Cancellation requested for worker {worker_id}."),
        Duration::ZERO,
    )
}

async fn spawn_child_detached(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    request: SpawnWorkerInput,
    trusted_sandbox_manifest: Option<&TrustedSandboxFileManifestRef>,
) -> Result<SpawnWorkerOutput, HandlerError> {
    let task_name = request.task_name.clone();
    let child_request = WorkerChildRequest {
        task: request.task,
        tool_subset: request.tool_subset,
        budget_tokens: request.budget_tokens,
        max_turns: request.max_turns,
        trusted_sandbox_manifest: trusted_sandbox_manifest.cloned(),
    };
    let reservation =
        reserve_and_start_child(ctx, parent, child_request, task_name, "spawn_worker_id").await?;

    Ok(SpawnWorkerOutput {
        worker_id: reservation.child_ref.id,
        path: reservation.path,
        status: WorkerState::Running,
    })
}

async fn reserve_and_start_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    request: WorkerChildRequest,
    task_name: Option<String>,
    idempotency_step: &'static str,
) -> Result<ReservedWorker, HandlerError> {
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
        DelegationParent::Worker { worker_id, .. } => ctx
            .object_client::<WorkerClient>(worker_id.to_string())
            .reserve_child(Json::from(ReserveWorkerInput {
                request,
                task_name: task_name.clone(),
            }))
            .call()
            .await?
            .into_inner(),
    };

    ctx.object_client::<WorkerClient>(reservation.child_ref.id.clone())
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
    request: WorkerChildRequest,
    task_name: Option<String>,
    idempotency_step: &'static str,
) -> Result<ReservedWorker, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    let hash = validate_dispatch_limits(0, &children, &request.task, &request.tool_subset)?;
    validate_dispatch_budget(request.budget_tokens, None)?;
    let sub_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(uuid::Uuid::now_v7().to_string())) })
        .name(idempotency_step)
        .await?
        .into_inner();
    let sub_id = format!("{}-{sub_id}", ctx.key());
    let child_ref = WorkerChildRef {
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

    Ok(ReservedWorker {
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
    input: WaitWorkerInput,
) -> Result<WaitWorkerOutput, HandlerError> {
    let timeout_ms = clamp_wait_timeout_ms(input.timeout_ms);
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.worker_id).await? {
        return Ok(wait_terminal_output(input.worker_id, terminal));
    }

    ensure_child_owned(ctx, parent, &input.worker_id).await?;
    let (awakeable_id, terminal_future) = ctx.awakeable::<String>();
    let attached = ctx
        .object_client::<WorkerClient>(input.worker_id.clone())
        .attach_result_waiter(Json::from(AttachWorkerResultWaiterInput {
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?
        .into_inner();
    if let Some(terminal) = attached.terminal {
        let _ = consume_parent_cached_terminal(ctx, parent, &input.worker_id).await?;
        return Ok(wait_terminal_output(input.worker_id, terminal));
    }

    if timeout_ms == 0 {
        return wait_timed_out(ctx, parent, input, awakeable_id).await;
    }

    restate_sdk::select! {
        terminal = terminal_future => {
            let terminal = parse_terminal_result(&terminal?)?;
            let _ = consume_parent_cached_terminal(ctx, parent, &input.worker_id).await?;
            Ok(wait_terminal_output(input.worker_id, terminal))
        },
        _ = ctx.sleep(Duration::from_millis(timeout_ms)) => {
            wait_timed_out(ctx, parent, input, awakeable_id).await
        }
    }
}

async fn wait_timed_out(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: WaitWorkerInput,
    awakeable_id: String,
) -> Result<WaitWorkerOutput, HandlerError> {
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.worker_id).await? {
        return Ok(wait_terminal_output(input.worker_id, terminal));
    }
    ctx.object_client::<WorkerClient>(input.worker_id.clone())
        .remove_result_waiter(Json::from(RemoveWorkerResultWaiterInput { awakeable_id }))
        .call()
        .await?;
    if let Some(terminal) = consume_parent_cached_terminal(ctx, parent, &input.worker_id).await? {
        return Ok(wait_terminal_output(input.worker_id, terminal));
    }
    let status = ctx
        .object_client::<WorkerClient>(input.worker_id.clone())
        .status()
        .call()
        .await?
        .into_inner();
    let progress = latest_child_progress(ctx, &input.worker_id).await;
    Ok(WaitWorkerOutput {
        worker_id: input.worker_id,
        state: status.state,
        result: None,
        timed_out: true,
        progress,
    })
}

/// Reads the latest compact summary for a still-active child for the wait output.
///
/// Additive to the wait sequence: a failed read is omitted rather than failing the
/// wait, since the timeout outcome itself is already determined.
async fn latest_child_progress(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
) -> Option<WorkerProgressSummary> {
    match ctx
        .object_client::<WorkerClient>(worker_id.to_string())
        .progress_summary()
        .call()
        .await
    {
        Ok(summary) => Some(summary.into_inner()),
        Err(error) => {
            tracing::warn!(
                worker_id = %worker_id,
                error = %error,
                "child progress summary unavailable for wait output"
            );
            None
        }
    }
}

fn wait_terminal_output(worker_id: WorkerId, terminal: WorkerTerminalResult) -> WaitWorkerOutput {
    let progress = Some(terminal_result_summary(worker_id.clone(), &terminal));
    WaitWorkerOutput {
        worker_id,
        state: terminal.state,
        result: Some(terminal.result),
        timed_out: false,
        progress,
    }
}

async fn message_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: MessageWorkerInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent, &input.worker_id).await?;
    let worker_id = input.worker_id.clone();
    ctx.object_client::<WorkerClient>(worker_id.clone())
        .post_message(Json::from(WorkerMessage::FollowUp {
            text: input.text.clone(),
        }))
        .send();
    append_session_event(
        ctx,
        parent.session_id(),
        Event::WorkerMessageSent {
            worker_id,
            parent_worker_id: parent.parent_worker_id().map(ToOwned::to_owned),
            text: input.text,
        },
    )
    .await?;
    Ok(())
}

/// Answers a child's `request_input` round-trip from the coordinator (or, via it, the user).
///
/// Reuses the existing parent→child message path: it sends `WorkerMessage::ProvideInput`,
/// which the child VO resolves against the awakeable registered under `input_request_id`,
/// unblocking the parked child turn. No command bus and no separate user-question route.
async fn provide_input_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: ProvideWorkerInputInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent, &input.worker_id).await?;
    let worker_id = input.worker_id.clone();
    ctx.object_client::<WorkerClient>(worker_id.clone())
        .post_message(Json::from(WorkerMessage::ProvideInput {
            input_request_id: input.input_request_id.clone(),
            text: input.text.clone(),
        }))
        .send();
    append_session_event(
        ctx,
        parent.session_id(),
        Event::WorkerMessageSent {
            worker_id,
            parent_worker_id: parent.parent_worker_id().map(ToOwned::to_owned),
            text: input.text,
        },
    )
    .await?;
    Ok(())
}

/// Builds a structured success output for `provide_worker_input`.
pub(crate) fn provide_input_output(worker_id: &str) -> ToolOutput {
    ToolOutput::text(
        format!("Provided input to worker {worker_id}."),
        Duration::ZERO,
    )
}

async fn list_children(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    _input: ListWorkersInput,
) -> Result<ListWorkersOutput, HandlerError> {
    let children = child_refs(ctx, parent).await?;
    let child_progress = collect_child_progress(ctx, &children).await;
    Ok(ListWorkersOutput { child_progress })
}

/// Builds the bounded, on-demand child-progress fan-in for `list_workers`.
///
/// Mirrors `Session/progress`: terminal children are synthesized from cached parent
/// refs with no live call, and at most `MAX_WORKER_FAN_OUT` active children are
/// read live via `Worker::progress_summary`, so the fan-in never walks an
/// unbounded tree. A child whose summary read fails is omitted rather than failing
/// the whole list call.
async fn collect_child_progress(
    ctx: &WorkflowContext<'_>,
    children: &[WorkerChildRef],
) -> Vec<WorkerProgressSummary> {
    let mut summaries = Vec::new();
    for item in plan_child_progress_fan_in(children, MAX_WORKER_FAN_OUT) {
        match item {
            ChildProgressFetch::Ready(summary) => summaries.push(summary),
            ChildProgressFetch::Fetch(child_id) => {
                match ctx
                    .object_client::<WorkerClient>(child_id.clone())
                    .progress_summary()
                    .call()
                    .await
                {
                    Ok(summary) => summaries.push(summary.into_inner()),
                    Err(error) => tracing::warn!(
                        child_id = %child_id,
                        error = %error,
                        "child progress summary unavailable; omitting from list fan-in"
                    ),
                }
            }
        }
    }
    summaries
}

async fn cancel_child(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
    input: CancelWorkerInput,
) -> Result<(), HandlerError> {
    ensure_child_owned(ctx, parent, &input.worker_id).await?;
    let worker_id = input.worker_id.clone();
    ctx.object_client::<WorkerClient>(input.worker_id)
        .cancel(input.reason)
        .send();
    append_session_event(
        ctx,
        parent.session_id(),
        Event::WorkerStatusChanged {
            worker_id,
            from: None,
            to: WorkerState::Cancelled,
            summary: Some(match parent.parent_worker_id() {
                Some(_) => "cancel requested by parent worker".to_string(),
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
    worker_id: &str,
) -> Result<Option<WorkerTerminalResult>, HandlerError> {
    let input = Json::from(ConsumeWorkerChildResultInput {
        worker_id: worker_id.to_string(),
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
        DelegationParent::Worker { worker_id, .. } => {
            ctx.object_client::<WorkerClient>(worker_id.to_string())
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
    worker_id: &str,
) -> Result<(), HandlerError> {
    let children = child_refs(ctx, parent).await?;
    if child_is_owned(&children, worker_id) {
        return Ok(());
    }
    Err(TerminalError::new(format!("worker {worker_id} is not owned by this parent")).into())
}

async fn child_refs(
    ctx: &WorkflowContext<'_>,
    parent: DelegationParent<'_>,
) -> Result<Vec<WorkerChildRef>, HandlerError> {
    match parent {
        DelegationParent::RootSession { session_id, .. } => {
            session_child_refs(ctx, session_id).await
        }
        DelegationParent::Worker { worker_id, .. } => Ok(ctx
            .object_client::<WorkerClient>(worker_id.to_string())
            .child_refs()
            .call()
            .await?
            .into_inner()),
    }
}

async fn session_child_refs(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<WorkerChildRef>, HandlerError> {
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
    child: WorkerChildRef,
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
    reservation: &ReservedWorker,
    task: String,
    budget_tokens: u64,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        parent.session_id(),
        Event::WorkerSpawned {
            worker_id: reservation.child_ref.id.clone(),
            parent_worker_id: parent.parent_worker_id().map(ToOwned::to_owned),
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
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: None,
        }))
        .call()
        .instrument(persist_span)
        .await?;
    Ok(sequence_num)
}

fn parse_terminal_result(raw: &str) -> Result<WorkerTerminalResult, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize worker terminal result from awakeable: {error}"
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
        ListWorkersOutput, SpawnWorkerOutput, ToolContent, WaitWorkerOutput, WorkerProgressSummary,
        WorkerResult, WorkerState, WorkerTerminalResult,
    };
    use serde_json::Value;

    use super::{
        MAX_WAIT_TIMEOUT_MS, clamp_wait_timeout_ms, is_terminal_worker_state, list_output,
        spawn_output, terminal_result_summary, wait_output, wait_terminal_output,
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
    fn terminal_state_detection_matches_worker_lifecycle() {
        // Pins: v2 wait/list behavior agrees on which worker statuses are terminal.
        assert!(!is_terminal_worker_state(WorkerState::Uninitialized));
        assert!(!is_terminal_worker_state(WorkerState::Running));
        assert!(is_terminal_worker_state(WorkerState::Completed));
        assert!(is_terminal_worker_state(WorkerState::Failed));
        assert!(is_terminal_worker_state(WorkerState::Cancelled));
    }

    #[test]
    fn delegation_json_outputs_preserve_structured_payloads() {
        // Pins: delegation helpers return machine-readable payloads, not only text summaries.
        let spawn = spawn_output(SpawnWorkerOutput {
            worker_id: "child-1".to_string(),
            path: "/session/child-1".to_string(),
            status: WorkerState::Running,
        });

        assert!(!spawn.is_error);
        assert_eq!(
            spawn
                .structured
                .as_ref()
                .and_then(|value| value.get("worker_id"))
                .and_then(serde_json::Value::as_str),
            Some("child-1")
        );
        assert!(matches!(
            spawn.content.as_slice(),
            [ToolContent::Text { .. }, ToolContent::Json { .. }]
        ));

        let wait = wait_output(WaitWorkerOutput {
            worker_id: "child-1".to_string(),
            state: WorkerState::Completed,
            result: Some(WorkerResult {
                worker_id: "child-1".to_string(),
                success: true,
                output: "done".to_string(),
                tokens_used: 17,
                tools_invoked: 2,
                error: None,
            }),
            timed_out: false,
            progress: None,
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

    #[test]
    fn wait_terminal_output_synthesizes_compact_progress_summary() {
        // Pins: a terminal wait carries a compact child progress summary synthesized
        // from the cached terminal result, with no live child read.
        let terminal = WorkerTerminalResult {
            state: WorkerState::Completed,
            result: WorkerResult {
                worker_id: "child-1".to_string(),
                success: true,
                output: "summarized 3 docs".to_string(),
                tokens_used: 42,
                tools_invoked: 5,
                error: None,
            },
        };

        let output = wait_terminal_output("child-1".to_string(), terminal);
        let progress = output
            .progress
            .as_ref()
            .expect("terminal wait carries a compact progress summary");
        assert_eq!(progress.worker_id, "child-1");
        assert_eq!(progress.state, WorkerState::Completed);
        assert_eq!(progress.tokens_used, 42);
        assert_eq!(progress.last_summary.as_deref(), Some("summarized 3 docs"));
        assert!(!progress.stale);

        // The structured payload serialized to the model carries the summary.
        let tool = wait_output(output);
        assert_eq!(
            tool.structured
                .as_ref()
                .and_then(|value| value.get("progress"))
                .and_then(|value| value.get("worker_id"))
                .and_then(Value::as_str),
            Some("child-1")
        );
    }

    #[test]
    fn wait_output_omits_progress_when_unavailable() {
        // Pins: the `progress` field is optional, so a payload without it decodes to None.
        let parsed: WaitWorkerOutput = serde_json::from_value(serde_json::json!({
            "worker_id": "child-1",
            "state": "running",
            "timed_out": true,
        }))
        .expect("wait output without progress deserializes");
        assert!(parsed.progress.is_none());
    }

    #[test]
    fn list_output_carries_child_progress_fan_in_summaries() {
        // Pins: list_workers surfaces compact per-child progress summaries
        // (terminal synthesized + active fan-in).
        let terminal = WorkerTerminalResult {
            state: WorkerState::Completed,
            result: WorkerResult {
                worker_id: "done-child".to_string(),
                success: true,
                output: "finished".to_string(),
                tokens_used: 8,
                tools_invoked: 1,
                error: None,
            },
        };
        let active = WorkerProgressSummary {
            worker_id: "live-child".to_string(),
            state: WorkerState::Running,
            active_turn_id: Some("turn-9".to_string()),
            last_summary: Some("searching pricing docs".to_string()),
            tokens_used: 30,
            budget_remaining: 70,
            last_heartbeat_at: None,
            stale: false,
            awaiting_input: false,
        };
        let output = ListWorkersOutput {
            child_progress: vec![
                terminal_result_summary("done-child".to_string(), &terminal),
                active,
            ],
        };

        // Serde round-trip: the structured payload is serialized to the model.
        let round_tripped: ListWorkersOutput =
            serde_json::from_value(serde_json::to_value(&output).expect("serialize"))
                .expect("deserialize");
        assert_eq!(round_tripped, output);

        let tool = list_output(output);
        let summaries = tool
            .structured
            .as_ref()
            .and_then(|value| value.get("child_progress"))
            .and_then(Value::as_array)
            .expect("child_progress array present in structured output");
        assert_eq!(summaries.len(), 2);
        // Terminal child is synthesized from the cached result (state + tokens).
        assert_eq!(
            summaries[0].get("worker_id").and_then(Value::as_str),
            Some("done-child")
        );
        assert_eq!(
            summaries[0].get("state").and_then(Value::as_str),
            Some("completed")
        );
        // Active child carries its live one-line summary.
        assert_eq!(
            summaries[1].get("last_summary").and_then(Value::as_str),
            Some("searching pricing docs")
        );
    }

    #[test]
    fn list_output_skips_empty_child_progress_on_the_wire() {
        // Pins: an empty fan-in serializes to `{}` and decodes back to an empty list,
        // so the wire payload stays compact.
        let parsed: ListWorkersOutput =
            serde_json::from_value(serde_json::json!({})).expect("empty list output deserializes");
        assert!(parsed.child_progress.is_empty());

        let serialized = serde_json::to_value(&parsed).expect("serialize");
        assert!(serialized.get("child_progress").is_none());
    }
}

//! Shared redacted execution-eval snapshot collection over production-owned state.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::ExecutionOperation;
use moa_core::events::Event;
use moa_core::types::events_stream::EventRange;
use moa_eval::execution::{
    ExecutionCapabilityCallObservation, ExecutionEvalSnapshot, ExecutionHarnessEvidence,
    ExecutionSessionEventSummary,
};
use moa_execution::{
    budget::BudgetLedger,
    repository::{
        ExecutionRepository, ExecutionRunRecord, ExecutionSchedulingSnapshot, ExecutionScope,
        ExecutionTaskCursor, ExecutionTaskPageRequest, ExecutionTaskRecord,
    },
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskProjection, ExecutionTaskStatus,
    },
    wire::ExecutionRunRequest,
};
use moa_test_support::execution_audits::load_execution_planning_audits;
use moa_test_support::{FixtureCapabilityController, TestApiClient};

const EVAL_TASK_PAGE_SIZE: u32 = 100;

/// Collects one strict, redacted eval snapshot from runtime-owned projections and public APIs.
pub(crate) async fn collect_execution_eval_snapshot(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    postgres_url: &str,
    client: &TestApiClient,
    request: &ExecutionRunRequest,
    capability_controller: Option<&FixtureCapabilityController>,
) -> Result<ExecutionEvalSnapshot> {
    let (scheduling_snapshot, task_records) =
        load_eval_runtime_state(repository, scope, request.run_uid).await?;
    let audits = load_execution_planning_audits(postgres_url, request.session_id).await?;
    let events = client
        .get_events(request.session_id, EventRange::all())
        .await
        .context("read raw durable session events")?;
    let harness = ExecutionHarnessEvidence {
        session_events: summarize_session_events(&events),
        capability_calls: capability_observations(&scheduling_snapshot, capability_controller)?,
        final_response: events.iter().rev().find_map(|record| match &record.event {
            Event::BrainResponse { text, .. } => Some(text.clone()),
            _ => None,
        }),
    };

    ExecutionEvalSnapshot::from_parts(scheduling_snapshot, task_records, audits, harness)
        .context("assemble strict execution eval snapshot")
}

/// Collects repository-only evidence for lower-level service tests that bypass Session admission.
#[allow(dead_code)]
pub(crate) async fn collect_repository_execution_eval_snapshot(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    postgres_url: &str,
    session_id: moa_core::types::identifiers::SessionId,
    run_uid: uuid::Uuid,
) -> Result<ExecutionEvalSnapshot> {
    let (scheduling_snapshot, task_records) =
        load_eval_runtime_state(repository, scope, run_uid).await?;
    let audits = load_execution_planning_audits(postgres_url, session_id).await?;
    ExecutionEvalSnapshot::from_parts(
        scheduling_snapshot,
        task_records,
        audits,
        ExecutionHarnessEvidence::default(),
    )
    .context("assemble repository-only execution eval snapshot")
}

async fn load_eval_runtime_state(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
) -> Result<(ExecutionSchedulingSnapshot, Vec<ExecutionTaskRecord>)> {
    let run = repository
        .load_run(scope, run_uid)
        .await
        .context("load bounded execution run projection")?
        .with_context(|| format!("execution run {run_uid} is not visible"))?;
    let task_records = list_all_task_records(repository, scope, run_uid).await?;
    let snapshot = ExecutionSchedulingSnapshot {
        catalog: run.catalog.clone(),
        authorization: run.authorization.clone(),
        pinned_instruction_skills: run.pinned_instruction_skills.clone(),
        budget_ledger: BudgetLedger {
            limit: run.approved_budget.clone(),
            reserved: run.reserved,
            consumed: run.consumed,
            overrun: run.budget_overrun,
        },
        projection: scheduling_projection(&run, &task_records),
        run,
    };
    Ok((snapshot, task_records))
}

fn scheduling_projection(
    run: &ExecutionRunRecord,
    tasks: &[ExecutionTaskRecord],
) -> ExecutionProjection {
    let task_projections = tasks
        .iter()
        .map(|task| ExecutionTaskProjection {
            task_id: task.task_id,
            node_id: task.node_id.clone(),
            item_key: task.item_key.clone(),
            status: task.status,
            attempt: task.attempt,
            generation: task.generation,
            input: task.input.clone(),
            outcome: task.current_outcome.clone(),
        })
        .collect::<Vec<_>>();
    let node_statuses = run
        .active_plan
        .definition
        .nodes
        .iter()
        .map(|node| {
            let node_tasks = tasks
                .iter()
                .filter(|task| task.node_id == node.id)
                .collect::<Vec<_>>();
            (
                node.id.clone(),
                persisted_node_status(&node.operation, &node_tasks),
            )
        })
        .collect::<BTreeMap<_, _>>();
    ExecutionProjection {
        plan_revision: run.plan_revision,
        node_statuses,
        tasks: task_projections,
    }
}

fn persisted_node_status(
    operation: &ExecutionOperation,
    tasks: &[&ExecutionTaskRecord],
) -> ExecutionNodeStatus {
    if tasks.is_empty() {
        return ExecutionNodeStatus::Pending;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::WaitingInput
                | ExecutionTaskStatus::WaitingReview
                | ExecutionTaskStatus::WaitingSignal
                | ExecutionTaskStatus::WaitingTimer
                | ExecutionTaskStatus::WaitingExternal
                | ExecutionTaskStatus::WaitingReplan
        )
    }) {
        return ExecutionNodeStatus::Waiting;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::Pending
                | ExecutionTaskStatus::Ready
                | ExecutionTaskStatus::Reserved
                | ExecutionTaskStatus::Dispatching
                | ExecutionTaskStatus::Running
        )
    }) {
        return ExecutionNodeStatus::Running;
    }
    if tasks.iter().any(|task| {
        matches!(
            task.status,
            ExecutionTaskStatus::Failed | ExecutionTaskStatus::UnknownOutcome
        )
    }) {
        return ExecutionNodeStatus::Failed;
    }
    if tasks
        .iter()
        .any(|task| task.status == ExecutionTaskStatus::Cancelled)
    {
        return ExecutionNodeStatus::Cancelled;
    }
    if matches!(
        operation,
        ExecutionOperation::Map { .. } | ExecutionOperation::Reduce { .. }
    ) {
        return ExecutionNodeStatus::Pending;
    }
    if tasks
        .iter()
        .all(|task| task.status == ExecutionTaskStatus::Skipped)
    {
        ExecutionNodeStatus::Skipped
    } else {
        ExecutionNodeStatus::Completed
    }
}

async fn list_all_task_records(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
) -> Result<Vec<moa_execution::repository::ExecutionTaskRecord>> {
    let mut records = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let page = repository
            .list_tasks(
                scope,
                run_uid,
                ExecutionTaskPageRequest {
                    limit: EVAL_TASK_PAGE_SIZE,
                    cursor,
                },
            )
            .await
            .context("paginate execution task records for eval")?;
        records.extend(page.tasks);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(records);
        };
        if !seen_cursors.insert(cursor_key(&next_cursor)) {
            bail!("execution task pagination repeated a cursor");
        }
        cursor = Some(next_cursor);
    }
}

fn cursor_key(cursor: &ExecutionTaskCursor) -> (String, String, uuid::Uuid) {
    (
        cursor.node_id.clone(),
        cursor.item_key.clone(),
        cursor.task_id.as_uuid(),
    )
}

fn summarize_session_events(
    events: &[moa_core::types::events_stream::EventRecord],
) -> ExecutionSessionEventSummary {
    let mut summary = ExecutionSessionEventSummary::default();
    for record in events {
        match record.event {
            Event::ExecutionRunStarted(_) => {
                summary.run_started = summary.run_started.saturating_add(1);
            }
            Event::ExecutionProgress(_) => {
                summary.progress = summary.progress.saturating_add(1);
            }
            Event::ExecutionInputRequired(_) => {
                summary.input_required = summary.input_required.saturating_add(1);
            }
            Event::ExecutionCompleted(_)
            | Event::ExecutionFailed { .. }
            | Event::ExecutionCancelled(_) => {
                summary.terminal = summary.terminal.saturating_add(1);
            }
            Event::Error { .. } => {
                summary.error = summary.error.saturating_add(1);
            }
            Event::SessionCreated { .. }
            | Event::SessionStatusChanged { .. }
            | Event::SessionChannelChanged { .. }
            | Event::SegmentStarted { .. }
            | Event::SegmentCompleted { .. }
            | Event::UserMessage { .. }
            | Event::QueuedMessage { .. }
            | Event::ExecutionSynthesisRequested(_)
            | Event::BrainThinking { .. }
            | Event::BrainResponse { .. }
            | Event::ProgressUpdate { .. }
            | Event::GuardrailCheck { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::ToolError { .. }
            | Event::PromptInjectionCircuitTransition { .. }
            | Event::ActionReviewRequested { .. }
            | Event::ActionReviewDecided { .. }
            | Event::ActionReviewTimedOut { .. }
            | Event::ActionReviewContinuationRequested { .. }
            | Event::WorkerSpawned { .. }
            | Event::WorkerMessageSent { .. }
            | Event::WorkerStatusChanged { .. }
            | Event::WorkerNotificationDelivered { .. }
            | Event::WorkerSignalReceived { .. }
            | Event::WorkerParentResumeRequested { .. }
            | Event::WorkerHeartbeatStale { .. }
            | Event::TurnMetrics { .. }
            | Event::MemoryRead { .. }
            | Event::MemoryWrite { .. }
            | Event::MemoryIngest { .. }
            | Event::Checkpoint { .. }
            | Event::CacheReport { .. }
            // Turn-level scheduling facts. This snapshot measures the execution-run
            // lifecycle, and folding a turn failure into `error` would double-count
            // the inner `Error` a failing run already records. Turn failures are
            // counted by `ConversationCost::failed_turns` instead.
            | Event::TurnFailed { .. }
            | Event::QueuedMessageRejected { .. }
            | Event::Warning { .. } => {}
        }
    }
    summary
}

fn capability_observations(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    controller: Option<&FixtureCapabilityController>,
) -> Result<Vec<ExecutionCapabilityCallObservation>> {
    let Some(controller) = controller else {
        return Ok(Vec::new());
    };
    controller
        .transport_attempts()
        .into_iter()
        .map(|attempt| {
            let reference = snapshot
                .catalog
                .capabilities
                .iter()
                // `attempt.capability` is the name the fixture SERVER was asked
                // for; the catalog keys connector tools under their
                // server-qualified reference.
                .find(|capability| {
                    capability.reference.name
                        == moa_hands::mcp_tool_reference("fixture-capability", &attempt.capability)
                })
                .map(|capability| capability.reference.clone())
                .with_context(|| {
                    format!(
                        "fixture capability {} is absent from the run catalog",
                        attempt.capability
                    )
                })?;
            Ok(ExecutionCapabilityCallObservation {
                logical_invocation_id: attempt.invocation_id,
                reference,
                item_key: (!attempt.item_key.is_empty()).then_some(attempt.item_key),
                replayed: attempt.is_replay,
            })
        })
        .collect()
}

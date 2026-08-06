//! Shared execution-service mutation handoff types and conversion helpers.

use super::*;

pub(super) fn scoped_catalog_error(
    error: crate::connector_catalog::ScopedConnectorCatalogError,
) -> HandlerError {
    TerminalError::new_with_code(
        409,
        format!("scoped connector catalog unavailable: {error}"),
    )
    .into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ExecutionMutationHandoff {
    wake_epoch: u64,
    task_ids_to_release: Vec<moa_execution::state::ExecutionTaskId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) enum ExecutionMutationAccepted {
    Accepted {
        response: ExecutionMutationResponse,
        handoff: ExecutionMutationHandoff,
    },
    Rejected {
        response: ExecutionMutationResponse,
    },
}

impl ExecutionMutationAccepted {
    pub(super) fn wake_epoch(&self) -> Option<u64> {
        match self {
            Self::Accepted { handoff, .. } => Some(handoff.wake_epoch),
            Self::Rejected { .. } => None,
        }
    }

    pub(super) fn task_ids_to_release(&self) -> &[moa_execution::state::ExecutionTaskId] {
        match self {
            Self::Accepted { handoff, .. } => &handoff.task_ids_to_release,
            Self::Rejected { .. } => &[],
        }
    }

    pub(super) fn with_task_ids_to_release(
        mut self,
        task_ids_to_release: Vec<moa_execution::state::ExecutionTaskId>,
    ) -> Self {
        if let Self::Accepted { handoff, .. } = &mut self {
            handoff.task_ids_to_release = task_ids_to_release;
        }
        self
    }

    pub(super) fn into_response(self) -> ExecutionMutationResponse {
        match self {
            Self::Accepted { response, .. } | Self::Rejected { response } => response,
        }
    }
}
pub(super) fn replan_evaluation_request(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    proposed_plan: &moa_execution::compiler::CanonicalExecutionPlan,
    proposed_estimate: ExecutionEstimate,
    remaining_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
    loop_evaluation: ReplanLoopEvaluationRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> ReplanEvaluationRequest {
    let mut seen_plan_hashes = BTreeSet::from([
        snapshot.run.initial_plan_hash,
        snapshot.run.active_plan_hash,
    ]);
    for entry in &snapshot.run.plan_history {
        if let Some(value) = entry.get("active_plan_hash").and_then(Value::as_str)
            && let Ok(hash) = value.parse()
        {
            seen_plan_hashes.insert(hash);
        }
    }
    ReplanEvaluationRequest {
        now,
        remaining_budget,
        proposed_estimate,
        proposed_plan_hash: proposed_plan.plan_hash,
        proposed_amendment_fingerprint: loop_evaluation.proposed_amendment_fingerprint,
        seen_plan_hashes,
        seen_amendment_fingerprints: loop_evaluation.seen_amendment_fingerprints,
        failure_fingerprint_counts: loop_evaluation.failure_fingerprint_counts,
        current_failure: loop_evaluation.current_failure,
        unresolved_requirement_ids: loop_evaluation.unresolved_requirement_ids,
        amendment: loop_evaluation.amendment,
        config: loop_evaluation.config,
    }
}

pub(super) fn replan_loop_evaluation_request(
    snapshot: &moa_execution::repository::ExecutionSchedulingSnapshot,
    proposed_amendment_fingerprint: ExecutionHash,
    amendment: PlanAmendment,
    config: ExecutionConfig,
    waiting_task: &ExecutionTaskProjection,
) -> moa_execution::Result<ReplanLoopEvaluationRequest> {
    let seen_amendment_fingerprints =
        durable_amendment_operation_fingerprints(&snapshot.run.plan_history)?;
    let failures = snapshot
        .projection
        .tasks
        .iter()
        .filter(|task| task.task_id != waiting_task.task_id)
        .filter_map(task_failure_fingerprint)
        .collect::<Vec<_>>();
    let current_failure = task_failure_fingerprint(waiting_task);
    let mut failure_fingerprint_counts =
        durable_failure_fingerprint_counts(&snapshot.run.plan_history);
    for failure in failures {
        if let Ok(fingerprint) = failure_fingerprint(&failure) {
            *failure_fingerprint_counts.entry(fingerprint).or_insert(0) += 1;
        }
    }
    let unresolved_requirement_ids = snapshot
        .run
        .goal
        .requirements
        .iter()
        .filter(|requirement| {
            !snapshot
                .run
                .active_plan
                .definition
                .nodes
                .iter()
                .any(|node| {
                    node.requirement_ids.contains(&requirement.id)
                        && snapshot.projection.node_statuses.get(&node.id)
                            == Some(&moa_execution::state::ExecutionNodeStatus::Completed)
                })
        })
        .map(|requirement| requirement.id.clone())
        .collect();
    Ok(ReplanLoopEvaluationRequest {
        proposed_amendment_fingerprint,
        seen_amendment_fingerprints,
        failure_fingerprint_counts,
        current_failure,
        unresolved_requirement_ids,
        amendment,
        config,
    })
}

pub(super) fn durable_amendment_operation_fingerprints(
    plan_history: &[Value],
) -> moa_execution::Result<BTreeSet<ExecutionHash>> {
    let mut fingerprints = BTreeSet::new();
    for entry in plan_history {
        let Some(amendment) = entry.get("amendment") else {
            continue;
        };
        let amendment =
            serde_json::from_value::<PlanAmendment>(amendment.clone()).map_err(|error| {
                moa_execution::Error::InvalidRepositoryData {
                    message: format!(
                        "persisted plan history contains an invalid amendment: {error}"
                    ),
                }
            })?;
        fingerprints.insert(amendment_operations_fingerprint(&amendment)?);
    }
    Ok(fingerprints)
}

pub(super) fn durable_failure_fingerprint_counts(
    plan_history: &[Value],
) -> BTreeMap<ExecutionHash, u32> {
    let mut counts: BTreeMap<ExecutionHash, u32> = BTreeMap::new();
    for entry in plan_history {
        let Some(fingerprint) = entry
            .get("failure_fingerprint")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<ExecutionHash>().ok())
        else {
            continue;
        };
        let count = entry
            .get("failure_fingerprint_count")
            .and_then(Value::as_u64)
            .map_or(1, |count| u32::try_from(count).unwrap_or(u32::MAX));
        counts
            .entry(fingerprint)
            .and_modify(|persisted| *persisted = (*persisted).max(count))
            .or_insert(count);
    }
    counts
}

pub(super) fn task_failure_fingerprint(
    task: &ExecutionTaskProjection,
) -> Option<FailureFingerprintInput> {
    let outcome = task.outcome.as_ref()?;
    let (class, message) = match &outcome.result {
        ExecutionTaskResult::Failed { class, message } => (class.clone(), message.clone()),
        ExecutionTaskResult::NeedsReplan { reason, .. } => {
            (ExecutionFailureClass::Terminal, reason.clone())
        }
        ExecutionTaskResult::UnknownOutcome { message } => {
            (ExecutionFailureClass::Terminal, message.clone())
        }
        ExecutionTaskResult::Completed { .. }
        | ExecutionTaskResult::NeedsInput { .. }
        | ExecutionTaskResult::Cancelled { .. } => return None,
    };
    Some(FailureFingerprintInput {
        class,
        node_id: task.node_id.clone(),
        capability_ref: None,
        message,
    })
}

pub(super) async fn mutation_from_transition(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
    transition: TransitionOutcome,
) -> Result<ExecutionMutationAccepted, HandlerError> {
    match transition {
        TransitionOutcome::Applied(task) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            let accepted = applied_mutation(&run);
            Ok(if task.status.is_terminal() {
                accepted.with_task_ids_to_release(vec![task.task_id])
            } else {
                accepted
            })
        }
        TransitionOutcome::RunApplied(_) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            Ok(applied_mutation(&run))
        }
        TransitionOutcome::AlreadyApplied(task) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            let accepted = replayed_mutation(&run);
            Ok(if task.status.is_terminal() {
                accepted.with_task_ids_to_release(vec![task.task_id])
            } else {
                accepted
            })
        }
        TransitionOutcome::RunAlreadyApplied(_) => {
            let run = repository
                .load_run(scope, run_uid)
                .await
                .map_err(execution_error)?
                .ok_or_else(|| TerminalError::new("execution run disappeared after transition"))?;
            Ok(replayed_mutation(&run))
        }
        TransitionOutcome::NotFound => Ok(not_found_mutation()),
        TransitionOutcome::Rejected(reason) => Ok(conflict_mutation(match reason {
            TransitionRejection::GenerationMismatch => ExecutionConflictReason::GenerationMismatch,
            TransitionRejection::InvalidTaskStatus
            | TransitionRejection::InvalidRunStatus
            | TransitionRejection::DeadlineElapsed
            | TransitionRejection::BudgetExceeded => ExecutionConflictReason::InvalidStatus,
            TransitionRejection::CounterOverflow => ExecutionConflictReason::AlreadyTerminal,
        })),
    }
}

pub(super) fn mutation_from_task_write(write: TaskOutcomeWrite) -> ExecutionMutationAccepted {
    match write {
        TaskOutcomeWrite::Applied { run, .. } => applied_mutation(&run),
        TaskOutcomeWrite::Replayed { run, .. } => replayed_mutation(&run),
        TaskOutcomeWrite::Rejected { reason, .. } => {
            use moa_execution::repository::TaskOutcomeRejection;
            conflict_mutation(match reason {
                TaskOutcomeRejection::StaleGeneration => {
                    ExecutionConflictReason::GenerationMismatch
                }
                TaskOutcomeRejection::TerminalTask | TaskOutcomeRejection::TerminalRun => {
                    ExecutionConflictReason::AlreadyTerminal
                }
                TaskOutcomeRejection::InvalidTaskStatus
                | TaskOutcomeRejection::NonCumulativeUsage
                | TaskOutcomeRejection::UnsupportedSchemaVersion => {
                    ExecutionConflictReason::InvalidStatus
                }
            })
        }
        TaskOutcomeWrite::NotFound => not_found_mutation(),
    }
}

pub(super) fn execution_scope(
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
) -> ExecutionScope {
    contact_id.map_or(ExecutionScope::Tenant { tenant_id }, |contact_id| {
        ExecutionScope::Contact {
            tenant_id,
            contact_id,
        }
    })
}

pub(super) fn verify_run_request(
    run: &ExecutionRunRecord,
    request: &ExecutionRunRequest,
) -> Result<(), HandlerError> {
    verify_run_scope(
        run,
        request.tenant_id,
        request.contact_id,
        request.session_id,
    )
}

pub(super) fn verify_run_scope(
    run: &ExecutionRunRecord,
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<moa_core::types::contact::ContactId>,
    session_id: moa_core::types::identifiers::SessionId,
) -> Result<(), HandlerError> {
    if run.tenant_id == tenant_id && run.contact_id == contact_id && run.session_id == session_id {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(409, "execution scope mismatch").into())
    }
}

pub(super) fn verify_start_replay(
    run: &ExecutionRunRecord,
    request: &ExecutionStartRequest,
    snapshot: &ExecutionPlanningContextSnapshot,
) -> Result<(), HandlerError> {
    let expected_hash = request
        .planning_context_hash
        .parse::<ExecutionHash>()
        .map_err(execution_error)?;
    if run.originating_user_sequence_num != request.originating_user_sequence_num
        || run.planning_context_uid != request.planning_context_uid
        || run.planning_context_hash != expected_hash
        || run.owner_user_id != snapshot.owner_user_id
        || run.goal != request.compiled.goal
        || run.initial_plan != request.compiled.plan
        || run.catalog != snapshot.catalog
        || run.authorization != snapshot.authorization
        || run.pinned_instruction_skills != snapshot.pinned_instruction_skills
        || run.source_provenance != request.source_provenance
        || run.input != request.run_input
        || run.approved_budget != snapshot.budget
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution start idempotency key conflicts with immutable admission input",
        )
        .into());
    }
    Ok(())
}

pub(super) fn run_summary(run: &ExecutionRunRecord) -> ExecutionRunSummary {
    ExecutionRunSummary {
        run_uid: run.run_uid,
        session_id: run.session_id,
        originating_user_sequence_num: run.originating_user_sequence_num,
        status: run.status,
        source_kind: run.source_kind,
        skill_template_ref: run.skill_template_ref.clone(),
        skill_template_revision_uid: run.skill_template_revision_uid,
        plan_revision: run.plan_revision,
        total_tasks: run.progress_total_tasks,
        completed_tasks: run.progress_completed_tasks,
        failed_tasks: run.progress_failed_tasks,
        budget_ledger: BudgetLedger {
            limit: run.approved_budget.clone(),
            reserved: run.reserved,
            consumed: run.consumed,
            overrun: run.budget_overrun,
        },
        created_at: run.created_at,
        queued_at: run.queued_at,
        updated_at: run.updated_at,
        completed_at: run.completed_at,
        terminal_evidence: run.terminal_evidence.clone(),
        terminal_reason: run.terminal_reason,
    }
}

pub(super) fn task_projection(task: &ExecutionTaskRecord) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: task.task_id,
        node_id: task.node_id.clone(),
        item_key: task.item_key.clone(),
        status: task.status,
        attempt: task.attempt,
        generation: task.generation,
        input: task.input.clone(),
        outcome: task.current_outcome.clone(),
    }
}

pub(super) fn applied_mutation(run: &ExecutionRunRecord) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Accepted {
        response: ExecutionMutationResponse::Applied {
            run: run_summary(run),
        },
        handoff: ExecutionMutationHandoff {
            wake_epoch: run.wake_epoch,
            task_ids_to_release: Vec::new(),
        },
    }
}

pub(super) fn replayed_mutation(run: &ExecutionRunRecord) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Accepted {
        response: ExecutionMutationResponse::Replayed {
            run: run_summary(run),
        },
        handoff: ExecutionMutationHandoff {
            wake_epoch: run.wake_epoch,
            task_ids_to_release: Vec::new(),
        },
    }
}

pub(super) fn conflict_mutation(reason: ExecutionConflictReason) -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Rejected {
        response: ExecutionMutationResponse::Conflict { reason },
    }
}

pub(super) fn not_found_mutation() -> ExecutionMutationAccepted {
    ExecutionMutationAccepted::Rejected {
        response: ExecutionMutationResponse::NotFound,
    }
}

pub(super) fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

pub(super) fn persisted_input_audience(
    current_generation: u64,
    current_outcome: Option<&ExecutionTaskOutcome>,
    outcome_audit: &[Value],
    expected_generation: u64,
) -> Option<moa_artifacts::execution_plan::InputAudience> {
    if current_generation == expected_generation
        && let Some(ExecutionTaskResult::NeedsInput { audience, .. }) =
            current_outcome.map(|outcome| &outcome.result)
    {
        return Some(audience.clone());
    }
    outcome_audit.iter().rev().find_map(|entry| {
        if entry.get("received_generation").and_then(Value::as_u64) != Some(expected_generation)
            || entry.get("accepted").and_then(Value::as_bool) != Some(true)
        {
            return None;
        }
        let outcome =
            serde_json::from_value::<ExecutionTaskOutcome>(entry.get("outcome")?.clone()).ok()?;
        match outcome.result {
            ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience),
            ExecutionTaskResult::Completed { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Cancelled { .. }
            | ExecutionTaskResult::UnknownOutcome { .. }
            | ExecutionTaskResult::Failed { .. } => None,
        }
    })
}

pub(super) fn validate_external_wait_payload(
    plan: &ExecutionPlanDefinition,
    node_id: &str,
    payload: &Value,
) -> Result<(), HandlerError> {
    let schema = plan
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .map(|node| &node.output_schema)
        .ok_or_else(|| invalid_execution_request("waiting task node is absent from active plan"))?;
    validate_instance(schema, payload, "execution.external_wait_output")
        .map_err(|error| invalid_execution_request(format!("invalid external payload: {error}")))
}

pub(super) fn execution_run_started_delivery(
    response: &ExecutionStartResponse,
) -> ExecutionRunStartedDelivery {
    let status = if response.confirmation_required {
        ExecutionRunAdmissionStatus::AwaitingConfirmation
    } else {
        ExecutionRunAdmissionStatus::Queued
    };
    let confirmation = response
        .confirmation_required
        .then(|| ExecutionConfirmationEvidence {
            active_plan_hash: response.active_plan_hash.to_string(),
            estimate: ExecutionAdmissionEstimate {
                cost_microusd: response.estimate.cost_microusd,
                tokens: response.estimate.tokens,
                tasks: response.estimate.tasks,
                tool_calls: response.estimate.tool_calls,
                retrieved_bytes: response.estimate.retrieved_bytes,
            },
            methodology: ExecutionEstimateMethodology::ConservativeWorstCase,
        });
    ExecutionRunStartedDelivery {
        started: ExecutionRunStarted {
            run_uid: response.run.run_uid,
            originating_user_sequence_num: response.run.originating_user_sequence_num,
            plan_revision: response.run.plan_revision,
            status,
            confirmation,
        },
        approved_budget: response.run.budget_ledger.limit.clone(),
    }
}

pub(super) fn send_run_wake(
    ctx: &Context<'_>,
    run_uid: uuid::Uuid,
    wake_epoch: u64,
    reason: ExecutionRunWakeReason,
) {
    crate::restate_identity::replay_safe_request(
        ctx.workflow_client::<ExecutionRunClient>(run_uid.to_string())
            .wake(Json::from(ExecutionRunWakeRequest {
                run_uid,
                wake_epoch,
                reason,
            })),
    )
    .send();
}

pub(super) fn invalid_execution_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

pub(super) fn execution_error(error: moa_execution::Error) -> HandlerError {
    match error {
        moa_execution::Error::Storage { message } => {
            TerminalError::new_with_code(503, format!("execution storage unavailable: {message}"))
                .into()
        }
        other => invalid_execution_request(other.to_string()),
    }
}

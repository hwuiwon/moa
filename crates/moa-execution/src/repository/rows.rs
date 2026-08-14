//! PostgreSQL row decoding for execution repository projections.

use super::audit::ExecutionPlanningContextRecord;
use super::*;

pub(super) fn planning_context_from_row(row: &PgRow) -> Result<ExecutionPlanningContextRecord> {
    let snapshot: Value = row.try_get("snapshot").map_err(row_error)?;
    Ok(ExecutionPlanningContextRecord {
        planning_context_uid: row.try_get("planning_context_uid").map_err(row_error)?,
        snapshot: serde_json::from_value(snapshot)?,
        planning_context_hash: row
            .try_get::<String, _>("planning_context_hash")
            .map_err(row_error)?
            .parse()?,
        created_at: row.try_get("created_at").map_err(row_error)?,
    })
}

pub(super) fn run_from_row(row: &PgRow) -> Result<ExecutionRunRecord> {
    let run_uid: Uuid = row.try_get("run_uid").map_err(row_error)?;
    let goal_value: Value = row.try_get("goal_contract").map_err(row_error)?;
    let initial_plan_value: Value = row.try_get("initial_plan").map_err(row_error)?;
    let active_plan_value: Value = row.try_get("active_plan").map_err(row_error)?;
    let plan_history: Value = row.try_get("plan_history").map_err(row_error)?;
    let catalog: Value = row.try_get("capability_catalog").map_err(row_error)?;
    let authorization: Value = row.try_get("authorization_envelope").map_err(row_error)?;
    let pinned_skills: Value = row
        .try_get("pinned_instruction_skills")
        .map_err(row_error)?;
    let completion_results: Value = row.try_get("completion_check_results").map_err(row_error)?;
    let terminal_gaps: Value = row.try_get("terminal_gaps").map_err(row_error)?;
    let terminal_cause: Option<Value> = row.try_get("terminal_cause").map_err(row_error)?;
    let terminal_satisfied_requirement_count =
        optional_u64(row, "terminal_satisfied_requirement_count")?;
    let terminal_requirement_count = optional_u64(row, "terminal_requirement_count")?;
    let terminal_evidence = match (
        terminal_cause,
        terminal_satisfied_requirement_count,
        terminal_requirement_count,
    ) {
        (None, None, None) => None,
        (Some(cause), Some(satisfied_requirement_count), Some(requirement_count)) => {
            Some(ExecutionTerminalEvidence {
                cause: serde_json::from_value(cause)?,
                satisfied_requirement_count,
                requirement_count,
            })
        }
        _ => {
            return Err(Error::InvalidRepositoryData {
                message: "execution terminal evidence columns are only partially populated"
                    .to_string(),
            });
        }
    };
    let waiting_reasons: Vec<WaitingReason> =
        serde_json::from_value(row.try_get("waiting_reasons").map_err(row_error)?)?;
    let waiting_task_count = required_u64(row, "waiting_task_count")?;
    let waiting_input_task_count = required_u64(row, "waiting_input_task_count")?;
    let waiting_review_task_count = required_u64(row, "waiting_review_task_count")?;
    let waiting_signal_task_count = required_u64(row, "waiting_signal_task_count")?;
    let waiting_timer_task_count = required_u64(row, "waiting_timer_task_count")?;
    let waiting_external_task_count = required_u64(row, "waiting_external_task_count")?;
    let waiting_replan_task_count = required_u64(row, "waiting_replan_task_count")?;
    let waiting_input_user_task_count = required_u64(row, "waiting_input_user_task_count")?;
    let waiting_input_tenant_admin_task_count =
        required_u64(row, "waiting_input_tenant_admin_task_count")?;
    let waiting_input_external_task_count = required_u64(row, "waiting_input_external_task_count")?;
    let waiting_reasons_truncated: bool = row
        .try_get("waiting_reasons_truncated")
        .map_err(row_error)?;
    let classified_waiting_count = waiting_input_task_count
        .checked_add(waiting_review_task_count)
        .and_then(|count| count.checked_add(waiting_signal_task_count))
        .and_then(|count| count.checked_add(waiting_timer_task_count))
        .and_then(|count| count.checked_add(waiting_external_task_count))
        .and_then(|count| count.checked_add(waiting_replan_task_count))
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "execution run waiting task count".to_string(),
        })?;
    let classified_input_count = waiting_input_user_task_count
        .checked_add(waiting_input_tenant_admin_task_count)
        .and_then(|count| count.checked_add(waiting_input_external_task_count))
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "execution run waiting input task count".to_string(),
        })?;
    let sample_count =
        u64::try_from(waiting_reasons.len()).map_err(|_| Error::ArithmeticOverflow {
            context: "execution run waiting reason sample".to_string(),
        })?;
    if classified_waiting_count != waiting_task_count
        || classified_input_count != waiting_input_task_count
        || sample_count > waiting_task_count
        || waiting_reasons.len() > 64
        || waiting_reasons_truncated != (sample_count < waiting_task_count)
    {
        return Err(Error::InvalidRepositoryData {
            message: "execution run waiting counters disagree with the bounded reason sample"
                .to_string(),
        });
    }
    let source_provenance: ExecutionSourceProvenance =
        serde_json::from_value(row.try_get("source_provenance").map_err(row_error)?)?;
    let source_kind = ExecutionSourceKind::from_str(
        &row.try_get::<String, _>("source_kind").map_err(row_error)?,
    )?;
    let status =
        ExecutionRunStatus::from_str(&row.try_get::<String, _>("status").map_err(row_error)?)?;
    let terminal_reason = row
        .try_get::<Option<String>, _>("terminal_reason")
        .map_err(row_error)?
        .map(|value| ExecutionTerminalReason::from_str(&value))
        .transpose()?;
    let pending_terminal_status = row
        .try_get::<Option<String>, _>("pending_terminal_status")
        .map_err(row_error)?
        .map(|value| ExecutionRunStatus::from_str(&value))
        .transpose()?;
    let pending_terminal_reason = row
        .try_get::<Option<String>, _>("pending_terminal_reason")
        .map_err(row_error)?
        .map(|value| ExecutionTerminalReason::from_str(&value))
        .transpose()?;
    let pending_terminal_evidence: Option<Value> =
        row.try_get("pending_terminal_cause").map_err(row_error)?;
    let pending_terminal_output: Option<Value> =
        row.try_get("pending_terminal_output").map_err(row_error)?;
    let cancellation_reason: Option<String> =
        row.try_get("cancellation_reason").map_err(row_error)?;
    let pending_terminal = match (
        pending_terminal_status,
        pending_terminal_reason,
        pending_terminal_evidence,
    ) {
        (None, None, None) if pending_terminal_output.is_none() => None,
        (Some(status), Some(reason), Some(evidence)) => {
            let payload: PendingTerminalEvidencePayload = serde_json::from_value(evidence)?;
            let terminal = PendingExecutionTerminal {
                status,
                reason,
                terminal_evidence: payload.terminal_evidence,
                completion_check_results: payload.completion_check_results,
                terminal_gaps: payload.terminal_gaps,
                output: pending_terminal_output,
                cancellation_reason: cancellation_reason.clone(),
            };
            terminal.validate()?;
            Some(terminal)
        }
        _ => {
            return Err(Error::InvalidRepositoryData {
                message: "pending execution terminal columns are only partially populated"
                    .to_string(),
            });
        }
    };
    if status.is_terminal() != terminal_reason.is_some() {
        return Err(Error::InvalidRepositoryData {
            message: "execution terminal reason nullability disagrees with run status".to_string(),
        });
    }
    let contact_id: Option<Uuid> = row.try_get("contact_id").map_err(row_error)?;
    let session_id: Uuid = row.try_get("session_id").map_err(row_error)?;
    let owner_user_id: String = row.try_get("owner_user_id").map_err(row_error)?;
    let tenant_id = TenantId(row.try_get("tenant_id").map_err(row_error)?);
    let admitted_identity: Identity =
        serde_json::from_value(row.try_get("admitted_identity").map_err(row_error)?)?;
    if admitted_identity.tenant_id != tenant_id {
        return Err(Error::InvalidRepositoryData {
            message: "execution admitted identity tenant disagrees with run tenant".to_string(),
        });
    }
    Ok(ExecutionRunRecord {
        run_uid,
        tenant_id,
        contact_id: contact_id.map(ContactId),
        session_id: SessionId(session_id),
        originating_user_sequence_num: required_u64(row, "originating_user_sequence_num")?,
        planning_context_uid: row.try_get("planning_context_uid").map_err(row_error)?,
        planning_context_hash: row
            .try_get::<String, _>("planning_context_hash")
            .map_err(row_error)?
            .parse()?,
        owner_user_id: UserId::new(owner_user_id),
        admitted_identity,
        goal: serde_json::from_value(goal_value)?,
        initial_plan: serde_json::from_value(initial_plan_value)?,
        active_plan: serde_json::from_value(active_plan_value)?,
        initial_plan_hash: row
            .try_get::<String, _>("initial_plan_hash")
            .map_err(row_error)?
            .parse()?,
        active_plan_hash: row
            .try_get::<String, _>("active_plan_hash")
            .map_err(row_error)?
            .parse()?,
        confirmed_plan_hash: row
            .try_get::<Option<String>, _>("confirmed_plan_hash")
            .map_err(row_error)?
            .map(|value| value.parse())
            .transpose()?,
        plan_revision: to_u64(
            row.try_get("plan_revision").map_err(row_error)?,
            "plan revision",
        )?,
        plan_history: serde_json::from_value(plan_history)?,
        catalog: serde_json::from_value(catalog)?,
        authorization: serde_json::from_value(authorization)?,
        pinned_instruction_skills: serde_json::from_value(pinned_skills)?,
        source_provenance,
        source_kind,
        skill_template_ref: row.try_get("skill_template_ref").map_err(row_error)?,
        skill_template_revision_uid: row
            .try_get("skill_template_revision_uid")
            .map_err(row_error)?,
        input: row.try_get("input").map_err(row_error)?,
        output: row.try_get("output").map_err(row_error)?,
        completion_check_results: serde_json::from_value(completion_results)?,
        terminal_gaps: serde_json::from_value(terminal_gaps)?,
        terminal_evidence,
        terminal_reason,
        status,
        controller_generation: required_u64(row, "controller_generation")?,
        activation_state: ExecutionActivationState::from_str(
            &row.try_get::<String, _>("activation_state")
                .map_err(row_error)?,
        )?,
        next_wake_at: row.try_get("next_wake_at").map_err(row_error)?,
        waiting_since: row.try_get("waiting_since").map_err(row_error)?,
        last_progress_at: row.try_get("last_progress_at").map_err(row_error)?,
        pause_requested_at: row.try_get("pause_requested_at").map_err(row_error)?,
        paused_at: row.try_get("paused_at").map_err(row_error)?,
        ready_task_count: required_u64(row, "ready_task_count")?,
        active_task_count: required_u64(row, "active_task_count")?,
        waiting_task_count,
        waiting_input_task_count,
        waiting_review_task_count,
        waiting_signal_task_count,
        waiting_timer_task_count,
        waiting_external_task_count,
        waiting_replan_task_count,
        waiting_input_user_task_count,
        waiting_input_tenant_admin_task_count,
        waiting_input_external_task_count,
        approved_budget: ExecutionBudgetLimit {
            max_cost_microusd: optional_u64(row, "budget_max_cost_microusd")?,
            max_tokens: optional_u64(row, "budget_max_tokens")?,
            max_tasks: optional_u64(row, "budget_max_tasks")?,
            max_tool_calls: optional_u64(row, "budget_max_tool_calls")?,
            max_retrieved_bytes: optional_u64(row, "budget_max_retrieved_bytes")?,
            deadline_at: row.try_get("budget_deadline_at").map_err(row_error)?,
        },
        budget_deadline_suspended_at: row
            .try_get("budget_deadline_suspended_at")
            .map_err(row_error)?,
        reserved: estimate_from_row(row, "reserved")?,
        consumed: estimate_from_row(row, "consumed")?,
        budget_overrun: row.try_get("budget_overrun").map_err(row_error)?,
        progress_total_tasks: required_u64(row, "progress_total_tasks")?,
        progress_completed_tasks: required_u64(row, "progress_completed_tasks")?,
        progress_failed_tasks: required_u64(row, "progress_failed_tasks")?,
        progress_cancelled_tasks: required_u64(row, "progress_cancelled_tasks")?,
        waiting_reasons,
        waiting_reasons_truncated,
        wake_epoch: required_u64(row, "wake_epoch")?,
        processed_wake_epoch: required_u64(row, "processed_wake_epoch")?,
        next_compensation_sequence: required_u64(row, "next_compensation_sequence")?,
        pending_terminal,
        manual_repair_required: row.try_get("manual_repair_required").map_err(row_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(row_error)?,
        cancellation_reason,
        created_at: row.try_get("created_at").map_err(row_error)?,
        queued_at: row.try_get("queued_at").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
        started_at: row.try_get("started_at").map_err(row_error)?,
        completed_at: row.try_get("completed_at").map_err(row_error)?,
        confirmed_at: row.try_get("confirmed_at").map_err(row_error)?,
    })
}

pub(super) fn task_from_row(row: &PgRow) -> Result<ExecutionTaskRecord> {
    let contact_id: Option<Uuid> = row.try_get("contact_id").map_err(row_error)?;
    let requirement_ids: Value = row.try_get("requirement_ids").map_err(row_error)?;
    let kind: Value = row.try_get("task_kind").map_err(row_error)?;
    let compensation_contract: Option<Value> =
        row.try_get("compensation_contract").map_err(row_error)?;
    let retry: Value = row.try_get("retry_policy").map_err(row_error)?;
    let resume_input_history: Value = row.try_get("resume_input_history").map_err(row_error)?;
    let current_outcome: Option<Value> = row.try_get("current_outcome").map_err(row_error)?;
    let citations: Value = row.try_get("citations").map_err(row_error)?;
    let generation_history: Value = row.try_get("generation_history").map_err(row_error)?;
    let outcome_audit: Value = row.try_get("outcome_audit").map_err(row_error)?;
    let actual = estimate_from_row(row, "actual")?;
    Ok(ExecutionTaskRecord {
        task_id: ExecutionTaskId::from_uuid(row.try_get("task_id").map_err(row_error)?),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        contact_id: contact_id.map(ContactId),
        node_id: row.try_get("node_id").map_err(row_error)?,
        item_key: row.try_get("item_key").map_err(row_error)?,
        requirement_ids: serde_json::from_value(requirement_ids)?,
        plan_revision: required_u64(row, "plan_revision")?,
        status: ExecutionTaskStatus::from_str(
            &row.try_get::<String, _>("status").map_err(row_error)?,
        )?,
        attempt: to_u32(row.try_get("attempt").map_err(row_error)?, "attempt")?,
        generation: required_u64(row, "generation")?,
        attempt_generation: required_u64(row, "attempt_generation")?,
        attempt_state: ExecutionAttemptState::from_str(
            &row.try_get::<String, _>("attempt_state")
                .map_err(row_error)?,
        )?,
        attempt_started_at: row.try_get("attempt_started_at").map_err(row_error)?,
        last_progress_at: row.try_get("last_progress_at").map_err(row_error)?,
        progress_step_bound_seconds: row
            .try_get::<Option<i32>, _>("progress_step_bound_seconds")
            .map_err(row_error)?
            .map(|seconds| to_positive_u32(seconds, "progress step bound seconds"))
            .transpose()?,
        attempt_deadline_at: row.try_get("attempt_deadline_at").map_err(row_error)?,
        waiting_since: row.try_get("waiting_since").map_err(row_error)?,
        ready_at: row.try_get("ready_at").map_err(row_error)?,
        external_job_uid: row.try_get("external_job_uid").map_err(row_error)?,
        active_dispatch_uid: row.try_get("active_dispatch_uid").map_err(row_error)?,
        dispatch_sequence: required_u64(row, "dispatch_sequence")?,
        input: row.try_get("input").map_err(row_error)?,
        resume_input_history: serde_json::from_value(resume_input_history)?,
        kind: serde_json::from_value(kind)?,
        compensation_contract: compensation_contract
            .map(serde_json::from_value)
            .transpose()?,
        retry: serde_json::from_value(retry)?,
        estimate: estimate_from_row(row, "estimate")?,
        reserved: estimate_from_row(row, "reserved")?,
        actual: ExecutionUsage {
            cost_microusd: actual.cost_microusd,
            tokens: actual.tokens,
            tool_calls: actual.tool_calls,
            retrieved_bytes: actual.retrieved_bytes,
        },
        actual_tasks: actual.tasks,
        current_outcome: current_outcome.map(serde_json::from_value).transpose()?,
        output: row.try_get("output").map_err(row_error)?,
        error: row.try_get("error").map_err(row_error)?,
        citations: serde_json::from_value(citations)?,
        generation_history: serde_json::from_value(generation_history)?,
        outcome_audit: serde_json::from_value(outcome_audit)?,
        created_at: row.try_get("created_at").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
        reserved_at: row.try_get("reserved_at").map_err(row_error)?,
        started_at: row.try_get("started_at").map_err(row_error)?,
        completed_at: row.try_get("completed_at").map_err(row_error)?,
    })
}

pub(super) fn compensation_from_row(row: &PgRow) -> Result<CompensationRegistrationProjection> {
    let compensator: Value = row.try_get("compensator").map_err(row_error)?;
    let outcome: Option<Value> = row.try_get("outcome").map_err(row_error)?;
    let outcome = outcome
        .map(serde_json::from_value::<CompensationPersistedOutcome>)
        .transpose()?;
    Ok(CompensationRegistrationProjection {
        compensation_id: CompensationId::from_uuid(
            row.try_get("compensation_id").map_err(row_error)?,
        ),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        forward_task_id: ExecutionTaskId::from_uuid(
            row.try_get("forward_task_id").map_err(row_error)?,
        ),
        registered_sequence: required_u64(row, "registered_sequence")?,
        forward_generation: required_u64(row, "forward_generation")?,
        compensator: serde_json::from_value(compensator)?,
        mapped_input: row.try_get("mapped_input").map_err(row_error)?,
        status: CompensationStatus::from_str(
            &row.try_get::<String, _>("status").map_err(row_error)?,
        )?,
        attempt: required_u64(row, "attempt")?,
        generation: required_u64(row, "generation")?,
        outcome: outcome.and_then(|persisted| persisted.result),
        error: row.try_get("error").map_err(row_error)?,
        created_at: row.try_get("created_at").map_err(row_error)?,
        updated_at: row.try_get("updated_at").map_err(row_error)?,
        started_at: row.try_get("started_at").map_err(row_error)?,
        completed_at: row.try_get("completed_at").map_err(row_error)?,
    })
}

pub(super) fn optional_u64(row: &PgRow, column: &str) -> Result<Option<u64>> {
    row.try_get::<Option<i64>, _>(column)
        .map_err(row_error)?
        .map(|value| to_u64(value, column))
        .transpose()
}

pub(super) fn required_u64(row: &PgRow, column: &str) -> Result<u64> {
    to_u64(row.try_get(column).map_err(row_error)?, column)
}

pub(super) fn estimate_from_row(row: &PgRow, prefix: &str) -> Result<ExecutionEstimate> {
    Ok(ExecutionEstimate {
        cost_microusd: required_u64(row, &format!("{prefix}_cost_microusd"))?,
        tokens: required_u64(row, &format!("{prefix}_tokens"))?,
        tasks: required_u64(row, &format!("{prefix}_tasks"))?,
        tool_calls: required_u64(row, &format!("{prefix}_tool_calls"))?,
        retrieved_bytes: required_u64(row, &format!("{prefix}_retrieved_bytes"))?,
    })
}

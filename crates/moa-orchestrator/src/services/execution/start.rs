//! Execution-run admission and replay validation.

use super::support::*;
use super::*;

pub(super) async fn start_inner(
    pool: sqlx::PgPool,
    config: ExecutionConfig,
    request: ExecutionStartRequest,
    originating_objective: String,
) -> Result<ExecutionStartResponse, HandlerError> {
    let scope = execution_scope(request.tenant_id, request.contact_id);
    let repository = ExecutionRepository::new(pool);
    let planning_context = repository
        .load_planning_context(scope, request.planning_context_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| {
            TerminalError::new_with_code(409, "execution planning context does not exist")
        })?;
    let expected_context_hash = request
        .planning_context_hash
        .parse::<ExecutionHash>()
        .map_err(execution_error)?;
    let snapshot = &planning_context.snapshot;
    if planning_context.planning_context_hash != expected_context_hash
        || snapshot.tenant_id != request.tenant_id
        || snapshot.contact_id != request.contact_id
        || snapshot.session_id != request.session_id
        || snapshot.originating_user_sequence_num != request.originating_user_sequence_num
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution planning context hash or origin scope mismatch",
        )
        .into());
    }
    if request.compiled.goal.objective.as_bytes() != originating_objective.as_bytes() {
        return Err(invalid_execution_request(
            "compiled execution objective must equal the persisted user message",
        ));
    }
    let validation = compile(CompileExecutionRequest {
        goal: request.compiled.goal.clone(),
        plan: request.compiled.plan.definition.clone(),
        run_input: request.run_input.clone(),
        catalog: snapshot.catalog.clone(),
        authorization: snapshot.authorization.clone(),
        approved_budget: snapshot.budget.clone(),
        config: config.clone(),
        now: Utc::now(),
    });
    if validation.compiled.as_ref() != Some(&request.compiled) {
        return Err(invalid_execution_request(
            "compiled execution does not match deterministic server validation",
        ));
    }
    if request.compiled.plan.plan_hash
        != plan_hash(&request.compiled.plan.definition).map_err(execution_error)?
        || request.compiled.plan.catalog_hash != snapshot.catalog.catalog_hash
    {
        return Err(invalid_execution_request(
            "compiled plan hashes do not match the supplied immutable snapshots",
        ));
    }
    estimate_fits_limit(request.compiled.plan.estimate, &snapshot.budget)
        .map_err(execution_error)?;
    validate_start_source_provenance(
        &request.source_provenance,
        &request.compiled.plan.plan_hash.to_string(),
        &snapshot.execution_templates,
    )
    .map_err(|error| invalid_execution_request(error.to_string()))?;
    let existing = if let Some(key) = request.idempotency_key.as_deref() {
        repository
            .load_run_by_idempotency_key(scope, request.tenant_id, request.contact_id, key)
            .await
            .map_err(execution_error)?
    } else {
        None
    };
    if let Some(run) = existing {
        verify_run_scope(
            &run,
            request.tenant_id,
            request.contact_id,
            request.session_id,
        )?;
        verify_start_replay(&run, &request, snapshot)?;
        let confirmation_required = run.status == ExecutionRunStatus::AwaitingConfirmation;
        return Ok(ExecutionStartResponse {
            active_plan_hash: run.active_plan_hash,
            estimate: run.active_plan.estimate,
            run: run_summary(&run),
            created: false,
            confirmation_required,
        });
    }
    let confirmation_required =
        request.compiled.plan.estimate.cost_microusd > config.unattended_max_cost_microusd;
    let status = if confirmation_required {
        ExecutionRunStatus::AwaitingConfirmation
    } else {
        ExecutionRunStatus::Queued
    };
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: request.tenant_id,
                contact_id: request.contact_id,
                session_id: request.session_id,
                originating_user_sequence_num: request.originating_user_sequence_num,
                planning_context_uid: request.planning_context_uid,
                planning_context_hash: expected_context_hash,
                owner_user_id: snapshot.owner_user_id.clone(),
                goal: request.compiled.goal,
                plan: request.compiled.plan,
                catalog: snapshot.catalog.clone(),
                authorization: snapshot.authorization.clone(),
                pinned_instruction_skills: snapshot.pinned_instruction_skills.clone(),
                source_provenance: request.source_provenance,
                input: request.run_input,
                status,
                approved_budget: snapshot.budget.clone(),
                idempotency_key: request.idempotency_key,
            },
        )
        .await
        .map_err(execution_error)?;
    Ok(ExecutionStartResponse {
        active_plan_hash: run.active_plan_hash,
        estimate: run.active_plan.estimate,
        run: run_summary(&run),
        created: true,
        confirmation_required,
    })
}

pub(super) fn validate_start_source_provenance(
    provenance: &ExecutionSourceProvenance,
    committed_plan_hash: &str,
    execution_templates: &[PinnedExecutionTemplate],
) -> Result<(), ExecutionPlanningContractError> {
    provenance.validate(committed_plan_hash)?;
    let (skill_template_ref, skill_template_revision_uid) = match provenance {
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        }
        | ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (skill_template_ref, skill_template_revision_uid),
        ExecutionSourceProvenance::GeneratedPlan { .. } => return Ok(()),
    };
    let parsed = skill_template_ref.parse::<ArtifactRef>().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    let canonical = parsed.canonical_string().map_err(|error| {
        ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message: error.to_string(),
        }
    })?;
    if canonical != *skill_template_ref
        || !execution_templates
            .iter()
            .any(|template| template.skill_ref == parsed)
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message:
                "must equal one canonical template reference in the persisted planning context"
                    .to_string(),
        });
    }
    if execution_templates
        .iter()
        .filter(|template| {
            template.skill_ref == parsed && template.revision_uid == *skill_template_revision_uid
        })
        .count()
        != 1
    {
        return Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_revision_uid".to_string(),
            message: "must equal one exact template revision in the persisted planning context"
                .to_string(),
        });
    }
    Ok(())
}

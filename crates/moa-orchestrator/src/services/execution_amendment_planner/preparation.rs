//! Bounded planner inputs frozen from one persisted `WaitingReplan` revision.
//!
//! Every value the amendment planner may observe is derived here, inside one
//! journaled database step, so the model call itself owns no authority: the
//! capability catalog, the authorization envelope, the remaining budget, and the
//! failure evidence all come from the locked run rather than from a caller.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{ExecutionBudgetLimit, ExecutionTaskResult};
use moa_brain::execution_planning::AmendmentPlanningEvidence;
use moa_config::ExecutionConfig;
use moa_core::{
    traits::Identity,
    types::{
        contact::ContactId,
        execution_planning::{
            EXECUTION_REPORT_MAX_BYTES, ExecutionPlanningAuditEnvelope,
            ExecutionPlanningAuditPayload,
        },
        identifiers::{SessionId, TenantId},
    },
};
use moa_execution::{
    replan::{ReplanExhaustion, replan_exhaustion_reason},
    repository::{
        ExecutionRepository, ExecutionScope,
        amendment::{AmendmentProjectionOutcome, AmendmentProjectionRequest},
        audit::{CompileAuditWriteOutcome, PlannerCallAuditWriteOutcome},
    },
    state::ExecutionTaskId,
    wire::ExecutionPlanningContextSnapshot,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Exact persisted run revision one bounded planning slice may act on.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningTarget {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Parent session that owns the amendment mutation boundary.
    pub session_id: SessionId,
    /// Run parked in `WaitingReplan`.
    pub run_uid: Uuid,
    /// Active plan revision the amendment must fence.
    pub base_plan_revision: u64,
}

impl AmendmentPlanningTarget {
    /// Returns the durable repository scope that owns this run.
    #[must_use]
    pub fn scope(&self) -> ExecutionScope {
        self.contact_id.map_or(
            ExecutionScope::Tenant {
                tenant_id: self.tenant_id,
            },
            |contact_id| ExecutionScope::Contact {
                tenant_id: self.tenant_id,
                contact_id,
            },
        )
    }
}

/// Exact `WaitingReplan` origin every replan-stop intent must fence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningOrigin {
    /// Owning run.
    pub run_uid: Uuid,
    /// Parent session that owns the mutation boundary.
    pub session_id: SessionId,
    /// Plan revision that stopped.
    pub base_plan_revision: u64,
    /// Originating `WaitingReplan` task.
    pub task_id: ExecutionTaskId,
    /// Exact logical task generation.
    pub task_generation: u64,
}

/// Frozen planner input for one bounded amendment call.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedAmendmentPlanning {
    /// Persisted planning authority narrowed to currently available capabilities.
    pub context: ExecutionPlanningContextSnapshot,
    /// Immutable goal, plan, projection, and bounded failure evidence.
    pub evidence: AmendmentPlanningEvidence,
    /// Resources still available for replacement work.
    pub remaining_budget: ExecutionBudgetLimit,
    /// Exact replan origin used by a later stop intent.
    pub origin: AmendmentPlanningOrigin,
    /// Principal admitted when the run was created.
    pub admitted_identity: Identity,
    /// Journaled planner time.
    pub now: DateTime<Utc>,
}

/// Closed disposition of one bounded planning-input load.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "inputs", rename_all = "snake_case")]
pub enum AmendmentPlanningInputs {
    /// The revision is ready for exactly one bounded planner call.
    Ready(Box<PreparedAmendmentPlanning>),
    /// Another replan attempt is already knowably exhausted.
    Exhausted {
        /// Exact replan origin the stop intent fences.
        origin: AmendmentPlanningOrigin,
        /// Immediately knowable typed exhaustion.
        exhaustion: ReplanExhaustion,
    },
    /// The revision is no longer the live `WaitingReplan` revision.
    Skip,
}

/// Freezes every planner input for one exact `WaitingReplan` revision.
///
/// Returns [`AmendmentPlanningInputs::Skip`] whenever the run moved on, already
/// carries a replan-stop intent, or is being terminalized: a planning slice is
/// always allowed to arrive late and must never resurrect a settled revision.
pub async fn prepare_amendment_planning(
    repository: &ExecutionRepository,
    config: &ExecutionConfig,
    target: AmendmentPlanningTarget,
    now: DateTime<Utc>,
) -> Result<AmendmentPlanningInputs, HandlerError> {
    let scope = target.scope();
    let snapshot = match repository
        .load_amendment_projection_for_session(
            scope,
            config,
            AmendmentProjectionRequest {
                run_uid: target.run_uid,
                session_id: target.session_id,
                expected_plan_revision: target.base_plan_revision,
            },
        )
        .await
        .map_err(execution_error)?
    {
        AmendmentProjectionOutcome::Ready(snapshot) => *snapshot,
        AmendmentProjectionOutcome::NotFound | AmendmentProjectionOutcome::Conflict => {
            return Ok(AmendmentPlanningInputs::Skip);
        }
    };
    if snapshot.run.tenant_id != target.tenant_id || snapshot.run.contact_id != target.contact_id {
        return Err(TerminalError::new_with_code(409, "execution scope mismatch").into());
    }
    if snapshot.run.pending_terminal.is_some() || snapshot.run.manual_repair_required {
        return Ok(AmendmentPlanningInputs::Skip);
    }
    // A recorded stop intent owns the exact fresh wake that terminalizes this run. Proposing an
    // amendment against the same revision would apply work the controller is about to discard.
    if repository
        .load_replan_stop_intent(
            scope,
            snapshot.run.run_uid,
            snapshot.run.controller_generation,
            snapshot.run.wake_epoch,
        )
        .await
        .map_err(execution_error)?
        .is_some()
    {
        return Ok(AmendmentPlanningInputs::Skip);
    }
    let [waiting_task] = snapshot.projection.replan_tasks.as_slice() else {
        return Ok(AmendmentPlanningInputs::Skip);
    };
    let origin = AmendmentPlanningOrigin {
        run_uid: snapshot.run.run_uid,
        session_id: snapshot.run.session_id,
        base_plan_revision: snapshot.run.plan_revision,
        task_id: waiting_task.task_id,
        task_generation: waiting_task.generation,
    };
    // The cheapest possible verdict: an elapsed deadline or a fully consumed dimension makes
    // another paid planner call pointless, so exhaustion is decided before any provider work.
    if let Some(exhaustion) = replan_exhaustion_reason(&snapshot.budget_ledger, now) {
        return Ok(AmendmentPlanningInputs::Exhausted { origin, exhaustion });
    }
    let Some(outcome) = waiting_task.outcome.as_ref() else {
        return Err(TerminalError::new("WaitingReplan task has no persisted outcome").into());
    };
    let ExecutionTaskResult::NeedsReplan { reason, evidence } = &outcome.result else {
        return Err(TerminalError::new("WaitingReplan task has no NeedsReplan evidence").into());
    };
    let failure_evidence = bounded_failure_evidence(reason, evidence)?;
    let planning_context = repository
        .load_planning_context(scope, snapshot.run.planning_context_uid)
        .await
        .map_err(execution_error)?
        .ok_or_else(|| TerminalError::new("execution planning context does not exist"))?;
    if planning_context.planning_context_hash != snapshot.run.planning_context_hash
        || planning_context.snapshot.tenant_id != snapshot.run.tenant_id
        || planning_context.snapshot.contact_id != snapshot.run.contact_id
        || planning_context.snapshot.session_id != snapshot.run.session_id
        || planning_context.snapshot.originating_user_sequence_num
            != snapshot.run.originating_user_sequence_num
        || planning_context.snapshot.owner_user_id != snapshot.run.owner_user_id
        || planning_context.snapshot.catalog != snapshot.run.catalog
        || planning_context.snapshot.authorization != snapshot.run.authorization
        || planning_context.snapshot.pinned_instruction_skills
            != snapshot.run.pinned_instruction_skills
    {
        return Err(TerminalError::new_with_code(
            409,
            "persisted amendment planning authority does not match the active run",
        )
        .into());
    }
    let mut effective_context = planning_context.snapshot;
    // Confirmation may replace the budget frozen at planning time, so the planner is shown the
    // approved run budget rather than the immutable admission budget.
    effective_context.budget = snapshot.run.approved_budget.clone();
    // Amendment planning is governed by the exact capability catalog persisted with
    // the run. Consulting the deployment router here would make the same wake
    // compile differently after a catalog refresh and would drop installed
    // connector provenance before dispatch can generation-fence it.
    let admitted_tool_names = effective_context
        .catalog
        .capabilities
        .iter()
        .filter_map(|capability| capability.source.model_visible_tool_name())
        .map(ToString::to_string)
        .collect();
    let context = narrow_amendment_context(effective_context, &admitted_tool_names)
        .map_err(execution_error)?;
    let remaining_budget = snapshot
        .budget_ledger
        .remaining_limit()
        .map_err(execution_error)?;
    Ok(AmendmentPlanningInputs::Ready(Box::new(
        PreparedAmendmentPlanning {
            context,
            evidence: AmendmentPlanningEvidence {
                goal: snapshot.run.goal,
                active_plan: snapshot.run.active_plan,
                projection: snapshot.projection,
                failure_evidence,
                waiting_task: origin.task_id,
            },
            remaining_budget,
            origin,
            admitted_identity: snapshot.run.admitted_identity,
            now,
        },
    )))
}

/// Wraps `NeedsReplan` evidence for planner use, rejecting an over-cap payload first.
///
/// The planner envelope is bounded, so an oversized projection must fail here rather
/// than after a paid provider call has already been issued with a truncated prompt.
pub fn bounded_failure_evidence(reason: &str, evidence: &Value) -> Result<Value, HandlerError> {
    let failure_evidence = json!({"reason": reason, "evidence": evidence});
    let encoded = moa_core::canonical_json::canonical_json_bytes(&failure_evidence)
        .map_err(|error| TerminalError::new(error.to_string()))?;
    if encoded.len() > EXECUTION_REPORT_MAX_BYTES {
        return Err(TerminalError::new_with_code(
            422,
            "WaitingReplan failure evidence exceeds the bounded planner envelope",
        )
        .into());
    }
    Ok(failure_evidence)
}

/// Retains only persisted capabilities whose governed tool is currently available.
pub fn narrow_amendment_context(
    mut context: ExecutionPlanningContextSnapshot,
    available_tool_names: &BTreeSet<String>,
) -> moa_execution::Result<ExecutionPlanningContextSnapshot> {
    use moa_execution::capability::CapabilitySource;

    let retained_refs = context
        .catalog
        .capabilities
        .iter()
        .filter(|capability| match &capability.source {
            CapabilitySource::BuiltInTool { name } | CapabilitySource::HandTool { name } => {
                available_tool_names.contains(name)
            }
            // `tool_name`, not `remote_name`: availability is membership in the
            // router's registered names, and a connector tool is registered
            // under its server-qualified reference.
            CapabilitySource::McpTool { tool_name, .. }
            | CapabilitySource::ActionArtifact { tool_name, .. }
            | CapabilitySource::ConnectorAction { tool_name, .. }
            | CapabilitySource::InstalledConnectorAction { tool_name, .. }
            | CapabilitySource::SkillAction { tool_name, .. }
            | CapabilitySource::Memory { tool_name, .. } => {
                available_tool_names.contains(tool_name)
            }
            CapabilitySource::SkillCode { .. }
            | CapabilitySource::Knowledge { .. }
            | CapabilitySource::Model => true,
        })
        .map(|capability| capability.reference.clone())
        .collect::<Vec<_>>();
    narrow_authorized_capability_refs(&mut context.authorization.capability_refs, &retained_refs);
    context
        .validate()
        .map_err(|error| moa_execution::Error::InvalidRepositoryInput {
            message: error.to_string(),
        })?;
    Ok(context)
}

/// Intersects persisted authorization with a live availability set.
///
/// This is deliberately a retain and never an extend: an availability observation
/// may only remove authority the planning context already froze.
pub fn narrow_authorized_capability_refs(
    authorized: &mut Vec<moa_artifacts::execution_plan::CapabilityReference>,
    live: &[moa_artifacts::execution_plan::CapabilityReference],
) {
    authorized.retain(|reference| live.contains(reference));
}

/// Persists one planner or compiler audit produced by amendment planning.
pub async fn persist_amendment_audit(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    envelope: &ExecutionPlanningAuditEnvelope,
) -> Result<(), HandlerError> {
    match &envelope.payload {
        ExecutionPlanningAuditPayload::PlannerCall { .. } => {
            let result = repository
                .write_planner_call_audit(scope, envelope)
                .await
                .map_err(execution_error)?;
            if matches!(result, PlannerCallAuditWriteOutcome::Conflict { .. }) {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution amendment planner audit conflicts with first persisted evidence",
                )
                .into());
            }
        }
        ExecutionPlanningAuditPayload::Compile { .. } => {
            let result = repository
                .write_compile_audit(scope, envelope)
                .await
                .map_err(execution_error)?;
            if matches!(result, CompileAuditWriteOutcome::Conflict { .. }) {
                return Err(TerminalError::new_with_code(
                    409,
                    "execution amendment compile audit conflicts with first persisted evidence",
                )
                .into());
            }
        }
        ExecutionPlanningAuditPayload::Route { .. } => {
            return Err(TerminalError::new_with_code(
                422,
                "execution amendment planning produced a route audit",
            )
            .into());
        }
    }
    Ok(())
}

fn execution_error(error: moa_execution::Error) -> HandlerError {
    crate::workflows::errors::execution_error_to_handler_error(error)
}

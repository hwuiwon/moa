//! Governed execution of one bounded compensation effect.

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::ExecutionUsage;
use moa_core::{
    traits::SessionStore as _,
    types::{
        action_policy::{ActionClass, CapabilityProvenance},
        completion::{ToolCallContent, ToolInvocation},
        identifiers::ToolCallId,
        resource::ResourceBudget,
        tools::IdempotencyClass,
    },
};
use moa_execution::{
    capability::{CapabilitySource, ExecutionCapability},
    repository::{
        ExecutionScope,
        compensation::{CompensationAttemptRecord, CompensationAttemptWriteOutcome},
    },
    schema::validate_instance,
    state::{ExecutionCompensationOutcome, LogicalTaskKind},
    wire::{ExecutionCompensationAttemptRequest, ExecutionToolDispatchRejection},
};
use restate_sdk::prelude::*;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, GovernedReviewPending, invoke_governed_tool,
    },
    workflows::{
        errors::{execution_error_to_handler_error, moa_error_to_handler_error},
        execution_compensation_attempt::ExecutionCompensationAttemptImpl,
    },
};

/// Complete set of boundaries at which a compensation workflow must return.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ActiveCompensationAttemptExit {
    /// The exact compensator produced a completed, failed, or ambiguous outcome.
    Outcome(ExecutionCompensationOutcome),
    /// Action policy persisted a review; resolution proceeds through storage.
    ReviewPending(GovernedReviewPending),
    /// The provider accepted durable asynchronous work owned by this attempt.
    ExternalJob(Uuid),
}

/// Executes the compiler-pinned compensator at most once.
pub(super) async fn execute_compensation_attempt(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionCompensationAttemptRequest,
    started: &CompensationAttemptRecord,
) -> Result<ActiveCompensationAttemptExit, HandlerError> {
    let scope = execution_scope(started);
    let repository = workflow.repository.clone();
    let run_uid = started.run.run_uid;
    let forward_task_id = started.registration.forward_task_id;
    let forward_task = ctx
        .run(|| async move {
            repository
                .load_task(scope, run_uid, forward_task_id)
                .await
                .map(Json::from)
                .map_err(execution_error_to_handler_error)
        })
        .name("load_compensation_forward_task")
        .await?
        .into_inner()
        .ok_or_else(|| TerminalError::new("compensation forward task was not found"))?;
    let capability = match validate_runtime_contract(started, &forward_task) {
        Ok(capability) => capability,
        Err(message) => {
            return Ok(ActiveCompensationAttemptExit::Outcome(failed(
                message,
                false,
                cumulative_usage(started),
            )));
        }
    };
    if let Err(error) = validate_instance(
        &capability.input_schema,
        &started.registration.mapped_input,
        "execution_compensation.input",
    ) {
        return Ok(ActiveCompensationAttemptExit::Outcome(failed(
            format!("compensator mapped input failed pinned schema: {error}"),
            false,
            cumulative_usage(started),
        )));
    }
    let session = load_session(workflow, ctx, started).await?;
    if session.tenant_id != started.run.tenant_id {
        return Err(
            TerminalError::new("authoritative compensation session tenant mismatch").into(),
        );
    }
    let tool_name = match capability.source.model_visible_tool_name() {
        Some(name) => name.to_string(),
        None => {
            return Ok(ActiveCompensationAttemptExit::Outcome(failed(
                "compensator has no governed tool owner".to_string(),
                false,
                cumulative_usage(started),
            )));
        }
    };
    let tool_id =
        stable_compensator_tool_id(request.compensation_id, request.compensation_generation);
    let tool_call = ToolCallContent {
        invocation: ToolInvocation {
            id: Some(tool_id.to_string()),
            name: tool_name.clone(),
            input: started.registration.mapped_input.clone(),
        },
        provider_metadata: None,
    };
    let allowed_tools = BTreeSet::from([tool_name]);
    let provenance = CapabilityProvenance {
        kind: Some(capability_source_kind(&capability.source).to_string()),
        id: Some(format!(
            "{}@{}",
            capability.reference.name, capability.reference.version
        )),
        step_id: Some(format!(
            "compensation:{}",
            started.registration.forward_task_id
        )),
    };
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: &session,
            identity: &started.run.admitted_identity,
            session_id: started.run.session_id,
            tool_id,
            tool_call: &tool_call,
            allowed_tools: &allowed_tools,
            expected_tool_contract_revision: Some(&capability.contract_revision),
            active_canary: None,
            trusted_sandbox_manifest: None,
            origin: GovernedInvocationOrigin::ExecutionCompensation {
                run_uid: started.run.run_uid,
                compensation_id: started.registration.compensation_id.as_uuid(),
                generation: started.registration.generation,
                attempt_generation: started.attempt_generation,
            },
            capability_provenance: Some(&provenance),
            capability_policy_context: Some(&capability.policy_context),
            resource_budget: ResourceBudget::until(request.attempt_deadline_at),
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    Ok(classify_outcome(started, capability, outcome))
}

fn classify_outcome(
    started: &CompensationAttemptRecord,
    capability: &ExecutionCapability,
    outcome: GovernedInvocationOutcome,
) -> ActiveCompensationAttemptExit {
    let mut usage = cumulative_usage(started);
    match outcome {
        GovernedInvocationOutcome::Completed(result)
            if result.disposition == GovernedInvocationDisposition::ReviewPending =>
        {
            match result.review {
                Some(review) => ActiveCompensationAttemptExit::ReviewPending(review),
                None => ActiveCompensationAttemptExit::Outcome(
                    ExecutionCompensationOutcome::UnknownOutcome {
                        message: "governed review admission omitted its persisted review reference"
                            .to_string(),
                        usage,
                    },
                ),
            }
        }
        GovernedInvocationOutcome::Completed(result) => {
            usage.tool_calls = usage.tool_calls.saturating_add(1);
            usage.retrieved_bytes = usage.retrieved_bytes.saturating_add(serialized_len(
                &result.output.safe_output.structured_payload(),
            ));
            if result.output.is_error() {
                return ActiveCompensationAttemptExit::Outcome(failed(
                    result.output.safe_output.to_text(),
                    true,
                    usage,
                ));
            }
            let output = result
                .output
                .safe_output
                .structured_payload()
                .cloned()
                .unwrap_or_else(|| Value::String(result.output.safe_output.to_text()));
            if let Err(error) = validate_instance(
                &capability.output_schema,
                &output,
                "execution_compensation.output",
            ) {
                ActiveCompensationAttemptExit::Outcome(
                    ExecutionCompensationOutcome::UnknownOutcome {
                        message: format!(
                            "compensator returned invalid output after possible commit: {error}"
                        ),
                        usage,
                    },
                )
            } else {
                ActiveCompensationAttemptExit::Outcome(ExecutionCompensationOutcome::Completed {
                    output,
                    usage,
                })
            }
        }
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => {
            ActiveCompensationAttemptExit::Outcome(ExecutionCompensationOutcome::UnknownOutcome {
                message,
                usage,
            })
        }
        GovernedInvocationOutcome::ExternalJob {
            external_job_uid, ..
        } => ActiveCompensationAttemptExit::ExternalJob(external_job_uid),
        GovernedInvocationOutcome::NotDispatched { reason, .. } => {
            ActiveCompensationAttemptExit::Outcome(failed(
                execution_dispatch_rejection_message(reason),
                false,
                usage,
            ))
        }
        GovernedInvocationOutcome::Delegation { .. } => {
            ActiveCompensationAttemptExit::Outcome(failed(
                "compensators cannot invoke delegation capabilities".to_string(),
                false,
                usage,
            ))
        }
    }
}

fn validate_runtime_contract<'a>(
    started: &'a CompensationAttemptRecord,
    forward_task: &moa_execution::repository::ExecutionTaskRecord,
) -> Result<&'a ExecutionCapability, String> {
    let LogicalTaskKind::Capability {
        reference: forward_reference,
    } = &forward_task.kind
    else {
        return Err("registered compensation forward task is not a direct capability".to_string());
    };
    if forward_task.compensation_contract.as_ref() != Some(&started.registration.compensator) {
        return Err("registered compensation drifted from the forward task contract".to_string());
    }
    let forward = find_catalog_capability(&started.run, forward_reference)?;
    if !forward
        .rollback
        .as_ref()
        .is_some_and(|rollback| rollback.matches(&started.registration.compensator))
    {
        return Err("pinned forward capability no longer promises the exact rollback".to_string());
    }
    let compensator =
        find_catalog_capability(&started.run, &started.registration.compensator.compensator)?;
    if compensator.action_class == ActionClass::Read {
        return Err("compensator catalog entry is read-only".to_string());
    }
    if compensator.idempotency_class != IdempotencyClass::Idempotent {
        return Err("compensator catalog entry is not idempotent".to_string());
    }
    Ok(compensator)
}

fn find_catalog_capability<'a>(
    run: &'a moa_execution::repository::ExecutionRunRecord,
    reference: &moa_artifacts::execution_plan::CapabilityReference,
) -> Result<&'a ExecutionCapability, String> {
    if !run.authorization.capability_refs.contains(reference) {
        return Err("compensator is outside the persisted authorization envelope".to_string());
    }
    run.catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == *reference)
        .ok_or_else(|| "compensator is absent from the persisted catalog".to_string())
}

async fn load_session(
    workflow: &ExecutionCompensationAttemptImpl,
    ctx: &WorkflowContext<'_>,
    started: &CompensationAttemptRecord,
) -> Result<moa_core::types::session::SessionMeta, HandlerError> {
    let store = workflow.session_store.clone();
    let session_id = started.run.session_id;
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("compensation_attempt_load_session")
        .await?
        .into_inner())
}

pub(super) fn execution_scope(started: &CompensationAttemptRecord) -> ExecutionScope {
    started.run.contact_id.map_or(
        ExecutionScope::Tenant {
            tenant_id: started.run.tenant_id,
        },
        |contact_id| ExecutionScope::Contact {
            tenant_id: started.run.tenant_id,
            contact_id,
        },
    )
}

pub(super) fn cumulative_usage(started: &CompensationAttemptRecord) -> ExecutionUsage {
    started
        .registration
        .outcome
        .as_ref()
        .map(ExecutionCompensationOutcome::usage)
        .cloned()
        .unwrap_or(ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        })
}

pub(super) fn write_applied(
    outcome: CompensationAttemptWriteOutcome,
) -> Result<(), moa_execution::Error> {
    match outcome {
        CompensationAttemptWriteOutcome::Applied(_)
        | CompensationAttemptWriteOutcome::Replayed(_)
        | CompensationAttemptWriteOutcome::NotFound => Ok(()),
        CompensationAttemptWriteOutcome::Conflict => {
            Err(moa_execution::Error::InvalidRepositoryData {
                message: "compensation attempt transition lost its exact generation fence"
                    .to_string(),
            })
        }
    }
}

fn failed(message: String, retryable: bool, usage: ExecutionUsage) -> ExecutionCompensationOutcome {
    ExecutionCompensationOutcome::Failed {
        message,
        retryable,
        usage,
    }
}

fn stable_compensator_tool_id(
    compensation_id: moa_execution::state::CompensationId,
    logical_generation: u64,
) -> ToolCallId {
    ToolCallId(Uuid::new_v5(
        &compensation_id.as_uuid(),
        format!("generation:{logical_generation}").as_bytes(),
    ))
}

const fn capability_source_kind(source: &CapabilitySource) -> &'static str {
    match source {
        CapabilitySource::BuiltInTool { .. } => "built_in_tool",
        CapabilitySource::HandTool { .. } => "hand_tool",
        CapabilitySource::McpTool { .. } => "mcp_tool",
        CapabilitySource::ActionArtifact { .. } => "action_artifact",
        CapabilitySource::ConnectorAction { .. } => "connector_action",
        CapabilitySource::InstalledConnectorAction { .. } => "installed_connector_action",
        CapabilitySource::SkillAction { .. } => "skill_action",
        CapabilitySource::SkillCode { .. } => "skill_code",
        CapabilitySource::Memory { .. } => "memory",
        CapabilitySource::Knowledge { .. } => "knowledge",
        CapabilitySource::Model => "model",
    }
}

fn execution_dispatch_rejection_message(reason: ExecutionToolDispatchRejection) -> String {
    let label = match reason {
        ExecutionToolDispatchRejection::OriginNotFound => "origin_not_found",
        ExecutionToolDispatchRejection::StaleGeneration => "stale_generation",
        ExecutionToolDispatchRejection::OperationNotRunning => "operation_not_running",
        ExecutionToolDispatchRejection::RunNotDispatchable => "run_not_dispatchable",
    };
    format!("execution effect was not dispatched: {label}")
}

fn serialized_len<T: Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: retries of one logical effect reuse the tool identity, while a new
    // logical generation cannot alias the prior provider idempotency key.
    #[test]
    fn compensator_tool_identity_is_fenced_by_logical_generation_offline() {
        let compensation_id = moa_execution::state::CompensationId::from_uuid(Uuid::from_u128(7));
        assert_eq!(
            stable_compensator_tool_id(compensation_id, 7),
            stable_compensator_tool_id(compensation_id, 7)
        );
        assert_ne!(
            stable_compensator_tool_id(compensation_id, 7),
            stable_compensator_tool_id(compensation_id, 8)
        );
    }
}

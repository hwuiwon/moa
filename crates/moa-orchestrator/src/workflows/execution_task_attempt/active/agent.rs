//! Bounded task-local agent execution and continuation transitions.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionFailureClass, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage,
};
use moa_core::types::{
    action_policy::{ActionRuleScope, CapabilityProvenance},
    completion::{CompletionContent, ToolCallContent, ToolInvocation},
    context::ContextMessage,
    identifiers::ToolCallId,
    resource::ResourceBudget,
    security::{
        SecurityCircuitOwner, SecurityCircuitStage, SecurityCircuitState, ToolCapabilityId,
    },
    tools::{AsyncToolJobTerminalOutcome, ToolAsyncMode},
};
use moa_execution::{
    capability::ExecutionCapability,
    repository::task::{TaskAttemptCheckpointKind, TaskAttemptRecord},
    state::failed_task_outcome,
    wire::ExecutionTaskAttemptRequest,
};
use restate_sdk::prelude::*;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    services::llm_gateway::{
        BoundedCompletionRequest, LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient,
        attach_completion_owner, completion_idempotency_key,
    },
    tool_invocation::governed::{
        GovernedInvocationDisposition, GovernedInvocationOrigin, GovernedInvocationOutcome,
        GovernedInvocationRequest, invoke_governed_tool,
    },
    workflows::{
        errors::moa_error_to_handler_error,
        execution_task_attempt::{
            ExecutionTaskAttemptImpl, capability_tool_name,
            continuation::{
                PendingExternalToolInvocation, PendingReviewedToolInvocation,
                TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION, TaskAttemptContinuation,
                TaskAttemptContinuationState,
            },
        },
    },
};

use super::{
    ActiveTaskAttemptExit, capability_source_kind, execution_dispatch_rejection_message,
    find_capability,
    heartbeat::{AttemptHeartbeat, begin_capability_dispatch, record_attempt_heartbeat},
    load_session, persist_external_start_checkpoint, serialized_len,
};

/// Immutable task-local agent definition selected by the logical task.
pub(super) struct AgentTaskSpec<'a> {
    /// Task-local instructions.
    pub(super) instructions: &'a str,
    /// Pinned instruction-only skills.
    pub(super) skill_refs: &'a [moa_artifacts::reference::ArtifactRef],
    /// Governed capabilities available to the task-local agent.
    pub(super) capability_refs: &'a [CapabilityReference],
    /// Maximum model turns admitted for this task.
    pub(super) max_turns: u32,
}

struct AgentPending {
    review: Option<PendingReviewedToolInvocation>,
    tool_calls: Vec<ToolInvocation>,
    external: Option<PendingExternalToolInvocation>,
}

/// Executes one bounded task-local agent model or tool boundary.
pub(super) async fn execute_agent_turn(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    request: &ExecutionTaskAttemptRequest,
    started: &TaskAttemptRecord,
    spec: AgentTaskSpec<'_>,
    continuation: Option<&TaskAttemptContinuation>,
) -> Result<ActiveTaskAttemptExit, HandlerError> {
    let AgentTaskSpec {
        instructions,
        skill_refs,
        capability_refs,
        max_turns,
    } = spec;
    if max_turns == 0 {
        return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
            ExecutionFailureClass::InvalidInput,
            "agent max_turns must be positive".to_string(),
            started.task.actual.clone(),
        )));
    }
    let mut capabilities = BTreeMap::<String, &ExecutionCapability>::new();
    for reference in capability_refs {
        let capability = find_capability(&started.run, reference)?;
        let tool_name = capability_tool_name(capability)?;
        if capabilities.insert(tool_name.clone(), capability).is_some() {
            return Err(TerminalError::new(format!(
                "task-local agent has ambiguous capability tool `{tool_name}`"
            ))
            .into());
        }
    }
    let circuit_owner = SecurityCircuitOwner::ExecutionTask {
        run_uid: started.run.run_uid,
        task_uid: started.task.task_id.as_uuid(),
        generation: started.task.generation,
    };
    let (
        mut messages,
        mut next_turn,
        mut usage,
        mut security_circuit,
        mut disabled_capabilities,
        mut pending_review,
        mut pending_tool_calls,
        mut pending_external,
    ) = match continuation {
        Some(TaskAttemptContinuation {
            state:
                TaskAttemptContinuationState::Agent {
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    pending_review,
                    pending_tool_calls,
                    pending_external,
                },
            ..
        }) => (
            messages.clone(),
            *next_turn,
            usage.clone(),
            security_circuit.clone(),
            disabled_capabilities.clone(),
            pending_review.as_deref().cloned(),
            pending_tool_calls.clone(),
            pending_external.clone(),
        ),
        Some(_) => {
            return Err(TerminalError::new(
                "task-local agent received an incompatible continuation",
            )
            .into());
        }
        None => {
            let skills = load_pinned_skills(workflow, ctx, started, skill_refs).await?;
            let mut circuit = SecurityCircuitState::default();
            circuit.adopt_owner(&circuit_owner);
            (
                vec![
                    ContextMessage::system(agent_system_prompt(instructions, &skills)),
                    ContextMessage::user(
                        json!({
                            "resolved_input": started.task.input,
                            "resume_inputs": started.task.resume_input_history,
                        })
                        .to_string(),
                    ),
                ],
                0,
                started.task.actual.clone(),
                circuit,
                BTreeMap::new(),
                None,
                Vec::new(),
                None,
            )
        }
    };
    security_circuit.adopt_owner(&circuit_owner);

    if let Some(external) = pending_external.take() {
        if let Some(external_job_uid) = external.external_job_uid {
            let resolution = continuation
                .and_then(|continuation| continuation.external_job_resolution.as_ref())
                .ok_or_else(|| {
                    TerminalError::new("agent external continuation has no terminal resolution")
                })?;
            let tool_use_id = external
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| format!("external-job-{external_job_uid}"));
            match resolution {
                AsyncToolJobTerminalOutcome::Completed { output } => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        output.to_string(),
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::Failed { error } => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        format!("external tool failed: {error}"),
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::Cancelled => {
                    messages.push(ContextMessage::tool_result(
                        tool_use_id,
                        "external tool was cancelled",
                        None,
                    ));
                }
                AsyncToolJobTerminalOutcome::UnknownOutcome { error } => {
                    return Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                        schema_version: 1,
                        usage,
                        result: ExecutionTaskResult::UnknownOutcome {
                            message: format!("external agent effect outcome is unknown: {error}"),
                        },
                    }));
                }
            }
        } else {
            // Provider start recovery proved NotStarted and re-admitted the exact continuation.
            // Reinsert the original model invocation so its stable tool id/idempotency key is
            // dispatched again without asking the model or repeating prior tool effects.
            pending_tool_calls.insert(0, external.invocation);
        }
    }

    if let Some(reviewed) = pending_review.take() {
        let resolution = continuation
            .and_then(|continuation| continuation.review_resolution.as_ref())
            .ok_or_else(|| {
                TerminalError::new("agent review continuation has no durable resolution")
            })?;
        match resolution {
            moa_execution::wire::ExecutionActionReviewResolution::Completed { tool_output } => {
                let output = serde_json::from_value::<moa_core::types::tools::SecuredToolOutput>(
                    tool_output.clone(),
                )
                .map_err(|error| {
                    TerminalError::new(format!("decode reviewed agent capability output: {error}"))
                })?;
                append_agent_tool_output(&mut messages, &reviewed.invocation, &output);
                usage.retrieved_bytes = usage
                    .retrieved_bytes
                    .saturating_add(serialized_len(&output.safe_output.structured_payload()));
            }
            moa_execution::wire::ExecutionActionReviewResolution::ExternalJob {
                external_job_uid,
                ..
            } => {
                return Ok(ActiveTaskAttemptExit::ExternalJob {
                    external_job_uid: *external_job_uid,
                    continuation: Some(agent_continuation(
                        messages,
                        next_turn,
                        usage,
                        security_circuit,
                        disabled_capabilities,
                        AgentPending {
                            review: None,
                            tool_calls: pending_tool_calls,
                            external: Some(PendingExternalToolInvocation {
                                external_job_uid: None,
                                invocation: reviewed.invocation,
                                effect_idempotency: reviewed.effect_idempotency,
                            }),
                        },
                    )),
                });
            }
            moa_execution::wire::ExecutionActionReviewResolution::Failed { class, message } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    class.clone(),
                    message.clone(),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::UnknownOutcome { message } => {
                return Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                    schema_version: 1,
                    usage,
                    result: ExecutionTaskResult::UnknownOutcome {
                        message: message.clone(),
                    },
                }));
            }
            moa_execution::wire::ExecutionActionReviewResolution::NotDispatched { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::Terminal,
                    execution_dispatch_rejection_message(*reason),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::Denied { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::AuthorizationDenied,
                    reason.clone(),
                    usage,
                )));
            }
            moa_execution::wire::ExecutionActionReviewResolution::TimedOut { reason } => {
                return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                    ExecutionFailureClass::DeadlineExceeded,
                    reason.clone(),
                    usage,
                )));
            }
        }
    }

    if pending_tool_calls.is_empty() {
        if next_turn >= max_turns {
            return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                format!("task-local agent exhausted max_turns={max_turns}"),
                usage,
            )));
        }
        let mut completion = moa_core::types::completion::CompletionRequest {
            model: None,
            messages: messages.clone(),
            tools: capabilities
                .iter()
                .filter(|(name, _)| !disabled_capabilities.contains_key(*name))
                .map(|(name, capability)| agent_tool_schema(name, capability))
                .collect(),
            max_output_tokens: None,
            temperature: None,
            response_format: None,
            native_web_search: Default::default(),
            metadata: std::collections::HashMap::new(),
        };
        let owner = LLMCompletionOwner::execution_task_attempt(request.dispatch_uid);
        attach_completion_owner(&mut completion, &owner);
        if !record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ModelTurnStart)
            .await?
        {
            return Ok(ActiveTaskAttemptExit::OwnershipLost);
        }
        let response = crate::restate_identity::replay_safe_request(
            ctx.service_client::<LLMGatewayClient>()
                .complete_bounded(Json::from(BoundedCompletionRequest {
                    request: completion,
                    budget: ResourceBudget::until(request.attempt_deadline_at),
                }))
                .idempotency_key(completion_idempotency_key(
                    ctx.invocation_id(),
                    LLMCompletionAction::ExecutionTaskModel {
                        generation: started.task.generation,
                        turn: next_turn,
                    },
                )),
        )
        .call()
        .await?
        .into_inner();
        if !record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ModelTurn).await? {
            return Ok(ActiveTaskAttemptExit::OwnershipLost);
        }
        usage.tokens = usage
            .tokens
            .saturating_add(response.usage.total_input_tokens() as u64)
            .saturating_add(response.usage.output_tokens as u64);
        usage.cost_microusd = usage.cost_microusd.saturating_add(
            moa_providers::pricing_for_model(response.model.as_str())
                .map(|pricing| pricing.cost_micros(&response.usage))
                .unwrap_or_default(),
        );
        let tool_calls = response
            .content
            .iter()
            .filter_map(|content| match content {
                CompletionContent::ToolCall(call) => Some(call.invocation.clone()),
                CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
            })
            .collect::<Vec<_>>();
        if tool_calls.is_empty() {
            let outcome = moa_execution::state::parse_agent_task_outcome(&response.text, usage);
            if matches!(outcome.result, ExecutionTaskResult::NeedsInput { .. }) {
                messages.push(ContextMessage::assistant_with_thought_signature(
                    response.text,
                    response.thought_signature,
                ));
                return Ok(ActiveTaskAttemptExit::InputPending {
                    continuation: agent_continuation(
                        messages,
                        next_turn.saturating_add(1),
                        outcome.usage.clone(),
                        security_circuit,
                        disabled_capabilities,
                        AgentPending {
                            review: None,
                            tool_calls: pending_tool_calls,
                            external: pending_external,
                        },
                    ),
                    outcome,
                });
            }
            return Ok(ActiveTaskAttemptExit::Outcome(outcome));
        }
        for (index, invocation) in tool_calls.iter().cloned().enumerate() {
            messages.push(ContextMessage::assistant_tool_call_with_thought_signature(
                invocation,
                if index == 0 {
                    response.text.clone()
                } else {
                    String::new()
                },
                (index == 0)
                    .then(|| response.thought_signature.clone())
                    .flatten(),
            ));
        }
        pending_tool_calls = tool_calls;
        next_turn = next_turn.saturating_add(1);
        return Ok(ActiveTaskAttemptExit::Continue {
            continuation: agent_continuation(
                messages,
                next_turn,
                usage,
                security_circuit,
                disabled_capabilities,
                AgentPending {
                    review: None,
                    tool_calls: pending_tool_calls,
                    external: pending_external,
                },
            ),
        });
    }

    let invocation = pending_tool_calls.remove(0);
    let capability = capabilities.get(&invocation.name).copied().ok_or_else(|| {
        TerminalError::new(format!(
            "agent emitted undeclared capability `{}`",
            invocation.name
        ))
    })?;
    if disabled_capabilities.contains_key(&invocation.name) {
        let tool_use_id = invocation
            .id
            .clone()
            .unwrap_or_else(|| format!("execution-{}-{next_turn}", started.task.task_id));
        messages.push(ContextMessage::tool_result(
            tool_use_id,
            "This tool capability is disabled for this task by the security circuit.",
            None,
        ));
        return Ok(ActiveTaskAttemptExit::Continue {
            continuation: agent_continuation(
                messages,
                next_turn,
                usage,
                security_circuit,
                disabled_capabilities,
                AgentPending {
                    review: None,
                    tool_calls: pending_tool_calls,
                    external: pending_external,
                },
            ),
        });
    }
    let session = load_session(workflow, ctx, &started.run, &started.task).await?;
    let tool_id = ToolCallId(Uuid::new_v5(
        &started.task.task_id.as_uuid(),
        format!(
            "agent-tool:{}:{}:{}",
            started.task.generation,
            next_turn,
            invocation.id.as_deref().unwrap_or(&invocation.name)
        )
        .as_bytes(),
    ));
    let tool_call = ToolCallContent {
        invocation: invocation.clone(),
        provider_metadata: None,
    };
    let allowed_tools = capabilities.keys().cloned().collect::<BTreeSet<_>>();
    let provenance = CapabilityProvenance {
        kind: Some(capability_source_kind(&capability.source).to_string()),
        id: Some(format!(
            "{}@{}",
            capability.reference.name, capability.reference.version
        )),
        step_id: Some(started.task.node_id.clone()),
    };
    if matches!(
        capability.async_mode,
        ToolAsyncMode::MayReturnExternalJob { .. }
    ) {
        let provisional = agent_continuation(
            messages.clone(),
            next_turn,
            usage.clone(),
            security_circuit.clone(),
            disabled_capabilities.clone(),
            AgentPending {
                review: None,
                tool_calls: pending_tool_calls.clone(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: capability.idempotency_class,
                }),
            },
        );
        if !persist_external_start_checkpoint(
            workflow,
            ctx,
            request,
            started,
            TaskAttemptCheckpointKind::AgentContinuation,
            &provisional,
        )
        .await?
        {
            return Ok(ActiveTaskAttemptExit::OwnershipLost);
        }
    }
    if !begin_capability_dispatch(workflow, ctx, request, capability, &tool_call).await? {
        return Ok(ActiveTaskAttemptExit::OwnershipLost);
    }
    let governed = invoke_governed_tool(
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
            origin: GovernedInvocationOrigin::ExecutionTask {
                run_uid: started.run.run_uid,
                task_uid: started.task.task_id.as_uuid(),
                generation: started.task.generation,
                attempt_generation: request.attempt_generation,
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
    if !record_attempt_heartbeat(workflow, ctx, request, AttemptHeartbeat::ToolCall).await? {
        return Ok(ActiveTaskAttemptExit::OwnershipLost);
    }
    usage.tool_calls = usage.tool_calls.saturating_add(1);
    match governed {
        GovernedInvocationOutcome::Completed(result)
            if result.disposition == GovernedInvocationDisposition::ReviewPending =>
        {
            let review = result.review.ok_or_else(|| {
                TerminalError::new("review-pending agent result is missing durable review identity")
            })?;
            Ok(ActiveTaskAttemptExit::ReviewPending {
                continuation: agent_continuation(
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    AgentPending {
                        review: Some(PendingReviewedToolInvocation {
                            review_uid: review.review_uid,
                            expires_at: review.expires_at,
                            invocation: result.invocation,
                            effect_idempotency: capability.idempotency_class,
                        }),
                        tool_calls: pending_tool_calls,
                        external: pending_external,
                    },
                ),
            })
        }
        GovernedInvocationOutcome::Completed(result) => {
            let output = result.output;
            usage.retrieved_bytes = usage
                .retrieved_bytes
                .saturating_add(serialized_len(&output.safe_output.structured_payload()));
            if !output.assessment.is_safe() {
                moa_security::apply_owner_assessment(
                    &mut security_circuit,
                    moa_security::CircuitTarget {
                        session_id: session.id,
                        owner: &circuit_owner,
                        capability: &output.capability,
                        tool_call_id: tool_id,
                    },
                    &output.assessment,
                )
                .map_err(|_| TerminalError::new("agent security assessment owner mismatch"))?;
                let stage = security_circuit.stage(&circuit_owner, &output.capability);
                if !stage.permits_dispatch() {
                    disabled_capabilities
                        .insert(invocation.name.clone(), output.capability.clone());
                }
                if stage == SecurityCircuitStage::Halted {
                    return Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                        ExecutionFailureClass::Terminal,
                        "task stopped after unsafe capability output".to_string(),
                        usage,
                    )));
                }
                if stage == SecurityCircuitStage::SuspendedForInput {
                    append_agent_tool_output(&mut messages, &invocation, &output);
                    let outcome = ExecutionTaskOutcome {
                        schema_version: 1,
                        usage: usage.clone(),
                        result: ExecutionTaskResult::NeedsInput {
                            question: "A capability returned potentially unsafe content. Continue?"
                                .to_string(),
                            audience: moa_artifacts::execution_plan::InputAudience::User,
                        },
                    };
                    return Ok(ActiveTaskAttemptExit::InputPending {
                        outcome,
                        continuation: agent_continuation(
                            messages,
                            next_turn,
                            usage,
                            security_circuit,
                            disabled_capabilities,
                            AgentPending {
                                review: None,
                                tool_calls: pending_tool_calls,
                                external: pending_external,
                            },
                        ),
                    });
                }
            }
            append_agent_tool_output(&mut messages, &invocation, &output);
            Ok(ActiveTaskAttemptExit::Continue {
                continuation: agent_continuation(
                    messages,
                    next_turn,
                    usage,
                    security_circuit,
                    disabled_capabilities,
                    AgentPending {
                        review: None,
                        tool_calls: pending_tool_calls,
                        external: pending_external,
                    },
                ),
            })
        }
        GovernedInvocationOutcome::ExternalJob {
            external_job_uid, ..
        } => Ok(ActiveTaskAttemptExit::ExternalJob {
            external_job_uid,
            continuation: Some(agent_continuation(
                messages,
                next_turn,
                usage,
                security_circuit,
                disabled_capabilities,
                AgentPending {
                    review: None,
                    tool_calls: pending_tool_calls,
                    external: Some(PendingExternalToolInvocation {
                        external_job_uid: None,
                        invocation,
                        effect_idempotency: capability.idempotency_class,
                    }),
                },
            )),
        }),
        GovernedInvocationOutcome::UnknownOutcome { message, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(ExecutionTaskOutcome {
                schema_version: 1,
                usage,
                result: ExecutionTaskResult::UnknownOutcome { message },
            }))
        }
        GovernedInvocationOutcome::NotDispatched { reason, .. } => {
            Ok(ActiveTaskAttemptExit::Outcome(failed_task_outcome(
                ExecutionFailureClass::Terminal,
                execution_dispatch_rejection_message(reason),
                usage,
            )))
        }
        GovernedInvocationOutcome::Delegation { .. } => Err(TerminalError::new(
            "execution task agents cannot invoke delegation capabilities",
        )
        .into()),
    }
}

fn agent_continuation(
    messages: Vec<ContextMessage>,
    next_turn: u32,
    usage: ExecutionUsage,
    security_circuit: SecurityCircuitState,
    disabled_capabilities: BTreeMap<String, ToolCapabilityId>,
    pending: AgentPending,
) -> TaskAttemptContinuation {
    TaskAttemptContinuation {
        schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
        state: TaskAttemptContinuationState::Agent {
            messages,
            next_turn,
            usage,
            security_circuit,
            disabled_capabilities,
            pending_review: pending.review.map(Box::new),
            pending_tool_calls: pending.tool_calls,
            pending_external: pending.external,
        },
        review_resolution: None,
        external_job_resolution: None,
        workspace_release_receipt_id: None,
    }
}

fn agent_tool_schema(name: &str, capability: &ExecutionCapability) -> Value {
    json!({
        "name": name,
        "description": capability.description,
        "input_schema": capability.input_schema,
    })
}

fn append_agent_tool_output(
    messages: &mut Vec<ContextMessage>,
    invocation: &ToolInvocation,
    output: &moa_core::types::tools::SecuredToolOutput,
) {
    let tool_use_id = invocation.id.clone().unwrap_or_else(|| {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{}:{}", invocation.name, invocation.input).as_bytes(),
        )
        .to_string()
    });
    messages.push(ContextMessage::tool_result(
        tool_use_id,
        output.safe_output.to_text(),
        Some(output.safe_output.content.clone()),
    ));
}

async fn load_pinned_skills(
    workflow: &ExecutionTaskAttemptImpl,
    ctx: &WorkflowContext<'_>,
    started: &TaskAttemptRecord,
    skill_refs: &[moa_artifacts::reference::ArtifactRef],
) -> Result<Vec<String>, HandlerError> {
    let mut markdown = Vec::with_capacity(skill_refs.len());
    let scope = started.run.contact_id.map_or(
        ActionRuleScope::Tenant {
            tenant_id: started.run.tenant_id,
        },
        |contact_id| ActionRuleScope::Contact {
            tenant_id: started.run.tenant_id,
            contact_id,
        },
    );
    for (index, skill_ref) in skill_refs.iter().enumerate() {
        if !started.run.authorization.skill_refs.contains(skill_ref) {
            return Err(TerminalError::new(
                "task requested a skill outside its authorization envelope",
            )
            .into());
        }
        let pinned = started
            .run
            .pinned_instruction_skills
            .iter()
            .find(|pinned| pinned.skill_ref == *skill_ref)
            .ok_or_else(|| TerminalError::new("task requested an unpinned skill"))?;
        let pool = workflow.pool.clone();
        let revision_uid = pinned.revision_uid;
        let loaded = ctx
            .run(|| async move {
                moa_skills::registry::SkillRegistry::new(pool)
                    .load_skill_markdown(&scope, revision_uid)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name(format!("task_attempt_skill:{index}:{revision_uid}"))
            .await?
            .into_inner();
        markdown.push(loaded);
    }
    Ok(markdown)
}

fn agent_system_prompt(instructions: &str, skills: &[String]) -> String {
    format!(
        "{instructions}\n\nPinned instruction skills:\n{}\n\nReturn only JSON.",
        skills.join("\n\n---\n\n")
    )
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::types::tools::IdempotencyClass;

    use super::*;

    // Pins: once an asynchronous provider start commits, the durable checkpoint
    // retains the exact model invocation, effect semantics, and MOA job identity;
    // decoding the checkpoint must not reconstruct or resend that effect.
    #[test]
    fn agent_external_continuation_round_trips_exact_effect_owner_offline() {
        let external_job_uid = Uuid::from_u128(41);
        let invocation = ToolInvocation {
            id: Some("provider-call-7".to_string()),
            name: "render_video".to_string(),
            input: json!({"prompt": "durable sunrise"}),
        };
        let mut continuation = agent_continuation(
            vec![ContextMessage::user("render a durable sunrise")],
            3,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
            },
        );

        continuation
            .bind_external_job(external_job_uid)
            .expect("fresh external continuation must accept its durable job identity");
        let persisted = continuation
            .to_bounded_json()
            .expect("exact continuation must fit the durable bound");
        let decoded: TaskAttemptContinuation =
            serde_json::from_value(persisted).expect("persisted continuation must decode");

        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            next_turn,
            ..
        } = decoded.state
        else {
            panic!("external continuation lost its exact pending effect");
        };
        assert_eq!(next_turn, 3);
        assert_eq!(pending.external_job_uid, Some(external_job_uid));
        assert_eq!(pending.invocation, invocation);
        assert_eq!(pending.effect_idempotency, IdempotencyClass::NonIdempotent);
    }

    // Pins: a storage-only review checkpoint retains the exact reviewed
    // invocation and expiry across serialization, so a resumed attempt consumes
    // the decision without regenerating the provider effect.
    #[test]
    fn agent_review_continuation_round_trips_exact_effect_fence_offline() {
        let review_uid = Uuid::from_u128(51);
        let expires_at = Utc
            .with_ymd_and_hms(2030, 5, 6, 7, 8, 9)
            .single()
            .expect("fixed review expiry");
        let invocation = ToolInvocation {
            id: Some("reviewed-call-2".to_string()),
            name: "publish_release".to_string(),
            input: json!({"version": "2.0.0"}),
        };
        let continuation = agent_continuation(
            vec![ContextMessage::user("publish only after review")],
            2,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: Some(PendingReviewedToolInvocation {
                    review_uid,
                    expires_at,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
                tool_calls: Vec::new(),
                external: None,
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("review continuation must fit the durable bound"),
        )
        .expect("persisted review continuation must decode");
        assert_eq!(decoded.pending_review_uid(), Some(review_uid));
        let TaskAttemptContinuationState::Agent {
            pending_review: Some(pending),
            ..
        } = decoded.state
        else {
            panic!("review continuation lost its exact pending effect");
        };
        assert_eq!(pending.expires_at, expires_at);
        assert_eq!(pending.invocation, invocation);
    }

    // Pins: an input boundary keeps the already-completed model turn and circuit
    // state in the bounded checkpoint; resumption starts at the following turn
    // instead of calling the model again for the same prompt.
    #[test]
    fn agent_input_continuation_round_trips_next_turn_and_messages_offline() {
        let continuation = agent_continuation(
            vec![
                ContextMessage::user("inspect the unsafe payload"),
                ContextMessage::assistant("May I continue with the unsafe payload?"),
            ],
            4,
            ExecutionUsage {
                cost_microusd: 17,
                tokens: 23,
                tool_calls: 2,
                retrieved_bytes: 31,
            },
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: None,
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("input continuation must fit the durable bound"),
        )
        .expect("persisted input continuation must decode");
        let TaskAttemptContinuationState::Agent {
            messages,
            next_turn,
            usage,
            ..
        } = decoded.state
        else {
            panic!("input continuation changed state kind");
        };
        assert_eq!(next_turn, 4);
        assert_eq!(messages.len(), 2);
        assert_eq!(usage.tokens, 23);
        assert_eq!(usage.tool_calls, 2);
    }

    // Pins: provider recovery may prove that a reserved start never happened;
    // the current checkpoint must retain the exact invocation with no job UID so
    // the successor attempt can replay that call without repeating the model turn.
    #[test]
    fn provisional_agent_external_start_round_trips_without_a_job_uid_offline() {
        let invocation = ToolInvocation {
            id: Some("stable-provider-call".to_string()),
            name: "render_video".to_string(),
            input: json!({"prompt": "recover this exact effect"}),
        };
        let continuation = agent_continuation(
            vec![ContextMessage::user("render once")],
            2,
            zero_usage(),
            SecurityCircuitState::default(),
            BTreeMap::new(),
            AgentPending {
                review: None,
                tool_calls: Vec::new(),
                external: Some(PendingExternalToolInvocation {
                    external_job_uid: None,
                    invocation: invocation.clone(),
                    effect_idempotency: IdempotencyClass::NonIdempotent,
                }),
            },
        );

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("provisional continuation must fit"),
        )
        .expect("provisional continuation must decode");
        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            ..
        } = decoded.state
        else {
            panic!("provisional external start lost its pending invocation");
        };
        assert_eq!(pending.external_job_uid, None);
        assert_eq!(pending.invocation, invocation);
    }

    const fn zero_usage() -> ExecutionUsage {
        ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        }
    }
}

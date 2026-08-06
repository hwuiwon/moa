//! Input and output guardrail evaluation for root turns.

use moa_core::{
    events::Event, types::agent::AgentContext, types::completion::CompletionResponse,
    types::guardrails::GuardrailDecision, types::guardrails::GuardrailDirection,
    types::identifiers::SessionId, types::provider::ModelTier, types::resource::ResourceBudget,
    types::session::SessionMeta,
};
use moa_wire::turn::{TurnOutcomeKind, TurnPhase};
use restate_sdk::prelude::*;

use crate::services::llm_gateway::{
    BoundedCompletionRequest, LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient,
    attach_completion_owner, completion_idempotency_key,
};
use crate::turn_driver::{guardrails as driver_guardrails, progress as driver_progress};
use crate::workflows::child_invocation::{ChildInvocationOutcome, cancel_and_join_child_call};
use crate::workflows::errors::moa_error_to_handler_error;
use crate::workflows::turn_events::append_session_event;
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL, SUMMARY_CHECKING_RESULTS};

use super::{BodyOutcome, TurnExecutionImpl};

pub(super) async fn evaluate_input_guardrail(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    user_message: &str,
    resource_budget: ResourceBudget,
) -> Result<Option<BodyOutcome>, HandlerError> {
    let Some(agent_context) = meta.agent_context.as_ref() else {
        return Ok(None);
    };
    let policy =
        AgentContext::parsed_policy_snapshot(agent_context).map_err(moa_error_to_handler_error)?;
    let Some(stage) = policy.guardrail_policy.stage(GuardrailDirection::Input) else {
        return Ok(None);
    };
    if !stage.is_active() {
        return Ok(None);
    }

    turn_progress::maybe_emit(
        ctx,
        session_id,
        SUMMARY_CALLING_MODEL,
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let mut guardrail_request = crate::guardrails::guardrail_completion_request(
        workflow.config.as_ref(),
        GuardrailDirection::Input,
        stage,
        user_message,
    );
    attach_completion_owner(
        &mut guardrail_request,
        &LLMCompletionOwner::root_turn(ctx.key()),
    );
    let call = crate::restate_identity::replay_safe_request(
        ctx.service_client::<LLMGatewayClient>()
            .complete_bounded(Json::from(BoundedCompletionRequest {
                request: guardrail_request,
                budget: super::per_model_call_budget(resource_budget),
            }))
            .idempotency_key(completion_idempotency_key(
                ctx.invocation_id(),
                LLMCompletionAction::RootInputGuardrail,
            )),
    )
    .call();
    let response = match cancel_and_join_child_call(
        ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE),
        call,
    )
    .await?
    {
        ChildInvocationOutcome::Completed(response) => response.into_inner(),
        ChildInvocationOutcome::Cancelled(reason) => {
            return Ok(Some(BodyOutcome {
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
                post_outcome_assessment: None,
            }));
        }
    };
    if response.stop_reason == moa_core::types::completion::StopReason::Cancelled {
        let reason = ctx
            .promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE)
            .await?;
        return Ok(Some(BodyOutcome {
            kind: TurnOutcomeKind::Cancelled,
            message: reason,
            post_outcome_assessment: None,
        }));
    }
    let evaluation = crate::guardrails::evaluate_guardrail_response(
        &agent_context.policy_hash,
        GuardrailDirection::Input,
        stage,
        &response,
    );
    append_session_event(
        workflow.event_appender(),
        ctx,
        session_id,
        evaluation.to_event(),
    )
    .await?;

    if matches!(evaluation.decision, GuardrailDecision::Block) {
        let text = driver_guardrails::block_message(driver_guardrails::GuardrailBlockMessage {
            stage,
            fallback: "I can't help with that request.",
        });
        append_session_event(
            workflow.event_appender(),
            ctx,
            session_id,
            Event::BrainResponse {
                text,
                thought_signature: None,
                model: evaluation.model.clone(),
                model_tier: ModelTier::Auxiliary,
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
                llm_ttft_ms: None,
            },
        )
        .await?;
        return Ok(Some(BodyOutcome {
            kind: TurnOutcomeKind::Completed,
            message: "input guardrail blocked".to_string(),
            post_outcome_assessment: None,
        }));
    }

    Ok(None)
}

pub(super) async fn visible_response_after_output_guardrail(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    response: &CompletionResponse,
    resource_budget: ResourceBudget,
    model_turn: usize,
) -> Result<OutputGuardrailOutcome, HandlerError> {
    if response.text.is_empty() {
        return Ok(OutputGuardrailOutcome::Completed(response.clone(), false));
    }
    let Some(agent_context) = meta.agent_context.as_ref() else {
        return Ok(OutputGuardrailOutcome::Completed(response.clone(), false));
    };
    let policy =
        AgentContext::parsed_policy_snapshot(agent_context).map_err(moa_error_to_handler_error)?;
    let Some(stage) = policy.guardrail_policy.stage(GuardrailDirection::Output) else {
        return Ok(OutputGuardrailOutcome::Completed(response.clone(), false));
    };
    if !stage.is_active() {
        return Ok(OutputGuardrailOutcome::Completed(response.clone(), false));
    }

    driver_progress::set_phase(ctx, TurnPhase::Persisting);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        SUMMARY_CHECKING_RESULTS,
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let mut guardrail_request = crate::guardrails::guardrail_completion_request(
        workflow.config.as_ref(),
        GuardrailDirection::Output,
        stage,
        &response.text,
    );
    attach_completion_owner(
        &mut guardrail_request,
        &LLMCompletionOwner::root_turn(ctx.key()),
    );
    let call = crate::restate_identity::replay_safe_request(
        ctx.service_client::<LLMGatewayClient>()
            .complete_bounded(Json::from(BoundedCompletionRequest {
                request: guardrail_request,
                budget: super::per_model_call_budget(resource_budget),
            }))
            .idempotency_key(completion_idempotency_key(
                ctx.invocation_id(),
                LLMCompletionAction::RootOutputGuardrail { turn: model_turn },
            )),
    )
    .call();
    let judge_response = match cancel_and_join_child_call(
        ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE),
        call,
    )
    .await?
    {
        ChildInvocationOutcome::Completed(response) => response.into_inner(),
        ChildInvocationOutcome::Cancelled(reason) => {
            return Ok(OutputGuardrailOutcome::Cancelled(reason));
        }
    };
    if judge_response.stop_reason == moa_core::types::completion::StopReason::Cancelled {
        let reason = ctx
            .promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE)
            .await?;
        return Ok(OutputGuardrailOutcome::Cancelled(reason));
    }
    let evaluation = crate::guardrails::evaluate_guardrail_response(
        &agent_context.policy_hash,
        GuardrailDirection::Output,
        stage,
        &judge_response,
    );
    append_session_event(
        workflow.event_appender(),
        ctx,
        session_id,
        evaluation.to_event(),
    )
    .await?;

    if matches!(evaluation.decision, GuardrailDecision::Block) {
        let visible_response =
            driver_guardrails::blocked_output_response(driver_guardrails::BlockedOutputResponse {
                response,
                stage,
            });
        return Ok(OutputGuardrailOutcome::Completed(visible_response, true));
    }

    Ok(OutputGuardrailOutcome::Completed(response.clone(), false))
}

/// Result of evaluating the optional output guardrail.
pub(super) enum OutputGuardrailOutcome {
    /// The guardrail completed and reports whether it replaced the response.
    Completed(CompletionResponse, bool),
    /// The turn was cancelled while the guardrail model call was in flight.
    Cancelled(String),
}

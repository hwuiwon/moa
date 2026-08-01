//! Shared streamed-turn execution loop.

mod signals;

use std::time::Instant;

use chrono::Utc;
use moa_core::{
    error::MoaError, error::Result, events::Event, types::completion::CompletionContent,
    types::completion::StopReason, types::completion::TokenUsage, types::context::WorkingContext,
    types::context::estimate_text_tokens, types::events_stream::EventRange,
    types::model::TokenPricing, types::observability::TraceContext,
    types::observability::genai_operation_name, types::observability::genai_provider_name,
    types::provider::ModelTask, types::resource::ResourceAmounts, types::resource::ResourceBudget,
    types::session::SessionMeta,
};
use moa_hands::ToolRouter;
use moa_lineage_core::TurnId;
use moa_observability::{
    apply_trace_context_to_span, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
};
use tracing::Instrument;

use self::signals::{drain_signal_queue, handle_stream_signal};
use crate::lineage::{emit_context_lineage, emit_generation_lineage};
use crate::runtime_events::RuntimeEvent;
use crate::turn::{StreamSignalDisposition, stream_completion_response};

use super::budget::enforce_tenant_budget;
use super::context_build::{
    BuildTurnContextOptions, append_event, build_cache_report, build_turn_context,
    complete_cache_report, last_user_message_text, record_turn_span_metrics,
    turn_number_for_events,
};
use super::tool_dispatch::{ToolCallOutcome, ToolFailure, handle_tool_call};
use super::{BrainTurnRequest, StreamedTurnRequest, StreamedTurnResult};

const TURN_EVENT_TAIL_LIMIT: usize = 16;

pub(super) async fn run_streamed_turn(
    request: StreamedTurnRequest<'_>,
) -> Result<StreamedTurnResult> {
    let StreamedTurnRequest {
        turn:
            BrainTurnRequest {
                identity,
                session_id,
                session_store,
                llm_provider,
                pipeline,
                tool_router,
            },
        runtime_tx,
        event_tx,
        cancel_token,
        hard_cancel_token,
        resource_budget,
        signal_state,
        lineage,
    } = request;
    let (mut signal_rx, turn_requested, soft_cancel_requested) = match signal_state {
        Some(state) => (
            Some(state.signal_rx),
            Some(state.turn_requested),
            Some(state.soft_cancel_requested),
        ),
        None => (None, None, None),
    };
    let initial_session = session_store.get_session(session_id).await?;
    let initial_events = session_store
        .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
        .await?;
    let turn_number = turn_number_for_events(&initial_events);
    let trace_context =
        TraceContext::from_session_meta(&initial_session, last_user_message_text(&initial_events));
    let turn_span = tracing::Span::current();
    let turn_id = TurnId::new_v7();
    turn_span.record("moa.turn.number", turn_number);
    turn_span.record("moa.turn.id", tracing::field::display(turn_id.0));
    turn_span.record("moa.model", tracing::field::display(&initial_session.model));
    apply_trace_context_to_span(&trace_context, &turn_span);

    let mut local_turn_requested = false;
    let turn_requested = turn_requested.unwrap_or(&mut local_turn_requested);
    let mut local_soft_cancel_requested = false;
    let soft_cancel_requested = soft_cancel_requested.unwrap_or(&mut local_soft_cancel_requested);

    async move {
        let cancel_token = cancel_token;
        let hard_cancel_token = hard_cancel_token;
        let mut total_tool_calls = 0usize;
        let mut total_input_tokens = 0usize;
        let mut total_output_tokens = 0usize;
        // First terminal tool failure seen this turn, if any. Capped at one so a
        // turn writes at most one incident when it completes.
        let mut durable_failure: Option<ToolFailure> = None;

        loop {
            let session = session_store.get_session(session_id).await?;
            let events = session_store
                .get_events(session_id, EventRange::recent(TURN_EVENT_TAIL_LIMIT))
                .await?;

            let pipeline_compile_span = tracing::info_span!(
                "pipeline_compile",
                moa.pipeline.stages = pipeline.stage_count() as i64,
                moa.pipeline.total_tokens = tracing::field::Empty,
            );
            let workspace_root = match &tool_router {
                Some(router) => router.workspace_root(&session.tenant_id).await,
                None => None,
            };
            let (mut ctx, active_canary) = build_turn_context(BuildTurnContextOptions {
                identity: &identity,
                session_id: &session_id,
                session: &session,
                session_store: &session_store,
                pipeline,
                llm_provider: &llm_provider,
                workspace_root,
                enable_canary: tool_router.is_some(),
                trace_context: &trace_context,
                snapshot_max_size_bytes: pipeline.snapshot_config().max_size_bytes,
                turn_id,
            })
            .instrument(pipeline_compile_span.clone())
            .await?;
            pipeline_compile_span.record("moa.pipeline.total_tokens", ctx.token_count as i64);
            register_selected_skill_files(tool_router.as_deref(), &session, &mut ctx).await;
            augment_agentic_memory_tools(&mut ctx, tool_router.as_deref());
            let citation_sources =
                emit_context_lineage(lineage.as_ref(), turn_id, &session, &ctx, &pipeline_compile_span)
                    .await;

            let mut emit_runtime = |event| {
                let _ = runtime_tx.send(event);
            };

            enforce_tenant_budget(
                &session_store,
                &session_id,
                &session.tenant_id,
                pipeline.daily_tenant_budget_cents(),
                runtime_tx,
                event_tx,
            )
            .await?;

            let model_dispatch_budget = *resource_budget;
            let estimated_input_token_count = ctx
                .tools()
                .iter()
                .fold(ctx.token_count, |total, tool| {
                    total.saturating_add(estimate_text_tokens(&tool.to_string()))
                });
            let estimated_input_tokens =
                u64::try_from(estimated_input_token_count).unwrap_or(u64::MAX);
            let pricing = llm_provider.capabilities().pricing;
            let minimum_model_cost =
                conservative_model_cost(&pricing, estimated_input_token_count, 1);
            let minimum_model_usage = ResourceAmounts {
                cost_micro_usd: minimum_model_cost,
                tokens: estimated_input_tokens.saturating_add(1),
                turns: 1,
                model_calls: 1,
                ..ResourceAmounts::ZERO
            };
            // Validate the prompt plus one output token without charging the
            // estimate. Provider-reported token usage is charged below, while
            // turns and calls are reserved before the request leaves MOA.
            model_dispatch_budget.try_consume_at(minimum_model_usage, Utc::now())?;
            *resource_budget = model_dispatch_budget.try_consume_at(
                ResourceAmounts {
                    turns: 1,
                    model_calls: 1,
                    ..ResourceAmounts::ZERO
                },
                Utc::now(),
            )?;
            let mut request = ctx.into_request();
            if let Some(remaining) = model_dispatch_budget.remaining {
                let token_budget_cap = usize::try_from(
                    remaining.tokens.saturating_sub(estimated_input_tokens),
                )
                .unwrap_or(usize::MAX);
                let cost_budget_cap = affordable_output_tokens(
                    &pricing,
                    usize::try_from(estimated_input_tokens).unwrap_or(usize::MAX),
                    remaining.cost_micro_usd,
                );
                let output_cap = token_budget_cap.min(cost_budget_cap);
                request.max_output_tokens = Some(
                    request
                        .max_output_tokens
                        .map_or(output_cap, |configured| configured.min(output_cap)),
                );
            }
            let request_model = request
                .model
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| session.model.to_string());
            let provider_name = genai_provider_name(llm_provider.name()).to_string();
            let operation_name = genai_operation_name(llm_provider.name()).to_string();
            let cache_report = build_cache_report(&events, llm_provider.name(), &request);
            let llm_call_span = tracing::info_span!(
                "llm_call",
                otel.kind = "client",
                gen_ai.operation.name = %operation_name,
                gen_ai.provider.name = %provider_name,
                gen_ai.request.model = %request_model,
                gen_ai.response.model = tracing::field::Empty,
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
                gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
                gen_ai.response.time_to_first_chunk = tracing::field::Empty,
            );
            let llm_call_started = Instant::now();
            let streamed = if let Some(receiver) = signal_rx.as_deref_mut() {
                stream_completion_response(
                    llm_provider.clone(),
                    request,
                    Some(&llm_call_span),
                    cancel_token.as_ref(),
                    Some(receiver),
                    &mut emit_runtime,
                    |signal| {
                        handle_stream_signal(
                            signal,
                            runtime_tx,
                            turn_requested,
                            soft_cancel_requested,
                        )
                    },
                )
                .instrument(llm_call_span.clone())
                .await?
            } else {
                stream_completion_response(
                    llm_provider.clone(),
                    request,
                    Some(&llm_call_span),
                    cancel_token.as_ref(),
                    None,
                    &mut emit_runtime,
                    |_| StreamSignalDisposition::Continue,
                )
                .instrument(llm_call_span.clone())
                .await?
            };
            let llm_call_duration = llm_call_started.elapsed();
            record_turn_llm_call_duration(llm_call_duration);
            if streamed.cancelled {
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "cancelled",
                );
                return Ok(StreamedTurnResult::Cancelled);
            }
            let response = streamed.response.ok_or_else(|| {
                MoaError::ProviderError(
                    "streamed turn finished without a provider response".to_string(),
                )
            })?;
            let response_usage = response.token_usage();
            let response_has_output = !streamed.streamed_text.is_empty()
                || !response.text.is_empty()
                || !response.content.is_empty();
            let response_resource_usage =
                reported_model_usage(&pricing, &response_usage, response_has_output)?;
            let response_cost_cents = pricing.cost_cents(&response_usage);
            let response_cost_micros = response_resource_usage.cost_micro_usd;
            *resource_budget =
                resource_budget.try_consume_at(response_resource_usage, Utc::now())?;
            emit_generation_lineage(
                lineage.as_ref(),
                turn_id,
                &session,
                llm_provider.name(),
                &request_model,
                &response,
                &citation_sources,
                response_cost_micros,
                llm_call_duration,
                &llm_call_span,
                None,
            )
            .await;
            llm_call_span.record("gen_ai.response.model", tracing::field::display(&response.model));
            llm_call_span.record(
                "gen_ai.usage.input_tokens",
                response_usage.total_input_tokens() as i64,
            );
            llm_call_span.record("gen_ai.usage.output_tokens", response_usage.output_tokens as i64);
            llm_call_span.record(
                "gen_ai.usage.cache_read.input_tokens",
                response_usage.input_tokens_cache_read as i64,
            );
            llm_call_span.record(
                "gen_ai.usage.cache_creation.input_tokens",
                response_usage.input_tokens_cache_write as i64,
            );
            total_input_tokens += response_usage.total_input_tokens();
            total_output_tokens += response_usage.output_tokens;
            append_event(
                &session_store,
                event_tx,
                session_id,
                Event::CacheReport {
                    report: complete_cache_report(cache_report, &response),
                },
            )
            .await?;

            if !streamed.streamed_text.trim().is_empty() {
                append_event(
                    &session_store,
                    event_tx,
                    session_id,
                    Event::BrainResponse {
                        text: streamed.streamed_text.clone(),
                        thought_signature: response.thought_signature.clone(),
                        model: response.model.clone(),
                        model_tier: ModelTask::MainLoop.tier(),
                        input_tokens_uncached: response_usage.input_tokens_uncached,
                        input_tokens_cache_write: response_usage.input_tokens_cache_write,
                        input_tokens_cache_read: response_usage.input_tokens_cache_read,
                        output_tokens: response_usage.output_tokens,
                        cost_cents: response_cost_cents,
                        duration_ms: response.duration_ms,
                        llm_ttft_ms: streamed.ttft_ms,
                    },
                )
                .await?;
                // This is the terminal assistant event for a turn; warn on a
                // dropped receiver so stream consumers do not silently miss it.
                if let Err(err) = runtime_tx.send(RuntimeEvent::AssistantFinished {
                    text: streamed.streamed_text,
                }) {
                    tracing::warn!(?err, "runtime receiver dropped while sending AssistantFinished");
                }
            }

            let mut emitted_tool_calls = 0usize;
            let mut saw_tool_request = false;
            let mut executed_tools = false;
            let tool_dispatch_span = tracing::info_span!(
                "tool_dispatch",
                moa.tool.count = tracing::field::Empty,
                moa.tool.parallel_count = 0i64,
            );
            let tool_dispatch_started = Instant::now();
            let tool_dispatch_outcome: Result<Option<StreamedTurnResult>> = async {
                for block in &response.content {
                    match block {
                        CompletionContent::ToolCall(call) => {
                            saw_tool_request = true;
                            let tool_dispatch_budget = *resource_budget;
                            *resource_budget = tool_dispatch_budget.try_consume_at(
                                ResourceAmounts {
                                    tool_calls: 1,
                                    ..ResourceAmounts::ZERO
                                },
                                Utc::now(),
                            )?;
                            let tool_leaf_budget = one_tool_call_budget(tool_dispatch_budget);
                            let outcome = handle_tool_call(
                                &identity,
                                session_id,
                                &session,
                                session_store.clone(),
                                tool_router.as_deref(),
                                call,
                                active_canary.as_deref(),
                                event_tx,
                                runtime_tx,
                                tool_leaf_budget,
                                cancel_token.as_ref(),
                                hard_cancel_token.as_ref(),
                                Some(&tool_dispatch_span),
                            )
                            .await?;
                            emitted_tool_calls += 1;
                            total_tool_calls += 1;
                            match outcome {
                                ToolCallOutcome::Executed => executed_tools = true,
                                ToolCallOutcome::Skipped(failure) => {
                                    if durable_failure.is_none() {
                                        durable_failure = failure;
                                    }
                                }
                                ToolCallOutcome::Cancelled => {
                                    record_turn_span_metrics(
                                        &turn_span,
                                        total_tool_calls,
                                        total_input_tokens,
                                        total_output_tokens,
                                        "cancelled",
                                    );
                                    return Ok(Some(StreamedTurnResult::Cancelled));
                                }
                            }
                            if signal_rx.is_some() {
                                drain_signal_queue(
                                    signal_rx.as_deref_mut(),
                                    runtime_tx,
                                    turn_requested,
                                    soft_cancel_requested,
                                )?;
                            }
                            if *soft_cancel_requested {
                                record_turn_span_metrics(
                                    &turn_span,
                                    total_tool_calls,
                                    total_input_tokens,
                                    total_output_tokens,
                                    "cancelled",
                                );
                                return Ok(Some(StreamedTurnResult::Cancelled));
                            }
                        }
                        CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => {}
                    }
                }
                Ok(None)
            }
            .instrument(tool_dispatch_span.clone())
            .await;
            tool_dispatch_span.record("moa.tool.count", emitted_tool_calls as i64);
            record_turn_tool_dispatch_duration(tool_dispatch_started.elapsed(), emitted_tool_calls);
            if let Some(result) = tool_dispatch_outcome? {
                return Ok(result);
            }

            let updated_session = session_store.get_session(session_id).await?;
            let _ = runtime_tx.send(RuntimeEvent::UsageUpdated {
                total_tokens: updated_session.total_input_tokens
                    + updated_session.total_output_tokens,
            });
            turn_span.record(
                "moa.session.cache_hit_rate",
                updated_session.cache_hit_rate(),
            );

            tracing::info!(
                session_id = %session_id,
                tool_calls = emitted_tool_calls,
                stop_reason = ?response.stop_reason,
                session_cache_hit_rate = %format!("{:.1}%", updated_session.cache_hit_rate() * 100.0),
                "streamed brain turn completed"
            );

            if *soft_cancel_requested {
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "cancelled",
                );
                return Ok(StreamedTurnResult::Cancelled);
            }

            if executed_tools || saw_tool_request || response.stop_reason == StopReason::ToolUse {
                if tool_router.is_some() {
                    continue;
                }
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "continue",
                );
                return Ok(StreamedTurnResult::Continue);
            }

            if response.stop_reason == StopReason::EndTurn {
                spawn_incident_capture(&session, turn_number, durable_failure.take());
                record_turn_span_metrics(
                    &turn_span,
                    total_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    "complete",
                );
                return Ok(StreamedTurnResult::Complete);
            }

            record_turn_span_metrics(
                &turn_span,
                total_tool_calls,
                total_input_tokens,
                total_output_tokens,
                "continue",
            );
            return Ok(StreamedTurnResult::Continue);
        }
    }
    .await
}

fn conservative_model_cost(
    pricing: &TokenPricing,
    estimated_input_tokens: usize,
    output_tokens: usize,
) -> u64 {
    let input_rates = [
        pricing.input_per_mtok,
        pricing.cache_write_per_mtok(),
        pricing
            .cached_input_per_mtok
            .unwrap_or(pricing.input_per_mtok),
    ];
    let output_rate = pricing.output_per_mtok;
    if input_rates
        .into_iter()
        .chain(std::iter::once(output_rate))
        .any(|rate| !rate.is_finite() || rate < 0.0)
    {
        return u64::MAX;
    }
    let input_rate = input_rates.into_iter().fold(0.0_f64, f64::max);
    let projected = estimated_input_tokens as f64 * input_rate + output_tokens as f64 * output_rate;
    if !projected.is_finite() || projected >= u64::MAX as f64 {
        u64::MAX
    } else {
        projected.ceil() as u64
    }
}

fn reported_model_usage(
    pricing: &TokenPricing,
    usage: &TokenUsage,
    response_has_output: bool,
) -> Result<ResourceAmounts> {
    let total_input_tokens = usage
        .input_tokens_uncached
        .checked_add(usage.input_tokens_cache_write)
        .and_then(|total| total.checked_add(usage.input_tokens_cache_read))
        .ok_or_else(|| {
            MoaError::BudgetExhausted(
                "provider token usage overflowed after model dispatch; refusing further work"
                    .to_string(),
            )
        })?;
    if total_input_tokens == 0 {
        return Err(MoaError::BudgetExhausted(
            "provider token usage omitted input tokens after model dispatch; refusing further work"
                .to_string(),
        ));
    }
    if response_has_output && usage.output_tokens == 0 {
        return Err(MoaError::BudgetExhausted(
            "provider token usage omitted output tokens for nonempty model output; refusing further work"
                .to_string(),
        ));
    }
    let total_tokens = total_input_tokens
        .checked_add(usage.output_tokens)
        .ok_or_else(|| {
            MoaError::BudgetExhausted(
                "provider token usage overflowed after model dispatch; refusing further work"
                    .to_string(),
            )
        })?;

    Ok(ResourceAmounts {
        cost_micro_usd: ResourceAmounts::cost_micro_usd_from_dollars(
            pricing.cost_dollars(usage),
        )?,
        tokens: u64::try_from(total_tokens).map_err(|_| {
            MoaError::BudgetExhausted(
                "provider token usage exceeded the resource counter after model dispatch; refusing further work"
                    .to_string(),
            )
        })?,
        ..ResourceAmounts::ZERO
    })
}

fn affordable_output_tokens(
    pricing: &TokenPricing,
    estimated_input_tokens: usize,
    remaining_cost_micro_usd: u64,
) -> usize {
    let input_cost = conservative_model_cost(pricing, estimated_input_tokens, 0);
    let Some(remaining_for_output) = remaining_cost_micro_usd.checked_sub(input_cost) else {
        return 0;
    };
    if pricing.output_per_mtok == 0.0 {
        return usize::MAX;
    }
    if !pricing.output_per_mtok.is_finite() || pricing.output_per_mtok < 0.0 {
        return 0;
    }

    let candidate = (remaining_for_output as f64 / pricing.output_per_mtok).floor();
    if !candidate.is_finite() || candidate >= usize::MAX as f64 {
        return usize::MAX;
    }
    let mut candidate = candidate as usize;
    while candidate > 0
        && conservative_model_cost(pricing, estimated_input_tokens, candidate)
            > remaining_cost_micro_usd
    {
        candidate -= 1;
    }
    candidate
}

fn one_tool_call_budget(budget: ResourceBudget) -> ResourceBudget {
    ResourceBudget::new(
        budget.deadline,
        budget.remaining.map(|mut remaining| {
            remaining.tool_calls = remaining.tool_calls.min(1);
            remaining
        }),
    )
}

/// Fire-and-forgets a negative-results incident write when a turn concludes on a
/// terminal tool failure.
///
/// Mirrors [`moa_retrieval::retrieval::bump_last_accessed`]'s background pattern: the
/// write runs off the turn's critical path and its result is logged at debug, so
/// a memory-storage hiccup never fails the turn. `record_incident` itself no-ops
/// when memory learning is disabled or the failure was already recorded.
fn spawn_incident_capture(session: &SessionMeta, turn_seq: i64, failure: Option<ToolFailure>) {
    let Some(failure) = failure else {
        return;
    };
    let session = session.clone();
    // Parent the detached write to the turn root so it stays in the turn's trace
    // instead of surfacing as an orphan root span in Tempo.
    let capture_span =
        moa_observability::current_turn_root_span().unwrap_or_else(tracing::Span::current);
    tokio::spawn(
        async move {
            match moa_memory_ingest::record_incident(
                &session,
                turn_seq,
                &failure.tool_name,
                failure.error_class,
            )
            .await
            {
                Ok(Some(uid)) => {
                    tracing::debug!(session_id = %session.id, %uid, "recorded turn incident");
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(session_id = %session.id, %error, "incident capture skipped");
                }
            }
        }
        .instrument(capture_span),
    );
}

async fn register_selected_skill_files(
    tool_router: Option<&ToolRouter>,
    session: &SessionMeta,
    ctx: &mut WorkingContext,
) {
    let Some(router) = tool_router else {
        return;
    };
    let files = ctx.take_trusted_sandbox_files();
    let file_count = files.len();
    router.set_trusted_sandbox_files(session, None, files).await;
    tracing::info!(
        session_id = %session.id,
        tenant_id = %session.tenant_id,
        file_count,
        "registered selected skill package files for lazy sandbox installation"
    );
}

/// Surfaces the read-only agentic memory tools onto this turn when the memory
/// stage gated them on (plan Task 11).
///
/// These tools are registered but excluded from the default prompt loadout, so
/// they are appended here only when the router selected the agentic strategy or
/// the injected retrieval returned nothing. This is deliberately a per-turn
/// mutation of the tool loadout: it changes the cached prompt prefix for those
/// turns only, which is the intended trade-off for the (rarer, costlier) agentic
/// path while the common fast-with-hits turn keeps a stable prefix. The pinned
/// agent tool policy is still honored so a tenant that denies a memory tool
/// never sees it offered.
fn augment_agentic_memory_tools(ctx: &mut WorkingContext, tool_router: Option<&ToolRouter>) {
    let offer = ctx
        .metadata()
        .get(crate::pipeline::memory::OFFER_RETRIEVAL_TOOLS_METADATA_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !offer {
        return;
    }
    let Some(router) = tool_router else {
        return;
    };

    let tool_policy = ctx
        .agent_context
        .as_ref()
        .and_then(|agent_context| agent_context.parsed_policy_snapshot().ok())
        .map(|snapshot| snapshot.tool_policy);

    let existing: std::collections::HashSet<String> = ctx
        .tools()
        .iter()
        .filter_map(|schema| schema.get("name").and_then(serde_json::Value::as_str))
        .map(ToString::to_string)
        .collect();

    let mut appended = Vec::new();
    for schema in router.agentic_memory_tool_schemas() {
        let Some(name) = schema.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if existing.contains(name) {
            continue;
        }
        let allowed = tool_policy
            .as_ref()
            .map(|policy| policy.allows(name))
            .unwrap_or(true);
        if !allowed {
            continue;
        }
        appended.push(name.to_string());
        ctx.tools_mut().push(schema);
    }

    if !appended.is_empty() {
        tracing::debug!(
            tools = ?appended,
            "surfaced agentic memory tools onto turn"
        );
    }
}

#[cfg(test)]
mod resource_tests {
    use moa_core::types::{
        model::TokenPricing,
        resource::{ResourceAmounts, ResourceBudget},
    };

    use super::{affordable_output_tokens, conservative_model_cost, one_tool_call_budget};

    fn pricing() -> TokenPricing {
        TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: None,
        }
    }

    #[test]
    fn cost_cap_accepts_exact_one_token_boundary_and_rejects_one_less() {
        // Pins: cost is projected in micro-USD before dispatch using the most
        // expensive configured input path. At the exact boundary one output
        // token is allowed; one micro-dollar less permits none.
        let pricing = pricing();
        let exact = conservative_model_cost(&pricing, 10, 1);

        assert_eq!(affordable_output_tokens(&pricing, 10, exact), 1);
        assert_eq!(affordable_output_tokens(&pricing, 10, exact - 1), 0);
    }

    #[test]
    fn tool_leaf_receives_one_call_while_parent_tracks_the_remainder() {
        // Pins: the router receives only the currently admitted tool call. The
        // caller owns decrementing the parent, so the router cannot spend the
        // rest of the case allowance or double-charge it.
        let parent = ResourceBudget::new(
            None,
            Some(ResourceAmounts {
                tool_calls: 7,
                ..ResourceAmounts::ZERO
            }),
        );
        let leaf = one_tool_call_budget(parent);

        assert_eq!(leaf.remaining.expect("bounded leaf").tool_calls, 1);
        assert_eq!(parent.remaining.expect("bounded parent").tool_calls, 7);
    }
}

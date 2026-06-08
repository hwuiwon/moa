//! Shared streamed-turn helpers used by the buffered harness and local orchestrator.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    ApprovalRequest, CompletionContent, CompletionRequest, CompletionResponse, Event, EventRecord,
    LLMProvider, Result, RuntimeEvent, SessionSignal, record_turn_llm_ttft,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use moa_core::events::tool_approval::{
    PendingToolApproval, StoredApprovalDecision, find_pending_tool_approval,
    find_resolved_pending_tool_approval,
};

/// Result of draining one streamed completion request.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamedCompletion {
    /// Final aggregated provider response when the stream reached completion.
    pub response: Option<CompletionResponse>,
    /// Aggregated streamed assistant text.
    pub streamed_text: String,
    /// Whether the stream was cancelled before the provider finished.
    pub cancelled: bool,
}

/// Control outcome returned by the streamed-signal callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSignalDisposition {
    /// Continue draining the provider response stream.
    Continue,
    /// Stop draining immediately and report the stream as cancelled.
    CancelImmediately,
}

/// Streams a provider response, optionally interleaving session signals.
pub async fn stream_completion_response<F, G>(
    llm_provider: Arc<dyn LLMProvider>,
    request: CompletionRequest,
    llm_call_span: Option<&tracing::Span>,
    cancel_token: Option<&CancellationToken>,
    mut signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    mut on_runtime_event: F,
    mut on_signal: G,
) -> Result<StreamedCompletion>
where
    F: FnMut(RuntimeEvent),
    G: FnMut(SessionSignal) -> StreamSignalDisposition,
{
    let started_at = Instant::now();
    let mut stream = llm_provider.complete(request).await?;
    let mut streamed_text = String::new();
    let mut started_assistant = false;
    let mut recorded_first_token = false;

    loop {
        if let Some(receiver) = signal_rx.as_deref_mut() {
            tokio::select! {
                block = stream.next() => {
                    let Some(block) = block else {
                        break;
                    };
                    if !recorded_first_token {
                        let ttft = started_at.elapsed();
                        record_turn_llm_ttft(ttft);
                        if let Some(span) = llm_call_span {
                            span.record("gen_ai.response.first_token_at_ms", ttft.as_millis() as i64);
                        }
                        recorded_first_token = true;
                    }
                    match block? {
                        CompletionContent::Text(delta) => {
                            if !started_assistant {
                                on_runtime_event(RuntimeEvent::AssistantStarted);
                                started_assistant = true;
                            }
                            streamed_text.push_str(&delta);
                            for ch in delta.chars() {
                                on_runtime_event(RuntimeEvent::AssistantDelta(ch));
                            }
                        }
                        CompletionContent::ToolCall(_) => {}
                        CompletionContent::ProviderToolResult { summary, .. } => {
                            on_runtime_event(RuntimeEvent::Notice(summary));
                        }
                    }
                }
                _ = async {
                    if let Some(cancel_token) = cancel_token {
                        cancel_token.cancelled().await;
                    }
                }, if cancel_token.is_some() => {
                    stream.abort();
                    return Ok(StreamedCompletion {
                        response: None,
                        streamed_text,
                        cancelled: true,
                    });
                }
                signal = receiver.recv() => {
                    let Some(signal) = signal else {
                        return Ok(StreamedCompletion {
                            response: None,
                            streamed_text,
                            cancelled: true,
                        });
                    };
                    if matches!(on_signal(signal), StreamSignalDisposition::CancelImmediately) {
                        return Ok(StreamedCompletion {
                            response: None,
                            streamed_text,
                            cancelled: true,
                        });
                    }
                }
            }
        } else {
            if let Some(cancel_token) = cancel_token {
                tokio::select! {
                    block = stream.next() => {
                        let Some(block) = block else {
                            break;
                        };
                        if !recorded_first_token {
                            let ttft = started_at.elapsed();
                            record_turn_llm_ttft(ttft);
                            if let Some(span) = llm_call_span {
                                span.record("gen_ai.response.first_token_at_ms", ttft.as_millis() as i64);
                            }
                            recorded_first_token = true;
                        }
                        match block? {
                            CompletionContent::Text(delta) => {
                                if !started_assistant {
                                    on_runtime_event(RuntimeEvent::AssistantStarted);
                                    started_assistant = true;
                                }
                                streamed_text.push_str(&delta);
                                for ch in delta.chars() {
                                    on_runtime_event(RuntimeEvent::AssistantDelta(ch));
                                }
                            }
                            CompletionContent::ToolCall(_) => {}
                            CompletionContent::ProviderToolResult { summary, .. } => {
                                on_runtime_event(RuntimeEvent::Notice(summary));
                            }
                        }
                    }
                    _ = cancel_token.cancelled() => {
                        stream.abort();
                        return Ok(StreamedCompletion {
                            response: None,
                            streamed_text,
                            cancelled: true,
                        });
                    }
                }
            } else {
                let Some(block) = stream.next().await else {
                    break;
                };
                if !recorded_first_token {
                    let ttft = started_at.elapsed();
                    record_turn_llm_ttft(ttft);
                    if let Some(span) = llm_call_span {
                        span.record("gen_ai.response.first_token_at_ms", ttft.as_millis() as i64);
                    }
                    recorded_first_token = true;
                }
                match block? {
                    CompletionContent::Text(delta) => {
                        if !started_assistant {
                            on_runtime_event(RuntimeEvent::AssistantStarted);
                            started_assistant = true;
                        }
                        streamed_text.push_str(&delta);
                        for ch in delta.chars() {
                            on_runtime_event(RuntimeEvent::AssistantDelta(ch));
                        }
                    }
                    CompletionContent::ToolCall(_) => {}
                    CompletionContent::ProviderToolResult { summary, .. } => {
                        on_runtime_event(RuntimeEvent::Notice(summary));
                    }
                }
            }
        }
    }

    Ok(StreamedCompletion {
        response: Some(stream.into_response().await?),
        streamed_text,
        cancelled: false,
    })
}

/// Returns the oldest unresolved approval request in the event log.
pub fn find_pending_approval_request(events: &[EventRecord]) -> Option<ApprovalRequest> {
    let mut requests = Vec::new();
    let mut decisions = HashSet::new();
    let mut completed = HashSet::new();

    for record in events {
        match &record.event {
            Event::ApprovalRequested {
                request_id,
                tool_name,
                input_summary,
                risk_level,
                ..
            } => {
                requests.push((
                    record.sequence_num,
                    ApprovalRequest {
                        request_id: *request_id,
                        sub_agent_id: None,
                        tool_name: tool_name.clone(),
                        input_summary: input_summary.clone(),
                        risk_level: risk_level.clone(),
                    },
                ));
            }
            Event::ApprovalDecided { request_id, .. } => {
                decisions.insert(*request_id);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(tool_id.0);
            }
            _ => {}
        }
    }

    requests.sort_by_key(|(sequence_num, _)| *sequence_num);
    requests.into_iter().find_map(|(_, request)| {
        (!decisions.contains(&request.request_id) && !completed.contains(&request.request_id))
            .then_some(request)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use moa_core::{
        ApprovalDecision, CompletionResponse, SessionId, StopReason, TokenUsage, ToolCallId,
    };
    use uuid::Uuid;

    use super::*;

    fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
        TokenUsage {
            input_tokens_uncached: input_tokens,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens,
        }
    }

    fn event_record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    struct ProviderToolResultLlm;

    #[async_trait::async_trait]
    impl LLMProvider for ProviderToolResultLlm {
        fn name(&self) -> &str {
            "provider-tool-result"
        }

        fn capabilities(&self) -> moa_core::ModelCapabilities {
            moa_core::ModelCapabilities {
                model_id: moa_core::ModelId::new("mock-model"),
                context_window: 32_000,
                max_output: 1_024,
                supports_tools: true,
                supports_vision: false,
                supports_prefix_caching: false,
                cache_ttl: None,
                tool_call_format: moa_core::ToolCallFormat::Anthropic,
                pricing: moa_core::TokenPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<moa_core::CompletionStream> {
            Ok(moa_core::CompletionStream::from_response(
                CompletionResponse {
                    text: "Fresh answer".to_string(),
                    content: vec![
                        CompletionContent::ProviderToolResult {
                            tool_name: "web_search".to_string(),
                            summary: "Searching the web...".to_string(),
                        },
                        CompletionContent::Text("Fresh answer".to_string()),
                    ],
                    stop_reason: StopReason::EndTurn,
                    model: moa_core::ModelId::new("mock-model"),
                    usage: token_usage(4, 2),
                    duration_ms: 1,
                    thought_signature: None,
                },
            ))
        }
    }

    #[tokio::test]
    async fn stream_completion_reports_provider_tool_results_as_notices() {
        let mut runtime_events = Vec::new();
        let streamed = stream_completion_response(
            Arc::new(ProviderToolResultLlm),
            CompletionRequest::simple("latest weather"),
            None,
            None,
            None,
            |event| runtime_events.push(event),
            |_| StreamSignalDisposition::Continue,
        )
        .await
        .unwrap();

        assert_eq!(streamed.streamed_text, "Fresh answer");
        assert!(runtime_events.contains(&RuntimeEvent::Notice("Searching the web...".to_string())));
        assert!(runtime_events.contains(&RuntimeEvent::AssistantStarted));
    }

    #[test]
    fn resolved_pending_tool_approval_preserves_provider_tool_use_id() {
        let tool_id = ToolCallId::new();
        let events = vec![
            event_record(
                0,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: Some("fc_pending_1".to_string()),
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "pwd" }),
                    hand_id: None,
                },
            ),
            event_record(
                1,
                Event::ApprovalRequested {
                    request_id: tool_id.0,
                    awakeable_id: None,
                    sub_agent_id: None,
                    tool_name: "bash".to_string(),
                    input_summary: "pwd".to_string(),
                    risk_level: moa_core::RiskLevel::Medium,
                    prompt: moa_core::ApprovalPrompt {
                        request: ApprovalRequest {
                            request_id: tool_id.0,
                            sub_agent_id: None,
                            tool_name: "bash".to_string(),
                            input_summary: "pwd".to_string(),
                            risk_level: moa_core::RiskLevel::Medium,
                        },
                        pattern: "bash:*".to_string(),
                        parameters: Vec::new(),
                        file_diffs: Vec::new(),
                    },
                },
            ),
            event_record(
                2,
                Event::ApprovalDecided {
                    request_id: tool_id.0,
                    sub_agent_id: None,
                    decision: ApprovalDecision::AllowOnce,
                    decided_by: "user".to_string(),
                    decided_at: Utc::now(),
                },
            ),
        ];

        let pending = find_resolved_pending_tool_approval(&events).expect("pending approval");
        assert_eq!(
            pending.provider_tool_use_id.as_deref(),
            Some("fc_pending_1")
        );
    }
}

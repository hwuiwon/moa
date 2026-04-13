//! Shared streamed-turn helpers used by the buffered harness and local orchestrator.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, ApprovalRequest, CompletionContent, CompletionRequest, CompletionResponse,
    Event, EventRecord, LLMProvider, Result, RuntimeEvent, SessionSignal,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// One previously requested tool call that is waiting on or has received approval.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolApproval {
    /// Tool call identifier.
    pub tool_id: Uuid,
    /// Provider-specific tool-use identifier, when available.
    pub provider_tool_use_id: Option<String>,
    /// Provider-specific thought signature that must be replayed with this tool call when present.
    pub provider_thought_signature: Option<String>,
    /// Tool name.
    pub tool_name: String,
    /// Original tool input payload.
    pub input: serde_json::Value,
    /// Stored approval decision, when one exists.
    pub decision: StoredApprovalDecision,
    /// Sequence number of the original `ToolCall` event.
    pub sequence_num: u64,
}

/// Approval decision reconstructed from persisted session events.
#[derive(Debug, Clone, PartialEq)]
pub enum StoredApprovalDecision {
    /// Allow the tool once.
    AllowOnce,
    /// Persist an allow rule and then execute the tool.
    AlwaysAllow {
        /// Persisted rule pattern.
        pattern: String,
        /// User that created the rule.
        decided_by: String,
    },
    /// Deny the tool execution.
    Deny {
        /// Optional human-readable denial reason.
        reason: Option<String>,
    },
}

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
    cancel_token: Option<&CancellationToken>,
    mut signal_rx: Option<&mut mpsc::Receiver<SessionSignal>>,
    mut on_runtime_event: F,
    mut on_signal: G,
) -> Result<StreamedCompletion>
where
    F: FnMut(RuntimeEvent),
    G: FnMut(SessionSignal) -> StreamSignalDisposition,
{
    let mut stream = llm_provider.complete(request).await?;
    let mut streamed_text = String::new();
    let mut started_assistant = false;

    loop {
        if let Some(receiver) = signal_rx.as_deref_mut() {
            tokio::select! {
                block = stream.next() => {
                    let Some(block) = block else {
                        break;
                    };
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
                completed.insert(*tool_id);
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

/// Returns the oldest requested tool call that is still waiting for a human decision.
pub fn find_pending_tool_approval(events: &[EventRecord]) -> Option<PendingToolApproval> {
    let mut tool_calls = HashMap::new();
    let mut decisions = HashSet::new();
    let mut completed = HashSet::new();
    let mut requested = HashSet::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                provider_tool_use_id,
                provider_thought_signature,
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
                        provider_tool_use_id: provider_tool_use_id.clone(),
                        provider_thought_signature: provider_thought_signature.clone(),
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        decision: StoredApprovalDecision::AllowOnce,
                        sequence_num: record.sequence_num,
                    },
                );
            }
            Event::ApprovalRequested { request_id, .. } => {
                requested.insert(*request_id);
            }
            Event::ApprovalDecided { request_id, .. } => {
                decisions.insert(*request_id);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter(|pending| {
            requested.contains(&pending.tool_id)
                && !decisions.contains(&pending.tool_id)
                && !completed.contains(&pending.tool_id)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);
    pending.into_iter().next()
}

/// Returns the oldest requested tool call that already has a persisted approval decision.
pub fn find_resolved_pending_tool_approval(events: &[EventRecord]) -> Option<PendingToolApproval> {
    let mut tool_calls = HashMap::new();
    let mut decisions = HashMap::new();
    let mut completed = HashSet::new();
    let mut requested = HashSet::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                provider_tool_use_id,
                provider_thought_signature,
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
                        provider_tool_use_id: provider_tool_use_id.clone(),
                        provider_thought_signature: provider_thought_signature.clone(),
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        decision: StoredApprovalDecision::AllowOnce,
                        sequence_num: record.sequence_num,
                    },
                );
            }
            Event::ApprovalRequested { request_id, .. } => {
                requested.insert(*request_id);
            }
            Event::ApprovalDecided {
                request_id,
                decision,
                decided_by,
                ..
            } => {
                let stored = match decision {
                    ApprovalDecision::AllowOnce => StoredApprovalDecision::AllowOnce,
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        StoredApprovalDecision::AlwaysAllow {
                            pattern: pattern.clone(),
                            decided_by: decided_by.clone(),
                        }
                    }
                    ApprovalDecision::Deny { reason } => StoredApprovalDecision::Deny {
                        reason: reason.clone(),
                    },
                };
                decisions.insert(*request_id, stored);
            }
            Event::ToolResult { tool_id, .. } | Event::ToolError { tool_id, .. } => {
                completed.insert(*tool_id);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter_map(|mut pending| {
            if completed.contains(&pending.tool_id) || !requested.contains(&pending.tool_id) {
                return None;
            }
            let decision = decisions.get(&pending.tool_id)?.clone();
            pending.decision = decision;
            Some(pending)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);
    pending.into_iter().next()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use moa_core::{CompletionResponse, SessionId, StopReason};
    use uuid::Uuid;

    use super::*;

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
                model_id: "mock-model".to_string(),
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
                    model: "mock-model".to_string(),
                    input_tokens: 4,
                    output_tokens: 2,
                    cached_input_tokens: 0,
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
        let tool_id = Uuid::now_v7();
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
                    request_id: tool_id,
                    tool_name: "bash".to_string(),
                    input_summary: "pwd".to_string(),
                    risk_level: moa_core::RiskLevel::Medium,
                    prompt: moa_core::ApprovalPrompt {
                        request: ApprovalRequest {
                            request_id: tool_id,
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
                    request_id: tool_id,
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

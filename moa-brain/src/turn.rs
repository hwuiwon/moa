//! Shared streamed-turn helpers used by the buffered harness and local orchestrator.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use moa_core::{
    ApprovalDecision, ApprovalRequest, CompletionContent, CompletionRequest, CompletionResponse,
    Event, EventRecord, LLMProvider, Result, RuntimeEvent, SessionSignal,
};
use tokio::sync::mpsc;
use uuid::Uuid;

/// One previously requested tool call that is waiting on or has received approval.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolApproval {
    /// Tool call identifier.
    pub tool_id: Uuid,
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
                    }
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
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
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
                tool_name,
                input,
                ..
            } => {
                tool_calls.insert(
                    *tool_id,
                    PendingToolApproval {
                        tool_id: *tool_id,
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

//! Helpers for reconstructing pending tool approval state from session events.

use std::collections::{HashMap, HashSet};

use crate::{ApprovalDecision, Event, EventRecord, ToolCallId};

/// One previously requested tool call that is waiting on or has received approval.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingToolApproval {
    /// Tool call identifier.
    pub tool_id: ToolCallId,
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
                completed.insert(tool_id.0);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter(|pending| {
            requested.contains(&pending.tool_id.0)
                && !decisions.contains(&pending.tool_id.0)
                && !completed.contains(&pending.tool_id.0)
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
                completed.insert(tool_id.0);
            }
            _ => {}
        }
    }

    let mut pending = tool_calls
        .into_values()
        .filter_map(|mut pending| {
            if completed.contains(&pending.tool_id.0) || !requested.contains(&pending.tool_id.0) {
                return None;
            }
            let decision = decisions.get(&pending.tool_id.0)?.clone();
            pending.decision = decision;
            Some(pending)
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|item| item.sequence_num);
    pending.into_iter().next()
}

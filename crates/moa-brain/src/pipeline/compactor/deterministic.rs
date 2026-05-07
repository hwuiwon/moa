//! Deterministic tier 1 and tier 2 message compaction.

use std::collections::{HashMap, HashSet};

use moa_core::{ContextMessage, ToolContent};

use super::triggers::recent_turn_boundary_messages;

const TOOL_RESULT_ELIDED_PLACEHOLDER: &str = "[tool result elided by compaction]";
const DUPLICATE_BASH_PLACEHOLDER: &str = "[duplicate bash output elided by compaction]";
pub(super) const CACHE_COMPACTION_PLACEHOLDER: &str =
    "[earlier history elided for cache compaction — see session log for full history]";
const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";
const FILE_READ_RANGE_HEADER_PREFIX: &str = "[showing lines ";

pub(super) fn apply_tier1(
    messages: &mut [ContextMessage],
    recent_turns_verbatim: usize,
    protected_tool_use_ids: &HashSet<String>,
) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let recent_boundary = recent_turn_boundary_messages(messages, recent_turns_verbatim);
    let tool_names = tool_names_by_use_id(messages);
    let referenced_ids = referenced_tool_use_ids(messages);
    let mut seen_bash_outputs = HashSet::new();
    let mut elided = 0usize;

    for message in messages.iter_mut().take(recent_boundary) {
        let Some(tool_use_id) = message.tool_use_id.clone() else {
            continue;
        };
        if is_compacted_tool_result(message) {
            continue;
        }
        if is_file_read_result_message(message) {
            continue;
        }
        if protected_tool_use_ids.contains(&tool_use_id) {
            continue;
        }
        if referenced_ids.contains(&tool_use_id) {
            continue;
        }

        let is_bash = tool_names
            .get(&tool_use_id)
            .map(|tool_name| tool_name == "bash")
            .unwrap_or(false);
        if is_bash && !seen_bash_outputs.insert(message.content.clone()) {
            *message = compacted_tool_result(message, DUPLICATE_BASH_PLACEHOLDER);
            elided += 1;
            continue;
        }

        *message = compacted_tool_result(message, TOOL_RESULT_ELIDED_PLACEHOLDER);
        elided += 1;
    }

    elided
}

pub(super) fn apply_tier2(messages: &mut Vec<ContextMessage>) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let dropped = messages.len();
    messages.clear();
    messages.push(ContextMessage::system(CACHE_COMPACTION_PLACEHOLDER));
    dropped
}

fn is_compacted_tool_result(message: &ContextMessage) -> bool {
    message.content.contains(TOOL_RESULT_ELIDED_PLACEHOLDER)
        || message.content.contains(DUPLICATE_BASH_PLACEHOLDER)
        || message.content.contains(CACHE_COMPACTION_PLACEHOLDER)
}

fn is_file_read_result_message(message: &ContextMessage) -> bool {
    message.content.contains(FILE_READ_DEDUP_PLACEHOLDER)
        || message.content.contains(FILE_READ_RANGE_HEADER_PREFIX)
}

fn tool_names_by_use_id(messages: &[ContextMessage]) -> HashMap<String, String> {
    messages
        .iter()
        .filter_map(|message| {
            let invocation = message.tool_invocation.as_ref()?;
            let id = invocation.id.as_ref()?;
            Some((id.clone(), invocation.name.clone()))
        })
        .collect()
}

fn referenced_tool_use_ids(messages: &[ContextMessage]) -> HashSet<String> {
    let candidate_ids = messages
        .iter()
        .filter_map(|message| message.tool_use_id.clone())
        .collect::<Vec<_>>();

    let mut referenced = HashSet::new();
    for (index, tool_use_id) in candidate_ids.iter().enumerate() {
        if messages.iter().skip(index + 1).any(|message| {
            message.content.contains(tool_use_id)
                || message
                    .tool_invocation
                    .as_ref()
                    .and_then(|invocation| invocation.id.as_ref())
                    .map(|id| id == tool_use_id)
                    .unwrap_or(false)
        }) {
            referenced.insert(tool_use_id.clone());
        }
    }

    referenced
}

fn compacted_tool_result(message: &ContextMessage, placeholder: &str) -> ContextMessage {
    ContextMessage::tool_result(
        message
            .tool_use_id
            .clone()
            .unwrap_or_else(|| "compacted".to_string()),
        placeholder,
        Some(vec![ToolContent::Text {
            text: placeholder.to_string(),
        }]),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use moa_core::ContextMessage;
    use serde_json::json;

    use super::{apply_tier1, apply_tier2};

    #[test]
    fn tier1_is_idempotent_for_compacted_messages() {
        let mut messages = vec![
            ContextMessage::assistant_tool_call(
                moa_core::ToolInvocation {
                    id: Some("toolu_1".to_string()),
                    name: "bash".to_string(),
                    input: json!({"cmd": "pwd"}),
                },
                "call",
            ),
            ContextMessage::tool_result("toolu_1", "output", None),
            ContextMessage::user("latest"),
        ];

        let once = apply_tier1(&mut messages, 1, &HashSet::new());
        let snapshot = messages.clone();
        let twice = apply_tier1(&mut messages, 1, &HashSet::new());

        assert_eq!(once, 1);
        assert_eq!(twice, 0);
        assert_eq!(messages, snapshot);
    }

    #[test]
    fn tier2_replaces_old_history_with_placeholder() {
        let mut messages = vec![
            ContextMessage::user("turn 1"),
            ContextMessage::assistant("answer 1"),
            ContextMessage::user("turn 2"),
            ContextMessage::assistant("answer 2"),
        ];

        let dropped = apply_tier2(&mut messages);

        assert_eq!(dropped, 4);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("cache compaction"));
    }
}

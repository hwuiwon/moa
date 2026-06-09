//! Prompt construction for the query rewriter model call.

use moa_core::{ContextMessage, MessageRole, WorkingContext};
use serde_json::Value;

use super::input::RewriteInput;

const MAX_PROMPT_MESSAGE_CHARS: usize = 1_000;

pub(super) fn build_rewriter_prompt(input: &RewriteInput, ctx: &WorkingContext) -> String {
    let history = format_history(&input.history);
    let tools = available_tool_names(ctx).join(", ");
    let skills = available_skill_lines(ctx).join("\n");

    format!(
        "You are a query rewriter for an AI agent system. Rewrite the user's query\n\
         into a self-contained, unambiguous request. Resolve pronouns and references\n\
         using the conversation history.\n\n\
         Rules:\n\
         - Do NOT invent information not present in the conversation history\n\
         - Do NOT add entities, file paths, or technical details not mentioned\n\
         - DO resolve \"that\", \"it\", \"the bug\", etc. to their concrete referents\n\
         - DO decompose compound requests into sub_queries\n\
         - Determine if this message starts a NEW task or continues the current one\n\
         - A new task means the user is asking about something unrelated to the current work\n\
         - Set is_new_task=true only when the topic genuinely shifts, not for follow-up questions\n\
         - Treat coreferences like \"that file\", \"the error above\", and \"try again\" as continuations\n\
         - Produce retrieval and segment-boundary metadata only; do not decide the agent's final actions\n\
         - Set freshness_required=true when answering requires current, external, or time-sensitive information\n\
         - Set repo_context_required=true when the agent should inspect repository files, code, config, logs, or tests before answering\n\
         - Set memory_action to one of: none, retrieve, remember, forget, supersede, ingest\n\
         - Use memory_action=retrieve when existing workspace or user memory is likely needed before answering\n\
         - Use needs_clarification only when the query cannot be interpreted even with history\n\
         - Treat suggested_tools, tool_bias, and suggested_promptlets as best-effort advisory hints, not routing decisions\n\
         - Prefer empty hint arrays when uncertain; the main agent model chooses tools and actions from context\n\
         - Respond ONLY with valid JSON matching the schema below. No preamble.\n\n\
         Schema: {{\"rewritten_query\": string, \"task_kind\": string, \"sub_queries\": [string],\n\
         \"suggested_tools\": [string], \"freshness_required\": bool,\n\
         \"repo_context_required\": bool, \"memory_action\": string,\n\
         \"needs_clarification\": bool,\n\
         \"clarification_question\": string|null, \"is_new_task\": bool,\n\
         \"task_summary\": string|null, \"tool_bias\": [string],\n\
         \"suggested_promptlets\": [string]}}\n\
         task_kind must be one of: coding, research, file_operation, system_admin,\n\
         creative, question, conversation, unknown.\n\n\
         Available tools: {tools}\n\n\
         Available skills:\n{skills}\n\n\
         Conversation history (last 5 turns):\n{history}\n\n\
         Current query:\n{}",
        input.query
    )
}

pub(super) fn available_tool_names(ctx: &WorkingContext) -> Vec<String> {
    ctx.tools()
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn available_skill_lines(ctx: &WorkingContext) -> Vec<String> {
    ctx.messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .flat_map(|message| message.content.lines())
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .take(50)
        .map(ToOwned::to_owned)
        .collect()
}

fn format_history(history: &[ContextMessage]) -> String {
    if history.is_empty() {
        return "(none)".to_string();
    }

    history
        .iter()
        .map(|message| {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };
            format!(
                "{role}: {}",
                truncate_for_prompt(message.content.trim(), MAX_PROMPT_MESSAGE_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_for_prompt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

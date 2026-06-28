//! Prompt construction for the query rewriter model call.

use moa_core::{ContextMessage, MessageRole, WorkingContext};
use serde_json::Value;

use super::input::RewriteInput;

const MAX_PROMPT_MESSAGE_CHARS: usize = 1_000;

pub(super) const REWRITER_SYSTEM_PROMPT: &str = "\
You are a query rewriter for an AI agent system. Rewrite the user's query into a \
self-contained retrieval query for graph memory search. Resolve pronouns and references using \
the conversation history.

Rules:
- Do NOT invent information not present in the conversation history
- Do NOT add entities, file paths, or technical details not mentioned
- DO resolve \"that\", \"it\", \"the bug\", etc. to their concrete referents
- Preserve exact identifiers, file paths, URLs, UUIDs, issue IDs, and quoted strings
- Determine if this message starts a NEW task or continues the current one
- A new task means the user is asking about something unrelated to the current work
- Set is_new_task=true only when the topic genuinely shifts, not for follow-up questions
- Treat coreferences like \"that file\", \"the error above\", and \"try again\" as continuations
- Produce retrieval and segment-boundary metadata only
- Do not classify intent, choose tools, request clarification, or add prompt advice
- Respond ONLY with valid JSON matching the schema below. No preamble.

Schema: {\"retrieval_query\": string, \"is_new_task\": bool, \"task_summary\": string|null, \"task_facets\": object|null}
task_facets is optional learning metadata with string-or-null fields: domain, action, \
artifact_kind, language_or_framework, verification_style, risk_class, plus arrays tool_pattern \
and skill_pattern. Use null when unsure.";

pub(super) fn build_rewriter_user_prompt(input: &RewriteInput, ctx: &WorkingContext) -> String {
    let history = format_history(&input.history);
    let tools = available_tool_names(ctx).join(", ");
    let skills = available_skill_lines(ctx).join("\n");

    format!(
        "Available tools: {tools}\n\n\
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

#[cfg(test)]
mod tests {
    use moa_core::{ContextMessage, ModelCapabilities, SessionMeta, WorkingContext};
    use serde_json::json;

    use super::*;

    #[test]
    fn rewriter_prompt_keeps_dynamic_turn_data_out_of_system_prompt() {
        // Pins: query rewriter cache prefix is stable while tools, skills, history, and query vary.
        let session = SessionMeta::default();
        let mut ctx = WorkingContext::new(&session, ModelCapabilities::default());
        ctx.set_tools(vec![json!({"name": "bash"})]);
        ctx.append_message(ContextMessage::system(
            "- skill-a: inspect auth failures".to_string(),
        ));
        let input = RewriteInput {
            query: "fix that".to_string(),
            history: vec![ContextMessage::user(
                "The OAuth refresh token race is in auth/refresh.rs",
            )],
            user_message_count: 2,
        };

        let user_prompt = build_rewriter_user_prompt(&input, &ctx);
        let stable_system_prompt = REWRITER_SYSTEM_PROMPT;

        let mut changed_ctx = WorkingContext::new(&session, ModelCapabilities::default());
        changed_ctx.set_tools(vec![json!({"name": "sql_query"})]);
        changed_ctx.append_message(ContextMessage::system(
            "- skill-b: inspect billing incidents".to_string(),
        ));
        let changed_input = RewriteInput {
            query: "summarize that".to_string(),
            history: vec![ContextMessage::user(
                "The invoice sync incident is in billing/sync.rs",
            )],
            user_message_count: 2,
        };
        let changed_user_prompt = build_rewriter_user_prompt(&changed_input, &changed_ctx);

        assert!(stable_system_prompt.starts_with("You are a query rewriter"));
        assert_eq!(stable_system_prompt, REWRITER_SYSTEM_PROMPT);
        assert!(!stable_system_prompt.contains("auth/refresh.rs"));
        assert!(!stable_system_prompt.contains("billing/sync.rs"));
        assert!(!stable_system_prompt.contains("Available tools"));
        assert!(user_prompt.contains("Available tools: bash"));
        assert!(user_prompt.contains("- skill-a: inspect auth failures"));
        assert!(user_prompt.contains("auth/refresh.rs"));
        assert!(user_prompt.contains("Current query:\nfix that"));
        assert!(changed_user_prompt.contains("Available tools: sql_query"));
        assert!(changed_user_prompt.contains("- skill-b: inspect billing incidents"));
        assert!(changed_user_prompt.contains("billing/sync.rs"));
        assert!(changed_user_prompt.contains("Current query:\nsummarize that"));
        assert!(!changed_user_prompt.contains("auth/refresh.rs"));
    }
}

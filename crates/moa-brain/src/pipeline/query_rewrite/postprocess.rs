//! Validation and metadata storage for query rewrite results.

use std::collections::HashSet;

use moa_core::{
    MemoryAction, MoaError, QueryRewriteResult, Result, RewriteSource, TaskKind, WorkingContext,
};
use serde::Deserialize;
use serde_json::Value;

use super::METADATA_KEY;
use super::input::RewriteInput;
use super::prompt::{available_skill_lines, available_tool_names};
use super::terms::{
    entity_terms, is_entity_like, is_rewrite_function_word, normalize_entity_token,
};

pub(super) fn store_rewrite_result(
    ctx: &mut WorkingContext,
    result: QueryRewriteResult,
) -> Result<()> {
    ctx.insert_metadata(METADATA_KEY, serde_json::to_value(result)?);
    Ok(())
}

pub(super) fn parse_rewrite_response(text: &str) -> Result<QueryRewriteResult> {
    let result = serde_json::from_str::<RawQueryRewriteResult>(text.trim())?.into_result();
    if result.rewritten_query.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "query rewriter returned an empty rewritten_query".to_string(),
        ));
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
struct RawQueryRewriteResult {
    rewritten_query: String,
    #[serde(default)]
    task_kind: Option<Value>,
    sub_queries: Vec<String>,
    suggested_tools: Vec<String>,
    #[serde(default)]
    freshness_required: bool,
    #[serde(default)]
    repo_context_required: bool,
    #[serde(default)]
    memory_action: Option<Value>,
    needs_clarification: bool,
    clarification_question: Option<String>,
    #[serde(default)]
    is_new_task: bool,
    #[serde(default)]
    task_summary: Option<String>,
    #[serde(default)]
    tool_bias: Vec<String>,
    #[serde(default)]
    suggested_promptlets: Vec<String>,
}

impl RawQueryRewriteResult {
    fn into_result(self) -> QueryRewriteResult {
        QueryRewriteResult {
            rewritten_query: self.rewritten_query,
            task_kind: parse_task_kind(self.task_kind),
            sub_queries: self.sub_queries,
            suggested_tools: self.suggested_tools,
            freshness_required: self.freshness_required,
            repo_context_required: self.repo_context_required,
            memory_action: parse_memory_action(self.memory_action),
            needs_clarification: self.needs_clarification,
            clarification_question: self.clarification_question,
            is_new_task: self.is_new_task,
            task_summary: self.task_summary,
            tool_bias: clean_hint_list(self.tool_bias),
            suggested_promptlets: clean_hint_list(self.suggested_promptlets),
            source: RewriteSource::Rewritten,
        }
    }
}

fn parse_task_kind(value: Option<Value>) -> TaskKind {
    value
        .and_then(|value| serde_json::from_value::<TaskKind>(value).ok())
        .unwrap_or(TaskKind::Unknown)
}

fn parse_memory_action(value: Option<Value>) -> MemoryAction {
    value
        .and_then(|value| serde_json::from_value::<MemoryAction>(value).ok())
        .unwrap_or(MemoryAction::None)
}

pub(super) fn validate_rewrite_result(
    mut result: QueryRewriteResult,
    input: &RewriteInput,
    ctx: &WorkingContext,
) -> QueryRewriteResult {
    let allowed_terms = allowed_terms(input, ctx);
    result.rewritten_query =
        strip_unsupported_entity_tokens(&result.rewritten_query, &allowed_terms);
    result.sub_queries = result
        .sub_queries
        .into_iter()
        .map(|query| strip_unsupported_entity_tokens(&query, &allowed_terms))
        .filter(|query| !query.trim().is_empty())
        .collect();
    result.clarification_question = result
        .clarification_question
        .map(|question| strip_unsupported_entity_tokens(&question, &allowed_terms))
        .filter(|question| !question.trim().is_empty());
    result.task_summary = result
        .task_summary
        .map(|summary| strip_unsupported_entity_tokens(&summary, &allowed_terms))
        .filter(|summary| !summary.trim().is_empty());
    result.suggested_tools = filter_suggested_tools(result.suggested_tools, ctx);
    result.tool_bias = clean_hint_list(result.tool_bias);
    result.suggested_promptlets = clean_hint_list(result.suggested_promptlets);
    result.source = RewriteSource::Rewritten;

    if result.rewritten_query.trim().is_empty() {
        QueryRewriteResult::passthrough(input.query.clone())
    } else {
        result
    }
}

fn clean_hint_list(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .take(16)
        .collect()
}

fn allowed_terms(input: &RewriteInput, ctx: &WorkingContext) -> HashSet<String> {
    let mut terms = HashSet::new();
    for message in &input.history {
        terms.extend(entity_terms(&message.content));
    }
    terms.extend(entity_terms(&input.query));
    for tool in available_tool_names(ctx) {
        terms.extend(entity_terms(&tool));
    }
    for line in available_skill_lines(ctx) {
        terms.extend(entity_terms(&line));
    }
    terms
}

fn filter_suggested_tools(suggested_tools: Vec<String>, ctx: &WorkingContext) -> Vec<String> {
    let available = available_tool_names(ctx)
        .into_iter()
        .collect::<HashSet<_>>();
    if available.is_empty() {
        return suggested_tools;
    }

    suggested_tools
        .into_iter()
        .filter(|tool| available.contains(tool))
        .collect()
}

fn strip_unsupported_entity_tokens(text: &str, allowed_terms: &HashSet<String>) -> String {
    let mut sanitized = Vec::new();
    for raw in text.split_whitespace() {
        let term = normalize_entity_token(raw);
        if term.is_empty()
            || !is_entity_like(&term)
            || is_rewrite_function_word(&term)
            || allowed_terms.contains(&term)
        {
            sanitized.push(raw);
        }
    }

    cleanup_stripped_text(&sanitized.join(" "))
}

fn cleanup_stripped_text(text: &str) -> String {
    let mut words = text
        .split_whitespace()
        .map(|word| word.trim_matches([',', ';', ':']))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    while words.last().is_some_and(|word| {
        matches!(
            word.to_ascii_lowercase().as_str(),
            "and" | "or" | "in" | "for" | "with" | "to"
        )
    }) {
        words.pop();
    }
    words.join(" ")
}

//! Validation and metadata storage for query rewrite results.

use std::collections::HashSet;

use moa_core::{
    error::MoaError, error::Result, types::context::WorkingContext,
    types::experience::TaskFacetSet, types::query_rewrite::QueryRewriteResult,
    types::query_rewrite::RewriteReason, types::query_rewrite::RewriteSource,
};
use serde::Deserialize;

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

pub(super) fn parse_rewrite_response(
    text: &str,
    reason: RewriteReason,
) -> Result<QueryRewriteResult> {
    let result = serde_json::from_str::<RawQueryRewriteResult>(text.trim())?.into_result(reason);
    if result.retrieval_query.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "query rewriter returned an empty retrieval_query".to_string(),
        ));
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
struct RawQueryRewriteResult {
    retrieval_query: String,
    #[serde(default)]
    is_new_task: bool,
    #[serde(default)]
    task_summary: Option<String>,
    #[serde(default)]
    task_facets: Option<TaskFacetSet>,
}

impl RawQueryRewriteResult {
    fn into_result(self, reason: RewriteReason) -> QueryRewriteResult {
        QueryRewriteResult {
            retrieval_query: self.retrieval_query,
            source: RewriteSource::Rewritten,
            reason: Some(reason),
            is_new_task: self.is_new_task,
            // The rewrite LLM ran and returned a parseable result, so `is_new_task`
            // is an authoritative task-boundary judgment.
            has_boundary_signal: true,
            task_summary: self.task_summary,
            task_facets: self.task_facets,
        }
    }
}

pub(super) fn validate_rewrite_result(
    mut result: QueryRewriteResult,
    input: &RewriteInput,
    ctx: &WorkingContext,
    reason: RewriteReason,
) -> QueryRewriteResult {
    let allowed_terms = allowed_terms(input, ctx);
    result.retrieval_query =
        strip_unsupported_entity_tokens(&result.retrieval_query, &allowed_terms);
    result.task_summary = result
        .task_summary
        .map(|summary| strip_unsupported_entity_tokens(&summary, &allowed_terms))
        .filter(|summary| !summary.trim().is_empty());
    result.task_facets = result.task_facets.map(normalize_task_facets);
    result.source = RewriteSource::Rewritten;
    result.reason = Some(reason);

    if result.retrieval_query.trim().is_empty() {
        QueryRewriteResult::original(input.query.clone())
    } else {
        result
    }
}

fn normalize_task_facets(mut facets: TaskFacetSet) -> TaskFacetSet {
    facets.domain = normalize_optional_facet(facets.domain);
    facets.action = normalize_optional_facet(facets.action);
    facets.artifact_kind = normalize_optional_facet(facets.artifact_kind);
    facets.language_or_framework = normalize_optional_facet(facets.language_or_framework);
    facets.verification_style = normalize_optional_facet(facets.verification_style);
    facets.risk_class = normalize_optional_facet(facets.risk_class);
    facets.tool_pattern = normalize_facet_list(facets.tool_pattern);
    facets.skill_pattern = normalize_facet_list(facets.skill_pattern);
    facets
}

fn normalize_optional_facet(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn normalize_facet_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
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

//! Skill distillation from successful agent runs.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    CompletionRequest, Event, EventRecord, LLMProvider, MemoryStore, Result, SessionMeta,
    SkillMetadata,
};
use moa_memory::FileMemoryStore;

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
    wiki_page_from_skill,
};
use crate::improver::{format_events_for_learning, normalize_llm_markdown, record_successful_use};
use crate::registry::SkillRegistry;

const MIN_TOOL_CALLS_FOR_DISTILLATION: usize = 5;
const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Distills a successful multi-step session into a reusable workspace skill when appropriate.
pub async fn maybe_distill_skill(
    session: &SessionMeta,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
    llm: Arc<dyn LLMProvider>,
) -> Result<Option<SkillMetadata>> {
    if count_tool_calls(events) < MIN_TOOL_CALLS_FOR_DISTILLATION {
        return Ok(None);
    }

    let task_summary = extract_task_summary(events);
    let registry_memory: Arc<dyn MemoryStore> = memory_store.clone();
    let registry = SkillRegistry::new(registry_memory);
    let existing_skills = registry.list_for_pipeline(&session.workspace_id).await?;

    if let Some(existing) = find_similar_skill(&task_summary, &existing_skills) {
        return crate::improver::maybe_improve_skill(session, existing, events, memory_store, llm)
            .await;
    }

    let prompt = build_distillation_prompt(&task_summary, events);
    let response = llm
        .complete(CompletionRequest::simple(prompt))
        .await?
        .collect()
        .await?;
    let skill_markdown = normalize_llm_markdown(&response.text);
    let mut skill = parse_skill_markdown(skill_markdown)?;
    normalize_new_skill(session, &mut skill);
    let path = build_skill_path(&skill.frontmatter.name);
    let page = wiki_page_from_skill(&skill, Some(path.clone()))?;
    memory_store
        .write_page_in_scope(
            &moa_core::MemoryScope::Workspace(session.workspace_id.clone()),
            &path,
            page,
        )
        .await?;

    Ok(Some(skill_metadata_from_document(path, &skill)))
}

fn count_tool_calls(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::ToolCall { .. }))
        .count()
}

fn extract_task_summary(events: &[EventRecord]) -> String {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(text.trim().to_string())
            }
            _ => None,
        })
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| "distilled session workflow".to_string())
}

fn build_distillation_prompt(task_summary: &str, events: &[EventRecord]) -> String {
    format!(
        "Distill the following successful MOA session into a reusable Agent Skill.\n\
         Output only a complete SKILL.md document using the Agent Skills format from agentskills.io.\n\
         Use spec-compatible top-level frontmatter fields such as `name`, `description`, optional \
         `compatibility`, optional `allowed-tools`, and a `metadata` map for MOA-specific bookkeeping.\n\
         Store MOA-specific fields inside `metadata` using `moa-` prefixes, including at least \
         `moa-version`, `moa-one-liner`, `moa-tags`, and `moa-estimated-tokens`.\n\
         The skill should include when-to-use guidance, a numbered procedure, pitfalls, and verification steps.\n\
         Task summary: {task_summary}\n\n\
         Session events:\n{}",
        format_events_for_learning(events)
    )
}

fn find_similar_skill<'a>(
    task_summary: &str,
    skills: &'a [SkillMetadata],
) -> Option<&'a SkillMetadata> {
    let summary_tokens = tokenize(task_summary);
    skills
        .iter()
        .map(|skill| (similarity_score(&summary_tokens, skill), skill))
        .filter(|(score, _)| *score >= SIMILARITY_THRESHOLD)
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, skill)| skill)
}

fn similarity_score(summary_tokens: &HashSet<String>, skill: &SkillMetadata) -> f32 {
    let mut skill_tokens = tokenize(&skill.name);
    skill_tokens.extend(tokenize(&skill.description));
    for tag in &skill.tags {
        skill_tokens.extend(tokenize(tag));
    }
    for tool in &skill.allowed_tools {
        skill_tokens.extend(tokenize(tool));
    }

    let intersection = summary_tokens.intersection(&skill_tokens).count() as f32;
    let union = summary_tokens.union(&skill_tokens).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn tokenize(text: &str) -> HashSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn normalize_new_skill(session: &SessionMeta, skill: &mut SkillDocument) {
    let now = Utc::now();
    skill.frontmatter.set_auto_generated(true);
    skill
        .frontmatter
        .set_source_session(Some(session.id.to_string()));
    skill.frontmatter.set_updated(now);
    record_successful_use(skill, now);
    if skill.frontmatter.use_count() == 0 {
        skill.frontmatter.set_use_count(1);
    }
}

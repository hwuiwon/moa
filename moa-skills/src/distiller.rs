//! Skill distillation from successful agent runs.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    CompletionRequest, Event, EventRecord, MemoryStore, MoaConfig, ModelTask, Result, SessionMeta,
    SkillMetadata,
};
use moa_memory::FileMemoryStore;
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
    wiki_page_from_skill,
};
use crate::improver::{format_events_for_learning, normalize_llm_markdown, record_successful_use};
use crate::registry::SkillRegistry;
use crate::regression::generate_skill_test_suite;

const MIN_TOOL_CALLS_FOR_DISTILLATION: usize = 5;
const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Distills a successful multi-step session into a reusable workspace skill when appropriate.
pub async fn maybe_distill_skill(
    config: &MoaConfig,
    session: &SessionMeta,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
    model_router: Arc<ModelRouter>,
) -> Result<Option<SkillMetadata>> {
    maybe_distill_skill_with_learning(config, session, events, memory_store, model_router, None)
        .await
}

/// Distills a successful session and records learning-log entries when a store is provided.
pub async fn maybe_distill_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<Option<SkillMetadata>> {
    if count_tool_calls(events) < MIN_TOOL_CALLS_FOR_DISTILLATION {
        return Ok(None);
    }

    let task_summary = extract_task_summary(events);
    let registry_memory: Arc<dyn MemoryStore> = memory_store.clone();
    let registry = SkillRegistry::new(registry_memory);
    let existing_skills = registry.list_for_pipeline(&session.workspace_id).await?;

    if let Some(existing) = find_similar_skill(&task_summary, &existing_skills) {
        return crate::improver::maybe_improve_skill_with_learning(
            config,
            session,
            existing,
            events,
            memory_store,
            model_router,
            learning_store,
        )
        .await;
    }

    let llm = model_router.provider_for(ModelTask::SkillDistillation);
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
    let scope = moa_core::MemoryScope::Workspace(session.workspace_id.clone());
    memory_store.write_page(&scope, &path, page).await?;
    generate_skill_test_suite(session, &skill, &path, events, memory_store.clone()).await?;

    let metadata = skill_metadata_from_document(path, &skill);
    if let Some(store) = learning_store {
        append_skill_learning(
            store.as_ref(),
            session,
            "skill_created",
            &metadata,
            serde_json::json!({
                "path": metadata.path.clone(),
                "name": metadata.name.clone(),
                "description": metadata.description.clone(),
            }),
        )
        .await?;
    }

    Ok(Some(metadata))
}

pub(crate) async fn append_skill_learning(
    store: &PostgresSessionStore,
    session: &SessionMeta,
    learning_type: &str,
    skill: &SkillMetadata,
    payload: serde_json::Value,
) -> Result<()> {
    store
        .append_learning(&moa_core::LearningEntry {
            id: uuid::Uuid::now_v7(),
            tenant_id: session.workspace_id.to_string(),
            learning_type: learning_type.to_string(),
            target_id: skill.path.to_string(),
            target_label: Some(skill.name.clone()),
            payload,
            confidence: Some(1.0),
            source_refs: vec![session.id.0],
            actor: format!("brain:{}", session.id),
            valid_from: Utc::now(),
            valid_to: None,
            batch_id: None,
            version: 1,
        })
        .await
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
        .map(str::to_ascii_lowercase)
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

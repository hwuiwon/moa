//! Existing-skill self-improvement logic.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    CompletionRequest, Event, EventRecord, MemoryScope, MemoryStore, MoaConfig, ModelTask, Result,
    SessionMeta, SkillMetadata,
};
use moa_memory::FileMemoryStore;
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use tokio::fs;

use crate::format::{
    SkillDocument, parse_skill_markdown, render_skill_markdown, skill_from_wiki_page,
    skill_metadata_from_document, wiki_page_from_skill,
};
use crate::regression::{append_skill_regression_log, run_skill_regression};

/// Compares a run against an existing skill and updates it when the LLM proposes a better version.
pub async fn maybe_improve_skill(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
    model_router: Arc<ModelRouter>,
) -> Result<Option<SkillMetadata>> {
    maybe_improve_skill_with_learning(
        config,
        session,
        existing,
        events,
        memory_store,
        model_router,
        None,
    )
    .await
}

/// Compares a run against an existing skill and records learning-log entries when provided.
pub async fn maybe_improve_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    memory_store: Arc<FileMemoryStore>,
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<Option<SkillMetadata>> {
    let scope = MemoryScope::Workspace(session.workspace_id.clone());
    let page = memory_store.read_page(&scope, &existing.path).await?;
    let mut current = skill_from_wiki_page(&page)?;
    let current_markdown = render_skill_markdown(&current)?;
    let prompt = build_improvement_prompt(&current_markdown, events);
    let llm = model_router.provider_for(ModelTask::SkillDistillation);
    let response = llm
        .complete(CompletionRequest::simple(prompt))
        .await?
        .collect()
        .await?;
    let updated_text = normalize_llm_markdown(&response.text);
    let now = Utc::now();

    if updated_text.trim() == "UNCHANGED" {
        record_successful_use(&mut current, now);
        let updated_page = wiki_page_from_skill(&current, Some(existing.path.clone()))?;
        memory_store
            .write_page(&scope, &existing.path, updated_page)
            .await?;
        return Ok(None);
    }

    let mut improved = parse_skill_markdown(updated_text)?;
    improved
        .frontmatter
        .set_created(current.frontmatter.created());
    improved.frontmatter.set_updated(now);
    improved
        .frontmatter
        .set_auto_generated(current.frontmatter.auto_generated());
    improved
        .frontmatter
        .set_source_session(Some(session.id.to_string()));
    improved
        .frontmatter
        .set_improved_from(Some(current.frontmatter.version()));
    improved
        .frontmatter
        .set_version(bump_version(&current.frontmatter.version()));
    record_successful_use_with_baseline(&mut improved, &current, now);
    persist_previous_version(
        memory_store.as_ref(),
        &session.workspace_id,
        &existing.path,
        &current_markdown,
    )
    .await?;

    let candidate_markdown = render_skill_markdown(&improved)?;
    let updated_page = wiki_page_from_skill(&improved, Some(existing.path.clone()))?;
    memory_store
        .write_page(&scope, &existing.path, updated_page)
        .await?;
    let report = run_skill_regression(
        config,
        session,
        existing,
        &current_markdown,
        &candidate_markdown,
        memory_store.clone(),
        llm.clone(),
    )
    .await?;
    append_skill_regression_log(
        memory_store.as_ref(),
        session,
        &current.frontmatter.name,
        &current.frontmatter.version(),
        &improved.frontmatter.version(),
        &report,
    )
    .await?;

    if !report.accepted() {
        let mut restored = current.clone();
        record_successful_use(&mut restored, now);
        restored
            .frontmatter
            .set_regression_count(restored.frontmatter.regression_count().saturating_add(1));
        let restored_page = wiki_page_from_skill(&restored, Some(existing.path.clone()))?;
        memory_store
            .write_page(&scope, &existing.path, restored_page)
            .await?;
        return Ok(None);
    }

    let metadata = skill_metadata_from_document(existing.path.clone(), &improved);
    if let Some(store) = learning_store {
        crate::distiller::append_skill_learning(
            store.as_ref(),
            session,
            "skill_improved",
            &metadata,
            serde_json::json!({
                "path": metadata.path.clone(),
                "name": metadata.name.clone(),
                "previous_version": current.frontmatter.version(),
                "version": improved.frontmatter.version(),
            }),
        )
        .await?;
    }

    Ok(Some(metadata))
}

pub(crate) fn normalize_llm_markdown(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(without_fence) = trimmed.strip_prefix("```markdown\n") {
        return without_fence.strip_suffix("\n```").unwrap_or(without_fence);
    }
    if let Some(without_fence) = trimmed.strip_prefix("```\n") {
        return without_fence.strip_suffix("\n```").unwrap_or(without_fence);
    }
    trimmed
}

pub(crate) fn record_successful_use(skill: &mut SkillDocument, now: chrono::DateTime<Utc>) {
    let previous_uses = skill.frontmatter.use_count();
    let previous_success_rate = skill.frontmatter.success_rate();
    let next_uses = previous_uses.saturating_add(1);
    skill.frontmatter.set_use_count(next_uses);
    skill.frontmatter.set_success_rate(blended_success_rate(
        previous_uses,
        previous_success_rate,
        next_uses,
    ));
    skill.frontmatter.set_last_used(Some(now));
    skill.frontmatter.set_updated(now);
}

pub(crate) fn record_successful_use_with_baseline(
    next_skill: &mut SkillDocument,
    previous_skill: &SkillDocument,
    now: chrono::DateTime<Utc>,
) {
    let previous_uses = previous_skill.frontmatter.use_count();
    let next_uses = previous_uses.saturating_add(1);
    next_skill.frontmatter.set_use_count(next_uses);
    next_skill.frontmatter.set_last_used(Some(now));
    next_skill
        .frontmatter
        .set_success_rate(blended_success_rate(
            previous_uses,
            previous_skill.frontmatter.success_rate(),
            next_uses,
        ));
    next_skill.frontmatter.set_updated(now);
}

pub(crate) fn bump_version(version: &str) -> String {
    let mut parts = Vec::new();
    for segment in version.split('.') {
        let parsed = match segment.parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => return "1.0".to_string(),
        };
        parts.push(parsed);
    }

    if let Some(last) = parts.last_mut() {
        *last = last.saturating_add(1);
    } else {
        return "1.0".to_string();
    }

    parts
        .into_iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

fn blended_success_rate(previous_uses: u32, previous_success_rate: f32, next_uses: u32) -> f32 {
    if next_uses == 0 {
        return 1.0;
    }
    ((previous_success_rate * previous_uses as f32) + 1.0) / next_uses as f32
}

async fn persist_previous_version(
    memory_store: &FileMemoryStore,
    workspace_id: &moa_core::WorkspaceId,
    skill_path: &moa_core::MemoryPath,
    markdown: &str,
) -> Result<()> {
    let skill_root = memory_store
        .base_dir()
        .join("workspaces")
        .join(workspace_id.as_str())
        .join("memory");
    let relative = Path::new(skill_path.as_str())
        .parent()
        .unwrap_or_else(|| Path::new("skills"));
    let previous_path = skill_root.join(relative).join("SKILL.md.prev");
    if let Some(parent) = previous_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(previous_path, markdown).await?;
    Ok(())
}

fn build_improvement_prompt(current_skill: &str, events: &[EventRecord]) -> String {
    format!(
        "You are improving an existing MOA Agent Skill.\n\
         Compare the current skill document with the successful execution below.\n\
         If the execution shows a better reusable approach, output the complete updated SKILL.md using the \
         Agent Skills format from agentskills.io.\n\
         Keep spec-compatible top-level frontmatter fields and preserve MOA-specific bookkeeping in the \
         `metadata` map with `moa-` prefixes.\n\
         If the existing skill is still correct, output exactly UNCHANGED.\n\n\
         Current skill:\n{current_skill}\n\n\
         Actual execution:\n{}",
        format_events_for_learning(events)
    )
}

pub(crate) fn format_events_for_learning(events: &[EventRecord]) -> String {
    let mut lines = Vec::new();
    for record in events {
        match &record.event {
            Event::UserMessage { text, .. } => lines.push(format!("user: {text}")),
            Event::QueuedMessage { text, .. } => lines.push(format!("queued: {text}")),
            Event::ToolCall {
                tool_name, input, ..
            } => lines.push(format!("tool_call {tool_name}: {input}")),
            Event::ToolResult {
                output, success, ..
            } => {
                lines.push(format!(
                    "tool_result success={success}: {}",
                    output.to_text()
                ));
            }
            Event::ToolError { error, .. } => lines.push(format!("tool_error: {error}")),
            Event::BrainResponse { text, .. } => lines.push(format!("assistant: {text}")),
            _ => {}
        }
    }
    lines.join("\n")
}

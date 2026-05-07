//! Existing-skill self-improvement logic.

use std::sync::{Arc, OnceLock};

use chrono::Utc;
use moa_core::{
    CompletionRequest, Event, EventRecord, MemoryScope, MoaConfig, ModelTask, Result, SessionMeta,
    SkillMetadata,
};
use moa_providers::ModelRouter;
use moa_session::{PostgresSessionStore, create_session_store};

use crate::format::{
    SkillDocument, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
};
use crate::registry::{NewSkill, SkillRegistry};
use crate::regression::{append_skill_regression_log, run_skill_regression};

static IMPROVEMENT_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// Outcome of one attempted existing-skill improvement.
#[derive(Debug, Clone, PartialEq)]
pub enum ImprovementResult {
    /// The candidate was accepted and became the active skill version.
    Improved {
        /// Metadata for the active improved version.
        metadata: SkillMetadata,
        /// Previous semantic skill version.
        previous_version: String,
        /// Accepted semantic skill version.
        version: String,
    },
    /// The LLM concluded the current skill already covers the successful run.
    Unchanged {
        /// Metadata for the unchanged active skill.
        metadata: SkillMetadata,
    },
    /// The proposed candidate regressed and the previous version remains active.
    Rejected {
        /// Regression report explaining why the candidate was rejected.
        report: crate::regression::SkillRegressionReport,
    },
    /// No improvement was attempted because the requested skill could not be loaded.
    Skipped,
}

/// Compares a run against an existing skill and updates it when the LLM proposes a better version.
pub async fn maybe_improve_skill(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
) -> Result<Option<SkillMetadata>> {
    let learning_store = create_session_store(config).await?;
    maybe_improve_skill_with_learning(
        config,
        session,
        existing,
        events,
        model_router,
        Some(learning_store),
    )
    .await
}

/// Compares a run against an existing skill and records learning-log entries when provided.
pub async fn maybe_improve_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<Option<SkillMetadata>> {
    match improve_skill_with_learning(
        config,
        session,
        existing,
        events,
        model_router,
        learning_store,
    )
    .await?
    {
        ImprovementResult::Improved { metadata, .. } => Ok(Some(metadata)),
        ImprovementResult::Unchanged { .. }
        | ImprovementResult::Rejected { .. }
        | ImprovementResult::Skipped => Ok(None),
    }
}

/// Compares a run against an existing skill and returns a typed outcome for tests and callers.
pub async fn improve_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<ImprovementResult> {
    let Some(store) = learning_store.clone() else {
        return Ok(ImprovementResult::Skipped);
    };
    let lock = IMPROVEMENT_LOCK.get_or_init(|| tokio::sync::Mutex::new(()));
    let _guard = lock.lock().await;
    let registry = SkillRegistry::new(store.pool().clone());
    let scope = MemoryScope::Workspace {
        workspace_id: session.workspace_id.clone(),
    };
    let Some(row) = registry.load_by_name(&scope, &existing.name).await? else {
        return Ok(ImprovementResult::Skipped);
    };
    let current = parse_skill_markdown(&row.body)?;
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
        return Ok(ImprovementResult::Unchanged {
            metadata: skill_metadata_from_document(existing.path.clone(), &current),
        });
    }

    let previous_version = current.frontmatter.version();
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
        .set_improved_from(Some(previous_version.clone()));
    let breaking_change = skill_signature_changed(&current, &improved);
    improved
        .frontmatter
        .set_version(bump_version_for_change(&previous_version, breaking_change));
    record_successful_use_with_baseline(&mut improved, &current, now);

    let candidate_markdown = render_skill_markdown(&improved)?;
    registry
        .upsert_by_name(NewSkill::from_document(
            scope.clone(),
            &improved,
            candidate_markdown.clone(),
        ))
        .await?;
    let report = run_skill_regression(
        config,
        session,
        existing,
        &current_markdown,
        &candidate_markdown,
        llm.clone(),
    )
    .await?;
    append_skill_regression_log(
        store.as_ref(),
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
        let markdown = render_skill_markdown(&restored)?;
        registry
            .upsert_by_name(NewSkill::from_document(scope, &restored, markdown))
            .await?;
        return Ok(ImprovementResult::Rejected { report });
    }

    let metadata = skill_metadata_from_document(existing.path.clone(), &improved);
    let version = improved.frontmatter.version();
    crate::distiller::append_skill_learning(
        store.as_ref(),
        session,
        "skill_improved",
        &metadata,
        serde_json::json!({
            "path": metadata.path.clone(),
            "name": metadata.name.clone(),
            "previous_version": previous_version,
            "version": version,
            "originating_session_id": session.id.to_string(),
            "diff_summary": summarize_diff(&current_markdown, &candidate_markdown),
        }),
    )
    .await?;

    Ok(ImprovementResult::Improved {
        metadata,
        previous_version: current.frontmatter.version(),
        version: improved.frontmatter.version(),
    })
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

fn bump_version_for_change(version: &str, breaking_change: bool) -> String {
    if breaking_change {
        return bump_major_version(version);
    }
    bump_version(version)
}

fn bump_major_version(version: &str) -> String {
    let Some(major) = version.split('.').next() else {
        return "2.0".to_string();
    };
    match major.parse::<u64>() {
        Ok(major) => format!("{}.0", major.saturating_add(1)),
        Err(_) => "2.0".to_string(),
    }
}

fn skill_signature_changed(previous: &SkillDocument, candidate: &SkillDocument) -> bool {
    previous.frontmatter.allowed_tools != candidate.frontmatter.allowed_tools
        || previous.frontmatter.compatibility != candidate.frontmatter.compatibility
}

fn blended_success_rate(previous_uses: u32, previous_success_rate: f32, next_uses: u32) -> f32 {
    if next_uses == 0 {
        return 1.0;
    }
    ((previous_success_rate * previous_uses as f32) + 1.0) / next_uses as f32
}

fn summarize_diff(previous: &str, candidate: &str) -> String {
    if previous == candidate {
        return "unchanged".to_string();
    }

    let previous_lines = previous.lines().count();
    let candidate_lines = candidate.lines().count();
    let line_delta = candidate_lines as isize - previous_lines as isize;
    format!("body changed; line_delta={line_delta}")
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

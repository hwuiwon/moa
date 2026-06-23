//! Existing-skill self-improvement draft generation.

use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    ActionRuleScope, CompletionRequest, Event, EventRecord, MoaConfig, ModelTask, Result,
    SessionMeta, SkillMetadata, WorkspaceId,
};
use moa_providers::ModelRouter;
use moa_session::{PostgresSessionStore, create_session_store};

use crate::format::{
    SkillDocument, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
};
use crate::package::{SKILL_MD_PATH, SkillPackage, SkillPackageFile, ValidatedSkillPackageFile};
use crate::proposals::{
    SkillDraftProposal, SkillProposalOperation, SkillProposalSource, store_skill_draft_proposal,
};
use crate::registry::SkillRegistry;
use crate::regression::generate_skill_test_suite_source;

/// Outcome of one attempted existing-skill improvement.
#[derive(Debug, Clone, PartialEq)]
pub enum ImprovementResult {
    /// The candidate was stored as a reviewable draft and the active skill was left unchanged.
    Improved {
        /// Stored draft proposal.
        proposal: SkillDraftProposal,
        /// Previous semantic skill version.
        previous_version: String,
        /// Proposed semantic skill version.
        version: String,
    },
    /// The LLM concluded the current skill already covers the successful run.
    Unchanged {
        /// Metadata for the unchanged active skill.
        metadata: SkillMetadata,
    },
    /// The generated output was rejected before a draft was stored.
    Rejected {
        /// Human-readable rejection reason.
        reason: String,
    },
    /// No improvement was attempted because the requested skill could not be loaded.
    Skipped,
}

/// Compares a run against an existing skill and proposes an update when useful.
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

/// Compares a run against an existing skill and records a draft proposal when provided.
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
        ImprovementResult::Improved { proposal, .. } => Ok(Some(proposal.metadata)),
        ImprovementResult::Unchanged { .. }
        | ImprovementResult::Rejected { .. }
        | ImprovementResult::Skipped => Ok(None),
    }
}

/// Compares a run against an existing skill and returns a typed proposal outcome.
pub async fn improve_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<ImprovementResult> {
    improve_skill_with_learning_for_sources(
        config,
        session,
        existing,
        events,
        model_router,
        learning_store,
        SkillProposalSource::session_only(),
    )
    .await
}

pub(crate) async fn improve_skill_with_learning_for_sources(
    _config: &MoaConfig,
    session: &SessionMeta,
    existing: &SkillMetadata,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
    source: SkillProposalSource,
) -> Result<ImprovementResult> {
    let Some(store) = learning_store else {
        return Ok(ImprovementResult::Skipped);
    };
    let registry = SkillRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let Some(stored_package) = registry
        .load_package_by_name(&scope, &existing.name)
        .await?
    else {
        return Ok(ImprovementResult::Skipped);
    };
    let stored_markdown = stored_package.skill_markdown()?;
    let current = parse_skill_markdown(stored_markdown)?;
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
    if improved.frontmatter.name != current.frontmatter.name {
        return Ok(ImprovementResult::Rejected {
            reason: "skill improvement changed the target skill name".to_string(),
        });
    }
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
    let metadata = skill_metadata_from_document(existing.path.clone(), &improved);
    let candidate_package =
        package_with_replaced_skill_md(&stored_package.files, candidate_markdown).validate()?;
    let workspace_id = WorkspaceId::new(session.tenant_id.to_string());
    let generated_suite = generate_skill_test_suite_source(&workspace_id, &improved, events)?;
    let proposal = store_skill_draft_proposal(
        store.as_ref(),
        session,
        &candidate_package,
        metadata,
        SkillProposalOperation::Improved {
            previous_version: previous_version.clone(),
        },
        source,
        generated_suite,
    )
    .await?;
    let version = improved.frontmatter.version();

    Ok(ImprovementResult::Improved {
        proposal,
        previous_version,
        version,
    })
}

fn package_with_replaced_skill_md(
    files: &[ValidatedSkillPackageFile],
    skill_md: String,
) -> SkillPackage {
    let skill_md_bytes = skill_md.into_bytes();
    SkillPackage::new(
        files
            .iter()
            .map(|file| SkillPackageFile {
                path: file.path.clone(),
                content: if file.path == SKILL_MD_PATH {
                    skill_md_bytes.clone()
                } else {
                    file.content.clone()
                },
                content_type: file.content_type.clone(),
                executable: file.executable,
            })
            .collect(),
    )
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

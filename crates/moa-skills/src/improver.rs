//! Existing-skill self-improvement draft generation.

use std::sync::Arc;

use moa_core::{
    ActionRuleScope, CompletionRequest, ContextMessage, Event, EventRecord, MoaConfig, ModelTask,
    Result, SessionMeta, SkillMetadata,
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
    let llm = model_router.provider_for(ModelTask::SkillDistillation);
    let response = llm
        .complete(build_improvement_request(&current_markdown, events))
        .await?
        .collect()
        .await?;
    let updated_text = normalize_llm_markdown(&response.text);
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
    let breaking_change = skill_signature_changed(&current, &improved);
    improved
        .frontmatter
        .set_version(bump_version_for_change(&previous_version, breaking_change));

    let candidate_markdown = render_skill_markdown(&improved)?;
    let metadata = skill_metadata_from_document(existing.path.clone(), &improved);
    let candidate_package =
        package_with_replaced_skill_md(&stored_package.files, candidate_markdown).validate()?;
    let generated_suite = generate_skill_test_suite_source(session.tenant_id, &improved, events)?;
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

const SKILL_IMPROVEMENT_SYSTEM_PROMPT: &str = "\
You are improving an existing Agent Skill.
Compare the current skill document with the successful execution provided by the user.
If the execution shows a better reusable approach, output the complete updated SKILL.md using the \
Agent Skills format from agentskills.io.
Keep spec-compatible top-level frontmatter fields and only use MOA metadata for `moa-version`, \
`moa-tags`, and `moa-estimated-tokens`.
If the existing skill is still correct, output exactly UNCHANGED.";

fn build_improvement_request(current_skill: &str, events: &[EventRecord]) -> CompletionRequest {
    CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system(SKILL_IMPROVEMENT_SYSTEM_PROMPT),
            ContextMessage::user(build_improvement_user_prompt(current_skill, events)),
        ],
        tools: Vec::new(),
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        metadata: Default::default(),
    }
}

fn build_improvement_user_prompt(current_skill: &str, events: &[EventRecord]) -> String {
    format!(
        "Current skill:\n{current_skill}\n\n\
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

#[cfg(test)]
mod tests {
    use moa_core::MessageRole;

    use super::*;

    #[test]
    fn skill_improvement_request_keeps_current_skill_out_of_system_prompt() {
        // Pins: skill-improvement evidence is dynamic so the stable judge instructions remain cacheable.
        let request = build_improvement_request("# Existing Skill\n", &[]);

        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[1].role, MessageRole::User);
        assert!(
            request.messages[0]
                .content
                .contains("output exactly UNCHANGED")
        );
        assert!(!request.messages[0].content.contains("# Existing Skill"));
        assert!(
            request.messages[1]
                .content
                .contains("Current skill:\n# Existing Skill")
        );
        assert!(request.messages[1].content.contains("Actual execution:"));
    }
}

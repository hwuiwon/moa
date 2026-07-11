//! Existing-skill self-improvement draft generation.

use std::sync::Arc;

use moa_core::{
    error::Result, events::Event, types::action_policy::ActionRuleScope,
    types::completion::CompletionRequest, types::events_stream::EventRecord,
    types::memory::SkillMetadata, types::provider::ModelTask, types::session::SessionMeta,
};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;

use crate::distiller::ExperienceDistillationInput;
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

/// Compares an assessed experience against an existing skill and returns a typed proposal outcome.
///
/// This is the experience-native improver entry: the proposal is keyed to and
/// audited against the experience record, so callers must have already applied
/// the learnability gates that `distill_skill_from_experience_with_learning`
/// enforces before routing here.
pub async fn improve_skill_from_experience_with_learning(
    session: &SessionMeta,
    existing: &SkillMetadata,
    input: &ExperienceDistillationInput,
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<ImprovementResult> {
    let routing = serde_json::json!({
        "decision": "improve_existing",
        "matched_skill": existing.name.clone(),
        "reason": "caller-directed improvement of a named skill",
    });
    let source = crate::distiller::proposal_source_from_experience(input, Some(routing));
    improve_skill_with_learning_for_sources(
        session,
        existing,
        &input.events,
        model_router,
        learning_store,
        source,
    )
    .await
}

pub(crate) async fn improve_skill_with_learning_for_sources(
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
    // Preflight before the LLM call: an open improvement draft for this skill
    // already awaits review, so generating again would only produce a
    // duplicate for the in-transaction dedupe to discard. The new session
    // still contributes a sibling suite as held-out gate material. The version
    // fields echo the open draft's baseline; the proposed version lives in the
    // draft artifact itself.
    if let Some(open) = crate::proposals::find_open_skill_proposal(
        store.as_ref(),
        session.tenant_id,
        Some(&existing.name),
        None,
    )
    .await?
    {
        if let Some(source_experience_id) = source.source_experience_ids.first().copied() {
            let sibling_suite = crate::regression::generate_skill_test_suite_source_for_name(
                session.tenant_id,
                &open.metadata.name,
                events,
            )?;
            crate::proposals::accumulate_sibling_suite(
                store.as_ref(),
                session.tenant_id,
                &open,
                sibling_suite,
                source_experience_id,
                session.id,
            )
            .await?;
        }
        let previous_version = match &open.operation {
            SkillProposalOperation::Improved { previous_version } => previous_version.clone(),
            SkillProposalOperation::Created => String::new(),
        };
        return Ok(ImprovementResult::Improved {
            proposal: open,
            previous_version: previous_version.clone(),
            version: previous_version,
        });
    }
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
    crate::util::completion_request(
        SKILL_IMPROVEMENT_SYSTEM_PROMPT,
        build_improvement_user_prompt(current_skill, events),
    )
}

fn build_improvement_user_prompt(current_skill: &str, events: &[EventRecord]) -> String {
    format!(
        "Current skill:\n{current_skill}\n\n\
         Actual execution:\n{}",
        format_events_for_learning(events)
    )
}

/// Per-event character cap for learning prompts; tool output can be megabytes.
const LEARNING_EVENT_TEXT_CAP: usize = 2_000;
/// Total character budget for the formatted event section of a learning prompt.
const LEARNING_EVENTS_TOTAL_CAP: usize = 60_000;

pub(crate) fn format_events_for_learning(events: &[EventRecord]) -> String {
    let mut lines = Vec::new();
    let mut total = 0usize;
    for record in events {
        let line = match &record.event {
            Event::UserMessage { text, .. } => {
                Some(format!("user: {}", truncate_for_learning(text)))
            }
            Event::QueuedMessage { text, .. } => {
                Some(format!("queued: {}", truncate_for_learning(text)))
            }
            Event::ToolCall {
                tool_name, input, ..
            } => Some(format!(
                "tool_call {tool_name}: {}",
                truncate_for_learning(&input.to_string())
            )),
            Event::ToolResult {
                output, success, ..
            } => Some(format!(
                "tool_result success={success}: {}",
                truncate_for_learning(&output.to_text())
            )),
            Event::ToolError { error, .. } => {
                Some(format!("tool_error: {}", truncate_for_learning(error)))
            }
            Event::BrainResponse { text, .. } => {
                Some(format!("assistant: {}", truncate_for_learning(text)))
            }
            _ => None,
        };
        let Some(line) = line else { continue };
        total += line.len();
        if total > LEARNING_EVENTS_TOTAL_CAP {
            lines.push("[remaining events omitted: learning prompt budget reached]".to_string());
            break;
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn truncate_for_learning(text: &str) -> String {
    if text.chars().count() <= LEARNING_EVENT_TEXT_CAP {
        return text.to_string();
    }
    let prefix: String = text.chars().take(LEARNING_EVENT_TEXT_CAP).collect();
    format!("{prefix} [truncated]")
}

#[cfg(test)]
mod tests {
    use moa_core::types::context::MessageRole;

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

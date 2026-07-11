//! Skill distillation from successful agent runs.

use std::collections::HashSet;
use std::sync::Arc;

use moa_core::{
    config::MoaConfig, error::Result, events::Event, types::completion::CompletionRequest,
    types::events_stream::EventRecord, types::experience::AttributionEffect,
    types::experience::AttributionSubjectType, types::experience::ExperienceAttribution,
    types::experience::ExperienceRecord, types::memory::SkillMetadata, types::provider::ModelTask,
    types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
    types::session::SessionMeta,
};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
};
use crate::improver::{ImprovementResult, format_events_for_learning, normalize_llm_markdown};
use crate::package::SkillPackage;
use crate::proposals::{
    SkillDraftProposal, SkillProposalOperation, SkillProposalSource, store_skill_draft_proposal,
};
use crate::registry::SkillRegistry;
use crate::regression::{
    generate_skill_test_suite_source, generate_skill_test_suite_source_for_name,
};

/// Similarity score at or above which distillation routes to existing-skill improvement.
pub const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Reason an experience was not distilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistillationSkipReason {
    /// The segment did not contain enough tool calls to justify distillation.
    BelowThreshold,
    /// The assessed segment outcome is not learnable enough to seed a reusable skill.
    UnlearnableOutcome,
    /// No learning store was available to persist a reviewable draft proposal.
    LearningStoreUnavailable,
}

/// Typed outcome of one skill-distillation attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum DistillationOutcome {
    /// A new skill draft proposal was persisted for review.
    NewSkillProposed {
        /// Stored draft proposal.
        proposal: SkillDraftProposal,
    },
    /// The session was routed to improvement of a similar existing skill.
    ImprovementProposed {
        /// Existing skill selected by similarity routing.
        existing_skill_id: String,
        /// Stored draft proposal when the improver generated a change.
        proposal: Option<SkillDraftProposal>,
    },
    /// Distillation was intentionally skipped.
    Skipped {
        /// Stable skip reason.
        reason: DistillationSkipReason,
    },
}

/// Proposal-level outcome distilled from a skill generation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillProposalGeneration {
    /// A reviewable skill draft proposal was created.
    Proposed {
        /// Proposed learning-candidate ID.
        candidate_id: uuid::Uuid,
        /// Draft artifact revision that contains the generated skill package.
        draft_artifact_revision_uid: uuid::Uuid,
    },
    /// Similar-skill routing found no useful draft change.
    Unchanged,
    /// Distillation was intentionally skipped.
    Skipped {
        /// Stable skip reason.
        reason: DistillationSkipReason,
    },
}

/// Converts a full distillation outcome into proposal-level review state.
#[must_use]
pub fn proposal_generation_from_distillation(
    outcome: DistillationOutcome,
) -> SkillProposalGeneration {
    match outcome {
        DistillationOutcome::NewSkillProposed { proposal }
        | DistillationOutcome::ImprovementProposed {
            proposal: Some(proposal),
            ..
        } => SkillProposalGeneration::Proposed {
            candidate_id: proposal.candidate_id,
            draft_artifact_revision_uid: proposal.draft_artifact_revision_uid,
        },
        DistillationOutcome::ImprovementProposed { .. } => SkillProposalGeneration::Unchanged,
        DistillationOutcome::Skipped { reason } => SkillProposalGeneration::Skipped { reason },
    }
}

/// Segment-native input for experience-backed skill distillation.
#[derive(Debug, Clone)]
pub struct ExperienceDistillationInput {
    /// Assessed experience record to learn from.
    pub experience: ExperienceRecord,
    /// Attribution records generated for the experience.
    pub attributions: Vec<ExperienceAttribution>,
    /// Bounded segment events used as learning evidence.
    pub events: Vec<EventRecord>,
}

/// Distills an assessed experience into a skill candidate and promotes it when gates pass.
pub async fn distill_skill_from_experience_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    input: ExperienceDistillationInput,
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<DistillationOutcome> {
    if !experience_is_learnable(&input.experience, &input.attributions) {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::UnlearnableOutcome,
        });
    }
    if count_tool_calls(&input.events) < config.learning.skills.min_tool_calls {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::BelowThreshold,
        });
    }
    let Some(store) = learning_store.clone() else {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::LearningStoreUnavailable,
        });
    };

    // Preflight before any LLM spend: an open proposal for this task
    // fingerprint already awaits review, so generating again would only
    // produce a duplicate for the in-transaction dedupe to discard. The new
    // session still contributes: its deterministic suite accumulates onto the
    // open candidate as held-out material for the review gate.
    if let Some(open) = crate::proposals::find_open_skill_proposal(
        store.as_ref(),
        session.tenant_id,
        None,
        Some(&input.experience.task_fingerprint.hash),
    )
    .await?
    {
        let sibling_suite = generate_skill_test_suite_source_for_name(
            session.tenant_id,
            &open.metadata.name,
            &input.events,
        )?;
        crate::proposals::accumulate_sibling_suite(
            store.as_ref(),
            session.tenant_id,
            &open,
            sibling_suite,
            input.experience.id,
            session.id,
        )
        .await?;
        return Ok(outcome_for_open_proposal(open));
    }

    let task_summary = experience_similarity_text(&input.experience);
    let existing_skills = SkillRegistry::new(store.pool().clone())
        .list_for_pipeline(session.tenant_id)
        .await?;

    if let Some((similarity, existing)) = find_similar_skill(&task_summary, &existing_skills) {
        let existing_skill_id = existing.name.clone();
        let routing = serde_json::json!({
            "decision": "improve_existing",
            "matched_skill": existing_skill_id.clone(),
            "similarity_score": similarity,
            "similarity_threshold": SIMILARITY_THRESHOLD,
        });
        let source = proposal_source_from_experience(&input, Some(routing));
        let result = crate::improver::improve_skill_with_learning_for_sources(
            session,
            existing,
            &input.events,
            model_router,
            learning_store,
            source,
        )
        .await;
        return result.map(|result| DistillationOutcome::ImprovementProposed {
            existing_skill_id,
            proposal: match result {
                ImprovementResult::Improved { proposal, .. } => Some(proposal),
                ImprovementResult::Unchanged { .. }
                | ImprovementResult::Rejected { .. }
                | ImprovementResult::Skipped => None,
            },
        });
    }

    let llm = model_router.provider_for(ModelTask::SkillDistillation);
    let response = llm
        .complete(build_experience_distillation_request(&input))
        .await?
        .collect()
        .await?;
    let skill_markdown = normalize_llm_markdown(&response.text);
    let skill = parse_skill_markdown(skill_markdown)?;
    let path = build_skill_path(&skill.frontmatter.name);
    let markdown = render_skill_for_registry(&skill)?;
    let metadata = skill_metadata_from_document(path.clone(), &skill);
    let package = SkillPackage::from_skill_markdown(markdown).validate()?;
    let generated_suite =
        generate_skill_test_suite_source(session.tenant_id, &skill, &input.events)?;
    let routing = serde_json::json!({
        "decision": "create_new",
        "similarity_threshold": SIMILARITY_THRESHOLD,
        "reason": "no existing tenant skill matched the task above the similarity threshold",
    });
    let proposal = store_skill_draft_proposal(
        store.as_ref(),
        session,
        &package,
        metadata,
        SkillProposalOperation::Created,
        proposal_source_from_experience(&input, Some(routing)),
        generated_suite,
    )
    .await?;

    Ok(DistillationOutcome::NewSkillProposed { proposal })
}

/// Maps an already-open draft proposal onto the distillation outcome it represents.
fn outcome_for_open_proposal(proposal: SkillDraftProposal) -> DistillationOutcome {
    match &proposal.operation {
        SkillProposalOperation::Created => DistillationOutcome::NewSkillProposed { proposal },
        SkillProposalOperation::Improved { .. } => DistillationOutcome::ImprovementProposed {
            existing_skill_id: proposal.metadata.name.clone(),
            proposal: Some(proposal),
        },
    }
}

/// Builds the proposal source (lineage + reviewer evidence) for one experience.
pub(crate) fn proposal_source_from_experience(
    input: &ExperienceDistillationInput,
    routing: Option<serde_json::Value>,
) -> SkillProposalSource {
    SkillProposalSource {
        source_experience_ids: vec![input.experience.id],
        task_fingerprint: Some(input.experience.task_fingerprint.clone()),
        task_facets: Some(input.experience.task_facets.clone()),
        confidence: Some(input.experience.confidence),
        evidence: Some(experience_evidence_payload(input, routing)),
    }
}

/// Renders the reviewer-facing rationale for a proposal derived from one experience.
///
/// Reviewers accept or reject drafts through `LearningReview/get`; this block
/// answers "why was this proposed" without requiring them to load the source
/// session: the assessed outcome and confidence, the segment-assessment
/// evidence rows, verification/tool attributions, and the similarity routing
/// that chose improve-vs-create.
fn experience_evidence_payload(
    input: &ExperienceDistillationInput,
    routing: Option<serde_json::Value>,
) -> serde_json::Value {
    let experience = &input.experience;
    let attributions = input
        .attributions
        .iter()
        .map(|row| {
            serde_json::json!({
                "subject_type": row.subject_type,
                "subject_id": row.subject_id,
                "effect": row.effect,
                "confidence": row.confidence,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "source": "segment_assessment",
        "task_summary": experience.task_summary,
        "outcome": experience.outcome.as_str(),
        "confidence": experience.confidence,
        "task_fingerprint": {
            "hash": experience.task_fingerprint.hash,
            "normalized_summary": experience.task_fingerprint.normalized_summary,
        },
        "tools_used": experience.tools_used,
        "skills_activated": experience.skills_activated,
        "turn_count": experience.turn_count,
        "segment_evidence": experience.evidence,
        "attributions": attributions,
        "routing": routing,
    })
}

fn render_skill_for_registry(skill: &SkillDocument) -> Result<String> {
    crate::format::render_skill_markdown(skill)
}

fn count_tool_calls(events: &[EventRecord]) -> usize {
    events
        .iter()
        .filter(|record| matches!(record.event, Event::ToolCall { .. }))
        .count()
}

const SKILL_DISTILLATION_SYSTEM_PROMPT: &str = "\
Distill task evidence into a reusable Agent Skill.
Output only a complete SKILL.md document using the Agent Skills format from agentskills.io.
Use spec-compatible top-level frontmatter fields such as `name`, `description`, optional \
`compatibility`, optional `allowed-tools`, and a `metadata` map for project-specific bookkeeping.
Store project-specific fields inside `metadata` using `moa-` prefixes, including at least \
`moa-version`, `moa-tags`, and `moa-estimated-tokens`.
The skill should include when-to-use guidance, a numbered procedure, pitfalls, and verification steps.
Learn only durable workflow structure. Do not copy secrets, transient IDs, or one-off paths unless \
the path is essential to the workflow.";

fn build_experience_distillation_request(input: &ExperienceDistillationInput) -> CompletionRequest {
    crate::util::completion_request(
        SKILL_DISTILLATION_SYSTEM_PROMPT,
        build_experience_distillation_user_prompt(input),
    )
}

fn build_experience_distillation_user_prompt(input: &ExperienceDistillationInput) -> String {
    let experience = &input.experience;
    format!(
        "Task summary: {}\n\
         Outcome: {} (confidence {:.3})\n\
         Task fingerprint: {}\n\
         Task facets: {}\n\
         Tools: {}\n\
         Skills: {}\n\
         Assessment evidence: {}\n\
         Attributions: {}\n\n\
         Segment events:\n{}",
        experience
            .task_summary
            .as_deref()
            .unwrap_or("unspecified task"),
        experience.outcome.as_str(),
        experience.confidence,
        experience.task_fingerprint.hash,
        serde_json::to_string(&experience.task_facets).unwrap_or_else(|_| "{}".to_string()),
        experience.tools_used.join(", "),
        experience.skills_activated.join(", "),
        serde_json::to_string(&experience.evidence).unwrap_or_else(|_| "[]".to_string()),
        serde_json::to_string(&input.attributions).unwrap_or_else(|_| "[]".to_string()),
        format_events_for_learning(&input.events)
    )
}

/// Returns whether an assessed experience is strong enough to seed skill learning.
///
/// Resolved outcomes need confidence >= 0.7; partial outcomes need >= 0.85 plus
/// a helpful attribution and verification support. Dispatchers call this before
/// spawning the detached learning workflow, and the distiller re-checks it as a
/// replay-safe gate.
pub fn experience_is_learnable(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
) -> bool {
    match experience.outcome {
        SegmentOutcome::Resolved => experience.confidence >= 0.7,
        SegmentOutcome::Partial => {
            experience.confidence >= 0.85
                && has_helpful_attribution(attributions)
                && has_verification_support(experience, attributions)
        }
        SegmentOutcome::Unknown | SegmentOutcome::Failed | SegmentOutcome::Abandoned => false,
        _ => false,
    }
}

fn has_helpful_attribution(attributions: &[ExperienceAttribution]) -> bool {
    attributions
        .iter()
        .any(|row| row.effect == AttributionEffect::Helpful)
}

fn has_verification_support(
    experience: &ExperienceRecord,
    attributions: &[ExperienceAttribution],
) -> bool {
    experience.evidence.iter().any(|evidence| {
        evidence.kind == SegmentEvidenceKind::Verification
            && matches!(
                evidence.polarity,
                SegmentEvidencePolarity::SupportsResolved
                    | SegmentEvidencePolarity::SupportsPartial
            )
    }) || attributions.iter().any(|row| {
        row.subject_type == AttributionSubjectType::Verification
            && row.effect == AttributionEffect::Helpful
    })
}

fn experience_similarity_text(experience: &ExperienceRecord) -> String {
    let mut parts = vec![
        experience
            .task_summary
            .clone()
            .unwrap_or_else(|| experience.task_fingerprint.normalized_summary.clone()),
        experience.task_fingerprint.normalized_summary.clone(),
    ];
    for value in [
        experience.task_facets.domain.as_deref(),
        experience.task_facets.action.as_deref(),
        experience.task_facets.artifact_kind.as_deref(),
        experience.task_facets.language_or_framework.as_deref(),
        experience.task_facets.verification_style.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        parts.push(value.to_string());
    }
    parts.extend(experience.task_facets.tool_pattern.clone());
    parts.extend(experience.task_facets.skill_pattern.clone());
    parts.join(" ")
}

fn find_similar_skill<'a>(
    task_summary: &str,
    skills: &'a [SkillMetadata],
) -> Option<(f32, &'a SkillMetadata)> {
    let summary_tokens = tokenize(task_summary);
    skills
        .iter()
        .map(|skill| (similarity_score(&summary_tokens, skill), skill))
        .filter(|(score, _)| *score >= SIMILARITY_THRESHOLD)
        .max_by(|left, right| left.0.total_cmp(&right.0))
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{
        types::context::MessageRole, types::experience::ExperienceRecord,
        types::experience::TaskFacetSet, types::experience::TaskFingerprint,
        types::identifiers::SegmentId, types::identifiers::SessionId, types::identifiers::TenantId,
        types::identifiers::UserId, types::segment_assessment::SegmentEvidence,
        types::segment_assessment::SegmentEvidenceKind,
        types::segment_assessment::SegmentEvidencePolarity,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn experience_distillation_skips_failed_experience_even_with_verification() {
        // Pins: failed assessed experiences cannot seed skill distillation.
        let experience = experience(SegmentOutcome::Failed, 0.95);
        let attributions = vec![verification_attribution(
            &experience,
            AttributionEffect::Harmful,
        )];

        assert!(!experience_is_learnable(&experience, &attributions));
    }

    #[test]
    fn experience_distillation_allows_high_confidence_partial_with_helpful_verification() {
        // Pins: partial experiences need both high confidence and helpful verification evidence.
        let experience = experience(SegmentOutcome::Partial, 0.9);
        let attributions = vec![verification_attribution(
            &experience,
            AttributionEffect::Helpful,
        )];

        assert!(experience_is_learnable(&experience, &attributions));
    }

    #[test]
    fn experience_distillation_request_keeps_segment_evidence_out_of_system_prompt() {
        // Pins: learned-skill generation reuses static instructions while segment evidence
        // stays dynamic, so the stable system prompt remains cacheable.
        let input = ExperienceDistillationInput {
            experience: experience(SegmentOutcome::Resolved, 0.9),
            attributions: Vec::new(),
            events: Vec::new(),
        };
        let request = build_experience_distillation_request(&input);

        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[1].role, MessageRole::User);
        assert!(request.messages[0].content.contains("Agent Skills format"));
        assert!(!request.messages[0].content.contains("Fix auth regression"));
        assert!(
            request.messages[1]
                .content
                .contains("Task summary: Fix auth regression")
        );
        assert!(request.messages[1].content.contains("Segment events:"));
    }

    fn experience(outcome: SegmentOutcome, confidence: f64) -> ExperienceRecord {
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        ExperienceRecord {
            id: Uuid::now_v7(),
            segment_id: SegmentId::new(),
            session_id: SessionId::new(),
            tenant_id: TenantId::new(),
            user_id: UserId::new("user"),
            task_summary: Some("Fix auth regression".to_string()),
            task_fingerprint: TaskFingerprint {
                hash: "hash".to_string(),
                normalized_summary: "auth fix regression".to_string(),
                policy_version: "experience_v1".to_string(),
            },
            task_facets: TaskFacetSet::default(),
            actions: Vec::new(),
            resources: Vec::new(),
            outcome,
            confidence,
            evidence: vec![SegmentEvidence {
                kind: SegmentEvidenceKind::Verification,
                polarity: SegmentEvidencePolarity::SupportsPartial,
                strength: 0.8,
                summary: "focused verification passed".to_string(),
            }],
            tools_used: vec![
                "bash".to_string();
                MoaConfig::default().learning.skills.min_tool_calls
            ],
            skills_activated: Vec::new(),
            turn_count: 2,
            token_cost: 10,
            duration_ms: Some(100),
            assessment_policy_version: "assessment_v1".to_string(),
            extraction_policy_version: "experience_v1".to_string(),
            created_at: now,
        }
    }

    fn verification_attribution(
        experience: &ExperienceRecord,
        effect: AttributionEffect,
    ) -> ExperienceAttribution {
        ExperienceAttribution {
            id: Uuid::now_v7(),
            experience_id: experience.id,
            tenant_id: experience.tenant_id,
            user_id: Some(experience.user_id.clone()),
            subject_type: AttributionSubjectType::Verification,
            subject_id: "verification".to_string(),
            effect,
            confidence: experience.confidence,
            evidence: Vec::new(),
            created_at: experience.created_at,
        }
    }
}

//! Skill distillation from successful agent runs.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use moa_core::{
    AttributionEffect, AttributionSubjectType, CompletionRequest, Event, EventRecord,
    ExperienceAttribution, ExperienceRecord, LearningCandidateStatus, MoaConfig, ModelTask, Result,
    SegmentEvidenceKind, SegmentEvidencePolarity, SegmentOutcome, SessionMeta, SkillMetadata,
};
use moa_providers::ModelRouter;
use moa_session::{PostgresSessionStore, create_session_store};

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
};
use crate::improver::{
    ImprovementResult, format_events_for_learning, normalize_llm_markdown, record_successful_use,
};
use crate::registry::{NewSkill, SkillRegistry};
use crate::regression::generate_skill_test_suite;

/// Minimum number of tool calls required before a successful session can become a skill.
pub const MIN_TOOL_CALLS_FOR_DISTILLATION: usize = 5;
/// Similarity score at or above which distillation routes to existing-skill improvement.
pub const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Reason a session was not distilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistillationSkipReason {
    /// The session did not contain enough tool calls to justify distillation.
    BelowThreshold,
    /// The session failed, so it must not seed a reusable skill.
    Failure,
    /// The assessed segment outcome is not learnable enough to seed a reusable skill.
    UnlearnableOutcome,
}

/// Typed outcome of one skill-distillation attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum DistillationOutcome {
    /// A new skill was proposed and persisted when a learning store was available.
    NewSkillProposed {
        /// Metadata for the proposed skill.
        skill: SkillMetadata,
    },
    /// The session was routed to improvement of a similar existing skill.
    ImprovementProposed {
        /// Existing skill selected by similarity routing.
        existing_skill_id: String,
        /// Metadata for the improved skill when an update was accepted.
        skill: Option<SkillMetadata>,
    },
    /// Distillation was intentionally skipped.
    Skipped {
        /// Stable skip reason.
        reason: DistillationSkipReason,
    },
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

/// Distills a successful multi-step session into a reusable workspace skill when appropriate.
pub async fn maybe_distill_skill(
    config: &MoaConfig,
    session: &SessionMeta,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
) -> Result<Option<SkillMetadata>> {
    let learning_store = create_session_store(config).await?;
    maybe_distill_skill_with_learning(config, session, events, model_router, Some(learning_store))
        .await
}

/// Distills a successful session and records learning-log entries when a store is provided.
pub async fn maybe_distill_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<Option<SkillMetadata>> {
    match distill_skill_with_learning(config, session, events, model_router, learning_store).await?
    {
        DistillationOutcome::NewSkillProposed { skill } => Ok(Some(skill)),
        DistillationOutcome::ImprovementProposed { skill, .. } => Ok(skill),
        DistillationOutcome::Skipped { .. } => Ok(None),
    }
}

/// Distills a successful session and returns the exact routing outcome.
pub async fn distill_skill_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    events: &[EventRecord],
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
) -> Result<DistillationOutcome> {
    if count_tool_calls(events) < MIN_TOOL_CALLS_FOR_DISTILLATION {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::BelowThreshold,
        });
    }
    if session_failed(session, events) {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::Failure,
        });
    }

    let task_summary = extract_task_summary(events);
    let existing_skills = if let Some(store) = &learning_store {
        SkillRegistry::new(store.pool().clone())
            .list_for_pipeline(&session.workspace_id)
            .await?
    } else {
        Vec::new()
    };

    if let Some(existing) = find_similar_skill(&task_summary, &existing_skills) {
        let existing_skill_id = existing.name.clone();
        let result = crate::improver::improve_skill_with_learning(
            config,
            session,
            existing,
            events,
            model_router,
            learning_store,
        )
        .await;
        return result.map(|result| DistillationOutcome::ImprovementProposed {
            existing_skill_id,
            skill: match result {
                ImprovementResult::Improved { metadata, .. } => Some(metadata),
                ImprovementResult::Unchanged { .. }
                | ImprovementResult::Rejected { .. }
                | ImprovementResult::Skipped => None,
            },
        });
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
    let markdown = render_skill_for_registry(&skill)?;
    let scope = moa_core::MemoryScope::Workspace {
        workspace_id: session.workspace_id.clone(),
    };
    generate_skill_test_suite(config, session, &skill, events).await?;

    if let Some(store) = learning_store {
        let registry = SkillRegistry::new(store.pool().clone());
        registry
            .upsert_by_name(NewSkill::from_skill_markdown(
                scope.clone(),
                markdown.clone(),
            ))
            .await?;
        append_skill_learning(
            store.as_ref(),
            session,
            "skill_created",
            &skill_metadata_from_document(path.clone(), &skill),
            serde_json::json!({
                "path": path.clone(),
                "name": skill.frontmatter.name.clone(),
                "description": skill.frontmatter.description.clone(),
            }),
        )
        .await?;
    }

    let metadata = skill_metadata_from_document(path, &skill);
    Ok(DistillationOutcome::NewSkillProposed { skill: metadata })
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
    if count_tool_calls(&input.events) < MIN_TOOL_CALLS_FOR_DISTILLATION {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::BelowThreshold,
        });
    }

    let task_summary = experience_similarity_text(&input.experience);
    let existing_skills = if let Some(store) = &learning_store {
        SkillRegistry::new(store.pool().clone())
            .list_for_pipeline(&session.workspace_id)
            .await?
    } else {
        Vec::new()
    };

    if let Some(existing) = find_similar_skill(&task_summary, &existing_skills) {
        let existing_skill_id = existing.name.clone();
        let result = crate::improver::improve_skill_with_learning(
            config,
            session,
            existing,
            &input.events,
            model_router,
            learning_store,
        )
        .await;
        return result.map(|result| DistillationOutcome::ImprovementProposed {
            existing_skill_id,
            skill: match result {
                ImprovementResult::Improved { metadata, .. } => Some(metadata),
                ImprovementResult::Unchanged { .. }
                | ImprovementResult::Rejected { .. }
                | ImprovementResult::Skipped => None,
            },
        });
    }

    let llm = model_router.provider_for(ModelTask::SkillDistillation);
    let prompt = build_experience_distillation_prompt(&input);
    let response = llm
        .complete(CompletionRequest::simple(prompt))
        .await?
        .collect()
        .await?;
    let skill_markdown = normalize_llm_markdown(&response.text);
    let mut skill = parse_skill_markdown(skill_markdown)?;
    normalize_new_skill(session, &mut skill);
    let path = build_skill_path(&skill.frontmatter.name);
    let markdown = render_skill_for_registry(&skill)?;
    let metadata = skill_metadata_from_document(path.clone(), &skill);
    let scope = moa_core::MemoryScope::Workspace {
        workspace_id: session.workspace_id.clone(),
    };

    if let Some(store) = learning_store {
        let now = Utc::now();
        let candidate = crate::candidates::skill_creation_candidate(
            session,
            &input.experience,
            &metadata,
            &markdown,
            now,
        );
        store.append_learning_candidate(&candidate).await?;
        store
            .update_learning_candidate_status(&crate::candidates::candidate_status_update(
                candidate.id,
                LearningCandidateStatus::Evaluating,
                "generated skill markdown; running generation gates",
                None,
                Utc::now(),
            ))
            .await?;
        generate_skill_test_suite(config, session, &skill, &input.events).await?;
        let registry = SkillRegistry::new(store.pool().clone());
        registry
            .upsert_by_name(NewSkill::from_skill_markdown(
                scope.clone(),
                markdown.clone(),
            ))
            .await?;
        store
            .update_learning_candidate_status(&crate::candidates::candidate_status_update(
                candidate.id,
                LearningCandidateStatus::Promoted,
                "skill package promoted after generation gates",
                Some(serde_json::json!({
                    "skill_name": metadata.name,
                    "source_experience_ids": candidate.source_experience_ids,
                })),
                Utc::now(),
            ))
            .await?;
        append_skill_learning_with_sources(
            store.as_ref(),
            session,
            "skill_created",
            &metadata,
            serde_json::json!({
                "path": path.clone(),
                "name": skill.frontmatter.name.clone(),
                "description": skill.frontmatter.description.clone(),
                "candidate_id": candidate.id,
                "source_experience_ids": candidate.source_experience_ids,
                "task_fingerprint": input.experience.task_fingerprint.hash,
            }),
            vec![session.id.0, input.experience.id],
        )
        .await?;
    }

    Ok(DistillationOutcome::NewSkillProposed { skill: metadata })
}

fn render_skill_for_registry(skill: &SkillDocument) -> Result<String> {
    crate::format::render_skill_markdown(skill)
}

pub(crate) async fn append_skill_learning(
    store: &PostgresSessionStore,
    session: &SessionMeta,
    learning_type: &str,
    skill: &SkillMetadata,
    payload: serde_json::Value,
) -> Result<()> {
    append_skill_learning_with_sources(
        store,
        session,
        learning_type,
        skill,
        payload,
        vec![session.id.0],
    )
    .await
}

pub(crate) async fn append_skill_learning_with_sources(
    store: &PostgresSessionStore,
    session: &SessionMeta,
    learning_type: &str,
    skill: &SkillMetadata,
    payload: serde_json::Value,
    source_refs: Vec<uuid::Uuid>,
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
            source_refs,
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

fn session_failed(session: &SessionMeta, events: &[EventRecord]) -> bool {
    session.status == moa_core::SessionStatus::Failed
        || events
            .iter()
            .any(|record| matches!(record.event, Event::ToolError { .. } | Event::Error { .. }))
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

fn build_experience_distillation_prompt(input: &ExperienceDistillationInput) -> String {
    let experience = &input.experience;
    format!(
        "Distill the following assessed MOA task experience into a reusable Agent Skill.\n\
         Output only a complete SKILL.md document using the Agent Skills format from agentskills.io.\n\
         Use top-level frontmatter fields such as `name`, `description`, optional `compatibility`, \
         optional `allowed-tools`, and a `metadata` map for MOA-specific bookkeeping.\n\
         Store MOA-specific fields inside `metadata` using `moa-` prefixes, including at least \
         `moa-version`, `moa-one-liner`, `moa-tags`, and `moa-estimated-tokens`.\n\
         Learn only durable workflow structure. Do not copy secrets, transient IDs, or one-off paths unless \
         the path is essential to the workflow.\n\n\
         Task summary: {}\n\
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

fn experience_is_learnable(
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
    skill
        .frontmatter
        .set_derived_from_session(Some(session.id.to_string()));
    skill.frontmatter.set_updated(now);
    record_successful_use(skill, now);
    if skill.frontmatter.use_count() == 0 {
        skill.frontmatter.set_use_count(1);
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{
        ExperienceRecord, SegmentEvidence, SegmentEvidenceKind, SegmentEvidencePolarity, SegmentId,
        SessionId, TaskFacetSet, TaskFingerprint, UserId, WorkspaceId,
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

    fn experience(outcome: SegmentOutcome, confidence: f64) -> ExperienceRecord {
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        ExperienceRecord {
            id: Uuid::now_v7(),
            segment_id: SegmentId::new(),
            session_id: SessionId::new(),
            tenant_id: "tenant".to_string(),
            workspace_id: WorkspaceId::new("workspace"),
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
            tools_used: vec!["bash".to_string(); MIN_TOOL_CALLS_FOR_DISTILLATION],
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
            tenant_id: experience.tenant_id.clone(),
            workspace_id: experience.workspace_id.clone(),
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

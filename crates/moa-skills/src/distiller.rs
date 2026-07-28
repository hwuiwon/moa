//! Skill distillation from successful agent runs.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_artifacts::registry::ArtifactRegistry;
use moa_config::MoaConfig;
use moa_core::{
    error::Result, traits::EmbeddingProvider, types::completion::CompletionRequest,
    types::experience::AttributionEffect, types::experience::AttributionSubjectType,
    types::experience::ExperienceAttribution, types::experience::ExperienceRecord,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::memory::SkillMetadata, types::provider::ModelTask,
    types::segment_assessment::SegmentEvidenceKind,
    types::segment_assessment::SegmentEvidencePolarity, types::segment_assessment::SegmentOutcome,
    types::session::SessionMeta,
};
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;

use crate::evidence::{EvidenceSource, SanitizedLearningEvidence};
use crate::semantic::{EmbeddingSkillMatch, route_improve_vs_create};

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
};
use crate::improver::{ImprovementResult, format_evidence_for_learning, normalize_llm_markdown};
use crate::package::SkillPackage;
use crate::proposals::{
    SiblingResynthesis, SkillDraftProposal, SkillProposalOperation, SkillProposalSource,
    store_skill_draft_proposal,
};
use crate::registry::SkillRegistry;
use crate::regression::{
    generate_skill_test_suite_source, generate_skill_test_suite_source_for_name,
};

/// Similarity score at or above which distillation routes to existing-skill improvement.
pub(crate) const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Why one distillation pass was dispatched, and the evidence bar it carries.
///
/// A single-session dispatch stands on the per-session gate alone, so it keeps
/// the configured `skills.min_tool_calls` floor. A recurrence-triggered dispatch
/// replaces that per-session bar with N-fold recurrence, so its exemplar only
/// needs the relaxed floor and its proposal carries the recurrence evidence for
/// reviewers. Threading this explicitly — rather than lowering the config floor
/// — keeps single-session behavior identical while the cron path relaxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchEvidence {
    /// The per-session dispatch gate is the sole evidence; the config floor holds.
    SingleSession,
    /// Recurrence across sessions is the evidence; the relaxed floor applies.
    Recurrence(RecurrenceEvidence),
}

/// Recurrence evidence attached to a recurrence-triggered dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceEvidence {
    /// Total resolved/partial occurrences of the fingerprint in the window.
    pub occurrences: usize,
    /// Every cluster member experience id (exemplar plus siblings), for review.
    pub member_experience_ids: Vec<uuid::Uuid>,
    /// Every exact task fingerprint merged into this cluster. More than one entry
    /// means semantic clustering pooled "same loop, different wording" groups.
    pub merged_fingerprints: Vec<String>,
    /// Earliest observed occurrence in the cluster.
    pub first_seen: DateTime<Utc>,
    /// Latest observed occurrence in the cluster.
    pub last_seen: DateTime<Utc>,
    /// Relaxed per-session tool-call floor for the exemplar.
    pub relaxed_min_tool_calls: usize,
}

impl DispatchEvidence {
    /// Returns the effective per-session tool-call floor for this dispatch.
    ///
    /// Single-session dispatch keeps `config_floor`; recurrence dispatch relaxes
    /// to its exemplar floor because the recurrence count is the evidence the
    /// per-session floor was standing in for.
    #[must_use]
    pub fn effective_min_tool_calls(&self, config_floor: usize) -> usize {
        match self {
            Self::SingleSession => config_floor,
            Self::Recurrence(evidence) => evidence.relaxed_min_tool_calls,
        }
    }

    /// Returns the recurrence evidence payload block, or `None` for single-session.
    fn evidence_block(&self) -> Option<serde_json::Value> {
        match self {
            Self::SingleSession => None,
            Self::Recurrence(evidence) => Some(serde_json::json!({
                "source": "recurrence_mined",
                "occurrences": evidence.occurrences,
                "member_experience_ids": evidence
                    .member_experience_ids
                    .iter()
                    .map(uuid::Uuid::to_string)
                    .collect::<Vec<_>>(),
                "merged_fingerprints": evidence.merged_fingerprints,
                "first_seen": evidence.first_seen,
                "last_seen": evidence.last_seen,
                "relaxed_min_tool_calls": evidence.relaxed_min_tool_calls,
            })),
        }
    }
}

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
    /// A recurring sibling experience deduped onto an already-open proposal.
    ///
    /// No new review candidate was filed: the sibling's evidence accumulated
    /// onto the open candidate, and `resynthesis` records whether the
    /// generalization pass rewrote the open draft. This is distinct from
    /// [`DistillationOutcome::NewSkillProposed`]/[`DistillationOutcome::ImprovementProposed`]
    /// so loop observability can count a changed re-synthesis apart from a fresh
    /// filing.
    DedupedOntoOpenProposal {
        /// The open proposal the sibling deduped onto.
        proposal: SkillDraftProposal,
        /// Whether the sibling's generalization pass rewrote the open draft.
        resynthesis: SiblingResynthesis,
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
        }
        | DistillationOutcome::DedupedOntoOpenProposal { proposal, .. } => {
            SkillProposalGeneration::Proposed {
                candidate_id: proposal.candidate_id,
                draft_artifact_revision_uid: proposal.draft_artifact_revision_uid,
            }
        }
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
    /// Sanitized bounded segment evidence used for learning.
    pub evidence: SanitizedLearningEvidence,
}

/// Distills an assessed experience into a skill candidate and promotes it when gates pass.
///
/// `evidence` names why this pass was dispatched and sets the per-session
/// tool-call floor: [`DispatchEvidence::SingleSession`] keeps the configured
/// floor, while [`DispatchEvidence::Recurrence`] relaxes it and rides its
/// recurrence evidence into the reviewer-facing proposal payload.
///
/// `embedder`, when present, activates the semantic (R2) layer: the task summary
/// is embedded once and reused to (1) route improve-vs-create by nearest published
/// skill embedding, replacing token Jaccard as the primary signal, and (2) dedup a
/// near-duplicate of an open proposal into a sibling instead of a parallel draft.
/// A missing embedder, a failed embed, or a dimension mismatch skips the semantic
/// layer entirely and the lexical Jaccard behavior stands in unchanged.
pub async fn distill_skill_from_experience_with_learning(
    config: &MoaConfig,
    session: &SessionMeta,
    input: ExperienceDistillationInput,
    model_router: Arc<ModelRouter>,
    learning_store: Option<Arc<PostgresSessionStore>>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    evidence: &DispatchEvidence,
) -> Result<DistillationOutcome> {
    if !experience_is_learnable(&input.experience, &input.attributions) {
        return Ok(DistillationOutcome::Skipped {
            reason: DistillationSkipReason::UnlearnableOutcome,
        });
    }
    if input.evidence.tool_call_count()
        < evidence.effective_min_tool_calls(config.learning.skills.min_tool_calls)
    {
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
            &input.evidence,
        )?;
        let resynthesis = crate::proposals::accumulate_sibling_and_resynthesize(
            store.as_ref(),
            model_router.as_ref(),
            session.tenant_id,
            &open,
            crate::proposals::SiblingContribution {
                suite: sibling_suite,
                evidence: &input.evidence,
                source_experience_id: input.experience.id,
                source_session_id: session.id,
            },
        )
        .await?;
        return Ok(DistillationOutcome::DedupedOntoOpenProposal {
            proposal: open,
            resynthesis,
        });
    }

    let jaccard_text = experience_similarity_text(&input.experience);
    let existing_skills = SkillRegistry::new(store.pool().clone())
        .list_for_pipeline(session.tenant_id)
        .await?;

    // Semantic (R2) layer: embed the task summary once and reuse it for the
    // improve-vs-create routing and the open-proposal dedup below. A missing or
    // failing embedder leaves `probe` None, and every semantic branch degrades to
    // today's lexical behavior.
    let probe = match &embedder {
        Some(embedder) => embed_task_summary_probe(embedder.as_ref(), &input.evidence).await,
        None => None,
    };
    // Constrain both filing-time NN probes to the active embedder's own vector
    // space, so a probe is never ranked against vectors a previous embedder wrote
    // (the incompatible-space hazard while the backfill converges older rows).
    let model_scope: Option<(&str, i32)> = embedder
        .as_ref()
        .map(|embedder| (embedder.model_id(), embedder.model_version()));

    let embedding_nearest = match &probe {
        Some(probe) => {
            nearest_skill_match(
                store.as_ref(),
                session.tenant_id,
                probe,
                &existing_skills,
                model_scope,
            )
            .await?
        }
        None => None,
    };
    let jaccard_match = find_similar_skill(&jaccard_text, &existing_skills)
        .map(|(score, skill)| (skill.name.clone(), f64::from(score)));
    let decision = route_improve_vs_create(
        embedding_nearest,
        jaccard_match,
        config.learning.skills.improve_route_similarity,
    );

    if let Some(skill_name) = decision.improve_skill.clone()
        && let Some(existing) = existing_skills
            .iter()
            .find(|skill| skill.name == skill_name)
    {
        let existing_skill_id = existing.name.clone();
        let routing = serde_json::json!({
            "decision": "improve_existing",
            "matched_skill": existing_skill_id.clone(),
            "similarity_score": decision.similarity,
            "similarity_method": decision.method.as_str(),
            "improve_route_similarity": config.learning.skills.improve_route_similarity,
            "jaccard_threshold": SIMILARITY_THRESHOLD,
        });
        let source = proposal_source_from_experience(&input, Some(routing), evidence);
        let result = crate::improver::improve_skill_with_learning_for_sources(
            session,
            existing,
            &input.evidence,
            model_router,
            learning_store,
            source,
        )
        .await;
        return result.map(|result| match result {
            ImprovementResult::Deduped {
                proposal,
                resynthesis,
            } => DistillationOutcome::DedupedOntoOpenProposal {
                proposal,
                resynthesis,
            },
            ImprovementResult::Improved { proposal, .. } => {
                DistillationOutcome::ImprovementProposed {
                    existing_skill_id,
                    proposal: Some(proposal),
                }
            }
            ImprovementResult::Unchanged { .. }
            | ImprovementResult::Rejected { .. }
            | ImprovementResult::Skipped => DistillationOutcome::ImprovementProposed {
                existing_skill_id,
                proposal: None,
            },
        });
    }

    // Create branch. Before filing a fresh draft, dedup semantically against open
    // proposals: a near-duplicate of work already in review accumulates as a
    // sibling on that candidate instead of spawning a parallel near-identical
    // draft. Only runs when the semantic probe is available.
    if let Some(probe) = &probe
        && let Some(outcome) = semantic_dedupe_onto_open_proposal(
            store.as_ref(),
            model_router.as_ref(),
            session,
            &input,
            probe,
            config.learning.skills.proposal_dedup_similarity,
            model_scope,
        )
        .await?
    {
        return Ok(outcome);
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
        generate_skill_test_suite_source(session.tenant_id, &skill, &input.evidence)?;
    let routing = serde_json::json!({
        "decision": "create_new",
        "similarity_method": decision.method.as_str(),
        "improve_route_similarity": config.learning.skills.improve_route_similarity,
        "jaccard_threshold": SIMILARITY_THRESHOLD,
        "reason": "no existing tenant skill matched the task above the improve threshold",
    });
    let proposal = store_skill_draft_proposal(
        store.as_ref(),
        session,
        &package,
        metadata,
        SkillProposalOperation::Created,
        proposal_source_from_experience(&input, Some(routing), evidence),
        generated_suite,
    )
    .await?;

    Ok(DistillationOutcome::NewSkillProposed { proposal })
}

/// Builds the proposal source (lineage + reviewer evidence) for one experience.
pub(crate) fn proposal_source_from_experience(
    input: &ExperienceDistillationInput,
    routing: Option<serde_json::Value>,
    evidence: &DispatchEvidence,
) -> SkillProposalSource {
    SkillProposalSource {
        // The complete closure the sanitized evidence actually saw: contact,
        // session, segment, experience, and the source events. Taking it from
        // the evidence rather than rebuilding it from the experience record
        // keeps the proposal's provenance identical to the transcript the model
        // was shown.
        sources: input.evidence.candidate_sources(),
        task_fingerprint: Some(input.experience.task_fingerprint.clone()),
        task_facets: Some(input.experience.task_facets.clone()),
        confidence: Some(input.experience.confidence),
        evidence: Some(experience_evidence_payload(input, routing, evidence)),
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
    evidence: &DispatchEvidence,
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
        "recurrence": evidence.evidence_block(),
    })
}

fn render_skill_for_registry(skill: &SkillDocument) -> Result<String> {
    crate::format::render_skill_markdown(skill)
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
        format_evidence_for_learning(&input.evidence)
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

/// Nearest-neighbor breadth scanned for an open-proposal dedupe-hit.
///
/// The dedup only cares whether *some* source experience of an open proposal is a
/// near-duplicate, so a small breadth suffices: past the closest handful, a
/// neighbor is already too far to clear the dedup ceiling. Kept bounded so the
/// detached pass never sweeps the whole tenant.
const PROPOSAL_DEDUP_NEIGHBOR_LIMIT: usize = 20;

/// Embeds the experience's sanitized task summary into a reusable semantic probe.
///
/// The probe is a provider call, so it embeds the sanitized summary carried by
/// the evidence rather than the raw stored one. The embedding backfill sanitizes
/// the same way, so a probe and the stored vectors it is compared against are
/// derived from identical text.
///
/// Returns `None` (skipping the semantic layer) when the embedder's dimension
/// disagrees with the stored `halfvec` width, when the evidence carries no task
/// summary, or when the provider call fails — each logs and degrades to the
/// lexical path rather than failing the distillation.
async fn embed_task_summary_probe(
    embedder: &dyn EmbeddingProvider,
    evidence: &SanitizedLearningEvidence,
) -> Option<Vec<f32>> {
    if embedder.dimensions() != crate::embeddings::EMBEDDING_DIM {
        tracing::warn!(
            configured = embedder.dimensions(),
            expected = crate::embeddings::EMBEDDING_DIM,
            "skill-learning embedder dimension mismatch; skipping semantic routing"
        );
        return None;
    }
    let text = evidence
        .entries_from(EvidenceSource::TaskSummary)
        .next()
        .map(|entry| entry.text().to_string())
        .unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }
    match embedder.embed(std::slice::from_ref(&text)).await {
        Ok(mut vectors) => vectors.pop(),
        Err(error) => {
            tracing::warn!(
                %error,
                "skill-learning probe embedding failed; skipping semantic routing"
            );
            None
        }
    }
}

/// Resolves the nearest published skill embedding to an improvable skill match.
///
/// Runs the skill-identity NN within the tenant's storage partition, resolves the
/// nearest artifact to its current skill name, and confirms that skill is present
/// in the tenant's live pipeline set (so the improver can load it). Any break in
/// that chain yields `None`, so the router falls back to the lexical signal rather
/// than routing to a skill it cannot improve.
async fn nearest_skill_match(
    store: &PostgresSessionStore,
    tenant_id: TenantId,
    probe: &[f32],
    existing_skills: &[SkillMetadata],
    model_scope: Option<(&str, i32)>,
) -> Result<Option<EmbeddingSkillMatch>> {
    let registry = ArtifactRegistry::new(store.pool().clone());
    let partition = StoragePartitionId::for_tenant(tenant_id);
    let Some(nearest) = registry
        .nearest_skill_embeddings_scoped(partition.as_str(), probe, 1, None, model_scope)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let Some(name) = registry
        .published_skill_name_for_artifact(nearest.artifact_uid)
        .await?
    else {
        return Ok(None);
    };
    if !existing_skills.iter().any(|skill| skill.name == name) {
        return Ok(None);
    }
    Ok(Some(EmbeddingSkillMatch {
        skill_name: name,
        distance: nearest.distance,
    }))
}

/// Accumulates a create-branch experience onto a semantically-duplicate open
/// proposal, or returns `None` when there is no dedupe-hit.
///
/// Probes the tenant's experience embeddings for near neighbors of the current
/// experience, maps the nearest qualifying neighbor to the open proposal it backs,
/// and routes the experience through the same sibling-accumulation/re-synthesis
/// path the exact-fingerprint dedupe uses. The suite is generated for the open
/// proposal's own skill name, since the match was by task similarity, not name.
async fn semantic_dedupe_onto_open_proposal(
    store: &PostgresSessionStore,
    model_router: &ModelRouter,
    session: &SessionMeta,
    input: &ExperienceDistillationInput,
    probe: &[f32],
    proposal_dedup_similarity: f64,
    model_scope: Option<(&str, i32)>,
) -> Result<Option<DistillationOutcome>> {
    let neighbors = store
        .nearest_experience_task_embeddings_scoped(
            &session.tenant_id,
            probe,
            PROPOSAL_DEDUP_NEIGHBOR_LIMIT,
            Some(input.experience.id),
            model_scope,
        )
        .await?;
    if neighbors.is_empty() {
        return Ok(None);
    }
    let open_sources = store
        .list_open_skill_proposal_sources(&session.tenant_id)
        .await?;
    let Some(candidate_id) = crate::semantic::select_proposal_dedupe_hit(
        &neighbors,
        &open_sources,
        proposal_dedup_similarity,
    ) else {
        return Ok(None);
    };
    let Some(open) =
        crate::proposals::load_open_skill_proposal(store, session.tenant_id, candidate_id).await?
    else {
        return Ok(None);
    };
    let suite = generate_skill_test_suite_source_for_name(
        session.tenant_id,
        &open.metadata.name,
        &input.evidence,
    )?;
    let resynthesis = crate::proposals::accumulate_sibling_and_resynthesize(
        store,
        model_router,
        session.tenant_id,
        &open,
        crate::proposals::SiblingContribution {
            suite,
            evidence: &input.evidence,
            source_experience_id: input.experience.id,
            source_session_id: session.id,
        },
    )
    .await?;
    Ok(Some(DistillationOutcome::DedupedOntoOpenProposal {
        proposal: open,
        resynthesis,
    }))
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
    fn single_session_dispatch_keeps_the_config_floor_recurrence_relaxes_it() {
        // Pins: single-session dispatch always uses the configured min_tool_calls
        // (8 by default), unchanged by recurrence; a recurrence dispatch relaxes to
        // its own exemplar floor so N-fold recurrence stands in for tool-call depth.
        let config_floor = MoaConfig::default().learning.skills.min_tool_calls;
        assert_eq!(config_floor, 8);
        assert_eq!(
            DispatchEvidence::SingleSession.effective_min_tool_calls(config_floor),
            8,
            "single-session dispatch must keep the config floor"
        );
        let recurrence = DispatchEvidence::Recurrence(RecurrenceEvidence {
            occurrences: 4,
            member_experience_ids: vec![Uuid::now_v7(), Uuid::now_v7()],
            merged_fingerprints: vec!["fp".to_string()],
            first_seen: Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            last_seen: Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap(),
            relaxed_min_tool_calls: 3,
        });
        assert_eq!(
            recurrence.effective_min_tool_calls(config_floor),
            3,
            "recurrence dispatch relaxes to its exemplar floor"
        );
    }

    #[test]
    fn recurrence_evidence_block_rides_the_reviewer_payload() {
        // Pins: a recurrence dispatch stamps the reviewer payload with cluster size,
        // member ids, and time span; a single-session dispatch carries no block.
        assert!(DispatchEvidence::SingleSession.evidence_block().is_none());
        let ids = vec![Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        let block = DispatchEvidence::Recurrence(RecurrenceEvidence {
            occurrences: 3,
            member_experience_ids: ids.clone(),
            merged_fingerprints: vec!["fp-a".to_string(), "fp-b".to_string()],
            first_seen: Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            last_seen: Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap(),
            relaxed_min_tool_calls: 3,
        })
        .evidence_block()
        .expect("recurrence carries an evidence block");
        assert_eq!(block["source"], "recurrence_mined");
        assert_eq!(block["occurrences"], 3);
        assert_eq!(
            block["merged_fingerprints"]
                .as_array()
                .expect("merged fingerprints array")
                .len(),
            2
        );
        assert_eq!(
            block["member_experience_ids"]
                .as_array()
                .expect("member ids array")
                .len(),
            3
        );
    }

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

    #[tokio::test]
    async fn experience_distillation_request_keeps_segment_evidence_out_of_system_prompt() {
        // Pins: learned-skill generation reuses static instructions while segment evidence
        // stays dynamic, so the stable system prompt remains cacheable.
        let input = ExperienceDistillationInput {
            experience: experience(SegmentOutcome::Resolved, 0.9),
            attributions: Vec::new(),
            evidence: crate::evidence::sanitize_for_tests(&[]).await,
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
            skills_used: Vec::new(),
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
            kind: moa_core::types::experience::AttributionKind::Standard,
            confidence: experience.confidence,
            evidence: Vec::new(),
            created_at: experience.created_at,
        }
    }
}

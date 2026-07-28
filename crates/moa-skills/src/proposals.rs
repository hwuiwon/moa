//! Draft artifact proposal storage for self-generated skill packages.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewSuiteContribution, SuiteContributionKind,
};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateStatus, types::experience::TaskFacetSet,
    types::experience::TaskFingerprint, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::memory::SkillMetadata, types::provider::ModelTask, types::session::SessionMeta,
};
use moa_db::ScopedConn;
use moa_eval_core::TestSuite;
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::artifact::{
    artifact_file_from_skill_file, skill_artifact_document_from_package, skill_artifact_source_text,
};
use crate::candidates::{
    SkillDraftCandidateInput, deterministic_skill_candidate_id, experience_ids,
    skill_draft_candidate,
};
use crate::distiller::DistillationSkipReason;
use crate::evidence::SanitizedLearningEvidence;
use crate::format::{
    build_skill_path, parse_skill_markdown, render_skill_markdown, skill_metadata_from_document,
};
use crate::improver::{format_evidence_for_learning, normalize_llm_markdown};
use crate::package::{SkillPackage, ValidatedSkillPackage};
use crate::regression::GeneratedSkillSuite;
use crate::util::map_sqlx_error;

/// Editable surface a self-improvement proposal may target.
///
/// The self-improvement loop may only mutate these four surfaces. The enum is
/// closed, so authz rules, action-policy definitions, audit configuration, eval
/// suite definitions, and budget gates are *structurally* non-proposable: they
/// are not variants, so no proposal that targets them can be constructed. Filing
/// therefore rejects anything outside this set by construction rather than by a
/// runtime allowlist check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditableSurface {
    /// A skill package's markdown body.
    SkillMarkdown,
    /// A query-rewrite prompt version.
    RewritePromptVersion,
    /// The retrieval router rule table.
    RouterRules,
    /// Retrieval ranking configuration values.
    RankingConfig,
}

impl EditableSurface {
    /// Returns the stable snake_case wire label for this surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SkillMarkdown => "skill_markdown",
            Self::RewritePromptVersion => "rewrite_prompt_version",
            Self::RouterRules => "router_rules",
            Self::RankingConfig => "ranking_config",
        }
    }
}

/// Reviewable draft proposal generated from skill self-learning.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDraftProposal {
    /// Deterministic learning-candidate identifier for this proposed change.
    pub candidate_id: Uuid,
    /// Draft artifact revision containing the generated skill package.
    pub draft_artifact_revision_uid: Uuid,
    /// Tier-one metadata for the generated skill package.
    pub metadata: SkillMetadata,
    /// Creation or improvement operation represented by the draft.
    pub operation: SkillProposalOperation,
    /// Editable surface this proposal targets; always [`EditableSurface::SkillMarkdown`].
    pub surface: EditableSurface,
}

/// Operation represented by a generated skill proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillProposalOperation {
    /// Proposed creation of a new skill artifact.
    Created,
    /// Proposed improvement of an existing active skill artifact.
    Improved {
        /// Semantic version of the active skill used as the improvement baseline.
        previous_version: String,
    },
}

/// Whether a sibling dedupe-hit rewrote the open proposal's draft.
///
/// Returned by [`accumulate_sibling_and_resynthesize`] so callers can tell a
/// recurring experience that generalized (rewrote) the open draft apart from one
/// that only accumulated held-out material. A changed re-synthesis is a distinct
/// filed candidate for loop observability; an unchanged one filed nothing new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingResynthesis {
    /// A generalization pass rewrote the open proposal's draft revision.
    DraftRewritten,
    /// The draft was kept as-is: no accepted sibling, an `UNCHANGED` pass, a
    /// rejected pass, a capped or claimed candidate, or a swallowed model error.
    DraftUnchanged,
}

/// Outcome of generating or attempting to generate a skill proposal.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillProposalOutcome {
    /// A reviewable draft proposal was stored.
    Proposed(SkillDraftProposal),
    /// The generator determined that no change was needed.
    Unchanged {
        /// Metadata for the unchanged active skill.
        metadata: SkillMetadata,
    },
    /// The generator output could not be accepted as a draft proposal.
    RejectedByGeneration {
        /// Human-readable reason the generated output was rejected.
        reason: String,
    },
    /// Proposal generation was intentionally skipped.
    Skipped {
        /// Stable skip reason.
        reason: DistillationSkipReason,
    },
}

pub(crate) struct SkillProposalSource {
    /// Typed provenance the proposal stands on, drawn from sanitized evidence.
    pub sources: Vec<LearningCandidateSourceRef>,
    pub task_fingerprint: Option<TaskFingerprint>,
    pub task_facets: Option<TaskFacetSet>,
    pub confidence: Option<f64>,
    /// Reviewer-facing rationale: assessed outcome, segment evidence,
    /// attributions, and similarity routing behind this proposal.
    pub evidence: Option<serde_json::Value>,
}

/// Finds an open `Proposed` skill draft by name or task fingerprint before generation.
///
/// Generators call this ahead of any LLM spend: an open proposal for the same
/// skill or task means the model call would produce a duplicate that the
/// in-transaction dedupe in [`store_skill_draft_proposal`] discards anyway.
/// That in-transaction check remains the race-safe backstop; this lookup only
/// avoids the wasted generation cost. Returns `None` when the stored candidate
/// payload cannot be interpreted, so generation proceeds instead of failing.
pub(crate) async fn find_open_skill_proposal(
    store: &PostgresSessionStore,
    tenant_id: moa_core::types::identifiers::TenantId,
    skill_name: Option<&str>,
    fingerprint_hash: Option<&str>,
) -> Result<Option<SkillDraftProposal>> {
    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(tenant_id)).await?;
    let mut found = None;
    if let Some(name) = skill_name {
        found = store
            .find_proposed_learning_candidate_by_target_with_conn(
                conn.as_mut(),
                &tenant_id,
                moa_core::types::experience::LearningCandidateType::Skill,
                name,
            )
            .await?;
    }
    if let (None, Some(hash)) = (&found, fingerprint_hash) {
        found = store
            .find_proposed_learning_candidate_by_fingerprint_with_conn(
                conn.as_mut(),
                &tenant_id,
                moa_core::types::experience::LearningCandidateType::Skill,
                hash,
            )
            .await?;
    }
    conn.commit().await?;
    Ok(found.and_then(proposal_from_open_candidate))
}

/// Loads an open skill candidate by id and interprets it as a draft proposal.
///
/// The filing-time semantic dedup resolves a dedupe-hit to a candidate id, then
/// needs the open proposal to accumulate the new experience as a sibling. Returns
/// `None` when the candidate is gone or its payload cannot be interpreted, so the
/// caller falls back to filing a fresh draft.
pub(crate) async fn load_open_skill_proposal(
    store: &PostgresSessionStore,
    tenant_id: TenantId,
    candidate_id: Uuid,
) -> Result<Option<SkillDraftProposal>> {
    Ok(store
        .get_learning_candidate(&tenant_id, candidate_id)
        .await?
        .filter(|candidate| candidate.status == LearningCandidateStatus::Proposed)
        .and_then(proposal_from_open_candidate))
}

/// Interprets an open candidate row as a draft proposal, or `None` when its
/// payload lacks the required fields.
fn proposal_from_open_candidate(candidate: LearningCandidate) -> Option<SkillDraftProposal> {
    let operation = operation_from_payload(&candidate.payload)?;
    let draft_artifact_revision_uid = candidate
        .payload
        .get("draft_artifact_revision_uid")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())?;
    let metadata = candidate
        .payload
        .get("skill_metadata")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())?;
    Some(SkillDraftProposal {
        candidate_id: candidate.id,
        draft_artifact_revision_uid,
        metadata,
        operation,
        surface: EditableSurface::SkillMarkdown,
    })
}

pub(crate) async fn store_skill_draft_proposal(
    store: &PostgresSessionStore,
    session: &SessionMeta,
    package: &ValidatedSkillPackage,
    metadata: SkillMetadata,
    operation: SkillProposalOperation,
    source: SkillProposalSource,
    generated_suite: GeneratedSkillSuite,
) -> Result<SkillDraftProposal> {
    let operation_label = operation.payload_operation();
    let candidate_id = deterministic_skill_candidate_id(
        session.tenant_id,
        session.id,
        &experience_ids(&source.sources),
        operation_label,
        &metadata.name,
    );

    // The generated suite rides the draft package, so every promoted revision
    // carries the suite derived from its own source session and the review
    // gate can pool previous revisions' suites as held-out material.
    let package = package_with_regression_suite(package, &generated_suite)?;
    let package = &package;
    let document = skill_artifact_document_from_package(package, ArtifactStatus::Draft)?;
    let source_text = skill_artifact_source_text(package, &document)?;
    let artifact_files = package
        .files
        .iter()
        .map(artifact_file_from_skill_file)
        .collect::<Vec<_>>();
    let scope = ActionRuleScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(session.tenant_id)).await?;
    // The lock is keyed by (tenant, skill name), not candidate id, so concurrent
    // sessions proposing the same skill serialize here and the open-proposal
    // dedup below observes the winner's insert.
    acquire_proposal_advisory_lock(conn.as_mut(), session.tenant_id, &metadata.name).await?;

    if let Some(existing) = store
        .get_learning_candidate_with_conn(conn.as_mut(), &session.tenant_id, candidate_id)
        .await?
    {
        let proposal = proposal_from_existing(existing, metadata, operation)?;
        conn.commit().await?;
        return Ok(proposal);
    }
    // One open review item per skill per tenant: a busy tenant re-running the
    // same task must bump reviewers to the existing proposal, not flood the
    // queue with near-identical drafts. The duplicate's session still
    // contributes: its generated suite accumulates onto the open candidate as
    // held-out material for the review gate.
    if let Some(open) = store
        .find_proposed_learning_candidate_by_target_with_conn(
            conn.as_mut(),
            &session.tenant_id,
            moa_core::types::experience::LearningCandidateType::Skill,
            &metadata.name,
        )
        .await?
    {
        if let Some(source_experience_id) = experience_ids(&source.sources).first().copied() {
            accumulate_sibling_suite_in_tx(
                store,
                conn.as_mut(),
                open.clone(),
                &generated_suite,
                source_experience_id,
                session.id,
            )
            .await?;
        }
        let operation = operation_from_payload(&open.payload).unwrap_or(operation);
        let proposal = proposal_from_existing(open, metadata, operation)?;
        conn.commit().await?;
        return Ok(proposal);
    }
    // The generator may name the same recurring work differently across
    // sessions, so an open proposal for the same task fingerprint also
    // dedupes even when the skill name differs. The fingerprint lock is
    // always taken after the name lock, so lock ordering is consistent and
    // concurrent different-name same-task proposals serialize here.
    if let Some(fingerprint) = source.task_fingerprint.as_ref() {
        acquire_fingerprint_advisory_lock(conn.as_mut(), session.tenant_id, &fingerprint.hash)
            .await?;
        if let Some(open) = store
            .find_proposed_learning_candidate_by_fingerprint_with_conn(
                conn.as_mut(),
                &session.tenant_id,
                moa_core::types::experience::LearningCandidateType::Skill,
                &fingerprint.hash,
            )
            .await?
        {
            if let Some(source_experience_id) = experience_ids(&source.sources).first().copied() {
                accumulate_sibling_suite_in_tx(
                    store,
                    conn.as_mut(),
                    open.clone(),
                    &generated_suite,
                    source_experience_id,
                    session.id,
                )
                .await?;
            }
            let operation = operation_from_payload(&open.payload).unwrap_or(operation);
            let proposal = proposal_from_existing(open, metadata, operation)?;
            conn.commit().await?;
            return Ok(proposal);
        }
    }

    let stored = ArtifactRegistry::create_draft_in_tx(
        conn.as_mut(),
        &scope,
        NewArtifactDraft {
            document: &document,
            source_format: "yaml",
            source_text: &source_text,
            files: &artifact_files,
        },
    )
    .await?;

    let now = Utc::now();
    let payload = proposal_payload(ProposalPayloadInput {
        candidate_id,
        session,
        package,
        metadata: &metadata,
        operation: &operation,
        source: &source,
        artifact_uid: stored.artifact_uid,
        draft_artifact_revision_uid: stored.revision_uid,
    });
    let source_experience_id = experience_ids(&source.sources).first().copied();
    let candidate = skill_draft_candidate(
        session,
        SkillDraftCandidateInput {
            candidate_id,
            operation: operation_label.to_string(),
            metadata: metadata.clone(),
            payload,
            sources: source.sources,
            task_fingerprint: source.task_fingerprint,
            task_facets: source.task_facets,
            confidence: source.confidence,
            now,
        },
    );
    store
        .append_learning_candidate_with_conn(conn.as_mut(), &candidate)
        .await?;
    // Written after the candidate row exists, because both contribution tables
    // carry a real foreign key to it, and in the same transaction, so a draft
    // revision and the record of whose data produced it cannot come apart.
    record_draft_attribution(
        conn.as_mut(),
        session.tenant_id,
        candidate_id,
        stored.revision_uid,
        &generated_suite,
        session.id,
        source_experience_id,
    )
    .await?;
    conn.commit().await?;

    Ok(SkillDraftProposal {
        candidate_id,
        draft_artifact_revision_uid: stored.revision_uid,
        metadata,
        operation,
        surface: EditableSurface::SkillMarkdown,
    })
}

/// Maximum sibling suites accumulated onto one open proposal.
pub const MAX_ACCUMULATED_SIBLING_SUITES: usize = 3;

/// Returns the package with the generated suite file inserted (replacing any
/// prior revision's suite carried over at the same path).
fn package_with_regression_suite(
    package: &ValidatedSkillPackage,
    generated_suite: &GeneratedSkillSuite,
) -> Result<ValidatedSkillPackage> {
    let mut files = package
        .files
        .iter()
        .filter(|file| file.path != crate::regression::REGRESSION_SUITE_PACKAGE_PATH)
        .map(|file| crate::package::SkillPackageFile {
            path: file.path.clone(),
            content: file.content.clone(),
            content_type: file.content_type.clone(),
            executable: file.executable,
        })
        .collect::<Vec<_>>();
    files.push(
        crate::package::SkillPackageFile::new(
            crate::regression::REGRESSION_SUITE_PACKAGE_PATH,
            generated_suite.source_toml.clone().into_bytes(),
        )
        .with_content_type("application/toml; charset=utf-8"),
    );
    crate::package::SkillPackage::new(files).validate()
}

/// One recurring experience's contribution to an open proposal.
///
/// Bundles the deterministic suite, the segment events, and the source
/// identifiers a dedupe-hit needs so the sibling entry point stays a single
/// argument instead of a long positional list.
pub(crate) struct SiblingContribution<'a> {
    /// Deterministic regression suite generated from the sibling's session.
    pub suite: GeneratedSkillSuite,
    /// Sanitized bounded segment evidence used for generalization.
    pub evidence: &'a SanitizedLearningEvidence,
    /// Experience record that produced this sibling.
    pub source_experience_id: Uuid,
    /// Session that produced this sibling.
    pub source_session_id: SessionId,
}

/// Accumulates a sibling suite, then runs a best-effort generalization pass.
///
/// This is the dedupe-hit entry both the distiller and improver preflights use
/// when a recurring task lands on an open `Proposed` candidate. Ordering is
/// deliberate and load-bearing:
///
/// 1. The sibling suite is accumulated first, in its own committed transaction.
///    The suite is durable held-out material for the review gate and must never
///    be lost to a later, best-effort model call.
/// 2. Re-synthesis runs only when this sibling was *newly accepted* (not a
///    replay of an already-recorded experience, not past the cap, candidate
///    still `Proposed`). Its failures are logged and swallowed so an
///    operational model error never rolls back the accumulation from step 1.
///
/// The return value reports whether the draft was actually rewritten: a
/// swallowed re-synthesis error, an `UNCHANGED`/rejected pass, or a
/// non-accepted sibling all yield [`SiblingResynthesis::DraftUnchanged`].
pub(crate) async fn accumulate_sibling_and_resynthesize(
    store: &PostgresSessionStore,
    model_router: &ModelRouter,
    tenant_id: TenantId,
    open: &SkillDraftProposal,
    contribution: SiblingContribution<'_>,
) -> Result<SiblingResynthesis> {
    let SiblingContribution {
        suite,
        evidence,
        source_experience_id,
        source_session_id,
    } = contribution;
    let accepted = accumulate_sibling_suite(
        store,
        tenant_id,
        open,
        suite,
        source_experience_id,
        source_session_id,
    )
    .await?;
    if !accepted {
        return Ok(SiblingResynthesis::DraftUnchanged);
    }
    let instances = [GeneralizationInstance {
        evidence,
        source_experience_id,
    }];
    match resynthesize_generalization(store, model_router, tenant_id, open, &instances).await {
        Ok(true) => Ok(SiblingResynthesis::DraftRewritten),
        Ok(false) => Ok(SiblingResynthesis::DraftUnchanged),
        Err(error) => {
            tracing::warn!(
                tenant_id = %tenant_id,
                candidate_id = %open.candidate_id,
                skill = open.metadata.name.as_str(),
                error = %error,
                "sibling re-synthesis failed; sibling suite kept, draft revision unchanged"
            );
            Ok(SiblingResynthesis::DraftUnchanged)
        }
    }
}

/// One sibling execution contributing to a generalization pass.
///
/// The organic dedupe-hit path builds a one-element slice of these; the recurrence
/// path builds one per accepted cluster member so a single combined pass covers
/// them all.
struct GeneralizationInstance<'a> {
    /// Sanitized bounded segment evidence (tool trajectory + content) for this sibling.
    evidence: &'a SanitizedLearningEvidence,
    /// Experience record that produced this sibling.
    source_experience_id: Uuid,
}

/// One recurrence cluster member the workflow feeds into the combined pass.
///
/// Carries the bounded segment events and provenance for a single sibling. The
/// caller (the recurrence workflow) loads these best-effort per member; a member
/// whose events cannot be loaded is dropped before it reaches this struct.
pub struct RecurrenceSiblingSuite<'a> {
    /// Sanitized bounded segment evidence for the sibling.
    pub evidence: &'a SanitizedLearningEvidence,
    /// Experience record that produced the sibling.
    pub source_experience_id: Uuid,
    /// Session that produced the sibling.
    pub source_session_id: SessionId,
}

/// Feeds every recurrence cluster member into an open proposal, then generalizes once.
///
/// The recurrence cron files the exemplar's proposal first and hands the remaining
/// cluster members here as a batch. Unlike the organic dedupe-hit (one sibling
/// arriving per session, generalized as it lands), the whole cluster is known up
/// front, so this accumulates *all* sibling suites first — each is durable
/// held-out review material and must survive a later best-effort model error —
/// then runs a *single* combined generalization pass over every newly-accepted
/// member instead of one paid model call per member. Suites are generated for the
/// proposal's own skill name (members were grouped by task fingerprint, not skill
/// name). Best-effort and capped: a member past the sibling cap, a claimed
/// candidate, or a per-member suite failure is skipped without aborting the rest,
/// and the combined pass is a no-op when no member was newly accepted.
pub async fn accumulate_recurrence_siblings(
    store: &PostgresSessionStore,
    model_router: &ModelRouter,
    tenant_id: TenantId,
    open: &SkillDraftProposal,
    siblings: &[RecurrenceSiblingSuite<'_>],
) -> Result<SiblingResynthesis> {
    // Phase 1: accumulate every member's suite durably. A per-member failure warns
    // and is skipped so one bad member never loses the others' held-out material.
    let mut accepted: Vec<GeneralizationInstance<'_>> = Vec::new();
    for sibling in siblings {
        let suite = match crate::regression::generate_skill_test_suite_source_for_name(
            tenant_id,
            &open.metadata.name,
            sibling.evidence,
        ) {
            Ok(suite) => suite,
            Err(error) => {
                tracing::warn!(
                    tenant_id = %tenant_id,
                    candidate_id = %open.candidate_id,
                    sibling_experience_id = %sibling.source_experience_id,
                    error = %error,
                    "recurrence sibling suite generation failed; skipping this member"
                );
                continue;
            }
        };
        match accumulate_sibling_suite(
            store,
            tenant_id,
            open,
            suite,
            sibling.source_experience_id,
            sibling.source_session_id,
        )
        .await
        {
            Ok(true) => accepted.push(GeneralizationInstance {
                evidence: sibling.evidence,
                source_experience_id: sibling.source_experience_id,
            }),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    tenant_id = %tenant_id,
                    candidate_id = %open.candidate_id,
                    sibling_experience_id = %sibling.source_experience_id,
                    error = %error,
                    "recurrence sibling suite accumulation failed; skipping this member"
                );
            }
        }
    }
    if accepted.is_empty() {
        return Ok(SiblingResynthesis::DraftUnchanged);
    }

    // Phase 2: a single combined generalization pass over every accepted member.
    match resynthesize_generalization(store, model_router, tenant_id, open, &accepted).await {
        Ok(true) => Ok(SiblingResynthesis::DraftRewritten),
        Ok(false) => Ok(SiblingResynthesis::DraftUnchanged),
        Err(error) => {
            tracing::warn!(
                tenant_id = %tenant_id,
                candidate_id = %open.candidate_id,
                skill = open.metadata.name.as_str(),
                error = %error,
                "combined recurrence re-synthesis failed; sibling suites kept, draft revision unchanged"
            );
            Ok(SiblingResynthesis::DraftUnchanged)
        }
    }
}

/// Attaches a sibling-session suite to an open proposal as held-out material.
///
/// Called when a recurring task dedupes onto an open `Proposed` candidate:
/// instead of discarding the new session's evidence, its deterministic suite
/// joins the candidate payload so the review gate can examine the eventual
/// draft on material it was not derived from. Serialized under the same
/// (tenant, skill name) advisory lock as proposal filing; deduped by source
/// experience and capped at [`MAX_ACCUMULATED_SIBLING_SUITES`]. A claimed
/// (`Evaluating`) candidate is left untouched. Returns whether a new sibling
/// suite was appended.
pub(crate) async fn accumulate_sibling_suite(
    store: &PostgresSessionStore,
    tenant_id: TenantId,
    open: &SkillDraftProposal,
    suite: GeneratedSkillSuite,
    source_experience_id: Uuid,
    source_session_id: SessionId,
) -> Result<bool> {
    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(tenant_id)).await?;
    acquire_proposal_advisory_lock(conn.as_mut(), tenant_id, &open.metadata.name).await?;

    let Some(candidate) = store
        .get_learning_candidate_with_conn(conn.as_mut(), &tenant_id, open.candidate_id)
        .await?
    else {
        conn.commit().await?;
        return Ok(false);
    };
    let accepted = accumulate_sibling_suite_in_tx(
        store,
        conn.as_mut(),
        candidate,
        &suite,
        source_experience_id,
        source_session_id,
    )
    .await?;
    conn.commit().await?;
    Ok(accepted)
}

/// Applies sibling-suite accumulation inside the caller's locked transaction.
///
/// Returns whether a new sibling suite was stored for this candidate.
async fn accumulate_sibling_suite_in_tx(
    store: &PostgresSessionStore,
    conn: &mut PgConnection,
    mut candidate: LearningCandidate,
    suite: &GeneratedSkillSuite,
    source_experience_id: Uuid,
    source_session_id: SessionId,
) -> Result<bool> {
    if candidate.status != LearningCandidateStatus::Proposed {
        return Ok(false);
    }
    // Already-recorded evidence is not new evidence. This covers both the
    // proposal's own source experience and a replayed sibling, so accumulation
    // is idempotent rather than merely guarded against the origin case.
    if candidate.sources.iter().any(|source| {
        matches!(
            source,
            LearningCandidateSourceRef::Experience { experience_id }
                if *experience_id == source_experience_id
        )
    }) {
        return Ok(false);
    }
    if ArtifactRegistry::count_suite_contributions_in_tx(
        conn,
        candidate.id,
        SuiteContributionKind::Accumulated,
    )
    .await?
        >= MAX_ACCUMULATED_SIBLING_SUITES
    {
        return Ok(false);
    }
    let partition = StoragePartitionId::for_tenant(candidate.tenant_id).to_string();
    // The suite bytes belong to the artifact registry, not to candidate JSON: a
    // TOML blob inside a payload cannot be joined to its source session, counted,
    // or deleted for one subject. The unique `(candidate, kind, suite_name)` index
    // is also the dedupe: a replayed sibling inserts nothing and reports it.
    let stored = ArtifactRegistry::record_suite_contribution_in_tx(
        conn,
        &partition,
        &candidate.tenant_id.to_string(),
        &NewSuiteContribution {
            candidate_id: candidate.id,
            // A sibling suite is held-out material for whatever draft is current;
            // it is not embedded in a revision, so it guards none.
            revision_uid: None,
            kind: SuiteContributionKind::Accumulated,
            suite_name: sibling_suite_name(source_experience_id),
            suite_source: suite.source_toml.clone(),
            source_session_id: Some(source_session_id.0),
            source_experience_id: Some(source_experience_id),
        },
    )
    .await?;
    if !stored {
        return Ok(false);
    }
    // A sibling contributed real evidence, so it joins the candidate's typed
    // sources. Recording it there is what makes the replay guard above idempotent
    // and what lets an erasure of the sibling's session reach this proposal.
    candidate
        .sources
        .push(LearningCandidateSourceRef::Experience {
            experience_id: source_experience_id,
        });
    candidate.sources.push(LearningCandidateSourceRef::Session {
        session_id: source_session_id,
    });
    candidate.updated_at = Utc::now();
    store
        .append_learning_candidate_with_conn(conn, &candidate)
        .await?;
    Ok(true)
}

/// Stable per-candidate name for one sibling's accumulated suite.
fn sibling_suite_name(source_experience_id: Uuid) -> String {
    format!("sibling/{source_experience_id}")
}

/// Records the draft revision's attribution and its generated suite.
///
/// Both rows name the candidate, so an erasure entering through the subject's
/// session or experience reaches the revision text and the generated suite bytes
/// by a typed join rather than by searching JSON.
async fn record_draft_attribution(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    candidate_id: Uuid,
    revision_uid: Uuid,
    suite: &GeneratedSkillSuite,
    source_session_id: SessionId,
    source_experience_id: Option<Uuid>,
) -> Result<()> {
    let partition = StoragePartitionId::for_tenant(tenant_id).to_string();
    let tenant = tenant_id.to_string();
    ArtifactRegistry::record_revision_attribution_in_tx(
        conn,
        &partition,
        &tenant,
        revision_uid,
        candidate_id,
    )
    .await?;
    ArtifactRegistry::record_suite_contribution_in_tx(
        conn,
        &partition,
        &tenant,
        &NewSuiteContribution {
            candidate_id,
            revision_uid: Some(revision_uid),
            kind: SuiteContributionKind::Generated,
            suite_name: suite.relative_path.clone(),
            suite_source: suite.source_toml.clone(),
            source_session_id: Some(source_session_id.0),
            source_experience_id,
        },
    )
    .await?;
    Ok(())
}

/// Static generalization instructions; kept out of the dynamic user prompt so
/// the provider can cache them across passes. Mirrors the improver's
/// `UNCHANGED` convention and the distiller's Agent Skills format contract.
const SKILL_RESYNTHESIS_SYSTEM_PROMPT: &str = "\
You are generalizing a draft Agent Skill across recurring instances of the same task.
You are given the current draft SKILL.md and one or more new successful executions of sibling \
tasks.
Produce a parameterized generalization that covers both the existing draft's task and every new \
instance: make the inputs explicit, keep the invariant steps, and turn instance-specific values \
into named variable slots instead of hard-coded literals.
Output only a complete SKILL.md document using the Agent Skills format from agentskills.io. Keep \
the skill `name` unchanged and keep spec-compatible top-level frontmatter fields, using MOA \
metadata only for `moa-version`, `moa-tags`, and `moa-estimated-tokens`.
If the current draft already covers the new instance without changes, output exactly UNCHANGED.";

/// Draft state produced by one generalization model call.
enum ResynthesisDraft {
    /// The model judged the current draft already covers the new instance.
    Unchanged,
    /// The model output was structurally valid but had to be rejected.
    Rejected(String),
    /// The model produced an accepted generalized draft package. Boxed because
    /// a validated package is far larger than the other variants.
    Changed(Box<ChangedResynthesis>),
}

/// Accepted generalized draft awaiting persistence.
struct ChangedResynthesis {
    package: ValidatedSkillPackage,
    metadata: SkillMetadata,
}

/// Maximum generalization attempts for one pass, including the optimistic retry.
///
/// A pass reads the current draft, calls the model, then persists under the lock.
/// If another pass rewrote the draft while the model call was in flight, the write
/// would clobber that winner with a generalization of a stale draft, so the apply
/// step reports a conflict and the pass re-reads the latest draft and runs once
/// more. The sibling/pass cap bounds total passes, so a single re-spent model call
/// is an acceptable price for not losing a concurrent generalization.
const MAX_RESYNTHESIS_ATTEMPTS: usize = 2;

/// Runs one best-effort generalization pass over an open proposal's draft.
///
/// `instances` is one sibling execution for the organic dedupe-hit path and every
/// newly-accepted cluster member for the combined recurrence path; either way this
/// is a *single* model call. Preflight ordering keeps model spend honest: the
/// candidate is reloaded and its status and pass count are checked *before* any
/// model call, so a claimed candidate or one already at
/// [`MAX_ACCUMULATED_SIBLING_SUITES`] passes spends nothing. The generalized draft,
/// when accepted, replaces the draft revision and rewrites
/// `skill_markdown`/`draft_artifact_revision_uid` under the same (tenant, skill
/// name) advisory lock sibling accumulation uses; the skill name may never change.
///
/// Optimistic concurrency guards against a lost update: the draft revision read
/// before the model call is the baseline, and the under-lock write proceeds only
/// if the draft revision is still that baseline. A concurrent pass that rewrote the
/// draft yields a conflict, and this pass re-reads and re-runs once
/// ([`MAX_RESYNTHESIS_ATTEMPTS`]) so it generalizes the winner's draft rather than
/// overwriting it. Every applied pass records a `resynthesis` evidence entry with
/// the recorded-only trajectory stability score, regardless of whether the draft
/// changed. Returns whether the draft revision was rewritten by this pass.
async fn resynthesize_generalization(
    store: &PostgresSessionStore,
    model_router: &ModelRouter,
    tenant_id: TenantId,
    open: &SkillDraftProposal,
    instances: &[GeneralizationInstance<'_>],
) -> Result<bool> {
    if instances.is_empty() {
        return Ok(false);
    }
    let pass_experience_ids: Vec<Uuid> = instances
        .iter()
        .map(|instance| instance.source_experience_id)
        .collect();

    for attempt in 0..MAX_RESYNTHESIS_ATTEMPTS {
        // Preflight: reload the candidate and bail before any model spend when it
        // was claimed for evaluation or has reached the resynthesis cap.
        let (candidate, existing_suite) = {
            let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(tenant_id)).await?;
            let loaded = store
                .get_learning_candidate_with_conn(conn.as_mut(), &tenant_id, open.candidate_id)
                .await?;
            let suite = generated_suite_in_tx(conn.as_mut(), open.candidate_id).await?;
            conn.commit().await?;
            (loaded, suite)
        };
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        if !resynthesis_gate_open(candidate.status, &candidate.payload) {
            return Ok(false);
        }
        // The draft the model is about to generalize. The under-lock write bails if
        // this revision changed meanwhile, so a concurrent pass is not clobbered.
        let Some(baseline_revision) = payload_draft_revision(&candidate.payload) else {
            return Ok(false);
        };
        let Some(current_markdown) = candidate
            .payload
            .get("skill_markdown")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(false);
        };
        let current = parse_skill_markdown(current_markdown)?;

        // Trajectory stability is recorded only: the mean LCS ratio of each new
        // instance's tool sequence against the candidate's existing expected
        // trajectory. Computed before the model call and independent of it.
        let existing_trajectory = expected_trajectory_from_suite(existing_suite.as_ref());
        let stability = mean_trajectory_stability(&existing_trajectory, instances);

        // Generalization model call. No lock or transaction is held across it.
        let llm = model_router.provider_for(ModelTask::SkillDistillation);
        let response = llm
            .complete(crate::util::completion_request(
                SKILL_RESYNTHESIS_SYSTEM_PROMPT,
                build_resynthesis_user_prompt(current_markdown, instances),
            ))
            .await?
            .collect()
            .await?;
        let output = normalize_llm_markdown(&response.text);
        let draft = build_resynthesis_draft(output, &current, existing_suite.as_ref())?;

        match apply_resynthesis_result(
            store,
            tenant_id,
            open,
            draft,
            &pass_experience_ids,
            baseline_revision,
            stability,
        )
        .await?
        {
            ResynthesisApply::Applied(changed) => return Ok(changed),
            ResynthesisApply::Conflict => {
                if attempt + 1 < MAX_RESYNTHESIS_ATTEMPTS {
                    tracing::info!(
                        tenant_id = %tenant_id,
                        candidate_id = %open.candidate_id,
                        "resynthesis draft changed concurrently; retrying against the latest draft"
                    );
                    continue;
                }
                tracing::warn!(
                    tenant_id = %tenant_id,
                    candidate_id = %open.candidate_id,
                    "resynthesis draft changed concurrently again; skipping to avoid a lost update"
                );
                return Ok(false);
            }
        }
    }
    Ok(false)
}

/// Interprets one generalization model output as a draft state.
///
/// `UNCHANGED` maps to [`ResynthesisDraft::Unchanged`]; output that renames the
/// skill is rejected; otherwise the parsed package (with the existing suite
/// re-attached) is an accepted change.
fn build_resynthesis_draft(
    output: &str,
    current: &crate::format::SkillDocument,
    existing_suite: Option<&GeneratedSkillSuite>,
) -> Result<ResynthesisDraft> {
    if output.trim() == "UNCHANGED" {
        return Ok(ResynthesisDraft::Unchanged);
    }
    let generalized = parse_skill_markdown(output)?;
    if generalized.frontmatter.name != current.frontmatter.name {
        return Ok(ResynthesisDraft::Rejected(
            "re-synthesis changed the target skill name".to_string(),
        ));
    }
    let markdown = render_skill_markdown(&generalized)?;
    let base = SkillPackage::from_skill_markdown(markdown).validate()?;
    let package = match existing_suite {
        Some(suite) => package_with_regression_suite(&base, suite)?,
        None => base,
    };
    let metadata = skill_metadata_from_document(
        build_skill_path(&generalized.frontmatter.name),
        &generalized,
    );
    Ok(ResynthesisDraft::Changed(Box::new(ChangedResynthesis {
        package,
        metadata,
    })))
}

/// Result of persisting a generalization pass under the advisory lock.
enum ResynthesisApply {
    /// The pass ran under the lock; the flag is whether it rewrote the draft.
    Applied(bool),
    /// The draft revision changed after the pre-model read; the caller may re-read
    /// the latest draft and retry so the winner's generalization is not clobbered.
    Conflict,
}

/// Persists one generalization pass under the proposal's advisory lock.
///
/// Bails to [`ResynthesisApply::Conflict`] (without writing) when the candidate's
/// draft revision no longer equals `baseline_revision` — the revision the model
/// generalized — so a pass that raced a concurrent rewrite retries against the
/// winner instead of clobbering it. Otherwise returns
/// [`ResynthesisApply::Applied`] carrying whether the pass rewrote the draft
/// revision (a `Changed` result persisted under the lock); an `UNCHANGED`/rejected
/// pass or a candidate that was claimed or capped while the model call was in
/// flight applies as `false`.
async fn apply_resynthesis_result(
    store: &PostgresSessionStore,
    tenant_id: TenantId,
    open: &SkillDraftProposal,
    draft: ResynthesisDraft,
    pass_experience_ids: &[Uuid],
    baseline_revision: Uuid,
    stability: f64,
) -> Result<ResynthesisApply> {
    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(tenant_id)).await?;
    acquire_proposal_advisory_lock(conn.as_mut(), tenant_id, &open.metadata.name).await?;

    let Some(mut candidate) = store
        .get_learning_candidate_with_conn(conn.as_mut(), &tenant_id, open.candidate_id)
        .await?
    else {
        conn.commit().await?;
        return Ok(ResynthesisApply::Applied(false));
    };
    // Re-check the guards under the lock: a concurrent pass may have claimed the
    // candidate or filled the cap while the model call was in flight.
    if !resynthesis_gate_open(candidate.status, &candidate.payload) {
        conn.commit().await?;
        return Ok(ResynthesisApply::Applied(false));
    }
    // Optimistic concurrency: a concurrent pass may have rewritten the draft while
    // the model call was in flight. This pass generalized `baseline_revision`, so
    // writing it now would clobber the winner with a stale generalization. Report a
    // conflict and let the caller retry against the latest draft.
    if payload_draft_revision(&candidate.payload) != Some(baseline_revision) {
        conn.commit().await?;
        return Ok(ResynthesisApply::Conflict);
    }
    let pass = resynthesis_pass_count(&candidate.payload) + 1;

    let (changed, rejected_reason) = match draft {
        ResynthesisDraft::Changed(changed) => {
            let ChangedResynthesis { package, metadata } = *changed;
            let scope = ActionRuleScope::Tenant { tenant_id };
            let document = skill_artifact_document_from_package(&package, ArtifactStatus::Draft)?;
            let source_text = skill_artifact_source_text(&package, &document)?;
            let artifact_files = package
                .files
                .iter()
                .map(artifact_file_from_skill_file)
                .collect::<Vec<_>>();
            let stored = ArtifactRegistry::create_draft_in_tx(
                conn.as_mut(),
                &scope,
                NewArtifactDraft {
                    document: &document,
                    source_format: "yaml",
                    source_text: &source_text,
                    files: &artifact_files,
                },
            )
            .await?;
            // A generalization pass produces a NEW revision fused from the
            // original draft plus every sibling in this pass, so the new revision
            // needs its own attribution rows; without them an erasure would reach
            // the superseded draft and leave the serving one standing. The suite
            // rows follow the draft they guard, since `build_resynthesis_draft`
            // re-attaches the same suite bytes to the rewritten package.
            let partition = StoragePartitionId::for_tenant(tenant_id).to_string();
            ArtifactRegistry::record_revision_attribution_in_tx(
                conn.as_mut(),
                &partition,
                &tenant_id.to_string(),
                stored.revision_uid,
                candidate.id,
            )
            .await?;
            ArtifactRegistry::repoint_suite_contributions_in_tx(
                conn.as_mut(),
                candidate.id,
                stored.revision_uid,
            )
            .await?;
            let metadata_value = serde_json::to_value(&metadata)
                .map_err(|error| MoaError::SerializationError(error.to_string()))?;
            if let Some(object) = candidate.payload.as_object_mut() {
                object.insert(
                    "skill_markdown".to_string(),
                    json!(package.skill_md.clone()),
                );
                object.insert(
                    "draft_artifact_revision_uid".to_string(),
                    json!(stored.revision_uid.to_string()),
                );
                object.insert(
                    "artifact_uid".to_string(),
                    json!(stored.artifact_uid.to_string()),
                );
                object.insert("skill_metadata".to_string(), metadata_value);
            }
            (true, None)
        }
        ResynthesisDraft::Unchanged => (false, None),
        ResynthesisDraft::Rejected(reason) => (false, Some(reason)),
    };

    append_resynthesis_evidence(
        &mut candidate.payload,
        ResynthesisEvidence {
            pass,
            pass_experience_ids,
            changed,
            trajectory_stability: stability,
            rejected_reason,
        },
    );
    for experience_id in pass_experience_ids {
        push_unique_experience_source(&mut candidate.sources, *experience_id);
    }
    candidate.updated_at = Utc::now();
    store
        .append_learning_candidate_with_conn(conn.as_mut(), &candidate)
        .await?;
    conn.commit().await?;
    Ok(ResynthesisApply::Applied(changed))
}

/// Recorded evidence for one generalization pass.
struct ResynthesisEvidence<'a> {
    pass: usize,
    /// Every sibling experience that contributed to this pass. One entry for the
    /// organic dedupe-hit; the whole accepted cluster for a combined recurrence
    /// pass.
    pass_experience_ids: &'a [Uuid],
    changed: bool,
    trajectory_stability: f64,
    rejected_reason: Option<String>,
}

/// Whether a candidate may still take another generalization pass.
///
/// The single gate both the preflight and the under-lock re-check consult: a
/// claimed (non-`Proposed`) candidate is left untouched, and a candidate that
/// has already taken [`MAX_ACCUMULATED_SIBLING_SUITES`] passes spends no more
/// model calls. Consulting it before [`ModelRouter::provider_for`] is what keeps
/// the "no model call when claimed or capped" invariant honest.
fn resynthesis_gate_open(status: LearningCandidateStatus, payload: &serde_json::Value) -> bool {
    status == LearningCandidateStatus::Proposed
        && resynthesis_pass_count(payload) < MAX_ACCUMULATED_SIBLING_SUITES
}

/// Adds one experience source to a candidate, ignoring an id already recorded.
///
/// Sibling provenance grows once per contributing experience: a repeat of the
/// same id is a no-op, so a replayed generalization pass neither duplicates a
/// source row nor makes the same session look like two contributors.
fn push_unique_experience_source(
    sources: &mut Vec<LearningCandidateSourceRef>,
    experience_id: Uuid,
) {
    if sources.iter().any(|source| {
        matches!(
            source,
            LearningCandidateSourceRef::Experience { experience_id: existing }
                if *existing == experience_id
        )
    }) {
        return;
    }
    sources.push(LearningCandidateSourceRef::Experience { experience_id });
}

/// Number of generalization passes already recorded on a candidate payload.
fn resynthesis_pass_count(payload: &serde_json::Value) -> usize {
    payload
        .get("resynthesis")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

/// Appends one `resynthesis` evidence entry to a candidate payload.
fn append_resynthesis_evidence(payload: &mut serde_json::Value, evidence: ResynthesisEvidence) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let entries = object
        .entry("resynthesis")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(entries) = entries.as_array_mut() else {
        return;
    };
    let source_ids: Vec<String> = evidence
        .pass_experience_ids
        .iter()
        .map(Uuid::to_string)
        .collect();
    let mut entry = json!({
        "pass": evidence.pass,
        // Display evidence only. The authoritative provenance for this pass is
        // the typed source rows written beside the candidate; this list exists
        // so a reviewer can read which siblings drove which pass without a join.
        "pass_experience_ids": source_ids,
        "changed": evidence.changed,
        "trajectory_stability": evidence.trajectory_stability,
    });
    if let Some(reason) = evidence.rejected_reason {
        entry["rejected_reason"] = json!(reason);
    }
    entries.push(entry);
}

/// Reads the current draft artifact revision UID from a candidate payload.
///
/// The optimistic-concurrency baseline for a generalization pass: the revision the
/// model generalized. `None` when the payload lacks a parseable revision, which the
/// caller treats as a non-generalizable candidate.
fn payload_draft_revision(payload: &serde_json::Value) -> Option<Uuid> {
    payload
        .get("draft_artifact_revision_uid")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

/// Reads back the candidate's own generated suite from the artifact owner.
///
/// The suite bytes live in `moa.artifact_suite_contribution`, so this is a
/// storage read rather than a payload parse; a candidate with no generated
/// contribution has no suite, which callers treat as non-generalizable.
async fn generated_suite_in_tx(
    conn: &mut PgConnection,
    candidate_id: Uuid,
) -> Result<Option<GeneratedSkillSuite>> {
    Ok(
        ArtifactRegistry::list_suite_contributions_in_tx(conn, candidate_id)
            .await?
            .into_iter()
            .find(|contribution| contribution.kind == SuiteContributionKind::Generated)
            .map(|contribution| GeneratedSkillSuite {
                relative_path: contribution.suite_name,
                source_toml: contribution.suite_source,
            }),
    )
}

/// Parses the candidate's expected tool trajectory from its generated suite.
///
/// Reads the stored suite TOML rather than regenerating it, so the comparison is
/// against the exact trajectory the review gate would use. Returns an empty
/// trajectory when the candidate has no parseable suite.
fn expected_trajectory_from_suite(suite: Option<&GeneratedSkillSuite>) -> Vec<String> {
    let Some(suite) = suite else {
        return Vec::new();
    };
    let Ok(parsed) = toml::from_str::<TestSuite>(&suite.source_toml) else {
        return Vec::new();
    };
    parsed
        .cases
        .into_iter()
        .next()
        .and_then(|case| case.expected_trajectory)
        .unwrap_or_default()
}

/// Mean recorded-only trajectory stability across a pass's instances.
///
/// Each instance scores the LCS ratio of its tool sequence against the existing
/// expected trajectory; the pass records their mean. One instance reduces to the
/// single-instance ratio; no instances is vacuously stable.
fn mean_trajectory_stability(expected: &[String], instances: &[GeneralizationInstance<'_>]) -> f64 {
    if instances.is_empty() {
        return 1.0;
    }
    let total: f64 = instances
        .iter()
        .map(|instance| trajectory_stability(expected, &instance.evidence.tool_trajectory()))
        .sum();
    total / instances.len() as f64
}

/// Longest-common-subsequence ratio between two tool-call sequences.
///
/// Mirrors the trajectory-match evaluator's LCS scoring locally to avoid
/// depending on the eval evaluator surface: `1.0` for identical sequences,
/// `0.0` for fully disjoint ones, normalized by the longer sequence.
fn trajectory_stability(expected: &[String], actual: &[String]) -> f64 {
    let max_len = expected.len().max(actual.len());
    if max_len == 0 {
        return 1.0;
    }
    lcs_len(expected, actual) as f64 / max_len as f64
}

fn lcs_len(expected: &[String], actual: &[String]) -> usize {
    let mut prev = vec![0usize; actual.len() + 1];
    let mut curr = vec![0usize; actual.len() + 1];
    for expected_item in expected {
        for (index, actual_item) in actual.iter().enumerate() {
            curr[index + 1] = if expected_item == actual_item {
                prev[index] + 1
            } else {
                prev[index + 1].max(curr[index])
            };
        }
        prev.clone_from(&curr);
        curr.fill(0);
    }
    prev[actual.len()]
}

/// Builds the generalization user prompt: the current draft plus one section per
/// new sibling execution. A single instance keeps the original singular framing;
/// multiple instances (a combined recurrence pass) are numbered so the model sees
/// each execution distinctly.
fn build_resynthesis_user_prompt(
    current_markdown: &str,
    instances: &[GeneralizationInstance<'_>],
) -> String {
    let mut prompt = format!("Current draft skill:\n{current_markdown}\n\n");
    if let [single] = instances {
        prompt.push_str(&format!(
            "New sibling execution:\n{}",
            format_evidence_for_learning(single.evidence)
        ));
    } else {
        prompt.push_str("New sibling executions:\n");
        for (index, instance) in instances.iter().enumerate() {
            prompt.push_str(&format!(
                "--- Instance {} ---\n{}\n",
                index + 1,
                format_evidence_for_learning(instance.evidence)
            ));
        }
    }
    prompt
}

async fn acquire_proposal_advisory_lock(
    conn: &mut PgConnection,
    tenant_id: moa_core::types::identifiers::TenantId,
    skill_name: &str,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(proposal_advisory_lock_key(tenant_id, skill_name))
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

async fn acquire_fingerprint_advisory_lock(
    conn: &mut PgConnection,
    tenant_id: moa_core::types::identifiers::TenantId,
    fingerprint_hash: &str,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(advisory_lock_key(
            b"moa.skill.proposal.fingerprint.lock.v1",
            tenant_id,
            fingerprint_hash,
        ))
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

fn proposal_advisory_lock_key(
    tenant_id: moa_core::types::identifiers::TenantId,
    skill_name: &str,
) -> i64 {
    advisory_lock_key(b"moa.skill.proposal.lock.v1", tenant_id, skill_name)
}

fn advisory_lock_key(
    namespace: &[u8],
    tenant_id: moa_core::types::identifiers::TenantId,
    key: &str,
) -> i64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update(tenant_id.to_string().as_bytes());
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

fn operation_from_payload(payload: &serde_json::Value) -> Option<SkillProposalOperation> {
    match payload.get("operation").and_then(serde_json::Value::as_str) {
        Some("skill_created") => Some(SkillProposalOperation::Created),
        Some("skill_improved") => Some(SkillProposalOperation::Improved {
            previous_version: payload
                .get("previous_version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

impl SkillProposalOperation {
    fn payload_operation(&self) -> &'static str {
        match self {
            Self::Created => "skill_created",
            Self::Improved { .. } => "skill_improved",
        }
    }
}

fn proposal_from_existing(
    candidate: LearningCandidate,
    fallback_metadata: SkillMetadata,
    operation: SkillProposalOperation,
) -> Result<SkillDraftProposal> {
    let draft_artifact_revision_uid = payload_uuid(
        &candidate.payload,
        "draft_artifact_revision_uid",
        "existing skill proposal is missing draft_artifact_revision_uid",
    )?;
    let metadata = candidate
        .payload
        .get("skill_metadata")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(fallback_metadata);
    Ok(SkillDraftProposal {
        candidate_id: candidate.id,
        draft_artifact_revision_uid,
        metadata,
        operation,
        surface: EditableSurface::SkillMarkdown,
    })
}

fn payload_uuid(payload: &serde_json::Value, key: &str, error: &str) -> Result<Uuid> {
    let value = payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| MoaError::ValidationError(error.to_string()))?;
    Uuid::parse_str(value)
        .map_err(|parse_error| MoaError::ValidationError(format!("{key}: {parse_error}")))
}

struct ProposalPayloadInput<'a> {
    candidate_id: Uuid,
    session: &'a SessionMeta,
    package: &'a ValidatedSkillPackage,
    metadata: &'a SkillMetadata,
    operation: &'a SkillProposalOperation,
    source: &'a SkillProposalSource,
    artifact_uid: Uuid,
    draft_artifact_revision_uid: Uuid,
}

fn proposal_payload(input: ProposalPayloadInput<'_>) -> serde_json::Value {
    let mut payload = json!({
        "kind": "skill_draft_proposal",
        "candidate_id": input.candidate_id,
        "operation": input.operation.payload_operation(),
        "artifact_uid": input.artifact_uid,
        "draft_artifact_revision_uid": input.draft_artifact_revision_uid,
        "artifact_kind": ArtifactKind::Skill.as_str(),
        "artifact_name": input.metadata.name.clone(),
        "artifact_status": ArtifactStatus::Draft.as_str(),
        "surface": EditableSurface::SkillMarkdown,
        "source_session_id": input.session.id.to_string(),
        "skill_metadata": input.metadata.clone(),
        "artifact_path": input.metadata.path.clone(),
        "skill_markdown": input.package.skill_md.clone(),
    });

    if let SkillProposalOperation::Improved { previous_version } = input.operation {
        payload["previous_version"] = json!(previous_version);
    }
    if let Some(evidence) = &input.source.evidence {
        payload["evidence"] = evidence.clone();
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_surface_round_trips_every_proposable_variant() {
        // Pins: the four proposable surfaces are the complete closed set and each serializes to a
        // stable snake_case label. A proposal can only ever carry one of these, so non-proposable
        // surfaces (authz, action policy, audit, eval definitions, budget gates) have no wire form.
        let variants = [
            (EditableSurface::SkillMarkdown, "skill_markdown"),
            (
                EditableSurface::RewritePromptVersion,
                "rewrite_prompt_version",
            ),
            (EditableSurface::RouterRules, "router_rules"),
            (EditableSurface::RankingConfig, "ranking_config"),
        ];
        for (surface, label) in variants {
            assert_eq!(surface.as_str(), label);
            let json = serde_json::to_value(surface).expect("surface serializes");
            assert_eq!(json, json!(label));
            let parsed: EditableSurface =
                serde_json::from_value(json).expect("surface round-trips from its label");
            assert_eq!(parsed, surface);
        }
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn resynthesis_gate_closes_for_claimed_and_capped_candidates() {
        // Pins: the single gate consulted before any model call. A Proposed candidate below the
        // pass cap may generalize again; a claimed candidate or one at the cap spends nothing.
        let open_payload = json!({});
        assert!(resynthesis_gate_open(
            LearningCandidateStatus::Proposed,
            &open_payload
        ));
        // Claimed for evaluation: left untouched, no matter the pass count.
        assert!(!resynthesis_gate_open(
            LearningCandidateStatus::Evaluating,
            &open_payload
        ));
        assert!(!resynthesis_gate_open(
            LearningCandidateStatus::Promoted,
            &open_payload
        ));
        // At the cap: no further passes even while Proposed.
        let capped = json!({
            "resynthesis": vec![json!({}); MAX_ACCUMULATED_SIBLING_SUITES],
        });
        assert!(!resynthesis_gate_open(
            LearningCandidateStatus::Proposed,
            &capped
        ));
        let below_cap = json!({
            "resynthesis": vec![json!({}); MAX_ACCUMULATED_SIBLING_SUITES - 1],
        });
        assert!(resynthesis_gate_open(
            LearningCandidateStatus::Proposed,
            &below_cap
        ));
    }

    #[test]
    fn trajectory_stability_scores_identical_divergent_and_disjoint_sequences() {
        // Pins: recorded-only stability is an LCS ratio over the longer sequence, so identical
        // tool sequences score 1.0, a single divergence drops below 1.0, and fully disjoint
        // sequences score 0.0. Two empty sequences are vacuously stable.
        assert_eq!(
            trajectory_stability(
                &owned(&["bash", "read", "edit"]),
                &owned(&["bash", "read", "edit"])
            ),
            1.0
        );
        // One of three positions diverges: LCS 2 over max length 3.
        let divergent = trajectory_stability(
            &owned(&["bash", "read", "edit"]),
            &owned(&["bash", "web", "edit"]),
        );
        assert!((divergent - 2.0 / 3.0).abs() < 1e-9, "got {divergent}");
        assert_eq!(
            trajectory_stability(&owned(&["bash", "read"]), &owned(&["web", "grep"])),
            0.0
        );
        assert_eq!(trajectory_stability(&[], &[]), 1.0);
    }

    #[test]
    fn payload_draft_revision_parses_the_baseline_or_none() {
        // Pins: the optimistic-concurrency baseline is the payload's
        // draft_artifact_revision_uid parsed as a UUID; a missing or unparseable
        // value yields None so the pass treats the candidate as non-generalizable.
        let revision = Uuid::now_v7();
        assert_eq!(
            payload_draft_revision(&json!({
                "draft_artifact_revision_uid": revision.to_string(),
            })),
            Some(revision)
        );
        assert_eq!(payload_draft_revision(&json!({})), None);
        assert_eq!(
            payload_draft_revision(&json!({ "draft_artifact_revision_uid": "not-a-uuid" })),
            None
        );
    }

    #[tokio::test]
    async fn resynthesis_prompt_numbers_multiple_instances_but_not_a_single_one() {
        // Pins: one sibling keeps the singular framing; a combined pass over several
        // siblings numbers each execution so the model sees them distinctly.
        let empty = crate::evidence::sanitize_for_tests(&[]).await;
        let one = [GeneralizationInstance {
            evidence: &empty,
            source_experience_id: Uuid::now_v7(),
        }];
        let single = build_resynthesis_user_prompt("draft body", &one);
        assert!(single.contains("New sibling execution:"));
        assert!(!single.contains("--- Instance 1 ---"));

        let many = [
            GeneralizationInstance {
                evidence: &empty,
                source_experience_id: Uuid::now_v7(),
            },
            GeneralizationInstance {
                evidence: &empty,
                source_experience_id: Uuid::now_v7(),
            },
        ];
        let combined = build_resynthesis_user_prompt("draft body", &many);
        assert!(combined.contains("New sibling executions:"));
        assert!(combined.contains("--- Instance 1 ---"));
        assert!(combined.contains("--- Instance 2 ---"));
    }

    #[test]
    fn resynthesis_pass_count_reads_the_recorded_entries() {
        // Pins: the pass count that drives the cap is the length of the payload's resynthesis
        // array, defaulting to zero when the loop has not run.
        assert_eq!(resynthesis_pass_count(&json!({})), 0);
        assert_eq!(
            resynthesis_pass_count(&json!({ "resynthesis": [json!({}), json!({})] })),
            2
        );
    }

    #[test]
    fn append_resynthesis_evidence_records_outcome_and_stability() {
        // Pins: every pass records a resynthesis entry carrying the pass number, contributing
        // experience, changed flag, and recorded-only stability; a rejection also carries its
        // reason. This is the reviewer- and cap-facing evidence.
        let mut payload = json!({});
        let experience = Uuid::now_v7();
        let combined = [Uuid::now_v7(), Uuid::now_v7()];
        append_resynthesis_evidence(
            &mut payload,
            ResynthesisEvidence {
                pass: 1,
                pass_experience_ids: &[experience],
                changed: true,
                trajectory_stability: 0.75,
                rejected_reason: None,
            },
        );
        append_resynthesis_evidence(
            &mut payload,
            ResynthesisEvidence {
                pass: 2,
                pass_experience_ids: &combined,
                changed: false,
                trajectory_stability: 1.0,
                rejected_reason: Some("re-synthesis changed the target skill name".to_string()),
            },
        );
        let entries = payload["resynthesis"].as_array().expect("evidence array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["pass"], 1);
        assert_eq!(entries[0]["changed"], true);
        assert_eq!(entries[0]["trajectory_stability"], 0.75);
        assert_eq!(
            entries[0]["pass_experience_ids"],
            json!([experience.to_string()])
        );
        assert!(entries[0].get("rejected_reason").is_none());
        // A combined pass records every contributing experience id.
        assert_eq!(
            entries[1]["pass_experience_ids"],
            json!([combined[0].to_string(), combined[1].to_string()])
        );
        assert_eq!(entries[1]["changed"], false);
        assert_eq!(
            entries[1]["rejected_reason"],
            "re-synthesis changed the target skill name"
        );
    }

    #[test]
    fn sibling_experience_sources_dedupe_and_keep_other_source_kinds() {
        // Pins: sibling provenance grows once per contributing experience, so a
        // replayed generalization pass cannot make one session look like two
        // contributors and inflate the closure an erasure walks. Also pins that
        // deduping keys on the experience id alone: an unrelated session source
        // must not block an experience source from being recorded.
        let mut sources = vec![LearningCandidateSourceRef::Session {
            session_id: SessionId(Uuid::from_u128(1)),
        }];
        let experience = Uuid::from_u128(2);

        push_unique_experience_source(&mut sources, experience);
        push_unique_experience_source(&mut sources, experience);

        assert_eq!(sources.len(), 2);
        assert_eq!(
            sources[1],
            LearningCandidateSourceRef::Experience {
                experience_id: experience
            }
        );
    }

    #[test]
    fn expected_trajectory_is_read_from_the_stored_suite_contribution() {
        // Pins: the candidate's expected trajectory is read from the stored suite TOML, not
        // regenerated, so stability is scored against the exact gate trajectory. A candidate
        // with no stored suite yields an empty trajectory rather than a fabricated one.
        use moa_eval_core::TestCase;
        let suite = TestSuite {
            name: "resynth-regression".to_string(),
            description: None,
            cases: vec![TestCase {
                name: "case".to_string(),
                input: "input".to_string(),
                expected_trajectory: Some(owned(&["bash", "file_read", "bash"])),
                ..TestCase::default()
            }],
            default_timeout_seconds: 120,
            tags: vec!["skill".to_string()],
        };
        let stored = GeneratedSkillSuite {
            relative_path: "tenants/x/skills/y/tests/suite.toml".to_string(),
            source_toml: toml::to_string_pretty(&suite).expect("suite toml"),
        };
        assert_eq!(
            expected_trajectory_from_suite(Some(&stored)),
            owned(&["bash", "file_read", "bash"])
        );
        assert!(expected_trajectory_from_suite(None).is_empty());
    }
}

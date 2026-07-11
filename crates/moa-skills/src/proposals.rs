//! Draft artifact proposal storage for self-generated skill packages.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::experience::LearningCandidate, types::experience::TaskFacetSet,
    types::experience::TaskFingerprint, types::memory::SkillMetadata, types::session::SessionMeta,
};
use moa_db::ScopedConn;
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::artifact::{
    artifact_file_from_skill_file, skill_artifact_document_from_package, skill_artifact_source_text,
};
use crate::candidates::{
    SkillDraftCandidateInput, deterministic_skill_candidate_id, skill_draft_candidate,
};
use crate::distiller::DistillationSkipReason;
use crate::package::ValidatedSkillPackage;
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
    pub source_experience_ids: Vec<Uuid>,
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
        &source.source_experience_ids,
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
        if let Some(source_experience_id) = source.source_experience_ids.first().copied() {
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
            if let Some(source_experience_id) = source.source_experience_ids.first().copied() {
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
        generated_suite: &generated_suite,
        artifact_uid: stored.artifact_uid,
        draft_artifact_revision_uid: stored.revision_uid,
    });
    let candidate = skill_draft_candidate(
        session,
        SkillDraftCandidateInput {
            candidate_id,
            operation: operation_label.to_string(),
            metadata: metadata.clone(),
            payload,
            source_experience_ids: source.source_experience_ids,
            task_fingerprint: source.task_fingerprint,
            task_facets: source.task_facets,
            confidence: source.confidence,
            now,
        },
    );
    store
        .append_learning_candidate_with_conn(conn.as_mut(), &candidate)
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
const MAX_ACCUMULATED_SIBLING_SUITES: usize = 3;

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

/// Attaches a sibling-session suite to an open proposal as held-out material.
///
/// Called when a recurring task dedupes onto an open `Proposed` candidate:
/// instead of discarding the new session's evidence, its deterministic suite
/// joins the candidate payload so the review gate can examine the eventual
/// draft on material it was not derived from. Serialized under the same
/// (tenant, skill name) advisory lock as proposal filing; deduped by source
/// experience and capped at [`MAX_ACCUMULATED_SIBLING_SUITES`]. A claimed
/// (`Evaluating`) candidate is left untouched.
pub(crate) async fn accumulate_sibling_suite(
    store: &PostgresSessionStore,
    tenant_id: moa_core::types::identifiers::TenantId,
    open: &SkillDraftProposal,
    suite: GeneratedSkillSuite,
    source_experience_id: Uuid,
    source_session_id: moa_core::types::identifiers::SessionId,
) -> Result<()> {
    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::tenant(tenant_id)).await?;
    acquire_proposal_advisory_lock(conn.as_mut(), tenant_id, &open.metadata.name).await?;

    let Some(candidate) = store
        .get_learning_candidate_with_conn(conn.as_mut(), &tenant_id, open.candidate_id)
        .await?
    else {
        conn.commit().await?;
        return Ok(());
    };
    accumulate_sibling_suite_in_tx(
        store,
        conn.as_mut(),
        candidate,
        &suite,
        source_experience_id,
        source_session_id,
    )
    .await?;
    conn.commit().await?;
    Ok(())
}

/// Applies sibling-suite accumulation inside the caller's locked transaction.
async fn accumulate_sibling_suite_in_tx(
    store: &PostgresSessionStore,
    conn: &mut PgConnection,
    mut candidate: LearningCandidate,
    suite: &GeneratedSkillSuite,
    source_experience_id: Uuid,
    source_session_id: moa_core::types::identifiers::SessionId,
) -> Result<()> {
    if candidate.status != moa_core::types::experience::LearningCandidateStatus::Proposed {
        return Ok(());
    }
    // A replay of the proposal's own source experience is not a sibling.
    if candidate
        .source_experience_ids
        .contains(&source_experience_id)
    {
        return Ok(());
    }
    if !append_sibling_suite_to_payload(
        &mut candidate.payload,
        source_experience_id,
        source_session_id,
        suite,
    ) {
        return Ok(());
    }
    // Sibling provenance lives in the payload entries: the candidate-row upsert
    // does not update `source_experience_ids`, and the origin list must stay
    // stable for the replay guard above.
    candidate.updated_at = Utc::now();
    store
        .append_learning_candidate_with_conn(conn, &candidate)
        .await?;
    Ok(())
}

/// Appends one sibling suite entry to a proposal payload; returns whether it changed.
fn append_sibling_suite_to_payload(
    payload: &mut serde_json::Value,
    source_experience_id: Uuid,
    source_session_id: moa_core::types::identifiers::SessionId,
    suite: &GeneratedSkillSuite,
) -> bool {
    let entries = payload
        .as_object_mut()
        .map(|object| {
            object
                .entry("accumulated_regression_suites")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        })
        .and_then(serde_json::Value::as_array_mut);
    let Some(entries) = entries else {
        return false;
    };
    if entries.len() >= MAX_ACCUMULATED_SIBLING_SUITES {
        return false;
    }
    let experience_key = source_experience_id.to_string();
    if entries.iter().any(|entry| {
        entry
            .get("source_experience_id")
            .and_then(serde_json::Value::as_str)
            == Some(experience_key.as_str())
    }) {
        return false;
    }
    entries.push(json!({
        "source_experience_id": experience_key,
        "source_session_id": source_session_id.to_string(),
        "source_format": "toml",
        "source_text": suite.source_toml,
    }));
    true
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
    generated_suite: &'a GeneratedSkillSuite,
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
        "source_experience_ids": input.source.source_experience_ids.clone(),
        "skill_metadata": input.metadata.clone(),
        "artifact_path": input.metadata.path.clone(),
        "skill_markdown": input.package.skill_md.clone(),
        "generated_regression_suite": {
            "relative_path": input.generated_suite.relative_path.clone(),
            "source_format": "toml",
            "source_text": input.generated_suite.source_toml.clone(),
        },
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
}

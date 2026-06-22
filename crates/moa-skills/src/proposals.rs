//! Draft artifact proposal storage for self-generated skill packages.

use chrono::Utc;
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_core::{
    ActionRuleScope, LearningCandidate, MoaError, Result, ScopeContext, ScopedConn, SessionMeta,
    SkillMetadata, TaskFacetSet, TaskFingerprint, WorkspaceId,
};
use moa_session::PostgresSessionStore;
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
}

impl SkillProposalSource {
    pub(crate) fn session_only() -> Self {
        Self {
            source_experience_ids: Vec::new(),
            task_fingerprint: None,
            task_facets: None,
            confidence: Some(1.0),
        }
    }
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
    let candidate_workspace_id = WorkspaceId::new(session.tenant_id.to_string());
    let mut conn =
        ScopedConn::begin(store.pool(), &ScopeContext::tenant(session.tenant_id)).await?;
    acquire_proposal_advisory_lock(conn.as_mut(), candidate_id).await?;

    if let Some(existing) = store
        .get_learning_candidate_with_conn(conn.as_mut(), &candidate_workspace_id, candidate_id)
        .await?
    {
        let proposal = proposal_from_existing(existing, metadata, operation)?;
        conn.commit().await?;
        return Ok(proposal);
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
    })
}

async fn acquire_proposal_advisory_lock(conn: &mut PgConnection, candidate_id: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(proposal_advisory_lock_key(candidate_id))
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

fn proposal_advisory_lock_key(candidate_id: Uuid) -> i64 {
    let bytes = candidate_id.as_bytes();
    let high = i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let low = i64::from_be_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    high ^ low
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
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
    payload
}

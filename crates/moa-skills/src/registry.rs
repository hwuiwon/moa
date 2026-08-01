//! Artifact-backed skill package registry with three-tier scoping.

use chrono::{DateTime, Utc};
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactFile, ArtifactRegistry, StoredArtifactRevision};
use moa_artifacts::release::TenantScope;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::identifiers::TenantId, types::identifiers::UserId, types::memory::SkillMetadata,
};
use moa_memory_types::MemoryScope;
use sqlx::PgPool;
use uuid::Uuid;

use crate::format::build_skill_path;
use crate::package::{
    SKILL_MD_PATH, SkillPackage, SkillPackageFile, SkillPackageManifest, ValidatedSkillPackage,
    ValidatedSkillPackageFile,
};
use crate::util::tenant_artifact_scope;

/// One active or historical skill package loaded from a skill artifact revision.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// Compatibility identifier for this skill revision; equals artifact `revision_uid`.
    pub skill_uid: Uuid,
    /// Tenant owning tenant-scoped skills.
    pub tenant_id: Option<TenantId>,
    /// User owning user scoped skills.
    pub user_id: Option<UserId>,
    /// Generated SQL scope tier.
    pub scope: String,
    /// Stable skill name.
    pub name: String,
    /// Human-readable skill description.
    pub description: String,
    /// Deterministic SHA-256 digest of the full package tree.
    pub package_hash: Vec<u8>,
    /// SHA-256 digest of the required `SKILL.md`.
    pub skill_md_hash: Vec<u8>,
    /// Number of files in the package.
    pub file_count: i32,
    /// Total package size in bytes.
    pub total_size_bytes: i64,
    /// Package manifest derived from `SKILL.md` and skill artifact metadata.
    pub manifest: SkillPackageManifest,
    /// Integer artifact-local version for this skill revision.
    pub version: i32,
    /// Search and ranking tags.
    pub tags: Vec<String>,
    /// Time when the artifact revision stopped being valid.
    pub valid_to: Option<DateTime<Utc>>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Row update time.
    pub updated_at: DateTime<Utc>,
}

impl Skill {
    /// Converts this artifact revision into tier-one pipeline metadata.
    pub fn metadata(&self) -> Result<SkillMetadata> {
        Ok(SkillMetadata {
            artifact_revision_uid: Some(self.skill_uid),
            path: build_skill_path(&self.name),
            name: self.name.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            allowed_tools: self.manifest.allowed_tools.clone(),
            actions: self
                .manifest
                .actions
                .iter()
                .map(|action| action.id.clone())
                .collect(),
            // The package manifest does not carry the canonical skill definition;
            // the live injection path derives this from the exact artifact revision.
            has_execution_plan: false,
            estimated_tokens: self.manifest.skill_md_estimated_tokens.max(1),
        })
    }
}

/// Stored skill package with its package files.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSkillPackage {
    /// Skill artifact revision metadata.
    pub skill: Skill,
    /// Sorted files belonging to this package revision.
    pub files: Vec<ValidatedSkillPackageFile>,
}

impl StoredSkillPackage {
    /// Returns the UTF-8 markdown from the required package `SKILL.md`.
    pub fn skill_markdown(&self) -> Result<&str> {
        let file = self
            .files
            .iter()
            .find(|file| file.path == SKILL_MD_PATH)
            .ok_or_else(|| {
                MoaError::StorageError("stored skill package missing SKILL.md".to_string())
            })?;
        std::str::from_utf8(&file.content)
            .map_err(|error| MoaError::StorageError(format!("stored SKILL.md is invalid: {error}")))
    }
}

/// Cached facade for skill packages stored as canonical artifacts.
pub struct SkillRegistry {
    pool: PgPool,
}

impl SkillRegistry {
    /// Creates a skill registry backed by the provided Postgres pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Returns every skill this scope currently serves.
    pub async fn load_for_scope(&self, scope: &ActionRuleScope) -> Result<Vec<Skill>> {
        let packages = self.load_packages_for_scope(scope).await?;
        Ok(packages.into_iter().map(|package| package.skill).collect())
    }

    /// Returns the sorted, de-duplicated names of the skills a tenant serves,
    /// without loading package file trees.
    ///
    /// Backed by a single serving-pointer listing query, so it is cheap enough
    /// for latency-critical paths (such as execution routing) that only need a
    /// coverage hint, unlike [`Self::list_for_pipeline`], which loads every
    /// package's full file tree.
    pub async fn list_skill_names(&self, tenant_id: TenantId) -> Result<Vec<String>> {
        let scope = tenant_artifact_scope(tenant_id);
        let registry = ArtifactRegistry::new(self.pool.clone());
        let summaries = registry.list_serving(&scope, ArtifactKind::Skill).await?;
        let mut names = summaries
            .into_iter()
            .map(|summary| summary.name)
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Returns tenant skill metadata for learning and regression helpers.
    pub async fn list_for_pipeline(&self, tenant_id: TenantId) -> Result<Vec<SkillMetadata>> {
        let scope = tenant_artifact_scope(tenant_id);
        let mut metadata = self
            .load_for_scope(&scope)
            .await?
            .iter()
            .map(Skill::metadata)
            .collect::<Result<Vec<_>>>()?;
        metadata.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(metadata)
    }

    /// Loads the full rendered `SKILL.md` markdown for a named tenant skill.
    pub async fn load_full(&self, tenant_id: TenantId, skill_name: &str) -> Result<String> {
        let scope = tenant_artifact_scope(tenant_id);
        let render_scope = tenant_memory_scope(tenant_id);
        let package = self
            .load_package_by_name(&scope, skill_name)
            .await?
            .ok_or_else(|| MoaError::StorageError(format!("skill not found: {skill_name}")))?;
        let skill_md = package.skill_markdown()?.to_string();
        crate::render::render(
            &package.skill,
            &skill_md,
            &render_scope,
            &crate::render::SkillRenderContext::new(self.pool.clone()),
        )
        .await
    }

    /// Loads the skill this scope serves under the provided name.
    pub async fn load_by_name(
        &self,
        scope: &ActionRuleScope,
        skill_name: &str,
    ) -> Result<Option<Skill>> {
        Ok(self
            .load_package_by_name(scope, skill_name)
            .await?
            .map(|package| package.skill))
    }

    /// Loads the UTF-8 `SKILL.md` file for a visible skill artifact revision.
    pub async fn load_skill_markdown(
        &self,
        scope: &ActionRuleScope,
        skill_uid: Uuid,
    ) -> Result<String> {
        let package = self
            .load_package_by_uid(scope, skill_uid)
            .await?
            .ok_or_else(|| MoaError::StorageError(format!("skill not found: {skill_uid}")))?;
        Ok(package.skill_markdown()?.to_string())
    }

    /// Loads the package this scope serves under the provided name.
    ///
    /// Serving is the type-owned pointer, so revision status alone never makes
    /// a skill resolvable here.
    pub async fn load_package_by_name(
        &self,
        scope: &ActionRuleScope,
        skill_name: &str,
    ) -> Result<Option<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let Some(revision) = registry
            .load_serving(scope, ArtifactKind::Skill, skill_name)
            .await?
        else {
            return Ok(None);
        };
        let files = registry.load_files(scope, revision.revision_uid).await?;
        stored_package_from_revision(&revision, files).map(Some)
    }

    /// Loads a package by exact skill artifact revision id.
    ///
    /// Accepts an executable `ready` or `superseded` revision with activation
    /// history, so a session pinned to an exact revision keeps working after a
    /// newer one activates. Draft and published revisions never qualify, and
    /// rollback makes an archived revision terminal even though its immutable
    /// activation history remains.
    pub(crate) async fn load_package_by_uid(
        &self,
        scope: &ActionRuleScope,
        skill_uid: Uuid,
    ) -> Result<Option<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let Some(revision) = registry.load_revision(scope, skill_uid).await? else {
            return Ok(None);
        };
        if revision.kind != ArtifactKind::Skill {
            return Ok(None);
        }
        if !matches!(
            revision.status,
            ArtifactStatus::Ready | ArtifactStatus::Superseded
        ) {
            return Err(MoaError::ValidationError(format!(
                "artifact revision {} is {} and is not executable skill content",
                revision.revision_uid, revision.status
            )));
        }
        let release_scope = TenantScope::from_action_rule_scope(scope)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;
        let served = registry
            .was_ever_activated(&release_scope, skill_uid)
            .await?;
        if !served {
            return Ok(None);
        }
        let files = registry.load_files(scope, revision.revision_uid).await?;
        stored_package_from_revision(&revision, files).map(Some)
    }

    /// Loads every package this scope serves.
    pub async fn load_packages_for_scope(
        &self,
        scope: &ActionRuleScope,
    ) -> Result<Vec<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let summaries = registry.list_serving(scope, ArtifactKind::Skill).await?;
        let mut packages = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let Some(revision) = registry.load_revision(scope, summary.revision_uid).await? else {
                continue;
            };
            let files = registry.load_files(scope, revision.revision_uid).await?;
            match stored_package_from_revision(&revision, files) {
                Ok(package) => packages.push(package),
                Err(MoaError::ValidationError(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        packages.sort_by(|left, right| left.skill.name.cmp(&right.skill.name));
        Ok(packages)
    }
}

fn tenant_memory_scope(tenant_id: TenantId) -> MemoryScope {
    MemoryScope::Tenant { tenant_id }
}

pub(crate) fn stored_package_from_revision(
    revision: &StoredArtifactRevision,
    files: Vec<ArtifactFile>,
) -> Result<StoredSkillPackage> {
    if revision.kind != ArtifactKind::Skill {
        return Err(MoaError::ValidationError(format!(
            "artifact revision {} is not a skill artifact",
            revision.revision_uid
        )));
    }
    if matches!(
        revision.status,
        ArtifactStatus::Draft | ArtifactStatus::Archived | ArtifactStatus::Published
    ) {
        return Err(MoaError::ValidationError(format!(
            "artifact revision {} is {} and is not executable skill content",
            revision.revision_uid, revision.status
        )));
    }
    let package_files = files
        .into_iter()
        .map(|file| SkillPackageFile {
            path: file.path,
            content: file.content,
            content_type: file.content_type,
            executable: file.executable,
        })
        .collect();
    let package = SkillPackage::new(package_files).validate()?;
    let skill = skill_from_package_revision(revision, &package)?;
    Ok(StoredSkillPackage {
        skill,
        files: package.files,
    })
}

fn skill_from_package_revision(
    revision: &StoredArtifactRevision,
    package: &ValidatedSkillPackage,
) -> Result<Skill> {
    Ok(Skill {
        skill_uid: revision.revision_uid,
        tenant_id: revision
            .storage_partition_id
            .as_ref()
            .map(|storage_partition_id| {
                Uuid::parse_str(storage_partition_id.as_str())
                    .map(TenantId::from)
                    .map_err(|error| {
                        MoaError::StorageError(format!(
                            "stored skill storage partition is not a tenant id: {error}"
                        ))
                    })
            })
            .transpose()?,
        user_id: revision.user_id.clone(),
        scope: revision.scope.clone(),
        name: package.name.clone(),
        description: package.description.clone(),
        package_hash: package.package_hash.clone(),
        skill_md_hash: package.skill_md_hash.clone(),
        file_count: package.file_count,
        total_size_bytes: package.total_size_bytes,
        manifest: package.manifest.clone(),
        version: revision.version,
        tags: package.tags.clone(),
        valid_to: revision.valid_to,
        created_at: revision.created_at,
        updated_at: revision.updated_at,
    })
}

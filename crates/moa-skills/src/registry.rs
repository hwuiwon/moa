//! Artifact-backed skill package registry with three-tier scoping.

use chrono::{DateTime, Utc};
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactFile, ArtifactRegistry, ArtifactScopeParts, NewPublishedArtifactRevision,
    StoredArtifactRevision, insert_published_revision,
};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::identifiers::TenantId, types::identifiers::UserId, types::memory::SkillMetadata,
};
use moa_db::ScopedConn;
use moa_memory_types::MemoryScope;
use sqlx::PgPool;
use uuid::Uuid;

use crate::artifact::{
    artifact_file_from_skill_file, skill_artifact_document_from_package, skill_artifact_source_text,
};
use crate::format::build_skill_path;
use crate::package::{
    SKILL_MD_PATH, SkillPackage, SkillPackageFile, SkillPackageManifest, ValidatedSkillPackage,
    ValidatedSkillPackageFile,
};
use crate::util::{artifact_scope_context, tenant_artifact_scope};

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

/// New skill package revision to publish as a skill artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSkill {
    /// Scope that owns the skill.
    pub scope: ActionRuleScope,
    /// Submitted package files.
    pub package: SkillPackage,
}

impl NewSkill {
    /// Builds an insertable skill from a submitted package.
    pub fn from_package(scope: ActionRuleScope, package: SkillPackage) -> Self {
        Self { scope, package }
    }

    /// Builds an insertable one-file skill package from rendered `SKILL.md` markdown.
    pub fn from_skill_markdown(scope: ActionRuleScope, markdown: String) -> Self {
        Self {
            scope,
            package: SkillPackage::from_skill_markdown(markdown),
        }
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

    /// Returns all visible published skills for the provided scope.
    pub async fn load_for_scope(&self, scope: &ActionRuleScope) -> Result<Vec<Skill>> {
        let packages = self.load_packages_for_scope(scope).await?;
        Ok(packages.into_iter().map(|package| package.skill).collect())
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

    /// Loads the most specific visible published skill matching the provided name.
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

    /// Loads the most specific visible package matching the provided name.
    pub async fn load_package_by_name(
        &self,
        scope: &ActionRuleScope,
        skill_name: &str,
    ) -> Result<Option<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let Some(revision) = registry
            .load_visible_published(scope, ArtifactKind::Skill, skill_name)
            .await?
        else {
            return Ok(None);
        };
        let files = registry.load_files(scope, revision.revision_uid).await?;
        stored_package_from_revision(&revision, files).map(Some)
    }

    /// Loads a visible package by skill artifact revision id.
    pub(crate) async fn load_package_by_uid(
        &self,
        scope: &ActionRuleScope,
        skill_uid: Uuid,
    ) -> Result<Option<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let Some(revision) = registry.load_revision(scope, skill_uid).await? else {
            return Ok(None);
        };
        if revision.kind != ArtifactKind::Skill || revision.status != ArtifactStatus::Published {
            return Ok(None);
        }
        let files = registry.load_files(scope, revision.revision_uid).await?;
        stored_package_from_revision(&revision, files).map(Some)
    }

    /// Loads all visible published packages from the provided scope.
    pub async fn load_packages_for_scope(
        &self,
        scope: &ActionRuleScope,
    ) -> Result<Vec<StoredSkillPackage>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let summaries = registry
            .list_visible(
                scope,
                Some(ArtifactKind::Skill),
                Some(ArtifactStatus::Published),
            )
            .await?;
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

    /// Publishes a new skill artifact revision.
    pub async fn create(&self, skill: NewSkill) -> Result<Uuid> {
        let skill = ValidatedNewSkill::from_new(skill)?;
        publish_skill_revision(&self.pool, &skill).await
    }

    /// Publishes a new artifact revision when the package changed, otherwise returns the current revision.
    pub async fn upsert_by_name(&self, skill: NewSkill) -> Result<Uuid> {
        let skill = ValidatedNewSkill::from_new(skill)?;
        if let Some(existing) = self
            .load_package_by_name(&skill.scope, &skill.package.name)
            .await?
            .filter(|existing| existing.skill.package_hash == skill.package.package_hash)
        {
            return Ok(existing.skill.skill_uid);
        }
        publish_skill_revision(&self.pool, &skill).await
    }
}

struct ValidatedNewSkill {
    scope: ActionRuleScope,
    package: ValidatedSkillPackage,
}

impl ValidatedNewSkill {
    fn from_new(skill: NewSkill) -> Result<Self> {
        Ok(Self {
            scope: skill.scope,
            package: skill.package.validate()?,
        })
    }
}

async fn publish_skill_revision(pool: &PgPool, skill: &ValidatedNewSkill) -> Result<Uuid> {
    let mut conn = ScopedConn::begin(pool, &artifact_scope_context(&skill.scope)).await?;
    let revision_uid = insert_skill_artifact(conn.as_mut(), skill).await?;
    conn.commit().await?;
    Ok(revision_uid)
}

fn tenant_memory_scope(tenant_id: TenantId) -> MemoryScope {
    MemoryScope::Tenant { tenant_id }
}

async fn insert_skill_artifact(
    conn: &mut sqlx::PgConnection,
    skill: &ValidatedNewSkill,
) -> Result<Uuid> {
    let document = skill_artifact_document_from_package(&skill.package, ArtifactStatus::Published)?;
    let source_text = skill_artifact_source_text(&skill.package, &document)?;
    let artifact_files = skill
        .package
        .files
        .iter()
        .map(artifact_file_from_skill_file)
        .collect::<Vec<_>>();

    insert_published_revision(
        conn,
        &ArtifactScopeParts::from_scope(&skill.scope),
        NewPublishedArtifactRevision {
            document: &document,
            source_format: "yaml",
            source_text: &source_text,
            files: &artifact_files,
            version: None,
        },
    )
    .await
}

fn stored_package_from_revision(
    revision: &StoredArtifactRevision,
    files: Vec<ArtifactFile>,
) -> Result<StoredSkillPackage> {
    if revision.kind != ArtifactKind::Skill {
        return Err(MoaError::ValidationError(format!(
            "artifact revision {} is not a skill artifact",
            revision.revision_uid
        )));
    }
    if revision.status != ArtifactStatus::Published {
        return Err(MoaError::ValidationError(format!(
            "artifact revision {} must be published before skill loading",
            revision.revision_uid
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

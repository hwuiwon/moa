//! Postgres-backed skill package registry with three-tier RLS scoping.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactScopeParts, NewArtifactFile, NewPublishedArtifactRevision, insert_published_revision,
};
use moa_core::{
    MemoryScope, MoaError, Result, ScopeContext, ScopedConn, SkillMetadata, UserId, WorkspaceId,
};
use moka::future::Cache;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::artifact::{SKILL_ARTIFACT_PATH, skill_artifact_document_from_package};
use crate::format::build_skill_path;
use crate::package::{
    SKILL_MD_PATH, SkillPackage, SkillPackageManifest, ValidatedSkillPackage,
    ValidatedSkillPackageFile,
};

const DEFAULT_CACHE_CAPACITY: u64 = 512;
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);

/// One active or historical skill package row loaded from `moa.skill`.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// Stable row identifier for this version of the skill.
    pub skill_uid: Uuid,
    /// Workspace owning workspace and user scoped skills.
    pub workspace_id: Option<WorkspaceId>,
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
    /// Package manifest stored with the skill row.
    pub manifest: SkillPackageManifest,
    /// Integer skill version for row-level supersession.
    pub version: i32,
    /// Previous skill version row, when this row superseded one.
    pub previous_skill_uid: Option<Uuid>,
    /// Search and ranking tags.
    pub tags: Vec<String>,
    /// Time when the row stopped being active.
    pub valid_to: Option<DateTime<Utc>>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Row update time.
    pub updated_at: DateTime<Utc>,
}

impl Skill {
    /// Converts this row into tier-one pipeline metadata.
    pub fn metadata(&self) -> Result<SkillMetadata> {
        Ok(SkillMetadata {
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
            estimated_tokens: self.manifest.skill_md_estimated_tokens.max(1),
            use_count: self.manifest.use_count,
            last_used: self.manifest.last_used,
            success_rate: self.manifest.success_rate,
            auto_generated: self.manifest.auto_generated,
        })
    }
}

/// New skill package version to insert into `moa.skill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSkill {
    /// Scope that owns the skill.
    pub scope: MemoryScope,
    /// Submitted package files.
    pub package: SkillPackage,
}

impl NewSkill {
    /// Builds an insertable skill from a submitted package.
    pub fn from_package(scope: MemoryScope, package: SkillPackage) -> Self {
        Self { scope, package }
    }

    /// Builds an insertable one-file skill package from rendered `SKILL.md` markdown.
    pub fn from_skill_markdown(scope: MemoryScope, markdown: String) -> Self {
        Self {
            scope,
            package: SkillPackage::from_skill_markdown(markdown),
        }
    }
}

/// Stored skill package with its package files.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSkillPackage {
    /// Skill package row.
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

/// Cached registry for workspace, user, and global skill packages stored in Postgres.
pub struct SkillRegistry {
    pool: PgPool,
    cache: Cache<MemoryScope, Vec<Skill>>,
}

impl SkillRegistry {
    /// Creates a skill registry backed by the provided Postgres pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cache: Cache::builder()
                .max_capacity(DEFAULT_CACHE_CAPACITY)
                .time_to_live(DEFAULT_CACHE_TTL)
                .build(),
        }
    }

    /// Returns all active skills visible from the provided scope.
    pub async fn load_for_scope(&self, scope: &MemoryScope) -> Result<Vec<Skill>> {
        if let Some(cached) = self.cache.get(scope).await {
            return Ok(cached);
        }

        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let rows = load_visible_skills(conn.as_mut(), scope).await?;
        conn.commit().await?;
        self.cache.insert(scope.clone(), rows.clone()).await;
        Ok(rows)
    }

    /// Returns workspace skill metadata for Stage 4 pipeline injection.
    pub async fn list_for_pipeline(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SkillMetadata>> {
        let scope = MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        };
        let mut metadata = self
            .load_for_scope(&scope)
            .await?
            .iter()
            .map(Skill::metadata)
            .collect::<Result<Vec<_>>>()?;
        metadata.sort_by(|left, right| {
            right
                .use_count
                .cmp(&left.use_count)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(metadata)
    }

    /// Loads the full `SKILL.md` markdown for a named workspace skill.
    pub async fn load_full(&self, workspace_id: &WorkspaceId, skill_name: &str) -> Result<String> {
        let scope = MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        };
        let skill = self
            .load_by_name(&scope, skill_name)
            .await?
            .ok_or_else(|| MoaError::StorageError(format!("skill not found: {skill_name}")))?;
        let skill_md = self.load_skill_markdown(&scope, skill.skill_uid).await?;
        crate::render::render(
            &skill,
            &skill_md,
            &scope,
            &crate::render::SkillRenderContext::new(self.pool.clone()),
        )
        .await
    }

    /// Loads the most specific active skill matching the provided name.
    pub async fn load_by_name(
        &self,
        scope: &MemoryScope,
        skill_name: &str,
    ) -> Result<Option<Skill>> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let skill = load_visible_skill_by_name(conn.as_mut(), scope, skill_name).await?;
        conn.commit().await?;
        Ok(skill)
    }

    /// Loads the UTF-8 `SKILL.md` file for a visible skill package revision.
    pub async fn load_skill_markdown(
        &self,
        scope: &MemoryScope,
        skill_uid: Uuid,
    ) -> Result<String> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let skill_md = load_skill_markdown(conn.as_mut(), skill_uid).await?;
        conn.commit().await?;
        Ok(skill_md)
    }

    /// Loads the most specific active package matching the provided name.
    pub async fn load_package_by_name(
        &self,
        scope: &MemoryScope,
        skill_name: &str,
    ) -> Result<Option<StoredSkillPackage>> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let Some(skill) = load_visible_skill_by_name(conn.as_mut(), scope, skill_name).await?
        else {
            conn.commit().await?;
            return Ok(None);
        };
        let files = load_skill_files(conn.as_mut(), skill.skill_uid).await?;
        conn.commit().await?;
        Ok(Some(StoredSkillPackage { skill, files }))
    }

    /// Loads a visible active package by skill revision id.
    pub async fn load_package_by_uid(
        &self,
        scope: &MemoryScope,
        skill_uid: Uuid,
    ) -> Result<Option<StoredSkillPackage>> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let Some(skill) = load_visible_skill_by_uid(conn.as_mut(), scope, skill_uid).await? else {
            conn.commit().await?;
            return Ok(None);
        };
        let files = load_skill_files(conn.as_mut(), skill.skill_uid).await?;
        conn.commit().await?;
        Ok(Some(StoredSkillPackage { skill, files }))
    }

    /// Loads all active packages visible from the provided scope.
    pub async fn load_packages_for_scope(
        &self,
        scope: &MemoryScope,
    ) -> Result<Vec<StoredSkillPackage>> {
        let mut conn = ScopedConn::begin(&self.pool, &ScopeContext::from(scope.clone())).await?;
        let skills = load_visible_skills(conn.as_mut(), scope).await?;
        let mut packages = Vec::with_capacity(skills.len());
        for skill in skills {
            let files = load_skill_files(conn.as_mut(), skill.skill_uid).await?;
            packages.push(StoredSkillPackage { skill, files });
        }
        conn.commit().await?;
        Ok(packages)
    }

    /// Creates a new skill package row without superseding an existing active row.
    pub async fn create(&self, skill: NewSkill) -> Result<Uuid> {
        let skill = ValidatedNewSkill::from_new(skill)?;
        let mut conn =
            ScopedConn::begin(&self.pool, &ScopeContext::from(skill.scope.clone())).await?;
        let uid = insert_skill(conn.as_mut(), &skill, 1, None).await?;
        conn.commit().await?;
        self.cache.invalidate_all();
        Ok(uid)
    }

    /// Inserts a skill package or creates a new active version when package files changed.
    pub async fn upsert_by_name(&self, skill: NewSkill) -> Result<Uuid> {
        let skill = ValidatedNewSkill::from_new(skill)?;
        let mut conn =
            ScopedConn::begin(&self.pool, &ScopeContext::from(skill.scope.clone())).await?;
        let uid = upsert_by_name(conn.as_mut(), &skill).await?;
        conn.commit().await?;
        self.cache.invalidate_all();
        Ok(uid)
    }
}

struct ValidatedNewSkill {
    scope: MemoryScope,
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

async fn load_visible_skill_by_uid(
    conn: &mut PgConnection,
    scope: &MemoryScope,
    skill_uid: Uuid,
) -> Result<Option<Skill>> {
    let (workspace_id, user_id) = scope_parts(scope);
    let row = sqlx::query(
        r#"
        SELECT skill_uid, workspace_id, user_id, scope, name, description,
               package_hash, skill_md_hash, file_count, total_size_bytes, manifest,
               version, previous_skill_uid, tags, valid_to, created_at, updated_at
        FROM moa.skill
        WHERE valid_to IS NULL
          AND skill_uid = $3
          AND (
            scope = 'global'
            OR (workspace_id = $1 AND user_id IS NULL)
            OR (workspace_id = $1 AND user_id = $2)
          )
        LIMIT 1
        "#,
    )
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(skill_uid)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?;

    row.as_ref().map(skill_from_row).transpose()
}

async fn load_visible_skills(conn: &mut PgConnection, scope: &MemoryScope) -> Result<Vec<Skill>> {
    let (workspace_id, user_id) = scope_parts(scope);
    let rows = sqlx::query(
        r#"
        SELECT skill_uid, workspace_id, user_id, scope, name, description,
               package_hash, skill_md_hash, file_count, total_size_bytes, manifest,
               version, previous_skill_uid, tags, valid_to, created_at, updated_at
        FROM moa.skill
        WHERE valid_to IS NULL
          AND (
            scope = 'global'
            OR (workspace_id = $1 AND user_id IS NULL)
            OR (workspace_id = $1 AND user_id = $2)
          )
        ORDER BY
          CASE scope WHEN 'global' THEN 0 WHEN 'workspace' THEN 1 ELSE 2 END,
          updated_at ASC,
          name ASC
        "#,
    )
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;

    let mut by_name = HashMap::new();
    for row in rows {
        let skill = skill_from_row(&row)?;
        by_name.insert(skill.name.clone(), skill);
    }

    let mut skills = by_name.into_values().collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

async fn load_visible_skill_by_name(
    conn: &mut PgConnection,
    scope: &MemoryScope,
    skill_name: &str,
) -> Result<Option<Skill>> {
    let (workspace_id, user_id) = scope_parts(scope);
    let row = sqlx::query(
        r#"
        SELECT skill_uid, workspace_id, user_id, scope, name, description,
               package_hash, skill_md_hash, file_count, total_size_bytes, manifest,
               version, previous_skill_uid, tags, valid_to, created_at, updated_at
        FROM moa.skill
        WHERE valid_to IS NULL
          AND name = $3
          AND (
            scope = 'global'
            OR (workspace_id = $1 AND user_id IS NULL)
            OR (workspace_id = $1 AND user_id = $2)
          )
        ORDER BY CASE scope WHEN 'user' THEN 2 WHEN 'workspace' THEN 1 ELSE 0 END DESC
        LIMIT 1
        "#,
    )
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(skill_name)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?;

    row.as_ref().map(skill_from_row).transpose()
}

async fn load_skill_markdown(conn: &mut PgConnection, skill_uid: Uuid) -> Result<String> {
    let content = sqlx::query_scalar::<_, Vec<u8>>(
        r#"
        SELECT content
        FROM moa.skill_file
        WHERE skill_uid = $1
          AND path = $2
        "#,
    )
    .bind(skill_uid)
    .bind(SKILL_MD_PATH)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?
    .ok_or_else(|| MoaError::StorageError(format!("SKILL.md not found for skill {skill_uid}")))?;

    String::from_utf8(content).map_err(|error| {
        MoaError::StorageError(format!(
            "stored SKILL.md for skill {skill_uid} is invalid: {error}"
        ))
    })
}

async fn load_skill_files(
    conn: &mut PgConnection,
    skill_uid: Uuid,
) -> Result<Vec<ValidatedSkillPackageFile>> {
    let rows = sqlx::query(
        r#"
        SELECT path, content, content_sha256, content_type, executable, file_size_bytes
        FROM moa.skill_file
        WHERE skill_uid = $1
        ORDER BY path ASC
        "#,
    )
    .bind(skill_uid)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;

    rows.into_iter()
        .map(|row| skill_file_from_row(&row))
        .collect()
}

async fn upsert_by_name(conn: &mut PgConnection, skill: &ValidatedNewSkill) -> Result<Uuid> {
    let (workspace_id, user_id) = scope_parts(&skill.scope);
    let active = sqlx::query(
        r#"
        SELECT skill_uid, package_hash, version
        FROM moa.skill
        WHERE valid_to IS NULL
          AND workspace_id IS NOT DISTINCT FROM $1
          AND user_id IS NOT DISTINCT FROM $2
          AND name = $3
        FOR UPDATE
        "#,
    )
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(&skill.package.name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    if let Some(row) = active {
        let existing_hash: Vec<u8> = row.try_get("package_hash").map_err(map_sqlx_error)?;
        let existing_uid: Uuid = row.try_get("skill_uid").map_err(map_sqlx_error)?;
        let existing_version: i32 = row.try_get("version").map_err(map_sqlx_error)?;
        if existing_hash == skill.package.package_hash {
            return Ok(existing_uid);
        }

        sqlx::query(
            "UPDATE moa.skill SET valid_to = now(), updated_at = now() WHERE skill_uid = $1",
        )
        .bind(existing_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        return insert_skill(
            conn,
            skill,
            existing_version.saturating_add(1),
            Some(existing_uid),
        )
        .await;
    }

    insert_skill(conn, skill, 1, None).await
}

async fn insert_skill(
    conn: &mut PgConnection,
    skill: &ValidatedNewSkill,
    version: i32,
    previous_skill_uid: Option<Uuid>,
) -> Result<Uuid> {
    let (workspace_id, user_id) = scope_parts(&skill.scope);
    let skill_uid = Uuid::now_v7();
    let manifest = serde_json::to_value(&skill.package.manifest)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    sqlx::query(
        r#"
        INSERT INTO moa.skill (
            skill_uid, workspace_id, user_id, name, description, package_hash,
            skill_md_hash, file_count, total_size_bytes, manifest, version,
            previous_skill_uid, tags
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        "#,
    )
    .bind(skill_uid)
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(&skill.package.name)
    .bind(&skill.package.description)
    .bind(&skill.package.package_hash)
    .bind(&skill.package.skill_md_hash)
    .bind(skill.package.file_count)
    .bind(skill.package.total_size_bytes)
    .bind(manifest)
    .bind(version)
    .bind(previous_skill_uid)
    .bind(&skill.package.tags)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    for file in &skill.package.files {
        sqlx::query(
            r#"
            INSERT INTO moa.skill_file (
                file_uid, skill_uid, workspace_id, user_id, path, content,
                content_sha256, content_type, executable, file_size_bytes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(skill_uid)
        .bind(workspace_id.as_deref())
        .bind(user_id.as_deref())
        .bind(&file.path)
        .bind(&file.content)
        .bind(&file.content_sha256)
        .bind(file.content_type.as_deref())
        .bind(file.executable)
        .bind(file.file_size_bytes)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    }

    insert_skill_artifact(conn, skill, version).await?;

    Ok(skill_uid)
}

async fn insert_skill_artifact(
    conn: &mut PgConnection,
    skill: &ValidatedNewSkill,
    version: i32,
) -> Result<()> {
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
            version: Some(version),
        },
    )
    .await
    .map(|_| ())
}

fn artifact_file_from_skill_file(file: &ValidatedSkillPackageFile) -> NewArtifactFile {
    NewArtifactFile {
        path: file.path.clone(),
        content: file.content.clone(),
        content_type: file.content_type.clone(),
        executable: file.executable,
    }
}

fn skill_artifact_source_text(
    package: &ValidatedSkillPackage,
    document: &ArtifactDocument,
) -> Result<Vec<u8>> {
    if let Some(file) = package
        .files
        .iter()
        .find(|file| file.path == SKILL_ARTIFACT_PATH)
    {
        return Ok(file.content.clone());
    }
    document
        .to_yaml()
        .map(String::into_bytes)
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

fn scope_parts(scope: &MemoryScope) -> (Option<String>, Option<String>) {
    (
        scope
            .workspace_id()
            .map(|workspace_id| workspace_id.to_string()),
        scope.user_id().map(|user_id| user_id.to_string()),
    )
}

fn skill_from_row(row: &sqlx::postgres::PgRow) -> Result<Skill> {
    let manifest_value: serde_json::Value = row.try_get("manifest").map_err(map_sqlx_error)?;
    Ok(Skill {
        skill_uid: row.try_get("skill_uid").map_err(map_sqlx_error)?,
        workspace_id: row
            .try_get::<Option<String>, _>("workspace_id")
            .map_err(map_sqlx_error)?
            .map(WorkspaceId::new),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(map_sqlx_error)?
            .map(UserId::new),
        scope: row.try_get("scope").map_err(map_sqlx_error)?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        description: row.try_get("description").map_err(map_sqlx_error)?,
        package_hash: row.try_get("package_hash").map_err(map_sqlx_error)?,
        skill_md_hash: row.try_get("skill_md_hash").map_err(map_sqlx_error)?,
        file_count: row.try_get("file_count").map_err(map_sqlx_error)?,
        total_size_bytes: row.try_get("total_size_bytes").map_err(map_sqlx_error)?,
        manifest: serde_json::from_value(manifest_value)
            .map_err(|error| MoaError::StorageError(error.to_string()))?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        previous_skill_uid: row.try_get("previous_skill_uid").map_err(map_sqlx_error)?,
        tags: row
            .try_get::<Option<Vec<String>>, _>("tags")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        valid_to: row.try_get("valid_to").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn skill_file_from_row(row: &sqlx::postgres::PgRow) -> Result<ValidatedSkillPackageFile> {
    Ok(ValidatedSkillPackageFile {
        path: row.try_get("path").map_err(map_sqlx_error)?,
        content: row.try_get("content").map_err(map_sqlx_error)?,
        content_sha256: row.try_get("content_sha256").map_err(map_sqlx_error)?,
        content_type: row.try_get("content_type").map_err(map_sqlx_error)?,
        executable: row.try_get("executable").map_err(map_sqlx_error)?,
        file_size_bytes: row.try_get("file_size_bytes").map_err(map_sqlx_error)?,
    })
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

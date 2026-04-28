//! Postgres-backed skill registry with three-tier RLS scoping.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{
    MemoryScope, MoaError, Result, ScopeContext, ScopedConn, SkillMetadata, UserId, WorkspaceId,
};
use moka::future::Cache;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::format::{
    SkillDocument, build_skill_path, parse_skill_markdown, skill_metadata_from_document,
};

const DEFAULT_CACHE_CAPACITY: u64 = 512;
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);

/// One active or historical skill row loaded from `moa.skill`.
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
    pub description: Option<String>,
    /// Full `SKILL.md` markdown, including YAML frontmatter.
    pub body: String,
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
        let document = parse_skill_markdown(&self.body)?;
        Ok(skill_metadata_from_document(
            build_skill_path(&self.name),
            &document,
        ))
    }
}

/// New skill version to insert into `moa.skill`.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSkill {
    /// Scope that owns the skill.
    pub scope: MemoryScope,
    /// Stable skill name.
    pub name: String,
    /// Human-readable skill description.
    pub description: Option<String>,
    /// Full `SKILL.md` markdown, including YAML frontmatter.
    pub body: String,
    /// Search and ranking tags.
    pub tags: Vec<String>,
}

impl NewSkill {
    /// Builds an insertable skill from a parsed skill document.
    pub fn from_document(scope: MemoryScope, document: &SkillDocument, markdown: String) -> Self {
        Self {
            scope,
            name: document.frontmatter.name.clone(),
            description: Some(document.frontmatter.description.clone()),
            body: markdown,
            tags: document.frontmatter.tags(),
        }
    }
}

/// Cached registry for workspace, user, and global skills stored in Postgres.
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
        crate::render::render(
            &skill,
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

    /// Creates a new skill row without superseding an existing active row.
    pub async fn create(&self, skill: NewSkill) -> Result<Uuid> {
        validate_new_skill(&skill)?;
        let mut conn =
            ScopedConn::begin(&self.pool, &ScopeContext::from(skill.scope.clone())).await?;
        let uid = insert_skill(conn.as_mut(), &skill, 1, None).await?;
        conn.commit().await?;
        self.cache.invalidate_all();
        Ok(uid)
    }

    /// Inserts a skill or creates a new active version when the body changed.
    pub async fn upsert_by_name(&self, skill: NewSkill) -> Result<Uuid> {
        validate_new_skill(&skill)?;
        let mut conn =
            ScopedConn::begin(&self.pool, &ScopeContext::from(skill.scope.clone())).await?;
        let uid = upsert_by_name(conn.as_mut(), &skill).await?;
        conn.commit().await?;
        self.cache.invalidate_all();
        Ok(uid)
    }
}

async fn load_visible_skills(conn: &mut PgConnection, scope: &MemoryScope) -> Result<Vec<Skill>> {
    let (workspace_id, user_id) = scope_parts(scope);
    let rows = sqlx::query(
        r#"
        SELECT skill_uid, workspace_id, user_id, scope, name, description, body,
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
        SELECT skill_uid, workspace_id, user_id, scope, name, description, body,
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

async fn upsert_by_name(conn: &mut PgConnection, skill: &NewSkill) -> Result<Uuid> {
    let (workspace_id, user_id) = scope_parts(&skill.scope);
    let hash = body_hash(&skill.body);
    let active = sqlx::query(
        r#"
        SELECT skill_uid, body_hash, version
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
    .bind(&skill.name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    if let Some(row) = active {
        let existing_hash: Vec<u8> = row.try_get("body_hash").map_err(map_sqlx_error)?;
        let existing_uid: Uuid = row.try_get("skill_uid").map_err(map_sqlx_error)?;
        let existing_version: i32 = row.try_get("version").map_err(map_sqlx_error)?;
        if existing_hash == hash {
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
    skill: &NewSkill,
    version: i32,
    previous_skill_uid: Option<Uuid>,
) -> Result<Uuid> {
    let (workspace_id, user_id) = scope_parts(&skill.scope);
    let skill_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.skill (
            skill_uid, workspace_id, user_id, name, description, body, body_hash,
            version, previous_skill_uid, tags
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(skill_uid)
    .bind(workspace_id.as_deref())
    .bind(user_id.as_deref())
    .bind(&skill.name)
    .bind(skill.description.as_deref())
    .bind(&skill.body)
    .bind(body_hash(&skill.body))
    .bind(version)
    .bind(previous_skill_uid)
    .bind(&skill.tags)
    .execute(conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(skill_uid)
}

fn validate_new_skill(skill: &NewSkill) -> Result<()> {
    let document = parse_skill_markdown(&skill.body)?;
    if document.frontmatter.name != skill.name {
        return Err(MoaError::ValidationError(format!(
            "skill body name `{}` does not match row name `{}`",
            document.frontmatter.name, skill.name
        )));
    }
    Ok(())
}

fn scope_parts(scope: &MemoryScope) -> (Option<String>, Option<String>) {
    (
        scope
            .workspace_id()
            .map(|workspace_id| workspace_id.to_string()),
        scope.user_id().map(|user_id| user_id.to_string()),
    )
}

fn body_hash(body: &str) -> Vec<u8> {
    Sha256::digest(body.as_bytes()).to_vec()
}

fn skill_from_row(row: &sqlx::postgres::PgRow) -> Result<Skill> {
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
        body: row.try_get("body").map_err(map_sqlx_error)?,
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

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{NewSkill, body_hash};
    use crate::parse_skill_markdown;

    const SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate OAuth refresh bugs"
metadata:
  moa-one-liner: "Debug refresh-token failures"
  moa-estimated-tokens: "42"
---

# Debug OAuth refresh

Check token refresh state.
"#;

    #[test]
    fn new_skill_from_document_preserves_metadata_fields() {
        let document = parse_skill_markdown(SKILL).expect("skill markdown should parse");
        let new_skill =
            NewSkill::from_document(moa_core::MemoryScope::Global, &document, SKILL.to_string());

        assert_eq!(new_skill.name, "debug-oauth-refresh");
        assert_eq!(
            new_skill.description.as_deref(),
            Some("Investigate OAuth refresh bugs")
        );
    }

    #[test]
    fn body_hash_is_stable_sha256() {
        assert_eq!(body_hash("abc").len(), 32);
        assert_eq!(body_hash("abc"), body_hash("abc"));
        assert_ne!(body_hash("abc"), body_hash("abcd"));
    }
}

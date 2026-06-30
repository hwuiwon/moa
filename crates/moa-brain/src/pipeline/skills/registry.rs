//! Skill registry reads and database-row conversion.

use std::collections::HashMap;

use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument};
use moa_core::{
    AgentSkillPolicyMode, MoaError, ResolvedArtifactRevisionRef, Result, SandboxFile,
    SkillMetadata, WorkingContext, estimate_text_tokens,
};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(super) async fn load_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let skill_policy = ctx
        .agent_policy_snapshot()?
        .map(|snapshot| snapshot.skill_policy)
        .unwrap_or_default();
    let locked_skills = locked_skill_dependencies(ctx, &skill_policy.refs);
    if matches!(skill_policy.mode, AgentSkillPolicyMode::Allowlist) && !locked_skills.is_empty() {
        return load_locked_skills(pool, ctx, &locked_skills).await;
    }
    let mut skills = load_visible_skills(pool, ctx).await?;
    if matches!(skill_policy.mode, AgentSkillPolicyMode::Pinned) && !locked_skills.is_empty() {
        let locked = load_locked_skills(pool, ctx, &locked_skills).await?;
        for locked_skill in locked {
            skills.retain(|skill| skill.name != locked_skill.name);
            skills.push(locked_skill);
        }
        skills.sort_by(|left, right| left.name.cmp(&right.name));
    }
    Ok(skills)
}

async fn load_visible_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let tenant_id = ctx.tenant_id.to_string();
    let rows = sqlx::query(
        r#"
        SELECT DISTINCT ON (a.name)
               a.name, a.description, a.tags, r.revision_uid, r.definition, r.source_text
        FROM moa.artifact a
        JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
        WHERE a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = 'skill'
          AND r.status = 'published'
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
        ORDER BY a.name ASC,
                 r.version DESC
        "#,
    )
    .bind(&tenant_id)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut skills = rows
        .into_iter()
        .map(skill_metadata_from_row)
        .collect::<Result<Vec<_>>>()?;
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

async fn load_locked_skills(
    pool: &PgPool,
    ctx: &WorkingContext,
    locked_skills: &[ResolvedArtifactRevisionRef],
) -> Result<Vec<SkillMetadata>> {
    let tenant_id = ctx.tenant_id.to_string();
    let revision_uids = locked_skills
        .iter()
        .map(|dependency| dependency.revision_uid)
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        r#"
        SELECT a.name, a.description, a.tags, r.revision_uid, r.definition, r.source_text
        FROM moa.artifact_revision r
        JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
        WHERE r.revision_uid = ANY($2::uuid[])
          AND a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = 'skill'
          AND r.status = 'published'
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
        ORDER BY array_position($2::uuid[], r.revision_uid), a.name ASC
        "#,
    )
    .bind(&tenant_id)
    .bind(&revision_uids)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    if rows.len() != revision_uids.len() {
        return Err(MoaError::StorageError(format!(
            "agent policy locked {} skill revisions but {} are visible",
            revision_uids.len(),
            rows.len()
        )));
    }

    rows.into_iter().map(skill_metadata_from_row).collect()
}

fn locked_skill_dependencies(
    ctx: &WorkingContext,
    policy_refs: &[String],
) -> Vec<ResolvedArtifactRevisionRef> {
    let mut dependencies = ctx
        .agent_context
        .as_ref()
        .map(|agent| {
            agent
                .artifact_dependencies
                .iter()
                .filter(|dependency| {
                    dependency.kind == "skill"
                        && policy_refs
                            .iter()
                            .any(|policy_ref| policy_ref == &dependency.reference)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    dependencies.sort_by(|left, right| left.reference.cmp(&right.reference));
    dependencies
}

pub(super) async fn load_selected_skill_files(
    pool: &PgPool,
    ctx: &WorkingContext,
    selected: &[SkillMetadata],
) -> Result<Vec<SandboxFile>> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }

    let tenant_id = ctx.tenant_id.to_string();
    let selected_names = selected
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    let selected_revision_uids = selected
        .iter()
        .map(|skill| skill.artifact_revision_uid)
        .collect::<Option<Vec<Uuid>>>();
    let base_paths = selected
        .iter()
        .map(|skill| {
            Ok((
                skill.name.clone(),
                skill_base_path(&skill.path)?.to_string(),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let rows = if let Some(revision_uids) = selected_revision_uids {
        sqlx::query(
            r#"
            WITH requested AS (
                SELECT revision_uid, ord
                FROM unnest($2::uuid[]) WITH ORDINALITY AS requested(revision_uid, ord)
            )
            SELECT a.name, f.path, f.content, f.executable
            FROM requested
            JOIN moa.artifact_revision r ON r.revision_uid = requested.revision_uid
            JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
            JOIN moa.artifact_file f ON f.revision_uid = r.revision_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND a.kind = 'skill'
              AND r.status = 'published'
              AND a.storage_partition_id = $1
              AND a.user_id IS NULL
            ORDER BY requested.ord ASC, f.path ASC
            "#,
        )
        .bind(&tenant_id)
        .bind(&revision_uids)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            r#"
        WITH requested AS (
            SELECT name, ord
            FROM unnest($2::text[]) WITH ORDINALITY AS requested(name, ord)
        ),
        visible AS (
            SELECT a.name, r.revision_uid, requested.ord,
                   row_number() OVER (
                       PARTITION BY a.name
                       ORDER BY r.version DESC
                   ) AS rank
            FROM requested
            JOIN moa.artifact a ON a.name = requested.name
            JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND a.kind = 'skill'
              AND r.status = 'published'
              AND a.storage_partition_id = $1
              AND a.user_id IS NULL
        )
        SELECT visible.name, f.path, f.content, f.executable
        FROM visible
        JOIN moa.artifact_file f ON f.revision_uid = visible.revision_uid
        WHERE visible.rank = 1
        ORDER BY visible.ord ASC, f.path ASC
        "#,
        )
        .bind(&tenant_id)
        .bind(&selected_names)
        .fetch_all(pool)
        .await
    }
    .map_err(|error| MoaError::StorageError(error.to_string()))?;

    let mut files = Vec::new();
    for row in rows {
        let name: String = row
            .try_get("name")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        let Some(base_path) = base_paths.get(&name) else {
            continue;
        };
        let package_path: String = row
            .try_get("path")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        files.push(SandboxFile {
            path: format!("{base_path}/{package_path}"),
            content: row
                .try_get("content")
                .map_err(|error| MoaError::StorageError(error.to_string()))?,
            executable: row
                .try_get("executable")
                .map_err(|error| MoaError::StorageError(error.to_string()))?,
        });
    }

    Ok(files)
}

fn skill_metadata_from_row(row: sqlx::postgres::PgRow) -> Result<SkillMetadata> {
    let name: String = row.try_get("name").map_err(map_sqlx_error)?;
    let description: String = row.try_get("description").map_err(map_sqlx_error)?;
    let tags: Vec<String> = row.try_get("tags").map_err(map_sqlx_error)?;
    let definition: Value = row.try_get("definition").map_err(map_sqlx_error)?;
    let source_text: Vec<u8> = row.try_get("source_text").map_err(map_sqlx_error)?;
    let source_text = String::from_utf8(source_text)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let revision_uid: Uuid = row.try_get("revision_uid").map_err(map_sqlx_error)?;
    let document: ArtifactDocument = serde_json::from_value(definition)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let ArtifactDefinition::Skill(skill) = document.definition else {
        return Err(MoaError::StorageError(format!(
            "artifact `{name}` is not a skill definition"
        )));
    };
    let instruction_path = if skill.instructions.path.trim().is_empty() {
        "SKILL.md"
    } else {
        skill.instructions.path.as_str()
    };
    let actions = skill
        .actions
        .into_iter()
        .map(|action| action.id)
        .collect::<Vec<_>>();
    Ok(SkillMetadata {
        artifact_revision_uid: Some(revision_uid),
        path: format!(
            ".moa/skills/{}/{}",
            slugify_skill_name(&name),
            instruction_path
        ),
        name,
        description,
        tags,
        allowed_tools: skill.allowed_tools,
        actions,
        estimated_tokens: estimate_text_tokens(&source_text).max(1),
    })
}

fn skill_base_path(skill_md_path: &str) -> Result<&str> {
    skill_md_path
        .rsplit_once('/')
        .map(|(base, _)| base)
        .ok_or_else(|| {
            MoaError::ValidationError(format!(
                "skill path `{skill_md_path}` must include a sandbox package path"
            ))
        })
}

fn slugify_skill_name(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

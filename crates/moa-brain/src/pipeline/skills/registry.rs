//! Skill registry reads and database-row conversion.

use std::collections::HashMap;

use moa_core::{MoaError, Result, SandboxFile, SkillMetadata, WorkingContext};
use serde_json::Value;
use sqlx::{PgPool, Row};

pub(super) async fn load_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let rows = sqlx::query(
        r#"
        SELECT name, description, tags, manifest
        FROM moa.skill
        WHERE valid_to IS NULL
          AND (
            scope = 'global'
            OR (workspace_id = $1 AND user_id IS NULL)
            OR (workspace_id = $1 AND user_id = $2)
          )
        ORDER BY CASE scope WHEN 'user' THEN 2 WHEN 'workspace' THEN 1 ELSE 0 END DESC,
                 name ASC
        "#,
    )
    .bind(ctx.workspace_id.as_str())
    .bind(ctx.user_id.as_str())
    .fetch_all(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;

    let mut by_name = HashMap::new();
    for row in rows {
        let name: String = row
            .try_get("name")
            .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
        by_name
            .entry(name.clone())
            .or_insert(skill_metadata_from_row(row)?);
    }

    let mut skills = by_name.into_values().collect::<Vec<_>>();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

pub(super) async fn load_selected_skill_files(
    pool: &PgPool,
    ctx: &WorkingContext,
    selected: &[SkillMetadata],
) -> Result<Vec<SandboxFile>> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }

    let selected_names = selected
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<Vec<_>>();
    let base_paths = selected
        .iter()
        .map(|skill| {
            Ok((
                skill.name.clone(),
                skill_base_path(&skill.path)?.to_string(),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let rows = sqlx::query(
        r#"
        WITH requested AS (
            SELECT name, ord
            FROM unnest($3::text[]) WITH ORDINALITY AS requested(name, ord)
        ),
        visible AS (
            SELECT s.skill_uid, s.name, requested.ord,
                   row_number() OVER (
                       PARTITION BY s.name
                       ORDER BY CASE s.scope WHEN 'user' THEN 2 WHEN 'workspace' THEN 1 ELSE 0 END DESC
                   ) AS rank
            FROM requested
            JOIN moa.skill s ON s.name = requested.name
            WHERE s.valid_to IS NULL
              AND (
                s.scope = 'global'
                OR (s.workspace_id = $1 AND s.user_id IS NULL)
                OR (s.workspace_id = $1 AND s.user_id = $2)
              )
        )
        SELECT visible.name, f.path, f.content, f.executable
        FROM visible
        JOIN moa.skill_file f ON f.skill_uid = visible.skill_uid
        WHERE visible.rank = 1
        ORDER BY visible.ord ASC, f.path ASC
        "#,
    )
    .bind(ctx.workspace_id.as_str())
    .bind(ctx.user_id.as_str())
    .bind(&selected_names)
    .fetch_all(pool)
    .await
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
    let name: String = row
        .try_get("name")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let description: String = row
        .try_get("description")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let tags: Vec<String> = row
        .try_get("tags")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let manifest: Value = row
        .try_get("manifest")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(SkillMetadata {
        path: format!(".moa/skills/{}/SKILL.md", slugify_skill_name(&name)),
        name,
        description,
        tags,
        allowed_tools: manifest_string_vec(&manifest, "allowed_tools"),
        actions: manifest_action_ids(&manifest),
        estimated_tokens: manifest_usize(&manifest, "skill_md_estimated_tokens").max(1),
        use_count: manifest_u32(&manifest, "use_count"),
        last_used: manifest_datetime(&manifest, "last_used"),
        success_rate: manifest_f32(&manifest, "success_rate").unwrap_or(1.0),
        auto_generated: manifest_bool(&manifest, "auto_generated"),
    })
}

fn skill_base_path(skill_md_path: &str) -> Result<&str> {
    skill_md_path.strip_suffix("/SKILL.md").ok_or_else(|| {
        MoaError::ValidationError(format!(
            "skill path `{skill_md_path}` must end with /SKILL.md"
        ))
    })
}

fn slugify_skill_name(value: &str) -> String {
    let mut slug = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

fn manifest_string_vec(manifest: &Value, key: &str) -> Vec<String> {
    manifest
        .get(key)
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_action_ids(manifest: &Value) -> Vec<String> {
    manifest
        .get("actions")
        .and_then(Value::as_array)
        .map(|actions| {
            actions
                .iter()
                .filter_map(|action| action.get("id").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_usize(manifest: &Value, key: &str) -> usize {
    manifest
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(1)
}

fn manifest_u32(manifest: &Value, key: &str) -> u32 {
    manifest
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

fn manifest_f32(manifest: &Value, key: &str) -> Option<f32> {
    manifest
        .get(key)
        .and_then(Value::as_f64)
        .map(|value| value as f32)
        .filter(|value| value.is_finite())
}

fn manifest_bool(manifest: &Value, key: &str) -> bool {
    manifest.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn manifest_datetime(manifest: &Value, key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    manifest
        .get(key)
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc))
}

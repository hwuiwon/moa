//! Skill registry reads and database-row conversion.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::{Result, SkillMetadata, WorkingContext};
use sqlx::{PgPool, Row};

pub(super) async fn load_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let rows = sqlx::query(
        r#"
        SELECT name, COALESCE(description, '') AS description, body, tags, updated_at
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

fn skill_metadata_from_row(row: sqlx::postgres::PgRow) -> Result<SkillMetadata> {
    let name: String = row
        .try_get("name")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let description: String = row
        .try_get("description")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let body: String = row
        .try_get("body")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let tags: Vec<String> = row
        .try_get("tags")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(SkillMetadata {
        path: format!("skills/{}/SKILL.md", slugify_skill_name(&name)),
        name,
        description,
        tags,
        allowed_tools: Vec::new(),
        estimated_tokens: estimate_skill_tokens(&body),
        use_count: 0,
        last_used: Some(updated_at),
        success_rate: 1.0,
        auto_generated: false,
    })
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
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

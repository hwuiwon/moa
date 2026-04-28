//! Skill rendering with linked graph lessons.

use moa_core::{MemoryScope, MoaError, Result, ScopeContext, ScopedConn};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::registry::Skill;

const DEFAULT_ADDENDUM_LIMIT: i64 = 5;

/// Context for rendering skills with visible learned lesson addenda.
#[derive(Clone)]
pub struct SkillRenderContext {
    pool: PgPool,
    addendum_limit: i64,
    assume_app_role: bool,
}

impl SkillRenderContext {
    /// Creates a skill renderer backed by the provided Postgres pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            addendum_limit: DEFAULT_ADDENDUM_LIMIT,
            assume_app_role: false,
        }
    }

    /// Creates a renderer that assumes `moa_app` inside each render transaction.
    pub fn for_app_role(pool: PgPool) -> Self {
        Self {
            pool,
            addendum_limit: DEFAULT_ADDENDUM_LIMIT,
            assume_app_role: true,
        }
    }

    /// Sets the maximum number of lesson addenda to prepend.
    pub fn with_addendum_limit(mut self, limit: i64) -> Self {
        self.addendum_limit = limit.max(0);
        self
    }

    /// Returns the Postgres pool used by this renderer.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Renders a skill body with visible learned lesson addenda prepended.
pub async fn render(
    skill: &Skill,
    scope: &MemoryScope,
    ctx: &SkillRenderContext,
) -> Result<String> {
    let mut conn = ScopedConn::begin(&ctx.pool, &ScopeContext::from(scope.clone())).await?;
    if ctx.assume_app_role {
        set_app_role(conn.as_mut()).await?;
    }
    let addenda = load_addenda(conn.as_mut(), skill.skill_uid, ctx.addendum_limit).await?;
    conn.commit().await?;

    if addenda.is_empty() {
        return Ok(skill.body.clone());
    }

    let mut out = String::with_capacity(skill.body.len() + addenda.len() * 96);
    out.push_str("<!-- learned lessons -->\n");
    for addendum in addenda {
        out.push_str("- ");
        out.push_str(&addendum.summary);
        out.push_str(" (lesson: ");
        out.push_str(&addendum.linked_lesson_uid.to_string());
        out.push_str(")\n");
    }
    out.push_str("\n---\n\n");
    out.push_str(&skill.body);
    Ok(out)
}

async fn load_addenda(
    conn: &mut PgConnection,
    skill_uid: Uuid,
    limit: i64,
) -> Result<Vec<SkillAddendum>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        r#"
        SELECT addendum.summary, addendum.linked_lesson_uid
        FROM moa.skill_addendum addendum
        JOIN moa.node_index lesson
          ON lesson.uid = addendum.linked_lesson_uid
        WHERE addendum.skill_uid = $1
          AND addendum.valid_to IS NULL
          AND lesson.valid_to IS NULL
        ORDER BY addendum.created_at DESC
        LIMIT $2
        "#,
    )
    .bind(skill_uid)
    .bind(limit)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;

    rows.into_iter()
        .map(|row| {
            Ok(SkillAddendum {
                summary: row.try_get("summary").map_err(map_sqlx_error)?,
                linked_lesson_uid: row.try_get("linked_lesson_uid").map_err(map_sqlx_error)?,
            })
        })
        .collect()
}

async fn set_app_role(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[derive(Debug)]
struct SkillAddendum {
    summary: String,
    linked_lesson_uid: Uuid,
}

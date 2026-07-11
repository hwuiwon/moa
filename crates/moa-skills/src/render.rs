//! Skill rendering with linked graph lessons.

use std::collections::BTreeSet;

use moa_core::error::Result;
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use moa_memory_types::MemoryScope;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::registry::Skill;
use crate::util::{map_sqlx_error, set_app_role};

const DEFAULT_ADDENDUM_LIMIT: i64 = 5;

/// Context for rendering skills with visible learned lessons.
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

    /// Sets the maximum number of graph lessons to prepend.
    pub fn with_addendum_limit(mut self, limit: i64) -> Self {
        self.addendum_limit = limit.max(0);
        self
    }

    /// Returns the Postgres pool used by this renderer.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Renders skill markdown with visible learned graph lessons prepended.
pub async fn render(
    skill: &Skill,
    skill_md: &str,
    scope: &MemoryScope,
    ctx: &SkillRenderContext,
) -> Result<String> {
    let mut conn = ScopedConn::begin(&ctx.pool, &RlsContext::from(scope.clone())).await?;
    if ctx.assume_app_role {
        set_app_role(conn.as_mut()).await?;
    }
    let addenda = load_addenda(conn.as_mut(), skill.skill_uid, ctx.addendum_limit).await?;
    conn.commit().await?;

    if addenda.is_empty() {
        return Ok(skill_md.to_string());
    }

    let mut out = String::with_capacity(skill_md.len() + addenda.len() * 96);
    out.push_str("<!-- learned lessons -->\n");
    for addendum in addenda {
        out.push_str("- ");
        out.push_str(&addendum.summary);
        out.push_str(" (lesson: ");
        out.push_str(&addendum.linked_lesson_uid.to_string());
        out.push_str(")\n");
    }
    out.push_str("\n---\n\n");
    out.push_str(skill_md);
    Ok(out)
}

/// Loads the newest active lessons for a skill, deduplicated by normalized
/// summary, capped at `limit`.
///
/// The `moa-memory-lifecycle` lesson-curation pass merges duplicate-summary
/// lessons into one canonical node, so most of the time only distinct summaries
/// are active here. That pass is periodic, though, and this render can run
/// between passes while duplicates are still active, so the newest-wins dedup
/// below keeps a burst of restatements from crowding out distinct lessons in
/// the rendered addenda.
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
        SELECT
            COALESCE(lesson.properties_summary->>'summary', lesson.name) AS summary,
            lesson.uid AS linked_lesson_uid
        FROM moa.node_index lesson
        WHERE lesson.label = 'Lesson'
          AND lesson.valid_to IS NULL
          AND lesson.properties_summary->>'skill_uid' = $1
        ORDER BY lesson.valid_from DESC
        "#,
    )
    .bind(skill_uid.to_string())
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;

    let mut seen = BTreeSet::new();
    let mut addenda = Vec::new();
    for row in rows {
        let summary: String = row.try_get("summary").map_err(map_sqlx_error)?;
        if !seen.insert(normalize_summary(&summary)) {
            continue;
        }
        addenda.push(SkillAddendum {
            summary,
            linked_lesson_uid: row.try_get("linked_lesson_uid").map_err(map_sqlx_error)?,
        });
        if addenda.len() as i64 >= limit {
            break;
        }
    }
    Ok(addenda)
}

/// Normalizes a lesson summary for newest-wins dedup at render time.
///
/// Mirrors the curation pass grouping key (lowercase, whitespace-collapsed,
/// trailing ASCII punctuation stripped) so summaries that curation would merge
/// also collapse here between passes.
fn normalize_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

#[derive(Debug)]
struct SkillAddendum {
    summary: String,
    linked_lesson_uid: Uuid,
}

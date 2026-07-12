//! Post-promotion skill-regression monitor queries for the Postgres session store.
//!
//! These read-only aggregates back the background monitor that turns a silently
//! rotting promoted skill into a reviewed rollback proposal. They join the
//! append-only `learning_log` (promotion events) against `task_segments`
//! (post-promotion usage) and `moa.artifact_revision` (the revision to restore),
//! keeping all rate-comparison logic in `moa-skills` and the I/O here.

use chrono::{DateTime, Utc};

use super::*;

/// One recently promoted skill discovered from the learning log.
///
/// Each row is a `skill_created` or `skill_improved` promotion whose resolution
/// rate the monitor should re-check. `previous_revision_uid` is the highest
/// published revision below the promoted one — the revision a rollback restores
/// — and is `None` for a created skill with no prior published revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSkillPromotion {
    /// Tenant that owns the promotion.
    pub tenant_id: TenantId,
    /// Promoted skill name, matched against `task_segments.skills_used`.
    pub skill_name: String,
    /// Learning-log operation: `skill_created` or `skill_improved`.
    pub operation: String,
    /// Artifact row backing the skill.
    pub artifact_uid: Uuid,
    /// Published revision installed by the promotion.
    pub promoted_revision_uid: Uuid,
    /// Prior published revision to restore on rollback, when one exists.
    pub previous_revision_uid: Option<Uuid>,
    /// Learning candidate that produced the promotion.
    pub promotion_candidate_id: Uuid,
    /// Time the promotion became valid.
    pub promoted_at: DateTime<Utc>,
}

/// Outcome-weighted resolution rate over a bounded set of used segments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkillResolutionSample {
    /// Number of assessed segments that used the skill in the window.
    pub samples: u64,
    /// Outcome-weighted resolution rate (`resolved` = 1.0, `partial` = 0.5,
    /// otherwise 0.0), or 0.0 when no segments matched.
    pub rate: f64,
}

impl PostgresSessionStore {
    /// Lists tenant IDs with at least one active skill promotion since `since`.
    pub async fn list_tenants_with_recent_skill_promotions(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<TenantId>> {
        let learning_log = self.table_name("learning_log");
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT tenant_id FROM {learning_log} \
             WHERE learning_type IN ('skill_created', 'skill_improved') \
               AND valid_to IS NULL \
               AND valid_from >= $1"
        ))
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let raw: String = row.try_get("tenant_id").map_err(map_sqlx_error)?;
                Uuid::parse_str(&raw).map(TenantId::from).map_err(|error| {
                    MoaError::StorageError(format!("invalid tenant id `{raw}`: {error}"))
                })
            })
            .collect()
    }

    /// Lists a tenant's latest active promotion per skill since `since`, newest
    /// first.
    ///
    /// Exactly one row is returned per skill — the promotion installing the
    /// currently-serving (highest-version published) revision — so the monitor
    /// only ever judges the revision that is actually serving. Earlier
    /// promotions of the same skill are excluded: measuring a superseded
    /// promotion's post-window (open-ended, so it would fold in the newer
    /// revision's outcomes) produced regression proposals against revisions that
    /// no longer serve. The post-window for the returned promotion is therefore
    /// bounded above by "now", since no later promotion of that skill exists.
    ///
    /// Malformed promotion rows (missing artifact/revision identifiers) are
    /// skipped rather than surfaced, so the monitor never trips on legacy or
    /// partial payloads.
    pub async fn list_recent_skill_promotions(
        &self,
        tenant_id: &TenantId,
        since: DateTime<Utc>,
    ) -> Result<Vec<RecentSkillPromotion>> {
        let learning_log = self.table_name("learning_log");
        let rows = sqlx::query(&format!(
            "SELECT \
                 latest.skill_name, \
                 latest.operation, \
                 latest.artifact_uid, \
                 latest.promoted_revision_uid, \
                 latest.promotion_candidate_id, \
                 latest.promoted_at, \
                 latest.previous_revision_uid \
             FROM ( \
                 SELECT DISTINCT ON (l.target_label) \
                     l.target_label AS skill_name, \
                     l.learning_type AS operation, \
                     (l.payload->>'artifact_uid')::uuid AS artifact_uid, \
                     (l.payload->>'published_artifact_revision_uid')::uuid AS promoted_revision_uid, \
                     (l.payload->>'candidate_id')::uuid AS promotion_candidate_id, \
                     l.valid_from AS promoted_at, \
                     promoted.version AS promoted_version, \
                     prev.revision_uid AS previous_revision_uid \
                 FROM {learning_log} l \
                 LEFT JOIN LATERAL ( \
                     SELECT rp.version \
                     FROM moa.artifact_revision rp \
                     WHERE rp.revision_uid = (l.payload->>'published_artifact_revision_uid')::uuid \
                 ) promoted ON TRUE \
                 LEFT JOIN LATERAL ( \
                     SELECT r2.revision_uid \
                     FROM moa.artifact_revision r2 \
                     WHERE r2.artifact_uid = (l.payload->>'artifact_uid')::uuid \
                       AND r2.status = 'published' \
                       AND r2.valid_to IS NULL \
                       AND r2.revision_uid <> (l.payload->>'published_artifact_revision_uid')::uuid \
                       AND r2.version < promoted.version \
                     ORDER BY r2.version DESC \
                     LIMIT 1 \
                 ) prev ON TRUE \
                 WHERE l.tenant_id = $1 \
                   AND l.learning_type IN ('skill_created', 'skill_improved') \
                   AND l.valid_to IS NULL \
                   AND l.valid_from >= $2 \
                   AND l.target_label IS NOT NULL \
                 ORDER BY l.target_label, promoted.version DESC NULLS LAST, l.valid_from DESC \
             ) latest \
             ORDER BY latest.promoted_at DESC"
        ))
        .bind(tenant_id.to_string())
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let mut promotions = Vec::with_capacity(rows.len());
        for row in &rows {
            let skill_name: Option<String> = row.try_get("skill_name").map_err(map_sqlx_error)?;
            let artifact_uid: Option<Uuid> = row.try_get("artifact_uid").map_err(map_sqlx_error)?;
            let promoted_revision_uid: Option<Uuid> = row
                .try_get("promoted_revision_uid")
                .map_err(map_sqlx_error)?;
            let promotion_candidate_id: Option<Uuid> = row
                .try_get("promotion_candidate_id")
                .map_err(map_sqlx_error)?;
            let (
                Some(skill_name),
                Some(artifact_uid),
                Some(promoted_revision_uid),
                Some(promotion_candidate_id),
            ) = (
                skill_name,
                artifact_uid,
                promoted_revision_uid,
                promotion_candidate_id,
            )
            else {
                continue;
            };
            promotions.push(RecentSkillPromotion {
                tenant_id: *tenant_id,
                skill_name,
                operation: row.try_get("operation").map_err(map_sqlx_error)?,
                artifact_uid,
                promoted_revision_uid,
                previous_revision_uid: row
                    .try_get("previous_revision_uid")
                    .map_err(map_sqlx_error)?,
                promotion_candidate_id,
                promoted_at: row.try_get("promoted_at").map_err(map_sqlx_error)?,
            });
        }
        Ok(promotions)
    }

    /// Computes a skill's outcome-weighted resolution rate over assessed segments
    /// that used it within `[start, end)`.
    ///
    /// The scoring matches the `skill_resolution_rates` materialized view
    /// (`resolved` = 1.0, `partial` = 0.5, otherwise 0.0). `end` of `None` leaves
    /// the window open at the top, selecting every segment started at or after
    /// `start`.
    pub async fn skill_resolution_rate_in_window(
        &self,
        tenant_id: &TenantId,
        skill_name: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> Result<SkillResolutionSample> {
        let task_segments = self.table_name("task_segments");
        let row = sqlx::query(&format!(
            "SELECT \
                 COUNT(*)::BIGINT AS samples, \
                 COALESCE(AVG(CASE WHEN outcome = 'resolved' THEN 1.0 \
                                   WHEN outcome = 'partial' THEN 0.5 \
                                   ELSE 0.0 END), 0.0)::DOUBLE PRECISION AS rate \
             FROM {task_segments} \
             WHERE tenant_id = $1 \
               AND outcome IS NOT NULL \
               AND skills_used @> ARRAY[$2]::text[] \
               AND started_at >= $3 \
               AND ($4::timestamptz IS NULL OR started_at < $4)"
        ))
        .bind(tenant_id.to_string())
        .bind(skill_name)
        .bind(start)
        .bind(end)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let samples: i64 = row.try_get("samples").map_err(map_sqlx_error)?;
        let rate: f64 = row.try_get("rate").map_err(map_sqlx_error)?;
        Ok(SkillResolutionSample {
            samples: samples.max(0) as u64,
            rate,
        })
    }

    /// Invalidates every active learning-log entry produced by one candidate.
    ///
    /// Used when a rollback supersedes the promotion recorded by
    /// `promotion_candidate_id`: the original `skill_created`/`skill_improved`
    /// entry is closed with `valid_to`, matching append-only rollback semantics.
    /// Returns the number of entries invalidated.
    pub async fn invalidate_learning_by_candidate_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        tenant_id: &TenantId,
        promotion_candidate_id: Uuid,
        valid_to: DateTime<Utc>,
    ) -> Result<u64> {
        let learning_log = self.table_name("learning_log");
        let affected = sqlx::query(&format!(
            "UPDATE {learning_log} SET valid_to = $3 \
             WHERE tenant_id = $1 \
               AND (payload->>'candidate_id') = $2 \
               AND valid_to IS NULL"
        ))
        .bind(tenant_id.to_string())
        .bind(promotion_candidate_id.to_string())
        .bind(valid_to)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected)
    }
}

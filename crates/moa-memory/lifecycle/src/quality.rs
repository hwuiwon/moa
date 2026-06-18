//! Outcome-weighted memory quality-score computation.

use moa_core::WorkspaceId;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::consolidate::{Error, Result};

const EPSILON: f64 = 1e-9;

/// Outcome for one quality-score computation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityStats {
    /// Active node-index rows updated with a non-neutral quality score.
    pub scored: u64,
    /// Workspaces skipped because no task-segment outcome source is visible.
    pub skipped_no_outcome_source: u64,
}

/// Computes the Beta(1,1)-smoothed quality prior for retrieval outcomes.
#[must_use]
pub fn beta_smoothed_quality(uses: u64, successes: u64) -> f64 {
    (1.0 + successes as f64) / (2.0 + uses as f64)
}

/// Recomputes quality scores from retrieval lineage and resolved task segments.
pub async fn compute_quality_scores(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    lookback_days: i64,
) -> Result<QualityStats> {
    if !task_segment_outcome_source_exists(pool).await? {
        tracing::warn!(
            workspace_id = %workspace_id,
            "no task-segment outcome source found for memory quality scoring"
        );
        return Ok(QualityStats {
            scored: 0,
            skipped_no_outcome_source: 1,
        });
    }

    let updated = sqlx::query_scalar::<_, i64>(
        r#"
        WITH segment_ranges AS (
            SELECT
                session_id,
                workspace_id,
                outcome,
                1 + COALESCE(
                    SUM(turn_count) OVER (
                        PARTITION BY session_id
                        ORDER BY segment_index
                        ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
                    ),
                    0
                ) AS start_turn,
                SUM(turn_count) OVER (
                    PARTITION BY session_id
                    ORDER BY segment_index
                    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                ) AS end_turn
            FROM task_segments
            WHERE workspace_id = $1
              AND outcome IS NOT NULL
              AND turn_count > 0
        ),
        scored AS (
            SELECT
                lineage.uid,
                COUNT(*)::bigint AS uses,
                COUNT(*) FILTER (WHERE segment_ranges.outcome = 'resolved')::bigint AS successes
            FROM moa.retrieval_lineage AS lineage
            LEFT JOIN segment_ranges
              ON segment_ranges.workspace_id = lineage.workspace_id
             AND segment_ranges.session_id = lineage.session_id
             AND lineage.turn_seq BETWEEN segment_ranges.start_turn AND segment_ranges.end_turn
            WHERE lineage.workspace_id = $1
              AND lineage.retrieved_at >= now() - ($2::text::interval)
            GROUP BY lineage.uid
        ),
        updates AS (
            UPDATE moa.node_index AS node
            SET quality_score = (1.0 + scored.successes::double precision)
                              / (2.0 + scored.uses::double precision)
            FROM scored
            WHERE node.uid = scored.uid
              AND node.workspace_id = $1
              AND ABS(
                    node.quality_score
                    - ((1.0 + scored.successes::double precision)
                       / (2.0 + scored.uses::double precision))
                  ) > $3
            RETURNING node.uid
        ),
        bumped AS (
            INSERT INTO moa.workspace_state (workspace_id, changelog_version)
            SELECT $1, 1
            WHERE EXISTS (SELECT 1 FROM updates)
            ON CONFLICT (workspace_id) DO UPDATE
                SET changelog_version = moa.workspace_state.changelog_version + 1,
                    updated_at = now()
            RETURNING 1
        )
        SELECT COUNT(*)::bigint FROM updates
        "#,
    )
    .bind(workspace_id.as_str())
    .bind(format!("{} days", lookback_days.max(0)))
    .bind(EPSILON)
    .fetch_one(pool)
    .await?;

    Ok(QualityStats {
        scored: u64::try_from(updated)
            .map_err(|_| Error::InvalidRow("negative quality update count".to_string()))?,
        skipped_no_outcome_source: 0,
    })
}

async fn task_segment_outcome_source_exists(pool: &PgPool) -> Result<bool> {
    let row = sqlx::query(
        r#"
        SELECT to_regclass('task_segments') IS NOT NULL
            OR to_regclass('public.task_segments') IS NOT NULL AS exists
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("exists")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_smoothing_formula_matches_spec() {
        // Pins: quality priors use Beta(1,1) smoothing, not raw success ratios.
        assert_eq!(beta_smoothed_quality(0, 0), 0.5);
        assert!((beta_smoothed_quality(8, 7) - 0.8).abs() < f64::EPSILON);
        assert!((beta_smoothed_quality(8, 1) - 0.2).abs() < f64::EPSILON);
    }
}

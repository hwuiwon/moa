//! Shared workspace-scoped analytics score queries for eval and experiments.

use moa_core::wire::{
    EvalCompareRequest, EvalCompareResponse, EvalCompareRow, EvalScoreSummaryRow,
    EvalScoresRequest, EvalScoresResponse,
};
use sqlx::{PgPool, Row};

use super::eval::EvalServiceError;

/// Workspace-scoped score summary SQL used by score-reader services.
pub const SCORES_BY_RUN_SQL: &str = r#"
SELECT name,
       value_type,
       COUNT(*)::BIGINT AS n,
       AVG(value_numeric) AS numeric_mean,
       AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM analytics.scores
WHERE run_id = $1 AND workspace_id = $2
GROUP BY name, value_type
ORDER BY name, value_type
"#;

/// Workspace-scoped numeric score comparison SQL used by score-reader services.
pub const COMPARE_NUMERIC_RUNS_SQL: &str = r#"
WITH base AS (
    SELECT name, AVG(value_numeric) AS mean
    FROM analytics.scores
    WHERE run_id = $1 AND workspace_id = $3 AND value_type = 'numeric'
    GROUP BY name
),
new AS (
    SELECT name, AVG(value_numeric) AS mean
    FROM analytics.scores
    WHERE run_id = $2 AND workspace_id = $3 AND value_type = 'numeric'
    GROUP BY name
)
SELECT COALESCE(base.name, new.name) AS name,
       base.mean AS base_mean,
       new.mean AS new_mean,
       COALESCE(new.mean, 0.0) - COALESCE(base.mean, 0.0) AS delta
FROM base
FULL OUTER JOIN new USING (name)
ORDER BY name
"#;

/// Reads workspace-scoped score summaries for one score run.
pub async fn score_summaries_for_workspace(
    pool: &PgPool,
    request: EvalScoresRequest,
) -> Result<EvalScoresResponse, EvalServiceError> {
    let rows = sqlx::query(SCORES_BY_RUN_SQL)
        .bind(request.run_id)
        .bind(request.workspace_id.as_str())
        .fetch_all(pool)
        .await?;

    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let n: i64 = row.try_get("n")?;
        let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
        let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
        summaries.push(EvalScoreSummaryRow {
            name: row.try_get("name")?,
            value_type: row.try_get("value_type")?,
            n: u64::try_from(n).map_err(|_| EvalServiceError::IntegerTooLarge { field: "n" })?,
            mean_or_rate: numeric_mean.or(boolean_rate).unwrap_or(0.0),
        });
    }

    Ok(EvalScoresResponse {
        workspace_id: request.workspace_id,
        run_id: request.run_id,
        rows: summaries,
    })
}

/// Compares workspace-scoped numeric scores between two score runs.
pub async fn compare_score_runs_for_workspace(
    pool: &PgPool,
    request: EvalCompareRequest,
) -> Result<EvalCompareResponse, EvalServiceError> {
    let rows = sqlx::query(COMPARE_NUMERIC_RUNS_SQL)
        .bind(request.base_run)
        .bind(request.new_run)
        .bind(request.workspace_id.as_str())
        .fetch_all(pool)
        .await?;

    let mut comparisons = Vec::with_capacity(rows.len());
    for row in rows {
        comparisons.push(EvalCompareRow {
            name: row.try_get("name")?,
            base_mean: row.try_get("base_mean")?,
            new_mean: row.try_get("new_mean")?,
            delta: row.try_get("delta")?,
        });
    }

    Ok(EvalCompareResponse {
        workspace_id: request.workspace_id,
        base_run: request.base_run,
        new_run: request.new_run,
        rows: comparisons,
    })
}

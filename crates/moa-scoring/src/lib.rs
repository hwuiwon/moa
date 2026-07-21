//! Shared score-run storage and score summary queries.

use moa_core::{
    types::action_policy::ActionRuleScope, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

/// Score-run source label for hosted eval replay parents.
pub const SCORE_RUN_SOURCE_EVAL_REPLAY: &str = "eval_replay";

/// Tenant-scoped score summary SQL used by score-reader services.
pub(crate) const SCORES_BY_RUN_SQL: &str = r#"
SELECT name,
       value_type,
       COUNT(*)::BIGINT AS n,
       AVG(value_numeric) AS numeric_mean,
       AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM analytics.scores
WHERE run_id = $1 AND storage_partition_id = $2
GROUP BY name, value_type
ORDER BY name, value_type
"#;

/// Tenant-scoped numeric score comparison SQL used by score-reader services.
pub(crate) const COMPARE_NUMERIC_RUNS_SQL: &str = r#"
WITH base AS (
    SELECT name, AVG(value_numeric) AS mean
    FROM analytics.scores
    WHERE run_id = $1 AND storage_partition_id = $3 AND value_type = 'numeric'
    GROUP BY name
),
new AS (
    SELECT name, AVG(value_numeric) AS mean
    FROM analytics.scores
    WHERE run_id = $2 AND storage_partition_id = $3 AND value_type = 'numeric'
    GROUP BY name
)
SELECT COALESCE(base.name, new.name) AS name,
       base.mean AS base_mean,
       new.mean AS new_mean,
       new.mean - base.mean AS delta
FROM base
FULL OUTER JOIN new USING (name)
ORDER BY name
"#;

/// Error type for score storage and query helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An integer value could not be represented on this platform.
    #[error("{field} is too large")]
    IntegerTooLarge {
        /// Field that overflowed.
        field: &'static str,
    },
    /// A score-run parent already exists with a different scope or source.
    #[error(
        "score_run `{score_run_id}` already exists outside the requested scope or source `{expected_source}`"
    )]
    ScoreRunMismatch {
        /// Score-run identifier that could not be reused.
        score_run_id: Uuid,
        /// Expected score-run source label.
        expected_source: &'static str,
    },
    /// Database access failed.
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

/// Request for reading summaries from one score run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreRunRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Score run identifier.
    pub run_id: Uuid,
}

/// Request for comparing two score runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreCompareRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline score run identifier.
    pub base_run: Uuid,
    /// New score run identifier.
    pub new_run: Uuid,
}

/// Tenant-scoped score summary row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreSummaryRow {
    /// Score name.
    pub name: String,
    /// Score value type.
    pub value_type: String,
    /// Number of rows summarized.
    pub n: u64,
    /// Numeric mean or boolean true-rate, or `None` when every summarized value is NULL.
    pub mean_or_rate: Option<f64>,
}

/// Score summary result for one score run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreSummary {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Score run identifier.
    pub run_id: Uuid,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ScoreSummaryRow>,
}

/// Tenant-scoped score comparison row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreCompareRow {
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Score comparison result for two score runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreCompare {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline score run identifier.
    pub base_run: Uuid,
    /// New score run identifier.
    pub new_run: Uuid,
    /// Comparison rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ScoreCompareRow>,
}

/// Reads tenant-scoped score summaries for one score run.
pub async fn score_summaries_for_tenant(
    pool: &PgPool,
    request: ScoreRunRef,
) -> Result<ScoreSummary, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(SCORES_BY_RUN_SQL)
        .bind(request.run_id)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;

    let summaries = rows
        .iter()
        .map(score_summary_row_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ScoreSummary {
        tenant_id: request.tenant_id,
        run_id: request.run_id,
        rows: summaries,
    })
}

/// Compares tenant-scoped numeric scores between two score runs.
pub async fn compare_score_runs_for_tenant(
    pool: &PgPool,
    request: ScoreCompareRef,
) -> Result<ScoreCompare, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(COMPARE_NUMERIC_RUNS_SQL)
        .bind(request.base_run)
        .bind(request.new_run)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;

    let mut comparisons = Vec::with_capacity(rows.len());
    for row in rows {
        comparisons.push(ScoreCompareRow {
            name: row.try_get("name")?,
            base_mean: row.try_get("base_mean")?,
            new_mean: row.try_get("new_mean")?,
            delta: row.try_get("delta")?,
        });
    }

    Ok(ScoreCompare {
        tenant_id: request.tenant_id,
        base_run: request.base_run,
        new_run: request.new_run,
        rows: comparisons,
    })
}

/// Inserts a score-run parent or validates that the existing parent matches the requested scope.
pub async fn ensure_score_run_parent(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    source: &'static str,
) -> Result<(), Error> {
    let parts = ScopeParts::from_scope(scope);
    if let Some(row) = load_score_run_parent(conn, score_run_id).await? {
        return validate_score_run_parent(&row, &parts, score_run_id, source);
    }

    let result = sqlx::query(
        r#"
        INSERT INTO analytics.score_run (
            run_id, storage_partition_id, user_id, source
        )
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (run_id) DO NOTHING
        "#,
    )
    .bind(score_run_id)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(source)
    .execute(&mut *conn)
    .await?;

    if result.rows_affected() > 0 {
        return Ok(());
    }

    let Some(row) = load_score_run_parent(conn, score_run_id).await? else {
        return Err(Error::ScoreRunMismatch {
            score_run_id,
            expected_source: source,
        });
    };
    validate_score_run_parent(&row, &parts, score_run_id, source)
}

fn score_summary_row_from_row(row: &PgRow) -> Result<ScoreSummaryRow, Error> {
    let n: i64 = row.try_get("n")?;
    let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
    let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
    Ok(ScoreSummaryRow {
        name: row.try_get("name")?,
        value_type: row.try_get("value_type")?,
        n: u64::try_from(n).map_err(|_| Error::IntegerTooLarge { field: "n" })?,
        mean_or_rate: numeric_mean.or(boolean_rate),
    })
}

async fn load_score_run_parent(
    conn: &mut PgConnection,
    score_run_id: Uuid,
) -> Result<Option<sqlx::postgres::PgRow>, Error> {
    sqlx::query(
        r#"
        SELECT storage_partition_id, user_id, scope, source
        FROM analytics.score_run
        WHERE run_id = $1
        LIMIT 1
        "#,
    )
    .bind(score_run_id)
    .fetch_optional(conn)
    .await
    .map_err(Error::from)
}

fn validate_score_run_parent(
    row: &sqlx::postgres::PgRow,
    parts: &ScopeParts,
    score_run_id: Uuid,
    source: &'static str,
) -> Result<(), Error> {
    let storage_partition_id: Option<String> = row.try_get("storage_partition_id")?;
    let user_id: Option<String> = row.try_get("user_id")?;
    let scope: String = row.try_get("scope")?;
    let existing_source: String = row.try_get("source")?;

    if scope == parts.scope
        && storage_partition_id.as_deref() == parts.storage_partition_id.as_deref()
        && user_id.as_deref() == parts.user_id.as_deref()
        && existing_source == source
    {
        return Ok(());
    }

    Err(Error::ScoreRunMismatch {
        score_run_id,
        expected_source: source,
    })
}

struct ScopeParts {
    scope: &'static str,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
}

impl ScopeParts {
    fn from_scope(scope: &ActionRuleScope) -> Self {
        match scope {
            ActionRuleScope::Tenant { tenant_id } => Self {
                scope: "tenant",
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: None,
            },
            ActionRuleScope::Contact {
                tenant_id,
                contact_id,
            } => Self {
                scope: "contact",
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: Some(contact_id.to_string()),
            },
        }
    }
}

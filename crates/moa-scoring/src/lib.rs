//! Shared score-run storage and score summary queries.

use moa_core::{
    types::action_policy::ActionRuleScope, types::experiments::ScorecardValueType,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
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

/// Raw exact-row SQL for one experiment score run.
///
/// This deliberately aggregates nothing. Scorecard completeness is decided by
/// counting and matching individual rows in `moa-experiments`, and an `AVG` or
/// `COUNT` here would hide exactly the duplicate and mislinked rows that gate is
/// supposed to catch.
pub(crate) const EXACT_EXPERIMENT_SCORE_ROWS_SQL: &str = r#"
SELECT score.score_id,
       score.run_id,
       score.name,
       score.value_type,
       score.value_numeric,
       score.value_boolean,
       score.value_categorical,
       score.model_or_evaluator,
       provenance.evaluator_id,
       provenance.evaluator_version,
       provenance.score_name AS provenance_score_name,
       provenance.value_type AS provenance_value_type,
       provenance.experiment_run_uid,
       provenance.plan_revision_uid,
       provenance.trial_uid,
       provenance.target_session_id,
       provenance.target_execution_run_uid,
       provenance.evidence_ref,
       provenance.evidence_hash
FROM analytics.scores AS score
JOIN moa.experiment_score_provenance AS provenance
  ON provenance.score_id = score.score_id
 AND provenance.score_ts = score.ts
 AND provenance.storage_partition_id = score.storage_partition_id
WHERE score.run_id = $1
  AND score.storage_partition_id = $2
  AND provenance.score_run_id = $1
ORDER BY score.name, score.score_id, score.ts
"#;

/// Raw exact-row SQL for every trial score in one experiment run.
///
/// Selected through `experiment_run_uid` on the provenance side so one query
/// covers a whole run's trials instead of one query per trial score run.
pub(crate) const EXACT_EXPERIMENT_RUN_SCORE_ROWS_SQL: &str = r#"
SELECT score.score_id,
       score.run_id,
       score.name,
       score.value_type,
       score.value_numeric,
       score.value_boolean,
       score.value_categorical,
       score.model_or_evaluator,
       provenance.evaluator_id,
       provenance.evaluator_version,
       provenance.score_name AS provenance_score_name,
       provenance.value_type AS provenance_value_type,
       provenance.experiment_run_uid,
       provenance.plan_revision_uid,
       provenance.trial_uid,
       provenance.target_session_id,
       provenance.target_execution_run_uid,
       provenance.evidence_ref,
       provenance.evidence_hash
FROM analytics.scores AS score
JOIN moa.experiment_score_provenance AS provenance
  ON provenance.score_id = score.score_id
 AND provenance.score_ts = score.ts
 AND provenance.storage_partition_id = score.storage_partition_id
 AND provenance.score_run_id = score.run_id
WHERE provenance.experiment_run_uid = $1
  AND provenance.storage_partition_id = $2
ORDER BY provenance.trial_uid, score.name, score.score_id, score.ts
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
    /// Durable storage contained an unknown score value type.
    #[error("invalid score value type `{value}`")]
    InvalidScoreValueType {
        /// Unknown stored value.
        value: String,
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
    pub value_type: ScorecardValueType,
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

/// One exact experiment score row joined with the provenance that explains it.
///
/// A score row with no provenance row is structurally absent from this result:
/// the join is inner, so a seeded or hand-written score can never appear here
/// and can never satisfy a scorecard requirement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoreRow {
    /// Stable score identity derived by the trial finalizer.
    pub score_id: Uuid,
    /// Score run the row belongs to.
    pub score_run_id: Option<Uuid>,
    /// Score name as written into `analytics.scores`.
    pub name: String,
    /// Score value type as written into `analytics.scores`.
    pub value_type: String,
    /// Numeric value when the row is numeric.
    pub value_numeric: Option<f64>,
    /// Boolean value when the row is boolean.
    pub value_boolean: Option<bool>,
    /// Categorical value when the row is categorical.
    pub value_categorical: Option<String>,
    /// Free-text evaluator label carried by the score row.
    pub model_or_evaluator: String,
    /// Evaluator that produced the row, from provenance.
    pub evaluator_id: String,
    /// Exact evaluator version that produced the row, from provenance.
    pub evaluator_version: String,
    /// Score name recorded in provenance.
    pub provenance_score_name: String,
    /// Score value type recorded in provenance.
    pub provenance_value_type: String,
    /// Experiment run the score belongs to.
    pub experiment_run_uid: Uuid,
    /// Pinned plan revision the score belongs to.
    pub plan_revision_uid: Uuid,
    /// Trial the score belongs to.
    pub trial_uid: Uuid,
    /// Exact target session, when the trial drove one.
    pub target_session_id: Option<Uuid>,
    /// Exact target execution run, when the trial drove one.
    pub target_execution_run_uid: Option<Uuid>,
    /// Bounded reference to the evidence the score was derived from.
    pub evidence_ref: String,
    /// BLAKE3 digest of the evidence the score was derived from.
    pub evidence_hash: Vec<u8>,
}

/// Request for reading every exact experiment score row in one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunScoreRowsRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Experiment run whose trial score rows should be read.
    pub experiment_run_uid: Uuid,
}

/// Request for reading exact experiment score rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentScoreRowsRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Score run whose exact rows should be read.
    pub score_run_id: Uuid,
}

/// Reads every exact provenance-backed score row for one experiment score run.
///
/// # Errors
///
/// Returns [`Error::Sql`] when the query or a column decode fails.
pub async fn exact_experiment_score_rows_for_tenant(
    pool: &PgPool,
    request: ExperimentScoreRowsRef,
) -> Result<Vec<ExperimentScoreRow>, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(EXACT_EXPERIMENT_SCORE_ROWS_SQL)
        .bind(request.score_run_id)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(experiment_score_row_from_row).collect()
}

/// Reads every exact provenance-backed score row for one experiment run.
///
/// # Errors
///
/// Returns [`Error::Sql`] when the query or a column decode fails.
pub async fn exact_experiment_run_score_rows_for_tenant(
    pool: &PgPool,
    request: ExperimentRunScoreRowsRef,
) -> Result<Vec<ExperimentScoreRow>, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(EXACT_EXPERIMENT_RUN_SCORE_ROWS_SQL)
        .bind(request.experiment_run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(experiment_score_row_from_row).collect()
}

fn experiment_score_row_from_row(row: &PgRow) -> Result<ExperimentScoreRow, Error> {
    Ok(ExperimentScoreRow {
        score_id: row.try_get("score_id")?,
        score_run_id: row.try_get("run_id")?,
        name: row.try_get("name")?,
        value_type: row.try_get("value_type")?,
        value_numeric: row.try_get("value_numeric")?,
        value_boolean: row.try_get("value_boolean")?,
        value_categorical: row.try_get("value_categorical")?,
        model_or_evaluator: row.try_get("model_or_evaluator")?,
        evaluator_id: row.try_get("evaluator_id")?,
        evaluator_version: row.try_get("evaluator_version")?,
        provenance_score_name: row.try_get("provenance_score_name")?,
        provenance_value_type: row.try_get("provenance_value_type")?,
        experiment_run_uid: row.try_get("experiment_run_uid")?,
        plan_revision_uid: row.try_get("plan_revision_uid")?,
        trial_uid: row.try_get("trial_uid")?,
        target_session_id: row.try_get("target_session_id")?,
        target_execution_run_uid: row.try_get("target_execution_run_uid")?,
        evidence_ref: row.try_get("evidence_ref")?,
        evidence_hash: row.try_get("evidence_hash")?,
    })
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
    let value_type: String = row.try_get("value_type")?;
    Ok(ScoreSummaryRow {
        name: row.try_get("name")?,
        value_type: ScorecardValueType::from_db(&value_type)
            .ok_or(Error::InvalidScoreValueType { value: value_type })?,
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

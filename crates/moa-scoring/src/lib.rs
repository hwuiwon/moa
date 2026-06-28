//! Shared score-run storage and score summary queries.

use moa_core::{ActionRuleScope, StoragePartitionId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

/// Score-run source label for hosted eval replay parents.
pub const SCORE_RUN_SOURCE_EVAL_REPLAY: &str = "eval_replay";

/// Score-run source label for live behavior experiment parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_RUN: &str = "experiment_run";

/// Score-run source label for live behavior experiment trial parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_TRIAL: &str = "experiment_trial";

/// Tenant-scoped score summary SQL used by score-reader services.
pub const SCORES_BY_RUN_SQL: &str = r#"
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
pub const COMPARE_NUMERIC_RUNS_SQL: &str = r#"
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

/// Tenant-scoped aggregate score SQL across trial-level score runs for an experiment run.
pub const TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
SELECT score.name,
       score.value_type,
       COUNT(*)::BIGINT AS n,
       AVG(score.value_numeric) AS numeric_mean,
       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM moa.experiment_trial AS trial
JOIN analytics.scores AS score
  ON score.run_id = trial.score_run_id
 AND score.storage_partition_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'tenant'
  AND trial.storage_partition_id = $2
  AND trial.user_id IS NULL
GROUP BY score.name, score.value_type
ORDER BY score.name, score.value_type
"#;

/// Tenant-scoped per-trial score SQL for an experiment run.
pub const TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
SELECT trial.trial_uid,
       trial.trial_key,
       trial.score_run_id,
       trial.variant_key,
       trial.scenario_id,
       score.name,
       score.value_type,
       COUNT(*)::BIGINT AS n,
       AVG(score.value_numeric) AS numeric_mean,
       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM moa.experiment_trial AS trial
JOIN analytics.scores AS score
  ON score.run_id = trial.score_run_id
 AND score.storage_partition_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'tenant'
  AND trial.storage_partition_id = $2
  AND trial.user_id IS NULL
GROUP BY trial.trial_uid,
         trial.trial_key,
         trial.score_run_id,
         trial.variant_key,
         trial.scenario_id,
         score.name,
         score.value_type
ORDER BY trial.variant_key,
         trial.scenario_id ASC NULLS FIRST,
         trial.trial_key,
         score.name,
         score.value_type
"#;

/// Tenant-scoped per-scenario score SQL for an experiment run.
pub const SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
SELECT trial.scenario_id,
       score.name,
       score.value_type,
       COUNT(*)::BIGINT AS n,
       AVG(score.value_numeric) AS numeric_mean,
       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM moa.experiment_trial AS trial
JOIN analytics.scores AS score
  ON score.run_id = trial.score_run_id
 AND score.storage_partition_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'tenant'
  AND trial.storage_partition_id = $2
  AND trial.user_id IS NULL
GROUP BY trial.scenario_id, score.name, score.value_type
ORDER BY trial.scenario_id ASC NULLS FIRST,
         score.name,
         score.value_type
"#;

/// Tenant-scoped numeric scenario comparison SQL for two experiment runs.
pub const COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
WITH base AS (
    SELECT trial.scenario_id,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.storage_partition_id = $3
    WHERE trial.run_uid = $1
      AND trial.scope = 'tenant'
      AND trial.storage_partition_id = $3
      AND trial.user_id IS NULL
      AND score.value_type = 'numeric'
    GROUP BY trial.scenario_id, score.name
),
new AS (
    SELECT trial.scenario_id,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.storage_partition_id = $3
    WHERE trial.run_uid = $2
      AND trial.scope = 'tenant'
      AND trial.storage_partition_id = $3
      AND trial.user_id IS NULL
      AND score.value_type = 'numeric'
    GROUP BY trial.scenario_id, score.name
)
SELECT COALESCE(base.scenario_id, new.scenario_id) AS scenario_id,
       COALESCE(base.name, new.name) AS name,
       base.mean AS base_mean,
       new.mean AS new_mean,
       new.mean - base.mean AS delta
FROM base
FULL OUTER JOIN new
  ON base.name = new.name
 AND base.scenario_id IS NOT DISTINCT FROM new.scenario_id
ORDER BY scenario_id ASC NULLS FIRST, name
"#;

/// Tenant-scoped numeric variant comparison SQL for two experiment runs.
pub const COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
WITH base AS (
    SELECT trial.variant_key,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.storage_partition_id = $3
    WHERE trial.run_uid = $1
      AND trial.scope = 'tenant'
      AND trial.storage_partition_id = $3
      AND trial.user_id IS NULL
      AND score.value_type = 'numeric'
    GROUP BY trial.variant_key, score.name
),
new AS (
    SELECT trial.variant_key,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.storage_partition_id = $3
    WHERE trial.run_uid = $2
      AND trial.scope = 'tenant'
      AND trial.storage_partition_id = $3
      AND trial.user_id IS NULL
      AND score.value_type = 'numeric'
    GROUP BY trial.variant_key, score.name
)
SELECT COALESCE(base.variant_key, new.variant_key) AS variant_key,
       COALESCE(base.name, new.name) AS name,
       base.mean AS base_mean,
       new.mean AS new_mean,
       new.mean - base.mean AS delta
FROM base
FULL OUTER JOIN new USING (variant_key, name)
ORDER BY variant_key, name
"#;

/// Error type for score storage and query helpers.
#[derive(Debug, thiserror::Error)]
pub enum ScoringError {
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

/// Request for reading trial-aware experiment score summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunScoreRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Experiment run identifier whose trial scores should be summarized.
    pub run_uid: Uuid,
}

/// Request for comparing trial-aware experiment score summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunCompareRef {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline experiment run identifier.
    pub base_run_uid: Uuid,
    /// New experiment run identifier.
    pub new_run_uid: Uuid,
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

/// Per-trial score summary for one experiment trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialScoreSummary {
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Deterministic trial key unique inside the experiment run.
    pub trial_key: String,
    /// Score run identifier used by trial-level score rows.
    pub score_run_id: Uuid,
    /// Stable target variant key selected for the trial.
    pub variant_key: String,
    /// Stable scenario ID selected for the trial.
    pub scenario_id: Option<String>,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ScoreSummaryRow>,
}

/// Per-scenario score summary for one experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioScoreSummary {
    /// Stable scenario ID summarized by this row group.
    pub scenario_id: Option<String>,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ScoreSummaryRow>,
}

/// Trial-aware score breakdown for one experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoreBreakdown {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Experiment run identifier summarized by the response.
    pub run_uid: Uuid,
    /// Aggregate score rows computed across trial-level score runs.
    #[serde(default)]
    pub trial_rollup_rows: Vec<ScoreSummaryRow>,
    /// Per-trial score summaries.
    #[serde(default)]
    pub trials: Vec<TrialScoreSummary>,
    /// Per-scenario score summaries.
    #[serde(default)]
    pub scenarios: Vec<ScenarioScoreSummary>,
}

/// Numeric score delta for one scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioScoreDeltaRow {
    /// Stable scenario ID compared by this row.
    pub scenario_id: Option<String>,
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Numeric score delta for one variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantScoreDeltaRow {
    /// Stable target variant key compared by this row.
    pub variant_key: String,
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Trial-aware score comparison result for two experiment runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentScoreBreakdownCompare {
    /// Tenant scope used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline experiment run identifier.
    pub base_run_uid: Uuid,
    /// New experiment run identifier.
    pub new_run_uid: Uuid,
    /// Numeric scenario deltas ordered for API display.
    #[serde(default)]
    pub scenario_deltas: Vec<ScenarioScoreDeltaRow>,
    /// Numeric variant deltas ordered for API display.
    #[serde(default)]
    pub variant_deltas: Vec<VariantScoreDeltaRow>,
}

/// Reads tenant-scoped score summaries for one score run.
pub async fn score_summaries_for_tenant(
    pool: &PgPool,
    request: ScoreRunRef,
) -> Result<ScoreSummary, ScoringError> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(SCORES_BY_RUN_SQL)
        .bind(request.run_id)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;

    let mut summaries = Vec::with_capacity(rows.len());
    for row in rows {
        let n: i64 = row.try_get("n")?;
        let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
        let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
        summaries.push(ScoreSummaryRow {
            name: row.try_get("name")?,
            value_type: row.try_get("value_type")?,
            n: u64::try_from(n).map_err(|_| ScoringError::IntegerTooLarge { field: "n" })?,
            mean_or_rate: numeric_mean.or(boolean_rate),
        });
    }

    Ok(ScoreSummary {
        tenant_id: request.tenant_id,
        run_id: request.run_id,
        rows: summaries,
    })
}

/// Reads tenant-scoped trial-aware score summaries for one experiment run.
pub async fn experiment_score_breakdown_for_tenant(
    pool: &PgPool,
    request: ExperimentRunScoreRef,
) -> Result<ExperimentScoreBreakdown, ScoringError> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let aggregate_query = sqlx::query(TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool);
    let trial_query = sqlx::query(TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool);
    let scenario_query = sqlx::query(SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool);
    let (aggregate_rows, trial_rows, scenario_rows) =
        tokio::try_join!(aggregate_query, trial_query, scenario_query)?;

    Ok(ExperimentScoreBreakdown {
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        trial_rollup_rows: score_summary_rows_from_rows(&aggregate_rows)?,
        trials: trial_score_summaries_from_rows(&trial_rows)?,
        scenarios: scenario_score_summaries_from_rows(&scenario_rows)?,
    })
}

/// Compares tenant-scoped numeric scores between two score runs.
pub async fn compare_score_runs_for_tenant(
    pool: &PgPool,
    request: ScoreCompareRef,
) -> Result<ScoreCompare, ScoringError> {
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

/// Compares tenant-scoped trial-aware numeric scores between two experiment runs.
pub async fn compare_experiment_score_breakdown_for_tenant(
    pool: &PgPool,
    request: ExperimentRunCompareRef,
) -> Result<ExperimentScoreBreakdownCompare, ScoringError> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let scenario_query = sqlx::query(COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool);
    let variant_query = sqlx::query(COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool);
    let (scenario_rows, variant_rows) = tokio::try_join!(scenario_query, variant_query)?;
    let scenario_deltas = scenario_rows
        .iter()
        .map(scenario_delta_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    let variant_deltas = variant_rows
        .iter()
        .map(variant_delta_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ExperimentScoreBreakdownCompare {
        tenant_id: request.tenant_id,
        base_run_uid: request.base_run_uid,
        new_run_uid: request.new_run_uid,
        scenario_deltas,
        variant_deltas,
    })
}

/// Inserts a score-run parent or validates that the existing parent matches the requested scope.
pub async fn ensure_score_run_parent(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    score_run_id: Uuid,
    source: &'static str,
) -> Result<(), ScoringError> {
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
        return Err(ScoringError::ScoreRunMismatch {
            score_run_id,
            expected_source: source,
        });
    };
    validate_score_run_parent(&row, &parts, score_run_id, source)
}

fn score_summary_rows_from_rows(rows: &[PgRow]) -> Result<Vec<ScoreSummaryRow>, ScoringError> {
    rows.iter().map(score_summary_row_from_row).collect()
}

fn score_summary_row_from_row(row: &PgRow) -> Result<ScoreSummaryRow, ScoringError> {
    let n: i64 = row.try_get("n")?;
    let numeric_mean: Option<f64> = row.try_get("numeric_mean")?;
    let boolean_rate: Option<f64> = row.try_get("boolean_rate")?;
    Ok(ScoreSummaryRow {
        name: row.try_get("name")?,
        value_type: row.try_get("value_type")?,
        n: u64::try_from(n).map_err(|_| ScoringError::IntegerTooLarge { field: "n" })?,
        mean_or_rate: numeric_mean.or(boolean_rate),
    })
}

/// Groups consecutive score rows into per-trial summaries.
///
/// Rows must already be ordered so that every row for one trial is contiguous
/// (as produced by [`TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL`]). Non-contiguous input
/// splits a single trial across multiple summary groups.
pub fn trial_score_summaries_from_rows(
    rows: &[PgRow],
) -> Result<Vec<TrialScoreSummary>, ScoringError> {
    let mut summaries: Vec<TrialScoreSummary> = Vec::new();
    for row in rows {
        let trial_uid = row.try_get("trial_uid")?;
        let maybe_summary = summaries
            .last_mut()
            .filter(|summary| summary.trial_uid == trial_uid);
        if let Some(summary) = maybe_summary {
            summary.rows.push(score_summary_row_from_row(row)?);
            continue;
        }

        summaries.push(TrialScoreSummary {
            trial_uid,
            trial_key: row.try_get("trial_key")?,
            score_run_id: row.try_get("score_run_id")?,
            variant_key: row.try_get("variant_key")?,
            scenario_id: row.try_get("scenario_id")?,
            rows: vec![score_summary_row_from_row(row)?],
        });
    }
    Ok(summaries)
}

/// Groups consecutive score rows into per-scenario summaries.
///
/// Rows must already be ordered so that every row for one scenario is contiguous
/// (as produced by [`SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL`]). Non-contiguous
/// input splits a single scenario across multiple summary groups.
pub fn scenario_score_summaries_from_rows(
    rows: &[PgRow],
) -> Result<Vec<ScenarioScoreSummary>, ScoringError> {
    let mut summaries: Vec<ScenarioScoreSummary> = Vec::new();
    for row in rows {
        let scenario_id = row.try_get("scenario_id")?;
        let maybe_summary = summaries
            .last_mut()
            .filter(|summary| summary.scenario_id == scenario_id);
        if let Some(summary) = maybe_summary {
            summary.rows.push(score_summary_row_from_row(row)?);
            continue;
        }

        summaries.push(ScenarioScoreSummary {
            scenario_id,
            rows: vec![score_summary_row_from_row(row)?],
        });
    }
    Ok(summaries)
}

fn scenario_delta_from_row(row: &PgRow) -> Result<ScenarioScoreDeltaRow, ScoringError> {
    Ok(ScenarioScoreDeltaRow {
        scenario_id: row.try_get("scenario_id")?,
        name: row.try_get("name")?,
        base_mean: row.try_get("base_mean")?,
        new_mean: row.try_get("new_mean")?,
        delta: row.try_get("delta")?,
    })
}

fn variant_delta_from_row(row: &PgRow) -> Result<VariantScoreDeltaRow, ScoringError> {
    Ok(VariantScoreDeltaRow {
        variant_key: row.try_get("variant_key")?,
        name: row.try_get("name")?,
        base_mean: row.try_get("base_mean")?,
        new_mean: row.try_get("new_mean")?,
        delta: row.try_get("delta")?,
    })
}

async fn load_score_run_parent(
    conn: &mut PgConnection,
    score_run_id: Uuid,
) -> Result<Option<sqlx::postgres::PgRow>, ScoringError> {
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
    .map_err(ScoringError::from)
}

fn validate_score_run_parent(
    row: &sqlx::postgres::PgRow,
    parts: &ScopeParts,
    score_run_id: Uuid,
    source: &'static str,
) -> Result<(), ScoringError> {
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

    Err(ScoringError::ScoreRunMismatch {
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
        }
    }
}

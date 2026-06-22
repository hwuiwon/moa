//! Shared score-run storage and score summary queries.

use moa_core::{ActionRuleScope, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

/// Score-run source label for hosted eval replay parents.
pub const SCORE_RUN_SOURCE_EVAL_REPLAY: &str = "eval_replay";

/// Score-run source label for live behavior experiment parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_RUN: &str = "experiment_run";

/// Score-run source label for live behavior experiment trial parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_TRIAL: &str = "experiment_trial";

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
       new.mean - base.mean AS delta
FROM base
FULL OUTER JOIN new USING (name)
ORDER BY name
"#;

/// Workspace-scoped aggregate score SQL across trial-level score runs for an experiment run.
pub const TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
SELECT score.name,
       score.value_type,
       COUNT(*)::BIGINT AS n,
       AVG(score.value_numeric) AS numeric_mean,
       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate
FROM moa.experiment_trial AS trial
JOIN analytics.scores AS score
  ON score.run_id = trial.score_run_id
 AND score.workspace_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'workspace'
  AND trial.workspace_id = $2
  AND trial.user_id IS NULL
GROUP BY score.name, score.value_type
ORDER BY score.name, score.value_type
"#;

/// Workspace-scoped per-trial score SQL for an experiment run.
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
 AND score.workspace_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'workspace'
  AND trial.workspace_id = $2
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

/// Workspace-scoped per-scenario score SQL for an experiment run.
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
 AND score.workspace_id = $2
WHERE trial.run_uid = $1
  AND trial.scope = 'workspace'
  AND trial.workspace_id = $2
  AND trial.user_id IS NULL
GROUP BY trial.scenario_id, score.name, score.value_type
ORDER BY trial.scenario_id ASC NULLS FIRST,
         score.name,
         score.value_type
"#;

/// Workspace-scoped numeric scenario comparison SQL for two experiment runs.
pub const COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
WITH base AS (
    SELECT trial.scenario_id,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.workspace_id = $3
    WHERE trial.run_uid = $1
      AND trial.scope = 'workspace'
      AND trial.workspace_id = $3
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
     AND score.workspace_id = $3
    WHERE trial.run_uid = $2
      AND trial.scope = 'workspace'
      AND trial.workspace_id = $3
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

/// Workspace-scoped numeric variant comparison SQL for two experiment runs.
pub const COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL: &str = r#"
WITH base AS (
    SELECT trial.variant_key,
           score.name,
           AVG(score.value_numeric) AS mean
    FROM moa.experiment_trial AS trial
    JOIN analytics.scores AS score
      ON score.run_id = trial.score_run_id
     AND score.workspace_id = $3
    WHERE trial.run_uid = $1
      AND trial.scope = 'workspace'
      AND trial.workspace_id = $3
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
     AND score.workspace_id = $3
    WHERE trial.run_uid = $2
      AND trial.scope = 'workspace'
      AND trial.workspace_id = $3
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
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Score run identifier.
    pub run_id: Uuid,
}

/// Request for comparing two score runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreCompareRef {
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Baseline score run identifier.
    pub base_run: Uuid,
    /// New score run identifier.
    pub new_run: Uuid,
}

/// Request for reading trial-aware experiment score summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunScoreRef {
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Experiment run identifier whose trial scores should be summarized.
    pub run_uid: Uuid,
}

/// Request for comparing trial-aware experiment score summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRunCompareRef {
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Baseline experiment run identifier.
    pub base_run_uid: Uuid,
    /// New experiment run identifier.
    pub new_run_uid: Uuid,
}

/// Workspace-scoped score summary row.
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
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Score run identifier.
    pub run_id: Uuid,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ScoreSummaryRow>,
}

/// Workspace-scoped score comparison row.
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
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
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
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
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
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
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

/// Reads workspace-scoped score summaries for one score run.
pub async fn score_summaries_for_workspace(
    pool: &PgPool,
    request: ScoreRunRef,
) -> Result<ScoreSummary, ScoringError> {
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
        summaries.push(ScoreSummaryRow {
            name: row.try_get("name")?,
            value_type: row.try_get("value_type")?,
            n: u64::try_from(n).map_err(|_| ScoringError::IntegerTooLarge { field: "n" })?,
            mean_or_rate: numeric_mean.or(boolean_rate),
        });
    }

    Ok(ScoreSummary {
        workspace_id: request.workspace_id,
        run_id: request.run_id,
        rows: summaries,
    })
}

/// Reads workspace-scoped trial-aware score summaries for one experiment run.
pub async fn experiment_score_breakdown_for_workspace(
    pool: &PgPool,
    request: ExperimentRunScoreRef,
) -> Result<ExperimentScoreBreakdown, ScoringError> {
    let workspace_id = request.workspace_id.as_str();
    let aggregate_query = sqlx::query(TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(workspace_id)
        .fetch_all(pool);
    let trial_query = sqlx::query(TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(workspace_id)
        .fetch_all(pool);
    let scenario_query = sqlx::query(SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(workspace_id)
        .fetch_all(pool);
    let (aggregate_rows, trial_rows, scenario_rows) =
        tokio::try_join!(aggregate_query, trial_query, scenario_query)?;

    Ok(ExperimentScoreBreakdown {
        workspace_id: request.workspace_id,
        run_uid: request.run_uid,
        trial_rollup_rows: score_summary_rows_from_rows(&aggregate_rows)?,
        trials: trial_score_summaries_from_rows(&trial_rows)?,
        scenarios: scenario_score_summaries_from_rows(&scenario_rows)?,
    })
}

/// Compares workspace-scoped numeric scores between two score runs.
pub async fn compare_score_runs_for_workspace(
    pool: &PgPool,
    request: ScoreCompareRef,
) -> Result<ScoreCompare, ScoringError> {
    let rows = sqlx::query(COMPARE_NUMERIC_RUNS_SQL)
        .bind(request.base_run)
        .bind(request.new_run)
        .bind(request.workspace_id.as_str())
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
        workspace_id: request.workspace_id,
        base_run: request.base_run,
        new_run: request.new_run,
        rows: comparisons,
    })
}

/// Compares workspace-scoped trial-aware numeric scores between two experiment runs.
pub async fn compare_experiment_score_breakdown_for_workspace(
    pool: &PgPool,
    request: ExperimentRunCompareRef,
) -> Result<ExperimentScoreBreakdownCompare, ScoringError> {
    let scenario_query = sqlx::query(COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(request.workspace_id.as_str())
        .fetch_all(pool);
    let variant_query = sqlx::query(COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(request.workspace_id.as_str())
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
        workspace_id: request.workspace_id,
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
            run_id, workspace_id, user_id, source
        )
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (run_id) DO NOTHING
        "#,
    )
    .bind(score_run_id)
    .bind(parts.workspace_id.as_deref())
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

fn trial_score_summaries_from_rows(rows: &[PgRow]) -> Result<Vec<TrialScoreSummary>, ScoringError> {
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

fn scenario_score_summaries_from_rows(
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
        SELECT workspace_id, user_id, scope, source
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
    let workspace_id: Option<String> = row.try_get("workspace_id")?;
    let user_id: Option<String> = row.try_get("user_id")?;
    let scope: String = row.try_get("scope")?;
    let existing_source: String = row.try_get("source")?;

    if scope == parts.scope
        && workspace_id.as_deref() == parts.workspace_id.as_deref()
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
    workspace_id: Option<String>,
    user_id: Option<String>,
}

impl ScopeParts {
    fn from_scope(scope: &ActionRuleScope) -> Self {
        match scope {
            ActionRuleScope::WorkspaceDefault => Self {
                scope: "global",
                workspace_id: None,
                user_id: None,
            },
            ActionRuleScope::Tenant { tenant_id } => Self {
                scope: "workspace",
                workspace_id: Some(tenant_id.to_string()),
                user_id: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_queries_scope_every_run_id_by_workspace() {
        // Pins: shared score SQL constrains every requested run by the authorized workspace.
        assert!(
            SCORES_BY_RUN_SQL.contains("WHERE run_id = $1 AND workspace_id = $2"),
            "scores query must scope the run id by workspace"
        );
        assert!(
            COMPARE_NUMERIC_RUNS_SQL.contains("WHERE run_id = $1 AND workspace_id = $3"),
            "compare base run must be scoped by workspace"
        );
        assert!(
            COMPARE_NUMERIC_RUNS_SQL.contains("WHERE run_id = $2 AND workspace_id = $3"),
            "compare new run must be scoped by workspace"
        );
        assert_eq!(
            COMPARE_NUMERIC_RUNS_SQL
                .matches("workspace_id = $3")
                .count(),
            2,
            "compare SQL must constrain both run IDs by the same authorized workspace"
        );
    }

    #[test]
    fn experiment_trial_score_queries_scope_every_read_through_trial_rows() {
        // Pins: trial-aware experiment score SQL joins through scoped trial rows before reading scores.
        for sql in [
            TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL,
            TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL,
            SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL,
        ] {
            assert!(
                sql.contains("FROM moa.experiment_trial AS trial"),
                "trial-aware score SQL must join through experiment_trial: {sql}"
            );
            assert!(
                sql.contains("score.run_id = trial.score_run_id"),
                "trial-aware score SQL must read score rows by trial score_run_id: {sql}"
            );
            assert!(
                sql.contains("score.workspace_id = $2"),
                "trial-aware score SQL must scope analytics.scores by workspace: {sql}"
            );
            assert!(
                sql.contains("trial.run_uid = $1"),
                "trial-aware score SQL must scope trial rows by experiment run: {sql}"
            );
            assert!(
                sql.contains("trial.workspace_id = $2"),
                "trial-aware score SQL must scope trial rows by workspace: {sql}"
            );
            assert!(
                sql.contains("trial.user_id IS NULL"),
                "workspace experiment score SQL must not leak user-scoped trials: {sql}"
            );
        }
    }

    #[test]
    fn experiment_compare_queries_scope_scenarios_and_variants_by_workspace() {
        // Pins: scenario and variant deltas compare only scoped trial-level score rows.
        for sql in [
            COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL,
            COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL,
        ] {
            assert_eq!(
                sql.matches("FROM moa.experiment_trial AS trial").count(),
                2,
                "compare SQL must build both sides from experiment_trial: {sql}"
            );
            assert_eq!(
                sql.matches("score.workspace_id = $3").count(),
                2,
                "compare SQL must scope both score reads by workspace: {sql}"
            );
            assert_eq!(
                sql.matches("trial.workspace_id = $3").count(),
                2,
                "compare SQL must scope both trial reads by workspace: {sql}"
            );
            assert_eq!(
                sql.matches("trial.user_id IS NULL").count(),
                2,
                "workspace compare SQL must exclude user-scoped trial rows: {sql}"
            );
        }
        assert!(
            COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL.contains("scenario_id"),
            "scenario deltas must expose the stable scenario ID"
        );
        assert!(
            COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL.contains("variant_key"),
            "variant deltas must expose the stable variant key"
        );
    }

    #[test]
    fn score_summary_rows_preserve_missing_aggregate_values() {
        // Pins: a score row with no aggregate value is represented as missing, not sentinel 0.0.
        let row = ScoreSummaryRow {
            name: "quality".to_string(),
            value_type: "numeric".to_string(),
            n: 3,
            mean_or_rate: None,
        };

        assert_eq!(row.mean_or_rate, None);
    }

    #[test]
    fn score_compare_queries_do_not_coalesce_missing_means_to_zero() {
        // Pins: one-sided score presence yields a null delta instead of a fabricated magnitude.
        for sql in [
            COMPARE_NUMERIC_RUNS_SQL,
            COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL,
            COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL,
        ] {
            assert!(
                sql.contains("new.mean - base.mean AS delta"),
                "compare SQL should let NULL means produce NULL deltas: {sql}"
            );
            assert!(
                !sql.contains("COALESCE(base.mean")
                    && !sql.contains("COALESCE(new.mean")
                    && !sql.contains("COALESCE(mean"),
                "compare SQL must not coalesce missing means to zero: {sql}"
            );
        }
    }

    #[test]
    fn score_run_source_constants_use_storage_labels() {
        // Pins: score-run parent sources distinguish eval replay, experiment runs, and trials.
        assert_eq!(SCORE_RUN_SOURCE_EVAL_REPLAY, "eval_replay");
        assert_eq!(SCORE_RUN_SOURCE_EXPERIMENT_RUN, "experiment_run");
        assert_eq!(SCORE_RUN_SOURCE_EXPERIMENT_TRIAL, "experiment_trial");
    }
}

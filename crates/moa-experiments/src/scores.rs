//! Experiment-owned score sources, trial joins, summaries, and comparisons.

use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::TenantId};
use moa_scoring::{Error, ScoreSummaryRow};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

/// Score-run source label for live behavior experiment parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_RUN: &str = "experiment_run";

/// Score-run source label for live behavior experiment trial parents.
pub const SCORE_RUN_SOURCE_EXPERIMENT_TRIAL: &str = "experiment_trial";

macro_rules! scores_by_experiment_run_sql {
    ($select:literal, $group:literal, $order:literal $(,)?) => {
        concat!(
            "\n",
            $select,
            "FROM moa.experiment_trial AS trial\n",
            "JOIN analytics.scores AS score\n",
            "  ON score.run_id = trial.score_run_id\n",
            " AND score.storage_partition_id = $2\n",
            "WHERE trial.run_uid = $1\n",
            "  AND trial.scope = 'tenant'\n",
            "  AND trial.storage_partition_id = $2\n",
            "  AND trial.user_id IS NULL\n",
            $group,
            $order,
        )
    };
}

macro_rules! compare_scores_by_experiment_run_sql {
    ($cte_select:literal, $cte_group:literal, $final:literal $(,)?) => {
        concat!(
            "\nWITH base AS (\n",
            $cte_select,
            "    FROM moa.experiment_trial AS trial\n",
            "    JOIN analytics.scores AS score\n",
            "      ON score.run_id = trial.score_run_id\n",
            "     AND score.storage_partition_id = $3\n",
            "    WHERE trial.run_uid = $1\n",
            "      AND trial.scope = 'tenant'\n",
            "      AND trial.storage_partition_id = $3\n",
            "      AND trial.user_id IS NULL\n",
            "      AND score.value_type = 'numeric'\n",
            $cte_group,
            "),\nnew AS (\n",
            $cte_select,
            "    FROM moa.experiment_trial AS trial\n",
            "    JOIN analytics.scores AS score\n",
            "      ON score.run_id = trial.score_run_id\n",
            "     AND score.storage_partition_id = $3\n",
            "    WHERE trial.run_uid = $2\n",
            "      AND trial.scope = 'tenant'\n",
            "      AND trial.storage_partition_id = $3\n",
            "      AND trial.user_id IS NULL\n",
            "      AND score.value_type = 'numeric'\n",
            $cte_group,
            ")\n",
            $final,
        )
    };
}

const TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL: &str = scores_by_experiment_run_sql!(
    "SELECT score.name,\n       score.value_type,\n       COUNT(*)::BIGINT AS n,\n       AVG(score.value_numeric) AS numeric_mean,\n       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate\n",
    "GROUP BY score.name, score.value_type\n",
    "ORDER BY score.name, score.value_type\n",
);

const TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL: &str = scores_by_experiment_run_sql!(
    "SELECT trial.trial_uid,\n       trial.trial_key,\n       trial.score_run_id,\n       trial.variant_key,\n       trial.scenario_id,\n       score.name,\n       score.value_type,\n       COUNT(*)::BIGINT AS n,\n       AVG(score.value_numeric) AS numeric_mean,\n       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate\n",
    "GROUP BY trial.trial_uid,\n         trial.trial_key,\n         trial.score_run_id,\n         trial.variant_key,\n         trial.scenario_id,\n         score.name,\n         score.value_type\n",
    "ORDER BY trial.variant_key,\n         trial.scenario_id ASC NULLS FIRST,\n         trial.trial_key,\n         score.name,\n         score.value_type\n",
);

const SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL: &str = scores_by_experiment_run_sql!(
    "SELECT trial.scenario_id,\n       score.name,\n       score.value_type,\n       COUNT(*)::BIGINT AS n,\n       AVG(score.value_numeric) AS numeric_mean,\n       AVG(CASE WHEN score.value_boolean THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS boolean_rate\n",
    "GROUP BY trial.scenario_id, score.name, score.value_type\n",
    "ORDER BY trial.scenario_id ASC NULLS FIRST,\n         score.name,\n         score.value_type\n",
);

const COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL: &str = compare_scores_by_experiment_run_sql!(
    "    SELECT trial.scenario_id,\n           score.name,\n           AVG(score.value_numeric) AS mean\n",
    "    GROUP BY trial.scenario_id, score.name\n",
    "SELECT COALESCE(base.scenario_id, new.scenario_id) AS scenario_id,\n       COALESCE(base.name, new.name) AS name,\n       base.mean AS base_mean,\n       new.mean AS new_mean,\n       new.mean - base.mean AS delta\nFROM base\nFULL OUTER JOIN new\n  ON base.name = new.name\n AND base.scenario_id IS NOT DISTINCT FROM new.scenario_id\nORDER BY scenario_id ASC NULLS FIRST, name\n",
);

const COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL: &str = compare_scores_by_experiment_run_sql!(
    "    SELECT trial.variant_key,\n           score.name,\n           AVG(score.value_numeric) AS mean\n",
    "    GROUP BY trial.variant_key, score.name\n",
    "SELECT COALESCE(base.variant_key, new.variant_key) AS variant_key,\n       COALESCE(base.name, new.name) AS name,\n       base.mean AS base_mean,\n       new.mean AS new_mean,\n       new.mean - base.mean AS delta\nFROM base\nFULL OUTER JOIN new USING (variant_key, name)\nORDER BY variant_key, name\n",
);

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

/// Reads tenant-scoped trial-aware score summaries for one experiment run.
pub async fn experiment_score_breakdown_for_tenant(
    pool: &PgPool,
    request: ExperimentRunScoreRef,
) -> Result<ExperimentScoreBreakdown, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let aggregate_rows = sqlx::query(TRIAL_ROLLUP_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
    let trial_rows = sqlx::query(TRIAL_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
    let scenario_rows = sqlx::query(SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;

    Ok(ExperimentScoreBreakdown {
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        trial_rollup_rows: aggregate_rows
            .iter()
            .map(score_summary_row_from_row)
            .collect::<Result<Vec<_>, _>>()?,
        trials: trial_score_summaries_from_rows(&trial_rows)?,
        scenarios: scenario_score_summaries_from_rows(&scenario_rows)?,
    })
}

/// Compares tenant-scoped trial-aware numeric scores between two experiment runs.
pub async fn compare_experiment_score_breakdown_for_tenant(
    pool: &PgPool,
    request: ExperimentRunCompareRef,
) -> Result<ExperimentScoreBreakdownCompare, Error> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let scenario_rows = sqlx::query(COMPARE_SCENARIO_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
    let variant_rows = sqlx::query(COMPARE_VARIANT_SCORES_BY_EXPERIMENT_RUN_SQL)
        .bind(request.base_run_uid)
        .bind(request.new_run_uid)
        .bind(&storage_partition_id)
        .fetch_all(pool)
        .await?;
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

fn trial_score_summaries_from_rows(rows: &[PgRow]) -> Result<Vec<TrialScoreSummary>, Error> {
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

fn scenario_score_summaries_from_rows(rows: &[PgRow]) -> Result<Vec<ScenarioScoreSummary>, Error> {
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

fn scenario_delta_from_row(row: &PgRow) -> Result<ScenarioScoreDeltaRow, Error> {
    Ok(ScenarioScoreDeltaRow {
        scenario_id: row.try_get("scenario_id")?,
        name: row.try_get("name")?,
        base_mean: row.try_get("base_mean")?,
        new_mean: row.try_get("new_mean")?,
        delta: row.try_get("delta")?,
    })
}

fn variant_delta_from_row(row: &PgRow) -> Result<VariantScoreDeltaRow, Error> {
    Ok(VariantScoreDeltaRow {
        variant_key: row.try_get("variant_key")?,
        name: row.try_get("name")?,
        base_mean: row.try_get("base_mean")?,
        new_mean: row.try_get("new_mean")?,
        delta: row.try_get("delta")?,
    })
}

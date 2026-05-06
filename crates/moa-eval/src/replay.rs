//! Offline replay dataset registration and score emission.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use moa_core::{MoaConfig, WorkspaceId};
use moa_lineage_core::{
    LineageEvent, LineageSink, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{AgentConfig, EngineOptions, EvalEngine, ExpectedOutput, TestCase, TestSuite};
use crate::{EvalError, Result};

/// Configuration for replaying one stored dataset.
#[derive(Clone, Debug)]
pub struct ReplayConfig {
    /// Dataset to replay.
    pub dataset_id: Uuid,
    /// Run identifier for grouping emitted scores.
    pub run_id: Uuid,
    /// Optional model override label.
    pub model_override: Option<String>,
    /// Optional embedder override label.
    pub embedder_override: Option<String>,
    /// Optional item cap.
    pub limit: Option<usize>,
}

/// Summary for one replay execution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplayReport {
    /// Replay run identifier.
    pub run_id: Uuid,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Items processed.
    pub items: usize,
    /// Scores emitted.
    pub scores: usize,
}

/// Stored eval dataset item.
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetItem {
    /// Dataset item identifier.
    pub item_id: Uuid,
    /// Workspace to evaluate against.
    pub workspace_id: WorkspaceId,
    /// Query text.
    pub query: String,
    /// Optional expected answer.
    pub expected_answer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonlDatasetItem {
    item_id: Option<Uuid>,
    workspace_id: Option<String>,
    scope: Option<Value>,
    query: String,
    expected_answer: Option<String>,
    expected_chunk_ids: Option<Vec<Uuid>>,
    metadata: Option<Value>,
}

/// Registers a JSONL dataset in the lineage analytics schema.
pub async fn register_dataset(pool: &PgPool, path: &Path, name: &str) -> Result<Uuid> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| EvalError::Io {
            path: PathBuf::from(path),
            source,
        })?;
    let items = parse_jsonl_items(path, &content)?;

    let mut tx = pool.begin().await?;
    let dataset_id = Uuid::now_v7();
    let dataset_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO analytics.eval_datasets (dataset_id, name, source_path)
        VALUES ($1, $2, $3)
        ON CONFLICT (name) DO UPDATE
        SET source_path = EXCLUDED.source_path
        RETURNING dataset_id
        "#,
    )
    .bind(dataset_id)
    .bind(name)
    .bind(path.display().to_string())
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM analytics.eval_dataset_items WHERE dataset_id = $1")
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;

    for item in items {
        sqlx::query(
            r#"
            INSERT INTO analytics.eval_dataset_items (
                item_id,
                dataset_id,
                workspace_id,
                scope,
                query,
                expected_answer,
                expected_chunk_ids,
                metadata
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(item.item_id.unwrap_or_else(Uuid::now_v7))
        .bind(dataset_id)
        .bind(item.workspace_id.unwrap_or_else(|| "default".to_string()))
        .bind(sqlx::types::Json(
            item.scope.unwrap_or_else(|| serde_json::json!({})),
        ))
        .bind(item.query)
        .bind(item.expected_answer)
        .bind(item.expected_chunk_ids.unwrap_or_default())
        .bind(sqlx::types::Json(
            item.metadata.unwrap_or_else(|| serde_json::json!({})),
        ))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(dataset_id)
}

/// Lists registered eval datasets.
pub async fn list_datasets(pool: &PgPool) -> Result<Vec<(Uuid, String, i64)>> {
    let rows = sqlx::query(
        r#"
        SELECT d.dataset_id, d.name, COUNT(i.item_id)::BIGINT AS items
        FROM analytics.eval_datasets d
        LEFT JOIN analytics.eval_dataset_items i ON i.dataset_id = d.dataset_id
        GROUP BY d.dataset_id, d.name, d.created_at
        ORDER BY d.created_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("dataset_id")?,
                row.try_get("name")?,
                row.try_get("items")?,
            ))
        })
        .collect()
}

/// Loads stored dataset items.
pub async fn load_dataset_items(
    pool: &PgPool,
    dataset_id: Uuid,
    limit: Option<usize>,
) -> Result<Vec<DatasetItem>> {
    let limit = i64::try_from(limit.unwrap_or(1000))
        .map_err(|_| EvalError::InvalidConfig("eval replay limit is too large".to_string()))?;
    let rows = sqlx::query(
        r#"
        SELECT item_id, workspace_id, query, expected_answer
        FROM analytics.eval_dataset_items
        WHERE dataset_id = $1
        ORDER BY created_at ASC, item_id ASC
        LIMIT $2
        "#,
    )
    .bind(dataset_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let workspace_id: String = row.try_get("workspace_id")?;
            Ok(DatasetItem {
                item_id: row.try_get("item_id")?,
                workspace_id: WorkspaceId::new(workspace_id),
                query: row.try_get("query")?,
                expected_answer: row.try_get("expected_answer")?,
            })
        })
        .collect()
}

/// Replays a dataset by emitting deterministic baseline scores into lineage.
pub async fn replay_dataset(
    pool: &PgPool,
    sink: Arc<dyn LineageSink>,
    cfg: ReplayConfig,
) -> Result<ReplayReport> {
    let items = load_dataset_items(pool, cfg.dataset_id, cfg.limit).await?;
    let evaluator = replay_evaluator_name(&cfg);
    let mut report = ReplayReport {
        run_id: cfg.run_id,
        dataset_id: cfg.dataset_id,
        items: 0,
        scores: 0,
    };

    for item in items {
        if let Some(expected) = &item.expected_answer {
            let score = token_f1(&item.query, expected);
            sink.record(LineageEvent::Eval(ScoreRecord {
                score_id: Uuid::now_v7(),
                ts: Utc::now(),
                target: ScoreTarget::DatasetRunItem {
                    run_id: cfg.run_id,
                    item_id: item.item_id,
                },
                workspace_id: item.workspace_id.clone(),
                user_id: None,
                name: "answer_f1".to_string(),
                value: ScoreValue::Numeric(score),
                source: ScoreSource::OfflineReplay,
                model_or_evaluator: evaluator.clone(),
                run_id: Some(cfg.run_id),
                dataset_id: Some(cfg.dataset_id),
                comment: None,
            }));
            report.scores += 1;
        }
        report.items += 1;
    }

    Ok(report)
}

/// Replays a dataset through the current eval engine and records answer scores.
pub async fn replay_dataset_live(
    config: MoaConfig,
    pool: &PgPool,
    sink: Arc<dyn LineageSink>,
    cfg: ReplayConfig,
) -> Result<ReplayReport> {
    let items = load_dataset_items(pool, cfg.dataset_id, cfg.limit).await?;
    let cases = items
        .iter()
        .map(|item| TestCase {
            name: item.item_id.to_string(),
            input: item.query.clone(),
            expected_output: item.expected_answer.as_ref().map(|answer| ExpectedOutput {
                exact: Some(answer.clone()),
                ..ExpectedOutput::default()
            }),
            ..TestCase::default()
        })
        .collect::<Vec<_>>();
    let suite = TestSuite {
        name: format!("replay-{}", cfg.dataset_id),
        cases,
        default_timeout_seconds: 300,
        ..TestSuite::default()
    };
    let agent_config = AgentConfig {
        name: "replay".to_string(),
        model: cfg.model_override.clone(),
        ..AgentConfig::default()
    };
    let engine = EvalEngine::new(
        config,
        EngineOptions {
            parallel: 1,
            ..EngineOptions::default()
        },
    )?;
    let run = engine.run_suite(&suite, &[agent_config]).await?;
    let evaluator = replay_evaluator_name(&cfg);
    let mut report = ReplayReport {
        run_id: cfg.run_id,
        dataset_id: cfg.dataset_id,
        items: 0,
        scores: 0,
    };

    for (item, result) in items.iter().zip(run.results.iter()) {
        if let Some(expected) = &item.expected_answer {
            let actual = result.response.as_deref().unwrap_or_default();
            let score = token_f1(actual, expected);
            sink.record(LineageEvent::Eval(ScoreRecord {
                score_id: Uuid::now_v7(),
                ts: Utc::now(),
                target: ScoreTarget::DatasetRunItem {
                    run_id: cfg.run_id,
                    item_id: item.item_id,
                },
                workspace_id: item.workspace_id.clone(),
                user_id: None,
                name: "answer_f1".to_string(),
                value: ScoreValue::Numeric(score),
                source: ScoreSource::OfflineReplay,
                model_or_evaluator: evaluator.clone(),
                run_id: Some(cfg.run_id),
                dataset_id: Some(cfg.dataset_id),
                comment: result.error.clone(),
            }));
            report.scores += 1;
        }
        report.items += 1;
    }

    Ok(report)
}

/// Computes a token-overlap F1 score.
#[must_use]
pub fn token_f1(actual: &str, expected: &str) -> f64 {
    let actual_tokens = normalized_tokens(actual);
    let expected_tokens = normalized_tokens(expected);
    if actual_tokens.is_empty() || expected_tokens.is_empty() {
        return 0.0;
    }

    let overlap = actual_tokens.intersection(&expected_tokens).count() as f64;
    if overlap == 0.0 {
        return 0.0;
    }
    let precision = overlap / actual_tokens.len() as f64;
    let recall = overlap / expected_tokens.len() as f64;
    2.0 * precision * recall / (precision + recall)
}

fn parse_jsonl_items(path: &Path, content: &str) -> Result<Vec<JsonlDatasetItem>> {
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            serde_json::from_str::<JsonlDatasetItem>(line).map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "invalid JSONL item in {} at line {}: {error}",
                    path.display(),
                    idx + 1
                ))
            })
        })
        .collect()
}

fn replay_evaluator_name(cfg: &ReplayConfig) -> String {
    match (&cfg.model_override, &cfg.embedder_override) {
        (Some(model), Some(embedder)) => format!("replay-f1:{model}:{embedder}"),
        (Some(model), None) => format!("replay-f1:{model}"),
        (None, Some(embedder)) => format!("replay-f1:{embedder}"),
        (None, None) => "f1-overlap".to_string(),
    }
}

fn normalized_tokens(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::token_f1;

    #[test]
    fn token_f1_scores_overlap() {
        assert_eq!(token_f1("alpha beta", "gamma delta"), 0.0);
        assert!(token_f1("alpha beta", "alpha gamma") > 0.0);
        assert_eq!(token_f1("alpha beta", "alpha beta"), 1.0);
    }
}

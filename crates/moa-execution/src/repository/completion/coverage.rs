//! Sequential persisted map-coverage evidence resolution and evaluation.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{CoverageRequirement, ExecutionOperation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{CompletionNodeEvidence, CompletionTaskEvidence};
use crate::{
    Error, Result,
    bindings::{BindingContext, extract_map_key, resolve_bindings},
    capability::hash_serializable,
    repository::{ExecutionRunRecord, ExecutionTaskRecord, row_error, sqlx_error},
    state::ExecutionTaskStatus,
};

const MAX_COVERAGE_EVIDENCE_SAMPLES: usize = 128;
const COVERAGE_EXPECTED_HASH_DOMAIN: &str = "moa.execution.coverage-expected";
const COVERAGE_OBSERVED_HASH_DOMAIN: &str = "moa.execution.coverage-observed";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CoverageTaskEvidence {
    expected_count: u64,
    observed_count: u64,
    matched_count: u64,
    unexpected_count: u64,
    failed_count: u64,
    completed_count: u64,
    completed_matched_count: u64,
    expected_keys_hash: String,
    observed_terminal_hash: String,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedCoverageExpectation {
    map_node_id: String,
    expected_keys: BTreeSet<String>,
    expected_keys_hash: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PersistedCoverageEvaluation {
    pub(super) coverage_id: String,
    pub(super) map_node_id: String,
    pub(super) passed: bool,
    expected_count: u64,
    observed_count: u64,
    matched_count: u64,
    missing_count: u64,
    extra_count: u64,
    failed_count: u64,
    completed_count: u64,
    expected_keys_hash: String,
    observed_terminal_hash: String,
    missing_keys: Vec<String>,
    extra_keys: Vec<String>,
    failed_keys: Vec<String>,
    completed_keys: Vec<String>,
    samples_truncated: bool,
}

pub(super) async fn resolve_coverage_expectation(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    coverage: &CoverageRequirement,
) -> Result<ResolvedCoverageExpectation> {
    let node = run
        .active_plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == coverage.map_node_id)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!(
                "coverage {} references missing map node {}",
                coverage.id, coverage.map_node_id
            ),
        })?;
    let ExecutionOperation::Map {
        item_key,
        max_items,
        ..
    } = &node.operation
    else {
        return Err(Error::InvalidRepositoryData {
            message: format!("coverage {} does not reference a map node", coverage.id),
        });
    };
    let dependencies = node.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let dependency_ids = dependencies.iter().cloned().collect::<Vec<_>>();
    let mut node_outputs = BTreeMap::new();
    if !dependency_ids.is_empty() {
        let rows = sqlx::query(
            "SELECT node_id,aggregate_output FROM moa.execution_node_state \
             WHERE run_uid=$1 AND node_id = ANY($2::TEXT[]) ORDER BY node_id",
        )
        .bind(run.run_uid)
        .bind(&dependency_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        for row in rows {
            let node_id: String = row.try_get("node_id").map_err(row_error)?;
            let aggregate_output: Option<Value> =
                row.try_get("aggregate_output").map_err(row_error)?;
            if let Some(output) = aggregate_output {
                node_outputs.insert(node_id, output);
            }
        }
    }
    let expected = resolve_bindings(
        &coverage.expected_items,
        &BindingContext {
            run_input: &run.input,
            node_outputs: &node_outputs,
            dependencies: &dependencies,
            item: None,
            item_key: None,
        },
    )?;
    let expected = expected
        .as_array()
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!(
                "coverage {} expected_items did not resolve to an array",
                coverage.id
            ),
        })?;
    let expected_count = u64::try_from(expected.len()).map_err(|_| Error::ArithmeticOverflow {
        context: format!("coverage {} expected item count", coverage.id),
    })?;
    if expected_count > *max_items {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "coverage {} expected item count {expected_count} exceeds map {} max_items {max_items}",
                coverage.id, coverage.map_node_id
            ),
        });
    }
    let mut expected_keys = BTreeSet::new();
    for item in expected {
        let key = extract_map_key(item, item_key)?;
        if !expected_keys.insert(key) {
            return Err(Error::InvalidRepositoryData {
                message: format!("coverage {} contains duplicate expected keys", coverage.id),
            });
        }
    }
    let expected_keys_hash =
        hash_serializable(COVERAGE_EXPECTED_HASH_DOMAIN, &expected_keys)?.to_string();
    Ok(ResolvedCoverageExpectation {
        map_node_id: coverage.map_node_id.clone(),
        expected_keys,
        expected_keys_hash,
    })
}

pub(super) fn prepare_coverage_evidence(
    coverage: &CoverageRequirement,
    expectation: &ResolvedCoverageExpectation,
    evidence: &mut CompletionTaskEvidence,
) -> Result<()> {
    let expected_count =
        u64::try_from(expectation.expected_keys.len()).map_err(|_| Error::ArithmeticOverflow {
            context: "coverage expected item count".to_string(),
        })?;
    let persisted = evidence.coverage.entry(coverage.id.clone()).or_default();
    if persisted.expected_keys_hash.is_empty() {
        persisted.expected_count = expected_count;
        persisted.expected_keys_hash = expectation.expected_keys_hash.clone();
    } else if persisted.expected_count != expected_count
        || persisted.expected_keys_hash != expectation.expected_keys_hash
    {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "coverage {} expected item universe changed during completion scan",
                coverage.id
            ),
        });
    }
    Ok(())
}

pub(super) fn accumulate_task_coverage_evidence(
    coverage: &CoverageRequirement,
    task: &ExecutionTaskRecord,
    expectation: &ResolvedCoverageExpectation,
    evidence: &mut CompletionTaskEvidence,
) -> Result<()> {
    if expectation.map_node_id != task.node_id {
        return Ok(());
    }
    let is_completed = task.status == ExecutionTaskStatus::Completed;
    let is_failed = matches!(
        task.status,
        ExecutionTaskStatus::Failed
            | ExecutionTaskStatus::UnknownOutcome
            | ExecutionTaskStatus::Cancelled
    );
    if !is_completed && !is_failed {
        return Ok(());
    }
    let persisted =
        evidence
            .coverage
            .get_mut(&coverage.id)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: format!("coverage {} has no initialized task evidence", coverage.id),
            })?;
    persisted.observed_count =
        checked_evidence_increment(persisted.observed_count, "coverage observed item count")?;
    let matched = expectation.expected_keys.contains(&task.item_key);
    if matched {
        persisted.matched_count =
            checked_evidence_increment(persisted.matched_count, "coverage matched item count")?;
    } else {
        persisted.unexpected_count = checked_evidence_increment(
            persisted.unexpected_count,
            "coverage unexpected item count",
        )?;
    }
    if is_failed {
        persisted.failed_count =
            checked_evidence_increment(persisted.failed_count, "coverage failed item count")?;
    } else {
        persisted.completed_count =
            checked_evidence_increment(persisted.completed_count, "coverage completed item count")?;
        if matched {
            persisted.completed_matched_count = checked_evidence_increment(
                persisted.completed_matched_count,
                "coverage completed matched item count",
            )?;
        }
    }
    persisted.observed_terminal_hash = hash_serializable(
        COVERAGE_OBSERVED_HASH_DOMAIN,
        &(
            &persisted.observed_terminal_hash,
            &task.item_key,
            task.status,
        ),
    )?
    .to_string();
    Ok(())
}

fn checked_evidence_increment(value: u64, context: &str) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: context.to_string(),
        })
}

fn persisted_coverage_passed(
    coverage: &CoverageRequirement,
    nodes: &CompletionNodeEvidence,
    evidence: &CompletionTaskEvidence,
) -> Result<bool> {
    let Some(task_evidence) = evidence.coverage.get(&coverage.id) else {
        return Ok(false);
    };
    if task_evidence.matched_count > task_evidence.expected_count
        || task_evidence
            .matched_count
            .checked_add(task_evidence.unexpected_count)
            != Some(task_evidence.observed_count)
        || task_evidence
            .failed_count
            .checked_add(task_evidence.completed_count)
            != Some(task_evidence.observed_count)
        || task_evidence.completed_matched_count > task_evidence.matched_count
    {
        return Err(Error::InvalidRepositoryData {
            message: format!("coverage {} has inconsistent persisted counts", coverage.id),
        });
    }
    let node_passed = nodes
        .coverage_passed
        .get(&coverage.id)
        .copied()
        .unwrap_or(false);
    let expected_passed = if coverage.require_all {
        task_evidence.matched_count == task_evidence.expected_count
    } else {
        task_evidence.expected_count == 0 || task_evidence.completed_matched_count > 0
    };
    Ok(node_passed
        && task_evidence.unexpected_count == 0
        && task_evidence.failed_count == 0
        && expected_passed)
}

pub(super) fn coverage_by_node(
    run: &ExecutionRunRecord,
    nodes: &CompletionNodeEvidence,
    evidence: &CompletionTaskEvidence,
) -> Result<BTreeMap<String, bool>> {
    let mut by_node = BTreeMap::new();
    for coverage in &run.goal.coverage {
        let passed = persisted_coverage_passed(coverage, nodes, evidence)?;
        by_node
            .entry(coverage.map_node_id.clone())
            .and_modify(|current| *current &= passed)
            .or_insert(passed);
    }
    Ok(by_node)
}

pub(super) async fn load_persisted_coverage_evaluations(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    nodes: &CompletionNodeEvidence,
    evidence: &CompletionTaskEvidence,
) -> Result<Vec<PersistedCoverageEvaluation>> {
    let mut sample_budget = MAX_COVERAGE_EVIDENCE_SAMPLES;
    let mut evaluations = Vec::with_capacity(run.goal.coverage.len());
    for coverage in &run.goal.coverage {
        let expectation = resolve_coverage_expectation(conn, run, coverage).await?;
        let counts =
            evidence
                .coverage
                .get(&coverage.id)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: format!("coverage {} has no persisted task evidence", coverage.id),
                })?;
        let expected_keys = expectation
            .expected_keys
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let missing_count = counts
            .expected_count
            .checked_sub(counts.matched_count)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: format!(
                    "coverage {} matched count exceeds expected count",
                    coverage.id
                ),
            })?;
        let missing_keys = load_coverage_key_samples(
            conn,
            run.run_uid,
            &coverage.map_node_id,
            CoverageSampleKind::Missing(&expected_keys),
            &mut sample_budget,
        )
        .await?;
        let extra_keys = load_coverage_key_samples(
            conn,
            run.run_uid,
            &coverage.map_node_id,
            CoverageSampleKind::Extra(&expected_keys),
            &mut sample_budget,
        )
        .await?;
        let failed_keys = load_coverage_key_samples(
            conn,
            run.run_uid,
            &coverage.map_node_id,
            CoverageSampleKind::Failed,
            &mut sample_budget,
        )
        .await?;
        let completed_keys = load_coverage_key_samples(
            conn,
            run.run_uid,
            &coverage.map_node_id,
            CoverageSampleKind::Completed,
            &mut sample_budget,
        )
        .await?;
        let sampled =
            missing_keys.len() + extra_keys.len() + failed_keys.len() + completed_keys.len();
        let total = missing_count
            .checked_add(counts.unexpected_count)
            .and_then(|value| value.checked_add(counts.failed_count))
            .and_then(|value| value.checked_add(counts.completed_count))
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "coverage evidence sample count".to_string(),
            })?;
        evaluations.push(PersistedCoverageEvaluation {
            coverage_id: coverage.id.clone(),
            map_node_id: coverage.map_node_id.clone(),
            passed: persisted_coverage_passed(coverage, nodes, evidence)?,
            expected_count: counts.expected_count,
            observed_count: counts.observed_count,
            matched_count: counts.matched_count,
            missing_count,
            extra_count: counts.unexpected_count,
            failed_count: counts.failed_count,
            completed_count: counts.completed_count,
            expected_keys_hash: counts.expected_keys_hash.clone(),
            observed_terminal_hash: counts.observed_terminal_hash.clone(),
            missing_keys,
            extra_keys,
            failed_keys,
            completed_keys,
            samples_truncated: u64::try_from(sampled).map_or(true, |sampled| sampled < total),
        });
    }
    Ok(evaluations)
}

enum CoverageSampleKind<'a> {
    Missing(&'a [String]),
    Extra(&'a [String]),
    Failed,
    Completed,
}

async fn load_coverage_key_samples(
    conn: &mut PgConnection,
    run_uid: Uuid,
    node_id: &str,
    kind: CoverageSampleKind<'_>,
    remaining_budget: &mut usize,
) -> Result<Vec<String>> {
    if *remaining_budget == 0 {
        return Ok(Vec::new());
    }
    let limit = *remaining_budget;
    let limit = i64::try_from(limit).map_err(|_| Error::ArithmeticOverflow {
        context: "coverage evidence sample limit".to_string(),
    })?;
    let rows = match kind {
        CoverageSampleKind::Missing(expected) => sqlx::query_scalar::<_, String>(
            "SELECT key FROM unnest($3::TEXT[]) AS expected(key) WHERE NOT EXISTS ( \
               SELECT 1 FROM moa.execution_task task WHERE task.run_uid=$1 \
                 AND task.node_id=$2 AND task.item_key=expected.key \
                 AND task.status IN ('completed','failed','unknown_outcome','cancelled')) \
             ORDER BY key LIMIT $4",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(expected)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?,
        CoverageSampleKind::Extra(expected) => sqlx::query_scalar::<_, String>(
            "SELECT item_key FROM moa.execution_task WHERE run_uid=$1 AND node_id=$2 \
               AND status IN ('completed','failed','unknown_outcome','cancelled') \
               AND NOT (item_key = ANY($3::TEXT[])) ORDER BY item_key LIMIT $4",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(expected)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?,
        CoverageSampleKind::Failed => sqlx::query_scalar::<_, String>(
            "SELECT item_key FROM moa.execution_task WHERE run_uid=$1 AND node_id=$2 \
               AND status IN ('failed','unknown_outcome','cancelled') ORDER BY item_key LIMIT $3",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?,
        CoverageSampleKind::Completed => sqlx::query_scalar::<_, String>(
            "SELECT item_key FROM moa.execution_task WHERE run_uid=$1 AND node_id=$2 \
               AND status='completed' ORDER BY item_key LIMIT $3",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await
        .map_err(sqlx_error)?,
    };
    *remaining_budget = remaining_budget.saturating_sub(rows.len());
    Ok(rows)
}

//! Atomic node-state reconciliation for compiler-validated plan amendments.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{CanonicalExecutionPlan, Error, Result};

use super::super::{
    ExecutionRunRecord, ExecutionTaskRecord, row_error, rows::required_u64, sqlx_error, to_i64,
};

/// Reconciles persisted node state with one validated replacement plan.
pub(super) async fn reconcile_amendment_node_state_in_conn(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    superseded_task: &ExecutionTaskRecord,
    active_plan: &CanonicalExecutionPlan,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT node_id,node_status,materialization_cursor,materialization_complete, \
                total_task_count FROM moa.execution_node_state \
         WHERE run_uid=$1 ORDER BY node_order FOR UPDATE",
    )
    .bind(run.run_uid)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let current_nodes = run
        .active_plan
        .definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    if rows.len() != current_nodes.len() {
        return Err(Error::InvalidRepositoryData {
            message: "persisted node state does not exactly cover the active plan before amendment"
                .to_string(),
        });
    }
    let replacement_nodes = active_plan
        .definition
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut preserved = BTreeMap::new();
    for row in &rows {
        let node_id: String = row.try_get("node_id").map_err(row_error)?;
        let status: String = row.try_get("node_status").map_err(row_error)?;
        let cursor = required_u64(row, "materialization_cursor")?;
        let materialization_complete: bool =
            row.try_get("materialization_complete").map_err(row_error)?;
        let total_tasks = required_u64(row, "total_task_count")?;
        let current =
            current_nodes
                .get(node_id.as_str())
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: format!(
                        "persisted amendment node `{node_id}` is absent from active plan"
                    ),
                })?;
        let unstarted =
            status == "pending" && cursor == 0 && !materialization_complete && total_tasks == 0;
        let superseded_wait =
            node_id == superseded_task.node_id && status == "waiting" && total_tasks == 1;
        if unstarted || superseded_wait {
            continue;
        }
        let replacement = replacement_nodes.get(node_id.as_str()).ok_or_else(|| {
            Error::InvalidRepositoryInput {
                message: format!("amendment removes started node `{node_id}`"),
            }
        })?;
        if *current != *replacement {
            return Err(Error::InvalidRepositoryInput {
                message: format!("amendment rewrites started node `{node_id}`"),
            });
        }
        preserved.insert(node_id, status);
    }
    if replacement_nodes.contains_key(superseded_task.node_id.as_str()) {
        return Err(Error::InvalidRepositoryInput {
            message: "amendment retains the superseded WaitingReplan node identity".to_string(),
        });
    }

    let shift = i64::try_from(
        run.active_plan
            .definition
            .nodes
            .len()
            .saturating_add(active_plan.definition.nodes.len())
            .saturating_add(1),
    )
    .map_err(|_| Error::ArithmeticOverflow {
        context: "amendment node-order reconciliation shift".to_string(),
    })?;
    if !preserved.is_empty() {
        let preserved_ids = preserved.keys().cloned().collect::<Vec<_>>();
        sqlx::query(
            "UPDATE moa.execution_node_state SET node_order=node_order+$3,updated_at=NOW() \
             WHERE run_uid=$1 AND node_id=ANY($2::TEXT[])",
        )
        .bind(run.run_uid)
        .bind(&preserved_ids)
        .bind(shift)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    }
    let preserved_ids = preserved.keys().cloned().collect::<Vec<_>>();
    sqlx::query(
        "DELETE FROM moa.execution_node_state WHERE run_uid=$1 \
         AND NOT (node_id=ANY($2::TEXT[]))",
    )
    .bind(run.run_uid)
    .bind(&preserved_ids)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;

    let resolved_dependencies = preserved
        .iter()
        .filter(|(_, status)| matches!(status.as_str(), "completed" | "skipped"))
        .map(|(node_id, _)| node_id.as_str())
        .collect::<BTreeSet<_>>();
    for (node_order, node) in active_plan.definition.nodes.iter().enumerate() {
        let node_order = i64::try_from(node_order).map_err(|_| Error::ArithmeticOverflow {
            context: "amendment node order".to_string(),
        })?;
        if preserved.contains_key(&node.id) {
            let updated = sqlx::query(
                "UPDATE moa.execution_node_state SET node_order=$3,updated_at=NOW() \
                 WHERE run_uid=$1 AND node_id=$2",
            )
            .bind(run.run_uid)
            .bind(&node.id)
            .bind(node_order)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
            if updated.rows_affected() != 1 {
                return Err(Error::InvalidRepositoryData {
                    message: format!("preserved amendment node `{}` disappeared", node.id),
                });
            }
            continue;
        }
        if node.depends_on.iter().any(|dependency| {
            preserved
                .get(dependency)
                .is_some_and(|status| matches!(status.as_str(), "failed" | "cancelled"))
        }) {
            return Err(Error::InvalidRepositoryInput {
                message: format!("amendment node `{}` depends on a failed node", node.id),
            });
        }
        let dependency_count =
            u64::try_from(node.depends_on.len()).map_err(|_| Error::ArithmeticOverflow {
                context: "amendment node dependency count".to_string(),
            })?;
        let resolved_count = u64::try_from(
            node.depends_on
                .iter()
                .filter(|dependency| resolved_dependencies.contains(dependency.as_str()))
                .count(),
        )
        .map_err(|_| Error::ArithmeticOverflow {
            context: "amendment resolved dependency count".to_string(),
        })?;
        let remaining = dependency_count
            .checked_sub(resolved_count)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: format!("amendment node `{}` has invalid dependency counts", node.id),
            })?;
        sqlx::query(
            "INSERT INTO moa.execution_node_state (node_state_uid,tenant_id,run_uid,node_id, \
                 node_order,dependency_count,remaining_dependency_count) \
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(Uuid::new_v5(&run.run_uid, node.id.as_bytes()))
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(&node.id)
        .bind(node_order)
        .bind(to_i64(dependency_count, "amendment node dependency count")?)
        .bind(to_i64(remaining, "amendment remaining dependency count")?)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
    }
    Ok(())
}

//! Bounded PostgreSQL repository for durable tenant purge.

use moa_authz::outbox::invert_tenant_batch;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Outcome of an idempotent relational tenant purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelationalPurgeOutcome {
    /// This invocation drained the remaining stages and committed relational deletion.
    Committed,
    /// The same operation had already committed relational deletion.
    AlreadyCommitted,
}

#[derive(Debug, sqlx::FromRow)]
struct PurgeProgress {
    status: String,
    current_stage: String,
}

#[derive(Debug, sqlx::FromRow)]
struct PurgeBatch {
    batch_state: String,
    stage: String,
    affected: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PurgeStep {
    Authz,
    Relational,
    Complete(RelationalPurgeOutcome),
}

impl PurgeStep {
    fn from_progress(progress: PurgeProgress) -> Result<Self, String> {
        match (progress.status.as_str(), progress.current_stage.as_str()) {
            ("in_progress", "authz") => Ok(Self::Authz),
            ("in_progress", stage) if !stage.is_empty() && stage != "complete" => {
                Ok(Self::Relational)
            }
            ("relationally_committed", "complete") => {
                Ok(Self::Complete(RelationalPurgeOutcome::AlreadyCommitted))
            }
            (status, stage) => Err(format!(
                "invalid tenant purge progress state/stage pair {status}/{stage}"
            )),
        }
    }

    fn from_batch(batch: &PurgeBatch) -> Result<Self, String> {
        match (batch.batch_state.as_str(), batch.stage.as_str()) {
            ("in_progress", "authz") => Ok(Self::Authz),
            ("in_progress", stage) if !stage.is_empty() && stage != "complete" => {
                Ok(Self::Relational)
            }
            ("committed", "complete") => Ok(Self::Complete(RelationalPurgeOutcome::Committed)),
            ("already_committed", "complete") => {
                Ok(Self::Complete(RelationalPurgeOutcome::AlreadyCommitted))
            }
            (state, stage) => Err(format!(
                "invalid tenant purge batch state/stage pair {state}/{stage}"
            )),
        }
    }
}

/// Acquires the exclusive tenant destruction boundary and initializes progress.
///
/// The database function owns legal-hold validation and both durable control
/// rows. It is idempotent for the same tenant/operation pair and rejects a
/// competing operation.
pub(super) async fn start_fenced_purge(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<(), String> {
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(operation_id)
        .execute(pool)
        .await
        .map_err(|error| format!("start bounded tenant purge: {error}"))?;
    Ok(())
}

/// Drains authorization and relational tenant state in 1,000-row transactions.
///
/// Every SQL function call is one short autocommit transaction. PostgreSQL owns
/// the progress row, stage order, fence validation, keyset cursor, CTID batch,
/// counters, and final residue proof, so a process crash can only repeat or
/// resume a committed batch.
pub async fn purge_relational(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<RelationalPurgeOutcome, String> {
    start_fenced_purge(pool, tenant_id, operation_id).await?;
    let progress: PurgeProgress = sqlx::query_as(
        "SELECT status, current_stage \
         FROM moa.tenant_purge_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .map_err(|error| format!("load bounded tenant purge progress: {error}"))?;
    let mut step = PurgeStep::from_progress(progress)?;

    loop {
        step = match step {
            PurgeStep::Complete(outcome) => return Ok(outcome),
            PurgeStep::Authz => {
                let batch = invert_tenant_batch(pool, tenant_id, operation_id)
                    .await
                    .map_err(|error| format!("invert tenant authorization batch: {error}"))?;
                if batch.exhausted {
                    PurgeStep::Relational
                } else {
                    PurgeStep::Authz
                }
            }
            PurgeStep::Relational => {
                let batch: PurgeBatch = sqlx::query_as(
                    "SELECT batch_state, stage, affected \
                     FROM moa.run_tenant_purge_batch($1, $2)",
                )
                .bind(tenant_id)
                .bind(operation_id)
                .fetch_one(pool)
                .await
                .map_err(|error| format!("drain tenant purge batch: {error}"))?;
                tracing::debug!(
                    tenant_id = %tenant_id,
                    operation_id,
                    stage = %batch.stage,
                    affected = batch.affected,
                    state = %batch.batch_state,
                    "tenant purge batch committed"
                );
                PurgeStep::from_batch(&batch)?
            }
        };
    }
}

/// Loads one keyset page of node uids owned by the tenant, ordered by uid.
///
/// The external-vector purge stage walks the tenant's graph nodes in stable uid
/// order so remote deletes run without holding a PostgreSQL connection. Returns
/// at most `limit` uids strictly greater than `after_uid`, or all of the
/// tenant's uids from the start when `after_uid` is `None`.
pub(super) async fn load_external_vector_uid_page(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    after_uid: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, String> {
    sqlx::query_scalar(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE tenant_id = $1
          AND ($2::UUID IS NULL OR uid > $2)
        ORDER BY uid
        LIMIT $3
        "#,
    )
    .bind(tenant_id.0)
    .bind(after_uid)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load tenant vector ids: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{PurgeBatch, PurgeProgress, PurgeStep, RelationalPurgeOutcome};

    // Pins: durable progress accepts only the state/stage pairs emitted by the
    // bounded tenant-purge functions.
    #[test]
    fn purge_step_rejects_invalid_durable_progress_pairs_offline() {
        let error = PurgeStep::from_progress(PurgeProgress {
            status: "in_progress".to_string(),
            current_stage: "complete".to_string(),
        })
        .expect_err("in-progress/complete must fail closed");
        assert_eq!(
            error,
            "invalid tenant purge progress state/stage pair in_progress/complete"
        );
    }

    // Pins: the repository follows returned state and stage instead of re-reading progress.
    #[test]
    fn purge_step_maps_sql_batch_transitions_offline() {
        assert_eq!(
            PurgeStep::from_batch(&PurgeBatch {
                batch_state: "in_progress".to_string(),
                stage: "authz".to_string(),
                affected: 0,
            }),
            Ok(PurgeStep::Authz)
        );
        assert_eq!(
            PurgeStep::from_batch(&PurgeBatch {
                batch_state: "in_progress".to_string(),
                stage: "public.users".to_string(),
                affected: 1,
            }),
            Ok(PurgeStep::Relational)
        );
        assert_eq!(
            PurgeStep::from_batch(&PurgeBatch {
                batch_state: "committed".to_string(),
                stage: "complete".to_string(),
                affected: 0,
            }),
            Ok(PurgeStep::Complete(RelationalPurgeOutcome::Committed))
        );

        let error = PurgeStep::from_batch(&PurgeBatch {
            batch_state: "committed".to_string(),
            stage: "public.users".to_string(),
            affected: 0,
        })
        .expect_err("committed/non-complete must fail closed");
        assert_eq!(
            error,
            "invalid tenant purge batch state/stage pair committed/public.users"
        );
    }
}

//! PostgreSQL persistence for four-eyes dual-control requests.
//!
//! This module owns every statement against `moa.dual_control_request`, the
//! tenant-scoped RLS transaction the statements run in, and the mapping from
//! result rows to typed storage outcomes. It makes no policy decisions: the
//! segregation-of-duties rule, the already-decided rule, and the idempotent
//! replay rule all live in the parent [`super`] module, which drives this
//! transaction and interprets the outcomes it returns.
//!
//! All writes run under a tenant-scoped `moa_app` transaction
//! ([`moa_db::ScopedConn`]) so the table's row-level security policy pins every
//! statement to the caller's tenant. Dropping a [`DualControlTx`] without
//! calling [`DualControlTx::commit`] rolls the transaction back.

use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::DualControlError;

/// Persisted lifecycle state of one dual-control request row.
///
/// The set is closed by the table's `dual_control_status_check` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestStatus {
    /// Raised by the first admin and awaiting a decision.
    Pending,
    /// Approved by a second, distinct admin and available for consumption.
    Approved,
    /// Already claimed by a guarded operation's execute path.
    Consumed,
}

impl RequestStatus {
    /// Maps a persisted `status` column value to its typed state.
    ///
    /// A value outside the table's `dual_control_status_check` constraint is
    /// unreachable in a healthy database and is reported as a storage failure
    /// rather than being silently treated as a decided request.
    fn from_column(value: &str) -> Result<Self, DualControlError> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "consumed" => Ok(Self::Consumed),
            other => Err(DualControlError::Storage(format!(
                "unknown dual-control request status: {other}"
            ))),
        }
    }
}

/// A dual-control request row locked for a check-then-write decision.
#[derive(Debug, Clone)]
pub(super) struct LockedRequest {
    /// Operator that raised the request, for the segregation-of-duties check.
    pub(super) requested_by: String,
    /// Current lifecycle state of the row.
    pub(super) status: RequestStatus,
}

/// A locked candidate row for consuming an approval.
#[derive(Debug, Clone, Copy)]
pub(super) struct ConsumptionCandidate {
    /// Identifier of the locked request.
    pub(super) request_id: Uuid,
    /// Current lifecycle state of the locked request.
    pub(super) status: RequestStatus,
}

/// Tenant-scoped RLS transaction over `moa.dual_control_request`.
///
/// Statements issued through this handle share one transaction, so row locks
/// and advisory locks taken here are held until [`DualControlTx::commit`] (or
/// the rollback that follows a drop).
pub(super) struct DualControlTx<'p> {
    conn: ScopedConn<'p>,
}

impl<'p> DualControlTx<'p> {
    /// Begins a tenant-scoped `moa_app` transaction for dual-control writes.
    pub(super) async fn begin(
        pool: &'p PgPool,
        tenant_id: TenantId,
    ) -> Result<Self, DualControlError> {
        let conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
            .await
            .map_err(storage_error)?;
        Ok(Self { conn })
    }

    /// Inserts a pending request and returns its generated id.
    pub(super) async fn insert_pending_request(
        &mut self,
        tenant_id: TenantId,
        operation_type: &str,
        operation_digest: &str,
        requested_by: &str,
    ) -> Result<Uuid, DualControlError> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO moa.dual_control_request
                (tenant_id, operation_type, operation_ref, requested_by)
            VALUES ($1, $2, $3, $4)
            RETURNING id
            "#,
        )
        .bind(tenant_id.0)
        .bind(operation_type)
        .bind(operation_digest)
        .bind(requested_by)
        .fetch_one(self.conn.as_mut())
        .await
        .map_err(storage_error)
    }

    /// Locks one request by id and returns its requester and state.
    ///
    /// Returns `None` when no such request exists in the tenant. The row stays
    /// locked for the remainder of the transaction, so two racing approvals of
    /// the same request cannot both observe it as pending.
    pub(super) async fn lock_request(
        &mut self,
        tenant_id: TenantId,
        request_id: Uuid,
    ) -> Result<Option<LockedRequest>, DualControlError> {
        let row = sqlx::query(
            r#"
            SELECT requested_by, status
            FROM moa.dual_control_request
            WHERE id = $1 AND tenant_id = $2
            FOR UPDATE
            "#,
        )
        .bind(request_id)
        .bind(tenant_id.0)
        .fetch_optional(self.conn.as_mut())
        .await
        .map_err(storage_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let requested_by: String = row.try_get("requested_by").map_err(storage_error)?;
        let status: String = row.try_get("status").map_err(storage_error)?;
        Ok(Some(LockedRequest {
            requested_by,
            status: RequestStatus::from_column(&status)?,
        }))
    }

    /// Marks a locked request approved by the given approver.
    pub(super) async fn mark_approved(
        &mut self,
        tenant_id: TenantId,
        request_id: Uuid,
        approver: &str,
    ) -> Result<(), DualControlError> {
        sqlx::query(
            r#"
            UPDATE moa.dual_control_request
            SET status = 'approved', approved_by = $3, approved_at = NOW()
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(request_id)
        .bind(tenant_id.0)
        .bind(approver)
        .execute(self.conn.as_mut())
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Serializes consumption of one operation across processes and replicas.
    ///
    /// Unlike `SKIP LOCKED`, waiting on this transaction-scoped advisory lock
    /// cannot turn ordinary lock contention into a false missing-approval
    /// result.
    pub(super) async fn lock_operation(
        &mut self,
        operation_digest: &str,
    ) -> Result<(), DualControlError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(operation_digest)
            .execute(self.conn.as_mut())
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    /// Locks the single row that may satisfy this consumption, if any.
    ///
    /// Prefers a row already consumed by `consumer_ref` so a durable retry of
    /// the same execution never burns another approval; otherwise it locks the
    /// oldest approved row whose approver is distinct from its requester.
    /// Returns `None` when neither exists.
    pub(super) async fn lock_consumable_request(
        &mut self,
        tenant_id: TenantId,
        operation_type: &str,
        operation_digest: &str,
        consumer_ref: &str,
    ) -> Result<Option<ConsumptionCandidate>, DualControlError> {
        let candidate = sqlx::query(
            r#"
            SELECT id, status
            FROM moa.dual_control_request
            WHERE tenant_id = $1
              AND operation_type = $2
              AND operation_ref = $3
              AND (
                  (status = 'consumed' AND consumed_ref = $4)
                  OR (
                      status = 'approved'
                      AND approved_by IS NOT NULL
                      AND approved_by <> requested_by
                  )
              )
            ORDER BY
                CASE WHEN status = 'consumed' THEN 0 ELSE 1 END,
                approved_at,
                id
            LIMIT 1
            FOR UPDATE
            "#,
        )
        .bind(tenant_id.0)
        .bind(operation_type)
        .bind(operation_digest)
        .bind(consumer_ref)
        .fetch_optional(self.conn.as_mut())
        .await
        .map_err(storage_error)?;
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let request_id: Uuid = candidate.try_get("id").map_err(storage_error)?;
        let status: String = candidate.try_get("status").map_err(storage_error)?;
        Ok(Some(ConsumptionCandidate {
            request_id,
            status: RequestStatus::from_column(&status)?,
        }))
    }

    /// Marks a locked, approved request consumed by `consumer_ref`.
    ///
    /// Fails with [`DualControlError::Storage`] when the locked row is no
    /// longer approved, so a lost race can never be reported as a successful
    /// consumption.
    pub(super) async fn mark_consumed(
        &mut self,
        tenant_id: TenantId,
        request_id: Uuid,
        consumer_ref: &str,
    ) -> Result<(), DualControlError> {
        let updated = sqlx::query(
            r#"
            UPDATE moa.dual_control_request
            SET status = 'consumed', consumed_at = NOW(), consumed_ref = $3
            WHERE id = $1 AND tenant_id = $2 AND status = 'approved'
            "#,
        )
        .bind(request_id)
        .bind(tenant_id.0)
        .bind(consumer_ref)
        .execute(self.conn.as_mut())
        .await
        .map_err(storage_error)?;
        if updated.rows_affected() != 1 {
            return Err(DualControlError::Storage(
                "locked dual-control approval changed before consumption".to_string(),
            ));
        }
        Ok(())
    }

    /// Commits the transaction, releasing its row and advisory locks.
    pub(super) async fn commit(self) -> Result<(), DualControlError> {
        self.conn.commit().await.map_err(storage_error)
    }
}

/// Wraps a storage failure in the dual-control error surface.
fn storage_error(error: impl std::fmt::Display) -> DualControlError {
    DualControlError::Storage(error.to_string())
}

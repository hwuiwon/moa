//! Four-eyes dual control with segregation of duties for privileged operations.
//!
//! A privileged, irreversible operation can opt into dual control: it must be
//! requested by one tenant admin and then approved by a SECOND, DISTINCT tenant
//! admin before it may execute. This module owns the persistence and the
//! segregation-of-duties (SoD) rule; the calling service owns authorization and
//! decides when the control applies.
//!
//! # Model
//!
//! One [`moa.dual_control_request`] row tracks a privileged operation through
//! three states:
//!
//! - [`request`] (operator A) inserts a `pending` row and returns its id.
//! - [`approve`] (operator B) marks it `approved`, recording `approved_by = B`.
//!   It FAILS CLOSED with [`DualControlError::SelfApproval`] when `B` equals the
//!   requester, which is the segregation-of-duties invariant.
//! - [`consume_approval_for`] is called on the guarded operation's execute path.
//!   It atomically claims one `approved`, un-consumed request whose approver is
//!   distinct from its requester, marks it `consumed`, and refuses (fails closed
//!   with [`DualControlError::NoValidApproval`]) when no such approval exists.
//!
//! `operation_type` names the guarded operation class (e.g. `privacy.erase`). The
//! service hashes the caller's canonical `operation_ref` into a versioned,
//! domain-separated, length-framed digest before persistence, so an approval binds
//! to exactly one request without storing raw tenant, subject, or reason material.
//!
//! # Idempotent consumption
//!
//! Guarded operations run inside durable, replayable steps, so
//! [`consume_approval_for`] takes a `consumer_ref` idempotency key (e.g. the
//! erasure approval-token JTI). The first consumption records it; a re-execution
//! of the SAME operation presents the same `consumer_ref` and is admitted without
//! consuming a second approval, while a genuinely new operation (a different
//! `consumer_ref`) cannot reuse an already-consumed approval and must obtain a
//! fresh one.
//!
//! # Authorization
//!
//! These functions perform NO authorization. Callers MUST authorize the actor as
//! a tenant admin (e.g. `authorize_tenant(.., Relation::Admin)`) before invoking
//! [`request`] or [`approve`]; the SoD check here is an additional control, not a
//! substitute for authorization. Writes run against the RLS-protected table under
//! a tenant-scoped `moa_app` transaction ([`moa_db::ScopedConn`]).

use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const OPERATION_REF_DIGEST_DOMAIN: &[u8] = b"moa.orchestrator.dual_control.operation_ref";
const OPERATION_REF_DIGEST_VERSION: &str = "v1";

/// Errors returned by the dual-control registry.
#[derive(Debug, thiserror::Error)]
pub enum DualControlError {
    /// The referenced request does not exist in the tenant.
    #[error("dual-control request not found")]
    NotFound,
    /// The request was already approved or consumed and cannot be approved again.
    #[error("dual-control request already decided")]
    AlreadyDecided,
    /// Segregation-of-duties violation: the approver is the same operator that
    /// requested the operation.
    #[error("segregation of duties: a request cannot be approved by its requester")]
    SelfApproval,
    /// No valid, distinct, un-consumed approval exists for the operation, so the
    /// guarded operation must fail closed.
    #[error("no distinct dual-control approval available for this operation")]
    NoValidApproval,
    /// Underlying storage failure.
    #[error("dual-control storage error: {0}")]
    Storage(String),
}

impl DualControlError {
    /// Maps the error to a Restate handler error with a fail-closed HTTP status.
    #[must_use]
    pub fn into_handler_error(self) -> HandlerError {
        match self {
            Self::NotFound => TerminalError::new_with_code(404, self.to_string()).into(),
            Self::AlreadyDecided => TerminalError::new_with_code(409, self.to_string()).into(),
            // A self-approval and a missing approval are both authorization-shaped
            // refusals of a privileged operation, so both surface as 403.
            Self::SelfApproval | Self::NoValidApproval => {
                TerminalError::new_with_code(403, self.to_string()).into()
            }
            Self::Storage(_) => TerminalError::new(self.to_string()).into(),
        }
    }
}

fn storage_error(error: impl std::fmt::Display) -> DualControlError {
    DualControlError::Storage(error.to_string())
}

fn digest_operation_ref(tenant_id: TenantId, operation_type: &str, operation_ref: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    update_digest_field(&mut hasher, OPERATION_REF_DIGEST_DOMAIN);
    update_digest_field(&mut hasher, OPERATION_REF_DIGEST_VERSION.as_bytes());
    update_digest_field(&mut hasher, tenant_id.0.as_bytes());
    update_digest_field(&mut hasher, operation_type.as_bytes());
    update_digest_field(&mut hasher, operation_ref.as_bytes());
    format!(
        "{OPERATION_REF_DIGEST_VERSION}:blake3:{}",
        hasher.finalize().to_hex()
    )
}

fn update_digest_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Records a pending dual-control request raised by the first tenant admin and
/// returns its id.
///
/// // SAFETY: performs NO authorization. The caller MUST authorize `requested_by`
/// as a tenant admin before invoking this (see module docs).
pub async fn request(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_type: &str,
    operation_ref: &str,
    requested_by: &str,
) -> Result<Uuid, DualControlError> {
    let operation_digest = digest_operation_ref(tenant_id, operation_type, operation_ref);
    let mut conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .map_err(storage_error)?;
    let id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO moa.dual_control_request
            (tenant_id, operation_type, operation_ref, requested_by)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
    )
    .bind(tenant_id.0)
    .bind(operation_type)
    .bind(&operation_digest)
    .bind(requested_by)
    .fetch_one(conn.as_mut())
    .await
    .map_err(storage_error)?;
    conn.commit().await.map_err(storage_error)?;
    tracing::info!(
        tenant_id = %tenant_id,
        operation_type,
        request_id = %id,
        "dual-control request raised"
    );
    Ok(id)
}

/// Approves a pending dual-control request as the second, distinct tenant admin.
///
/// Fails closed with [`DualControlError::SelfApproval`] when `approver` equals the
/// requester (segregation of duties), with [`DualControlError::NotFound`] when the
/// request does not exist in the tenant, and with
/// [`DualControlError::AlreadyDecided`] when it is no longer pending. The row is
/// locked for the check-then-update so two racing approvals cannot both win.
///
/// // SAFETY: performs NO authorization. The caller MUST authorize `approver` as a
/// tenant admin before invoking this (see module docs).
pub async fn approve(
    pool: &PgPool,
    tenant_id: TenantId,
    request_id: Uuid,
    approver: &str,
) -> Result<(), DualControlError> {
    let mut conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .map_err(storage_error)?;
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
    .fetch_optional(conn.as_mut())
    .await
    .map_err(storage_error)?;
    let Some(row) = row else {
        return Err(DualControlError::NotFound);
    };
    let requested_by: String = row.try_get("requested_by").map_err(storage_error)?;
    let status: String = row.try_get("status").map_err(storage_error)?;
    if status != "pending" {
        return Err(DualControlError::AlreadyDecided);
    }
    // Segregation of duties: the operator that requested the privileged operation
    // may never be the one that authorizes it. This is the load-bearing SoD check;
    // the table's dual_control_sod_check constraint is a defense-in-depth backstop.
    if approver == requested_by {
        tracing::warn!(
            tenant_id = %tenant_id,
            request_id = %request_id,
            "dual-control approval rejected: segregation-of-duties violation (approver == requester)"
        );
        return Err(DualControlError::SelfApproval);
    }
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
    .execute(conn.as_mut())
    .await
    .map_err(storage_error)?;
    conn.commit().await.map_err(storage_error)?;
    tracing::info!(
        tenant_id = %tenant_id,
        request_id = %request_id,
        "dual-control request approved by a distinct admin (segregation of duties satisfied)"
    );
    Ok(())
}

/// Consumes a valid, distinct dual-control approval for a guarded operation, or
/// fails closed with [`DualControlError::NoValidApproval`].
///
/// Atomically claims one `approved`, not-yet-consumed request whose approver is
/// distinct from its requester and marks it `consumed`, stamping `consumer_ref` so
/// a durable re-execution of the SAME operation (matching `consumer_ref`) is
/// admitted without consuming a second approval. A different `consumer_ref` cannot
/// reuse an already-consumed approval and must obtain a fresh one.
///
/// // SAFETY: performs NO authorization. This is called from the guarded
/// operation's already-authorized execute path.
pub async fn consume_approval_for(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_type: &str,
    operation_ref: &str,
    consumer_ref: &str,
) -> Result<(), DualControlError> {
    let operation_digest = digest_operation_ref(tenant_id, operation_type, operation_ref);
    let mut conn = ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .map_err(storage_error)?;

    // Serialize consumption for this exact operation across every process and
    // Kubernetes replica. Unlike SKIP LOCKED, waiting here cannot turn ordinary
    // lock contention into a false missing-approval result.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&operation_digest)
        .execute(conn.as_mut())
        .await
        .map_err(storage_error)?;

    // Prefer a row already consumed by this execution so a durable retry never
    // burns another approval. Otherwise lock exactly one valid approved row.
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
    .bind(&operation_digest)
    .bind(consumer_ref)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(storage_error)?;

    let Some(candidate) = candidate else {
        conn.commit().await.map_err(storage_error)?;
        tracing::warn!(
            tenant_id = %tenant_id,
            operation_type,
            "privileged operation refused: no distinct dual-control approval available (fail closed)"
        );
        return Err(DualControlError::NoValidApproval);
    };

    let request_id: Uuid = candidate.try_get("id").map_err(storage_error)?;
    let status: String = candidate.try_get("status").map_err(storage_error)?;
    if status == "consumed" {
        conn.commit().await.map_err(storage_error)?;
        tracing::info!(
            tenant_id = %tenant_id,
            operation_type,
            request_id = %request_id,
            "dual-control approval already consumed by this execution; admitting durable replay"
        );
        return Ok(());
    }

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
    .execute(conn.as_mut())
    .await
    .map_err(storage_error)?;
    if updated.rows_affected() != 1 {
        return Err(DualControlError::Storage(
            "locked dual-control approval changed before consumption".to_string(),
        ));
    }

    conn.commit().await.map_err(storage_error)?;
    tracing::info!(
        tenant_id = %tenant_id,
        operation_type,
        request_id = %request_id,
        "dual-control approval consumed for privileged operation"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_status_codes_fail_closed() {
        // Pins: dual-control refusals map to fail-closed HTTP statuses so a guarded
        // operation is denied (403) rather than silently succeeding, and SoD and
        // missing-approval both read as authorization refusals.
        for (error, expected) in [
            (DualControlError::NotFound, "[404]"),
            (DualControlError::AlreadyDecided, "[409]"),
            (DualControlError::SelfApproval, "[403]"),
            (DualControlError::NoValidApproval, "[403]"),
        ] {
            let handler_error = error.into_handler_error();
            let rendered = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(
                &handler_error,
            )
            .to_string();
            assert!(
                rendered.contains(expected),
                "expected status {expected} in {rendered}"
            );
        }
    }
}

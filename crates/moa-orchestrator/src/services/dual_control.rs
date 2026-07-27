//! Four-eyes dual control with segregation of duties for privileged operations.
//!
//! A privileged, irreversible operation can opt into dual control: it must be
//! requested by one tenant admin and then approved by a SECOND, DISTINCT tenant
//! admin before it may execute. This module owns the segregation-of-duties
//! (SoD) rule and drives the storage transaction; its private `repository`
//! submodule owns every statement and row mapping. The calling service owns
//! authorization and decides when the control applies.
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
//! substitute for authorization. The repository runs every write against the
//! RLS-protected table under a tenant-scoped `moa_app` transaction
//! ([`moa_db::ScopedConn`]).

use moa_core::types::identifiers::TenantId;
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::PgPool;
use uuid::Uuid;

use self::repository::{DualControlTx, RequestStatus};

mod repository;

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
    let mut tx = DualControlTx::begin(pool, tenant_id).await?;
    let id = tx
        .insert_pending_request(tenant_id, operation_type, &operation_digest, requested_by)
        .await?;
    tx.commit().await?;
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
    let mut tx = DualControlTx::begin(pool, tenant_id).await?;
    let Some(request) = tx.lock_request(tenant_id, request_id).await? else {
        return Err(DualControlError::NotFound);
    };
    if request.status != RequestStatus::Pending {
        return Err(DualControlError::AlreadyDecided);
    }
    // Segregation of duties: the operator that requested the privileged operation
    // may never be the one that authorizes it. This is the load-bearing SoD check;
    // the table's dual_control_sod_check constraint is a defense-in-depth backstop.
    if approver == request.requested_by {
        tracing::warn!(
            tenant_id = %tenant_id,
            request_id = %request_id,
            "dual-control approval rejected: segregation-of-duties violation (approver == requester)"
        );
        return Err(DualControlError::SelfApproval);
    }
    tx.mark_approved(tenant_id, request_id, approver).await?;
    tx.commit().await?;
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
    let mut tx = DualControlTx::begin(pool, tenant_id).await?;

    // Serialize consumption for this exact operation across every process and
    // Kubernetes replica.
    tx.lock_operation(&operation_digest).await?;

    // Prefer a row already consumed by this execution so a durable retry never
    // burns another approval. Otherwise lock exactly one valid approved row.
    let candidate = tx
        .lock_consumable_request(tenant_id, operation_type, &operation_digest, consumer_ref)
        .await?;

    let Some(candidate) = candidate else {
        tx.commit().await?;
        tracing::warn!(
            tenant_id = %tenant_id,
            operation_type,
            "privileged operation refused: no distinct dual-control approval available (fail closed)"
        );
        return Err(DualControlError::NoValidApproval);
    };

    let request_id = candidate.request_id;
    if candidate.status == RequestStatus::Consumed {
        tx.commit().await?;
        tracing::info!(
            tenant_id = %tenant_id,
            operation_type,
            request_id = %request_id,
            "dual-control approval already consumed by this execution; admitting durable replay"
        );
        return Ok(());
    }

    tx.mark_consumed(tenant_id, request_id, consumer_ref)
        .await?;

    tx.commit().await?;
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

//! Transactional outbox enqueue helpers for OpenFGA tuple operations.

use crate::error::AuthzError;
use moa_authz_schema::{MODEL_VERSION, TupleKey, TupleOp};
use sqlx::PgExecutor;
use uuid::Uuid;

/// Enqueue the desired state of a tuple into `authz_outbox`.
///
/// Callers should execute this inside the same Postgres transaction as the
/// state mutation that requires the tuple. Enqueue models the *latest desired
/// state* of the tuple identity `(user, relation, object, model_version)`: the
/// single row for that identity is upserted to `op`, its generation is bumped,
/// and it is reset to `pending`. Re-enqueuing the desired op that a row already
/// carries in a non-terminal state is a no-op, but any change of desired op —
/// or re-enqueuing a dead-lettered tuple — reactivates the row so the newest
/// intent is applied. This lets `write -> delete -> write` converge to `write`.
pub async fn enqueue<'executor, Executor>(
    exec: Executor,
    op: TupleOp,
    tuple: &TupleKey,
    tenant_id: Option<Uuid>,
) -> Result<(), AuthzError>
where
    Executor: PgExecutor<'executor>,
{
    upsert_desired_state(
        exec,
        op,
        &tuple.user_wire(),
        &tuple.relation.to_string(),
        &tuple.object_wire(),
        tenant_id,
    )
    .await
}

/// Enqueue the desired state of a tuple using OpenFGA wire strings directly.
///
/// This is used for parent-edge tuples whose subject is another object, such
/// as `tenant:<id> tenant session:<id>`, which cannot be represented by
/// the typed subject enum in [`TupleKey`]. It shares the desired-state upsert
/// semantics of [`enqueue`].
pub async fn enqueue_raw<'executor, Executor>(
    exec: Executor,
    op: TupleOp,
    user_wire: &str,
    relation: &str,
    object_wire: &str,
    tenant_id: Option<Uuid>,
) -> Result<(), AuthzError>
where
    Executor: PgExecutor<'executor>,
{
    upsert_desired_state(exec, op, user_wire, relation, object_wire, tenant_id).await
}

/// Upsert one tuple identity to its latest desired operation.
///
/// Shared by [`enqueue`] and [`enqueue_raw`], which differ only in how they
/// derive the wire strings. On conflict the row is reactivated (op set,
/// generation incremented, status reset to `pending`, retry state cleared) only
/// when the desired op differs from the stored op or the stored row is
/// dead-lettered. A same-op enqueue against a non-terminal row leaves the row —
/// including any in-flight lease — untouched.
async fn upsert_desired_state<'executor, Executor>(
    exec: Executor,
    op: TupleOp,
    user_wire: &str,
    relation: &str,
    object_wire: &str,
    tenant_id: Option<Uuid>,
) -> Result<(), AuthzError>
where
    Executor: PgExecutor<'executor>,
{
    sqlx::query(
        r#"
        INSERT INTO authz_outbox
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id,
             generation, status, attempts, next_attempt_at)
        VALUES ($1, $2, $3, $4, $5, $6, 1, 'pending', 0, NOW())
        ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE
        SET op = EXCLUDED.op,
            generation = authz_outbox.generation + 1,
            status = 'pending',
            attempts = 0,
            last_error = NULL,
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NOW(),
            updated_at = NOW()
        WHERE authz_outbox.op IS DISTINCT FROM EXCLUDED.op
           OR authz_outbox.status = 'dead_letter'
        "#,
    )
    .bind(op.to_string())
    .bind(user_wire)
    .bind(relation)
    .bind(object_wire)
    .bind(MODEL_VERSION as i32)
    .bind(tenant_id)
    .execute(exec)
    .await?;
    Ok(())
}

/// One claimed `authz_outbox` row projected for OpenFGA application.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct OutboxRow {
    pub id: Uuid,
    pub op: String,
    pub tuple_user: String,
    pub tuple_relation: String,
    pub tuple_object: String,
    pub attempts: i32,
    pub generation: i64,
}

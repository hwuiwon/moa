//! Transactional outbox enqueue helpers for OpenFGA tuple operations.

use crate::error::AuthzError;
use moa_authz_schema::{MODEL_VERSION, TupleKey, TupleOp};
use sqlx::PgExecutor;
use uuid::Uuid;

/// Enqueue a tuple operation into `authz_outbox`.
///
/// Callers should execute this inside the same Postgres transaction as the
/// state mutation that requires the tuple. Enqueue is idempotent: if the
/// deterministic key already exists, the existing row is left unchanged.
pub async fn enqueue<'executor, Executor>(
    exec: Executor,
    op: TupleOp,
    tuple: &TupleKey,
    tenant_id: Option<Uuid>,
) -> Result<(), AuthzError>
where
    Executor: PgExecutor<'executor>,
{
    let idempotency_key = tuple.idempotency_key(op, MODEL_VERSION);
    sqlx::query(
        r#"
        INSERT INTO authz_outbox
            (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (idempotency_key) DO NOTHING
        "#,
    )
    .bind(&idempotency_key)
    .bind(op.to_string())
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .bind(MODEL_VERSION as i32)
    .bind(tenant_id)
    .execute(exec)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct OutboxRow {
    pub id: Uuid,
    pub idempotency_key: String,
    pub op: String,
    pub tuple_user: String,
    pub tuple_relation: String,
    pub tuple_object: String,
    pub attempts: i32,
}

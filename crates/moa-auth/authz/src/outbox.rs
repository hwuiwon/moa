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
    insert_outbox(
        exec,
        op,
        &idempotency_key,
        &tuple.user_wire(),
        &tuple.relation.to_string(),
        &tuple.object_wire(),
        tenant_id,
    )
    .await
}

/// Enqueue a tuple operation using OpenFGA wire strings directly.
///
/// This is used for parent-edge tuples whose subject is another object, such
/// as `tenant:<id> tenant session:<id>`, which cannot be represented by
/// the typed subject enum in [`TupleKey`].
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
    let idempotency_key = format!("{op}-{object_wire}-{relation}-{user_wire}-v{MODEL_VERSION}");
    insert_outbox(
        exec,
        op,
        &idempotency_key,
        user_wire,
        relation,
        object_wire,
        tenant_id,
    )
    .await
}

/// Insert one `authz_outbox` row, leaving an existing row untouched on conflict.
///
/// Shared by [`enqueue`] and [`enqueue_raw`], which differ only in how they
/// derive the idempotency key and wire strings.
async fn insert_outbox<'executor, Executor>(
    exec: Executor,
    op: TupleOp,
    idempotency_key: &str,
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
            (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (idempotency_key) DO NOTHING
        "#,
    )
    .bind(idempotency_key)
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

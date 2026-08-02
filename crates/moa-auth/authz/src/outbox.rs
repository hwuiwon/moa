//! Transactional outbox enqueue helpers for OpenFGA tuple operations.

use crate::error::AuthzError;
use moa_authz_schema::{MODEL_VERSION, TupleKey, TupleOp};
use sqlx::PgExecutor;
use uuid::Uuid;

/// One desired OpenFGA tuple state at the private wire boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WireTupleIntent {
    op: TupleOp,
    user: String,
    relation: String,
    object: String,
}

/// Result of one keyset page of tenant-purge authorization inversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct TenantPurgeAuthzBatch {
    /// Number of tenant-attributed outbox identities inspected in the page.
    pub scanned: i32,
    /// Number of writes or dead-letter deletes reactivated as pending deletes.
    pub inverted: i32,
    /// Whether the page proved that no identity remains beyond the cursor.
    pub exhausted: bool,
    /// Final UUID in the page, or `None` for the exhausted page.
    pub next_cursor: Option<Uuid>,
}

/// Inverts one bounded page of the tenant's actual authorization tuple intents.
///
/// The database function validates the exact in-progress purge operation and
/// destruction fence, locks the progress row, and advances its UUID keyset
/// cursor atomically. The batch size is intentionally fixed at 1,000.
pub async fn invert_tenant_batch(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<TenantPurgeAuthzBatch, AuthzError> {
    sqlx::query_as(
        "SELECT scanned, inverted, exhausted, next_cursor \
         FROM moa.invert_tenant_authz_batch($1, $2)",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .map_err(AuthzError::from)
}

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
    let intent = wire_intent(op, tuple);
    upsert_desired_state_batch(exec, tenant_id, std::slice::from_ref(&intent)).await
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
    let intent = WireTupleIntent {
        op,
        user: user_wire.to_string(),
        relation: relation.to_string(),
        object: object_wire.to_string(),
    };
    upsert_desired_state_batch(exec, tenant_id, std::slice::from_ref(&intent)).await
}

/// Enqueues a tenant's desired typed tuple states in one set-based statement.
///
/// Duplicate identities are reduced in input order so the last intent is the
/// desired state. A same-op active row remains byte-for-byte unchanged; an
/// operation change or dead-letter redrive bumps its generation and resets its
/// delivery state. An identity already attributed to another tenant aborts the
/// transaction instead of silently changing or borrowing that attribution.
pub async fn enqueue_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    intents: &[(TupleOp, TupleKey)],
) -> Result<(), AuthzError> {
    let intents = intents
        .iter()
        .map(|(op, tuple)| wire_intent(*op, tuple))
        .collect::<Vec<_>>();
    upsert_desired_state_batch(&mut **tx, Some(tenant_id), &intents).await
}

fn wire_intent(op: TupleOp, tuple: &TupleKey) -> WireTupleIntent {
    WireTupleIntent {
        op,
        user: tuple.user_wire(),
        relation: tuple.relation.to_string(),
        object: tuple.object_wire(),
    }
}

/// Upserts tuple identities to their latest desired operations in one statement.
async fn upsert_desired_state_batch<'executor, Executor>(
    exec: Executor,
    tenant_id: Option<Uuid>,
    intents: &[WireTupleIntent],
) -> Result<(), AuthzError>
where
    Executor: PgExecutor<'executor>,
{
    if intents.is_empty() {
        return Ok(());
    }

    let operations = intents
        .iter()
        .map(|intent| intent.op.to_string())
        .collect::<Vec<_>>();
    let users = intents
        .iter()
        .map(|intent| intent.user.as_str())
        .collect::<Vec<_>>();
    let relations = intents
        .iter()
        .map(|intent| intent.relation.as_str())
        .collect::<Vec<_>>();
    let objects = intents
        .iter()
        .map(|intent| intent.object.as_str())
        .collect::<Vec<_>>();

    sqlx::query(
        r#"
        WITH raw_intents AS (
            SELECT input.op,
                   input.tuple_user,
                   input.tuple_relation,
                   input.tuple_object,
                   input.ordinality
            FROM UNNEST($1::TEXT[], $2::TEXT[], $3::TEXT[], $4::TEXT[])
                WITH ORDINALITY AS input(
                    op,
                    tuple_user,
                    tuple_relation,
                    tuple_object,
                    ordinality
                )
        ),
        normalized AS (
            SELECT DISTINCT ON (
                       tuple_user,
                       tuple_relation,
                       tuple_object
                   )
                   op,
                   tuple_user,
                   tuple_relation,
                   tuple_object
            FROM raw_intents
            ORDER BY tuple_user,
                     tuple_relation,
                     tuple_object,
                     ordinality DESC
        )
        INSERT INTO authz_outbox
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id,
             generation, status, attempts, next_attempt_at)
        SELECT op,
               tuple_user,
               tuple_relation,
               tuple_object,
               $5,
               $6,
               1,
               'pending',
               0,
               NOW()
        FROM normalized
        ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE
        SET op = EXCLUDED.op,
            tenant_id = EXCLUDED.tenant_id,
            generation = authz_outbox.generation + 1,
            status = 'pending',
            attempts = 0,
            last_error = NULL,
            lease_token = NULL,
            lease_expires_at = NULL,
            next_attempt_at = NOW(),
            updated_at = NOW()
        WHERE authz_outbox.tenant_id IS DISTINCT FROM EXCLUDED.tenant_id
           OR authz_outbox.op IS DISTINCT FROM EXCLUDED.op
           OR authz_outbox.status = 'dead_letter'
        "#,
    )
    .bind(&operations)
    .bind(&users)
    .bind(&relations)
    .bind(&objects)
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

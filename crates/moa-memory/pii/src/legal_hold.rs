//! Linearizable legal-hold and destructive-operation fencing.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// Result type returned by legal-hold and destruction-fence operations.
pub type Result<T> = std::result::Result<T, LegalHoldError>;

/// Errors returned by legal-hold and destruction-fence operations.
#[derive(Debug, thiserror::Error)]
pub enum LegalHoldError {
    /// Scoped transaction setup failed.
    #[error("legal-hold scope: {0}")]
    Scope(#[from] moa_core::error::MoaError),
    /// PostgreSQL operation failed.
    #[error("legal-hold sql: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// An active legal hold blocks destructive work.
    #[error("active legal hold blocks destruction")]
    ActiveHold,
    /// Destruction already began for the requested scope.
    #[error("destruction already began for this scope")]
    DestructionStarted,
    /// A different durable destruction operation owns the scope.
    #[error("destruction scope belongs to a different operation")]
    FenceConflict,
    /// A destructive stage could not find its durable fence.
    #[error("durable destruction fence is missing")]
    FenceMissing,
}

/// One legal-hold record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegalHold {
    /// Stable hold identifier.
    pub id: Uuid,
    /// Tenant the hold belongs to.
    pub tenant_id: TenantId,
    /// Subject under hold; `None` for a tenant-wide hold.
    pub subject_id: Option<Uuid>,
    /// Administrative reason recorded when the hold was placed.
    pub reason: String,
    /// Principal that placed the hold.
    pub placed_by: String,
    /// When the hold was placed.
    pub placed_at: DateTime<Utc>,
    /// When the hold was released; `None` while active.
    pub released_at: Option<DateTime<Utc>>,
}

/// Active-hold coverage for one tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TenantHoldSnapshot {
    tenant_wide: bool,
    held_subjects: BTreeSet<Uuid>,
}

impl TenantHoldSnapshot {
    /// Returns true when no hold is active for the tenant.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.tenant_wide && self.held_subjects.is_empty()
    }

    /// Returns true when an active hold covers `subject_id`.
    #[must_use]
    pub fn covers(&self, subject_id: Option<Uuid>) -> bool {
        self.tenant_wide || subject_id.is_some_and(|id| self.held_subjects.contains(&id))
    }
}

/// Transaction-scoped advisory guard held across one irreversible operation.
pub struct DestructionGuard<'a> {
    conn: ScopedConn<'a>,
}

impl DestructionGuard<'_> {
    /// Releases the advisory locks by committing the otherwise read-only guard transaction.
    pub async fn finish(self) -> Result<()> {
        self.conn.commit().await?;
        Ok(())
    }
}

/// Places a subject or tenant legal hold after serializing with destruction.
pub async fn place_hold(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_id: Option<Uuid>,
    reason: &str,
    placed_by: &str,
) -> Result<LegalHold> {
    let subjects = subject_id.into_iter().collect::<Vec<_>>();
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &subjects).await?;
    let destruction_started = if let Some(subject_id) = subject_id {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.destruction_operation_fence WHERE tenant_id = $1 AND (subject_id IS NULL OR subject_id = $2))",
        )
        .bind(tenant_id.0)
        .bind(subject_id)
        .fetch_one(conn.as_mut())
        .await?
    } else {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.destruction_operation_fence WHERE tenant_id = $1)",
        )
        .bind(tenant_id.0)
        .fetch_one(conn.as_mut())
        .await?
    };
    if destruction_started {
        return Err(LegalHoldError::DestructionStarted);
    }
    let row = sqlx::query(
        r#"
        INSERT INTO moa.legal_hold (tenant_id, subject_id, reason, placed_by)
        VALUES ($1, $2, $3, $4)
        RETURNING id, tenant_id, subject_id, reason, placed_by, placed_at, released_at
        "#,
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .bind(reason)
    .bind(placed_by)
    .fetch_one(conn.as_mut())
    .await?;
    let hold = hold_from_row(&row)?;
    conn.commit().await?;
    tracing::info!(tenant_id = %tenant_id, hold_id = %hold.id, hold_scope = if subject_id.is_some() { "subject" } else { "tenant" }, "legal hold placed");
    Ok(hold)
}

/// Releases an active legal hold after taking tenant then subject locks.
pub async fn release_hold(
    pool: &PgPool,
    tenant_id: TenantId,
    hold_id: Uuid,
    released_by: &str,
) -> Result<bool> {
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &[]).await?;
    let subject_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT subject_id FROM moa.legal_hold WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(tenant_id.0)
    .bind(hold_id)
    .fetch_optional(conn.as_mut())
    .await?
    .flatten();
    if let Some(subject_id) = subject_id {
        lock_subjects(conn.as_mut(), &[subject_id]).await?;
    }
    let released = sqlx::query(
        "UPDATE moa.legal_hold SET released_at = NOW(), released_by = $3 WHERE tenant_id = $1 AND id = $2 AND released_at IS NULL",
    )
    .bind(tenant_id.0)
    .bind(hold_id)
    .bind(released_by)
    .execute(conn.as_mut())
    .await?
    .rows_affected()
        == 1;
    conn.commit().await?;
    Ok(released)
}

/// Returns whether a subject or its tenant is under an active hold.
pub async fn active_hold_for(pool: &PgPool, tenant_id: TenantId, subject_id: Uuid) -> Result<bool> {
    let mut conn = begin_tenant(pool, tenant_id).await?;
    let held = active_hold_query(conn.as_mut(), tenant_id.0, &[subject_id], false).await?;
    conn.commit().await?;
    Ok(held)
}

/// Loads active hold coverage for one tenant.
pub async fn tenant_hold_snapshot(
    pool: &PgPool,
    tenant_id: TenantId,
) -> Result<TenantHoldSnapshot> {
    let mut conn = begin_tenant(pool, tenant_id).await?;
    let rows = sqlx::query(
        "SELECT subject_id FROM moa.legal_hold WHERE tenant_id = $1 AND released_at IS NULL",
    )
    .bind(tenant_id.0)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;
    let mut snapshot = TenantHoldSnapshot::default();
    for row in rows {
        match row.try_get::<Option<Uuid>, _>("subject_id")? {
            Some(subject) => {
                snapshot.held_subjects.insert(subject);
            }
            None => snapshot.tenant_wide = true,
        }
    }
    Ok(snapshot)
}

/// Starts a durable destructive operation before its first destructive commit.
pub async fn start_destruction(
    pool: &PgPool,
    tenant_id: TenantId,
    subjects: &[Uuid],
    operation_id: &str,
    operation_kind: &str,
) -> Result<()> {
    let subjects = sorted_subjects(subjects);
    let tenant_wide = subjects.is_empty();
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &subjects).await?;
    if active_hold_query(conn.as_mut(), tenant_id.0, &subjects, tenant_wide).await? {
        return Err(LegalHoldError::ActiveHold);
    }
    validate_existing_fences(
        conn.as_mut(),
        tenant_id.0,
        &subjects,
        operation_id,
        tenant_wide,
    )
    .await?;
    if tenant_wide {
        sqlx::query(
            "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, NULL, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(tenant_id.0)
        .bind(operation_id)
        .bind(operation_kind)
        .execute(conn.as_mut())
        .await?;
    } else {
        for subject in &subjects {
            sqlx::query(
                "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
            )
            .bind(tenant_id.0)
            .bind(subject)
            .bind(operation_id)
            .bind(operation_kind)
            .execute(conn.as_mut())
            .await?;
        }
    }
    validate_expected_fences(
        conn.as_mut(),
        tenant_id.0,
        &subjects,
        operation_id,
        tenant_wide,
    )
    .await?;
    conn.commit().await?;
    Ok(())
}

/// Acquires the ordered locks and revalidates an already-started destructive stage.
pub async fn begin_destruction_stage_guard<'a>(
    pool: &'a PgPool,
    tenant_id: TenantId,
    subjects: &[Uuid],
    operation_id: &str,
) -> Result<DestructionGuard<'a>> {
    let subjects = sorted_subjects(subjects);
    let tenant_wide = subjects.is_empty();
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &subjects).await?;
    if active_hold_query(conn.as_mut(), tenant_id.0, &subjects, tenant_wide).await? {
        return Err(LegalHoldError::ActiveHold);
    }
    validate_expected_fences(
        conn.as_mut(),
        tenant_id.0,
        &subjects,
        operation_id,
        tenant_wide,
    )
    .await?;
    Ok(DestructionGuard { conn })
}

/// Marks every fence belonging to an operation committed.
pub async fn complete_destruction(
    pool: &PgPool,
    tenant_id: TenantId,
    subjects: &[Uuid],
    operation_id: &str,
) -> Result<()> {
    let subjects = sorted_subjects(subjects);
    let tenant_wide = subjects.is_empty();
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &subjects).await?;
    validate_expected_fences(
        conn.as_mut(),
        tenant_id.0,
        &subjects,
        operation_id,
        tenant_wide,
    )
    .await?;
    sqlx::query(
        "UPDATE moa.destruction_operation_fence SET status = 'committed', committed_at = COALESCE(committed_at, NOW()) WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(())
}

/// Acquires a retention guard; returns `None` when a hold covers the candidate.
pub async fn begin_retention_guard<'a>(
    pool: &'a PgPool,
    tenant_id: TenantId,
    subject_id: Option<Uuid>,
) -> Result<Option<DestructionGuard<'a>>> {
    let subjects = subject_id.into_iter().collect::<Vec<_>>();
    let mut conn = begin_tenant(pool, tenant_id).await?;
    lock_tenant_and_subjects(conn.as_mut(), tenant_id.0, &subjects).await?;
    if active_hold_query(conn.as_mut(), tenant_id.0, &subjects, subject_id.is_none()).await? {
        return Ok(None);
    }
    Ok(Some(DestructionGuard { conn }))
}

/// Takes the canonical transaction advisory locks: tenant, then sorted subjects.
pub async fn lock_tenant_and_subjects(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    subjects: &[Uuid],
) -> Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
    )
    .bind(tenant_id)
    .execute(&mut *conn)
    .await?;
    lock_subjects(conn, subjects).await
}

async fn lock_subjects(conn: &mut PgConnection, subjects: &[Uuid]) -> Result<()> {
    for subject in sorted_subjects(subjects) {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('moa:destruction:subject:' || $1::text, 0))")
            .bind(subject)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn begin_tenant(pool: &PgPool, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
    Ok(ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true).await?)
}

async fn active_hold_query(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    subjects: &[Uuid],
    tenant_wide: bool,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM moa.legal_hold
            WHERE tenant_id = $1 AND released_at IS NULL
              AND ($3 OR subject_id IS NULL OR subject_id = ANY($2))
        )
        "#,
    )
    .bind(tenant_id)
    .bind(subjects)
    .bind(tenant_wide)
    .fetch_one(conn)
    .await?)
}

async fn validate_existing_fences(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    subjects: &[Uuid],
    operation_id: &str,
    tenant_wide: bool,
) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT subject_id, operation_id
        FROM moa.destruction_operation_fence
        WHERE tenant_id = $1
          AND (
              $3
              OR (NOT $3 AND (subject_id IS NULL OR subject_id = ANY($2)))
          )
        FOR UPDATE
        "#,
    )
    .bind(tenant_id)
    .bind(subjects)
    .bind(tenant_wide)
    .fetch_all(conn)
    .await?;
    if rows.iter().any(|row| {
        (tenant_wide && row.get::<Option<Uuid>, _>("subject_id").is_some())
            || row.get::<String, _>("operation_id") != operation_id
    }) {
        return Err(LegalHoldError::FenceConflict);
    }
    Ok(())
}

async fn validate_expected_fences(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    subjects: &[Uuid],
    operation_id: &str,
    tenant_wide: bool,
) -> Result<()> {
    let count: i64 = if tenant_wide {
        sqlx::query_scalar(
            "SELECT count(*) FROM moa.destruction_operation_fence WHERE tenant_id = $1 AND subject_id IS NULL AND operation_id = $2",
        )
        .bind(tenant_id)
        .bind(operation_id)
        .fetch_one(conn)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT count(*) FROM moa.destruction_operation_fence WHERE tenant_id = $1 AND subject_id = ANY($2) AND operation_id = $3",
        )
        .bind(tenant_id)
        .bind(subjects)
        .bind(operation_id)
        .fetch_one(conn)
        .await?
    };
    let expected = if tenant_wide {
        1
    } else {
        subjects.len() as i64
    };
    if count != expected {
        return Err(LegalHoldError::FenceMissing);
    }
    Ok(())
}

fn sorted_subjects(subjects: &[Uuid]) -> Vec<Uuid> {
    let mut subjects = subjects.to_vec();
    subjects.sort_unstable();
    subjects.dedup();
    subjects
}

fn hold_from_row(row: &sqlx::postgres::PgRow) -> Result<LegalHold> {
    Ok(LegalHold {
        id: row.try_get("id")?,
        tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
        subject_id: row.try_get("subject_id")?,
        reason: row.try_get("reason")?,
        placed_by: row.try_get("placed_by")?,
        placed_at: row.try_get("placed_at")?,
        released_at: row.try_get("released_at")?,
    })
}

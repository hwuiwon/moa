//! Postgres storage for builtin async-authorization challenges.

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// Terminal builtin challenge that still needs awakeable delivery.
#[derive(Debug, Clone)]
pub(crate) struct UnresolvedBuiltinChallenge {
    /// Challenge row id.
    pub(crate) id: Uuid,
    /// Restate awakeable id to resolve.
    pub(crate) awakeable_id: String,
    /// Persisted terminal status.
    pub(crate) status: String,
    /// Persisted denial reason, when denied.
    pub(crate) deny_reason: Option<String>,
    /// Resolution lease token held by the reaper that loaded this row.
    pub(crate) resolve_claim_token: Uuid,
}

/// Locked builtin challenge row used while deciding or retrying delivery.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct BuiltinChallengeDecisionRow {
    /// Challenge row id.
    pub(crate) id: Uuid,
    /// User allowed to decide the challenge.
    pub(crate) deciding_user_id: Uuid,
    /// Tenant that owns the security event.
    pub(crate) tenant_id: Uuid,
    /// Restate awakeable id to resolve.
    pub(crate) awakeable_id: String,
    /// Persisted challenge status.
    pub(crate) status: String,
    /// Persisted denial reason, when denied.
    pub(crate) deny_reason: Option<String>,
    /// Time when a pending decision expires.
    pub(crate) expires_at: DateTime<Utc>,
    /// Time when the terminal decision was delivered to Restate.
    pub(crate) resolved_at: Option<DateTime<Utc>>,
}

/// Decision update for one builtin challenge row.
pub(crate) struct BuiltinChallengeDecisionUpdate {
    /// Challenge row id.
    pub(crate) id: Uuid,
    /// New challenge status.
    pub(crate) status: &'static str,
    /// Optional denial reason.
    pub(crate) deny_reason: Option<String>,
    /// User that decided the challenge.
    pub(crate) decided_by_user_id: Uuid,
}

/// List pending builtin challenges visible to one deciding user.
pub(crate) async fn list_pending_builtin_challenges(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
) -> Result<Vec<BuiltinApprovalRow>, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, session_id, deciding_user_id, tenant_id, awakeable_id,
               action_summary, action_details, status, deny_reason,
               created_at, expires_at, decided_at, decided_by_user_id
        FROM builtin_pending_approvals
        WHERE deciding_user_id = $1 AND status = 'pending' AND expires_at > NOW()
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(deciding_user_id)
    .fetch_all(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("list authz challenges: {error}")).into())
}

/// Lock and load one builtin challenge for decision processing.
pub(crate) async fn load_builtin_challenge_for_update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<BuiltinChallengeDecisionRow, HandlerError> {
    let row = sqlx::query_as(
        r#"
        SELECT id, deciding_user_id, tenant_id, awakeable_id, status, deny_reason,
               expires_at, resolved_at
        FROM builtin_pending_approvals
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| TerminalError::new(format!("load authz challenge: {error}")))?;
    row.ok_or_else(|| TerminalError::new_with_code(404, "authz challenge not found").into())
}

/// Persist a builtin challenge decision and return the updated row.
pub(crate) async fn update_builtin_challenge_decision(
    tx: &mut Transaction<'_, Postgres>,
    update: BuiltinChallengeDecisionUpdate,
) -> Result<BuiltinChallengeDecisionRow, HandlerError> {
    sqlx::query_as(
        r#"
        UPDATE builtin_pending_approvals
        SET status = $2,
            deny_reason = $3,
            decided_at = NOW(),
            decided_by_user_id = $4
        WHERE id = $1
        RETURNING id, deciding_user_id, tenant_id, awakeable_id, status, deny_reason,
                  expires_at, resolved_at
        "#,
    )
    .bind(update.id)
    .bind(update.status)
    .bind(update.deny_reason.as_deref())
    .bind(update.decided_by_user_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| TerminalError::new(format!("update authz challenge: {error}")).into())
}

/// Result of one reaper sweep over builtin approvals.
pub(crate) struct BuiltinChallengeSweep {
    /// Terminal rows still awaiting awakeable delivery.
    pub(crate) unresolved: Vec<UnresolvedBuiltinChallenge>,
    /// IDs of rows this sweep newly transitioned from pending to timeout.
    pub(crate) timed_out: Vec<Uuid>,
}

/// Mark expired pending challenges and list every terminal row still awaiting delivery.
pub(crate) async fn unresolved_terminal_builtin_challenges(
    pool: &sqlx::PgPool,
) -> Result<BuiltinChallengeSweep, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let claim_token = Uuid::new_v4();
    let timed_out: Vec<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE builtin_pending_approvals
        SET status = 'timeout',
            decided_at = NOW()
        WHERE status = 'pending'
          AND expires_at <= NOW()
          AND resolved_at IS NULL
        RETURNING id
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;

    let unresolved: Vec<(Uuid, String, String, Option<String>, Uuid)> = sqlx::query_as(
        r#"
        WITH candidate AS (
            SELECT id
            FROM builtin_pending_approvals
            WHERE status IN ('approved', 'denied', 'timeout')
              AND resolved_at IS NULL
              AND (
                resolve_claim_expires_at IS NULL
                OR resolve_claim_expires_at <= NOW()
              )
            ORDER BY decided_at ASC NULLS LAST, expires_at ASC
            LIMIT 100
            FOR UPDATE SKIP LOCKED
        )
        UPDATE builtin_pending_approvals AS approval
        SET resolve_claim_token = $1,
            resolve_claim_expires_at = NOW() + INTERVAL '2 minutes'
        FROM candidate
        WHERE approval.id = candidate.id
        RETURNING approval.id, approval.awakeable_id, approval.status,
                  approval.deny_reason, approval.resolve_claim_token
        "#,
    )
    .bind(claim_token)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let unresolved = unresolved
        .into_iter()
        .map(
            |(id, awakeable_id, status, deny_reason, resolve_claim_token)| {
                UnresolvedBuiltinChallenge {
                    id,
                    awakeable_id,
                    status,
                    deny_reason,
                    resolve_claim_token,
                }
            },
        )
        .collect();
    Ok(BuiltinChallengeSweep {
        unresolved,
        timed_out,
    })
}

/// Pending builtin-approval queue snapshot for gauge emission.
pub(crate) struct BuiltinApprovalStats {
    /// Number of pending, unexpired builtin approvals.
    pub(crate) pending_depth: i64,
    /// Age in seconds of the oldest pending approval, or `0.0` when empty.
    pub(crate) oldest_pending_age_seconds: f64,
}

/// Sample the pending builtin-approval queue for gauge emission.
pub(crate) async fn builtin_approval_pending_stats(
    pool: &sqlx::PgPool,
) -> Result<BuiltinApprovalStats, sqlx::Error> {
    let row = sqlx::query(
        r#"
        SELECT COUNT(*) AS pending_depth,
               COALESCE(EXTRACT(EPOCH FROM (NOW() - MIN(created_at))), 0.0)::DOUBLE PRECISION AS oldest_age
        FROM builtin_pending_approvals
        WHERE status = 'pending'
          AND expires_at > NOW()
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(BuiltinApprovalStats {
        pending_depth: row.try_get("pending_depth")?,
        oldest_pending_age_seconds: row.try_get::<f64, _>("oldest_age")?.max(0.0),
    })
}

/// Mark one terminal challenge as delivered to Restate.
pub(crate) async fn mark_builtin_challenge_resolved(
    pool: &sqlx::PgPool,
    id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE builtin_pending_approvals
        SET resolved_at = COALESCE(resolved_at, NOW())
        WHERE id = $1
        "#,
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark one claimed terminal challenge as delivered to Restate.
pub(crate) async fn mark_claimed_builtin_challenge_resolved(
    pool: &sqlx::PgPool,
    id: Uuid,
    resolve_claim_token: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        UPDATE builtin_pending_approvals
        SET resolved_at = COALESCE(resolved_at, NOW()),
            resolve_claim_token = NULL,
            resolve_claim_expires_at = NULL
        WHERE id = $1
          AND resolve_claim_token = $2
        "#,
    )
    .bind(id)
    .bind(resolve_claim_token)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Release a terminal challenge resolution claim after a transient delivery failure.
pub(crate) async fn release_builtin_challenge_resolution_claim(
    pool: &sqlx::PgPool,
    id: Uuid,
    resolve_claim_token: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE builtin_pending_approvals
        SET resolve_claim_token = NULL,
            resolve_claim_expires_at = NULL
        WHERE id = $1
          AND resolve_claim_token = $2
        "#,
    )
    .bind(id)
    .bind(resolve_claim_token)
    .execute(pool)
    .await?;
    Ok(())
}

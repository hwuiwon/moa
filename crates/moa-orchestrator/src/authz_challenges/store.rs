//! Postgres storage for builtin async-authorization challenges.

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::{Postgres, Transaction};
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

/// Mark expired pending challenges and list every terminal row still awaiting delivery.
pub(crate) async fn unresolved_terminal_builtin_challenges(
    pool: &sqlx::PgPool,
) -> Result<Vec<UnresolvedBuiltinChallenge>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"
        UPDATE builtin_pending_approvals
        SET status = 'timeout',
            decided_at = NOW()
        WHERE status = 'pending'
          AND expires_at <= NOW()
          AND resolved_at IS NULL
        "#,
    )
    .execute(&mut *tx)
    .await?;

    let unresolved: Vec<(Uuid, String, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT id, awakeable_id, status, deny_reason
        FROM builtin_pending_approvals
        WHERE status IN ('approved', 'denied', 'timeout')
          AND resolved_at IS NULL
        ORDER BY decided_at ASC NULLS LAST, expires_at ASC
        LIMIT 100
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(unresolved
        .into_iter()
        .map(
            |(id, awakeable_id, status, deny_reason)| UnresolvedBuiltinChallenge {
                id,
                awakeable_id,
                status,
                deny_reason,
            },
        )
        .collect())
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

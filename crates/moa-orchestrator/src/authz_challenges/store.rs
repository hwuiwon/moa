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

/// Mark expired pending challenges and list every terminal row still awaiting delivery.
pub(crate) async fn unresolved_terminal_builtin_challenges(
    pool: &sqlx::PgPool,
) -> Result<Vec<UnresolvedBuiltinChallenge>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let claim_token = Uuid::new_v4();
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

    Ok(unresolved
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use std::time::Duration;

    #[tokio::test]
    async fn unresolved_terminal_challenges_are_claimed_once() {
        // Pins: competing reapers cannot both claim the same terminal awakeable delivery.
        let pool = test_pool().await;
        let challenge_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO builtin_pending_approvals
                (id, session_id, deciding_user_id, tenant_id, awakeable_id,
                 action_summary, action_details, status, expires_at, decided_at)
            VALUES
                ($1, $2, $3, $4, 'awakeable-once', 'approve deploy', '{}'::jsonb,
                 'approved', NOW() - INTERVAL '1 minute', NOW() - INTERVAL '1 minute')
            "#,
        )
        .bind(challenge_id)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .expect("terminal challenge should insert");

        let (first, second) = tokio::join!(
            unresolved_terminal_builtin_challenges(&pool),
            unresolved_terminal_builtin_challenges(&pool)
        );
        let first = first.expect("first claim should succeed");
        let second = second.expect("second claim should succeed");
        let total_claimed = first.len() + second.len();

        assert_eq!(
            total_claimed, 1,
            "terminal challenge should have one claimant"
        );
        let challenge = first
            .first()
            .or_else(|| second.first())
            .expect("one reaper should claim the challenge");
        assert_eq!(challenge.id, challenge_id);
        assert!(
            !mark_claimed_builtin_challenge_resolved(&pool, challenge_id, Uuid::new_v4())
                .await
                .expect("wrong-token mark should query"),
            "wrong resolution claim token must not complete the row"
        );
        assert!(
            mark_claimed_builtin_challenge_resolved(
                &pool,
                challenge_id,
                challenge.resolve_claim_token
            )
            .await
            .expect("right-token mark should query"),
            "owning resolution claim token should complete the row"
        );
    }

    async fn test_pool() -> PgPool {
        let database_url = std::env::var("MOA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
        let schema_name = format!("authz_challenge_store_test_{}", Uuid::new_v4().simple());
        let search_path = format!("{}, public", quote_identifier(&schema_name));
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _meta| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("test Postgres should be reachable");
        moa_migrations::run_auth_schema(&pool, &schema_name)
            .await
            .expect("auth schema should apply");
        pool
    }

    fn quote_identifier(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }
}

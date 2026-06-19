//! Application rules for builtin async-authorization challenges.

use chrono::Utc;
use moa_core::traits::ApprovalDecision as AsyncApprovalDecision;
use moa_ocsf::ActorInput;
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::authz_challenges::store::{self, BuiltinChallengeDecisionUpdate};
use crate::services::authz_challenges::AuthzChallengeDecisionRequest;

/// Resolved builtin challenge side effect for the service adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResolvedAuthzChallenge {
    /// Challenge row id.
    pub(crate) id: Uuid,
    /// Restate awakeable id to resolve.
    pub(crate) awakeable_id: String,
    /// Decision payload to resolve with.
    pub(crate) decision: AsyncApprovalDecision,
}

/// Decide a builtin async-authorization challenge.
pub(crate) async fn decide_builtin_challenge(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
    request: AuthzChallengeDecisionRequest,
) -> Result<ResolvedAuthzChallenge, HandlerError> {
    let (status, decision) = decision_from_outcome(&request.outcome, request.reason.clone())?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row = store::load_builtin_challenge_for_update(&mut transaction, request.id).await?;
    validate_deciding_user(&row, deciding_user_id)?;

    let resolved = if row.status == "pending" {
        validate_pending_challenge(&row)?;

        let updated = store::update_builtin_challenge_decision(
            &mut transaction,
            BuiltinChallengeDecisionUpdate {
                id: request.id,
                status,
                deny_reason: match &decision {
                    AsyncApprovalDecision::Denied { reason } => reason.clone(),
                    AsyncApprovalDecision::Approved | AsyncApprovalDecision::Timeout => None,
                },
                decided_by_user_id: deciding_user_id,
            },
        )
        .await?;

        moa_ocsf::emit_approval_decided_tx(
            &mut transaction,
            updated.tenant_id,
            ActorInput::user(deciding_user_id),
            updated.id,
            updated.status == "approved",
        )
        .await
        .map_err(|error| TerminalError::new(format!("audit authz challenge decision: {error}")))?;

        ResolvedAuthzChallenge {
            id: updated.id,
            awakeable_id: updated.awakeable_id,
            decision,
        }
    } else {
        validate_terminal_retry(&row, status)?;
        let decision = terminal_decision_from_row(&row)?;
        ResolvedAuthzChallenge {
            id: row.id,
            awakeable_id: row.awakeable_id,
            decision,
        }
    };

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(resolved)
}

fn decision_from_outcome(
    outcome: &str,
    reason: Option<String>,
) -> Result<(&'static str, AsyncApprovalDecision), TerminalError> {
    match outcome {
        "approved" => Ok(("approved", AsyncApprovalDecision::Approved)),
        "denied" => Ok(("denied", AsyncApprovalDecision::Denied { reason })),
        other => Err(TerminalError::new_with_code(
            400,
            format!("bad outcome: {other}"),
        )),
    }
}

fn validate_deciding_user(
    row: &store::BuiltinChallengeDecisionRow,
    deciding_user_id: Uuid,
) -> Result<(), HandlerError> {
    if row.deciding_user_id != deciding_user_id {
        return Err(TerminalError::new_with_code(403, "not your authz challenge").into());
    }
    Ok(())
}

fn validate_pending_challenge(
    row: &store::BuiltinChallengeDecisionRow,
) -> Result<(), HandlerError> {
    if row.expires_at <= Utc::now() {
        return Err(TerminalError::new_with_code(410, "authz challenge expired").into());
    }
    Ok(())
}

fn validate_terminal_retry(
    row: &store::BuiltinChallengeDecisionRow,
    requested_status: &str,
) -> Result<(), HandlerError> {
    if row.status == "pending" {
        return Ok(());
    }
    if row.resolved_at.is_some() {
        return Err(TerminalError::new_with_code(
            409,
            format!("authz challenge already {}", row.status),
        )
        .into());
    }
    if row.status != requested_status {
        return Err(TerminalError::new_with_code(
            409,
            format!("authz challenge already {}", row.status),
        )
        .into());
    }
    Ok(())
}

fn terminal_decision_from_row(
    row: &store::BuiltinChallengeDecisionRow,
) -> Result<AsyncApprovalDecision, TerminalError> {
    match row.status.as_str() {
        "approved" => Ok(AsyncApprovalDecision::Approved),
        "denied" => Ok(AsyncApprovalDecision::Denied {
            reason: row.deny_reason.clone(),
        }),
        "timeout" => Ok(AsyncApprovalDecision::Timeout),
        other => Err(TerminalError::new(format!(
            "bad terminal authz challenge status: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use moa_core::traits::ApprovalDecision as AsyncApprovalDecision;

    use super::*;

    #[test]
    fn decision_from_outcome_maps_denial_reason() {
        // Pins: builtin async-authz denial preserves the user-supplied reason.
        let (status, decision) = decision_from_outcome("denied", Some("not allowed".to_string()))
            .expect("denied outcome should translate");

        assert_eq!(status, "denied");
        assert_eq!(
            decision,
            AsyncApprovalDecision::Denied {
                reason: Some("not allowed".to_string())
            }
        );
    }

    #[test]
    fn decision_from_outcome_rejects_unknown_value() {
        // Pins: builtin async-authz rejects unknown decision outcomes before writing the row.
        let error = decision_from_outcome("cleared", None)
            .expect_err("workspace action-review language is not valid async-authz language");

        assert!(
            error.to_string().contains("bad outcome: cleared"),
            "error should report the invalid builtin authz outcome: {error}"
        );
    }

    #[test]
    fn terminal_retry_reuses_unresolved_stored_decision() {
        // Pins: retry after DB commit but before awakeable resolution can deliver the stored decision.
        let row = challenge_row("denied", None);

        validate_terminal_retry(&row, "denied").expect("unresolved matching terminal row retries");
        assert_eq!(
            terminal_decision_from_row(&row).expect("stored denial should map"),
            AsyncApprovalDecision::Denied {
                reason: Some("stored reason".to_string())
            }
        );
    }

    #[test]
    fn terminal_retry_rejects_resolved_row() {
        // Pins: once awakeable delivery is marked resolved, duplicate user decisions stay conflicts.
        let row = challenge_row("approved", Some(Utc::now()));

        let error = validate_terminal_retry(&row, "approved")
            .expect_err("resolved terminal row should not retry");
        assert!(
            format!("{error:?}").contains("authz challenge already approved"),
            "resolved duplicate should report the terminal status: {error:?}"
        );
    }

    fn challenge_row(
        status: &str,
        resolved_at: Option<chrono::DateTime<Utc>>,
    ) -> store::BuiltinChallengeDecisionRow {
        store::BuiltinChallengeDecisionRow {
            id: Uuid::now_v7(),
            deciding_user_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            awakeable_id: "awakeable".to_string(),
            status: status.to_string(),
            deny_reason: Some("stored reason".to_string()),
            expires_at: Utc::now() + Duration::minutes(5),
            resolved_at,
        }
    }
}

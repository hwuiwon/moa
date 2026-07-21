//! Application rules for tenant action reviews.

use chrono::{DateTime, Utc};
use moa_core::{
    events::Event, types::action_policy::ActionClass, types::action_policy::ActionReviewDecision,
    types::action_policy::ActionReviewStatus, types::action_policy::ExecutionTaskOrigin,
    types::identifiers::StoragePartitionId, types::identifiers::ToolCallId,
    types::tools::ToolCallRequest,
};
use moa_observability::propagation::ValidatedTraceContext;
use moa_security::{ToolInputCanaryScreening, screen_tool_input_for_canary};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::action_reviews::store::{self, ReviewDecisionRow, ReviewDecisionUpdate};
use crate::services::action_reviews::{
    ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest, RequestActionReview,
};

/// Result of requesting a tenant action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RequestedReview {
    /// Stored review DTO rendered by the service.
    pub(crate) summary: ActionReviewSummary,
    /// Whether the service should append the requested event.
    pub(crate) record_requested_event: bool,
    /// Whether this request inserted the review row.
    pub(crate) newly_inserted: bool,
}

/// Result of deciding a tenant action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DecidedReview {
    /// Review identifier.
    pub(crate) review_id: Uuid,
    /// Storage partition that owns the review.
    pub(crate) storage_partition_id: StoragePartitionId,
    /// Owning session, when present.
    pub(crate) session_id: Option<moa_core::types::identifiers::SessionId>,
    /// Admin decision.
    pub(crate) decision: ActionReviewDecision,
    /// Terminal review status.
    pub(crate) status: ActionReviewStatus,
    /// Action class for decision metrics.
    pub(crate) action_class: ActionClass,
    /// User that decided the review.
    pub(crate) decided_by: String,
    /// Timestamp the review was created, used for approval-wait metrics.
    pub(crate) created_at: DateTime<Utc>,
    /// Decision timestamp.
    pub(crate) decided_at: DateTime<Utc>,
    /// Whether the service should append the decision event.
    pub(crate) record_decision_event: bool,
    /// Whether this call newly moved the review out of pending.
    pub(crate) newly_decided: bool,
    /// Stored tool request to invoke after a clear decision.
    pub(crate) tool_request: Option<ToolCallRequest>,
    /// Execution task that owns a cleared dispatch, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) execution_origin: Option<ExecutionTaskOrigin>,
}

/// Screen and normalize a request before it is persisted.
pub(crate) fn prepare_request(request: &mut RequestActionReview) -> Result<(), HandlerError> {
    screen_review_tool_input(&request.tool_request)?;
    request.tool_request.active_canary = None;
    Ok(())
}

/// Create the durable requested event for a review request.
pub(crate) fn requested_event(request: &RequestActionReview) -> Event {
    Event::ActionReviewRequested {
        review_id: request.envelope.review_id,
        envelope: request.envelope.clone(),
        preview: request.preview.clone(),
    }
}

/// Insert or load one tenant action review.
pub(crate) async fn request_review(
    pool: sqlx::PgPool,
    request: RequestActionReview,
    review_timeout_secs: i64,
    execution_task_trace_context: Option<ValidatedTraceContext>,
) -> Result<RequestedReview, HandlerError> {
    let stored = store::insert_review(
        pool,
        request,
        review_timeout_secs,
        execution_task_trace_context,
    )
    .await?;
    Ok(RequestedReview {
        record_requested_event: stored.requested_event_recorded_at.is_none(),
        newly_inserted: stored.newly_inserted,
        summary: stored.summary,
    })
}

/// List pending reviews for one tenant storage partition.
pub(crate) async fn list_pending_reviews(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
) -> Result<Vec<ActionReviewSummary>, HandlerError> {
    store::list_pending_reviews(pool, storage_partition_id).await
}

/// Apply a tenant-admin decision to one action review.
pub(crate) async fn decide_review(
    pool: sqlx::PgPool,
    request: DecideActionReviewRequest,
    decided_by: String,
    resolution_trace_context: Option<ValidatedTraceContext>,
) -> Result<DecidedReview, HandlerError> {
    let requested_decider = decided_by;
    let decision = decision_from_request(&request);
    let desired_status = status_for_decision(&decision);
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let storage_partition_id = storage_partition_id(request.tenant_id);
    let row =
        store::load_review_for_update(&mut tx, &storage_partition_id, request.review_id).await?;
    if row.status == ActionReviewStatus::Pending && row.execution_requested_at.is_some() {
        let exact_claim_replay = matches!(decision, ActionReviewDecision::Cleared)
            && row.decided_by.as_deref() == Some(requested_decider.as_str());
        if !exact_claim_replay {
            return Err(TerminalError::new_with_code(
                409,
                "action review cleared execution is already claimed by another decision",
            )
            .into());
        }
    }
    let newly_decided = validate_review_transition(row.status, desired_status)?;
    let decided_at = row.decided_at.unwrap_or_else(Utc::now);
    let decided_by = row.decided_by.clone().unwrap_or(requested_decider);
    let deny_reason = deny_reason_for_decision(&decision, row.deny_reason.clone());
    let execution_tool_call_id = execution_tool_call_id_for_decision(&decision, &row);

    if matches!(decision, ActionReviewDecision::Cleared)
        && row.execution_origin.is_some()
        && row.status == ActionReviewStatus::Pending
    {
        let execution_tool_call_id = execution_tool_call_id.ok_or_else(|| {
            TerminalError::new("cleared execution review has no execution tool call id")
        })?;
        let replaying_claim = row.execution_requested_at.is_some();
        if !replaying_claim {
            store::claim_execution_review(
                &mut tx,
                &storage_partition_id,
                request.review_id,
                &decided_by,
                decided_at,
                execution_tool_call_id,
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
        let tool_request = if replaying_claim {
            Some(execution_tool_request(&row, execution_tool_call_id))
        } else {
            execution_tool_request_for_decision(&decision, &row, Some(execution_tool_call_id))?
        };
        return Ok(DecidedReview {
            review_id: request.review_id,
            storage_partition_id,
            session_id: row.session_id,
            decision,
            status: ActionReviewStatus::Pending,
            action_class: row.action_class,
            decided_by,
            created_at: row.created_at,
            decided_at,
            record_decision_event: false,
            newly_decided: false,
            tool_request,
            execution_origin: row.execution_origin,
        });
    }

    if newly_decided || row.execution_tool_call_id != execution_tool_call_id {
        store::update_review_decision(
            &mut tx,
            ReviewDecisionUpdate {
                storage_partition_id: storage_partition_id.clone(),
                review_id: request.review_id,
                status: desired_status,
                decided_by: decided_by.clone(),
                deny_reason,
                decided_at,
                execution_tool_call_id,
            },
        )
        .await?;
        if let Some(origin) = row.execution_origin
            && matches!(decision, ActionReviewDecision::Denied { .. })
        {
            store::insert_execution_review_resolution(
                &mut tx,
                request.tenant_id,
                request.review_id,
                origin,
                &moa_execution::wire::ExecutionActionReviewResolution::Denied {
                    reason: deny_reason_for_resolution(&decision),
                },
                resolution_trace_context.as_ref(),
                row.execution_task_trace_context.as_ref(),
            )
            .await?;
        }
    }
    tx.commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    let tool_request =
        execution_tool_request_for_decision(&decision, &row, execution_tool_call_id)?;
    Ok(DecidedReview {
        review_id: request.review_id,
        storage_partition_id,
        session_id: row.session_id,
        decision,
        status: desired_status,
        action_class: row.action_class,
        decided_by,
        created_at: row.created_at,
        decided_at,
        record_decision_event: row.decision_event_recorded_at.is_none(),
        newly_decided,
        tool_request,
        execution_origin: row.execution_origin,
    })
}

/// Atomically persists a cleared execution review and its typed delivery outbox row.
pub(crate) async fn finalize_execution_review(
    pool: sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    review_id: Uuid,
    resolution: moa_execution::wire::ExecutionActionReviewResolution,
    resolution_trace_context: Option<ValidatedTraceContext>,
) -> Result<DecidedReview, HandlerError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let storage_partition_id = storage_partition_id(tenant_id);
    let row = store::load_review_for_update(&mut tx, &storage_partition_id, review_id).await?;
    let origin = row.execution_origin.ok_or_else(|| {
        TerminalError::new("cleared execution finalization requires execution origin")
    })?;
    if row.status != ActionReviewStatus::Pending && row.status != ActionReviewStatus::Cleared {
        return Err(TerminalError::new_with_code(
            409,
            format!("action review already {}", row.status.as_str()),
        )
        .into());
    }
    if row.execution_requested_at.is_none() || row.execution_tool_call_id.is_none() {
        return Err(TerminalError::new_with_code(
            409,
            "execution review was not claimed before finalization",
        )
        .into());
    }
    let newly_decided = row.status == ActionReviewStatus::Pending;
    let decided_at = row.decided_at.unwrap_or_else(Utc::now);
    let decided_by = row
        .decided_by
        .clone()
        .ok_or_else(|| TerminalError::new("claimed execution review has no deciding user"))?;
    if newly_decided {
        store::update_review_decision(
            &mut tx,
            ReviewDecisionUpdate {
                storage_partition_id: storage_partition_id.clone(),
                review_id,
                status: ActionReviewStatus::Cleared,
                decided_by: decided_by.clone(),
                deny_reason: None,
                decided_at,
                execution_tool_call_id: row.execution_tool_call_id,
            },
        )
        .await?;
    }
    store::insert_execution_review_resolution(
        &mut tx,
        tenant_id,
        review_id,
        origin,
        &resolution,
        resolution_trace_context.as_ref(),
        row.execution_task_trace_context.as_ref(),
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(DecidedReview {
        review_id,
        storage_partition_id,
        session_id: row.session_id,
        decision: ActionReviewDecision::Cleared,
        status: ActionReviewStatus::Cleared,
        action_class: row.action_class,
        decided_by,
        created_at: row.created_at,
        decided_at,
        record_decision_event: row.decision_event_recorded_at.is_none(),
        newly_decided,
        tool_request: None,
        execution_origin: Some(origin),
    })
}

fn storage_partition_id(tenant_id: moa_core::types::identifiers::TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

/// Mark the requested event as recorded.
pub(crate) async fn mark_requested_event_recorded(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    store::mark_requested_event_recorded(pool, storage_partition_id, review_id).await
}

/// Mark the decision event as recorded.
pub(crate) async fn mark_decision_event_recorded(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    store::mark_decision_event_recorded(pool, storage_partition_id, review_id).await
}

/// Mark cleared execution as requested.
pub(crate) async fn mark_execution_requested(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    store::mark_execution_requested(pool, storage_partition_id, review_id).await
}

fn decision_from_request(request: &DecideActionReviewRequest) -> ActionReviewDecision {
    match request.decision {
        ActionReviewDecisionKind::Cleared => ActionReviewDecision::Cleared,
        ActionReviewDecisionKind::Denied => ActionReviewDecision::Denied {
            reason: request.reason.clone(),
        },
    }
}

fn status_for_decision(decision: &ActionReviewDecision) -> ActionReviewStatus {
    match decision {
        ActionReviewDecision::Cleared => ActionReviewStatus::Cleared,
        ActionReviewDecision::Denied { .. } => ActionReviewStatus::Denied,
    }
}

fn validate_review_transition(
    stored_status: ActionReviewStatus,
    desired_status: ActionReviewStatus,
) -> Result<bool, TerminalError> {
    if stored_status != ActionReviewStatus::Pending && stored_status != desired_status {
        return Err(TerminalError::new_with_code(
            409,
            format!("action review already {}", stored_status.as_str()),
        ));
    }
    Ok(stored_status == ActionReviewStatus::Pending)
}

fn deny_reason_for_decision(
    decision: &ActionReviewDecision,
    existing_deny_reason: Option<String>,
) -> Option<String> {
    match decision {
        ActionReviewDecision::Denied { reason } => existing_deny_reason.or_else(|| reason.clone()),
        ActionReviewDecision::Cleared => None,
    }
}

fn deny_reason_for_resolution(decision: &ActionReviewDecision) -> String {
    match decision {
        ActionReviewDecision::Denied { reason } => reason
            .clone()
            .unwrap_or_else(|| "action denied by tenant administrator".to_string()),
        ActionReviewDecision::Cleared => "action was not denied".to_string(),
    }
}

fn execution_tool_call_id_for_decision(
    decision: &ActionReviewDecision,
    row: &ReviewDecisionRow,
) -> Option<Uuid> {
    if matches!(decision, ActionReviewDecision::Cleared) {
        Some(row.execution_tool_call_id.unwrap_or_else(Uuid::now_v7))
    } else {
        None
    }
}

fn execution_tool_request_for_decision(
    decision: &ActionReviewDecision,
    row: &ReviewDecisionRow,
    execution_tool_call_id: Option<Uuid>,
) -> Result<Option<ToolCallRequest>, TerminalError> {
    if !matches!(decision, ActionReviewDecision::Cleared) || row.execution_requested_at.is_some() {
        return Ok(None);
    }

    let execution_tool_call_id = execution_tool_call_id.ok_or_else(|| {
        TerminalError::new("cleared action review did not have an execution tool id")
    })?;
    Ok(Some(execution_tool_request(row, execution_tool_call_id)))
}

fn execution_tool_request(
    row: &ReviewDecisionRow,
    execution_tool_call_id: Uuid,
) -> ToolCallRequest {
    let mut tool_request = row.tool_request.clone();
    tool_request.tool_call_id = ToolCallId(execution_tool_call_id);
    tool_request.active_canary = None;
    tool_request
}

fn screen_review_tool_input(request: &ToolCallRequest) -> Result<(), TerminalError> {
    let serialized_input = serde_json::to_string(&request.input)
        .map_err(|error| TerminalError::new(format!("serialize tool input: {error}")))?;
    if matches!(
        screen_tool_input_for_canary(request.active_canary.as_deref(), &serialized_input),
        ToolInputCanaryScreening::Blocked(_)
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "tool {} blocked because it leaked a protected canary token",
                request.tool_name
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        traits::{Identity, IdentityType},
        types::action_policy::ActionClass,
        types::action_policy::ActionReviewDecision,
        types::action_policy::ActionReviewStatus,
        types::identifiers::SessionId,
        types::identifiers::TenantId,
        types::identifiers::ToolCallId,
        types::tools::ToolCallRequest,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn validate_review_transition_rejects_cross_terminal_decision() {
        // Pins: a cleared tenant action review cannot later be denied.
        let error =
            validate_review_transition(ActionReviewStatus::Cleared, ActionReviewStatus::Denied)
                .expect_err("cleared review should reject a later deny decision");

        assert!(
            error.to_string().contains("action review already cleared"),
            "error should name the existing terminal status: {error}"
        );
    }

    #[test]
    fn validate_review_transition_rejects_clear_after_timeout() {
        // Pins: a review the reaper failed closed (timeout) cannot later be
        // cleared, so a timed-out action never executes its gated tool.
        let error =
            validate_review_transition(ActionReviewStatus::Timeout, ActionReviewStatus::Cleared)
                .expect_err("timed-out review must reject a later clear decision");

        assert!(
            error.to_string().contains("action review already timeout"),
            "error should name the terminal timeout status: {error}"
        );
    }

    #[test]
    fn execution_tool_request_refreshes_internal_id_and_preserves_provider_identity() {
        // Pins: clearing a tenant action review refreshes MOA's internal execution id while
        // preserving the provider invocation id that must reach the eventual MCP request.
        let original_tool_id = ToolCallId::new();
        let execution_tool_id = Uuid::now_v7();
        let row = ReviewDecisionRow {
            session_id: None,
            action_class: ActionClass::CommandExecution,
            created_at: Utc::now(),
            status: ActionReviewStatus::Pending,
            tool_request: ToolCallRequest {
                tool_call_id: original_tool_id,
                caller_identity: test_identity(),
                provider_tool_use_id: Some("provider-tool-use".to_string()),
                tool_name: "bash".to_string(),
                input: json!({"cmd": "printf ok"}),
                active_canary: Some("canary-token".to_string()),
                session_id: SessionId::new(),
                trusted_sandbox_manifest: None,
                worker_id: None,
            },
            execution_origin: None,
            execution_task_trace_context: None,
            decided_by: None,
            deny_reason: None,
            decided_at: Some(Utc::now()),
            decision_event_recorded_at: None,
            execution_tool_call_id: None,
            execution_requested_at: None,
        };

        let tool_request = execution_tool_request_for_decision(
            &ActionReviewDecision::Cleared,
            &row,
            Some(execution_tool_id),
        )
        .expect("clear execution request should build")
        .expect("clear decision should request execution");

        assert_eq!(tool_request.tool_call_id, ToolCallId(execution_tool_id));
        assert_ne!(tool_request.tool_call_id, original_tool_id);
        assert_eq!(
            tool_request.provider_tool_use_id.as_deref(),
            Some("provider-tool-use")
        );
        assert_eq!(tool_request.active_canary, None);
        assert_eq!(tool_request.tool_name, "bash");
        assert_eq!(tool_request.input, json!({"cmd": "printf ok"}));
    }

    #[test]
    fn execution_tool_request_is_idempotent_after_execution_was_requested() {
        // Pins: retrying a cleared review after execution was requested does not invoke the tool again.
        let row = ReviewDecisionRow {
            session_id: None,
            action_class: ActionClass::CommandExecution,
            created_at: Utc::now(),
            status: ActionReviewStatus::Cleared,
            tool_request: ToolCallRequest {
                tool_call_id: ToolCallId::new(),
                caller_identity: test_identity(),
                provider_tool_use_id: None,
                tool_name: "bash".to_string(),
                input: json!({}),
                active_canary: None,
                session_id: SessionId::new(),
                trusted_sandbox_manifest: None,
                worker_id: None,
            },
            execution_origin: None,
            execution_task_trace_context: None,
            decided_by: Some("admin".to_string()),
            deny_reason: None,
            decided_at: Some(Utc::now()),
            decision_event_recorded_at: Some(Utc::now()),
            execution_tool_call_id: Some(Uuid::now_v7()),
            execution_requested_at: Some(Utc::now()),
        };

        let tool_request = execution_tool_request_for_decision(
            &ActionReviewDecision::Cleared,
            &row,
            row.execution_tool_call_id,
        )
        .expect("idempotent clear retry should not fail");

        assert_eq!(tool_request, None);
    }

    #[test]
    fn prepare_request_rejects_canary_leak() {
        // Pins: tenant action review storage rejects tool input that leaks the active canary.
        let request = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            caller_identity: test_identity(),
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            input: json!({"cmd": "printf secret-canary"}),
            active_canary: Some("secret-canary".to_string()),
            session_id: SessionId::new(),
            trusted_sandbox_manifest: None,
            worker_id: None,
        };

        let error =
            screen_review_tool_input(&request).expect_err("canary leak should block persistence");

        assert!(
            error
                .to_string()
                .contains("blocked because it leaked a protected canary token"),
            "error should explain the canary screening failure: {error}"
        );
    }

    fn test_identity() -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(2),
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }
}

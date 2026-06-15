//! Restate service for builtin human approval lifecycle operations.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_auth_providers::builtin_authz::BuiltinApprovalRow;
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{ApprovalDecision as AsyncApprovalDecision, IdentityType};
use moa_core::{
    ApprovalDecision as ToolApprovalDecision, Event, EventRange, EventRecord, SessionFilter,
    SessionId, SessionStore as _, UserId,
};
use moa_ocsf::ActorInput;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::require_identity;
use crate::objects::session::SessionClient;
use crate::objects::sub_agent::SubAgentClient;
use crate::workflows::approval_wait;

/// Approval summary returned to users.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalSummary {
    /// Approval row id.
    pub id: Uuid,
    /// Session waiting on this approval.
    pub session_id: Uuid,
    /// One-line action summary.
    pub action_summary: String,
    /// Full action details.
    pub action_details: serde_json::Value,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Approval decision request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// Approval id to resolve.
    pub id: Uuid,
    /// Decision outcome: `approved` or `denied`.
    pub outcome: String,
    /// Optional denial reason.
    pub reason: Option<String>,
}

/// Restate service surface for builtin approvals.
#[restate_sdk::service]
#[name = "Approvals"]
pub trait Approvals {
    /// List pending approvals for the caller.
    async fn list_mine() -> Result<Json<Vec<ApprovalSummary>>, HandlerError>;

    /// Resolve one approval with an approve or deny decision.
    async fn decide(request: Json<DecisionRequest>) -> Result<(), HandlerError>;
}

/// Concrete approvals service implementation.
#[derive(Clone, Default)]
pub struct ApprovalsImpl;

impl Approvals for ApprovalsImpl {
    #[tracing::instrument(skip(self, ctx))]
    async fn list_mine(
        &self,
        ctx: Context<'_>,
    ) -> Result<Json<Vec<ApprovalSummary>>, HandlerError> {
        annotate_restate_handler_span("Approvals", "list_mine");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(TerminalError::new_with_code(403, "only users can list approvals").into());
        }
        let orchestrator_ctx = OrchestratorCtx::current();
        let pool = orchestrator_ctx.graph_pool.clone();
        let session_store = orchestrator_ctx.session_store.clone();

        Ok(ctx
            .run(|| async move {
                list_mine_inner(pool, session_store, identity.id)
                    .await
                    .map(Json::from)
            })
            .name("approvals_list_mine")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide(
        &self,
        ctx: Context<'_>,
        request: Json<DecisionRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Approvals", "decide");
        let identity = require_identity(&ctx)?;
        if identity.identity_type != IdentityType::User {
            return Err(
                TerminalError::new_with_code(403, "only users can resolve approvals").into(),
            );
        }
        let request = request.into_inner();
        let orchestrator_ctx = OrchestratorCtx::current();
        let pool = orchestrator_ctx.graph_pool.clone();
        let session_store = orchestrator_ctx.session_store.clone();
        let resolved = ctx
            .run(|| async move {
                decide_inner(pool, session_store, identity.id, request)
                    .await
                    .map(Json::from)
            })
            .name("approvals_decide")
            .await?
            .into_inner();

        match resolved {
            ResolvedApproval::Builtin {
                awakeable_id,
                status,
                deny_reason,
            } => {
                let decision = match status.as_str() {
                    "approved" => AsyncApprovalDecision::Approved,
                    "denied" => AsyncApprovalDecision::Denied {
                        reason: deny_reason,
                    },
                    other => {
                        return Err(TerminalError::new(format!(
                            "unexpected approval status after decision: {other}"
                        ))
                        .into());
                    }
                };
                ctx.resolve_awakeable(&awakeable_id, Json::from(decision));
            }
            ResolvedApproval::Tool {
                session_id,
                sub_agent_id,
                decision,
            } => {
                if let Some(sub_agent_id) = sub_agent_id {
                    ctx.object_client::<SubAgentClient>(sub_agent_id)
                        .approve(Json::from(decision))
                        .call()
                        .await?;
                } else {
                    ctx.object_client::<SessionClient>(session_id.to_string())
                        .approve(Json::from(decision))
                        .call()
                        .await?;
                }
            }
        }
        Ok(())
    }
}

async fn list_mine_inner(
    pool: sqlx::PgPool,
    session_store: Arc<moa_session::PostgresSessionStore>,
    deciding_user_id: Uuid,
) -> Result<Vec<ApprovalSummary>, HandlerError> {
    let mut summaries = list_builtin_approvals(pool, deciding_user_id).await?;
    summaries.extend(list_event_backed_approvals(session_store, deciding_user_id).await?);
    summaries.sort_by_key(|summary| Reverse(summary.created_at));
    summaries.truncate(100);
    Ok(summaries)
}

async fn list_builtin_approvals(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
) -> Result<Vec<ApprovalSummary>, HandlerError> {
    let rows: Vec<BuiltinApprovalRow> = sqlx::query_as(
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
    .map_err(|error| TerminalError::new(format!("list approvals: {error}")))?;

    Ok(rows
        .into_iter()
        .map(|row| ApprovalSummary {
            id: row.id,
            session_id: row.session_id,
            action_summary: row.action_summary,
            action_details: row.action_details,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
        .collect())
}

async fn decide_inner(
    pool: sqlx::PgPool,
    session_store: Arc<moa_session::PostgresSessionStore>,
    deciding_user_id: Uuid,
    request: DecisionRequest,
) -> Result<ResolvedApproval, HandlerError> {
    if let Some(resolved) = try_decide_builtin(pool, deciding_user_id, &request).await? {
        return Ok(resolved);
    }

    let target = find_event_approval_target(session_store, deciding_user_id, request.id).await?;
    let decision = tool_decision_from_request(&request)?;
    Ok(ResolvedApproval::Tool {
        session_id: target.session_id,
        sub_agent_id: target.sub_agent_id,
        decision,
    })
}

async fn try_decide_builtin(
    pool: sqlx::PgPool,
    deciding_user_id: Uuid,
    request: &DecisionRequest,
) -> Result<Option<ResolvedApproval>, HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row: Option<BuiltinApprovalRow> = sqlx::query_as(
        r#"
        SELECT id, session_id, deciding_user_id, tenant_id, awakeable_id,
               action_summary, action_details, status, deny_reason,
               created_at, expires_at, decided_at, decided_by_user_id
        FROM builtin_pending_approvals
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(request.id)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("load approval: {error}")))?;
    let Some(row) = row else {
        return Ok(None);
    };

    if row.deciding_user_id != deciding_user_id {
        return Err(TerminalError::new_with_code(403, "not your approval").into());
    }
    if row.status != "pending" {
        return Err(
            TerminalError::new_with_code(409, format!("approval already {}", row.status)).into(),
        );
    }
    if row.expires_at <= Utc::now() {
        return Err(TerminalError::new_with_code(410, "approval expired").into());
    }

    let (status, decision) = match request.outcome.as_str() {
        "approved" => ("approved", AsyncApprovalDecision::Approved),
        "denied" => (
            "denied",
            AsyncApprovalDecision::Denied {
                reason: request.reason.clone(),
            },
        ),
        other => {
            return Err(TerminalError::new_with_code(400, format!("bad outcome: {other}")).into());
        }
    };

    let updated: BuiltinApprovalRow = sqlx::query_as(
        r#"
        UPDATE builtin_pending_approvals
        SET status = $2,
            deny_reason = $3,
            decided_at = NOW(),
            decided_by_user_id = $4
        WHERE id = $1
        RETURNING id, session_id, deciding_user_id, tenant_id, awakeable_id,
                  action_summary, action_details, status, deny_reason,
                  created_at, expires_at, decided_at, decided_by_user_id
        "#,
    )
    .bind(request.id)
    .bind(status)
    .bind(match &decision {
        AsyncApprovalDecision::Denied { reason } => reason.as_deref(),
        AsyncApprovalDecision::Approved | AsyncApprovalDecision::Timeout => None,
    })
    .bind(deciding_user_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("update approval: {error}")))?;

    moa_ocsf::emit_approval_decided_tx(
        &mut transaction,
        updated.tenant_id,
        ActorInput::user(deciding_user_id),
        updated.id,
        updated.status == "approved",
    )
    .await
    .map_err(|error| TerminalError::new(format!("audit approval decision: {error}")))?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(Some(ResolvedApproval::Builtin {
        awakeable_id: updated.awakeable_id,
        status: updated.status,
        deny_reason: updated.deny_reason,
    }))
}

async fn list_event_backed_approvals(
    session_store: Arc<moa_session::PostgresSessionStore>,
    deciding_user_id: Uuid,
) -> Result<Vec<ApprovalSummary>, HandlerError> {
    let sessions = session_store
        .list_sessions(SessionFilter {
            user_id: Some(UserId::new(deciding_user_id.to_string())),
            limit: Some(100),
            ..SessionFilter::default()
        })
        .await
        .map_err(HandlerError::from)?;

    let mut out = Vec::new();
    for session in sessions {
        let events = session_store
            .get_events(session.session_id, EventRange::all())
            .await
            .map_err(HandlerError::from)?;
        out.extend(pending_event_approval_summaries(
            session.session_id,
            &events,
        ));
    }
    Ok(out)
}

async fn find_event_approval_target(
    session_store: Arc<moa_session::PostgresSessionStore>,
    deciding_user_id: Uuid,
    approval_id: Uuid,
) -> Result<EventApprovalTarget, HandlerError> {
    let sessions = session_store
        .list_sessions(SessionFilter {
            user_id: Some(UserId::new(deciding_user_id.to_string())),
            limit: Some(100),
            ..SessionFilter::default()
        })
        .await
        .map_err(HandlerError::from)?;

    for session in sessions {
        let events = session_store
            .get_events(session.session_id, EventRange::all())
            .await
            .map_err(HandlerError::from)?;
        if let Some(target) = event_approval_target(session.session_id, &events, approval_id) {
            return Ok(target);
        }
    }

    Err(TerminalError::new_with_code(404, "approval not found").into())
}

fn pending_event_approval_summaries(
    session_id: SessionId,
    events: &[EventRecord],
) -> Vec<ApprovalSummary> {
    let decided = decided_approval_ids(events);
    let expires_in = event_approval_expiration_delta();

    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ApprovalRequested {
                request_id,
                tool_name,
                input_summary,
                risk_level,
                prompt,
                ..
            } if !decided.contains(request_id) => {
                let action_details = serde_json::json!({
                    "source": "session_event",
                    "tool_name": tool_name,
                    "input_summary": input_summary,
                    "risk_level": risk_level,
                    "prompt": prompt,
                });
                Some(ApprovalSummary {
                    id: *request_id,
                    session_id: session_id.0,
                    action_summary: format!("{tool_name}: {input_summary}"),
                    action_details,
                    created_at: record.timestamp,
                    expires_at: record.timestamp + expires_in,
                })
            }
            _ => None,
        })
        .collect()
}

fn event_approval_target(
    session_id: SessionId,
    events: &[EventRecord],
    approval_id: Uuid,
) -> Option<EventApprovalTarget> {
    let decided = decided_approval_ids(events);
    if decided.contains(&approval_id) {
        return None;
    }

    events.iter().rev().find_map(|record| match &record.event {
        Event::ApprovalRequested {
            request_id,
            sub_agent_id,
            ..
        } if *request_id == approval_id => Some(EventApprovalTarget {
            session_id: session_id.0,
            sub_agent_id: sub_agent_id.as_ref().map(ToString::to_string),
        }),
        _ => None,
    })
}

fn decided_approval_ids(events: &[EventRecord]) -> HashSet<Uuid> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ApprovalDecided { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .collect()
}

fn event_approval_expiration_delta() -> chrono::Duration {
    chrono::Duration::from_std(approval_wait::configured_timeout())
        .unwrap_or_else(|_| chrono::Duration::minutes(30))
}

fn tool_decision_from_request(
    request: &DecisionRequest,
) -> Result<ToolApprovalDecision, HandlerError> {
    match request.outcome.as_str() {
        "approved" => Ok(ToolApprovalDecision::AllowOnce),
        "denied" => Ok(ToolApprovalDecision::Deny {
            reason: request.reason.clone(),
        }),
        other => Err(TerminalError::new_with_code(400, format!("bad outcome: {other}")).into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ResolvedApproval {
    Builtin {
        awakeable_id: String,
        status: String,
        deny_reason: Option<String>,
    },
    Tool {
        session_id: Uuid,
        sub_agent_id: Option<String>,
        decision: ToolApprovalDecision,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventApprovalTarget {
    session_id: Uuid,
    sub_agent_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{ApprovalField, ApprovalPrompt, ApprovalRequest, RiskLevel, SessionId};

    use super::*;

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::from_u128(0x9000 + u128::from(sequence_num)),
            session_id: SessionId(Uuid::from_u128(0x100)),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc
                .with_ymd_and_hms(2026, 6, 15, 12, sequence_num as u32, 0)
                .single()
                .expect("valid timestamp"),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn approval_requested(request_id: Uuid, sub_agent_id: Option<&str>) -> Event {
        Event::ApprovalRequested {
            request_id,
            awakeable_id: Some(format!("awakeable-{request_id}")),
            sub_agent_id: sub_agent_id.map(str::to_string),
            tool_name: "bash".to_string(),
            input_summary: "run deploy".to_string(),
            risk_level: RiskLevel::High,
            prompt: ApprovalPrompt {
                request: ApprovalRequest {
                    request_id,
                    sub_agent_id: sub_agent_id.map(str::to_string),
                    tool_name: "bash".to_string(),
                    input_summary: "run deploy".to_string(),
                    risk_level: RiskLevel::High,
                },
                pattern: "bash deploy".to_string(),
                parameters: vec![ApprovalField {
                    label: "command".to_string(),
                    value: "deploy".to_string(),
                }],
                file_diffs: Vec::new(),
            },
        }
    }

    #[test]
    fn pending_event_approvals_ignore_decided_requests() {
        // Pins: /v1/approvals lists pending tool approvals from session events.
        let pending_id = Uuid::from_u128(0x201);
        let decided_id = Uuid::from_u128(0x202);
        let events = vec![
            record(1, approval_requested(pending_id, None)),
            record(2, approval_requested(decided_id, None)),
            record(
                3,
                Event::ApprovalDecided {
                    request_id: decided_id,
                    sub_agent_id: None,
                    decision: ToolApprovalDecision::AllowOnce,
                    decided_by: "user".to_string(),
                    decided_at: Utc::now(),
                },
            ),
        ];

        let summaries =
            pending_event_approval_summaries(SessionId(Uuid::from_u128(0x100)), &events);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, pending_id);
        assert_eq!(summaries[0].action_summary, "bash: run deploy");
        assert_eq!(
            summaries[0].action_details["source"],
            serde_json::json!("session_event")
        );
    }

    #[test]
    fn event_approval_target_preserves_sub_agent_owner() {
        // Pins: decisions for sub-agent approvals route to SubAgent/approve.
        let request_id = Uuid::from_u128(0x301);
        let events = vec![record(
            1,
            approval_requested(request_id, Some("agent:root/worker")),
        )];

        assert_eq!(
            event_approval_target(SessionId(Uuid::from_u128(0x100)), &events, request_id),
            Some(EventApprovalTarget {
                session_id: Uuid::from_u128(0x100),
                sub_agent_id: Some("agent:root/worker".to_string())
            })
        );
    }

    #[test]
    fn tool_decision_maps_public_approval_to_allow_once() {
        // Pins: public "approved" decisions do not silently persist always-allow rules.
        let request = DecisionRequest {
            id: Uuid::from_u128(0x401),
            outcome: "approved".to_string(),
            reason: None,
        };

        assert_eq!(
            tool_decision_from_request(&request).expect("approved maps"),
            ToolApprovalDecision::AllowOnce
        );
    }
}

//! Internal service that persists signed OCSF findings for security transitions.
//!
//! Separated from the owning virtual object on purpose. The Session and Worker
//! VOs are single-writer and must not block their queue on a Postgres write, but
//! the finding still has to be durable *before* the owner applies its outcome —
//! otherwise a halt could take effect with no audit record explaining why. A
//! synchronous call to this keyless service gives both: the VO's own state
//! transition stays a fast in-memory step, and the caller still awaits the
//! finding's durability before acting on it.

use moa_core::types::action_policy::ActionReviewOwner;
use moa_core::types::identifiers::{SessionId, TenantId, ToolCallId};
use moa_core::types::security::{
    InjectionSignal, SecurityCircuitOwner, SecurityCircuitStage, SecurityCircuitTransition,
    ToolCapabilityId, ToolOutputAssessment,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Request to record one prompt-injection circuit transition as a signed finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordCircuitTransitionRequest {
    /// Tenant that owns the finding.
    pub tenant_id: TenantId,
    /// Session the transition belongs to.
    pub session_id: SessionId,
    /// Exact transition the owner applied.
    pub transition: SecurityCircuitTransition,
    /// Stable detector signals behind the triggering assessment.
    pub signals: Vec<InjectionSignal>,
    /// Timestamp the owner journaled *before* applying the transition.
    ///
    /// Supplied by the caller rather than read here: a replay must reproduce the
    /// identical signed payload, and a clock read inside this handler would make
    /// every attempt look like a different finding.
    pub occurred_at: chrono::DateTime<chrono::Utc>,
}

/// Result of recording one circuit transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordCircuitTransitionResponse {
    /// Deterministic security-event identity, derived from the transition key.
    pub event_uid: uuid::Uuid,
    /// Whether this call inserted the finding or matched an existing one.
    pub replayed: bool,
}

/// Keyless Restate service for durable security-finding emission.
#[restate_sdk::service]
pub trait SecurityEvents {
    /// Persists one signed OCSF Detection Finding for a circuit transition.
    async fn record_circuit_transition(
        request: Json<RecordCircuitTransitionRequest>,
    ) -> Result<Json<RecordCircuitTransitionResponse>, HandlerError>;
}

/// Concrete security-events service backed by the shared Postgres pool.
#[derive(Clone)]
pub struct SecurityEventsImpl {
    pool: PgPool,
}

impl SecurityEventsImpl {
    /// Creates the service over the shared pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl SecurityEvents for SecurityEventsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal owner-dispatched audit write. It reads no caller-owned data
    // back and returns only a deterministic identity derived from its own input.
    async fn record_circuit_transition(
        &self,
        ctx: Context<'_>,
        request: Json<RecordCircuitTransitionRequest>,
    ) -> Result<Json<RecordCircuitTransitionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SecurityEvents", "record_circuit_transition");
        let request = request.into_inner();
        let pool = self.pool.clone();

        let response = ctx
            .run(|| async move {
                let (event_uid, write) = moa_ocsf::emit_prompt_injection_finding(
                    &pool,
                    request.tenant_id,
                    moa_ocsf::PromptInjectionFinding {
                        session_id: request.session_id.0,
                        transition: request.transition.clone(),
                        signals: request.signals.clone(),
                        occurred_at: request.occurred_at,
                    },
                )
                .await
                .map_err(finding_error_to_handler_error)?;
                Ok(Json::from(RecordCircuitTransitionResponse {
                    event_uid,
                    replayed: matches!(write, moa_ocsf::FindingWrite::ReplayMatched),
                }))
            })
            // Named so the finding write is one journaled step: a replay of the
            // owner replays this result rather than re-signing and re-inserting.
            .name("security_events_record_circuit_transition")
            .await?
            .into_inner();

        Ok(Json::from(response))
    }
}

/// Applies durable reviewed output metadata to its conversational owner's circuit.
///
/// Action-review recovery may have only the persisted capability and assessment,
/// not the original secured output envelope. Keeping this boundary on those exact
/// durable facts makes replay and crash recovery use the same circuit input.
pub(crate) async fn apply_reviewed_conversational_assessment(
    ctx: &Context<'_>,
    tenant_id: TenantId,
    owner: &ActionReviewOwner,
    tool_call_id: ToolCallId,
    capability: &ToolCapabilityId,
    assessment: &ToolOutputAssessment,
) -> Result<SecurityCircuitStage, HandlerError> {
    let session_id = owner.session_id();
    let circuit_owner = match owner {
        ActionReviewOwner::Coordinator {
            turn_id,
            generation,
            ..
        } => SecurityCircuitOwner::Coordinator {
            turn_id: turn_id.clone(),
            generation: *generation,
        },
        ActionReviewOwner::Worker {
            worker_id,
            turn_id,
            generation,
            ..
        } => SecurityCircuitOwner::Worker {
            worker_id: worker_id.to_string(),
            turn_id: turn_id.clone(),
            generation: *generation,
        },
        ActionReviewOwner::ExecutionTask { .. } => {
            return Err(TerminalError::new(
                "execution-task reviews do not have a conversational security-circuit owner",
            )
            .into());
        }
    };
    let occurred_at = ctx
        .run(|| async move { Ok(Json::from(chrono::Utc::now())) })
        .name("reviewed_prompt_injection_transition_timestamp")
        .await?
        .into_inner();
    let request = moa_wire::turn::ApplySecurityAssessmentRequest {
        owner: circuit_owner,
        allow_superseded_owner_noop: true,
        capability: capability.clone(),
        tool_call_id,
        assessment: assessment.clone(),
    };
    let applied = match owner {
        ActionReviewOwner::Coordinator { .. } => crate::restate_identity::replay_safe_request(
            ctx.object_client::<crate::objects::session::SessionClient>(session_id.to_string())
                .apply_security_assessment(Json::from(request)),
        )
        .call()
        .await?
        .into_inner(),
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<crate::objects::worker::WorkerClient>(worker_id.clone())
                    .apply_security_assessment(Json::from(request)),
            )
            .call()
            .await?
            .into_inner()
        }
        ActionReviewOwner::ExecutionTask { .. } => unreachable!("handled above"),
    };

    let Some(transition) = applied.transition else {
        return Ok(applied.stage);
    };
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<crate::services::session_store::RestateSessionStoreClient>()
            .append_event(Json::from(moa_wire::session_store::AppendEventRequest {
                session_id,
                event: moa_core::events::Event::PromptInjectionCircuitTransition {
                    transition: transition.clone(),
                    signals: assessment.signals.clone(),
                    redacted_spans: assessment.redacted_spans,
                    deduplicated_carriers: assessment.deduplicated_carriers,
                },
                dedupe_key: Some(transition.key.clone()),
            })),
    )
    .call()
    .await?;
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<SecurityEventsClient>()
            .record_circuit_transition(Json::from(RecordCircuitTransitionRequest {
                tenant_id,
                session_id,
                transition,
                signals: assessment.signals.clone(),
                occurred_at,
            })),
    )
    .call()
    .await?;
    Ok(applied.stage)
}

/// Maps a finding-emission failure onto the right Restate error kind.
///
/// A replay conflict means two genuinely different transitions collided on one
/// deterministic identity. Retrying cannot fix that, so it is terminal and must
/// surface rather than spin. Signing and database failures are transient and stay
/// retryable, because losing an audit record to a blip is not acceptable.
fn finding_error_to_handler_error(error: moa_ocsf::EmitError) -> HandlerError {
    match error {
        moa_ocsf::EmitError::ReplayConflict(message) => {
            TerminalError::new(format!("security finding replay conflict: {message}")).into()
        }
        moa_ocsf::EmitError::InvalidInput(message) => {
            TerminalError::new(format!("invalid security finding: {message}")).into()
        }
        internal @ (moa_ocsf::EmitError::Signing(_)
        | moa_ocsf::EmitError::Serialize(_)
        | moa_ocsf::EmitError::Database(_)) => {
            tracing::error!(
                error = ?internal,
                "security finding write failed"
            );
            HandlerError::from(anyhow::anyhow!("security finding write failed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_finding_errors_do_not_cross_the_service_boundary() {
        // Pins: signing material, serializer inputs, and database diagnostics
        // remain server-side even though these failures stay retryable.
        let serialize =
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON fixture");
        let serialize_detail = serialize.to_string();
        let cases = [
            (
                moa_ocsf::EmitError::Signing(moa_ocsf::signing::SigningError::InvalidKey(
                    "raw-signing-detail".to_string(),
                )),
                "raw-signing-detail".to_string(),
            ),
            (moa_ocsf::EmitError::Serialize(serialize), serialize_detail),
            (
                moa_ocsf::EmitError::Database(sqlx::Error::Protocol(
                    "raw-database-detail".to_string(),
                )),
                "raw-database-detail".to_string(),
            ),
        ];

        for (error, raw_detail) in cases {
            let rendered = format!("{:?}", finding_error_to_handler_error(error));
            assert!(
                rendered.contains("security finding write failed"),
                "unexpected client-facing error: {rendered}"
            );
            assert!(
                !rendered.contains(&raw_detail),
                "internal detail crossed the service boundary: {rendered}"
            );
        }
    }
}

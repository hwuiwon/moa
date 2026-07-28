//! Internal service that persists signed OCSF findings for security transitions.
//!
//! Separated from the owning virtual object on purpose. The Session and Worker
//! VOs are single-writer and must not block their queue on a Postgres write, but
//! the finding still has to be durable *before* the owner applies its outcome —
//! otherwise a halt could take effect with no audit record explaining why. A
//! synchronous call to this keyless service gives both: the VO's own state
//! transition stays a fast in-memory step, and the caller still awaits the
//! finding's durability before acting on it.

use moa_core::types::identifiers::{SessionId, TenantId};
use moa_core::types::security::{InjectionSignal, SecurityCircuitTransition};
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
        other => HandlerError::from(anyhow::anyhow!("security finding write failed: {other}")),
    }
}

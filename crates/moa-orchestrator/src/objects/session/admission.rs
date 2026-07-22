//! Shared fleet and tenant admission for coordinator turns.

use std::sync::Arc;
use std::time::Duration;

use moa_config::SessionLimitsConfig;
use moa_core::traits::RuntimeCacheStore;
use moa_core::types::identifiers::{SessionId, TenantId};
use restate_sdk::prelude::*;

use crate::workflows::errors::moa_error_to_handler_error;

const FLEET_LEASE_KEY: &str = "moa:turn-admission:{turn-admission}:fleet";

/// Scope that rejected one coordinator-turn admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RejectedScope {
    /// The fleet-wide running-turn budget was full.
    Fleet,
    /// The caller's per-tenant running-turn budget was full.
    Tenant,
}

/// Result of one shared admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct AdmissionDecision {
    /// Saturated scope, or `None` when the lease was acquired.
    pub(super) rejected_scope: Option<RejectedScope>,
    /// Live fleet leases after the attempt.
    pub(super) fleet_live: usize,
    /// Live tenant leases after the attempt, when that scope was reached.
    pub(super) tenant_live: usize,
}

impl AdmissionDecision {
    fn admitted(fleet_live: usize, tenant_live: usize) -> Self {
        Self {
            rejected_scope: None,
            fleet_live,
            tenant_live,
        }
    }

    fn rejected(scope: RejectedScope, fleet_live: usize, tenant_live: usize) -> Self {
        Self {
            rejected_scope: Some(scope),
            fleet_live,
            tenant_live,
        }
    }
}

/// Shared TTL-backed coordinator-turn admission policy.
#[derive(Clone)]
pub(super) struct TurnAdmission {
    store: Arc<dyn RuntimeCacheStore>,
    fleet_limit: usize,
    tenant_limit: usize,
    lease_ttl: Duration,
    retry_after_ms: u64,
}

impl TurnAdmission {
    /// Builds admission from the process-wide shared runtime store and typed limits.
    pub(super) fn new(store: Arc<dyn RuntimeCacheStore>, limits: &SessionLimitsConfig) -> Self {
        Self {
            store,
            fleet_limit: limits.turn_admission_fleet_limit as usize,
            tenant_limit: limits.turn_admission_tenant_limit as usize,
            lease_ttl: Duration::from_millis(limits.turn_admission_lease_ttl_ms),
            retry_after_ms: limits.turn_admission_retry_after_ms,
        }
    }

    /// Delay between lease renewals; three missed beats are required for crash reclamation.
    pub(super) fn heartbeat_interval_ms(&self) -> u64 {
        (self.lease_ttl.as_millis() as u64 / 3).max(1)
    }

    /// Acquires or idempotently renews the session's shared fleet and tenant lease.
    pub(super) async fn acquire(
        &self,
        ctx: &ObjectContext<'_>,
        session_id: SessionId,
        tenant_id: TenantId,
        action_name: &'static str,
    ) -> Result<(), HandlerError> {
        let admission = self.clone();
        let decision = ctx
            .run(move || async move {
                admission
                    .acquire_shared(session_id, tenant_id)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name(action_name)
            .await?
            .into_inner();
        self.record_decision(decision);
        if let Some(scope) = decision.rejected_scope {
            let scope = match scope {
                RejectedScope::Fleet => "fleet",
                RejectedScope::Tenant => "tenant",
            };
            return Err(TerminalError::new_with_code(
                429,
                format!(
                    "turn admission {scope} budget is saturated; retry_after_ms={}",
                    self.retry_after_ms
                ),
            )
            .into());
        }
        Ok(())
    }

    /// Releases the session's fleet and tenant leases after durable terminal handling.
    pub(super) async fn release(
        &self,
        ctx: &ObjectContext<'_>,
        session_id: SessionId,
        tenant_id: TenantId,
    ) -> Result<(), HandlerError> {
        let admission = self.clone();
        let remaining = ctx
            .run(move || async move {
                admission
                    .release_shared(session_id, tenant_id)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name("turn_admission_release")
            .await?
            .into_inner();
        metrics::gauge!("moa_turn_admission_live", "scope" => "fleet").set(remaining.0 as f64);
        Ok(())
    }

    async fn acquire_shared(
        &self,
        session_id: SessionId,
        tenant_id: TenantId,
    ) -> moa_core::error::Result<AdmissionDecision> {
        let lease_id = session_id.to_string();
        let fleet = self
            .store
            .try_acquire_bounded_lease(FLEET_LEASE_KEY, &lease_id, self.fleet_limit, self.lease_ttl)
            .await?;
        if !fleet.acquired {
            return Ok(AdmissionDecision::rejected(
                RejectedScope::Fleet,
                fleet.live,
                0,
            ));
        }

        let tenant_key = tenant_lease_key(tenant_id);
        let tenant = self
            .store
            .try_acquire_bounded_lease(&tenant_key, &lease_id, self.tenant_limit, self.lease_ttl)
            .await?;
        if !tenant.acquired {
            self.store
                .release_bounded_lease(FLEET_LEASE_KEY, &lease_id)
                .await?;
            return Ok(AdmissionDecision::rejected(
                RejectedScope::Tenant,
                fleet.live.saturating_sub(1),
                tenant.live,
            ));
        }
        Ok(AdmissionDecision::admitted(fleet.live, tenant.live))
    }

    async fn release_shared(
        &self,
        session_id: SessionId,
        tenant_id: TenantId,
    ) -> moa_core::error::Result<(usize, usize)> {
        let lease_id = session_id.to_string();
        let fleet = self
            .store
            .release_bounded_lease(FLEET_LEASE_KEY, &lease_id)
            .await;
        let tenant = self
            .store
            .release_bounded_lease(&tenant_lease_key(tenant_id), &lease_id)
            .await;
        match (fleet, tenant) {
            (Ok(fleet), Ok(tenant)) => Ok((fleet, tenant)),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    fn record_decision(&self, decision: AdmissionDecision) {
        let outcome = if decision.rejected_scope.is_some() {
            "rejected"
        } else {
            "admitted"
        };
        let scope = match decision.rejected_scope {
            Some(RejectedScope::Tenant) => "tenant",
            Some(RejectedScope::Fleet) | None => "fleet",
        };
        metrics::counter!(
            "moa_turn_admission_decisions_total",
            "scope" => scope,
            "outcome" => outcome
        )
        .increment(1);
        metrics::gauge!("moa_turn_admission_live", "scope" => "fleet")
            .set(decision.fleet_live as f64);
        metrics::histogram!("moa_turn_admission_tenant_utilization_ratio")
            .record(decision.tenant_live as f64 / self.tenant_limit as f64);
    }
}

fn tenant_lease_key(tenant_id: TenantId) -> String {
    format!("moa:turn-admission:{{turn-admission}}:tenant:{tenant_id}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use moa_config::SessionLimitsConfig;
    use moa_core::types::identifiers::{SessionId, TenantId};
    use moa_runtime_store::MemoryRuntimeCacheStore;

    use super::{RejectedScope, TurnAdmission};

    fn policy(fleet: u32, tenant: u32, ttl_ms: u64) -> TurnAdmission {
        let limits = SessionLimitsConfig {
            turn_admission_fleet_limit: fleet,
            turn_admission_tenant_limit: tenant,
            turn_admission_lease_ttl_ms: ttl_ms,
            ..SessionLimitsConfig::default()
        };
        TurnAdmission::new(Arc::new(MemoryRuntimeCacheStore::new()), &limits)
    }

    #[tokio::test(start_paused = true)]
    async fn shared_admission_enforces_fleet_and_tenant_caps_and_reclaims_crash_leases() {
        // Pins: the same shared store enforces both caps across policy clones,
        // durable session ids make retry idempotent, terminal release frees a
        // slot, and TTL reclaims a lease after a simulated replica crash.
        let admission = policy(2, 1, 10_000);
        assert_eq!(admission.heartbeat_interval_ms(), 3_333);
        let other_replica = admission.clone();
        let tenant_a = TenantId::new();
        let tenant_b = TenantId::new();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let session_c = SessionId::new();

        let first = admission.acquire_shared(session_a, tenant_a).await.unwrap();
        assert_eq!(first.rejected_scope, None);
        let retry = admission.acquire_shared(session_a, tenant_a).await.unwrap();
        assert_eq!(retry.rejected_scope, None);
        let tenant_full = other_replica
            .acquire_shared(session_b, tenant_a)
            .await
            .unwrap();
        assert_eq!(tenant_full.rejected_scope, Some(RejectedScope::Tenant));
        assert_eq!(
            other_replica
                .acquire_shared(session_b, tenant_b)
                .await
                .unwrap()
                .rejected_scope,
            None
        );
        assert_eq!(
            other_replica
                .acquire_shared(session_c, TenantId::new())
                .await
                .unwrap()
                .rejected_scope,
            Some(RejectedScope::Fleet)
        );

        admission.release_shared(session_a, tenant_a).await.unwrap();
        assert_eq!(
            admission
                .acquire_shared(session_c, TenantId::new())
                .await
                .unwrap()
                .rejected_scope,
            None
        );
        tokio::time::advance(Duration::from_millis(10_001)).await;
        assert_eq!(
            admission
                .acquire_shared(SessionId::new(), tenant_a)
                .await
                .unwrap()
                .rejected_scope,
            None
        );
    }
}

//! Multi-tenant identity pools for load generation.
//!
//! Real MOA traffic is spread across tenants with a heavy-tailed skew. A
//! single-tenant load test measures the Session-owned Valkey admission cap
//! instead of fleet capacity. The pool creates
//! `tenants x identities_per_tenant` caller identities up front, grants each
//! identity its tenant-operator tuple exactly once, and samples tenants with
//! Zipf weights (a few heavy tenants, a long tail).

use moa_authz::FgaClient;
use moa_core::traits::{Identity, IdentityType};
use rand::{Rng, rngs::StdRng};

use crate::*;

/// Zipf exponent for tenant weights; 1.0 gives the classic 1/rank skew.
const TENANT_ZIPF_EXPONENT: f64 = 1.0;

/// One caller the harness can act as.
#[derive(Debug, Clone)]
pub(crate) struct TenantIdentity {
    /// Tenant this caller belongs to.
    pub(crate) tenant_id: TenantId,
    /// Caller identity forwarded in trusted headers.
    pub(crate) identity: Identity,
}

/// Weighted pool of load-test callers.
pub(crate) struct TenancyPool {
    entries: Vec<TenantIdentity>,
    cumulative_weights: Vec<f64>,
}

impl TenancyPool {
    /// Generates a fresh pool of `tenants x identities_per_tenant` callers.
    pub(crate) fn generate(tenants: usize, identities_per_tenant: usize) -> Result<Self> {
        if tenants == 0 || identities_per_tenant == 0 {
            return Err(MoaError::ValidationError(
                "tenancy pool requires at least one tenant and one identity per tenant".to_string(),
            ));
        }
        let mut entries = Vec::with_capacity(tenants * identities_per_tenant);
        let mut cumulative_weights = Vec::with_capacity(tenants * identities_per_tenant);
        let mut total = 0.0_f64;
        for tenant_rank in 0..tenants {
            let tenant_id = TenantId::new();
            let tenant_weight = 1.0 / ((tenant_rank + 1) as f64).powf(TENANT_ZIPF_EXPONENT);
            for _ in 0..identities_per_tenant {
                let identity = Identity {
                    identity_type: IdentityType::Operator,
                    id: Uuid::now_v7(),
                    tenant_id,
                    api_key_id: None,
                    acting_on_behalf_of: None,
                };
                entries.push(TenantIdentity {
                    tenant_id,
                    identity,
                });
                total += tenant_weight / identities_per_tenant as f64;
                cumulative_weights.push(total);
            }
        }
        Ok(Self {
            entries,
            cumulative_weights,
        })
    }

    /// All callers in the pool, in stable order.
    pub(crate) fn entries(&self) -> &[TenantIdentity] {
        &self.entries
    }

    /// Samples a caller index with tenant-Zipf weighting.
    pub(crate) fn pick_index(&self, rng: &mut StdRng) -> usize {
        let Some(total) = self.cumulative_weights.last().copied() else {
            return 0;
        };
        let target: f64 = rng.gen_range(0.0..total);
        self.cumulative_weights
            .partition_point(|weight| *weight <= target)
            .min(self.entries.len() - 1)
    }

    /// Grants each identity its tenant-operator tuple. Runs once at setup so
    /// per-session grant traffic never pollutes turn measurements.
    pub(crate) async fn grant_operators(&self, fga: &FgaClient) -> Result<()> {
        for entry in &self.entries {
            grant_raw_tuple(
                fga,
                format!("operator:{}", entry.identity.id),
                "operator",
                format!("tenant:{}", entry.tenant_id),
            )
            .await?;
        }
        Ok(())
    }
}

/// Writes one raw OpenFGA tuple.
pub(crate) async fn grant_raw_tuple(
    fga: &FgaClient,
    user: String,
    relation: &str,
    object: String,
) -> Result<()> {
    fga.apply_raw(serde_json::json!({
        "authorization_model_id": fga.model_id(),
        "writes": {
            "tuple_keys": [{
                "user": user,
                "relation": relation,
                "object": object,
            }],
        },
    }))
    .await
    .map_err(|error| MoaError::ProviderError(format!("loadtest OpenFGA grant failed: {error}")))
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;

    #[test]
    fn pool_generates_distinct_tenants_and_identities() {
        // Pins: every caller gets a unique identity id and tenants are not
        // reused across ranks, so RLS/authz caches see realistic diversity.
        let pool = TenancyPool::generate(3, 2).expect("pool");

        let entries = pool.entries();
        assert_eq!(entries.len(), 6);
        let mut tenant_ids: Vec<_> = entries.iter().map(|entry| entry.tenant_id).collect();
        tenant_ids.dedup();
        assert_eq!(
            tenant_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
        let identity_ids: std::collections::HashSet<_> =
            entries.iter().map(|entry| entry.identity.id).collect();
        assert_eq!(identity_ids.len(), 6);
    }

    #[test]
    fn zipf_sampling_prefers_low_rank_tenants() {
        // Pins: tenant rank 0 receives more traffic than the last rank under
        // Zipf weighting, so Session-owned Valkey admission sees a hot tenant.
        let pool = TenancyPool::generate(4, 1).expect("pool");
        let mut rng = StdRng::seed_from_u64(11);

        let mut counts = [0usize; 4];
        for _ in 0..4_000 {
            counts[pool.pick_index(&mut rng)] += 1;
        }

        assert!(
            counts[0] > counts[3] * 2,
            "expected rank-0 dominance, got {counts:?}"
        );
        assert!(counts.iter().all(|count| *count > 0));
    }

    #[test]
    fn empty_pool_dimensions_are_rejected() {
        // Pins: zero tenants or identities fails fast instead of panicking at
        // sample time.
        assert!(TenancyPool::generate(0, 1).is_err());
        assert!(TenancyPool::generate(1, 0).is_err());
    }
}

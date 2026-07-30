//! Effective sandbox policy resolution and provider capability admission.
//!
//! Five layers decide what a sandbox may consume: the deployment configuration,
//! the tenant's current authored policy, the agent policy pinned on the
//! session, the hand route serving the tool, and the provenance of the call
//! being served. This module is the one place they are combined, and the only
//! place an [`EffectiveSandboxProfile`] is produced. Resolution is a
//! restrictive intersection, so no layer can widen what another bounded, and
//! every layer contributes a named revision to the resulting policy identity
//! hash — including the layers MOA contributes when no one authored one.
//!
//! The origin layer is what binds a generated-code or experiment-trial sandbox
//! to [`moa_core::types::hands::EgressPolicy::DenyAll`]. Expressing it as a
//! policy layer rather than as a check somewhere in dispatch means the
//! restriction is in the resolved profile, is admitted against the serving
//! provider's capabilities before anything is provisioned, and is part of the
//! sandbox's policy identity — so a production sandbox can never be reused to
//! serve generated code or an experiment trial.
//!
//! Resolution is followed immediately by admission against the serving
//! provider's [`HandProviderCapabilities`]. A provider that cannot enforce a
//! dimension refuses the sandbox here, before any lease is claimed and before
//! any provider API call, rather than accepting the field and dropping it.

use async_trait::async_trait;
use moa_config::{LOCAL_DEVELOPMENT_SANDBOX_REVISION, MoaConfig};
use moa_core::{
    error::MoaError, error::Result, types::agent::AgentPolicySnapshot,
    types::hands::BuiltinPolicyRevision, types::hands::EffectiveSandboxProfile,
    types::hands::HandProviderCapabilities, types::hands::SandboxPolicySnapshot,
    types::hands::resolve_effective_sandbox_profile, types::identifiers::TenantId,
    types::memory::RlsContext, types::session::SessionMeta,
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row, types::Json};

use super::{HandRoute, ToolRouter};

/// Durable owner of each tenant's authored sandbox policy layer.
///
/// The router reads through this owner on every provisioning decision rather
/// than caching a tenant's policy for the life of the process: a tenant that
/// tightens its sandbox policy must affect the next sandbox, not the next
/// deployment.
#[async_trait]
pub trait TenantSandboxPolicyStore: Send + Sync {
    /// Loads the tenant's current authored policy layer, when it has one.
    async fn current(&self, tenant_id: TenantId) -> Result<Option<SandboxPolicySnapshot>>;
}

/// Postgres-backed tenant sandbox policy store.
#[derive(Clone)]
pub struct PostgresTenantSandboxPolicyStore {
    pool: PgPool,
}

impl PostgresTenantSandboxPolicyStore {
    /// Creates a tenant sandbox policy store from an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TenantSandboxPolicyStore for PostgresTenantSandboxPolicyStore {
    async fn current(&self, tenant_id: TenantId) -> Result<Option<SandboxPolicySnapshot>> {
        let mut conn =
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await?;
        let row = sqlx::query(
            r#"
            SELECT revision, profile
            FROM moa.tenant_sandbox_policy
            WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
        conn.commit().await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let revision: String = row
            .try_get("revision")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        let profile: Json<moa_core::types::hands::SandboxProfile> = row
            .try_get("profile")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        SandboxPolicySnapshot::new(&revision, profile.0).map(Some)
    }
}

/// Builds the deployment policy layer a configuration declares.
pub fn deployment_sandbox_policy(config: &MoaConfig) -> Result<SandboxPolicySnapshot> {
    config.sandbox_policy.deployment.snapshot()
}

/// Returns the built-in local-development deployment layer.
///
/// This is the same deliberately unbounded policy a default configuration
/// declares, under the same named revision, for callers that assemble a router
/// without a `MoaConfig`. It is not a shortcut around stating policy: the
/// revision is `local-development-unbounded`, it enters every policy identity
/// hash, and `security_profile = cloud` refuses to construct a router with it.
#[must_use]
pub fn local_development_sandbox_policy() -> SandboxPolicySnapshot {
    SandboxPolicySnapshot {
        revision: LOCAL_DEVELOPMENT_SANDBOX_REVISION.to_string(),
        profile: moa_core::types::hands::SandboxProfile::unrestricted(),
    }
}

/// Builds the authored route policy layer for one hand provider.
///
/// A provider with no authored entry gets
/// [`BuiltinPolicyRevision::RouteUnset`]: named and hash-significant, but
/// restricting nothing on its own.
pub fn route_sandbox_policy(config: &MoaConfig, provider: &str) -> Result<SandboxPolicySnapshot> {
    match config.sandbox_policy.route(provider) {
        Some(layer) => layer.snapshot(),
        None => Ok(SandboxPolicySnapshot::builtin(
            BuiltinPolicyRevision::RouteUnset,
        )),
    }
}

impl ToolRouter {
    /// Resolves and admits the one effective sandbox policy for a provisioning
    /// decision.
    ///
    /// Reads the tenant layer through the durable store on every call, takes
    /// the agent layer from the snapshot pinned on the session, takes the route
    /// layer from the route being served, and refuses the sandbox outright when
    /// the serving provider cannot enforce a dimension the resolved profile
    /// requires.
    pub(super) async fn resolve_sandbox_profile(
        &self,
        route: &HandRoute,
        session: &SessionMeta,
    ) -> Result<EffectiveSandboxProfile> {
        let capabilities = self.provider_capabilities(&route.provider)?;
        let tenant = self.tenant_sandbox_policy(session.tenant_id).await?;
        let agent = agent_sandbox_policy(session)?;
        // The composition of the router's deployment-level origin ceiling and
        // the session's durable origin, so neither can be widened by the other.
        let origin = SandboxPolicySnapshot::origin(self.effective_call_origin(session));

        let effective = resolve_effective_sandbox_profile(
            &self.deployment_sandbox_policy,
            &tenant,
            &agent,
            &route.policy,
            &origin,
            &capabilities.revision,
        )?;
        capabilities.admit(
            route.tier,
            effective.profile(),
            self.hand_lease_reaper_installed,
        )?;
        Ok(effective)
    }

    /// Returns one registered provider's declared capabilities.
    pub(super) fn provider_capabilities(&self, provider: &str) -> Result<HandProviderCapabilities> {
        self.providers
            .get(provider)
            .map(|registered| registered.capabilities())
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))
    }

    /// Reads the tenant's current authored policy layer.
    ///
    /// A tenant that has authored nothing contributes
    /// [`BuiltinPolicyRevision::TenantUnset`], the identity element: it cannot
    /// widen the deployment, agent, or route layers, and it is named in the
    /// policy identity hash so a tenant that later authors one changes it.
    async fn tenant_sandbox_policy(&self, tenant_id: TenantId) -> Result<SandboxPolicySnapshot> {
        let Some(store) = self.tenant_sandbox_policy.as_ref() else {
            return Ok(SandboxPolicySnapshot::builtin(
                BuiltinPolicyRevision::TenantUnset,
            ));
        };
        Ok(store
            .current(tenant_id)
            .await?
            .unwrap_or_else(|| SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset)))
    }
}

/// Returns the sandbox policy layer the session's pinned agent declares.
fn agent_sandbox_policy(session: &SessionMeta) -> Result<SandboxPolicySnapshot> {
    let Some(agent_context) = session.agent_context.as_ref() else {
        return Ok(SandboxPolicySnapshot::builtin(
            BuiltinPolicyRevision::AgentUnset,
        ));
    };
    let snapshot: AgentPolicySnapshot = agent_context.parsed_policy_snapshot()?;
    snapshot.sandbox_policy.snapshot()
}

#[cfg(test)]
pub(crate) mod test_support {
    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::hands::{
        BuiltinPolicyRevision, HandSpec, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
        resolve_effective_sandbox_profile,
    };

    /// Builds a hand spec through the production resolution path, so a test
    /// spec is admissible for exactly the reasons a real one is.
    pub(crate) fn hand_spec(tier: SandboxTier, profile: SandboxProfile) -> HandSpec {
        let effective_profile = resolve_effective_sandbox_profile(
            &SandboxPolicySnapshot::new("test-deployment", profile).expect("deployment snapshot"),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
            &SandboxPolicySnapshot::origin(CallOrigin::Production),
            "test-capabilities-v1",
        )
        .expect("test policy resolution should succeed");
        HandSpec {
            budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
            sandbox_tier: tier,
            image: None,
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            effective_profile,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use moa_core::types::hands::{
        CpuLimit, DeadlineEnforcement, DiskLimit, EgressMode, EgressPolicy,
        HandProviderCapabilities, LifetimeLimit, MemoryLimit, ResourceSupport, SandboxProfile,
        SandboxTier, SandboxTierCapabilities,
    };

    use crate::core::{ToolRegistry, ToolRouter};

    use super::*;

    fn seconds(value: u64) -> LifetimeLimit {
        LifetimeLimit::Bounded {
            seconds: NonZeroU64::new(value).expect("nonzero seconds"),
        }
    }

    /// A provider whose capability revision can flip, standing in for a
    /// deployment that upgrades what its sandbox backend can enforce.
    struct RevisionedProvider {
        upgraded: AtomicBool,
    }

    #[async_trait]
    impl moa_core::traits::HandProvider for RevisionedProvider {
        fn provider_name(&self) -> &str {
            "local"
        }

        fn capabilities(&self) -> HandProviderCapabilities {
            let revision = if self.upgraded.load(Ordering::SeqCst) {
                "revisioned-v2"
            } else {
                "revisioned-v1"
            };
            HandProviderCapabilities {
                revision: revision.to_string(),
                tiers: vec![SandboxTierCapabilities {
                    tier: SandboxTier::Local,
                    cpu: ResourceSupport::unbounded_only(),
                    memory: ResourceSupport::unbounded_only(),
                    ephemeral_disk: ResourceSupport::unbounded_only(),
                    egress_modes: vec![EgressMode::DenyAll, EgressMode::Unrestricted],
                    idle_enforcement: DeadlineEnforcement::DurableReaper,
                    max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
                }],
            }
        }

        async fn provision(
            &self,
            _spec: moa_core::types::hands::HandSpec,
        ) -> Result<moa_core::types::hands::HandHandle> {
            Err(MoaError::Unsupported("test double".to_string()))
        }

        async fn execute(
            &self,
            _handle: &moa_core::types::hands::HandHandle,
            _tool: &str,
            _input: &str,
        ) -> Result<moa_core::types::tools::ToolOutput> {
            Err(MoaError::Unsupported("test double".to_string()))
        }

        async fn status(
            &self,
            _handle: &moa_core::types::hands::HandHandle,
        ) -> Result<moa_core::types::hands::HandStatus> {
            Ok(moa_core::types::hands::HandStatus::Running)
        }

        async fn pause(&self, _handle: &moa_core::types::hands::HandHandle) -> Result<()> {
            Ok(())
        }

        async fn resume(&self, _handle: &moa_core::types::hands::HandHandle) -> Result<()> {
            Ok(())
        }

        async fn destroy(&self, _handle: &moa_core::types::hands::HandHandle) -> Result<()> {
            Ok(())
        }
    }

    /// A tenant policy store serving one fixed authored layer.
    struct StaticTenantPolicy {
        snapshot: Option<SandboxPolicySnapshot>,
    }

    #[async_trait]
    impl TenantSandboxPolicyStore for StaticTenantPolicy {
        async fn current(&self, _tenant_id: TenantId) -> Result<Option<SandboxPolicySnapshot>> {
            Ok(self.snapshot.clone())
        }
    }

    fn router(
        provider: Arc<RevisionedProvider>,
        tenant: Option<SandboxPolicySnapshot>,
    ) -> ToolRouter {
        let mut providers = std::collections::HashMap::new();
        let provider_trait: Arc<dyn moa_core::traits::HandProvider> = provider;
        providers.insert("local".to_string(), provider_trait);
        ToolRouter::new(
            ToolRegistry::new(),
            providers,
            SandboxPolicySnapshot::new(
                "deployment-v1",
                SandboxProfile::new(
                    CpuLimit::Unbounded,
                    MemoryLimit::Unbounded,
                    DiskLimit::Unbounded,
                    EgressPolicy::Unrestricted,
                    seconds(600),
                    seconds(7200),
                )
                .expect("deployment profile"),
            )
            .expect("deployment snapshot"),
        )
        .with_hand_lease_reaper()
        .with_tenant_sandbox_policy_store(Arc::new(StaticTenantPolicy { snapshot: tenant }))
    }

    fn route() -> HandRoute {
        HandRoute {
            provider: "local".to_string(),
            tier: SandboxTier::Local,
            policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        }
    }

    #[tokio::test]
    async fn the_tenant_layer_reaches_resolution_and_tightens_the_deployment_layer() {
        // Pins: the tenant's authored layer is read through the durable store on
        // every provisioning decision and actually participates in the
        // intersection. Dropping it from resolution would leave the deployment's
        // looser deadlines in force for a tenant that asked for tighter ones.
        let provider = Arc::new(RevisionedProvider {
            upgraded: AtomicBool::new(false),
        });
        let tenant_layer = SandboxPolicySnapshot::new(
            "tenant-v1",
            SandboxProfile::new(
                CpuLimit::Unbounded,
                MemoryLimit::Unbounded,
                DiskLimit::Unbounded,
                EgressPolicy::DenyAll,
                seconds(60),
                seconds(120),
            )
            .expect("tenant profile"),
        )
        .expect("tenant snapshot");

        let without_tenant = router(provider.clone(), None)
            .resolve_sandbox_profile(&route(), &session())
            .await
            .expect("resolve without a tenant layer");
        assert_eq!(without_tenant.profile().idle_timeout, seconds(600));
        assert_eq!(without_tenant.profile().egress, EgressPolicy::Unrestricted);
        assert_eq!(without_tenant.sources().tenant, "tenant-sandbox-unset");

        let with_tenant = router(provider, Some(tenant_layer))
            .resolve_sandbox_profile(&route(), &session())
            .await
            .expect("resolve with a tenant layer");
        assert_eq!(
            with_tenant.profile().idle_timeout,
            seconds(60),
            "the tenant's tighter idle timeout must win"
        );
        assert_eq!(
            with_tenant.profile().max_lifetime,
            seconds(120),
            "the tenant's tighter hard lifetime must win"
        );
        assert_eq!(
            with_tenant.profile().egress,
            EgressPolicy::DenyAll,
            "the tenant's deny-all egress must dominate"
        );
        assert_eq!(with_tenant.sources().tenant, "tenant-v1");
        assert_ne!(
            with_tenant.profile_hash(),
            without_tenant.profile_hash(),
            "the tenant layer is hash-significant"
        );
    }

    #[tokio::test]
    async fn a_changed_provider_capability_revision_changes_the_policy_identity() {
        // Pins: the serving provider's capability revision is part of the
        // sandbox's policy identity. When a deployment upgrades what its backend
        // enforces, the hash moves, so a sandbox provisioned under the old
        // declaration can no longer match a lease and is replaced rather than
        // reinterpreted under capabilities it was never admitted for.
        let provider = Arc::new(RevisionedProvider {
            upgraded: AtomicBool::new(false),
        });
        let router = router(provider.clone(), None);
        let session = session();

        let before = router
            .resolve_sandbox_profile(&route(), &session)
            .await
            .expect("resolve under the original capability revision");
        provider.upgraded.store(true, Ordering::SeqCst);
        let after = router
            .resolve_sandbox_profile(&route(), &session)
            .await
            .expect("resolve under the upgraded capability revision");

        assert_eq!(before.capability_revision(), "revisioned-v1");
        assert_eq!(after.capability_revision(), "revisioned-v2");
        assert_eq!(
            before.profile(),
            after.profile(),
            "the six dimensions are unchanged; only the provider's declaration moved"
        );
        assert_ne!(
            before.profile_hash(),
            after.profile_hash(),
            "an unchanged profile under a changed capability revision is a different policy identity"
        );
    }
}

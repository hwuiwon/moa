//! Effective sandbox profile resolution, provider admission, and adapter
//! translate-or-reject behavior.

use std::num::{NonZeroU32, NonZeroU64};

use moa_config::{CloudHandsConfig, MoaConfig, SandboxProfileConfig, SecurityProfile};
use moa_core::types::action_policy::CallOrigin;
use moa_core::{
    error::MoaError,
    traits::HandProvider,
    types::hands::{
        BuiltinPolicyRevision, CpuLimit, DeadlineEnforcement, DiskLimit, EgressMode, EgressPolicy,
        HandSpec, LifetimeLimit, MemoryLimit, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
        resolve_effective_sandbox_profile,
    },
};
use moa_hands::{LocalHandProvider, ToolRouter, deployment_sandbox_policy, route_sandbox_policy};
use tempfile::tempdir;

fn seconds(value: u64) -> LifetimeLimit {
    LifetimeLimit::Bounded {
        seconds: NonZeroU64::new(value).expect("nonzero seconds"),
    }
}

fn millicores(value: u32) -> CpuLimit {
    CpuLimit::Bounded {
        millicores: NonZeroU32::new(value).expect("nonzero millicores"),
    }
}

fn mebibytes(value: u32) -> MemoryLimit {
    MemoryLimit::Bounded {
        mebibytes: NonZeroU32::new(value).expect("nonzero mebibytes"),
    }
}

fn profile(
    cpu: CpuLimit,
    memory: MemoryLimit,
    disk: DiskLimit,
    egress: EgressPolicy,
    idle: LifetimeLimit,
    hard: LifetimeLimit,
) -> SandboxProfile {
    SandboxProfile::new(cpu, memory, disk, egress, idle, hard).expect("profile should validate")
}

fn spec(tier: SandboxTier, profile: SandboxProfile) -> HandSpec {
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
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        workspace_mount: None,
        effective_profile,
    }
}

#[tokio::test]
async fn local_host_sandbox_refuses_every_limit_it_cannot_enforce_offline() {
    // Pins: a bare host process advertises no CPU, memory, disk, or network
    // enforcement, so a bounded request for any of them is refused rather than
    // silently degrading a bounded policy into an unrestricted host process.
    let dir = tempdir().expect("create tempdir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("create local hand provider");

    for (label, unenforceable) in [
        (
            "cpu",
            profile(
                millicores(500),
                MemoryLimit::Unbounded,
                DiskLimit::Unbounded,
                EgressPolicy::Unrestricted,
                LifetimeLimit::Unbounded,
                LifetimeLimit::Unbounded,
            ),
        ),
        (
            "memory",
            profile(
                CpuLimit::Unbounded,
                mebibytes(512),
                DiskLimit::Unbounded,
                EgressPolicy::Unrestricted,
                LifetimeLimit::Unbounded,
                LifetimeLimit::Unbounded,
            ),
        ),
        (
            "ephemeral disk",
            profile(
                CpuLimit::Unbounded,
                MemoryLimit::Unbounded,
                DiskLimit::Bounded {
                    mebibytes: NonZeroU32::new(2048).expect("nonzero mebibytes"),
                },
                EgressPolicy::Unrestricted,
                LifetimeLimit::Unbounded,
                LifetimeLimit::Unbounded,
            ),
        ),
        (
            "egress",
            profile(
                CpuLimit::Unbounded,
                MemoryLimit::Unbounded,
                DiskLimit::Unbounded,
                EgressPolicy::DenyAll,
                LifetimeLimit::Unbounded,
                LifetimeLimit::Unbounded,
            ),
        ),
    ] {
        let error = provider
            .provision(spec(SandboxTier::Local, unenforceable))
            .await
            .expect_err("host sandbox must refuse a bound it cannot enforce");
        assert!(
            matches!(error, MoaError::Unsupported(_)),
            "{label}: expected an Unsupported refusal, got {error:?}"
        );
    }

    // The deliberately unbounded profile is exactly what the host tier can serve.
    provider
        .provision(spec(SandboxTier::Local, SandboxProfile::unrestricted()))
        .await
        .expect("an explicitly unbounded profile is admissible on the host tier");
}

#[test]
fn local_capabilities_declare_docker_enforcement_and_host_limits_separately_offline() {
    // Pins: capability declarations are per tier. Docker advertises bounded CPU
    // and memory plus deny-all/unrestricted egress; the host tier advertises
    // none of that. A single blended declaration would either forbid what
    // Docker can do or claim enforcement the host does not have.
    let capabilities = moa_hands::LOCAL_HAND_CAPABILITIES.clone();
    let container = capabilities
        .tiers
        .iter()
        .find(|tier| tier.tier == SandboxTier::Container)
        .expect("container tier is declared");
    let host = capabilities
        .tiers
        .iter()
        .find(|tier| tier.tier == SandboxTier::Local)
        .expect("host tier is declared");

    assert!(container.cpu.bounded.is_some());
    assert!(container.memory.bounded.is_some());
    assert!(
        container.ephemeral_disk.bounded.is_none(),
        "Docker cannot bound ephemeral disk on the default storage driver"
    );
    assert_eq!(
        container.egress_modes,
        vec![EgressMode::DenyAll, EgressMode::Unrestricted],
        "Docker has no per-destination egress filter"
    );

    assert!(host.cpu.bounded.is_none());
    assert!(host.memory.bounded.is_none());
    assert!(host.ephemeral_disk.bounded.is_none());
    assert_eq!(host.egress_modes, vec![EgressMode::Unrestricted]);

    // Neither tier stops a sandbox on its own, so both name the durable reaper.
    for tier in [container, host] {
        assert_eq!(tier.idle_enforcement, DeadlineEnforcement::DurableReaper);
        assert_eq!(
            tier.max_lifetime_enforcement,
            DeadlineEnforcement::DurableReaper
        );
    }
}

#[test]
fn bounded_deadlines_are_inadmissible_without_the_durable_reaper_offline() {
    // Pins: a provider whose deadline owner is the durable reaper may only serve
    // a bounded deadline when that reaper is actually installed. Without it the
    // sandbox would be provisioned with a deadline nothing enforces.
    let capabilities = moa_hands::LOCAL_HAND_CAPABILITIES.clone();
    let bounded = profile(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::Unrestricted,
        seconds(300),
        seconds(3600),
    );

    assert!(
        capabilities
            .admit(SandboxTier::Local, &bounded, false)
            .is_err(),
        "a bounded deadline with no installed reaper must be refused"
    );
    capabilities
        .admit(SandboxTier::Local, &bounded, true)
        .expect("the same profile is admissible once the reaper is installed");
    capabilities
        .admit(SandboxTier::Local, &SandboxProfile::unrestricted(), false)
        .expect("unbounded deadlines need no destruction owner");
}

#[tokio::test]
async fn cloud_profile_refuses_the_built_in_local_development_sandbox_policy_offline() {
    // Pins: `security_profile = cloud` will not construct a router on the
    // built-in unbounded local-development policy. A cloud deployment must
    // author its own six dimensions rather than inheriting the local default.
    let mut config = MoaConfig {
        security_profile: SecurityProfile::Cloud,
        ..MoaConfig::default()
    };
    config.permissions.default_effect = moa_core::types::action_policy::ActionPolicyEffect::Deny;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        e2b_api_key: Some("MOA_TEST_E2B_KEY".to_string()),
        ..CloudHandsConfig::default()
    });

    let error =
        match ToolRouter::from_config(&config, None, Some(std::sync::Arc::new(NoRules))).await {
            Ok(_) => panic!("cloud must refuse the built-in local development sandbox policy"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(
        message.contains("local-development-unbounded"),
        "error should name the built-in policy it refused, got: {message}"
    );

    // Authoring a deployment policy is what unblocks it.
    config.sandbox_policy.deployment = SandboxProfileConfig {
        revision: "cloud-sandbox-v1".to_string(),
        cpu: CpuLimit::Unbounded,
        memory: MemoryLimit::Unbounded,
        ephemeral_disk: DiskLimit::Unbounded,
        egress: EgressPolicy::DenyAll,
        idle_timeout: seconds(300),
        max_lifetime: seconds(3600),
    };
    ToolRouter::from_config(&config, None, Some(std::sync::Arc::new(NoRules)))
        .await
        .expect("an authored deployment sandbox policy lets the cloud router construct");
}

#[test]
fn route_layers_come_from_config_and_are_always_named_offline() {
    // Pins: an authored per-provider route layer reaches resolution, and a
    // provider with no authored layer still contributes a named, hash-significant
    // route revision rather than an absent one.
    let mut config = MoaConfig::default();
    config.sandbox_policy.deployment = SandboxProfileConfig {
        revision: "deployment-v1".to_string(),
        cpu: CpuLimit::Unbounded,
        memory: MemoryLimit::Unbounded,
        ephemeral_disk: DiskLimit::Unbounded,
        egress: EgressPolicy::Unrestricted,
        idle_timeout: LifetimeLimit::Unbounded,
        max_lifetime: LifetimeLimit::Unbounded,
    };
    config.sandbox_policy.routes.insert(
        "e2b".to_string(),
        SandboxProfileConfig {
            revision: "e2b-route-v1".to_string(),
            cpu: CpuLimit::Unbounded,
            memory: MemoryLimit::Unbounded,
            ephemeral_disk: DiskLimit::Unbounded,
            egress: EgressPolicy::DenyAll,
            idle_timeout: seconds(120),
            max_lifetime: seconds(600),
        },
    );

    let deployment = deployment_sandbox_policy(&config).expect("deployment layer");
    assert_eq!(deployment.revision, "deployment-v1");

    let authored = route_sandbox_policy(&config, "e2b").expect("authored route layer");
    assert_eq!(authored.revision, "e2b-route-v1");
    assert_eq!(authored.profile.egress, EgressPolicy::DenyAll);

    let unauthored = route_sandbox_policy(&config, "local").expect("unauthored route layer");
    assert_eq!(
        unauthored.revision,
        BuiltinPolicyRevision::RouteUnset.as_str()
    );

    // Resolving through the authored route tightens the deployment layer, and
    // the unauthored one leaves it exactly as the deployment declared it.
    let tenant = SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset);
    let agent = SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset);
    let tightened = resolve_effective_sandbox_profile(
        &deployment,
        &tenant,
        &agent,
        &authored,
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "cap-1",
    )
    .expect("resolve with authored route");
    assert_eq!(tightened.profile().egress, EgressPolicy::DenyAll);
    assert_eq!(tightened.profile().idle_timeout, seconds(120));
    assert_eq!(tightened.sources().route, "e2b-route-v1");

    let untightened = resolve_effective_sandbox_profile(
        &deployment,
        &tenant,
        &agent,
        &unauthored,
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "cap-1",
    )
    .expect("resolve with unauthored route");
    assert_eq!(untightened.profile().egress, EgressPolicy::Unrestricted);
    assert_ne!(
        untightened.profile_hash(),
        tightened.profile_hash(),
        "the route layer is hash-significant"
    );
}

/// A rule store with no persisted rules, which is all the cloud profile check
/// requires: it asserts that a durable owner exists, not what it contains.
struct NoRules;

#[async_trait::async_trait]
impl moa_security::ActionPolicyRuleStore for NoRules {
    async fn list_action_policy_rules_for_tool(
        &self,
        _tenant_id: &moa_core::types::identifiers::TenantId,
        _user_id: &moa_core::types::identifiers::UserId,
        _tool: &str,
    ) -> moa_core::error::Result<Vec<moa_core::types::action_policy::ActionPolicyRule>> {
        Ok(Vec::new())
    }

    async fn upsert_action_policy_rule(
        &self,
        _rule: moa_core::types::action_policy::ActionPolicyRule,
    ) -> moa_core::error::Result<()> {
        Ok(())
    }

    async fn delete_action_policy_rule(
        &self,
        _tenant_id: &moa_core::types::identifiers::TenantId,
        _user_id: Option<&moa_core::types::identifiers::UserId>,
        _tool: &str,
        _pattern: &str,
    ) -> moa_core::error::Result<()> {
        Ok(())
    }
}

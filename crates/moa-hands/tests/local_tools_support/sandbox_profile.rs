// Shared sandbox-policy fixtures for hand provider tests.
//
// Every fixture goes through the production resolution path
// (`resolve_effective_sandbox_profile` over four real policy layers) rather
// than hand-assembling an effective profile, so a test spec is admissible for
// exactly the reasons a production spec would be.

#[allow(dead_code)]
fn sandbox_seconds(seconds: u64) -> moa_core::types::hands::LifetimeLimit {
    moa_core::types::hands::LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(seconds).expect("nonzero seconds"),
    }
}

/// Resolves an effective profile from one deployment-authored profile plus the
/// three built-in identity layers.
#[allow(dead_code)]
fn effective_profile_from(
    profile: moa_core::types::hands::SandboxProfile,
    capability_revision: &str,
) -> moa_core::types::hands::EffectiveSandboxProfile {
    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::hands::{BuiltinPolicyRevision, SandboxPolicySnapshot};

    moa_core::types::hands::resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("test-deployment", profile).expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        capability_revision,
    )
    .expect("test policy resolution should succeed")
}

/// The deliberately unbounded local-development profile: every dimension is an
/// explicit `Unbounded`/`Unrestricted`, which is what a local host sandbox can
/// actually enforce.
#[allow(dead_code)]
fn unbounded_sandbox_profile() -> moa_core::types::hands::SandboxProfile {
    moa_core::types::hands::SandboxProfile::unrestricted()
}

/// A hand spec whose profile is deliberately unbounded on every dimension.
#[allow(dead_code)]
fn hand_spec(tier: moa_core::types::hands::SandboxTier) -> moa_core::types::hands::HandSpec {
    hand_spec_with_profile(tier, unbounded_sandbox_profile(), "test-capabilities-v1")
}

/// A hand spec carrying an explicitly stated profile.
#[allow(dead_code)]
fn hand_spec_with_profile(
    tier: moa_core::types::hands::SandboxTier,
    profile: moa_core::types::hands::SandboxProfile,
    capability_revision: &str,
) -> moa_core::types::hands::HandSpec {
    moa_core::types::hands::HandSpec {
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        workspace: workspace_binding(),
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        filesystem: moa_core::types::sandbox_workspace::SandboxFilesystemLayout::standard(),
        effective_profile: effective_profile_from(profile, capability_revision),
    }
}

fn workspace_binding() -> moa_core::types::sandbox_workspace::WorkspaceBinding {
    moa_core::types::sandbox_workspace::WorkspaceBinding {
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        scope: moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
            session_id: moa_core::types::identifiers::SessionId::new(),
            worker_id: "local-tools-worker".to_string(),
        },
        workspace_id: moa_core::types::identifiers::SandboxWorkspaceId::new(),
        provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
        provider_account_generation: 1,
        durability_class: moa_core::types::sandbox_workspace::DurabilityClass::PortableFilesystem,
        writer_epoch: 1,
        instance_generation: 1,
        current_revision: None,
    }
}

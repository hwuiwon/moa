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
    use moa_core::types::hands::{BuiltinPolicyRevision, SandboxPolicySnapshot};

    moa_core::types::hands::resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("test-deployment", profile).expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
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
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        workspace_mount: None,
        effective_profile: effective_profile_from(profile, capability_revision),
    }
}

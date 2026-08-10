//! Offline contract tests for sandbox profiles and provider capabilities.

use std::num::{NonZeroU32, NonZeroU64};

use moa_core::types::{action_policy::CallOrigin, hands::*};

fn cpu(millicores: u32) -> CpuLimit {
    CpuLimit::Bounded {
        millicores: NonZeroU32::new(millicores).expect("nonzero millicores"),
    }
}

fn memory(mebibytes: u32) -> MemoryLimit {
    MemoryLimit::Bounded {
        mebibytes: NonZeroU32::new(mebibytes).expect("nonzero mebibytes"),
    }
}

fn disk(mebibytes: u32) -> DiskLimit {
    DiskLimit::Bounded {
        mebibytes: NonZeroU32::new(mebibytes).expect("nonzero mebibytes"),
    }
}

fn seconds(value: u64) -> LifetimeLimit {
    LifetimeLimit::Bounded {
        seconds: NonZeroU64::new(value).expect("nonzero seconds"),
    }
}

fn profile(egress: EgressPolicy, idle: LifetimeLimit, hard: LifetimeLimit) -> SandboxProfile {
    SandboxProfile::new(cpu(2000), memory(2048), disk(4096), egress, idle, hard)
        .expect("profile should validate")
}

fn snapshot(revision: &str, profile: SandboxProfile) -> SandboxPolicySnapshot {
    SandboxPolicySnapshot::new(revision, profile).expect("snapshot should validate")
}

fn unrestricted_profile() -> SandboxProfile {
    profile(EgressPolicy::Unrestricted, seconds(600), seconds(3600))
}

#[test]
fn sandbox_profile_rejects_zero_and_missing_dimensions() {
    // Pins: stale partial/legacy sandbox JSON fails instead of deserializing
    // a zero or an absent dimension into an unbounded sandbox.
    let zero_cpu = r#"{"cpu":{"mode":"bounded","millicores":0},
        "memory":{"mode":"bounded","mebibytes":512},
        "ephemeral_disk":{"mode":"unbounded"},"egress":{"mode":"deny_all"},
        "idle_timeout":{"mode":"bounded","seconds":60},
        "max_lifetime":{"mode":"bounded","seconds":600}}"#;
    assert!(serde_json::from_str::<SandboxProfile>(zero_cpu).is_err());

    let missing_disk = r#"{"cpu":{"mode":"unbounded"},"memory":{"mode":"unbounded"},
        "egress":{"mode":"deny_all"},"idle_timeout":{"mode":"unbounded"},
        "max_lifetime":{"mode":"unbounded"}}"#;
    assert!(serde_json::from_str::<SandboxProfile>(missing_disk).is_err());

    // The pre-6.5 shape carried cpu_millicores/memory_mb and bare durations.
    let legacy = r#"{"cpu_millicores":0,"memory_mb":0,"idle_timeout":{"secs":300,"nanos":0},
        "max_lifetime":{"secs":300,"nanos":0}}"#;
    assert!(serde_json::from_str::<SandboxProfile>(legacy).is_err());
}

#[test]
fn sandbox_profile_rejects_idle_longer_than_hard_lifetime() {
    // Pins: an idle deadline that can outlive the hard deadline is refused
    // at construction and on deserialization, including the unbounded case.
    assert!(
        SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::DenyAll,
            seconds(601),
            seconds(600),
        )
        .is_err()
    );
    assert!(
        SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::DenyAll,
            LifetimeLimit::Unbounded,
            seconds(600),
        )
        .is_err()
    );
    // Unbounded hard lifetime accepts any idle timeout.
    assert!(
        SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::DenyAll,
            LifetimeLimit::Unbounded,
            LifetimeLimit::Unbounded,
        )
        .is_ok()
    );
}

#[test]
fn egress_allow_list_canonicalizes_and_collapses_to_deny_all() {
    // Pins: allowlists canonicalize (lowercase, sorted, deduplicated) and an
    // allowlist that names nothing becomes DenyAll rather than a policy that
    // claims to restrict while permitting an empty set.
    let policy = EgressPolicy::allow_list(["B.example.com:443", "a.example.com", "A.EXAMPLE.COM"])
        .expect("allowlist should canonicalize");
    let destinations = policy
        .destinations()
        .iter()
        .map(EgressDestination::as_str)
        .collect::<Vec<_>>();
    assert_eq!(destinations, vec!["a.example.com", "b.example.com:443"]);

    assert_eq!(
        EgressPolicy::allow_list(Vec::<String>::new()).expect("empty allowlist"),
        EgressPolicy::DenyAll
    );
    assert!(EgressPolicy::allow_list(["https://a.example.com/x"]).is_err());
    assert!(EgressPolicy::allow_list(["a.example.com:0"]).is_err());
    assert!(EgressPolicy::allow_list(["a.example.com:http"]).is_err());
    assert!(
        serde_json::from_value::<EgressPolicy>(serde_json::json!({
            "mode": "allow_list",
            "revision": "obsolete-inner-revision",
            "destinations": ["a.example.com"]
        }))
        .is_err(),
        "allowlist revision belongs to the surrounding policy snapshot and is not accepted here"
    );
}

#[test]
fn restriction_takes_the_lowest_bound_and_never_widens() {
    // Pins: intersection is restrictive on every dimension — lowest bounded
    // limit wins, Unbounded is the identity, DenyAll dominates, allowlists
    // intersect, and an empty intersection is DenyAll.
    // Every dimension is bounded on both sides so each `restrict` arm that
    // takes a minimum is actually exercised; leaving one side unbounded
    // would let a max-instead-of-min bug survive on that dimension.
    let wide = SandboxProfile::new(
        cpu(4000),
        memory(4096),
        disk(8192),
        EgressPolicy::Unrestricted,
        seconds(900),
        seconds(7200),
    )
    .expect("wide profile");
    let narrow = SandboxProfile::new(
        cpu(500),
        memory(1024),
        disk(2048),
        EgressPolicy::allow_list(["a.example.com"]).expect("allowlist"),
        seconds(300),
        seconds(1800),
    )
    .expect("narrow profile");

    let resolved = wide.clone().restrict(narrow.clone());
    assert_eq!(resolved.cpu, cpu(500));
    assert_eq!(resolved.memory, memory(1024));
    assert_eq!(resolved.ephemeral_disk, disk(2048));
    assert_eq!(resolved.egress.mode(), EgressMode::AllowList);
    assert_eq!(resolved.idle_timeout, seconds(300));
    assert_eq!(resolved.max_lifetime, seconds(1800));
    // The wide side's larger bounds must not appear anywhere in the result.
    assert_ne!(resolved.cpu, cpu(4000));
    assert_ne!(resolved.memory, memory(4096));
    assert_ne!(resolved.ephemeral_disk, disk(8192));
    assert_ne!(resolved.idle_timeout, seconds(900));
    assert_ne!(resolved.max_lifetime, seconds(7200));
    // Intersection is order-independent, so no layer ordering can widen it.
    assert_eq!(resolved, narrow.clone().restrict(wide));

    let denied = narrow.clone().restrict(
        SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::DenyAll,
            LifetimeLimit::Unbounded,
            LifetimeLimit::Unbounded,
        )
        .expect("deny profile"),
    );
    assert_eq!(denied.egress, EgressPolicy::DenyAll);

    let disjoint = narrow.restrict(
        SandboxProfile::new(
            CpuLimit::Unbounded,
            MemoryLimit::Unbounded,
            DiskLimit::Unbounded,
            EgressPolicy::allow_list(["b.example.com"]).expect("allowlist"),
            LifetimeLimit::Unbounded,
            LifetimeLimit::Unbounded,
        )
        .expect("disjoint profile"),
    );
    assert_eq!(
        disjoint.egress,
        EgressPolicy::DenyAll,
        "an empty allowlist intersection permits nothing"
    );
}

#[test]
fn restriction_keeps_idle_at_or_below_the_intersected_hard_lifetime() {
    // Pins: intersecting a layer with a long idle timeout and an unbounded
    // hard lifetime against a layer with a short hard lifetime lowers the
    // hard deadline and leaves idle at or below it — the invariant survives
    // intersection without any layer raising a deadline.
    let long_idle = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(900),
        LifetimeLimit::Unbounded,
    )
    .expect("long-idle profile");
    let short_hard = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(120),
        seconds(300),
    )
    .expect("short-hard profile");

    let resolved = long_idle.restrict(short_hard);
    assert_eq!(resolved.max_lifetime, seconds(300));
    assert_eq!(resolved.idle_timeout, seconds(120));
    resolved
        .validate()
        .expect("intersection preserves the idle/hard invariant");
}

#[test]
fn effective_profile_hash_covers_every_source_and_capability_revision() {
    // Pins: all five policy-layer revisions and the provider capability
    // revision are hash-significant, so changing any one of them changes
    // the sandbox's policy identity.
    let base = resolve_effective_sandbox_profile(
        &snapshot("deploy-1", unrestricted_profile()),
        &snapshot("tenant-1", unrestricted_profile()),
        &snapshot("agent-1", unrestricted_profile()),
        &snapshot("route-1", unrestricted_profile()),
        &snapshot("origin-1", unrestricted_profile()),
        "cap-1",
    )
    .expect("resolve base");

    for (deployment, tenant, agent, route, origin, capability) in [
        (
            "deploy-2", "tenant-1", "agent-1", "route-1", "origin-1", "cap-1",
        ),
        (
            "deploy-1", "tenant-2", "agent-1", "route-1", "origin-1", "cap-1",
        ),
        (
            "deploy-1", "tenant-1", "agent-2", "route-1", "origin-1", "cap-1",
        ),
        (
            "deploy-1", "tenant-1", "agent-1", "route-2", "origin-1", "cap-1",
        ),
        (
            "deploy-1", "tenant-1", "agent-1", "route-1", "origin-2", "cap-1",
        ),
        (
            "deploy-1", "tenant-1", "agent-1", "route-1", "origin-1", "cap-2",
        ),
    ] {
        let changed = resolve_effective_sandbox_profile(
            &snapshot(deployment, unrestricted_profile()),
            &snapshot(tenant, unrestricted_profile()),
            &snapshot(agent, unrestricted_profile()),
            &snapshot(route, unrestricted_profile()),
            &snapshot(origin, unrestricted_profile()),
            capability,
        )
        .expect("resolve changed");
        assert_ne!(
            changed.profile_hash(),
            base.profile_hash(),
            "revision change must change the policy identity hash"
        );
    }

    // Same inputs, same hash — identity is stable, not incidental.
    let repeat = resolve_effective_sandbox_profile(
        &snapshot("deploy-1", unrestricted_profile()),
        &snapshot("tenant-1", unrestricted_profile()),
        &snapshot("agent-1", unrestricted_profile()),
        &snapshot("route-1", unrestricted_profile()),
        &snapshot("origin-1", unrestricted_profile()),
        "cap-1",
    )
    .expect("resolve repeat");
    assert_eq!(repeat.profile_hash(), base.profile_hash());
    assert!(base.profile_hash().starts_with("sha256:"));
}

#[test]
fn generated_code_and_experiments_bind_deny_all_egress_with_distinct_identities() {
    // Pins: the origin layer is a real policy layer, not a label.
    // Experiment trials and generated code both bind `DenyAll`, while their
    // distinct revisions prevent a sandbox admitted for one provenance
    // from being reused for the other.
    let resolve = |origin: CallOrigin| {
        resolve_effective_sandbox_profile(
            &snapshot("deploy-1", unrestricted_profile()),
            &snapshot("tenant-1", unrestricted_profile()),
            &snapshot("agent-1", unrestricted_profile()),
            &snapshot("route-1", unrestricted_profile()),
            &SandboxPolicySnapshot::origin(origin),
            "cap-1",
        )
        .expect("resolve")
    };
    let trial = CallOrigin::Experiment {
        run_uid: uuid::Uuid::nil(),
        trial_uid: None,
    };

    let production = resolve(CallOrigin::Production);
    assert_eq!(production.profile().egress, EgressPolicy::Unrestricted);
    assert_eq!(production.sources().origin, "origin-production");

    let generated = resolve(CallOrigin::GeneratedCode);
    assert_eq!(
        generated.profile().egress,
        EgressPolicy::DenyAll,
        "generated-code sandboxes must resolve to deny-all egress"
    );

    let experiment = resolve(trial);
    assert_eq!(
        experiment.profile().egress,
        EgressPolicy::DenyAll,
        "experiment sandboxes must resolve to deny-all egress"
    );
    assert_eq!(experiment.sources().origin, "origin-experiment-deny-all");

    // Every origin is hash-significant even where two share a profile: the
    // revision string differs, so the resolved identities cannot collide.
    for (label, hash) in [
        ("generated code", generated.profile_hash()),
        ("experiment", experiment.profile_hash()),
    ] {
        assert_ne!(
            hash,
            production.profile_hash(),
            "the {label} origin layer must be hash-significant"
        );
    }
    assert_ne!(
        generated.profile_hash(),
        experiment.profile_hash(),
        "a trial sandbox must never be reusable to serve generated code"
    );
}

#[test]
fn a_permissive_origin_layer_cannot_widen_a_denied_one() {
    // Pins: the origin layer participates in the same restrictive
    // intersection as the other four. A production origin composed with a
    // tenant that denied egress still denies it.
    let resolved = resolve_effective_sandbox_profile(
        &snapshot("deploy-1", unrestricted_profile()),
        &snapshot(
            "tenant-1",
            profile(EgressPolicy::DenyAll, seconds(60), seconds(120)),
        ),
        &snapshot("agent-1", unrestricted_profile()),
        &snapshot("route-1", unrestricted_profile()),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "cap-1",
    )
    .expect("resolve");
    assert_eq!(resolved.profile().egress, EgressPolicy::DenyAll);
}

#[test]
fn effective_profile_deserialization_rejects_a_tampered_hash() {
    // Pins: a persisted effective profile whose stored hash disagrees with
    // its profile and revisions fails to load rather than being trusted.
    let resolved = resolve_effective_sandbox_profile(
        &snapshot("deploy-1", unrestricted_profile()),
        &snapshot("tenant-1", unrestricted_profile()),
        &snapshot("agent-1", unrestricted_profile()),
        &snapshot("route-1", unrestricted_profile()),
        &snapshot("origin-1", unrestricted_profile()),
        "cap-1",
    )
    .expect("resolve");
    let encoded = serde_json::to_string(&resolved).expect("serialize");
    let round_tripped = serde_json::from_str::<EffectiveSandboxProfile>(&encoded)
        .expect("clean payload round-trips");
    assert_eq!(round_tripped, resolved);

    let tampered = encoded.replace(
        "\"capability_revision\":\"cap-1\"",
        "\"capability_revision\":\"cap-9\"",
    );
    assert_ne!(tampered, encoded);
    assert!(
        serde_json::from_str::<EffectiveSandboxProfile>(&tampered).is_err(),
        "a revision changed without rehashing must not deserialize"
    );
}

#[test]
fn resolution_requires_a_capability_revision() {
    // Pins: absence of the provider capability revision is an error rather
    // than an unnamed provider silently entering the policy identity.
    assert!(
        resolve_effective_sandbox_profile(
            &snapshot("deploy-1", unrestricted_profile()),
            &snapshot("tenant-1", unrestricted_profile()),
            &snapshot("agent-1", unrestricted_profile()),
            &snapshot("route-1", unrestricted_profile()),
            &snapshot("origin-1", unrestricted_profile()),
            "   ",
        )
        .is_err()
    );
    assert!(SandboxPolicySnapshot::new("  ", unrestricted_profile()).is_err());
}

#[test]
fn capabilities_reject_every_dimension_the_provider_cannot_enforce() {
    // Pins: admission refuses an unsupported tier, an unbounded request on a
    // provider that must bound, an out-of-range or off-granularity bound, an
    // unsupported egress mode, and a bounded deadline with no owner.
    let container = SandboxTierCapabilities {
        tier: SandboxTier::Container,
        cpu: ResourceSupport::bounded_range(500, 4000, 500).expect("cpu range"),
        memory: ResourceSupport {
            allows_unbounded: false,
            bounded: ResourceSupport::bounded_range(256, 8192, 256)
                .expect("memory range")
                .bounded,
        },
        ephemeral_disk: ResourceSupport::unbounded_only(),
        egress_modes: vec![EgressMode::DenyAll],
        idle_enforcement: DeadlineEnforcement::Provider,
        max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
    };
    let capabilities = HandProviderCapabilities {
        revision: "cap-1".to_string(),
        tiers: vec![container.clone()],
    };
    let ok = SandboxProfile::new(
        cpu(1000),
        memory(1024),
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(300),
        seconds(3600),
    )
    .expect("admissible profile");
    capabilities
        .admit(SandboxTier::Container, &ok, true)
        .expect("supported profile is admitted");

    assert!(capabilities.admit(SandboxTier::Local, &ok, true).is_err());
    assert!(
        capabilities
            .admit(SandboxTier::Container, &ok, false)
            .is_err(),
        "a bounded hard lifetime owned by the reaper needs the reaper installed"
    );

    let unbounded_memory = SandboxProfile::new(
        cpu(1000),
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(300),
        seconds(3600),
    )
    .expect("profile");
    assert!(
        capabilities
            .admit(SandboxTier::Container, &unbounded_memory, true)
            .is_err()
    );

    let off_granularity = SandboxProfile::new(
        cpu(750),
        memory(1024),
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(300),
        seconds(3600),
    )
    .expect("profile");
    assert!(
        capabilities
            .admit(SandboxTier::Container, &off_granularity, true)
            .is_err()
    );

    let out_of_range = SandboxProfile::new(
        cpu(8000),
        memory(1024),
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(300),
        seconds(3600),
    )
    .expect("profile");
    assert!(
        capabilities
            .admit(SandboxTier::Container, &out_of_range, true)
            .is_err()
    );

    let bounded_disk = SandboxProfile::new(
        cpu(1000),
        memory(1024),
        disk(2048),
        EgressPolicy::DenyAll,
        seconds(300),
        seconds(3600),
    )
    .expect("profile");
    assert!(
        capabilities
            .admit(SandboxTier::Container, &bounded_disk, true)
            .is_err(),
        "a provider that cannot bound disk must reject a bounded disk request"
    );

    let allow_list = SandboxProfile::new(
        cpu(1000),
        memory(1024),
        DiskLimit::Unbounded,
        EgressPolicy::allow_list(["a.example.com"]).expect("allowlist"),
        seconds(300),
        seconds(3600),
    )
    .expect("profile");
    assert!(
        capabilities
            .admit(SandboxTier::Container, &allow_list, true)
            .is_err(),
        "a provider that only enforces deny-all must reject an allowlist"
    );

    let no_owner = HandProviderCapabilities {
        revision: "cap-1".to_string(),
        tiers: vec![SandboxTierCapabilities {
            idle_enforcement: DeadlineEnforcement::None,
            ..container
        }],
    };
    assert!(
        no_owner.admit(SandboxTier::Container, &ok, true).is_err(),
        "a bounded idle timeout with no destruction owner is inadmissible"
    );
}

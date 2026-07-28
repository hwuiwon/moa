//! Hand provisioning and sandbox lifecycle types.
//!
//! The centre of this module is [`SandboxProfile`]: the six dimensions —
//! CPU, memory, ephemeral disk, egress, idle timeout, and maximum lifetime —
//! that decide how much of the machine a provisioned sandbox may consume and
//! for how long. Every dimension is required and typed, so a caller either
//! states a bounded nonzero limit or states `Unbounded` on purpose. There is no
//! `Default`, no `Option`, no zero-means-unlimited sentinel, and no
//! `serde(default)`: a profile that fails to say something does not
//! deserialize.
//!
//! Four policy layers each contribute a [`SandboxPolicySnapshot`] — deployment
//! configuration, the tenant's current configuration, the agent snapshot pinned
//! on the session, and the hand route serving the tool. They are combined by
//! [`resolve_effective_sandbox_profile`] into one [`EffectiveSandboxProfile`]
//! whose limits are the restrictive intersection of all four, and whose
//! [`EffectiveSandboxProfile::profile_hash`] covers the profile, all four source
//! revisions, and the serving provider's capability revision. That hash is the
//! sandbox's policy identity: it is persisted on the durable lease and
//! recomputed on recovery, so a sandbox provisioned under one policy can never
//! be reused under another.

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_canonical_json::CanonicalFormatter;
use sha2::{Digest, Sha256};

use crate::error::{MoaError, Result};

/// Sandbox isolation tier for a hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    /// No sandbox.
    None,
    /// Container sandbox.
    Container,
    /// `MicroVM` sandbox.
    MicroVM,
    /// Direct host execution.
    Local,
}

impl SandboxTier {
    /// Returns the stable persisted/telemetry label for this tier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Container => "container",
            Self::MicroVM => "microvm",
            Self::Local => "local",
        }
    }

    /// Parses a persisted tier label.
    pub fn from_label(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "container" => Ok(Self::Container),
            "microvm" => Ok(Self::MicroVM),
            "local" => Ok(Self::Local),
            other => Err(MoaError::ValidationError(format!(
                "unknown sandbox tier: {other}"
            ))),
        }
    }
}

/// CPU allocation for one provisioned sandbox.
///
/// `Unbounded` is a deliberate declaration that the sandbox may use whatever
/// CPU the host has, which is how local development is expressed without
/// smuggling a zero or a missing field through the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CpuLimit {
    /// A bounded, nonzero CPU allocation.
    Bounded {
        /// Millicores, where 1000 millicores is one full core.
        millicores: NonZeroU32,
    },
    /// Deliberately unbounded CPU.
    Unbounded,
}

impl CpuLimit {
    /// Returns the bounded millicore count, or `None` when unbounded.
    #[must_use]
    pub fn bounded_millicores(self) -> Option<NonZeroU32> {
        match self {
            Self::Bounded { millicores } => Some(millicores),
            Self::Unbounded => None,
        }
    }

    /// Returns the more restrictive of two CPU limits.
    ///
    /// `Unbounded` is the identity element: intersecting it with anything
    /// yields the other side, so a layer that declines to bound CPU can never
    /// raise a bound another layer set.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded, keep) | (keep, Self::Unbounded) => keep,
            (Self::Bounded { millicores: left }, Self::Bounded { millicores: right }) => {
                Self::Bounded {
                    millicores: left.min(right),
                }
            }
        }
    }
}

/// Memory allocation for one provisioned sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum MemoryLimit {
    /// A bounded, nonzero memory allocation.
    Bounded {
        /// Mebibytes of resident memory.
        mebibytes: NonZeroU32,
    },
    /// Deliberately unbounded memory.
    Unbounded,
}

impl MemoryLimit {
    /// Returns the bounded mebibyte count, or `None` when unbounded.
    #[must_use]
    pub fn bounded_mebibytes(self) -> Option<NonZeroU32> {
        match self {
            Self::Bounded { mebibytes } => Some(mebibytes),
            Self::Unbounded => None,
        }
    }

    /// Returns the more restrictive of two memory limits.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded, keep) | (keep, Self::Unbounded) => keep,
            (Self::Bounded { mebibytes: left }, Self::Bounded { mebibytes: right }) => {
                Self::Bounded {
                    mebibytes: left.min(right),
                }
            }
        }
    }
}

/// Ephemeral (scratch) disk allocation for one provisioned sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DiskLimit {
    /// A bounded, nonzero ephemeral disk allocation.
    Bounded {
        /// Mebibytes of ephemeral disk.
        mebibytes: NonZeroU32,
    },
    /// Deliberately unbounded ephemeral disk.
    Unbounded,
}

impl DiskLimit {
    /// Returns the bounded mebibyte count, or `None` when unbounded.
    #[must_use]
    pub fn bounded_mebibytes(self) -> Option<NonZeroU32> {
        match self {
            Self::Bounded { mebibytes } => Some(mebibytes),
            Self::Unbounded => None,
        }
    }

    /// Returns the more restrictive of two ephemeral disk limits.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded, keep) | (keep, Self::Unbounded) => keep,
            (Self::Bounded { mebibytes: left }, Self::Bounded { mebibytes: right }) => {
                Self::Bounded {
                    mebibytes: left.min(right),
                }
            }
        }
    }
}

/// A sandbox deadline: idle timeout or maximum lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LifetimeLimit {
    /// A bounded, nonzero deadline.
    Bounded {
        /// Whole seconds until the deadline fires.
        seconds: NonZeroU64,
    },
    /// Deliberately unbounded: the deadline never fires on its own.
    Unbounded,
}

impl LifetimeLimit {
    /// Returns the bounded second count, or `None` when unbounded.
    #[must_use]
    pub fn bounded_seconds(self) -> Option<NonZeroU64> {
        match self {
            Self::Bounded { seconds } => Some(seconds),
            Self::Unbounded => None,
        }
    }

    /// Returns the more restrictive of two deadlines.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unbounded, keep) | (keep, Self::Unbounded) => keep,
            (Self::Bounded { seconds: left }, Self::Bounded { seconds: right }) => Self::Bounded {
                seconds: left.min(right),
            },
        }
    }

    /// Returns whether this deadline is strictly longer than `hard`.
    ///
    /// An unbounded idle timeout is longer than any bounded hard lifetime,
    /// which is why the comparison is not a plain numeric one.
    #[must_use]
    fn exceeds(self, hard: Self) -> bool {
        match (self, hard) {
            (_, Self::Unbounded) => false,
            (Self::Unbounded, Self::Bounded { .. }) => true,
            (Self::Bounded { seconds: idle }, Self::Bounded { seconds: hard }) => idle > hard,
        }
    }
}

/// One canonical outbound network destination in an egress allowlist.
///
/// Canonical form is a lowercase host with an optional `:port` suffix. The
/// newtype exists so a malformed destination is rejected once, at the type
/// boundary, instead of being discovered by whichever adapter happens to parse
/// it first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EgressDestination(String);

impl EgressDestination {
    /// Parses and canonicalizes one `host` or `host:port` destination.
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(MoaError::ValidationError(
                "egress destination must not be empty".to_string(),
            ));
        }
        let (host, port) = match trimmed.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (trimmed, None),
        };
        if host.is_empty() {
            return Err(MoaError::ValidationError(format!(
                "egress destination `{value}` has an empty host"
            )));
        }
        if host.contains(['/', ' ', '\\', '@', '?', '#']) {
            return Err(MoaError::ValidationError(format!(
                "egress destination `{value}` must be a bare host, not a URL"
            )));
        }
        if let Some(port) = port {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(MoaError::ValidationError(format!(
                    "egress destination `{value}` has a non-numeric port"
                )));
            }
            let parsed = port.parse::<u16>().map_err(|_| {
                MoaError::ValidationError(format!(
                    "egress destination `{value}` has an out-of-range port"
                ))
            })?;
            if parsed == 0 {
                return Err(MoaError::ValidationError(format!(
                    "egress destination `{value}` has port 0"
                )));
            }
            return Ok(Self(format!("{}:{parsed}", host.to_ascii_lowercase())));
        }
        Ok(Self(host.to_ascii_lowercase()))
    }

    /// Returns the canonical destination string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EgressDestination {
    type Error = MoaError;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<EgressDestination> for String {
    fn from(value: EgressDestination) -> Self {
        value.0
    }
}

/// The three egress postures a sandbox profile can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    /// No outbound network at all.
    DenyAll,
    /// Outbound network restricted to an explicit destination allowlist.
    AllowList,
    /// Unrestricted outbound network.
    Unrestricted,
}

impl EgressMode {
    /// Returns the stable telemetry label for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DenyAll => "deny_all",
            Self::AllowList => "allow_list",
            Self::Unrestricted => "unrestricted",
        }
    }
}

/// Outbound network policy for one provisioned sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressPolicy {
    /// No outbound network at all. Dominates every intersection.
    DenyAll,
    /// Outbound network restricted to a canonical destination allowlist.
    AllowList {
        /// Revision identifying the authored allowlist, part of policy identity.
        revision: String,
        /// Sorted, deduplicated canonical destinations.
        destinations: Vec<EgressDestination>,
    },
    /// Unrestricted outbound network. The identity element of intersection.
    Unrestricted,
}

impl EgressPolicy {
    /// Builds a canonical allowlist policy.
    ///
    /// Destinations are canonicalized, sorted, and deduplicated. An allowlist
    /// that names nothing permits nothing, so it collapses to
    /// [`EgressPolicy::DenyAll`] rather than becoming a policy that says
    /// "restricted" while enforcing nothing.
    pub fn allow_list<I, S>(revision: &str, destinations: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if revision.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "egress allowlist revision must not be empty".to_string(),
            ));
        }
        let mut parsed = destinations
            .into_iter()
            .map(|destination| EgressDestination::parse(destination.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        parsed.sort();
        parsed.dedup();
        if parsed.is_empty() {
            return Ok(Self::DenyAll);
        }
        Ok(Self::AllowList {
            revision: revision.trim().to_string(),
            destinations: parsed,
        })
    }

    /// Returns the posture this policy declares.
    #[must_use]
    pub fn mode(&self) -> EgressMode {
        match self {
            Self::DenyAll => EgressMode::DenyAll,
            Self::AllowList { .. } => EgressMode::AllowList,
            Self::Unrestricted => EgressMode::Unrestricted,
        }
    }

    /// Returns the allowlist revision when this policy is an allowlist.
    #[must_use]
    pub fn allow_list_revision(&self) -> Option<&str> {
        match self {
            Self::AllowList { revision, .. } => Some(revision.as_str()),
            Self::DenyAll | Self::Unrestricted => None,
        }
    }

    /// Returns the canonical destinations when this policy is an allowlist.
    #[must_use]
    pub fn destinations(&self) -> &[EgressDestination] {
        match self {
            Self::AllowList { destinations, .. } => destinations,
            Self::DenyAll | Self::Unrestricted => &[],
        }
    }

    /// Returns the more restrictive of two egress policies.
    ///
    /// `DenyAll` dominates, `Unrestricted` is the identity element, and two
    /// allowlists intersect. The resolved allowlist revision is the sorted,
    /// `+`-joined set of contributing revisions, so a resolved policy names
    /// every authored allowlist that shaped it. An empty intersection permits
    /// nothing and therefore becomes `DenyAll`.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::DenyAll, _) | (_, Self::DenyAll) => Self::DenyAll,
            (Self::Unrestricted, keep) | (keep, Self::Unrestricted) => keep,
            (
                Self::AllowList {
                    revision: left_revision,
                    destinations: left,
                },
                Self::AllowList {
                    revision: right_revision,
                    destinations: right,
                },
            ) => {
                let destinations = left
                    .into_iter()
                    .filter(|destination| right.contains(destination))
                    .collect::<Vec<_>>();
                if destinations.is_empty() {
                    return Self::DenyAll;
                }
                let mut revisions = [left_revision, right_revision];
                revisions.sort();
                let revision = if revisions[0] == revisions[1] {
                    revisions[0].clone()
                } else {
                    format!("{}+{}", revisions[0], revisions[1])
                };
                Self::AllowList {
                    revision,
                    destinations,
                }
            }
        }
    }
}

/// Deserialization mirror for [`EgressPolicy`] that runs canonicalization.
#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum EgressPolicyFields {
    DenyAll,
    AllowList {
        revision: String,
        destinations: Vec<String>,
    },
    Unrestricted,
}

impl<'de> Deserialize<'de> for EgressPolicy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match EgressPolicyFields::deserialize(deserializer)? {
            EgressPolicyFields::DenyAll => Ok(Self::DenyAll),
            EgressPolicyFields::Unrestricted => Ok(Self::Unrestricted),
            EgressPolicyFields::AllowList {
                revision,
                destinations,
            } => Self::allow_list(&revision, destinations).map_err(serde::de::Error::custom),
        }
    }
}

/// The six-dimension sandbox resource and egress profile.
///
/// Every field is required. Construct with [`SandboxProfile::new`] so the
/// idle-versus-hard-lifetime invariant is checked once; deserialization runs
/// the same check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxProfile {
    /// CPU allocation.
    pub cpu: CpuLimit,
    /// Resident memory allocation.
    pub memory: MemoryLimit,
    /// Ephemeral scratch disk allocation.
    pub ephemeral_disk: DiskLimit,
    /// Outbound network policy.
    pub egress: EgressPolicy,
    /// Idle timeout: how long the sandbox may sit without a tool call.
    pub idle_timeout: LifetimeLimit,
    /// Hard maximum lifetime, which renewal can never extend.
    pub max_lifetime: LifetimeLimit,
}

impl SandboxProfile {
    /// Builds a validated profile.
    ///
    /// Rejects an idle timeout longer than the hard maximum lifetime: an idle
    /// deadline that can outlive the hard one is not a deadline.
    pub fn new(
        cpu: CpuLimit,
        memory: MemoryLimit,
        ephemeral_disk: DiskLimit,
        egress: EgressPolicy,
        idle_timeout: LifetimeLimit,
        max_lifetime: LifetimeLimit,
    ) -> Result<Self> {
        let profile = Self {
            cpu,
            memory,
            ephemeral_disk,
            egress,
            idle_timeout,
            max_lifetime,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Validates the idle-versus-hard-lifetime invariant.
    pub fn validate(&self) -> Result<()> {
        if self.idle_timeout.exceeds(self.max_lifetime) {
            return Err(MoaError::ValidationError(
                "sandbox idle timeout must not exceed the maximum lifetime".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the restrictive intersection of two profiles.
    ///
    /// Each dimension takes the lower bound and `DenyAll` egress dominates, so
    /// intersection only ever tightens: no combination of layers can widen a
    /// limit any single layer set. Both operands are validated by construction,
    /// which is what keeps the idle-versus-hard invariant true of the result —
    /// each side's idle already sits at or below its own hard limit, so the
    /// minimum idle sits at or below the minimum hard limit too.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        Self {
            cpu: self.cpu.restrict(other.cpu),
            memory: self.memory.restrict(other.memory),
            ephemeral_disk: self.ephemeral_disk.restrict(other.ephemeral_disk),
            egress: self.egress.restrict(other.egress),
            idle_timeout: self.idle_timeout.restrict(other.idle_timeout),
            max_lifetime: self.max_lifetime.restrict(other.max_lifetime),
        }
    }
}

/// Deserialization mirror for [`SandboxProfile`] that runs validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxProfileFields {
    cpu: CpuLimit,
    memory: MemoryLimit,
    ephemeral_disk: DiskLimit,
    egress: EgressPolicy,
    idle_timeout: LifetimeLimit,
    max_lifetime: LifetimeLimit,
}

impl<'de> Deserialize<'de> for SandboxProfile {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = SandboxProfileFields::deserialize(deserializer)?;
        Self::new(
            fields.cpu,
            fields.memory,
            fields.ephemeral_disk,
            fields.egress,
            fields.idle_timeout,
            fields.max_lifetime,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// One policy layer's contribution: a revision plus the profile it declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxPolicySnapshot {
    /// Revision identifying this layer's authored policy. Never empty.
    pub revision: String,
    /// The profile this layer declares.
    pub profile: SandboxProfile,
}

/// A policy layer MOA itself contributes when no one authored one.
///
/// These are not "missing" layers. Each is the identity element of the
/// restrictive intersection — it can never widen what another layer bounded —
/// and each carries a distinct, non-empty revision into the policy identity
/// hash, so the day a layer starts declaring real limits the hash changes and
/// no sandbox provisioned under the identity layer can be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinPolicyRevision {
    /// The tenant has authored no sandbox policy.
    TenantUnset,
    /// The pinned agent declares no sandbox policy.
    AgentUnset,
    /// The serving hand route has no authored sandbox policy.
    RouteUnset,
}

impl BuiltinPolicyRevision {
    /// Returns the stable, non-empty revision string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TenantUnset => "tenant-sandbox-unset",
            Self::AgentUnset => "agent-sandbox-unset",
            Self::RouteUnset => "route-sandbox-unset",
        }
    }
}

impl SandboxProfile {
    /// The fully permissive profile: the identity element of intersection.
    ///
    /// Every dimension is an explicit `Unbounded`/`Unrestricted`, so this is a
    /// stated profile rather than an absent one. It is trivially valid — an
    /// unbounded idle timeout cannot exceed an unbounded hard lifetime — which
    /// is why it needs no fallible constructor.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            cpu: CpuLimit::Unbounded,
            memory: MemoryLimit::Unbounded,
            ephemeral_disk: DiskLimit::Unbounded,
            egress: EgressPolicy::Unrestricted,
            idle_timeout: LifetimeLimit::Unbounded,
            max_lifetime: LifetimeLimit::Unbounded,
        }
    }
}

impl SandboxPolicySnapshot {
    /// Builds the non-restricting layer MOA contributes for `revision`.
    #[must_use]
    pub fn builtin(revision: BuiltinPolicyRevision) -> Self {
        Self {
            revision: revision.as_str().to_string(),
            profile: SandboxProfile::unrestricted(),
        }
    }

    /// Builds a validated policy snapshot.
    pub fn new(revision: &str, profile: SandboxProfile) -> Result<Self> {
        if revision.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "sandbox policy snapshot revision must not be empty".to_string(),
            ));
        }
        Ok(Self {
            revision: revision.trim().to_string(),
            profile,
        })
    }
}

/// Deserialization mirror for [`SandboxPolicySnapshot`] that runs validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicySnapshotFields {
    revision: String,
    profile: SandboxProfile,
}

impl<'de> Deserialize<'de> for SandboxPolicySnapshot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = SandboxPolicySnapshotFields::deserialize(deserializer)?;
        Self::new(&fields.revision, fields.profile).map_err(serde::de::Error::custom)
    }
}

/// The four policy-layer revisions that produced an effective profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicySources {
    /// Deployment configuration revision.
    pub deployment: String,
    /// Current tenant configuration revision.
    pub tenant: String,
    /// Pinned agent policy snapshot revision.
    pub agent: String,
    /// Serving hand route revision.
    pub route: String,
}

/// Identity payload hashed into [`EffectiveSandboxProfile::profile_hash`].
///
/// Serialized field order is the declaration order, and every nested type has a
/// fixed field order with sorted allowlists, so this serialization is canonical
/// without a separate normalizer.
#[derive(Serialize)]
struct EffectiveProfileIdentity<'a> {
    profile: &'a SandboxProfile,
    sources: &'a SandboxPolicySources,
    capability_revision: &'a str,
}

/// One fully resolved sandbox policy, carrying its own identity hash.
///
/// Fields are private and the only constructor is
/// [`resolve_effective_sandbox_profile`], so the hash cannot drift from the
/// profile and revisions it covers. Deserialization recomputes the hash and
/// rejects a payload whose stored hash disagrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveSandboxProfile {
    profile: SandboxProfile,
    sources: SandboxPolicySources,
    capability_revision: String,
    profile_hash: String,
}

impl EffectiveSandboxProfile {
    /// Returns the resolved six-dimension profile.
    #[must_use]
    pub fn profile(&self) -> &SandboxProfile {
        &self.profile
    }

    /// Returns the four contributing policy-layer revisions.
    #[must_use]
    pub fn sources(&self) -> &SandboxPolicySources {
        &self.sources
    }

    /// Returns the serving provider's capability revision.
    #[must_use]
    pub fn capability_revision(&self) -> &str {
        &self.capability_revision
    }

    /// Returns the stable `sha256:`-prefixed policy identity hash.
    #[must_use]
    pub fn profile_hash(&self) -> &str {
        &self.profile_hash
    }

    /// Returns the canonical JSON serialization the hash is taken over.
    pub fn canonical_identity_json(&self) -> Result<String> {
        canonical_identity_json(&self.profile, &self.sources, &self.capability_revision)
    }
}

/// Serializes the hashed identity payload canonically.
///
/// Struct field order is fixed by declaration and allowlists are sorted at
/// construction; the canonical formatter additionally pins map-key order, so
/// two runs over equal inputs always produce byte-identical output.
fn canonical_identity_json(
    profile: &SandboxProfile,
    sources: &SandboxPolicySources,
    capability_revision: &str,
) -> Result<String> {
    let mut buffer = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut buffer, CanonicalFormatter::new());
    EffectiveProfileIdentity {
        profile,
        sources,
        capability_revision,
    }
    .serialize(&mut serializer)
    .map_err(|error| {
        MoaError::ValidationError(format!(
            "failed to canonically serialize sandbox policy identity: {error}"
        ))
    })?;
    String::from_utf8(buffer).map_err(|error| {
        MoaError::ValidationError(format!(
            "canonical sandbox policy identity was not valid UTF-8: {error}"
        ))
    })
}

/// Computes the `sha256:`-prefixed hash of a canonical identity payload.
fn hash_identity(canonical: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Resolves the four policy layers plus provider capability revision into one
/// effective profile.
///
/// The resolved limits are the restrictive intersection of all four layers, so
/// no layer can widen another. Every layer is required: a missing layer is an
/// error, never an inferred unrestricted policy.
pub fn resolve_effective_sandbox_profile(
    deployment: &SandboxPolicySnapshot,
    tenant: &SandboxPolicySnapshot,
    agent: &SandboxPolicySnapshot,
    route: &SandboxPolicySnapshot,
    capability_revision: &str,
) -> Result<EffectiveSandboxProfile> {
    if capability_revision.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "hand provider capability revision must not be empty".to_string(),
        ));
    }
    let profile = deployment
        .profile
        .clone()
        .restrict(tenant.profile.clone())
        .restrict(agent.profile.clone())
        .restrict(route.profile.clone());
    profile.validate()?;
    let sources = SandboxPolicySources {
        deployment: deployment.revision.clone(),
        tenant: tenant.revision.clone(),
        agent: agent.revision.clone(),
        route: route.revision.clone(),
    };
    let capability_revision = capability_revision.trim().to_string();
    let canonical = canonical_identity_json(&profile, &sources, &capability_revision)?;
    Ok(EffectiveSandboxProfile {
        profile,
        sources,
        capability_revision,
        profile_hash: hash_identity(&canonical),
    })
}

/// Deserialization mirror for [`EffectiveSandboxProfile`] that re-derives the hash.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveSandboxProfileFields {
    profile: SandboxProfile,
    sources: SandboxPolicySources,
    capability_revision: String,
    profile_hash: String,
}

impl<'de> Deserialize<'de> for EffectiveSandboxProfile {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = EffectiveSandboxProfileFields::deserialize(deserializer)?;
        let canonical = canonical_identity_json(
            &fields.profile,
            &fields.sources,
            &fields.capability_revision,
        )
        .map_err(serde::de::Error::custom)?;
        let recomputed = hash_identity(&canonical);
        if recomputed != fields.profile_hash {
            return Err(serde::de::Error::custom(
                "sandbox policy identity hash does not match its profile and revisions",
            ));
        }
        Ok(Self {
            profile: fields.profile,
            sources: fields.sources,
            capability_revision: fields.capability_revision,
            profile_hash: fields.profile_hash,
        })
    }
}

/// Who terminates a sandbox when one of its deadlines fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineEnforcement {
    /// The provider itself stops the sandbox at the deadline.
    Provider,
    /// The durable hand-lease reaper stops it. Admissible only when that
    /// reaper is actually installed on the router serving the call.
    DurableReaper,
    /// Nothing enforces this deadline, so only `Unbounded` is admissible.
    None,
}

impl DeadlineEnforcement {
    /// Returns the stable telemetry label for this owner.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::DurableReaper => "durable_reaper",
            Self::None => "none",
        }
    }
}

/// The bounded range and granularity a provider can enforce for one dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedResourceRange {
    /// Smallest bound the provider accepts.
    pub min: NonZeroU32,
    /// Largest bound the provider accepts.
    pub max: NonZeroU32,
    /// Bounds must be an exact multiple of this granularity.
    pub granularity: NonZeroU32,
}

/// What a provider can do with one resource dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSupport {
    /// Whether the provider can honor an explicit `Unbounded` request.
    pub allows_unbounded: bool,
    /// The bounded range it can enforce; `None` when it cannot bound at all.
    pub bounded: Option<BoundedResourceRange>,
}

impl ResourceSupport {
    /// A dimension the provider can neither bound nor meaningfully report,
    /// so only an explicit `Unbounded` request is admissible.
    #[must_use]
    pub fn unbounded_only() -> Self {
        Self {
            allows_unbounded: true,
            bounded: None,
        }
    }

    /// A dimension the provider can bound within a range, and can also leave
    /// unbounded when the effective profile says so.
    pub fn bounded_range(min: u32, max: u32, granularity: u32) -> Result<Self> {
        let range = BoundedResourceRange {
            min: NonZeroU32::new(min).ok_or_else(|| {
                MoaError::ValidationError("resource range min must be nonzero".to_string())
            })?,
            max: NonZeroU32::new(max).ok_or_else(|| {
                MoaError::ValidationError("resource range max must be nonzero".to_string())
            })?,
            granularity: NonZeroU32::new(granularity).ok_or_else(|| {
                MoaError::ValidationError("resource granularity must be nonzero".to_string())
            })?,
        };
        if range.min > range.max {
            return Err(MoaError::ValidationError(
                "resource range min must not exceed max".to_string(),
            ));
        }
        Ok(Self {
            allows_unbounded: true,
            bounded: Some(range),
        })
    }

    /// Checks one bounded request against this dimension's support.
    fn admit_bounded(&self, dimension: &str, value: NonZeroU32) -> Result<()> {
        let Some(range) = self.bounded else {
            return Err(MoaError::Unsupported(format!(
                "hand provider cannot enforce a bounded {dimension} limit"
            )));
        };
        if value < range.min || value > range.max {
            return Err(MoaError::Unsupported(format!(
                "hand provider supports {dimension} between {} and {}, requested {value}",
                range.min, range.max
            )));
        }
        if !value.get().is_multiple_of(range.granularity.get()) {
            return Err(MoaError::Unsupported(format!(
                "hand provider requires {dimension} in multiples of {}, requested {value}",
                range.granularity
            )));
        }
        Ok(())
    }

    /// Checks an unbounded request against this dimension's support.
    fn admit_unbounded(&self, dimension: &str) -> Result<()> {
        if self.allows_unbounded {
            return Ok(());
        }
        Err(MoaError::Unsupported(format!(
            "hand provider requires a bounded {dimension} limit"
        )))
    }
}

/// What one provider can enforce for one sandbox tier.
///
/// Enforcement is a property of the tier, not only of the provider: the local
/// provider can bound CPU and deny egress in a Docker container and can do
/// neither for a bare host process, and stating one blended answer for both
/// would either forbid what Docker can do or claim what the host cannot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxTierCapabilities {
    /// The tier these capabilities describe.
    pub tier: SandboxTier,
    /// CPU support, in millicores.
    pub cpu: ResourceSupport,
    /// Memory support, in mebibytes.
    pub memory: ResourceSupport,
    /// Ephemeral disk support, in mebibytes.
    pub ephemeral_disk: ResourceSupport,
    /// Egress postures this tier can enforce.
    pub egress_modes: Vec<EgressMode>,
    /// Who enforces the idle timeout.
    pub idle_enforcement: DeadlineEnforcement,
    /// Who enforces the hard maximum lifetime.
    pub max_lifetime_enforcement: DeadlineEnforcement,
}

/// What a [`HandProvider`](crate::traits::HandProvider) can actually enforce.
///
/// This is required of every provider and has no `Default`. A provider that
/// accepts a profile dimension it silently drops is the failure this type
/// exists to make impossible: admission compares the effective profile against
/// these capabilities and refuses before any lease claim or provision call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandProviderCapabilities {
    /// Stable revision of this capability declaration, part of policy identity.
    pub revision: String,
    /// Per-tier capabilities. A tier absent from this list is not served.
    pub tiers: Vec<SandboxTierCapabilities>,
}

impl HandProviderCapabilities {
    /// Rejects an effective profile this provider cannot honor at `tier`.
    ///
    /// `durable_reaper_installed` reports whether the caller actually has the
    /// durable lease reaper running. A provider that relies on the reaper for a
    /// deadline may only serve a bounded deadline when that owner exists, so a
    /// deployment without it fails admission instead of provisioning a sandbox
    /// nothing will ever destroy.
    pub fn admit(
        &self,
        tier: SandboxTier,
        profile: &SandboxProfile,
        durable_reaper_installed: bool,
    ) -> Result<()> {
        let tier_capabilities = self
            .tiers
            .iter()
            .find(|candidate| candidate.tier == tier)
            .ok_or_else(|| {
                MoaError::Unsupported(format!(
                    "hand provider does not support the {} sandbox tier",
                    tier.as_str()
                ))
            })?;
        tier_capabilities.admit(profile, durable_reaper_installed)
    }
}

impl SandboxTierCapabilities {
    /// Rejects an effective profile this tier cannot honor.
    pub fn admit(&self, profile: &SandboxProfile, durable_reaper_installed: bool) -> Result<()> {
        match profile.cpu {
            CpuLimit::Bounded { millicores } => {
                self.cpu.admit_bounded("cpu millicores", millicores)
            }
            CpuLimit::Unbounded => self.cpu.admit_unbounded("cpu"),
        }?;
        match profile.memory {
            MemoryLimit::Bounded { mebibytes } => {
                self.memory.admit_bounded("memory mebibytes", mebibytes)
            }
            MemoryLimit::Unbounded => self.memory.admit_unbounded("memory"),
        }?;
        match profile.ephemeral_disk {
            DiskLimit::Bounded { mebibytes } => self
                .ephemeral_disk
                .admit_bounded("ephemeral disk mebibytes", mebibytes),
            DiskLimit::Unbounded => self.ephemeral_disk.admit_unbounded("ephemeral disk"),
        }?;
        let egress_mode = profile.egress.mode();
        if !self.egress_modes.contains(&egress_mode) {
            return Err(MoaError::Unsupported(format!(
                "hand provider cannot enforce {} egress",
                egress_mode.as_str()
            )));
        }
        admit_deadline(
            "idle timeout",
            profile.idle_timeout,
            self.idle_enforcement,
            durable_reaper_installed,
        )?;
        admit_deadline(
            "maximum lifetime",
            profile.max_lifetime,
            self.max_lifetime_enforcement,
            durable_reaper_installed,
        )
    }
}

/// Rejects a bounded deadline with no installed destruction owner.
fn admit_deadline(
    dimension: &str,
    limit: LifetimeLimit,
    owner: DeadlineEnforcement,
    durable_reaper_installed: bool,
) -> Result<()> {
    if limit.bounded_seconds().is_none() {
        return Ok(());
    }
    match owner {
        DeadlineEnforcement::Provider => Ok(()),
        DeadlineEnforcement::DurableReaper if durable_reaper_installed => Ok(()),
        DeadlineEnforcement::DurableReaper => Err(MoaError::ConfigError(format!(
            "bounded sandbox {dimension} requires the durable hand-lease reaper, which is not installed"
        ))),
        DeadlineEnforcement::None => Err(MoaError::Unsupported(format!(
            "hand provider has no owner that enforces a bounded sandbox {dimension}"
        ))),
    }
}

/// Specification for provisioning a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandSpec {
    /// Required sandbox tier.
    pub sandbox_tier: SandboxTier,
    /// Optional image identifier.
    pub image: Option<String>,
    /// Environment variables passed to the hand.
    pub env: HashMap<String, String>,
    /// Optional workspace mount path.
    pub workspace_mount: Option<PathBuf>,
    /// The one resolved policy every provider must honor or reject.
    pub effective_profile: EffectiveSandboxProfile,
}

/// One trusted file to install into a provisioned sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxFile {
    /// POSIX relative path inside the sandbox.
    pub path: String,
    /// Raw bytes to write at `path`.
    pub content: Vec<u8>,
    /// Whether the file should be executable after installation.
    #[serde(default)]
    pub executable: bool,
}

/// Validates a trusted sandbox file path as a POSIX relative path.
pub fn validate_sandbox_file_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(MoaError::ValidationError(format!(
            "sandbox file path `{path}` must be a POSIX relative path"
        )));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(MoaError::ValidationError(format!(
                "sandbox file path `{path}` contains an invalid segment"
            )));
        }
    }
    Ok(())
}

/// Opaque handle to a provisioned hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandHandle {
    /// Local host execution sandbox.
    Local { sandbox_dir: PathBuf },
    /// Docker container-backed sandbox.
    Docker { container_id: String },
    /// Daytona workspace handle.
    Daytona { workspace_id: String },
    /// E2B sandbox handle.
    E2B { sandbox_id: String },
}

impl HandHandle {
    /// Creates a local hand handle.
    pub fn local(sandbox_dir: PathBuf) -> Self {
        Self::Local { sandbox_dir }
    }

    /// Creates a Docker hand handle.
    pub fn docker(container_id: impl Into<String>) -> Self {
        Self::Docker {
            container_id: container_id.into(),
        }
    }

    /// Creates a Daytona hand handle.
    pub fn daytona(workspace_id: impl Into<String>) -> Self {
        Self::Daytona {
            workspace_id: workspace_id.into(),
        }
    }

    /// Creates an E2B hand handle.
    pub fn e2b(sandbox_id: impl Into<String>) -> Self {
        Self::E2B {
            sandbox_id: sandbox_id.into(),
        }
    }

    /// Returns the Daytona workspace identifier when the handle is Daytona-backed.
    pub fn daytona_id(&self) -> Result<&str> {
        match self {
            Self::Daytona { workspace_id } => Ok(workspace_id.as_str()),
            _ => Err(MoaError::ProviderError(
                "hand handle is not a Daytona workspace".to_string(),
            )),
        }
    }
}

/// Observed lifecycle state of a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandStatus {
    /// Provisioning is in progress.
    Provisioning,
    /// Ready to accept tool calls.
    Running,
    /// Temporarily paused.
    Paused,
    /// Stopped but recoverable.
    Stopped,
    /// Permanently destroyed.
    Destroyed,
    /// Failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let policy = EgressPolicy::allow_list(
            "rev-1",
            ["B.example.com:443", "a.example.com", "A.EXAMPLE.COM"],
        )
        .expect("allowlist should canonicalize");
        let destinations = policy
            .destinations()
            .iter()
            .map(EgressDestination::as_str)
            .collect::<Vec<_>>();
        assert_eq!(destinations, vec!["a.example.com", "b.example.com:443"]);

        assert_eq!(
            EgressPolicy::allow_list("rev-1", Vec::<String>::new()).expect("empty allowlist"),
            EgressPolicy::DenyAll
        );
        assert!(EgressPolicy::allow_list("", ["a.example.com"]).is_err());
        assert!(EgressPolicy::allow_list("rev-1", ["https://a.example.com/x"]).is_err());
        assert!(EgressPolicy::allow_list("rev-1", ["a.example.com:0"]).is_err());
        assert!(EgressPolicy::allow_list("rev-1", ["a.example.com:http"]).is_err());
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
            EgressPolicy::allow_list("rev-a", ["a.example.com"]).expect("allowlist"),
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
                EgressPolicy::allow_list("rev-b", ["b.example.com"]).expect("allowlist"),
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
        // Pins: all four policy-layer revisions and the provider capability
        // revision are hash-significant, so changing any one of them changes
        // the sandbox's policy identity.
        let base = resolve_effective_sandbox_profile(
            &snapshot("deploy-1", unrestricted_profile()),
            &snapshot("tenant-1", unrestricted_profile()),
            &snapshot("agent-1", unrestricted_profile()),
            &snapshot("route-1", unrestricted_profile()),
            "cap-1",
        )
        .expect("resolve base");

        for (deployment, tenant, agent, route, capability) in [
            ("deploy-2", "tenant-1", "agent-1", "route-1", "cap-1"),
            ("deploy-1", "tenant-2", "agent-1", "route-1", "cap-1"),
            ("deploy-1", "tenant-1", "agent-2", "route-1", "cap-1"),
            ("deploy-1", "tenant-1", "agent-1", "route-2", "cap-1"),
            ("deploy-1", "tenant-1", "agent-1", "route-1", "cap-2"),
        ] {
            let changed = resolve_effective_sandbox_profile(
                &snapshot(deployment, unrestricted_profile()),
                &snapshot(tenant, unrestricted_profile()),
                &snapshot(agent, unrestricted_profile()),
                &snapshot(route, unrestricted_profile()),
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
            "cap-1",
        )
        .expect("resolve repeat");
        assert_eq!(repeat.profile_hash(), base.profile_hash());
        assert!(base.profile_hash().starts_with("sha256:"));
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
            EgressPolicy::allow_list("rev-a", ["a.example.com"]).expect("allowlist"),
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
}

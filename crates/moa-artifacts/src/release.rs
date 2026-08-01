//! Release-control types: candidate states, activation targets, evaluation
//! subjects, server-resolved policies, and activation attestations.
//!
//! A tenant change to a skill, action, or agent is an immutable candidate
//! revision. Nothing about validating or storing that candidate makes it
//! visible: what a session resolves is a type-owned serving pointer
//! ([`crate::registry::ServingPointer`] for skills and actions, the agent
//! installation for agents). Moving a pointer requires an unconsumed
//! [`ActivationAttestation`] over an exact [`EvaluationSubjectV1`], and the only
//! writer of both is the activation repository transaction.
//!
//! The types here own the parts of that contract that must be true before any
//! database work happens:
//!
//! * [`TenantScope`] makes a contact-scoped release subject unrepresentable.
//! * [`ReleaseState`] is the candidate lifecycle, with an explicit transition
//!   relation instead of an implicit "published means everything" state.
//! * [`ActivationTarget`] names the exact serving mutation being gated.
//! * [`EvaluationSubjectV1`] is the exact thing that was evaluated; its digest
//!   covers every input that could change the answer.
//! * [`ReleasePolicy`] is the gate, resolved server-side, carrying mandatory
//!   deterministic score assertions and a nonempty primary gate family.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::agent::AgentRevisionLock;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::canonical::canonical_hash;
use crate::document::{ArtifactKind, ArtifactStatus};
use crate::{Error, ReleaseRejection, Result};

/// Deterministic score assertions every release policy must carry.
///
/// A policy without these is not a weaker gate, it is an absent one, so
/// [`ReleasePolicy::validate`] refuses it before evaluation is ever dispatched.
pub const PLATFORM_BLOCKING_ASSERTIONS: [&str; 3] =
    ["target_completed", "result_produced", "privacy_safe_output"];

/// Tenant scope accepted by the release system.
///
/// The release type system accepts a tenant scope only. Contact-scoped serving
/// artifacts were archived by migration `V000373`, and this newtype is why they
/// cannot come back: a contact scope has no conversion into a release subject.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TenantScope {
    tenant_id: TenantId,
}

impl TenantScope {
    /// Wraps a tenant id as a release scope.
    #[must_use]
    pub fn new(tenant_id: TenantId) -> Self {
        Self { tenant_id }
    }

    /// Returns the tenant owning this release subject.
    #[must_use]
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the storage partition that stores this tenant's artifacts.
    #[must_use]
    pub fn storage_partition_id(&self) -> StoragePartitionId {
        StoragePartitionId::for_tenant(self.tenant_id)
    }

    /// Returns the artifact inheritance scope for registry reads and writes.
    #[must_use]
    pub fn action_rule_scope(&self) -> ActionRuleScope {
        ActionRuleScope::Tenant {
            tenant_id: self.tenant_id,
        }
    }

    /// Converts an artifact inheritance scope into a release scope.
    ///
    /// A contact scope is refused: it is not a narrower release subject, it is
    /// an unevaluatable one.
    pub fn from_action_rule_scope(scope: &ActionRuleScope) -> Result<Self> {
        match scope {
            ActionRuleScope::Tenant { tenant_id } => Ok(Self::new(*tenant_id)),
            ActionRuleScope::Contact { contact_id, .. } => Err(Error::Release {
                rejection: ReleaseRejection::ContactScopeUnsupported,
                detail: format!("contact {contact_id} cannot own a release subject"),
            }),
        }
    }
}

/// Class of serving mutation a release attempt would perform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationTargetClass {
    /// Skill artifact serving pointer.
    SkillVisibility,
    /// Action artifact serving pointer.
    ActionVisibility,
    /// Agent installation serving pointer.
    AgentDeployment,
}

impl ActivationTargetClass {
    /// Returns the lowercase database label for this class.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SkillVisibility => "skill_visibility",
            Self::ActionVisibility => "action_visibility",
            Self::AgentDeployment => "agent_deployment",
        }
    }

    /// Returns the artifact kind whose revisions this class activates.
    #[must_use]
    pub fn artifact_kind(&self) -> ArtifactKind {
        match self {
            Self::SkillVisibility => ArtifactKind::Skill,
            Self::ActionVisibility => ArtifactKind::Action,
            Self::AgentDeployment => ArtifactKind::Agent,
        }
    }

    /// Returns the activation class that gates an artifact kind, if any.
    ///
    /// Connector catalogs and experiment plans are not release-gated here:
    /// connector catalog activation is platform-owned, and an experiment plan is
    /// evaluation configuration rather than something a session resolves.
    #[must_use]
    pub fn for_artifact_kind(kind: &ArtifactKind) -> Option<Self> {
        match kind {
            ArtifactKind::Skill => Some(Self::SkillVisibility),
            ArtifactKind::Action => Some(Self::ActionVisibility),
            ArtifactKind::Agent => Some(Self::AgentDeployment),
            ArtifactKind::Connector | ArtifactKind::ExperimentPlan => None,
        }
    }

    /// Returns whether the artifact kind's serving transition is release-gated.
    #[must_use]
    pub fn is_release_gated(kind: &ArtifactKind) -> bool {
        Self::for_artifact_kind(kind).is_some()
    }
}

impl fmt::Display for ActivationTargetClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ActivationTargetClass {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "skill_visibility" => Ok(Self::SkillVisibility),
            "action_visibility" => Ok(Self::ActionVisibility),
            "agent_deployment" => Ok(Self::AgentDeployment),
            other => Err(Error::Release {
                rejection: ReleaseRejection::TargetKindMismatch,
                detail: format!("unknown activation target class `{other}`"),
            }),
        }
    }
}

/// Exact serving mutation a release attempt would perform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "target")]
pub enum ActivationTarget {
    /// Move a skill artifact's serving pointer.
    SkillVisibility {
        /// Artifact whose serving pointer would move.
        artifact_uid: Uuid,
    },
    /// Move an action artifact's serving pointer.
    ActionVisibility {
        /// Artifact whose serving pointer would move.
        artifact_uid: Uuid,
    },
    /// Move an agent installation's current revision.
    AgentDeployment {
        /// Agent artifact the installation was installed from.
        artifact_uid: Uuid,
        /// Installation whose serving pointer would move.
        installation_uid: Uuid,
    },
}

impl ActivationTarget {
    /// Returns the class of this target.
    #[must_use]
    pub fn class(&self) -> ActivationTargetClass {
        match self {
            Self::SkillVisibility { .. } => ActivationTargetClass::SkillVisibility,
            Self::ActionVisibility { .. } => ActivationTargetClass::ActionVisibility,
            Self::AgentDeployment { .. } => ActivationTargetClass::AgentDeployment,
        }
    }

    /// Returns the artifact whose revisions this target activates.
    #[must_use]
    pub fn artifact_uid(&self) -> Uuid {
        match self {
            Self::SkillVisibility { artifact_uid }
            | Self::ActionVisibility { artifact_uid }
            | Self::AgentDeployment { artifact_uid, .. } => *artifact_uid,
        }
    }

    /// Returns the installation this target deploys into, for agent targets.
    #[must_use]
    pub fn installation_uid(&self) -> Option<Uuid> {
        match self {
            Self::AgentDeployment {
                installation_uid, ..
            } => Some(*installation_uid),
            Self::SkillVisibility { .. } | Self::ActionVisibility { .. } => None,
        }
    }

    /// Builds a target for an artifact kind, refusing kinds that are not gated.
    pub fn for_kind(
        kind: &ArtifactKind,
        artifact_uid: Uuid,
        installation_uid: Option<Uuid>,
    ) -> Result<Self> {
        match (
            ActivationTargetClass::for_artifact_kind(kind),
            installation_uid,
        ) {
            (Some(ActivationTargetClass::SkillVisibility), None) => {
                Ok(Self::SkillVisibility { artifact_uid })
            }
            (Some(ActivationTargetClass::ActionVisibility), None) => {
                Ok(Self::ActionVisibility { artifact_uid })
            }
            (Some(ActivationTargetClass::AgentDeployment), Some(installation_uid)) => {
                Ok(Self::AgentDeployment {
                    artifact_uid,
                    installation_uid,
                })
            }
            (Some(class), _) => Err(Error::Release {
                rejection: ReleaseRejection::TargetKindMismatch,
                detail: format!(
                    "activation target class {class} does not accept the supplied installation binding"
                ),
            }),
            (None, _) => Err(Error::Release {
                rejection: ReleaseRejection::TargetKindMismatch,
                detail: format!("artifact kind {kind} has no release-gated serving pointer"),
            }),
        }
    }
}

/// Candidate lifecycle state for a release-gated artifact revision.
///
/// None of these states serve. Serving is a pointer, and [`Self::Ready`] means
/// only that an activation request is permitted to try.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// Immutable candidate created by an import, update, or distillation.
    Draft,
    /// A release attempt holds the artifact's active run slot.
    Evaluating,
    /// Deterministic evidence passed the gate; activation may be requested.
    Ready,
    /// Deterministic assertions failed. Terminal for this revision.
    Rejected,
    /// Evidence was incomplete or the paired gate could not resolve. Retryable.
    Inconclusive,
    /// A newer candidate or activation replaced this one. Terminal.
    Superseded,
    /// Withdrawn or archived by migration. Terminal.
    Archived,
}

impl ReleaseState {
    /// Returns the lowercase database label for this state.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Evaluating => "evaluating",
            Self::Ready => "ready",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }

    /// Returns whether an activation request may name a candidate in this state.
    #[must_use]
    pub fn is_activatable(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Returns whether a further release attempt may start from this state.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Draft | Self::Inconclusive)
    }

    /// Returns whether `next` is a legal successor of this state.
    ///
    /// The relation is deliberately narrow. A `Ready` candidate cannot re-enter
    /// evaluation, because its evidence is bound to a subject digest that would
    /// change; it can only be superseded or archived. `Rejected` never becomes
    /// activatable: a rejected revision is replaced, not retried.
    #[must_use]
    pub fn can_transition_to(&self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Draft,
                Self::Evaluating | Self::Superseded | Self::Archived
            ) | (
                Self::Evaluating,
                Self::Ready
                    | Self::Rejected
                    | Self::Inconclusive
                    | Self::Superseded
                    | Self::Archived,
            ) | (Self::Ready, Self::Superseded | Self::Archived)
                | (
                    Self::Inconclusive,
                    Self::Evaluating | Self::Superseded | Self::Archived
                )
                | (Self::Rejected | Self::Superseded, Self::Archived)
        )
    }

    /// Returns this state, or an [`Error::Release`] when `next` is illegal.
    pub fn transition_to(&self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            return Ok(next);
        }
        Err(Error::Release {
            rejection: ReleaseRejection::IllegalStateTransition,
            detail: format!(
                "release candidate cannot move from {} to {}",
                self.as_str(),
                next.as_str()
            ),
        })
    }
}

impl ReleaseState {
    /// Returns the persisted revision status for this candidate state.
    #[must_use]
    pub fn artifact_status(&self) -> ArtifactStatus {
        match self {
            Self::Draft => ArtifactStatus::Draft,
            Self::Evaluating => ArtifactStatus::Evaluating,
            Self::Ready => ArtifactStatus::Ready,
            Self::Rejected => ArtifactStatus::Rejected,
            Self::Inconclusive => ArtifactStatus::Inconclusive,
            Self::Superseded => ArtifactStatus::Superseded,
            Self::Archived => ArtifactStatus::Archived,
        }
    }

    /// Reads a candidate state from a persisted revision status.
    ///
    /// `published` is refused: a release-gated revision in that status would mean
    /// the pre-pointer semantics came back.
    pub fn from_artifact_status(status: &ArtifactStatus) -> Result<Self> {
        match status {
            ArtifactStatus::Draft => Ok(Self::Draft),
            ArtifactStatus::Evaluating => Ok(Self::Evaluating),
            ArtifactStatus::Ready => Ok(Self::Ready),
            ArtifactStatus::Rejected => Ok(Self::Rejected),
            ArtifactStatus::Inconclusive => Ok(Self::Inconclusive),
            ArtifactStatus::Superseded => Ok(Self::Superseded),
            ArtifactStatus::Archived => Ok(Self::Archived),
            ArtifactStatus::Published => Err(Error::Release {
                rejection: ReleaseRejection::IllegalStateTransition,
                detail: "a release-gated revision cannot use the published status".to_string(),
            }),
        }
    }
}

impl fmt::Display for ReleaseState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReleaseState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "draft" => Ok(Self::Draft),
            "evaluating" => Ok(Self::Evaluating),
            "ready" => Ok(Self::Ready),
            "rejected" => Ok(Self::Rejected),
            "inconclusive" => Ok(Self::Inconclusive),
            "superseded" => Ok(Self::Superseded),
            "archived" => Ok(Self::Archived),
            other => Err(Error::Release {
                rejection: ReleaseRejection::IllegalStateTransition,
                detail: format!("`{other}` is not a release candidate state"),
            }),
        }
    }
}

/// Coalescing slot a candidate occupies for its artifact.
///
/// At most one candidate per artifact may hold [`Self::Active`] and at most one
/// may hold [`Self::Pending`], so rapid submissions collapse to one running
/// attempt plus the newest waiting subject instead of a queue that grows with
/// tenant impatience.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSlot {
    /// Holds the artifact's single active run slot.
    Active,
    /// Newest waiting subject; runs when the active slot is released.
    Pending,
    /// Holds no slot.
    Released,
}

impl ReleaseSlot {
    /// Returns the lowercase database label for this slot.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Released => "released",
        }
    }
}

impl fmt::Display for ReleaseSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReleaseSlot {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "pending" => Ok(Self::Pending),
            "released" => Ok(Self::Released),
            other => Err(Error::Release {
                rejection: ReleaseRejection::IllegalStateTransition,
                detail: format!("`{other}` is not a release slot"),
            }),
        }
    }
}

/// A 32-byte digest carried inside a release subject or attestation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Digest32(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl Digest32 {
    /// Builds a digest from database bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        <[u8; 32]>::try_from(bytes)
            .map(Self)
            .map_err(|_| Error::Release {
                rejection: ReleaseRejection::SubjectDigestMismatch,
                detail: format!("expected a 32-byte digest, found {} bytes", bytes.len()),
            })
    }

    /// Returns the digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the digest as a database-bindable vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl fmt::Display for Digest32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Hex serialization for [`Digest32`], so a persisted subject is readable.
mod hex_bytes {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        bytes: &[u8; 32],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut encoded = String::with_capacity(64);
        for byte in bytes {
            encoded.push_str(&format!("{byte:02x}"));
        }
        serializer.serialize_str(&encoded)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 32], D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 64 {
            return Err(D::Error::custom("expected 64 hex characters"));
        }
        let mut bytes = [0_u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let start = index * 2;
            *slot = u8::from_str_radix(&encoded[start..start + 2], 16)
                .map_err(|error| D::Error::custom(error.to_string()))?;
        }
        Ok(bytes)
    }
}

/// Whether an assertion's outcome is reproducible from persisted evidence.
///
/// Only [`Self::Deterministic`] assertions may block a release. Model judges,
/// simulator opinions, and tool-choice probes are diagnostic.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterminismClass {
    /// Reproducible from persisted, versioned evidence.
    Deterministic,
    /// Informative but not reproducible; never blocking.
    Diagnostic,
}

/// A registered assertion selected by a release policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssertionRef {
    /// Registered assertion identifier.
    pub id: String,
    /// Exact assertion implementation version.
    pub version: String,
    /// Whether this assertion may block.
    pub determinism: DeterminismClass,
}

/// Whether a larger metric value is better.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    /// Larger values are better.
    HigherIsBetter,
    /// Smaller values are better.
    LowerIsBetter,
}

/// One metric in a policy's primary gate family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GateMetric {
    /// Metric name as persisted by the evaluation surface.
    pub metric: String,
    /// Orientation used to compute the oriented utility delta.
    pub direction: MetricDirection,
    /// Predeclared one-sided non-inferiority margin, in basis points.
    pub margin_bp: i32,
}

/// Exact identity of the policy a subject was evaluated under.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PolicyIdentity {
    /// Policy row identifier.
    pub policy_uid: Uuid,
    /// Policy revision; a policy edit is a new revision, never an in-place edit.
    pub revision: i32,
    /// Canonical hash over the exact policy body.
    pub policy_hash: Digest32,
}

/// Server-resolved release gate.
///
/// A candidate submitter never supplies this. It is resolved from the policy
/// table, whose platform rows are global-scope and therefore unwritable by any
/// tenant role, and whose tenant overrides are written under a strictly stronger
/// authorization relation than candidate submission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleasePolicy {
    /// Policy row identifier.
    pub policy_uid: Uuid,
    /// Owning tenant, or `None` for the platform default.
    pub tenant_id: Option<TenantId>,
    /// Operator-facing policy name.
    pub name: String,
    /// Policy revision.
    pub revision: i32,
    /// Activation class this policy gates.
    pub target_class: ActivationTargetClass,
    /// Mandatory deterministic platform score assertions plus policy-specific ones.
    pub blocking_assertions: Vec<AssertionRef>,
    /// Primary gate family evaluated with intersection-union non-inferiority.
    pub primary_gate_family: Vec<GateMetric>,
    /// Lifetime of an attestation minted under this policy.
    pub attestation_ttl_secs: i64,
    /// Hash of the resource policy the evaluation must run under.
    pub resource_policy_hash: Digest32,
    /// Canonical hash over the exact policy body.
    pub policy_hash: Digest32,
}

impl ReleasePolicy {
    /// Returns the exact identity that becomes part of the subject digest.
    #[must_use]
    pub fn identity(&self) -> PolicyIdentity {
        PolicyIdentity {
            policy_uid: self.policy_uid,
            revision: self.revision,
            policy_hash: self.policy_hash,
        }
    }

    /// Refuses a policy that could not block anything.
    ///
    /// An empty gate family, a missing platform blocking assertion, or an
    /// assertion whose determinism class cannot block are all the same failure:
    /// a policy that would let evaluation "pass" without deciding anything.
    pub fn validate(&self) -> Result<()> {
        if self.primary_gate_family.is_empty() {
            return Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                detail: format!(
                    "release policy {} has an empty primary gate family",
                    self.name
                ),
            });
        }
        for metric in &self.primary_gate_family {
            if metric.metric.trim().is_empty() {
                return Err(Error::Release {
                    rejection: ReleaseRejection::PolicyInvalid,
                    detail: format!(
                        "release policy {} declares an unnamed gate metric",
                        self.name
                    ),
                });
            }
            if metric.margin_bp <= 0 {
                return Err(Error::Release {
                    rejection: ReleaseRejection::PolicyInvalid,
                    detail: format!(
                        "release policy {} declares a non-positive margin for {}",
                        self.name, metric.metric
                    ),
                });
            }
        }
        for required in PLATFORM_BLOCKING_ASSERTIONS {
            let present = self
                .blocking_assertions
                .iter()
                .any(|assertion| assertion.id == required);
            if !present {
                return Err(Error::Release {
                    rejection: ReleaseRejection::PolicyInvalid,
                    detail: format!(
                        "release policy {} is missing platform blocking assertion {required}",
                        self.name
                    ),
                });
            }
        }
        for assertion in &self.blocking_assertions {
            if assertion.determinism != DeterminismClass::Deterministic {
                return Err(Error::Release {
                    rejection: ReleaseRejection::PolicyInvalid,
                    detail: format!(
                        "blocking assertion {} in policy {} is {:?}; only deterministic evidence may block",
                        assertion.id, self.name, assertion.determinism
                    ),
                });
            }
            if assertion.version.trim().is_empty() {
                return Err(Error::Release {
                    rejection: ReleaseRejection::PolicyInvalid,
                    detail: format!(
                        "blocking assertion {} in policy {} has no version",
                        assertion.id, self.name
                    ),
                });
            }
        }
        if self.attestation_ttl_secs <= 0 {
            return Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                detail: format!(
                    "release policy {} declares a non-positive attestation lifetime",
                    self.name
                ),
            });
        }
        Ok(())
    }
}

/// The serving revision a candidate was compared against.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ServingBaseline {
    /// Revision that was serving when the candidate was evaluated.
    pub revision_uid: Uuid,
    /// Canonical hash of that revision.
    pub revision_hash: Digest32,
    /// Serving pointer version observed at submission.
    pub pointer_version: i64,
}

/// Agent runtime inputs that change behavior without changing skill bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentRuntimeSubject {
    /// Hash of the exact resolved system prompt.
    pub prompt_hash: Digest32,
    /// Exact model identifier.
    pub model: String,
    /// Exact provider identifier.
    pub provider: String,
    /// Hash of the resolved runtime policy (guardrails, limits, routing).
    pub runtime_policy_hash: Digest32,
}

/// Simulator policy binding for simulator-backed subjects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorPolicyBinding {
    /// Certified simulator policy identifier.
    pub policy_uid: Uuid,
    /// Simulator policy revision.
    pub revision: i32,
    /// Hash over the exact simulator policy body.
    pub policy_hash: Digest32,
    /// End of the fidelity certification validity window.
    pub certified_until: DateTime<Utc>,
}

/// Activated connector catalog snapshot for tool-bearing subjects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogSnapshotBinding {
    /// Catalog snapshot identifier.
    pub snapshot_uid: Uuid,
    /// Hash over the exact tool schema set in the snapshot.
    pub schema_hash: Digest32,
    /// Whether the snapshot is the activated one, not merely a candidate.
    pub activated: bool,
}

/// Evaluation-plan inputs that determine what was run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvaluationPlanSubject {
    /// Hash of the pinned experiment plan revision.
    pub plan_hash: Digest32,
    /// Hash of the scenario and dataset pack.
    pub scenario_dataset_hash: Digest32,
    /// Hash of the paired seed set.
    pub seed_hash: Digest32,
    /// Evaluator identifiers mapped to their exact versions.
    pub evaluator_versions: BTreeMap<String, String>,
}

/// Everything that could change a release answer, in one hashable value.
///
/// Two evaluations agree only if their subjects agree exactly. Any drift --
/// candidate bytes, serving baseline, dependency lock, prompt, model, provider,
/// runtime policy, tool policy, catalog schema, plan, scenario pack, simulator
/// policy, seeds, evaluator versions, gate policy, or resource policy -- yields a
/// different digest, which is what makes a stale attestation fail closed instead
/// of silently authorizing a different change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvaluationSubjectV1 {
    /// Subject schema version. Present in the digest so a schema change cannot
    /// collide with an older subject.
    pub subject_version: u16,
    /// Tenant owning the subject.
    pub tenant_id: TenantId,
    /// Exact serving mutation under evaluation.
    pub activation_target: ActivationTarget,
    /// Candidate revision identifier.
    pub candidate_revision_uid: Uuid,
    /// Canonical hash of the candidate revision document.
    pub candidate_revision_hash: Digest32,
    /// Serving revision compared against, or `None` for a first activation.
    pub serving_baseline: Option<ServingBaseline>,
    /// Hash of the resolved dependency lock.
    pub dependency_lock_hash: Digest32,
    /// Agent prompt, model, provider, and runtime policy.
    pub agent_runtime: AgentRuntimeSubject,
    /// Hash of the resolved tool policy.
    pub tool_policy_hash: Digest32,
    /// Whether the subject can call tools at all.
    pub tool_bearing: bool,
    /// Activated catalog schema snapshot; required when tool-bearing.
    pub tool_catalog: Option<CatalogSnapshotBinding>,
    /// Plan, scenario, seed, and evaluator versions.
    pub plan: EvaluationPlanSubject,
    /// Certified simulator policy; required when the plan uses a simulator.
    pub simulator: Option<SimulatorPolicyBinding>,
    /// Gate policy identity.
    pub release_policy: PolicyIdentity,
    /// Hash of the resource policy the run must be contained by.
    pub resource_policy_hash: Digest32,
}

impl EvaluationSubjectV1 {
    /// Current subject schema version.
    pub const VERSION: u16 = 1;

    /// Returns the canonical digest over every subject input.
    pub fn digest(&self) -> Result<Digest32> {
        canonical_hash(self)
            .map(Digest32)
            .map_err(|error| Error::Release {
                rejection: ReleaseRejection::SubjectDigestMismatch,
                detail: format!("evaluation subject is not canonicalizable: {error}"),
            })
    }

    /// Refuses a subject that cannot support a blocking decision.
    ///
    /// Fail-closed checks, in the order the plan states them: the subject schema
    /// must be the one this build understands, the target artifact must match the
    /// target class, a tool-bearing subject needs an activated catalog snapshot,
    /// a simulator-backed subject needs unexpired certification, and a subject
    /// with no evaluator versions has nothing versioned to reproduce.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<()> {
        if self.subject_version != Self::VERSION {
            return Err(Error::Release {
                rejection: ReleaseRejection::SubjectDigestMismatch,
                detail: format!(
                    "evaluation subject version {} is not {}",
                    self.subject_version,
                    Self::VERSION
                ),
            });
        }
        if self.plan.evaluator_versions.is_empty() {
            return Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                detail: "evaluation subject names no evaluator versions".to_string(),
            });
        }
        if self.tool_bearing {
            match &self.tool_catalog {
                Some(binding) if binding.activated => {}
                Some(_) => {
                    return Err(Error::Release {
                        rejection: ReleaseRejection::ToolCatalogSnapshotMissing,
                        detail:
                            "tool-bearing subject names a catalog snapshot that is not activated"
                                .to_string(),
                    });
                }
                None => {
                    return Err(Error::Release {
                        rejection: ReleaseRejection::ToolCatalogSnapshotMissing,
                        detail: "tool-bearing subject names no activated catalog snapshot"
                            .to_string(),
                    });
                }
            }
        }
        if let Some(simulator) = &self.simulator
            && simulator.certified_until <= now
        {
            return Err(Error::Release {
                rejection: ReleaseRejection::SimulatorCertificationExpired,
                detail: format!(
                    "simulator policy {} certification expired at {}",
                    simulator.policy_uid, simulator.certified_until
                ),
            });
        }
        Ok(())
    }
}

/// Deterministic release verdict over persisted evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterministicVerdict {
    /// Every blocking assertion and gate metric passed.
    Pass,
    /// A blocking assertion or gate metric declared a regression.
    Regression,
    /// Evidence was incomplete or the gate could not resolve.
    Inconclusive,
}

impl DeterministicVerdict {
    /// Returns the lowercase database label for this verdict.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Regression => "regression",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// Returns the candidate state this verdict moves an evaluating candidate to.
    #[must_use]
    pub fn candidate_state(&self) -> ReleaseState {
        match self {
            Self::Pass => ReleaseState::Ready,
            Self::Regression => ReleaseState::Rejected,
            Self::Inconclusive => ReleaseState::Inconclusive,
        }
    }
}

/// Provenance of the decision that minted an attestation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionProvenance {
    /// Gate policy the decision was made under.
    pub policy: PolicyIdentity,
    /// Deterministic verdict.
    pub verdict: DeterministicVerdict,
    /// Per-metric gate outcomes, keyed by metric name.
    pub gate_results: BTreeMap<String, String>,
    /// Blocking assertions that were evaluated.
    pub blocking_assertions: Vec<AssertionRef>,
    /// Identity that recorded the decision.
    pub decided_by: String,
    /// When the decision was recorded.
    pub decided_at: DateTime<Utc>,
    /// Evidence adapter that produced the deterministic result.
    pub evidence_adapter: EvidenceAdapter,
}

/// Which evidence surface produced a deterministic release result.
///
/// The learning-specific skill regression result is one adapter among others, not
/// the shared gate type: a distilled skill and a hand-authored skill reach the
/// same decision contract through different evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAdapter {
    /// Behavior Lab experiment run and trial evidence.
    BehaviorLabExperiment,
    /// Skill-learning regression suite result over sanitized learning evidence.
    SkillLearningRegression,
}

impl EvidenceAdapter {
    /// Returns the lowercase database label for this adapter.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BehaviorLabExperiment => "behavior_lab_experiment",
            Self::SkillLearningRegression => "skill_learning_regression",
        }
    }
}

/// An immutable, expiring, single-use permission to move a serving pointer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivationAttestation {
    /// Attestation identifier.
    pub attestation_uid: Uuid,
    /// Tenant owning the attested subject.
    pub tenant_id: TenantId,
    /// Exact serving mutation permitted.
    pub activation_target: ActivationTarget,
    /// Candidate revision permitted to serve.
    pub candidate_revision_uid: Uuid,
    /// Digest of the exact subject that was evaluated.
    pub subject_digest: Digest32,
    /// Experiment run that produced the evidence.
    pub run_uid: Uuid,
    /// Trials that produced the evidence.
    pub trial_uids: Vec<Uuid>,
    /// Evidence rows the decision consumed.
    pub evidence_ids: Vec<Uuid>,
    /// Decision provenance.
    pub decision: DecisionProvenance,
    /// When the attestation was minted.
    pub created_at: DateTime<Utc>,
    /// When the attestation stops being usable.
    pub expires_at: DateTime<Utc>,
    /// When the attestation was consumed, if it was.
    pub consumed_at: Option<DateTime<Utc>>,
    /// Audit row that consumed it, if it was.
    pub consumed_by_audit_uid: Option<Uuid>,
}

impl ActivationAttestation {
    /// Returns whether this attestation is still spendable at `now`.
    #[must_use]
    pub fn is_spendable(&self, now: DateTime<Utc>) -> bool {
        self.consumed_at.is_none() && self.expires_at > now
    }
}

/// A request to mint an attestation from a deterministic release decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewActivationAttestation {
    /// Tenant owning the subject.
    pub scope: TenantScope,
    /// Exact serving mutation to permit.
    pub activation_target: ActivationTarget,
    /// Candidate revision the decision was made about.
    pub candidate_revision_uid: Uuid,
    /// Subject the decision was made over.
    pub subject: EvaluationSubjectV1,
    /// Experiment run that produced the evidence.
    pub run_uid: Uuid,
    /// Trials that produced the evidence.
    pub trial_uids: Vec<Uuid>,
    /// Evidence rows the decision consumed.
    pub evidence_ids: Vec<Uuid>,
    /// Decision provenance, including the deterministic verdict.
    pub decision: DecisionProvenance,
}

/// Serving pointer state an activation request expects to replace.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExpectedServing {
    /// Revision expected to be serving, or `None` when nothing serves yet.
    pub revision_uid: Option<Uuid>,
    /// Pointer version expected. `0` means "no pointer row yet".
    pub pointer_version: i64,
}

/// A request to move a type-owned serving pointer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationRequest {
    /// Tenant owning the pointer.
    pub scope: TenantScope,
    /// Exact serving mutation requested.
    pub activation_target: ActivationTarget,
    /// Candidate revision to activate.
    pub candidate_revision_uid: Uuid,
    /// Canonical hash the caller believes that revision has.
    pub candidate_revision_hash: Digest32,
    /// Attestation to consume.
    pub attestation_uid: Uuid,
    /// Pointer state the caller expects to replace.
    pub expected_serving: ExpectedServing,
    /// Exact runtime dependency lock persisted by an agent deployment.
    ///
    /// Required for agent targets and forbidden for skill or action targets.
    pub agent_revision_lock: Option<AgentRevisionLock>,
    /// Identity requesting activation.
    pub actor: String,
    /// Operator-supplied reason.
    pub reason: Option<String>,
}

/// Result of a successful activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationOutcome {
    /// Audit row recording the decision.
    pub audit_uid: Uuid,
    /// Revision now serving.
    pub activated_revision_uid: Uuid,
    /// Revision that was serving before, if any.
    pub previous_revision_uid: Option<Uuid>,
    /// New pointer version.
    pub pointer_version: i64,
    /// Candidates moved to `superseded` by this activation.
    pub superseded_revision_uids: Vec<Uuid>,
    /// Deployment row written for an agent activation.
    pub deployment_uid: Option<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::contact::ContactId;

    /// One named mutation of a subject, used to prove digest significance.
    type SubjectMutation = (&'static str, Box<dyn Fn(&mut EvaluationSubjectV1)>);

    fn digest(byte: u8) -> Digest32 {
        Digest32([byte; 32])
    }

    fn policy(target_class: ActivationTargetClass) -> ReleasePolicy {
        ReleasePolicy {
            policy_uid: Uuid::nil(),
            tenant_id: None,
            name: "platform-default".to_string(),
            revision: 1,
            target_class,
            blocking_assertions: PLATFORM_BLOCKING_ASSERTIONS
                .iter()
                .map(|id| AssertionRef {
                    id: (*id).to_string(),
                    version: "1".to_string(),
                    determinism: DeterminismClass::Deterministic,
                })
                .collect(),
            primary_gate_family: vec![GateMetric {
                metric: "result_produced".to_string(),
                direction: MetricDirection::HigherIsBetter,
                margin_bp: 500,
            }],
            attestation_ttl_secs: 86_400,
            resource_policy_hash: digest(7),
            policy_hash: digest(8),
        }
    }

    fn subject() -> EvaluationSubjectV1 {
        EvaluationSubjectV1 {
            subject_version: EvaluationSubjectV1::VERSION,
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            activation_target: ActivationTarget::SkillVisibility {
                artifact_uid: Uuid::from_u128(2),
            },
            candidate_revision_uid: Uuid::from_u128(3),
            candidate_revision_hash: digest(1),
            serving_baseline: Some(ServingBaseline {
                revision_uid: Uuid::from_u128(4),
                revision_hash: digest(2),
                pointer_version: 3,
            }),
            dependency_lock_hash: digest(3),
            agent_runtime: AgentRuntimeSubject {
                prompt_hash: digest(4),
                model: "model-a".to_string(),
                provider: "provider-a".to_string(),
                runtime_policy_hash: digest(5),
            },
            tool_policy_hash: digest(6),
            tool_bearing: false,
            tool_catalog: None,
            plan: EvaluationPlanSubject {
                plan_hash: digest(9),
                scenario_dataset_hash: digest(10),
                seed_hash: digest(11),
                evaluator_versions: BTreeMap::from([(
                    "assertion".to_string(),
                    "1.0.0".to_string(),
                )]),
            },
            simulator: None,
            release_policy: policy(ActivationTargetClass::SkillVisibility).identity(),
            resource_policy_hash: digest(7),
        }
    }

    // Pins: every input the plan lists must be digest-significant, so a stale
    // attestation cannot authorize a different change.
    #[test]
    fn every_declared_subject_input_changes_the_digest_offline() {
        let baseline = subject().digest().expect("digest");
        let mutations: Vec<SubjectMutation> = vec![
            (
                "tenant",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.tenant_id = TenantId::from(Uuid::from_u128(99));
                }),
            ),
            (
                "activation target",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.activation_target = ActivationTarget::ActionVisibility {
                        artifact_uid: Uuid::from_u128(2),
                    };
                }),
            ),
            (
                "candidate hash",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.candidate_revision_hash = digest(200);
                }),
            ),
            (
                "serving baseline",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.serving_baseline = None;
                }),
            ),
            (
                "dependency lock",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.dependency_lock_hash = digest(201);
                }),
            ),
            (
                "prompt",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.agent_runtime.prompt_hash = digest(202);
                }),
            ),
            (
                "model",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.agent_runtime.model = "model-b".to_string();
                }),
            ),
            (
                "provider",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.agent_runtime.provider = "provider-b".to_string();
                }),
            ),
            (
                "runtime policy",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.agent_runtime.runtime_policy_hash = digest(203);
                }),
            ),
            (
                "tool policy",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.tool_policy_hash = digest(204);
                }),
            ),
            (
                "catalog snapshot",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.tool_bearing = true;
                    subject.tool_catalog = Some(CatalogSnapshotBinding {
                        snapshot_uid: Uuid::from_u128(50),
                        schema_hash: digest(205),
                        activated: true,
                    });
                }),
            ),
            (
                "plan",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.plan.plan_hash = digest(206);
                }),
            ),
            (
                "scenario dataset",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.plan.scenario_dataset_hash = digest(207);
                }),
            ),
            (
                "seeds",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.plan.seed_hash = digest(208);
                }),
            ),
            (
                "evaluator versions",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject
                        .plan
                        .evaluator_versions
                        .insert("assertion".to_string(), "2.0.0".to_string());
                }),
            ),
            (
                "simulator policy",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.simulator = Some(SimulatorPolicyBinding {
                        policy_uid: Uuid::from_u128(60),
                        revision: 1,
                        policy_hash: digest(209),
                        certified_until: DateTime::from_timestamp(1_900_000_000, 0)
                            .expect("timestamp"),
                    });
                }),
            ),
            (
                "release policy",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.release_policy.revision = 2;
                }),
            ),
            (
                "resource policy",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.resource_policy_hash = digest(210);
                }),
            ),
            (
                "subject version",
                Box::new(|subject: &mut EvaluationSubjectV1| {
                    subject.subject_version = 2;
                }),
            ),
        ];

        for (label, mutate) in mutations {
            let mut mutated = subject();
            mutate(&mut mutated);
            assert_ne!(
                baseline,
                mutated.digest().expect("digest"),
                "{label} must be digest-significant"
            );
        }
    }

    // Pins: the digest is stable across re-serialization, so a round-tripped
    // subject recomputes to the same value at activation time.
    #[test]
    fn subject_digest_survives_a_json_round_trip_offline() {
        let subject = subject();
        let encoded = serde_json::to_value(&subject).expect("encode");
        let decoded: EvaluationSubjectV1 = serde_json::from_value(encoded).expect("decode");
        assert_eq!(subject.digest().expect("a"), decoded.digest().expect("b"));
    }

    // Pins: the exact legal transition relation. A table-driven check is the only
    // way a new state cannot quietly acquire an extra edge.
    #[test]
    fn release_state_transition_relation_is_exact_offline() {
        use ReleaseState::{
            Archived, Draft, Evaluating, Inconclusive, Ready, Rejected, Superseded,
        };
        let legal = [
            (Draft, Evaluating),
            (Draft, Superseded),
            (Draft, Archived),
            (Evaluating, Ready),
            (Evaluating, Rejected),
            (Evaluating, Inconclusive),
            (Evaluating, Superseded),
            (Evaluating, Archived),
            (Ready, Superseded),
            (Ready, Archived),
            (Inconclusive, Evaluating),
            (Inconclusive, Superseded),
            (Inconclusive, Archived),
            (Rejected, Archived),
            (Superseded, Archived),
        ];
        let all = [
            Draft,
            Evaluating,
            Ready,
            Rejected,
            Inconclusive,
            Superseded,
            Archived,
        ];
        for from in all {
            for to in all {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{from} -> {to} legality"
                );
                assert_eq!(from.transition_to(to).is_ok(), expected);
            }
        }
        assert!(Ready.is_activatable());
        for state in all.iter().filter(|state| **state != Ready) {
            assert!(!state.is_activatable(), "{state} must not be activatable");
        }
        assert!(Inconclusive.is_retryable());
        assert!(Draft.is_retryable());
        assert!(!Rejected.is_retryable());
    }

    // Pins: a contact scope has no release subject at all, so contact-scoped
    // release subjects are unrepresentable rather than merely rejected later.
    #[test]
    fn contact_scope_cannot_become_a_release_scope_offline() {
        let tenant_id = TenantId::from(Uuid::from_u128(1));
        let contact = ActionRuleScope::Contact {
            tenant_id,
            contact_id: ContactId(Uuid::from_u128(2)),
        };
        let error = TenantScope::from_action_rule_scope(&contact).expect_err("must refuse");
        assert!(matches!(
            error,
            Error::Release {
                rejection: ReleaseRejection::ContactScopeUnsupported,
                ..
            }
        ));
        let tenant = ActionRuleScope::Tenant { tenant_id };
        assert_eq!(
            TenantScope::from_action_rule_scope(&tenant).expect("tenant scope"),
            TenantScope::new(tenant_id)
        );
    }

    // Pins: every way a policy could fail to block something is refused.
    #[test]
    fn policy_without_a_real_gate_is_rejected_offline() {
        policy(ActivationTargetClass::SkillVisibility)
            .validate()
            .expect("platform default policy is valid");

        let mut empty_family = policy(ActivationTargetClass::SkillVisibility);
        empty_family.primary_gate_family.clear();
        assert!(matches!(
            empty_family.validate(),
            Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                ..
            })
        ));

        let mut missing_assertion = policy(ActivationTargetClass::SkillVisibility);
        missing_assertion.blocking_assertions.remove(0);
        assert!(matches!(
            missing_assertion.validate(),
            Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                ..
            })
        ));

        let mut diagnostic_assertion = policy(ActivationTargetClass::SkillVisibility);
        diagnostic_assertion.blocking_assertions[0].determinism = DeterminismClass::Diagnostic;
        assert!(matches!(
            diagnostic_assertion.validate(),
            Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                ..
            })
        ));

        let mut no_ttl = policy(ActivationTargetClass::SkillVisibility);
        no_ttl.attestation_ttl_secs = 0;
        assert!(matches!(
            no_ttl.validate(),
            Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                ..
            })
        ));
    }

    // Pins: a tool-bearing subject needs an activated schema snapshot and a
    // simulator-backed subject needs live certification.
    #[test]
    fn subject_validation_fails_closed_on_missing_certification_offline() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("timestamp");
        subject().validate(now).expect("baseline subject is valid");

        let mut tool_bearing = subject();
        tool_bearing.tool_bearing = true;
        assert!(matches!(
            tool_bearing.validate(now),
            Err(Error::Release {
                rejection: ReleaseRejection::ToolCatalogSnapshotMissing,
                ..
            })
        ));

        let mut unactivated = subject();
        unactivated.tool_bearing = true;
        unactivated.tool_catalog = Some(CatalogSnapshotBinding {
            snapshot_uid: Uuid::from_u128(50),
            schema_hash: digest(20),
            activated: false,
        });
        assert!(matches!(
            unactivated.validate(now),
            Err(Error::Release {
                rejection: ReleaseRejection::ToolCatalogSnapshotMissing,
                ..
            })
        ));

        let mut expired = subject();
        expired.simulator = Some(SimulatorPolicyBinding {
            policy_uid: Uuid::from_u128(60),
            revision: 1,
            policy_hash: digest(21),
            certified_until: now,
        });
        assert!(matches!(
            expired.validate(now),
            Err(Error::Release {
                rejection: ReleaseRejection::SimulatorCertificationExpired,
                ..
            })
        ));

        let mut no_evaluators = subject();
        no_evaluators.plan.evaluator_versions.clear();
        assert!(matches!(
            no_evaluators.validate(now),
            Err(Error::Release {
                rejection: ReleaseRejection::PolicyInvalid,
                ..
            })
        ));
    }

    // Pins: only release-gated kinds have an activation target, and an agent
    // target cannot be built without the installation it would move.
    #[test]
    fn activation_targets_bind_kind_to_pointer_offline() {
        let artifact_uid = Uuid::from_u128(9);
        assert_eq!(
            ActivationTarget::for_kind(&ArtifactKind::Skill, artifact_uid, None).expect("skill"),
            ActivationTarget::SkillVisibility { artifact_uid }
        );
        assert!(
            ActivationTarget::for_kind(&ArtifactKind::Skill, artifact_uid, Some(Uuid::nil()))
                .is_err()
        );
        assert!(ActivationTarget::for_kind(&ArtifactKind::Agent, artifact_uid, None).is_err());
        assert!(ActivationTarget::for_kind(&ArtifactKind::Connector, artifact_uid, None).is_err());
        assert!(
            ActivationTarget::for_kind(&ArtifactKind::ExperimentPlan, artifact_uid, None).is_err()
        );
        assert!(ActivationTargetClass::is_release_gated(
            &ArtifactKind::Skill
        ));
        assert!(ActivationTargetClass::is_release_gated(
            &ArtifactKind::Action
        ));
        assert!(ActivationTargetClass::is_release_gated(
            &ArtifactKind::Agent
        ));
        assert!(!ActivationTargetClass::is_release_gated(
            &ArtifactKind::Connector
        ));
        assert!(!ActivationTargetClass::is_release_gated(
            &ArtifactKind::ExperimentPlan
        ));
    }
}

/// Everything a caller needs to resolve one artifact under an evaluation overlay.
///
/// Held only by the release-evaluation workflow arm that owns the overlay. The
/// token is a secret whose plaintext never reaches Postgres — only its hash is
/// stored — so possession of this struct is the capability, and losing it does not
/// leave a usable row behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalOverlayBinding {
    /// Overlay the arm was provisioned with.
    pub overlay_uid: Uuid,
    /// Per-arm secret, in plaintext. Hashed before it is sent to the database.
    pub overlay_token: String,
    /// Eval-owned session this overlay answers for, and no other.
    pub eval_session_id: Uuid,
}

impl EvalOverlayBinding {
    /// Returns the stored hash of this binding's secret.
    ///
    /// Hashing here rather than at the call site is what keeps the plaintext out of
    /// query logs and out of any statement Postgres ever parses.
    #[must_use]
    pub fn token_hash(&self) -> [u8; 32] {
        overlay_token_hash(&self.overlay_token)
    }
}

/// Domain separator for overlay token hashing.
const OVERLAY_TOKEN_DOMAIN: &[u8] = b"moa.release.overlay_token.v1\0";

/// Hashes an overlay token exactly as the stored column holds it.
///
/// This is the single definition. The writer that provisions an overlay and the
/// reader that resolves through one must agree byte for byte or the lookup silently
/// finds nothing — a failure that would look like "the candidate was not pinned"
/// rather than like a hash mismatch, which is why there is one function and not two.
#[must_use]
pub fn overlay_token_hash(token: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(OVERLAY_TOKEN_DOMAIN);
    hasher.update(token.as_bytes());
    *hasher.finalize().as_bytes()
}

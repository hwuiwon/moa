#![recursion_limit = "256"]

//! Canonical artifact definitions for MOA agents, skills, connectors, actions, and experiment plans.
//!
//! The crate owns the code-addressable document model used by API imports,
//! Postgres storage, and future visual builders. Runtime crates should depend
//! on these types instead of duplicating ad hoc JSON shapes.

/// Standalone action artifact definitions.
pub mod action;
/// Tenant-configurable agent artifact definitions.
pub mod agent;
/// Canonical artifact hashing helpers.
pub mod canonical;
/// Connector artifact definitions.
pub mod connector;
/// Artifact document wrappers and metadata.
pub mod document;
/// Canonical execution-plan, goal, outcome, and amendment definitions.
pub mod execution_plan;
/// Stable artifact reference parsing and formatting.
pub mod reference;
/// Postgres-backed artifact registry.
pub mod registry;
/// Release-candidate states, activation targets, subjects, and attestations.
pub mod release;
/// Reference resolution against serving artifact revisions.
pub mod resolver;
/// Behavior-lab experiment plan and embedded simulation definitions.
pub mod simulation;
/// Skill artifact definitions.
pub mod skill;
/// Fixture helpers for tests that need a serving revision.
#[cfg(feature = "test-support")]
pub mod test_fixtures;
/// Semantic validation for artifact documents.
pub mod validation;

/// Result type returned by artifact helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by artifact document parsing and canonicalization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// JSON parsing or canonical serialization failed.
    #[error("artifact json: {0}")]
    Json(#[from] serde_json::Error),
    /// YAML parsing or serialization failed.
    #[error("artifact yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// A reference string did not match a supported URI form.
    #[error("invalid artifact reference `{reference}`: {message}")]
    InvalidReference {
        /// The rejected reference string.
        reference: String,
        /// Human-readable rejection reason.
        message: String,
    },
    /// A release-control predicate refused the request.
    #[error("release refused ({rejection}): {detail}")]
    Release {
        /// Which predicate refused.
        rejection: ReleaseRejection,
        /// Human-readable detail naming the exact mismatch.
        detail: String,
    },
    /// Storage failed while reading or writing release state.
    #[error("release storage: {0}")]
    Storage(String),
}

/// The exact release predicate that refused a request.
///
/// Every variant is a fail-closed check, and every one is asserted by name in
/// tests: a rejection that cannot be named cannot be mutation-checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseRejection {
    /// The subject, attestation, or pointer belongs to a different tenant.
    WrongTenant,
    /// A contact scope cannot own a release subject.
    ContactScopeUnsupported,
    /// No candidate row exists for the named revision.
    CandidateNotFound,
    /// The candidate is not in an activatable state.
    CandidateNotActivatable,
    /// The candidate has not passed generic validation with resolved references.
    CandidateNotEligible,
    /// The candidate's stored canonical hash differs from the request.
    CandidateHashMismatch,
    /// The artifact kind does not match the activation target class.
    TargetKindMismatch,
    /// The named agent installation is missing, retired, or in another tenant.
    InstallationNotFound,
    /// The serving pointer moved since the caller read it.
    ServingPointerConflict,
    /// No attestation exists with that identifier in this tenant.
    AttestationNotFound,
    /// The attestation names a different subject digest.
    SubjectDigestMismatch,
    /// The attestation names a different candidate revision or target.
    AttestationSubjectMismatch,
    /// The attestation expired.
    AttestationExpired,
    /// The attestation was already spent.
    AttestationAlreadyConsumed,
    /// The deterministic verdict was not a pass.
    VerdictNotPass,
    /// No release policy resolved for the tenant and activation class.
    PolicyNotFound,
    /// The resolved policy could not block anything.
    PolicyInvalid,
    /// The requested candidate state transition is not legal.
    IllegalStateTransition,
    /// Another candidate already holds the artifact's active run slot.
    ActiveRunSlotHeld,
    /// A simulator-backed subject has no current fidelity certification.
    SimulatorCertificationExpired,
    /// A tool-bearing subject names no activated catalog schema snapshot.
    ToolCatalogSnapshotMissing,
}

impl std::fmt::Display for ReleaseRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::WrongTenant => "wrong_tenant",
            Self::ContactScopeUnsupported => "contact_scope_unsupported",
            Self::CandidateNotFound => "candidate_not_found",
            Self::CandidateNotActivatable => "candidate_not_activatable",
            Self::CandidateNotEligible => "candidate_not_eligible",
            Self::CandidateHashMismatch => "candidate_hash_mismatch",
            Self::TargetKindMismatch => "target_kind_mismatch",
            Self::InstallationNotFound => "installation_not_found",
            Self::ServingPointerConflict => "serving_pointer_conflict",
            Self::AttestationNotFound => "attestation_not_found",
            Self::SubjectDigestMismatch => "subject_digest_mismatch",
            Self::AttestationSubjectMismatch => "attestation_subject_mismatch",
            Self::AttestationExpired => "attestation_expired",
            Self::AttestationAlreadyConsumed => "attestation_already_consumed",
            Self::VerdictNotPass => "verdict_not_pass",
            Self::PolicyNotFound => "policy_not_found",
            Self::PolicyInvalid => "policy_invalid",
            Self::IllegalStateTransition => "illegal_state_transition",
            Self::ActiveRunSlotHeld => "active_run_slot_held",
            Self::SimulatorCertificationExpired => "simulator_certification_expired",
            Self::ToolCatalogSnapshotMissing => "tool_catalog_snapshot_missing",
        };
        formatter.write_str(label)
    }
}

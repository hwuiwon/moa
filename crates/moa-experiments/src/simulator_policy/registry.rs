//! Immutable certified simulator-policy identities.
//!
//! A policy revision owns every input that changes simulated-user behavior. A
//! fidelity certification names the exact policy hash it measured. Production
//! experiment admission persists the resulting [`ResolvedSimulatorPolicy`], so
//! a replay never resolves a newer policy revision implicitly.

use chrono::{DateTime, Utc};
use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::release::Digest32;
use moa_artifacts::simulation::SimulatorPolicyReference;
use moa_core::types::identifiers::ModelId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::simulator_policy::SimulatorPolicyError;

/// Longest accepted scenario-domain identifier.
pub const MAX_DOMAIN_LEN: usize = 64;

/// Longest accepted simulator system prompt.
pub const MAX_SYSTEM_PROMPT_LEN: usize = 16_384;

/// Stable evaluator that decides simulator fidelity certifications.
pub const FIDELITY_EVALUATOR_ID: &str = "moa.simulator_fidelity";

/// Current fidelity evaluator version.
pub const FIDELITY_EVALUATOR_VERSION: u32 = 1;

/// Scenario domain a simulator policy is certified for.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ScenarioDomain(String);

impl ScenarioDomain {
    /// Creates a scenario domain from an operator-declared slug.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidDomain`] when the slug is empty,
    /// too long, or contains unsupported characters.
    pub fn new(value: impl Into<String>) -> Result<Self, SimulatorPolicyError> {
        let value = value.into();
        let shaped = !value.is_empty()
            && value.len() <= MAX_DOMAIN_LEN
            && value
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "-_.".contains(ch));
        if !shaped {
            return Err(SimulatorPolicyError::InvalidDomain { domain: value });
        }
        Ok(Self(value))
    }

    /// Returns the domain slug.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScenarioDomain {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Legal basis under which human interaction data may be used.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentBasis {
    /// Each participant explicitly opted in to research use.
    ExplicitParticipantConsent,
    /// Interactions produced by authorized internal staff testing.
    AuthorizedInternalDogfood,
}

/// How a human interaction cohort was de-identified before use.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeidentificationMethod {
    /// Direct identifiers were pseudonymized and quasi-identifiers redacted.
    PseudonymizedAndRedacted,
    /// Interactions were replaced by generated structural surrogates.
    SyntheticSurrogate,
}

/// Hash-pinned cohort of consented human interactions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CohortPin {
    /// Stable cohort identifier.
    pub cohort_id: String,
    /// Number of independent human units in the cohort.
    pub independent_units: u32,
    /// Digest over the exact de-identified interaction set.
    pub content_hash: Digest32,
    /// Legal basis for using the cohort.
    pub consent_basis: ConsentBasis,
    /// De-identification applied before use.
    pub deidentification: DeidentificationMethod,
}

impl CohortPin {
    /// Rejects a cohort pin that cannot support an auditable study.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidCohort`] for an invalid identifier
    /// or zero independent units.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        if self.cohort_id.is_empty() || self.cohort_id.len() > 128 {
            return Err(SimulatorPolicyError::InvalidCohort {
                detail: format!("cohort id `{}` must be 1..=128 characters", self.cohort_id),
            });
        }
        if self.independent_units == 0 {
            return Err(SimulatorPolicyError::InvalidCohort {
                detail: format!("cohort `{}` declares no independent units", self.cohort_id),
            });
        }
        Ok(())
    }
}

/// Structured response protocol the simulator is held to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorProtocol {
    /// Stable protocol identifier.
    pub id: String,
    /// Exact protocol version.
    pub version: u32,
    /// Digest over the exact response schema.
    pub schema_hash: Digest32,
}

/// Exact decoding parameters used for simulator turns.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SimulatorDecoding {
    /// Sampling temperature in thousandths.
    pub temperature_milli: u32,
    /// Highest output-token count for one simulator turn.
    pub max_output_tokens: u32,
    /// Whether the policy requires a deterministic per-trial seed.
    pub seeded: bool,
}

impl SimulatorDecoding {
    /// Returns the provider temperature represented by this policy.
    #[must_use]
    pub fn temperature(self) -> f32 {
        self.temperature_milli as f32 / 1_000.0
    }
}

/// Window in which a policy may be used, independent of certification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidityWindow {
    /// First instant the policy may be used.
    pub valid_from: DateTime<Utc>,
    /// Instant after which the policy may not be used.
    pub valid_until: DateTime<Utc>,
}

impl ValidityWindow {
    /// Returns whether `now` falls inside the window.
    #[must_use]
    pub fn contains(&self, now: DateTime<Utc>) -> bool {
        now >= self.valid_from && now < self.valid_until
    }
}

/// Every pinned input that decides simulated-user behavior.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorPolicyComponents {
    /// Scenario domain the policy was certified for.
    pub domain: ScenarioDomain,
    /// Exact simulator model identifier.
    pub model: ModelId,
    /// Exact provider family expected to serve the model.
    pub provider: String,
    /// Exact decoding parameters.
    pub decoding: SimulatorDecoding,
    /// Exact system prompt sent on every simulator call.
    pub system_prompt: String,
    /// Structured response protocol.
    pub protocol: SimulatorProtocol,
    /// Digest over the server-owned simulator context compiler contract.
    pub context_contract_hash: Digest32,
    /// Cohort the policy was calibrated against.
    pub calibration_cohort: CohortPin,
    /// Window the policy may be used in.
    pub validity: ValidityWindow,
}

impl SimulatorPolicyComponents {
    /// Rejects components that cannot be certified or executed.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidComponents`] for malformed model,
    /// provider, prompt, decoding, protocol, or validity inputs.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        if self.model.as_str().trim().is_empty() || self.model.as_str().len() > 128 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "simulator model identifier must be 1..=128 characters".to_string(),
            });
        }
        if self.provider.trim().is_empty() || self.provider.len() > 64 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "simulator provider must be 1..=64 characters".to_string(),
            });
        }
        if self.system_prompt.trim().is_empty() || self.system_prompt.len() > MAX_SYSTEM_PROMPT_LEN
        {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: format!(
                    "simulator system prompt must be 1..={MAX_SYSTEM_PROMPT_LEN} bytes"
                ),
            });
        }
        if self.protocol.id.trim().is_empty() || self.protocol.id.len() > 128 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "simulator protocol id must be 1..=128 characters".to_string(),
            });
        }
        if self.protocol.version == 0 || self.decoding.max_output_tokens == 0 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "protocol version and max output tokens must be positive".to_string(),
            });
        }
        if self.decoding.temperature_milli > 2_000 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "simulator decoding parameters are outside provider-safe bounds"
                    .to_string(),
            });
        }
        if self.validity.valid_until <= self.validity.valid_from {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: "simulator validity window must be non-empty".to_string(),
            });
        }
        self.calibration_cohort.validate()
    }
}

/// One registry entry: an identity plus its pinned components.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorPolicy {
    /// Stable policy identifier, shared across revisions.
    pub policy_uid: Uuid,
    /// Positive monotonic revision.
    pub revision: i32,
    /// Pinned components.
    pub components: SimulatorPolicyComponents,
}

impl SimulatorPolicy {
    /// Returns the canonical digest over the identity and every component.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::NotCanonicalizable`] when serialization
    /// fails.
    pub fn policy_hash(&self) -> Result<Digest32, SimulatorPolicyError> {
        canonical_hash(self).map(Digest32).map_err(|error| {
            SimulatorPolicyError::NotCanonicalizable {
                detail: error.to_string(),
            }
        })
    }

    /// Rejects a policy that cannot be registered.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] for a non-positive revision or invalid
    /// components.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        if self.revision < 1 {
            return Err(SimulatorPolicyError::InvalidComponents {
                detail: format!("simulator policy revision {} must be >= 1", self.revision),
            });
        }
        self.components.validate()
    }

    /// Returns the immutable reference stored in an experiment plan.
    #[must_use]
    pub const fn reference(&self) -> SimulatorPolicyReference {
        SimulatorPolicyReference {
            policy_uid: self.policy_uid,
            revision: self.revision,
        }
    }
}

/// Registry lifecycle state for one policy revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulatorPolicyState {
    /// Registered but not certified.
    Draft,
    /// A fidelity study certified this exact hash.
    Certified,
    /// A powered study contradicted its bounds.
    Rejected,
    /// Withdrawn by an operator.
    Revoked,
}

impl SimulatorPolicyState {
    /// Returns the database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Certified => "certified",
            Self::Rejected => "rejected",
            Self::Revoked => "revoked",
        }
    }

    /// Parses the database representation.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(Self::Draft),
            "certified" => Some(Self::Certified),
            "rejected" => Some(Self::Rejected),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

impl std::fmt::Display for SimulatorPolicyState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Certification granted by one immutable fidelity-study artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CertificationWindow {
    /// Study that produced the certification.
    pub study_uid: Uuid,
    /// Digest over the immutable fidelity-study artifact.
    pub study_artifact_hash: Digest32,
    /// Exact policy hash the study certified.
    pub certified_policy_hash: Digest32,
    /// First instant the certification is in force.
    pub certified_from: DateTime<Utc>,
    /// Instant the certification lapses.
    pub certified_until: DateTime<Utc>,
}

/// Immutable identity persisted with every admitted run and trial.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorPolicyBinding {
    /// Policy identifier.
    pub policy_uid: Uuid,
    /// Exact revision.
    pub revision: i32,
    /// Digest over the exact policy body.
    pub policy_hash: Digest32,
    /// Fidelity study that certified it.
    pub study_uid: Uuid,
    /// Digest over the immutable fidelity-study artifact.
    pub study_artifact_hash: Digest32,
    /// Exact certification evaluator version.
    pub evaluator_version: u32,
    /// Earliest policy or certification expiry.
    pub certified_until: DateTime<Utc>,
}

/// Durable registry row for one policy revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SimulatorPolicyRecord {
    /// Pinned policy body.
    pub policy: SimulatorPolicy,
    /// Hash stored beside the body.
    pub stored_policy_hash: Digest32,
    /// Current lifecycle state.
    pub state: SimulatorPolicyState,
    /// Certification, when granted.
    pub certification: Option<CertificationWindow>,
    /// Timestamp when registered.
    pub created_at: DateTime<Utc>,
    /// Timestamp when last changed.
    pub updated_at: DateTime<Utc>,
}

impl SimulatorPolicyRecord {
    /// Produces an executable binding only for a live exact certification.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] for hash drift, non-certified state, or
    /// expired policy/certification windows.
    pub fn execution_binding(
        &self,
        now: DateTime<Utc>,
    ) -> Result<SimulatorPolicyBinding, SimulatorPolicyError> {
        let recomputed = self.policy.policy_hash()?;
        if recomputed != self.stored_policy_hash {
            return Err(SimulatorPolicyError::PolicyHashDrift {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
            });
        }
        if self.state != SimulatorPolicyState::Certified {
            return Err(SimulatorPolicyError::NotCertified {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
                state: self.state,
            });
        }
        let certification = self
            .certification
            .ok_or(SimulatorPolicyError::NotCertified {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
                state: self.state,
            })?;
        if certification.certified_policy_hash != recomputed {
            return Err(SimulatorPolicyError::CertifiedComponentChanged {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
                study_uid: certification.study_uid,
            });
        }
        if !self.policy.components.validity.contains(now) {
            return Err(SimulatorPolicyError::OutsideValidityWindow {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
                valid_until: self.policy.components.validity.valid_until,
            });
        }
        if now < certification.certified_from || now >= certification.certified_until {
            return Err(SimulatorPolicyError::CertificationLapsed {
                policy_uid: self.policy.policy_uid,
                revision: self.policy.revision,
                certified_until: certification.certified_until,
            });
        }
        Ok(SimulatorPolicyBinding {
            policy_uid: self.policy.policy_uid,
            revision: self.policy.revision,
            policy_hash: recomputed,
            study_uid: certification.study_uid,
            study_artifact_hash: certification.study_artifact_hash,
            evaluator_version: FIDELITY_EVALUATOR_VERSION,
            certified_until: certification
                .certified_until
                .min(self.policy.components.validity.valid_until),
        })
    }
}

/// Full immutable policy snapshot consumed by production trial execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedSimulatorPolicy {
    /// Exact certification identity.
    pub binding: SimulatorPolicyBinding,
    /// Pinned runtime components.
    pub components: SimulatorPolicyComponents,
}

impl ResolvedSimulatorPolicy {
    /// Returns the compact plan reference for this snapshot.
    #[must_use]
    pub const fn reference(&self) -> SimulatorPolicyReference {
        SimulatorPolicyReference {
            policy_uid: self.binding.policy_uid,
            revision: self.binding.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator_policy::test_support::{at, certified_record};

    #[test]
    fn exact_policy_change_invalidates_certification_offline() {
        // Pins: a prompt edit cannot inherit an earlier study's certification.
        let now = at(1_500_000);
        let mut record = certified_record(at(1_000_000), at(1_900_000), at(2_000_000));
        record.policy.components.system_prompt.push_str(" changed");
        assert!(matches!(
            record.execution_binding(now),
            Err(SimulatorPolicyError::PolicyHashDrift { .. })
        ));
    }

    #[test]
    fn effective_binding_uses_earliest_expiry_offline() {
        // Pins: execution never outlives either the policy or its study.
        let record = certified_record(at(1_000_000), at(1_900_000), at(1_800_000));
        let binding = record
            .execution_binding(at(1_500_000))
            .expect("both windows cover the instant");
        assert_eq!(binding.certified_until, at(1_800_000));
        assert_eq!(binding.evaluator_version, FIDELITY_EVALUATOR_VERSION);
    }
}

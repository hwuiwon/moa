//! Certified simulator policies for production Behavior Lab trials.
//!
//! A simulated user is not a real user. The way this surface stays honest about
//! that is not by running more simulators on every release — it is by certifying
//! one simulator policy per scenario domain against consented human interactions,
//! recording exactly what was measured and how uncertain it was, and expiring the
//! certification the instant any pinned input changes.
//!
//! The pieces:
//!
//! * [`registry`] — immutable policy and certification identities.
//! * [`fidelity`] — per-domain predeclared bounds and the certification decision.
//!   Support is checked before bounds, so an underpowered study is
//!   `INCONCLUSIVE`, never a pass and never a fail. There is no universal
//!   threshold constant anywhere in this module.
//! * [`runtime`] — the structured response schema and context compiler identity
//!   shared by certification and the production trial workflow.
//! * [`authorization`] — the gate a live, billed fidelity study must pass:
//!   explicit flag, positive budget, credentials, and human-data authorization.
//! * [`store`] — the tenant-scoped durable registry and study records.
//!
//! Experiment admission resolves one exact certified revision and persists the
//! full immutable snapshot on the run and every trial. The existing production
//! simulator loop consumes that snapshot directly.

pub mod authorization;
pub mod fidelity;
pub mod registry;
pub mod runtime;
pub mod store;

use uuid::Uuid;

use crate::simulator_policy::registry::SimulatorPolicyState;

/// Why a simulator-policy operation was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SimulatorPolicyError {
    /// The scenario-domain slug is not a legal identifier.
    #[error(
        "scenario domain `{domain}` must be 1..=64 lowercase alphanumeric, `-`, `_`, or `.` characters"
    )]
    InvalidDomain {
        /// Offending slug.
        domain: String,
    },
    /// A human interaction cohort pin cannot support an auditable study.
    #[error("invalid human interaction cohort: {detail}")]
    InvalidCohort {
        /// Validation detail.
        detail: String,
    },
    /// The pinned policy components cannot be registered.
    #[error("invalid simulator policy components: {detail}")]
    InvalidComponents {
        /// Validation detail.
        detail: String,
    },
    /// A per-domain fidelity predeclaration cannot decide anything.
    #[error("invalid domain fidelity bounds: {detail}")]
    InvalidBounds {
        /// Validation detail.
        detail: String,
    },
    /// A fidelity measurement is internally inconsistent.
    #[error("invalid fidelity measurement: {detail}")]
    InvalidMeasurement {
        /// Validation detail.
        detail: String,
    },
    /// The stored hash does not match the stored policy body.
    #[error("simulator policy {policy_uid} revision {revision} does not match its stored hash")]
    PolicyHashDrift {
        /// Policy identifier.
        policy_uid: Uuid,
        /// Policy revision.
        revision: i32,
    },
    /// The policy has no certification in force.
    #[error("simulator policy {policy_uid} revision {revision} is {state}, not certified")]
    NotCertified {
        /// Policy identifier.
        policy_uid: Uuid,
        /// Policy revision.
        revision: i32,
        /// Current registry state.
        state: SimulatorPolicyState,
    },
    /// A pinned component changed after the certification was granted.
    #[error(
        "simulator policy {policy_uid} revision {revision} changed a pinned component after study {study_uid} certified it"
    )]
    CertifiedComponentChanged {
        /// Policy identifier.
        policy_uid: Uuid,
        /// Policy revision.
        revision: i32,
        /// Study whose certification no longer applies.
        study_uid: Uuid,
    },
    /// The policy's own validity window does not cover now.
    #[error("simulator policy {policy_uid} revision {revision} validity ended at {valid_until}")]
    OutsideValidityWindow {
        /// Policy identifier.
        policy_uid: Uuid,
        /// Policy revision.
        revision: i32,
        /// End of the validity window.
        valid_until: chrono::DateTime<chrono::Utc>,
    },
    /// The certification window has closed and re-certification is required.
    #[error(
        "simulator policy {policy_uid} revision {revision} certification lapsed at {certified_until}"
    )]
    CertificationLapsed {
        /// Policy identifier.
        policy_uid: Uuid,
        /// Policy revision.
        revision: i32,
        /// End of the certification window.
        certified_until: chrono::DateTime<chrono::Utc>,
    },
    /// A value could not be canonically serialized for hashing.
    #[error("simulator policy value is not canonicalizable: {detail}")]
    NotCanonicalizable {
        /// Serializer detail.
        detail: String,
    },
    /// The certified protocol cannot be served by this runtime build.
    #[error("simulator policy runtime contract mismatch: {detail}")]
    RuntimeContractMismatch {
        /// Mismatched protocol or context compiler detail.
        detail: String,
    },
    /// A durable registry operation failed.
    #[error("simulator policy storage failure: {detail}")]
    Storage {
        /// Storage detail.
        detail: String,
    },
    /// A durable row disagreed with the shape this build understands.
    #[error("simulator policy row is unreadable: {detail}")]
    UnreadableRow {
        /// Decode detail.
        detail: String,
    },
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the simulator-policy unit tests.

    use chrono::{DateTime, TimeZone, Utc};
    use moa_artifacts::release::Digest32;
    use moa_core::types::identifiers::ModelId;
    use uuid::Uuid;

    use super::fidelity::{
        ClassAgreement, ConfidenceInterval, CriticalClassBound, DisagreementSlice,
        DomainFidelityBounds, EffectEquivalenceBound, FIDELITY_ARTIFACT_VERSION,
        FidelityStudyArtifact, FidelityStudyCost, HumanDataAuthorization, IndependentUnit,
        IntervalMethod, LabelAdjudication, LabelProtocolPin, MinimumSupport, PowerAnalysisPin,
        TreatmentEffectAgreement,
    };
    use super::registry::{
        CertificationWindow, CohortPin, ConsentBasis, DeidentificationMethod,
        ResolvedSimulatorPolicy, ScenarioDomain, SimulatorDecoding, SimulatorPolicy,
        SimulatorPolicyComponents, SimulatorPolicyRecord, SimulatorPolicyState, ValidityWindow,
    };
    use super::runtime::{
        DEFAULT_SIMULATOR_SYSTEM_PROMPT, production_context_contract_hash, production_protocol,
    };

    /// Policy identifier used by every fixture in these tests.
    pub(crate) const POLICY_UID: Uuid = Uuid::from_u128(0x5117_0001_0000_0000_0000_0000_0000_0001);

    /// Study identifier used by the fidelity fixtures.
    pub(crate) const STUDY_UID: Uuid = Uuid::from_u128(0x5117_0002_0000_0000_0000_0000_0000_0002);

    /// Critical outcome class the fixtures bound.
    pub(crate) const CRITICAL_CLASS: &str = "handoff_required";

    /// Returns a fixed UTC instant.
    pub(crate) fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0)
            .single()
            .expect("fixed timestamp")
    }

    /// Returns the domain every fixture uses.
    pub(crate) fn sample_domain() -> ScenarioDomain {
        ScenarioDomain::new("retail-support").expect("fixture domain is valid")
    }

    /// Returns the calibration cohort pin.
    pub(crate) fn calibration_cohort() -> CohortPin {
        CohortPin {
            cohort_id: "calib-2026-q2".to_string(),
            independent_units: 40,
            content_hash: Digest32([0xC0; 32]),
            consent_basis: ConsentBasis::ExplicitParticipantConsent,
            deidentification: DeidentificationMethod::PseudonymizedAndRedacted,
        }
    }

    /// Returns the policy-selection cohort pin.
    pub(crate) fn selection_cohort() -> CohortPin {
        CohortPin {
            cohort_id: "sel-2026-q2".to_string(),
            independent_units: 80,
            content_hash: Digest32([0x51; 32]),
            consent_basis: ConsentBasis::ExplicitParticipantConsent,
            deidentification: DeidentificationMethod::PseudonymizedAndRedacted,
        }
    }

    /// Returns the untouched certification cohort pin.
    pub(crate) fn certification_cohort() -> CohortPin {
        CohortPin {
            cohort_id: "cert-2026-q2".to_string(),
            independent_units: 220,
            content_hash: Digest32([0xCE; 32]),
            consent_basis: ConsentBasis::ExplicitParticipantConsent,
            deidentification: DeidentificationMethod::PseudonymizedAndRedacted,
        }
    }

    /// Returns pinned simulator components for the fixture domain.
    pub(crate) fn components() -> SimulatorPolicyComponents {
        SimulatorPolicyComponents {
            domain: sample_domain(),
            model: ModelId::new("fixture-simulator-model"),
            provider: "fixture-provider".to_string(),
            decoding: SimulatorDecoding {
                temperature_milli: 200,
                max_output_tokens: 512,
                seeded: true,
            },
            system_prompt: DEFAULT_SIMULATOR_SYSTEM_PROMPT.to_string(),
            protocol: production_protocol().expect("fixture protocol hashes"),
            context_contract_hash: production_context_contract_hash()
                .expect("fixture context contract hashes"),
            calibration_cohort: calibration_cohort(),
            validity: ValidityWindow {
                valid_from: at(500_000),
                valid_until: at(2_000_000),
            },
        }
    }

    /// Returns the fixture policy at revision 1.
    pub(crate) fn policy() -> SimulatorPolicy {
        SimulatorPolicy {
            policy_uid: POLICY_UID,
            revision: 1,
            components: components(),
        }
    }

    /// Returns a certified registry record with the given windows.
    pub(crate) fn certified_record(
        certified_from: DateTime<Utc>,
        certified_until: DateTime<Utc>,
        policy_valid_until: DateTime<Utc>,
    ) -> SimulatorPolicyRecord {
        let mut policy = policy();
        policy.components.validity.valid_until = policy_valid_until;
        let hash = policy.policy_hash().expect("fixture policy hashes");
        SimulatorPolicyRecord {
            policy,
            stored_policy_hash: hash,
            state: SimulatorPolicyState::Certified,
            certification: Some(CertificationWindow {
                study_uid: STUDY_UID,
                study_artifact_hash: Digest32([0x5A; 32]),
                certified_policy_hash: hash,
                certified_from,
                certified_until,
            }),
            created_at: at(600_000),
            updated_at: at(600_000),
        }
    }

    /// Returns the immutable certified snapshot used by expansion fixtures.
    pub(crate) fn resolved_policy() -> ResolvedSimulatorPolicy {
        let record = certified_record(at(1_000_000), at(1_900_000), at(2_000_000));
        ResolvedSimulatorPolicy {
            binding: record
                .execution_binding(at(1_500_000))
                .expect("fixture certification is live"),
            components: record.policy.components,
        }
    }

    /// Returns the predeclared bounds for the fixture domain.
    pub(crate) fn bounds() -> DomainFidelityBounds {
        DomainFidelityBounds {
            domain: sample_domain(),
            independent_unit: IndependentUnit::HumanParticipant,
            minimum_support: MinimumSupport {
                selection_units: 60,
                certification_units: 120,
                per_critical_class_units: 100,
                treatment_effect_units_per_arm: 150,
                per_slice_units: 40,
                power_analysis: PowerAnalysisPin {
                    analysis_id: "pa-retail-2026-q2".to_string(),
                    analysis_hash: Digest32([0xD3; 32]),
                    detectable_effect_micro: 50_000,
                    power_permille: 800,
                },
            },
            class_confidence_permille: 950,
            critical_classes: vec![CriticalClassBound {
                class: CRITICAL_CLASS.to_string(),
                min_sensitivity_lower_bound_permille: 800,
                min_specificity_lower_bound_permille: 850,
            }],
            effect_equivalence: EffectEquivalenceBound {
                margin_micro: 50_000,
                method: IntervalMethod::ClusterBootstrapPercentile {
                    resamples: 2_000,
                    seed: 7,
                },
                confidence_permille: 950,
            },
            max_slice_disagreement_permille: Some(100),
            recertification_interval_days: 90,
        }
    }

    /// Returns a study artifact that meets every predeclared bound.
    pub(crate) fn passing_artifact() -> FidelityStudyArtifact {
        let components = components();
        FidelityStudyArtifact {
            artifact_version: FIDELITY_ARTIFACT_VERSION,
            study_uid: STUDY_UID,
            policy_uid: POLICY_UID,
            policy_revision: 1,
            policy_hash: policy().policy_hash().expect("fixture policy hashes"),
            simulator_components: components,
            domain: sample_domain(),
            bounds: bounds(),
            selection_cohort: selection_cohort(),
            certification_cohort: certification_cohort(),
            label_protocol: LabelProtocolPin {
                protocol_id: "label-retail-handoff".to_string(),
                version: 3,
                rubric_hash: Digest32([0xE4; 32]),
                adjudication: LabelAdjudication::IndependentWithAdjudication,
                annotators: 3,
                agreement_permille: Some(880),
            },
            class_agreement: vec![ClassAgreement {
                class: CRITICAL_CLASS.to_string(),
                true_positive: 110,
                false_negative: 6,
                true_negative: 100,
                false_positive: 4,
                independent_units: 220,
            }],
            disagreement_slices: vec![DisagreementSlice {
                slice: "escalation".to_string(),
                simulated_rate_permille: 320,
                human_rate_permille: 280,
                independent_units: 90,
            }],
            effect_agreement: Some(TreatmentEffectAgreement {
                simulated_effect_micro: 120_000,
                human_effect_micro: 100_000,
                difference_interval: ConfidenceInterval {
                    low_micro: -10_000,
                    high_micro: 30_000,
                    confidence_permille: 950,
                    method: IntervalMethod::ClusterBootstrapPercentile {
                        resamples: 2_000,
                        seed: 7,
                    },
                },
                simulated_units: 400,
                human_units: 180,
            }),
            cost: FidelityStudyCost {
                budget_micro_usd: 5_000_000,
                spent_micro_usd: 4_100_000,
                simulator_calls: 4_400,
                human_units_consumed: 300,
            },
            authorization: HumanDataAuthorization {
                authorization_id: "hda-2026-q2".to_string(),
                approved_by: "privacy-review".to_string(),
                approved_at: at(1_000_000),
                expires_at: at(2_000_000),
            },
            observed_at: at(1_400_000),
        }
    }
}

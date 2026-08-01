//! Shared integration-test fixtures for certified simulator policy snapshots.

use chrono::Duration;
use moa_artifacts::release::Digest32;
use moa_core::types::identifiers::ModelId;
use moa_experiments::simulator_policy::registry::{
    CohortPin, ConsentBasis, DeidentificationMethod, ResolvedSimulatorPolicy, ScenarioDomain,
    SimulatorDecoding, SimulatorPolicy, SimulatorPolicyBinding, SimulatorPolicyComponents,
    ValidityWindow,
};
use moa_experiments::simulator_policy::runtime::{
    DEFAULT_SIMULATOR_SYSTEM_PROMPT, production_context_contract_hash, production_protocol,
};
use uuid::Uuid;

/// Returns a coherent certified policy snapshot for persistence tests.
pub(crate) fn simulator_policy(model: &str) -> ResolvedSimulatorPolicy {
    let now = moa_test_support::fixtures::pg_now();
    let components = SimulatorPolicyComponents {
        domain: ScenarioDomain::new("test-support").expect("fixture domain"),
        model: ModelId::new(model),
        provider: "fixture-provider".to_string(),
        decoding: SimulatorDecoding {
            temperature_milli: 200,
            max_output_tokens: 512,
            seeded: true,
        },
        system_prompt: DEFAULT_SIMULATOR_SYSTEM_PROMPT.to_string(),
        protocol: production_protocol().expect("production protocol hashes"),
        context_contract_hash: production_context_contract_hash()
            .expect("production context contract hashes"),
        calibration_cohort: CohortPin {
            cohort_id: "fixture-calibration".to_string(),
            independent_units: 10,
            content_hash: Digest32([0x11; 32]),
            consent_basis: ConsentBasis::AuthorizedInternalDogfood,
            deidentification: DeidentificationMethod::SyntheticSurrogate,
        },
        validity: ValidityWindow {
            valid_from: now - Duration::days(1),
            valid_until: now + Duration::days(30),
        },
    };
    let policy = SimulatorPolicy {
        policy_uid: Uuid::now_v7(),
        revision: 1,
        components: components.clone(),
    };
    ResolvedSimulatorPolicy {
        binding: SimulatorPolicyBinding {
            policy_uid: policy.policy_uid,
            revision: policy.revision,
            policy_hash: policy.policy_hash().expect("fixture policy hashes"),
            study_uid: Uuid::now_v7(),
            study_artifact_hash: Digest32([0x22; 32]),
            evaluator_version: 1,
            certified_until: now + Duration::days(30),
        },
        components,
    }
}

//! Certified simulator-policy snapshot fixtures for DTO and persistence tests.

use chrono::{Duration, Utc};
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

/// Returns a coherent snapshot for tests that do not execute the simulator.
pub fn fixture(model: &str) -> ResolvedSimulatorPolicy {
    let now = Utc::now();
    let components = SimulatorPolicyComponents {
        domain: ScenarioDomain::new("orchestrator-test").expect("fixture domain"),
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
            cohort_id: "orchestrator-test".to_string(),
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

/// Seeds one certified registry row for tests whose subject starts at admission.
pub async fn seed_certified(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    policy_uid: Uuid,
    model: &str,
) -> anyhow::Result<ResolvedSimulatorPolicy> {
    use moa_core::types::memory::RlsContext;
    use moa_db::ScopedConn;
    use moa_experiments::simulator_policy::store::SimulatorPolicyStore;

    let snapshot = fixture(model);
    let policy = SimulatorPolicy {
        policy_uid,
        revision: 1,
        components: snapshot.components,
    };
    let policy_hash = policy.policy_hash()?;
    let study_uid = Uuid::now_v7();
    let study_hash = Digest32([0x33; 32]);
    let now = Utc::now();
    SimulatorPolicyStore::new(pool.clone())
        .register_policy(tenant_id, &policy)
        .await?;
    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(tenant_id)).await?;
    sqlx::query(
        r#"
        UPDATE moa.simulator_policy
        SET state = 'certified',
            certification_study_uid = $1,
            certification_artifact_hash = $2,
            certified_policy_hash = $3,
            certified_from = $4,
            certified_until = $5
        WHERE policy_uid = $6 AND revision = 1
        "#,
    )
    .bind(study_uid)
    .bind(study_hash.0.as_slice())
    .bind(policy_hash.0.as_slice())
    .bind(now - Duration::minutes(1))
    .bind(now + Duration::days(30))
    .bind(policy_uid)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    SimulatorPolicyStore::new(pool.clone())
        .resolve_policy(tenant_id, policy.reference(), now)
        .await
        .map_err(Into::into)
}

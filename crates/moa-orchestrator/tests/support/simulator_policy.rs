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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub async fn seed_certified(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    policy_uid: Uuid,
    model: &str,
    provider: &str,
) -> anyhow::Result<ResolvedSimulatorPolicy> {
    use moa_core::types::memory::RlsContext;
    use moa_db::ScopedConn;
    use moa_experiments::simulator_policy::store::SimulatorPolicyStore;

    let mut snapshot = fixture(model);
    snapshot.components.provider = provider.to_string();
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

/// Records passing synthetic-surrogate fidelity evidence for the migrated
/// platform release simulator through the production certification store.
pub async fn certify_platform_release(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> anyhow::Result<ResolvedSimulatorPolicy> {
    use moa_artifacts::release::{
        PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID, PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
    };
    use moa_experiments::simulator_policy::fidelity::{
        ClassAgreement, ConfidenceInterval, CriticalClassBound, DisagreementSlice,
        DomainFidelityBounds, EffectEquivalenceBound, FIDELITY_ARTIFACT_VERSION,
        FidelityStudyArtifact, FidelityStudyCost, HumanDataAuthorization, IndependentUnit,
        IntervalMethod, LabelAdjudication, LabelProtocolPin, MinimumSupport, PowerAnalysisPin,
        TreatmentEffectAgreement,
    };
    use moa_experiments::simulator_policy::store::SimulatorPolicyStore;

    const CRITICAL_CLASS: &str = "release_boundary_preserved";

    let store = SimulatorPolicyStore::new(pool.clone());
    let record = store
        .load_policy(tenant_id, PLATFORM_RELEASE_SIMULATOR_POLICY_UID, 1)
        .await?
        .ok_or_else(|| anyhow::anyhow!("migrated platform release simulator policy is absent"))?;
    let now = Utc::now();
    let cohort = |id: &str, units: u32, fill: u8| CohortPin {
        cohort_id: id.to_string(),
        independent_units: units,
        content_hash: Digest32([fill; 32]),
        consent_basis: ConsentBasis::AuthorizedInternalDogfood,
        deidentification: DeidentificationMethod::SyntheticSurrogate,
    };
    let domain = record.policy.components.domain.clone();
    let artifact = FidelityStudyArtifact {
        artifact_version: FIDELITY_ARTIFACT_VERSION,
        study_uid: Uuid::now_v7(),
        policy_uid: record.policy.policy_uid,
        policy_revision: record.policy.revision,
        policy_hash: record.policy.policy_hash()?,
        simulator_components: record.policy.components.clone(),
        domain: domain.clone(),
        bounds: DomainFidelityBounds {
            domain,
            independent_unit: IndependentUnit::HumanParticipant,
            minimum_support: MinimumSupport {
                selection_units: 60,
                certification_units: 120,
                per_critical_class_units: 100,
                treatment_effect_units_per_arm: 150,
                per_slice_units: 40,
                power_analysis: PowerAnalysisPin {
                    analysis_id: "platform-release-fixture-power-v1".to_string(),
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
        },
        selection_cohort: record.policy.components.calibration_cohort.clone(),
        certification_cohort: cohort("platform-release-fixture-certification", 220, 0xCE),
        label_protocol: LabelProtocolPin {
            protocol_id: "platform-release-boundary-label-v1".to_string(),
            version: 1,
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
            slice: "hidden-safety".to_string(),
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
            authorization_id: "platform-release-fixture-authorization".to_string(),
            approved_by: "fixture-privacy-review".to_string(),
            approved_at: now - Duration::days(1),
            expires_at: now + Duration::days(30),
        },
        observed_at: now - Duration::minutes(1),
    };
    let source_manifest_hash = Digest32([0xA5; 32]);
    sqlx::query("DELETE FROM moa.simulator_certification_mandate WHERE mandate_uid = $1")
        .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
        .execute(pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.simulator_certification_mandate (
            mandate_uid, storage_partition_id, user_id, policy_uid,
            policy_revision, policy_hash, domain, bounds, selection_cohort,
            certification_cohort, label_protocol, human_data_authorization,
            study_budget_micro_usd, required_source_manifest_hash,
            study_window_from, study_window_until, predeclared_at
        )
        VALUES (
            $1, NULL, NULL, $2, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15
        )
        "#,
    )
    .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
    .bind(artifact.policy_uid)
    .bind(artifact.policy_revision)
    .bind(artifact.policy_hash.to_vec())
    .bind(artifact.domain.as_str())
    .bind(serde_json::to_value(&artifact.bounds)?)
    .bind(serde_json::to_value(&artifact.selection_cohort)?)
    .bind(serde_json::to_value(&artifact.certification_cohort)?)
    .bind(serde_json::to_value(&artifact.label_protocol)?)
    .bind(serde_json::to_value(&artifact.authorization)?)
    .bind(i64::try_from(artifact.cost.budget_micro_usd)?)
    .bind(source_manifest_hash.to_vec())
    .bind(artifact.observed_at - Duration::minutes(1))
    .bind(artifact.observed_at + Duration::days(1))
    .bind(artifact.observed_at - Duration::minutes(1))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.simulator_certification_evidence_import (
            mandate_uid, storage_partition_id, user_id, study_uid,
            study_artifact_hash, source_manifest_hash, source_reference,
            imported_by
        )
        VALUES ($1, NULL, NULL, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
    .bind(artifact.study_uid)
    .bind(artifact.digest()?.to_vec())
    .bind(source_manifest_hash.to_vec())
    .bind("fixture://reviewed-source-manifest")
    .bind("fixture-independent-reviewer")
    .execute(pool)
    .await?;
    let outcome = store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &artifact,
            now,
        )
        .await?;
    anyhow::ensure!(
        outcome.verdict() == "certified",
        "fixture fidelity study did not certify"
    );
    store
        .resolve_policy(tenant_id, record.policy.reference(), now)
        .await
        .map_err(Into::into)
}

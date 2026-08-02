//! Durable behavior of the simulator-policy registry (`V000047`).
//!
//! These run against a real Postgres because every property under test is
//! enforced by the schema or by a transaction: pinned-component immutability,
//! cohort independence, and the certified-hash predicate that makes a study
//! unable to certify a policy body it did not measure.
//!
//! This is a standalone `_db` binary rather than a module inside
//! `experiment_store_db.rs` deliberately: that harness serializes every test in
//! the file behind one process-wide mutex it owns, and these tests each bootstrap
//! their own isolated database and are safe to run concurrently.

use chrono::{DateTime, TimeZone, Utc};
use moa_artifacts::release::{
    Digest32, PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
    PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION, PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
};
use moa_core::error::Result;
use moa_core::types::identifiers::{ModelId, TenantId};
use moa_experiments::simulator_policy::SimulatorPolicyError;
use moa_experiments::simulator_policy::fidelity::{
    ClassAgreement, ConfidenceInterval, CriticalClassBound, DisagreementSlice,
    DomainFidelityBounds, EffectEquivalenceBound, FIDELITY_ARTIFACT_VERSION, FidelityStudyArtifact,
    FidelityStudyCost, HumanDataAuthorization, IndependentUnit, IntervalMethod, LabelAdjudication,
    LabelProtocolPin, MinimumSupport, PowerAnalysisPin, TreatmentEffectAgreement,
};
use moa_experiments::simulator_policy::registry::{
    CohortPin, ConsentBasis, DeidentificationMethod, ScenarioDomain, SimulatorDecoding,
    SimulatorPolicy, SimulatorPolicyComponents, SimulatorPolicyState, ValidityWindow,
};
use moa_experiments::simulator_policy::runtime::{
    DEFAULT_SIMULATOR_SYSTEM_PROMPT, production_context_contract_hash, production_protocol,
};
use moa_experiments::simulator_policy::store::SimulatorPolicyStore;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const CRITICAL_CLASS: &str = "handoff_required";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0)
        .single()
        .expect("fixed timestamp")
}

fn domain() -> ScenarioDomain {
    ScenarioDomain::new("retail-support").expect("valid domain")
}

fn components(model: &str) -> SimulatorPolicyComponents {
    SimulatorPolicyComponents {
        domain: domain(),
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
        calibration_cohort: selection_cohort(),
        validity: ValidityWindow {
            valid_from: at(500_000),
            valid_until: at(2_000_000),
        },
    }
}

fn cohort(id: &str, units: u32, fill: u8) -> CohortPin {
    CohortPin {
        cohort_id: id.to_string(),
        independent_units: units,
        content_hash: Digest32([fill; 32]),
        consent_basis: ConsentBasis::ExplicitParticipantConsent,
        deidentification: DeidentificationMethod::PseudonymizedAndRedacted,
    }
}

fn selection_cohort() -> CohortPin {
    cohort("sel-2026-q2", 80, 0x51)
}

fn policy(policy_uid: Uuid, revision: i32, model: &str) -> SimulatorPolicy {
    SimulatorPolicy {
        policy_uid,
        revision,
        components: components(model),
    }
}

fn bounds(domain: ScenarioDomain) -> DomainFidelityBounds {
    DomainFidelityBounds {
        domain,
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

fn artifact(policy: &SimulatorPolicy, study_uid: Uuid) -> FidelityStudyArtifact {
    let domain = policy.components.domain.clone();
    FidelityStudyArtifact {
        artifact_version: FIDELITY_ARTIFACT_VERSION,
        study_uid,
        policy_uid: policy.policy_uid,
        policy_revision: policy.revision,
        policy_hash: policy.policy_hash().expect("policy hashes"),
        simulator_components: policy.components.clone(),
        domain: domain.clone(),
        bounds: bounds(domain),
        selection_cohort: policy.components.calibration_cohort.clone(),
        certification_cohort: cohort("cert-2026-q2", 220, 0xCE),
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

async fn stored_state(pool: &PgPool, policy_uid: Uuid, revision: i32) -> Result<String> {
    let state: String = sqlx::query_scalar(
        "SELECT state FROM moa.simulator_policy WHERE policy_uid = $1 AND revision = $2",
    )
    .bind(policy_uid)
    .bind(revision)
    .fetch_one(pool)
    .await
    .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    Ok(state)
}

async fn replace_platform_mandate(
    pool: &PgPool,
    artifact: &FidelityStudyArtifact,
    source_manifest_hash: Digest32,
) {
    sqlx::query("DELETE FROM moa.simulator_certification_mandate WHERE mandate_uid = $1")
        .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
        .execute(pool)
        .await
        .expect("test owner replaces the unprovisioned migration mandate");
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
    .bind(serde_json::to_value(&artifact.bounds).expect("bounds serialize"))
    .bind(serde_json::to_value(&artifact.selection_cohort).expect("selection cohort serializes"))
    .bind(
        serde_json::to_value(&artifact.certification_cohort)
            .expect("certification cohort serializes"),
    )
    .bind(serde_json::to_value(&artifact.label_protocol).expect("label protocol serializes"))
    .bind(serde_json::to_value(&artifact.authorization).expect("authorization serializes"))
    .bind(i64::try_from(artifact.cost.budget_micro_usd).expect("fixture budget fits i64"))
    .bind(source_manifest_hash.to_vec())
    .bind(artifact.observed_at - chrono::Duration::days(1))
    .bind(artifact.observed_at + chrono::Duration::days(1))
    .bind(artifact.observed_at - chrono::Duration::days(1))
    .execute(pool)
    .await
    .expect("insert independently provisioned test mandate");
}

async fn import_platform_evidence(
    pool: &PgPool,
    artifact: &FidelityStudyArtifact,
    source_manifest_hash: Digest32,
) {
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
    .bind(artifact.digest().expect("fixture artifact hashes").to_vec())
    .bind(source_manifest_hash.to_vec())
    .bind("fixture://reviewed-source-manifest")
    .bind("fixture-independent-reviewer")
    .execute(pool)
    .await
    .expect("import independently reviewed test evidence");
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn certified_exact_policy_resolves_with_tenant_isolation_db() -> Result<()> {
    // Pins: only an exact certified revision resolves, its snapshot is bounded by
    // both windows, and another tenant cannot observe it through RLS.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = SimulatorPolicyStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId(Uuid::new_v4());
    let policy = policy(Uuid::new_v4(), 1, "primary-simulator");

    let registered = store
        .register_policy(tenant_id, &policy)
        .await
        .expect("register the policy");
    assert_eq!(registered.state, SimulatorPolicyState::Draft);
    assert!(registered.certification.is_none());

    // A draft policy refuses to resolve, so nothing uncertified reaches execution.
    let refusal = store
        .resolve_policy(tenant_id, policy.reference(), at(1_500_000))
        .await
        .expect_err("a draft policy must not resolve a binding");
    assert!(
        matches!(refusal, SimulatorPolicyError::NotCertified { .. }),
        "expected NotCertified, got {refusal}"
    );

    let artifact = artifact(&policy, Uuid::new_v4());
    let outcome = store
        .record_study(tenant_id, &artifact, at(1_500_000))
        .await
        .expect("record the passing study");
    assert_eq!(outcome.verdict(), "certified");

    assert_eq!(
        stored_state(test_db.store().pool(), policy.policy_uid, 1).await?,
        "certified"
    );

    let resolved = store
        .resolve_policy(tenant_id, policy.reference(), at(1_500_000))
        .await
        .expect("a certified policy must resolve a binding");
    assert_eq!(resolved.binding.policy_uid, policy.policy_uid);
    assert_eq!(resolved.binding.revision, 1);
    assert_eq!(
        resolved.binding.policy_hash,
        policy.policy_hash().expect("policy hashes")
    );
    let window = outcome.window().expect("certified window");
    assert_eq!(
        resolved.binding.certified_until,
        window
            .certified_until
            .min(policy.components.validity.valid_until),
        "the published binding must not outlive either window"
    );
    assert_eq!(resolved.components.model.as_str(), "primary-simulator");
    assert!(
        store
            .load_policy(TenantId(Uuid::new_v4()), policy.policy_uid, policy.revision)
            .await
            .expect("cross-tenant load executes")
            .is_none(),
        "another tenant must not see the policy row"
    );

    // Past the certification window the same row refuses to bind.
    let lapsed = store
        .resolve_policy(
            tenant_id,
            policy.reference(),
            window
                .certified_until
                .min(policy.components.validity.valid_until),
        )
        .await
        .expect_err("a lapsed certification must refuse");
    assert!(
        matches!(
            lapsed,
            SimulatorPolicyError::CertificationLapsed { .. }
                | SimulatorPolicyError::OutsideValidityWindow { .. }
        ),
        "expected a lapse, got {lapsed}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn global_release_policy_requires_independent_mandate_and_evidence_import_db() -> Result<()> {
    // Pins: a caller-authored study cannot choose its own bounds, authorization,
    // or source evidence. Certification requires the fixed migration-owned
    // mandate plus an independently imported approval of the exact artifact hash.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let owner_pool = test_db.store().pool().clone();
    let tenant_id = TenantId(Uuid::new_v4());
    let tenant_store = SimulatorPolicyStore::new(owner_pool.clone());
    let record = tenant_store
        .load_policy(
            tenant_id,
            PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
            PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
        )
        .await
        .expect("load migrated global platform policy")
        .expect("global platform policy exists");
    let now = Utc::now();
    let mut evidence = artifact(&record.policy, Uuid::now_v7());
    evidence.authorization.approved_at = now - chrono::Duration::days(1);
    evidence.authorization.expires_at = now + chrono::Duration::days(1);
    evidence.observed_at = now;

    let tenant_refusal = tenant_store
        .record_study(tenant_id, &evidence, evidence.observed_at)
        .await
        .expect_err("tenant certification must not address the global row");
    assert!(
        matches!(tenant_refusal, SimulatorPolicyError::NotCertified { .. }),
        "expected tenant/global scope refusal, got {tenant_refusal}"
    );

    let promoter_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET ROLE moa_promoter")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(test_db.database_url())
        .await
        .expect("connect with the promoter role");
    let promoter_store = SimulatorPolicyStore::new(promoter_pool.clone());

    let promoter_rewrite = sqlx::query(
        "UPDATE moa.simulator_certification_mandate SET study_budget_micro_usd = 1 WHERE mandate_uid = $1",
    )
    .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
    .execute(&promoter_pool)
    .await;
    assert!(
        promoter_rewrite.is_err(),
        "the evidence importer must not rewrite migration-owned authority"
    );
    let promoter_delete =
        sqlx::query("DELETE FROM moa.simulator_certification_mandate WHERE mandate_uid = $1")
            .bind(PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID)
            .execute(&promoter_pool)
            .await;
    assert!(
        promoter_delete.is_err(),
        "the evidence importer must not delete migration-owned authority"
    );

    let unprovisioned = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &evidence,
            Utc::now(),
        )
        .await
        .expect_err("the migration mandate intentionally carries no external evidence authority");
    assert!(
        matches!(
            unprovisioned,
            SimulatorPolicyError::CertificationMandateMismatch { .. }
        ),
        "expected unprovisioned mandate refusal, got {unprovisioned}"
    );

    let source_manifest_hash = Digest32([0xA5; 32]);
    replace_platform_mandate(&owner_pool, &evidence, source_manifest_hash).await;
    let missing_import = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &evidence,
            Utc::now(),
        )
        .await
        .expect_err("a mandate without exact imported evidence must not certify");
    assert!(
        matches!(
            missing_import,
            SimulatorPolicyError::CertificationEvidenceMissing { .. }
        ),
        "expected missing-evidence refusal, got {missing_import}"
    );

    let mut component_drift = evidence.clone();
    component_drift.simulator_components.provider = "different-provider".to_string();
    component_drift.policy_hash = SimulatorPolicy {
        policy_uid: component_drift.policy_uid,
        revision: component_drift.policy_revision,
        components: component_drift.simulator_components.clone(),
    }
    .policy_hash()
    .expect("drifted policy hashes");
    let drift_refusal = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &component_drift,
            Utc::now(),
        )
        .await
        .expect_err("evidence for different components must not certify the stored policy");
    assert!(
        matches!(
            drift_refusal,
            SimulatorPolicyError::CertificationMandateMismatch { .. }
        ),
        "expected mandate component-drift refusal, got {drift_refusal}"
    );

    let mut cohort_drift = evidence.clone();
    cohort_drift.selection_cohort.cohort_id = "different-selection".to_string();
    let cohort_refusal = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &cohort_drift,
            Utc::now(),
        )
        .await
        .expect_err("evidence from another selection cohort must be refused");
    assert!(
        matches!(
            cohort_refusal,
            SimulatorPolicyError::CertificationMandateMismatch { .. }
        ),
        "expected mandate cohort refusal, got {cohort_refusal}"
    );

    let mut easier_bounds = evidence.clone();
    easier_bounds
        .bounds
        .critical_classes
        .first_mut()
        .expect("one critical class")
        .min_sensitivity_lower_bound_permille = 1;
    let bounds_refusal = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &easier_bounds,
            Utc::now(),
        )
        .await
        .expect_err("a study cannot choose easier bounds than the mandate");
    assert!(
        matches!(
            bounds_refusal,
            SimulatorPolicyError::CertificationMandateMismatch { .. }
        ),
        "expected predeclared-bounds refusal, got {bounds_refusal}"
    );

    let mut authorization_drift = evidence.clone();
    authorization_drift.authorization.authorization_id = "caller-selected".to_string();
    let authorization_refusal = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &authorization_drift,
            Utc::now(),
        )
        .await
        .expect_err("a study cannot self-select human-data authorization");
    assert!(
        matches!(
            authorization_refusal,
            SimulatorPolicyError::CertificationMandateMismatch { .. }
        ),
        "expected independent-authorization refusal, got {authorization_refusal}"
    );

    import_platform_evidence(&owner_pool, &evidence, source_manifest_hash).await;

    let mut changed_after_import = evidence.clone();
    changed_after_import.cost.spent_micro_usd += 1;
    let changed_refusal = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &changed_after_import,
            Utc::now(),
        )
        .await
        .expect_err("the imported approval applies only to exact canonical artifact bytes");
    assert!(
        matches!(
            changed_refusal,
            SimulatorPolicyError::CertificationEvidenceMismatch { .. }
        ),
        "expected exact-evidence refusal, got {changed_refusal}"
    );

    let outcome = promoter_store
        .record_platform_study(
            PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
            &evidence,
            Utc::now(),
        )
        .await
        .expect("record independently authorized platform evidence as promoter");
    assert_eq!(outcome.verdict(), "certified");
    let resolved = promoter_store
        .resolve_platform_policy(record.policy.reference(), Utc::now())
        .await
        .expect("certified global policy resolves for the operator");
    assert_eq!(
        resolved.binding.policy_uid,
        PLATFORM_RELEASE_SIMULATOR_POLICY_UID
    );
    assert_eq!(
        resolved.binding.policy_hash,
        record.policy.policy_hash().expect("platform policy hashes")
    );

    let global_studies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.simulator_fidelity_study WHERE storage_partition_id IS NULL",
    )
    .fetch_one(&owner_pool)
    .await
    .expect("count global studies");
    assert_eq!(
        global_studies, 1,
        "refused tenant, mandate, and evidence drift must not persist"
    );
    promoter_pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn pinned_components_cannot_be_rewritten_in_place_db() -> Result<()> {
    // Pins: the database itself refuses to change a pinned component, so a
    // certification can never come to describe a different simulator. Without the
    // trigger, editing the prompt in place would silently inherit the
    // certification recorded against the old body.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = SimulatorPolicyStore::new(pool.clone());
    let tenant_id = TenantId(Uuid::new_v4());
    let policy = policy(Uuid::new_v4(), 1, "primary-simulator");
    store
        .register_policy(tenant_id, &policy)
        .await
        .expect("register the policy");

    let rewrite = sqlx::query(
        r#"
        UPDATE moa.simulator_policy
        SET components = jsonb_set(components, '{provider}', '"other-provider"')
        WHERE policy_uid = $1 AND revision = $2
        "#,
    )
    .bind(policy.policy_uid)
    .bind(1_i32)
    .execute(&pool)
    .await;
    let error = rewrite.expect_err("rewriting pinned components must be refused");
    assert!(
        error.to_string().contains("immutable"),
        "expected the immutability guard, got {error}"
    );

    // Registering a different body under the same revision is refused too, so the
    // only way to change a policy is a new revision.
    let mut edited = policy.clone();
    edited.components.system_prompt.push_str(" changed");
    let drift = store
        .register_policy(tenant_id, &edited)
        .await
        .expect_err("a different body at the same revision must be refused");
    assert!(
        matches!(drift, SimulatorPolicyError::PolicyHashDrift { .. }),
        "expected PolicyHashDrift, got {drift}"
    );

    // The same body re-registers idempotently.
    let again = store
        .register_policy(tenant_id, &policy)
        .await
        .expect("identical re-registration is idempotent");
    assert_eq!(
        again.stored_policy_hash,
        policy.policy_hash().expect("policy hashes")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn failed_and_inconclusive_studies_never_certify_db() -> Result<()> {
    // Pins: a bound violation marks the policy rejected and an underpowered study
    // leaves it exactly as it was. Neither writes a certification window, so
    // neither can back a release run.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let store = SimulatorPolicyStore::new(pool.clone());
    let tenant_id = TenantId(Uuid::new_v4());

    let failing_policy = policy(Uuid::new_v4(), 1, "failing-simulator");
    store
        .register_policy(tenant_id, &failing_policy)
        .await
        .expect("register");
    let mut failing = artifact(&failing_policy, Uuid::new_v4());
    let row = failing
        .class_agreement
        .first_mut()
        .expect("one measured class");
    row.true_positive = 60;
    row.false_negative = 56;
    let outcome = store
        .record_study(tenant_id, &failing, at(1_500_000))
        .await
        .expect("record the failing study");
    assert_eq!(outcome.verdict(), "failed");
    assert_eq!(
        stored_state(&pool, failing_policy.policy_uid, 1).await?,
        "rejected"
    );
    let loaded = store
        .load_policy(tenant_id, failing_policy.policy_uid, 1)
        .await
        .expect("load")
        .expect("row exists");
    assert!(
        loaded.certification.is_none(),
        "a failed study must write no certification window"
    );
    assert!(loaded.execution_binding(at(1_500_000)).is_err());

    let thin_policy = policy(Uuid::new_v4(), 1, "underpowered-simulator");
    store
        .register_policy(tenant_id, &thin_policy)
        .await
        .expect("register");
    let mut thin = artifact(&thin_policy, Uuid::new_v4());
    thin.certification_cohort.independent_units = 10;
    let thin_row = thin.class_agreement.first_mut().expect("one class");
    thin_row.true_positive = 5;
    thin_row.false_negative = 0;
    thin_row.true_negative = 5;
    thin_row.false_positive = 0;
    thin_row.independent_units = 10;
    let thin_outcome = store
        .record_study(tenant_id, &thin, at(1_500_000))
        .await
        .expect("record the inconclusive study");
    assert_eq!(thin_outcome.verdict(), "inconclusive");
    assert_eq!(
        stored_state(&pool, thin_policy.policy_uid, 1).await?,
        "draft",
        "an inconclusive study decides nothing"
    );

    // Both studies are kept as evidence, with their verdicts.
    let verdicts: Vec<String> =
        sqlx::query_scalar("SELECT verdict FROM moa.simulator_fidelity_study ORDER BY verdict ASC")
            .fetch_all(&pool)
            .await
            .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
    assert_eq!(verdicts, vec!["failed", "inconclusive"]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn certification_cohort_reuse_is_refused_by_the_schema_db() -> Result<()> {
    // Pins: the schema refuses a study whose certification cohort is its selection
    // cohort, by id or by content. The certifier reports the same thing as a bound
    // violation; this is the layer that makes the row unwritable at all.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = SimulatorPolicyStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId(Uuid::new_v4());
    let policy = policy(Uuid::new_v4(), 1, "reuse-simulator");
    store
        .register_policy(tenant_id, &policy)
        .await
        .expect("register");

    let mut reused = artifact(&policy, Uuid::new_v4());
    reused.certification_cohort.cohort_id = reused.selection_cohort.cohort_id.clone();
    let error = store
        .record_study(tenant_id, &reused, at(1_500_000))
        .await
        .expect_err("a study reusing its selection cohort must not persist");
    assert!(
        matches!(error, SimulatorPolicyError::Storage { .. }),
        "expected a storage refusal, got {error}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn fidelity_study_replay_is_idempotent_but_identity_drift_is_refused_db() -> Result<()> {
    // Pins: retrying identical evidence is safe, while reusing a study id for
    // different canonical bytes cannot rewrite the certification record.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let store = SimulatorPolicyStore::new(test_db.store().pool().clone());
    let tenant_id = TenantId(Uuid::new_v4());
    let policy = policy(Uuid::new_v4(), 1, "replay-simulator");
    store
        .register_policy(tenant_id, &policy)
        .await
        .expect("register");

    let original = artifact(&policy, Uuid::new_v4());
    let first = store
        .record_study(tenant_id, &original, at(1_500_000))
        .await
        .expect("first study write");
    let replay = store
        .record_study(tenant_id, &original, at(1_500_000))
        .await
        .expect("identical replay is idempotent");
    assert_eq!(replay, first);

    let mut changed = original;
    changed.cost.spent_micro_usd += 1;
    let error = store
        .record_study(tenant_id, &changed, at(1_500_000))
        .await
        .expect_err("changed bytes under the same study id must fail");
    assert!(
        matches!(error, SimulatorPolicyError::Storage { .. }),
        "expected identity-drift refusal, got {error}"
    );
    Ok(())
}

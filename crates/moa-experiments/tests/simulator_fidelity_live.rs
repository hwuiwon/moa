//! Authorized entry point for certifying a real fidelity study.
//!
//! This binary is the only place a study measured on *real, consented human
//! interactions* is turned into a certification. It never runs by default: every
//! test here is `#[ignore]`d, and each one re-checks its own authorization so a
//! lane that schedules ignored tests still cannot spend money or read human data
//! by accident. Four independent things are required, and the checks are the
//! production ones from
//! `moa_experiments::simulator_policy::authorization`:
//!
//! * `MOA_RUN_LIVE_FIDELITY_TESTS=1` — explicit opt-in;
//! * `MOA_FIDELITY_STUDY_BUDGET_USD` — a positive budget, which must also cover
//!   the spend the study artifact records;
//! * `MOA_FIDELITY_SIMULATOR_API_KEY` — simulator provider credentials;
//! * `MOA_FIDELITY_HUMAN_DATA_AUTHORIZATION` — the approved human-data
//!   authorization record, which must match the one the artifact ran under.
//!
//! The measurements themselves come from a study run, supplied as a canonical
//! fidelity-artifact JSON document at `MOA_FIDELITY_STUDY_ARTIFACT_PATH`. This
//! binary does not fabricate measurements and contains no fixture cohort: if the
//! artifact is absent the run fails loudly rather than certifying anything.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use moa_core::types::identifiers::TenantId;
use moa_experiments::simulator_policy::authorization::{
    LIVE_FIDELITY_HUMAN_DATA_ENV, LiveFidelityAuthorization,
};
use moa_experiments::simulator_policy::fidelity::{CertificationOutcome, FidelityStudyArtifact};
use moa_experiments::simulator_policy::store::SimulatorPolicyStore;
use uuid::Uuid;

/// Environment variable naming the canonical study artifact to certify.
const ARTIFACT_PATH_ENV: &str = "MOA_FIDELITY_STUDY_ARTIFACT_PATH";

/// Environment variable naming the tenant whose registry receives the study.
const TENANT_ENV: &str = "MOA_FIDELITY_TENANT_ID";

/// Resolves the four authorizations plus the artifact the study produced.
fn authorized_artifact() -> Result<(LiveFidelityAuthorization, FidelityStudyArtifact, TenantId)> {
    let authorization = LiveFidelityAuthorization::from_env()
        .map_err(|refusal| anyhow::anyhow!("live fidelity study is not authorized: {refusal}"))?;

    let path: PathBuf = std::env::var(ARTIFACT_PATH_ENV)
        .with_context(|| format!("live fidelity certification requires {ARTIFACT_PATH_ENV}"))?
        .into();
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read fidelity study artifact at {}", path.display()))?;
    let artifact: FidelityStudyArtifact = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode fidelity study artifact at {}", path.display()))?;
    artifact
        .validate()
        .context("supplied fidelity study artifact is not internally consistent")?;

    // The declared budget must cover what the study actually spent. Without this
    // the budget variable would be decoration: an operator could authorize one
    // dollar and certify a study that spent a thousand.
    ensure!(
        artifact.cost.spent_micro_usd <= authorization.budget_micro_usd,
        "study spent {} micro-USD, above the authorized {} micro-USD",
        artifact.cost.spent_micro_usd,
        authorization.budget_micro_usd
    );
    ensure!(
        artifact.authorization.authorization_id == authorization.human_data_authorization_id,
        "study ran under human-data authorization `{}` but {LIVE_FIDELITY_HUMAN_DATA_ENV} names `{}`",
        artifact.authorization.authorization_id,
        authorization.human_data_authorization_id
    );

    let tenant_id = TenantId(
        std::env::var(TENANT_ENV)
            .with_context(|| format!("live fidelity certification requires {TENANT_ENV}"))?
            .parse::<Uuid>()
            .with_context(|| format!("{TENANT_ENV} must be a UUID"))?,
    );
    Ok((authorization, artifact, tenant_id))
}

#[tokio::test]
#[ignore = "billed and reads consented human data; requires MOA_RUN_LIVE_FIDELITY_TESTS=1, a positive MOA_FIDELITY_STUDY_BUDGET_USD, MOA_FIDELITY_SIMULATOR_API_KEY, MOA_FIDELITY_HUMAN_DATA_AUTHORIZATION, and a study artifact"]
async fn simulator_policy_certification_from_human_cohort_live() -> Result<()> {
    // Pins: a real study artifact decides certification through exactly the same
    // predeclared bounds the offline tests exercise, and the verdict is recorded
    // durably with its uncertainty. A study that misses a bound or lacks support
    // leaves the policy uncertified, and this test says so rather than passing.
    let (_authorization, artifact, tenant_id) = authorized_artifact()?;
    let now = Utc::now();

    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .map_err(|error| anyhow::anyhow!("bootstrap registry database: {error}"))?;
    let store = SimulatorPolicyStore::new(test_db.store().pool().clone());
    let policy = moa_experiments::simulator_policy::registry::SimulatorPolicy {
        policy_uid: artifact.policy_uid,
        revision: artifact.policy_revision,
        components: artifact.simulator_components.clone(),
    };
    ensure!(
        policy.policy_hash().map_err(anyhow::Error::from)? == artifact.policy_hash,
        "artifact policy hash does not match the components it pins"
    );
    store
        .register_policy(tenant_id, &policy)
        .await
        .map_err(anyhow::Error::from)?;
    let outcome = store
        .record_study(tenant_id, &artifact, now)
        .await
        .map_err(anyhow::Error::from)?;

    match &outcome {
        CertificationOutcome::Certified {
            window,
            uncertainty,
        } => {
            let record = store
                .load_policy(tenant_id, artifact.policy_uid, artifact.policy_revision)
                .await
                .map_err(anyhow::Error::from)?
                .context("certified policy must be loadable")?;
            let binding = record
                .execution_binding(now)
                .map_err(anyhow::Error::from)
                .context("a certified policy must publish an execution binding")?;
            ensure!(
                binding.certified_until <= window.certified_until,
                "published binding outlives its certification window"
            );
            ensure!(
                !uncertainty.class_bounds.is_empty(),
                "a certification must pin per-class uncertainty"
            );
        }
        CertificationOutcome::Failed { violations } => {
            bail!("study did not meet its predeclared bounds: {violations:?}")
        }
        CertificationOutcome::Inconclusive { gaps } => {
            bail!("study has insufficient independent support: {gaps:?}")
        }
    }
    Ok(())
}

#[test]
#[ignore = "billed and reads consented human data; requires MOA_RUN_LIVE_FIDELITY_TESTS=1, a positive MOA_FIDELITY_STUDY_BUDGET_USD, MOA_FIDELITY_SIMULATOR_API_KEY, MOA_FIDELITY_HUMAN_DATA_AUTHORIZATION, and a study artifact"]
fn live_fidelity_authorization_is_complete_live() -> Result<()> {
    // Pins: the authorization chain this lane depends on is present and coherent
    // before any billed work is attempted. Running this first localizes a
    // misconfigured lane to the configuration rather than to a provider failure.
    let (authorization, artifact, _tenant) = authorized_artifact()?;
    ensure!(
        authorization.budget_micro_usd > 0,
        "authorized budget must be positive"
    );
    ensure!(
        artifact.authorization.permits(artifact.observed_at),
        "the study's human-data authorization did not permit use when it ran"
    );
    Ok(())
}

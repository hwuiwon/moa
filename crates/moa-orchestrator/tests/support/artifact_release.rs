//! Production-shaped artifact-release environment fixtures.

use moa_artifacts::release::ActivationTargetClass;
use moa_core::types::identifiers::TenantId;
use moa_orchestrator::workflows::artifact_release_evaluation::repository::{
    ReleaseEvaluationRepository, ReleaseSubjectEnvironment,
};
use sqlx::PgPool;

/// Per-trial token ceiling sized for the production context pipeline exercised by the fixture.
#[allow(dead_code)]
pub const RELEASE_FIXTURE_MAX_TRIAL_TOKENS: u32 = 10_000;
/// Whole-run token ceiling for the fixture's 24 paired platform release trials.
#[allow(dead_code)]
pub const RELEASE_FIXTURE_MAX_TOTAL_TOKENS: u32 = 240_000;

/// Certifies the migrated platform simulator through the production evidence
/// path, then resolves the migrated platform plan and case cohort.
pub async fn seed_environment(
    pool: &PgPool,
    tenant_id: TenantId,
    target_class: ActivationTargetClass,
) -> anyhow::Result<ReleaseSubjectEnvironment> {
    crate::simulator_policy::certify_platform_release(pool, tenant_id).await?;
    ReleaseEvaluationRepository::new(pool.clone())
        .resolve_subject_environment(tenant_id, target_class)
        .await
        .map_err(Into::into)
}

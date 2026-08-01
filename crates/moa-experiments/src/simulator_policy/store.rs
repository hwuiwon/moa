//! Tenant-scoped durable simulator-policy registry.
//!
//! Policy components are immutable. Fidelity studies are append-only. The only
//! production read returns a full certified snapshot for an exact policy
//! reference; experiment admission persists that snapshot before dispatch.

use chrono::{DateTime, Utc};
use moa_artifacts::release::Digest32;
use moa_artifacts::simulation::SimulatorPolicyReference;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::simulator_policy::SimulatorPolicyError;
use crate::simulator_policy::fidelity::{CertificationOutcome, FidelityStudyArtifact};
use crate::simulator_policy::registry::{
    CertificationWindow, ResolvedSimulatorPolicy, SimulatorPolicy, SimulatorPolicyComponents,
    SimulatorPolicyRecord, SimulatorPolicyState,
};
use crate::simulator_policy::runtime::validate_runtime_contract;

/// Columns loaded for one registry row.
const POLICY_COLUMNS: &str = "policy_uid, revision, domain, policy_hash, components, state, \
     certification_study_uid, certification_artifact_hash, certified_policy_hash, \
     certified_from, certified_until, created_at, updated_at";

/// Postgres-backed simulator-policy registry.
pub struct SimulatorPolicyStore {
    pool: PgPool,
}

impl SimulatorPolicyStore {
    /// Creates a registry store backed by a Postgres pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Registers one immutable policy revision in the draft state.
    ///
    /// Re-registering identical bytes is idempotent; reusing a revision for a
    /// different body fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] for invalid policy components, hash
    /// drift, or a storage failure.
    pub async fn register_policy(
        &self,
        tenant_id: TenantId,
        policy: &SimulatorPolicy,
    ) -> Result<SimulatorPolicyRecord, SimulatorPolicyError> {
        policy.validate()?;
        validate_runtime_contract(&policy.components)?;
        let policy_hash = policy.policy_hash()?;
        let components = serde_json::to_value(&policy.components).map_err(not_canonicalizable)?;
        let mut conn = self.begin(tenant_id).await?;
        sqlx::query(
            r#"
            INSERT INTO moa.simulator_policy (
                policy_uid, revision, storage_partition_id, user_id, domain,
                policy_hash, components, state, valid_from, valid_until
            )
            VALUES ($1, $2, $3, NULL, $4, $5, $6, 'draft', $7, $8)
            ON CONFLICT (policy_uid, revision, storage_partition_id) DO NOTHING
            "#,
        )
        .bind(policy.policy_uid)
        .bind(policy.revision)
        .bind(partition(tenant_id))
        .bind(policy.components.domain.as_str())
        .bind(policy_hash.0.as_slice())
        .bind(&components)
        .bind(policy.components.validity.valid_from)
        .bind(policy.components.validity.valid_until)
        .execute(conn.as_mut())
        .await
        .map_err(storage)?;
        let row = load_policy_row(conn.as_mut(), tenant_id, policy.policy_uid, policy.revision)
            .await?
            .ok_or_else(|| SimulatorPolicyError::Storage {
                detail: "registered simulator policy did not persist".to_string(),
            })?;
        let record = policy_from_row(&row)?;
        if record.stored_policy_hash != policy_hash || record.policy != *policy {
            return Err(SimulatorPolicyError::PolicyHashDrift {
                policy_uid: policy.policy_uid,
                revision: policy.revision,
            });
        }
        conn.commit().await.map_err(storage)?;
        Ok(record)
    }

    /// Loads one exact policy revision.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] on storage or decode failure.
    pub async fn load_policy(
        &self,
        tenant_id: TenantId,
        policy_uid: Uuid,
        revision: i32,
    ) -> Result<Option<SimulatorPolicyRecord>, SimulatorPolicyError> {
        let mut conn = self.begin(tenant_id).await?;
        let row = load_policy_row(conn.as_mut(), tenant_id, policy_uid, revision).await?;
        conn.commit().await.map_err(storage)?;
        row.as_ref().map(policy_from_row).transpose()
    }

    /// Evaluates and records one immutable fidelity study.
    ///
    /// The store computes the verdict itself. A caller cannot submit a passing
    /// verdict that disagrees with the artifact's predeclared bounds. Replaying
    /// the same study bytes is idempotent; reusing a study id for different bytes
    /// is refused.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] for invalid evidence, identity drift, or
    /// a storage failure.
    pub async fn record_study(
        &self,
        tenant_id: TenantId,
        artifact: &FidelityStudyArtifact,
        evaluated_at: DateTime<Utc>,
    ) -> Result<CertificationOutcome, SimulatorPolicyError> {
        let outcome = artifact.certify(evaluated_at)?;
        let artifact_bytes = artifact.canonical_bytes()?;
        let artifact_json = String::from_utf8(artifact_bytes).map_err(|error| {
            SimulatorPolicyError::NotCanonicalizable {
                detail: error.to_string(),
            }
        })?;
        let artifact_hash = artifact.digest()?;
        let outcome_json = serde_json::to_value(&outcome).map_err(not_canonicalizable)?;
        let mut conn = self.begin(tenant_id).await?;
        let policy_row = sqlx::query(&format!(
            r#"
            SELECT {POLICY_COLUMNS}
            FROM moa.simulator_policy
            WHERE policy_uid = $1 AND revision = $2 AND storage_partition_id = $3
            FOR UPDATE
            "#
        ))
        .bind(artifact.policy_uid)
        .bind(artifact.policy_revision)
        .bind(partition(tenant_id))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(storage)?
        .ok_or(SimulatorPolicyError::NotCertified {
            policy_uid: artifact.policy_uid,
            revision: artifact.policy_revision,
            state: SimulatorPolicyState::Draft,
        })?;
        let policy_record = policy_from_row(&policy_row)?;
        if policy_record.stored_policy_hash != artifact.policy_hash
            || policy_record.policy.components.domain != artifact.domain
        {
            return Err(SimulatorPolicyError::CertifiedComponentChanged {
                policy_uid: artifact.policy_uid,
                revision: artifact.policy_revision,
                study_uid: artifact.study_uid,
            });
        }
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.simulator_fidelity_study (
                study_uid, storage_partition_id, user_id, policy_uid, policy_revision,
                policy_hash, domain, verdict, artifact_json, artifact_hash, outcome,
                selection_cohort_id, selection_cohort_hash, selection_cohort_units,
                certification_cohort_id, certification_cohort_hash, certification_cohort_units,
                budget_micro_usd, spent_micro_usd, human_data_authorization_id, observed_at
            )
            VALUES (
                $1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15, $16, $17, $18, $19, $20
            )
            ON CONFLICT (study_uid, storage_partition_id) DO NOTHING
            "#,
        )
        .bind(artifact.study_uid)
        .bind(partition(tenant_id))
        .bind(artifact.policy_uid)
        .bind(artifact.policy_revision)
        .bind(artifact.policy_hash.0.as_slice())
        .bind(artifact.domain.as_str())
        .bind(outcome.verdict())
        .bind(&artifact_json)
        .bind(artifact_hash.0.as_slice())
        .bind(&outcome_json)
        .bind(&artifact.selection_cohort.cohort_id)
        .bind(artifact.selection_cohort.content_hash.0.as_slice())
        .bind(i32::try_from(artifact.selection_cohort.independent_units).unwrap_or(i32::MAX))
        .bind(&artifact.certification_cohort.cohort_id)
        .bind(artifact.certification_cohort.content_hash.0.as_slice())
        .bind(i32::try_from(artifact.certification_cohort.independent_units).unwrap_or(i32::MAX))
        .bind(i64::try_from(artifact.cost.budget_micro_usd).unwrap_or(i64::MAX))
        .bind(i64::try_from(artifact.cost.spent_micro_usd).unwrap_or(i64::MAX))
        .bind(&artifact.authorization.authorization_id)
        .bind(artifact.observed_at)
        .execute(conn.as_mut())
        .await
        .map_err(storage)?
        .rows_affected();

        if inserted == 0 {
            let row = sqlx::query(
                r#"
                SELECT artifact_hash, outcome
                FROM moa.simulator_fidelity_study
                WHERE study_uid = $1 AND storage_partition_id = $2
                "#,
            )
            .bind(artifact.study_uid)
            .bind(partition(tenant_id))
            .fetch_one(conn.as_mut())
            .await
            .map_err(storage)?;
            if digest_from_row(&row, "artifact_hash")? != artifact_hash
                || row
                    .try_get::<serde_json::Value, _>("outcome")
                    .map_err(storage)?
                    != outcome_json
            {
                return Err(SimulatorPolicyError::Storage {
                    detail: format!(
                        "fidelity study {} was replayed with different evidence or verdict",
                        artifact.study_uid
                    ),
                });
            }
            conn.commit().await.map_err(storage)?;
            return Ok(outcome);
        }

        let updated = match &outcome {
            CertificationOutcome::Certified { window, .. } => sqlx::query(
                r#"
                UPDATE moa.simulator_policy
                SET state = 'certified',
                    certification_study_uid = $1,
                    certification_artifact_hash = $2,
                    certified_policy_hash = $3,
                    certified_from = $4,
                    certified_until = $5
                WHERE policy_uid = $6
                  AND revision = $7
                  AND storage_partition_id = $8
                  AND policy_hash = $3
                  AND state <> 'revoked'
                "#,
            )
            .bind(window.study_uid)
            .bind(window.study_artifact_hash.0.as_slice())
            .bind(window.certified_policy_hash.0.as_slice())
            .bind(window.certified_from)
            .bind(window.certified_until)
            .bind(artifact.policy_uid)
            .bind(artifact.policy_revision)
            .bind(partition(tenant_id))
            .execute(conn.as_mut())
            .await
            .map_err(storage)?
            .rows_affected(),
            CertificationOutcome::Failed { .. } => sqlx::query(
                r#"
                UPDATE moa.simulator_policy
                SET state = 'rejected'
                WHERE policy_uid = $1
                  AND revision = $2
                  AND storage_partition_id = $3
                  AND policy_hash = $4
                  AND state <> 'revoked'
                "#,
            )
            .bind(artifact.policy_uid)
            .bind(artifact.policy_revision)
            .bind(partition(tenant_id))
            .bind(artifact.policy_hash.0.as_slice())
            .execute(conn.as_mut())
            .await
            .map_err(storage)?
            .rows_affected(),
            CertificationOutcome::Inconclusive { .. } => 1,
        };
        if updated == 0 && matches!(outcome, CertificationOutcome::Certified { .. }) {
            return Err(SimulatorPolicyError::CertifiedComponentChanged {
                policy_uid: artifact.policy_uid,
                revision: artifact.policy_revision,
                study_uid: artifact.study_uid,
            });
        }
        conn.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Withdraws a policy revision regardless of prior certification.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::Storage`] on a storage fault.
    pub async fn revoke_policy(
        &self,
        tenant_id: TenantId,
        policy_uid: Uuid,
        revision: i32,
    ) -> Result<bool, SimulatorPolicyError> {
        let mut conn = self.begin(tenant_id).await?;
        let updated = sqlx::query(
            r#"
            UPDATE moa.simulator_policy
            SET state = 'revoked'
            WHERE policy_uid = $1 AND revision = $2 AND storage_partition_id = $3
            "#,
        )
        .bind(policy_uid)
        .bind(revision)
        .bind(partition(tenant_id))
        .execute(conn.as_mut())
        .await
        .map_err(storage)?
        .rows_affected();
        conn.commit().await.map_err(storage)?;
        Ok(updated > 0)
    }

    /// Resolves one exact certified policy for production experiment admission.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] when the revision is absent, uncertified,
    /// expired, hash-drifted, or incompatible with the served runtime contract.
    pub async fn resolve_policy(
        &self,
        tenant_id: TenantId,
        reference: SimulatorPolicyReference,
        now: DateTime<Utc>,
    ) -> Result<ResolvedSimulatorPolicy, SimulatorPolicyError> {
        let record = self
            .load_policy(tenant_id, reference.policy_uid, reference.revision)
            .await?
            .ok_or(SimulatorPolicyError::NotCertified {
                policy_uid: reference.policy_uid,
                revision: reference.revision,
                state: SimulatorPolicyState::Draft,
            })?;
        let binding = record.execution_binding(now)?;
        validate_runtime_contract(&record.policy.components)?;
        Ok(ResolvedSimulatorPolicy {
            binding,
            components: record.policy.components,
        })
    }

    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>, SimulatorPolicyError> {
        ScopedConn::begin(&self.pool, &RlsContext::tenant(tenant_id))
            .await
            .map_err(storage)
    }
}

async fn load_policy_row(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    policy_uid: Uuid,
    revision: i32,
) -> Result<Option<sqlx::postgres::PgRow>, SimulatorPolicyError> {
    sqlx::query(&format!(
        r#"
        SELECT {POLICY_COLUMNS}
        FROM moa.simulator_policy
        WHERE policy_uid = $1 AND revision = $2 AND storage_partition_id = $3
        "#
    ))
    .bind(policy_uid)
    .bind(revision)
    .bind(partition(tenant_id))
    .fetch_optional(conn)
    .await
    .map_err(storage)
}

fn partition(tenant_id: TenantId) -> String {
    StoragePartitionId::for_tenant(tenant_id).to_string()
}

fn storage<E: std::fmt::Display>(error: E) -> SimulatorPolicyError {
    SimulatorPolicyError::Storage {
        detail: error.to_string(),
    }
}

fn not_canonicalizable<E: std::fmt::Display>(error: E) -> SimulatorPolicyError {
    SimulatorPolicyError::NotCanonicalizable {
        detail: error.to_string(),
    }
}

fn digest_from_row(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<Digest32, SimulatorPolicyError> {
    let bytes: Vec<u8> = row.try_get(column).map_err(storage)?;
    let fixed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SimulatorPolicyError::UnreadableRow {
            detail: format!("column `{column}` is not a 32-byte digest"),
        })?;
    Ok(Digest32(fixed))
}

fn policy_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<SimulatorPolicyRecord, SimulatorPolicyError> {
    let components: SimulatorPolicyComponents =
        serde_json::from_value(row.try_get("components").map_err(storage)?).map_err(|error| {
            SimulatorPolicyError::UnreadableRow {
                detail: format!("simulator policy components do not decode: {error}"),
            }
        })?;
    let state_text: String = row.try_get("state").map_err(storage)?;
    let state = SimulatorPolicyState::from_db(&state_text).ok_or_else(|| {
        SimulatorPolicyError::UnreadableRow {
            detail: format!("unknown simulator policy state `{state_text}`"),
        }
    })?;
    let certification_study_uid: Option<Uuid> =
        row.try_get("certification_study_uid").map_err(storage)?;
    let certification = match certification_study_uid {
        None => None,
        Some(study_uid) => Some(CertificationWindow {
            study_uid,
            study_artifact_hash: digest_from_row(row, "certification_artifact_hash")?,
            certified_policy_hash: digest_from_row(row, "certified_policy_hash")?,
            certified_from: row.try_get("certified_from").map_err(storage)?,
            certified_until: row.try_get("certified_until").map_err(storage)?,
        }),
    };
    Ok(SimulatorPolicyRecord {
        policy: SimulatorPolicy {
            policy_uid: row.try_get("policy_uid").map_err(storage)?,
            revision: row.try_get("revision").map_err(storage)?,
            components,
        },
        stored_policy_hash: digest_from_row(row, "policy_hash")?,
        state,
        certification,
        created_at: row.try_get("created_at").map_err(storage)?,
        updated_at: row.try_get("updated_at").map_err(storage)?,
    })
}

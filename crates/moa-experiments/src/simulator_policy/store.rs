//! Durable simulator-policy registry and platform certification authority.
//!
//! Policy components are immutable. Fidelity studies are append-only. The only
//! production read returns a full certified snapshot for an exact policy
//! reference; experiment admission persists that snapshot before dispatch. The
//! global release policy additionally requires a migration-owned mandate and a
//! separate exact-artifact evidence import.

use chrono::{DateTime, Utc};
use moa_artifacts::release::{
    Digest32, PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
    PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION, PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
};
use moa_artifacts::simulation::SimulatorPolicyReference;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::simulator_policy::SimulatorPolicyError;
use crate::simulator_policy::fidelity::{
    CertificationOutcome, DomainFidelityBounds, FidelityStudyArtifact, HumanDataAuthorization,
    LabelProtocolPin,
};
use crate::simulator_policy::registry::{
    CertificationWindow, CohortPin, ResolvedSimulatorPolicy, ScenarioDomain, SimulatorPolicy,
    SimulatorPolicyComponents, SimulatorPolicyRecord, SimulatorPolicyState,
};
use crate::simulator_policy::runtime::validate_runtime_contract;

/// Columns loaded for one registry row.
const POLICY_COLUMNS: &str = "policy_uid, revision, domain, policy_hash, components, state, \
     certification_study_uid, certification_artifact_hash, certified_policy_hash, \
     certified_from, certified_until, created_at, updated_at";

struct PlatformCertificationMandate {
    mandate_uid: Uuid,
    policy_uid: Uuid,
    policy_revision: i32,
    policy_hash: Digest32,
    domain: ScenarioDomain,
    bounds: DomainFidelityBounds,
    selection_cohort: CohortPin,
    certification_cohort: CohortPin,
    label_protocol: LabelProtocolPin,
    authorization: HumanDataAuthorization,
    study_budget_micro_usd: u64,
    required_source_manifest_hash: Digest32,
    study_window_from: DateTime<Utc>,
    study_window_until: DateTime<Utc>,
    predeclared_at: DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct PlatformStudyAuthority {
    mandate_uid: Uuid,
    source_manifest_hash: Digest32,
}

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
        let storage_partition_id = partition(tenant_id);
        let mut conn = self.begin(tenant_id).await?;
        let outcome = record_study_in_conn(
            conn.as_mut(),
            Some(storage_partition_id.as_str()),
            None,
            artifact,
            evaluated_at,
        )
        .await?;
        conn.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Evaluates and records independently authorized evidence for a global policy.
    ///
    /// `mandate_uid` must already name the fixed immutable migration-owned
    /// predeclaration. A separate promoter import must approve the exact canonical
    /// study hash and source-manifest digest. This is an operator-only ingestion
    /// seam, not a tenant Behavior Lab endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] when the mandate is absent, the submitted
    /// artifact disagrees with it, the exact artifact has not been imported from
    /// the mandated evidence source, or persistence fails.
    pub async fn record_platform_study(
        &self,
        mandate_uid: Uuid,
        artifact: &FidelityStudyArtifact,
        evaluated_at: DateTime<Utc>,
    ) -> Result<CertificationOutcome, SimulatorPolicyError> {
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let mandate = load_platform_mandate(&mut transaction, mandate_uid)
            .await?
            .ok_or(SimulatorPolicyError::CertificationMandateMissing { mandate_uid })?;
        validate_platform_mandate(&mandate, artifact, evaluated_at)?;
        let authority = load_platform_study_authority(&mut transaction, &mandate, artifact).await?;
        let outcome = record_study_in_conn(
            &mut transaction,
            None,
            Some(authority),
            artifact,
            evaluated_at,
        )
        .await?;
        transaction.commit().await.map_err(storage)?;
        Ok(outcome)
    }

    /// Resolves one exact certified global policy for an operator workflow.
    ///
    /// This bypasses tenant resolution deliberately: only the global row can be
    /// returned, so a tenant policy with the same identity cannot influence an
    /// operator certification check.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] when the global revision is absent,
    /// uncertified, expired, hash-drifted, or incompatible with this runtime.
    pub async fn resolve_platform_policy(
        &self,
        reference: SimulatorPolicyReference,
        now: DateTime<Utc>,
    ) -> Result<ResolvedSimulatorPolicy, SimulatorPolicyError> {
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let row = load_exact_policy_row(
            &mut transaction,
            None,
            reference.policy_uid,
            reference.revision,
        )
        .await?
        .ok_or(SimulatorPolicyError::NotCertified {
            policy_uid: reference.policy_uid,
            revision: reference.revision,
            state: SimulatorPolicyState::Draft,
        })?;
        let record = policy_from_row(&row)?;
        let binding = record.execution_binding(now)?;
        validate_runtime_contract(&record.policy.components)?;
        transaction.commit().await.map_err(storage)?;
        Ok(ResolvedSimulatorPolicy {
            binding,
            components: record.policy.components,
        })
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

async fn record_study_in_conn(
    conn: &mut PgConnection,
    storage_partition_id: Option<&str>,
    platform_authority: Option<PlatformStudyAuthority>,
    artifact: &FidelityStudyArtifact,
    evaluated_at: DateTime<Utc>,
) -> Result<CertificationOutcome, SimulatorPolicyError> {
    let measured_policy = SimulatorPolicy {
        policy_uid: artifact.policy_uid,
        revision: artifact.policy_revision,
        components: artifact.simulator_components.clone(),
    };
    measured_policy.validate()?;
    if measured_policy.policy_hash()? != artifact.policy_hash {
        return Err(SimulatorPolicyError::CertifiedComponentChanged {
            policy_uid: artifact.policy_uid,
            revision: artifact.policy_revision,
            study_uid: artifact.study_uid,
        });
    }
    if artifact.selection_cohort != artifact.simulator_components.calibration_cohort {
        return Err(SimulatorPolicyError::InvalidMeasurement {
            detail: "study selection cohort does not match the simulator calibration cohort"
                .to_string(),
        });
    }
    let outcome = artifact.certify(evaluated_at)?;
    let artifact_bytes = artifact.canonical_bytes()?;
    let artifact_json = String::from_utf8(artifact_bytes).map_err(|error| {
        SimulatorPolicyError::NotCanonicalizable {
            detail: error.to_string(),
        }
    })?;
    let artifact_hash = artifact.digest()?;
    let outcome_json = serde_json::to_value(&outcome).map_err(not_canonicalizable)?;
    let policy_row = sqlx::query(&format!(
        r#"
        SELECT {POLICY_COLUMNS}
        FROM moa.simulator_policy
        WHERE policy_uid = $1
          AND revision = $2
          AND storage_partition_id IS NOT DISTINCT FROM $3
        FOR UPDATE
        "#
    ))
    .bind(artifact.policy_uid)
    .bind(artifact.policy_revision)
    .bind(storage_partition_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or(SimulatorPolicyError::NotCertified {
        policy_uid: artifact.policy_uid,
        revision: artifact.policy_revision,
        state: SimulatorPolicyState::Draft,
    })?;
    let policy_record = policy_from_row(&policy_row)?;
    if policy_record.stored_policy_hash != artifact.policy_hash
        || policy_record.policy.components != artifact.simulator_components
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
            budget_micro_usd, spent_micro_usd, human_data_authorization_id,
            platform_mandate_uid, evidence_source_manifest_hash, observed_at
        )
        VALUES (
            $1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10,
            $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22
        )
        ON CONFLICT (study_uid, storage_partition_id) DO NOTHING
        "#,
    )
    .bind(artifact.study_uid)
    .bind(storage_partition_id)
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
    .bind(platform_authority.map(|authority| authority.mandate_uid))
    .bind(platform_authority.map(|authority| authority.source_manifest_hash.to_vec()))
    .bind(artifact.observed_at)
    .execute(&mut *conn)
    .await
    .map_err(storage)?
    .rows_affected();

    if inserted == 0 {
        let row = sqlx::query(
            r#"
            SELECT artifact_hash, outcome, platform_mandate_uid,
                   evidence_source_manifest_hash
            FROM moa.simulator_fidelity_study
            WHERE study_uid = $1
              AND storage_partition_id IS NOT DISTINCT FROM $2
            "#,
        )
        .bind(artifact.study_uid)
        .bind(storage_partition_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(storage)?;
        if digest_from_row(&row, "artifact_hash")? != artifact_hash
            || row
                .try_get::<serde_json::Value, _>("outcome")
                .map_err(storage)?
                != outcome_json
            || row
                .try_get::<Option<Uuid>, _>("platform_mandate_uid")
                .map_err(storage)?
                != platform_authority.map(|authority| authority.mandate_uid)
            || optional_digest_from_row(&row, "evidence_source_manifest_hash")?
                != platform_authority.map(|authority| authority.source_manifest_hash)
        {
            return Err(SimulatorPolicyError::Storage {
                detail: format!(
                    "fidelity study {} was replayed with different evidence or verdict",
                    artifact.study_uid
                ),
            });
        }
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
              AND storage_partition_id IS NOT DISTINCT FROM $8
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
        .bind(storage_partition_id)
        .execute(&mut *conn)
        .await
        .map_err(storage)?
        .rows_affected(),
        CertificationOutcome::Failed { .. } => sqlx::query(
            r#"
            UPDATE moa.simulator_policy
            SET state = 'rejected'
            WHERE policy_uid = $1
              AND revision = $2
              AND storage_partition_id IS NOT DISTINCT FROM $3
              AND policy_hash = $4
              AND state <> 'revoked'
            "#,
        )
        .bind(artifact.policy_uid)
        .bind(artifact.policy_revision)
        .bind(storage_partition_id)
        .bind(artifact.policy_hash.0.as_slice())
        .execute(&mut *conn)
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
    Ok(outcome)
}

async fn load_platform_mandate(
    conn: &mut PgConnection,
    mandate_uid: Uuid,
) -> Result<Option<PlatformCertificationMandate>, SimulatorPolicyError> {
    let row = sqlx::query(
        r#"
        SELECT mandate_uid, policy_uid, policy_revision, policy_hash, domain,
               bounds, selection_cohort, certification_cohort, label_protocol,
               human_data_authorization, study_budget_micro_usd,
               required_source_manifest_hash, study_window_from,
               study_window_until, predeclared_at
        FROM moa.simulator_certification_mandate
        WHERE mandate_uid = $1 AND storage_partition_id IS NULL
        "#,
    )
    .bind(mandate_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?;
    row.as_ref().map(platform_mandate_from_row).transpose()
}

fn platform_mandate_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<PlatformCertificationMandate, SimulatorPolicyError> {
    let mandate_uid = row.try_get("mandate_uid").map_err(storage)?;
    let domain_text: String = row.try_get("domain").map_err(storage)?;
    let domain = ScenarioDomain::new(&domain_text)?;
    let bounds = decode_json_column(row, "bounds")?;
    let selection_cohort: CohortPin = decode_json_column(row, "selection_cohort")?;
    let certification_cohort: CohortPin = decode_json_column(row, "certification_cohort")?;
    let label_protocol = decode_json_column(row, "label_protocol")?;
    let authorization = decode_json_column(row, "human_data_authorization")?;
    let budget: i64 = row.try_get("study_budget_micro_usd").map_err(storage)?;
    let study_budget_micro_usd =
        u64::try_from(budget).map_err(|_| SimulatorPolicyError::UnreadableRow {
            detail: format!(
                "platform certification mandate {mandate_uid} has a negative study budget"
            ),
        })?;
    let mandate = PlatformCertificationMandate {
        mandate_uid,
        policy_uid: row.try_get("policy_uid").map_err(storage)?,
        policy_revision: row.try_get("policy_revision").map_err(storage)?,
        policy_hash: digest_from_row(row, "policy_hash")?,
        domain,
        bounds,
        selection_cohort,
        certification_cohort,
        label_protocol,
        authorization,
        study_budget_micro_usd,
        required_source_manifest_hash: digest_from_row(row, "required_source_manifest_hash")?,
        study_window_from: row.try_get("study_window_from").map_err(storage)?,
        study_window_until: row.try_get("study_window_until").map_err(storage)?,
        predeclared_at: row.try_get("predeclared_at").map_err(storage)?,
    };
    validate_stored_platform_mandate(&mandate)?;
    Ok(mandate)
}

fn validate_stored_platform_mandate(
    mandate: &PlatformCertificationMandate,
) -> Result<(), SimulatorPolicyError> {
    mandate.bounds.validate()?;
    mandate.selection_cohort.validate()?;
    mandate.certification_cohort.validate()?;
    if mandate.policy_uid != PLATFORM_RELEASE_SIMULATOR_POLICY_UID
        || mandate.mandate_uid != PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID
        || mandate.policy_revision != PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION
        || mandate.domain.as_str() != "artifact-release"
    {
        return Err(SimulatorPolicyError::CertificationMandateMismatch {
            mandate_uid: mandate.mandate_uid,
            detail: "mandate does not govern the fixed platform release simulator identity"
                .to_string(),
        });
    }
    if mandate.bounds.domain != mandate.domain {
        return Err(SimulatorPolicyError::CertificationMandateMismatch {
            mandate_uid: mandate.mandate_uid,
            detail: "bounds domain does not match mandate domain".to_string(),
        });
    }
    if mandate.selection_cohort.cohort_id == mandate.certification_cohort.cohort_id
        || mandate.selection_cohort.content_hash == mandate.certification_cohort.content_hash
    {
        return Err(SimulatorPolicyError::CertificationMandateMismatch {
            mandate_uid: mandate.mandate_uid,
            detail: "selection and certification cohorts are not independent".to_string(),
        });
    }
    if mandate.study_budget_micro_usd == 0
        || mandate.study_window_until <= mandate.study_window_from
    {
        return Err(SimulatorPolicyError::CertificationMandateMismatch {
            mandate_uid: mandate.mandate_uid,
            detail: "mandate budget and study window must be positive".to_string(),
        });
    }
    if mandate.required_source_manifest_hash == Digest32([0; 32]) {
        return Err(SimulatorPolicyError::CertificationMandateMismatch {
            mandate_uid: mandate.mandate_uid,
            detail: "external source-manifest authority is not provisioned".to_string(),
        });
    }
    Ok(())
}

fn validate_platform_mandate(
    mandate: &PlatformCertificationMandate,
    artifact: &FidelityStudyArtifact,
    evaluated_at: DateTime<Utc>,
) -> Result<(), SimulatorPolicyError> {
    let mismatch = |detail: &str| SimulatorPolicyError::CertificationMandateMismatch {
        mandate_uid: mandate.mandate_uid,
        detail: detail.to_string(),
    };
    if artifact.policy_uid != mandate.policy_uid
        || artifact.policy_revision != mandate.policy_revision
        || artifact.policy_hash != mandate.policy_hash
        || artifact.domain != mandate.domain
    {
        return Err(mismatch("policy identity, hash, or domain differs"));
    }
    if artifact.bounds != mandate.bounds {
        return Err(mismatch("predeclared fidelity bounds differ"));
    }
    if artifact.selection_cohort != mandate.selection_cohort
        || artifact.certification_cohort != mandate.certification_cohort
    {
        return Err(mismatch(
            "predeclared cohort identity, hash, or units differ",
        ));
    }
    if artifact.label_protocol != mandate.label_protocol {
        return Err(mismatch("predeclared label protocol differs"));
    }
    if artifact.authorization != mandate.authorization {
        return Err(mismatch("independent human-data authorization differs"));
    }
    if artifact.cost.budget_micro_usd != mandate.study_budget_micro_usd {
        return Err(mismatch("authorized study budget differs"));
    }
    if artifact.observed_at < mandate.predeclared_at
        || artifact.observed_at < mandate.study_window_from
        || artifact.observed_at >= mandate.study_window_until
    {
        return Err(mismatch(
            "study observation is outside the predeclared window",
        ));
    }
    if evaluated_at < artifact.observed_at {
        return Err(mismatch(
            "certification decision predates the study observation",
        ));
    }
    Ok(())
}

async fn load_platform_study_authority(
    conn: &mut PgConnection,
    mandate: &PlatformCertificationMandate,
    artifact: &FidelityStudyArtifact,
) -> Result<PlatformStudyAuthority, SimulatorPolicyError> {
    let row = sqlx::query(
        r#"
        SELECT study_uid, study_artifact_hash, source_manifest_hash, imported_at
        FROM moa.simulator_certification_evidence_import
        WHERE mandate_uid = $1 AND storage_partition_id IS NULL
        "#,
    )
    .bind(mandate.mandate_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(storage)?
    .ok_or(SimulatorPolicyError::CertificationEvidenceMissing {
        mandate_uid: mandate.mandate_uid,
        study_uid: artifact.study_uid,
    })?;
    let imported_study_uid: Uuid = row.try_get("study_uid").map_err(storage)?;
    let imported_artifact_hash = digest_from_row(&row, "study_artifact_hash")?;
    let imported_source_manifest_hash = digest_from_row(&row, "source_manifest_hash")?;
    let imported_at: DateTime<Utc> = row.try_get("imported_at").map_err(storage)?;
    let submitted_artifact_hash = artifact.digest()?;
    if imported_study_uid != artifact.study_uid
        || imported_artifact_hash != submitted_artifact_hash
        || imported_source_manifest_hash != mandate.required_source_manifest_hash
        || imported_at < artifact.observed_at
    {
        return Err(SimulatorPolicyError::CertificationEvidenceMismatch {
            mandate_uid: mandate.mandate_uid,
            study_uid: artifact.study_uid,
            detail: "study identity, canonical artifact hash, source manifest digest, or import time differs"
                .to_string(),
        });
    }
    Ok(PlatformStudyAuthority {
        mandate_uid: mandate.mandate_uid,
        source_manifest_hash: imported_source_manifest_hash,
    })
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
        WHERE policy_uid = $1
          AND revision = $2
          AND (storage_partition_id IS NULL OR storage_partition_id = $3)
        -- Global identities are platform-owned and cannot be shadowed by a
        -- tenant registering the same UUID/revision.
        ORDER BY storage_partition_id IS NULL DESC
        LIMIT 1
        "#
    ))
    .bind(policy_uid)
    .bind(revision)
    .bind(partition(tenant_id))
    .fetch_optional(conn)
    .await
    .map_err(storage)
}

async fn load_exact_policy_row(
    conn: &mut sqlx::PgConnection,
    storage_partition_id: Option<&str>,
    policy_uid: Uuid,
    revision: i32,
) -> Result<Option<sqlx::postgres::PgRow>, SimulatorPolicyError> {
    sqlx::query(&format!(
        r#"
        SELECT {POLICY_COLUMNS}
        FROM moa.simulator_policy
        WHERE policy_uid = $1
          AND revision = $2
          AND storage_partition_id IS NOT DISTINCT FROM $3
        "#
    ))
    .bind(policy_uid)
    .bind(revision)
    .bind(storage_partition_id)
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

fn optional_digest_from_row(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<Option<Digest32>, SimulatorPolicyError> {
    let Some(bytes) = row.try_get::<Option<Vec<u8>>, _>(column).map_err(storage)? else {
        return Ok(None);
    };
    let fixed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| SimulatorPolicyError::UnreadableRow {
            detail: format!("column `{column}` is not a 32-byte digest"),
        })?;
    Ok(Some(Digest32(fixed)))
}

fn decode_json_column<T>(
    row: &sqlx::postgres::PgRow,
    column: &str,
) -> Result<T, SimulatorPolicyError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(row.try_get(column).map_err(storage)?).map_err(|error| {
        SimulatorPolicyError::UnreadableRow {
            detail: format!("column `{column}` does not decode: {error}"),
        }
    })
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

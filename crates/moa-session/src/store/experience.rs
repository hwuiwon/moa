//! Experience-learning operations for the Postgres session store.

use moa_core::types::experience::LearningCandidateType;
use sqlx::PgConnection;

use super::*;

impl PostgresSessionStore {
    /// Appends or idempotently refreshes one experience record.
    ///
    /// On a re-assessment of the same `(segment_id, extraction_policy_version)`
    /// the upsert overwrites `task_summary`; when the summary actually changes it
    /// also clears `task_embedding` and its provenance so the backfill re-embeds
    /// the new text. Without that, a summary edit would strand a vector describing
    /// the old text on a non-NULL (never re-selected) row.
    pub async fn append_experience_record(&self, experience: &ExperienceRecord) -> Result<()> {
        let experience_records = self.table_name("experience_records");
        sqlx::query(&format!(
            "INSERT INTO {experience_records} \
             (id, segment_id, session_id, storage_partition_id, user_id, tenant_id, task_summary, \
              task_fingerprint, task_fingerprint_payload, task_facets, actions, resources, \
              outcome, confidence, evidence, tools_used, skills_activated, skills_used, turn_count, token_cost, \
              duration_ms, assessment_policy_version, extraction_policy_version, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
                     $18, $19, $20, $21, $22, $23, $24) \
             ON CONFLICT (segment_id, extraction_policy_version) DO UPDATE SET \
                 task_summary = EXCLUDED.task_summary, \
                 task_embedding = CASE \
                     WHEN {experience_records}.task_summary IS DISTINCT FROM EXCLUDED.task_summary \
                     THEN NULL ELSE {experience_records}.task_embedding END, \
                 task_embedding_model = CASE \
                     WHEN {experience_records}.task_summary IS DISTINCT FROM EXCLUDED.task_summary \
                     THEN NULL ELSE {experience_records}.task_embedding_model END, \
                 task_embedding_model_version = CASE \
                     WHEN {experience_records}.task_summary IS DISTINCT FROM EXCLUDED.task_summary \
                     THEN NULL ELSE {experience_records}.task_embedding_model_version END, \
                 task_fingerprint = EXCLUDED.task_fingerprint, \
                 task_fingerprint_payload = EXCLUDED.task_fingerprint_payload, \
                 task_facets = EXCLUDED.task_facets, \
                 actions = EXCLUDED.actions, \
                 resources = EXCLUDED.resources, \
                 outcome = EXCLUDED.outcome, \
                 confidence = EXCLUDED.confidence, \
                 evidence = EXCLUDED.evidence, \
                 tools_used = EXCLUDED.tools_used, \
                 skills_activated = EXCLUDED.skills_activated, \
                 skills_used = EXCLUDED.skills_used, \
                 turn_count = EXCLUDED.turn_count, \
                 token_cost = EXCLUDED.token_cost, \
                 duration_ms = EXCLUDED.duration_ms, \
                 assessment_policy_version = EXCLUDED.assessment_policy_version"
        ))
        .bind(experience.id)
        .bind(experience.segment_id.0)
        .bind(experience.session_id.0)
        .bind(storage_partition_id(experience.tenant_id))
        .bind(experience.user_id.to_string())
        .bind(experience.tenant_id.to_string())
        .bind(experience.task_summary.as_deref())
        .bind(&experience.task_fingerprint.hash)
        .bind(Json(experience.task_fingerprint.clone()))
        .bind(Json(experience.task_facets.clone()))
        .bind(&experience.actions)
        .bind(Json(experience.resources.clone()))
        .bind(experience.outcome.as_str())
        .bind(experience.confidence)
        .bind(Json(experience.evidence.clone()))
        .bind(&experience.tools_used)
        .bind(&experience.skills_activated)
        .bind(&experience.skills_used)
        .bind(experience.turn_count as i32)
        .bind(experience.token_cost as i64)
        .bind(experience.duration_ms.map(|value| value as i64))
        .bind(&experience.assessment_policy_version)
        .bind(&experience.extraction_policy_version)
        .bind(experience.created_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Lists experience records for a session in creation order.
    pub async fn list_experience_records(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Vec<ExperienceRecord>> {
        let experience_records = self.table_name("experience_records");
        let rows = sqlx::query(&format!(
            "SELECT {EXPERIENCE_RECORD_COLUMNS} FROM {experience_records} \
             WHERE session_id = $1 \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(session_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(experience_record_from_row).collect()
    }

    /// Loads one experience record by session and experience ID.
    pub async fn get_experience_record(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        experience_id: Uuid,
    ) -> Result<Option<ExperienceRecord>> {
        let experience_records = self.table_name("experience_records");
        let row = sqlx::query(&format!(
            "SELECT {EXPERIENCE_RECORD_COLUMNS} FROM {experience_records} \
             WHERE session_id = $1 AND id = $2"
        ))
        .bind(session_id.0)
        .bind(experience_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        row.as_ref().map(experience_record_from_row).transpose()
    }

    /// Appends or idempotently refreshes attribution records.
    pub async fn append_experience_attributions(
        &self,
        attributions: &[ExperienceAttribution],
    ) -> Result<()> {
        if attributions.is_empty() {
            return Ok(());
        }
        let experience_attributions = self.table_name("experience_attributions");
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        for attribution in attributions {
            sqlx::query(&format!(
                "INSERT INTO {experience_attributions} \
                 (id, experience_id, tenant_id, storage_partition_id, user_id, subject_type, subject_id, \
                  effect, kind, confidence, evidence, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 ON CONFLICT (experience_id, subject_type, subject_id) DO UPDATE SET \
                     effect = EXCLUDED.effect, \
                     kind = EXCLUDED.kind, \
                     confidence = EXCLUDED.confidence, \
                     evidence = EXCLUDED.evidence"
            ))
            .bind(attribution.id)
            .bind(attribution.experience_id)
            .bind(attribution.tenant_id.to_string())
            .bind(storage_partition_id(attribution.tenant_id))
            .bind(attribution.user_id.as_ref().map(ToString::to_string))
            .bind(attribution.subject_type.as_str())
            .bind(&attribution.subject_id)
            .bind(attribution.effect.as_str())
            .bind(attribution.kind.as_str())
            .bind(attribution.confidence)
            .bind(Json(attribution.evidence.clone()))
            .bind(attribution.created_at)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }
        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Lists attributions for one experience.
    pub async fn list_experience_attributions(
        &self,
        experience_id: uuid::Uuid,
    ) -> Result<Vec<ExperienceAttribution>> {
        let experience_attributions = self.table_name("experience_attributions");
        let rows = sqlx::query(&format!(
            "SELECT {EXPERIENCE_ATTRIBUTION_COLUMNS} FROM {experience_attributions} \
             WHERE experience_id = $1 \
             ORDER BY subject_type ASC, subject_id ASC"
        ))
        .bind(experience_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(experience_attribution_from_row).collect()
    }

    /// Appends or idempotently refreshes one learning candidate and its sources.
    pub async fn append_learning_candidate(&self, candidate: &LearningCandidate) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        self.append_learning_candidate_with_conn(tx.as_mut(), candidate)
            .await?;
        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Appends or idempotently refreshes one learning candidate using an existing connection.
    ///
    /// The candidate row and every normalized source it carries are written
    /// together. That is not a convenience: a deferred database constraint
    /// refuses to let the transaction commit if the candidate ends it with no
    /// sources, so splitting these into two calls would produce a candidate that
    /// cannot be committed at all rather than one that is silently
    /// unattributable.
    pub async fn append_learning_candidate_with_conn(
        &self,
        conn: &mut PgConnection,
        candidate: &LearningCandidate,
    ) -> Result<()> {
        let learning_candidates = self.table_name("learning_candidates");
        sqlx::query(&format!(
            "INSERT INTO {learning_candidates} \
             (id, tenant_id, storage_partition_id, user_id, candidate_type, proposal_kind, status, \
              target_id, target_label, \
              task_fingerprint, task_fingerprint_payload, task_facets, payload, evaluation_payload, \
              confidence, risk_class, promotion_requirements, status_reason, \
              batch_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
                     $18, $19, $20, $21) \
             ON CONFLICT (id) DO UPDATE SET \
                 target_id = EXCLUDED.target_id, \
                 target_label = EXCLUDED.target_label, \
                 payload = EXCLUDED.payload, \
                 evaluation_payload = COALESCE(EXCLUDED.evaluation_payload, {learning_candidates}.evaluation_payload), \
                 confidence = EXCLUDED.confidence, \
                 risk_class = EXCLUDED.risk_class, \
                 promotion_requirements = EXCLUDED.promotion_requirements, \
                 status_reason = COALESCE(EXCLUDED.status_reason, {learning_candidates}.status_reason), \
                 updated_at = EXCLUDED.updated_at"
        ))
        .bind(candidate.id)
        .bind(candidate.tenant_id.to_string())
        .bind(storage_partition_id(candidate.tenant_id))
        .bind(candidate.user_id.as_ref().map(ToString::to_string))
        .bind(candidate.candidate_type.as_str())
        .bind(candidate.proposal_kind.as_str())
        .bind(candidate.status.as_str())
        .bind(candidate.target_id.as_deref())
        .bind(candidate.target_label.as_deref())
        .bind(candidate.task_fingerprint.as_ref().map(|value| value.hash.as_str()))
        .bind(candidate.task_fingerprint.clone().map(Json))
        .bind(candidate.task_facets.clone().map(Json))
        .bind(Json(candidate.payload.clone()))
        .bind(candidate.evaluation_payload.clone().map(Json))
        .bind(candidate.confidence)
        .bind(candidate.risk_class.as_str())
        .bind(&candidate.promotion_requirements)
        .bind(candidate.status_reason.as_deref())
        .bind(candidate.batch_id)
        .bind(candidate.created_at)
        .bind(candidate.updated_at)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        self.append_learning_candidate_sources_in_tx(conn, candidate.id, &candidate.sources)
            .await
    }

    /// Lists learning candidates for a tenant and optional status.
    pub async fn list_learning_candidates(
        &self,
        tenant_id: &str,
        status: Option<LearningCandidateStatus>,
        limit: usize,
    ) -> Result<Vec<LearningCandidate>> {
        let learning_candidates = self.table_name("learning_candidates");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {LEARNING_CANDIDATE_COLUMNS} FROM {learning_candidates} \
             WHERE tenant_id = "
        ));
        query.push_bind(tenant_id);
        if let Some(status) = status {
            query.push(" AND status = ");
            query.push_bind(status.as_str());
        }
        query.push(" ORDER BY updated_at DESC, id ASC LIMIT ");
        query.push_bind(limit as i64);
        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut candidates = rows
            .iter()
            .map(learning_candidate_from_row)
            .collect::<Result<Vec<_>>>()?;
        self.hydrate_learning_candidate_sources(&mut candidates)
            .await?;
        Ok(candidates)
    }

    /// Finds one open proposed candidate of a type targeting a label, using an existing connection.
    ///
    /// Proposal writers use this to dedupe against an already-open review item
    /// (for example one `Proposed` skill draft per skill name per tenant)
    /// before filing a new candidate.
    pub async fn find_proposed_learning_candidate_by_target_with_conn(
        &self,
        conn: &mut PgConnection,
        tenant_id: &TenantId,
        candidate_type: LearningCandidateType,
        target_label: &str,
    ) -> Result<Option<LearningCandidate>> {
        let learning_candidates = self.table_name("learning_candidates");
        let row = sqlx::query(&format!(
            "SELECT {LEARNING_CANDIDATE_COLUMNS} FROM {learning_candidates} \
             WHERE tenant_id = $1 AND candidate_type = $2 AND status = $3 AND target_label = $4 \
             ORDER BY created_at ASC, id ASC LIMIT 1"
        ))
        .bind(tenant_id.to_string())
        .bind(candidate_type.as_str())
        .bind(LearningCandidateStatus::Proposed.as_str())
        .bind(target_label)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let mut candidate = row
            .as_ref()
            .map(learning_candidate_from_row)
            .transpose()?
            .map(|candidate| vec![candidate])
            .unwrap_or_default();
        self.hydrate_learning_candidate_sources(&mut candidate)
            .await?;
        Ok(candidate.into_iter().next())
    }

    /// Finds one open proposed candidate of a type with a task fingerprint, using an existing connection.
    ///
    /// Complements [`Self::find_proposed_learning_candidate_by_target_with_conn`]
    /// for the case where a generator names the same recurring work differently:
    /// the task fingerprint is stable across similar tasks, so an open proposal
    /// for the same fingerprint dedupes even when the target label differs.
    pub async fn find_proposed_learning_candidate_by_fingerprint_with_conn(
        &self,
        conn: &mut PgConnection,
        tenant_id: &TenantId,
        candidate_type: LearningCandidateType,
        fingerprint_hash: &str,
    ) -> Result<Option<LearningCandidate>> {
        let learning_candidates = self.table_name("learning_candidates");
        let row = sqlx::query(&format!(
            "SELECT {LEARNING_CANDIDATE_COLUMNS} FROM {learning_candidates} \
             WHERE tenant_id = $1 AND candidate_type = $2 AND status = $3 \
               AND task_fingerprint = $4 \
             ORDER BY created_at ASC, id ASC LIMIT 1"
        ))
        .bind(tenant_id.to_string())
        .bind(candidate_type.as_str())
        .bind(LearningCandidateStatus::Proposed.as_str())
        .bind(fingerprint_hash)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let mut candidate = row
            .as_ref()
            .map(learning_candidate_from_row)
            .transpose()?
            .map(|candidate| vec![candidate])
            .unwrap_or_default();
        self.hydrate_learning_candidate_sources(&mut candidate)
            .await?;
        Ok(candidate.into_iter().next())
    }

    /// Loads one full learning candidate by tenant and candidate ID.
    pub async fn get_learning_candidate(
        &self,
        tenant_id: &TenantId,
        candidate_id: Uuid,
    ) -> Result<Option<LearningCandidate>> {
        let mut conn = self.pool.acquire().await.map_err(map_sqlx_error)?;
        self.get_learning_candidate_with_conn(&mut conn, tenant_id, candidate_id)
            .await
    }

    /// Loads one full learning candidate by tenant and candidate ID using an existing connection.
    pub async fn get_learning_candidate_with_conn(
        &self,
        conn: &mut PgConnection,
        tenant_id: &TenantId,
        candidate_id: Uuid,
    ) -> Result<Option<LearningCandidate>> {
        let learning_candidates = self.table_name("learning_candidates");
        let row = sqlx::query(&format!(
            "SELECT {LEARNING_CANDIDATE_COLUMNS} FROM {learning_candidates} \
             WHERE tenant_id = $1 AND id = $2"
        ))
        .bind(tenant_id.to_string())
        .bind(candidate_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let mut candidate = row
            .as_ref()
            .map(learning_candidate_from_row)
            .transpose()?
            .map(|candidate| vec![candidate])
            .unwrap_or_default();
        self.hydrate_learning_candidate_sources(&mut candidate)
            .await?;
        Ok(candidate.into_iter().next())
    }

    /// Applies a status transition only when the candidate is still in the expected status.
    ///
    /// This is the only status writer. The unconditional variant that used to
    /// sit beside it took no expected status at all, so any caller could move
    /// any candidate to any status, and two concurrent reviewers could both be
    /// told they succeeded at contradictory transitions.
    pub async fn update_learning_candidate_status_from(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> Result<bool> {
        self.update_learning_candidate_status_with_expected(update, expected_status)
            .await
            .map(|affected| affected > 0)
    }

    /// Applies a status transition in the caller's open transaction.
    pub async fn update_learning_candidate_status_from_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> Result<bool> {
        let learning_candidates = self.table_name("learning_candidates");
        let affected = sqlx::query(&format!(
            "UPDATE {learning_candidates} SET \
                 status = $1, \
                 status_reason = $2, \
                 evaluation_payload = COALESCE($3, evaluation_payload), \
                 updated_at = $4 \
             WHERE id = $5 AND status = $6"
        ))
        .bind(update.status.as_str())
        .bind(update.status_reason.as_deref())
        .bind(update.evaluation_payload.clone().map(Json))
        .bind(update.updated_at)
        .bind(update.candidate_id)
        .bind(expected_status.as_str())
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Finalizes a claimed learning candidate after validating its normalized compile audit.
    pub async fn finalize_learning_candidate_status_from(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
        expected_compile_operation_key: Option<&str>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        let changed = self
            .finalize_learning_candidate_status_from_in_tx(
                &mut tx,
                update,
                expected_status,
                expected_compile_operation_key,
            )
            .await?;
        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(changed)
    }

    /// Finalizes a claimed candidate in an open transaction with compile-audit CAS validation.
    pub async fn finalize_learning_candidate_status_from_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
        expected_compile_operation_key: Option<&str>,
    ) -> Result<bool> {
        if !matches!(
            update.status,
            LearningCandidateStatus::Promoted | LearningCandidateStatus::Rejected
        ) {
            return Err(MoaError::ValidationError(
                "learning candidate finalization requires a terminal review status".to_string(),
            ));
        }
        let learning_candidates = self.table_name("learning_candidates");
        let row = sqlx::query(&format!(
            "SELECT tenant_id, status, evaluation_payload FROM {learning_candidates} \
             WHERE id = $1 FOR UPDATE"
        ))
        .bind(update.candidate_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            return Ok(false);
        };
        let status: String = row.try_get("status").map_err(map_sqlx_error)?;
        if status != expected_status.as_str() {
            return Ok(false);
        }
        let tenant_id: String = row.try_get("tenant_id").map_err(map_sqlx_error)?;
        let current = row
            .try_get::<Option<serde_json::Value>, _>("evaluation_payload")
            .map_err(map_sqlx_error)?
            .unwrap_or_else(|| serde_json::json!({}));
        validate_expected_learning_compile_audit(conn, &tenant_id, expected_compile_operation_key)
            .await?;
        let merged = match &update.evaluation_payload {
            Some(terminal) => merge_learning_evaluation_payload(current, terminal.clone())?,
            None => current,
        };
        let affected = sqlx::query(&format!(
            "UPDATE {learning_candidates} SET status = $1, status_reason = $2, \
                 evaluation_payload = $3, updated_at = $4 \
             WHERE id = $5 AND status = $6"
        ))
        .bind(update.status.as_str())
        .bind(update.status_reason.as_deref())
        .bind(Json(merged))
        .bind(update.updated_at)
        .bind(update.candidate_id)
        .bind(expected_status.as_str())
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected == 1)
    }

    async fn update_learning_candidate_status_with_expected(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> Result<u64> {
        let learning_candidates = self.table_name("learning_candidates");
        let affected = sqlx::query(&format!(
            "UPDATE {learning_candidates} SET \
                 status = $1, \
                 status_reason = $2, \
                 evaluation_payload = COALESCE($3, evaluation_payload), \
                 updated_at = $4 \
             WHERE id = $5 AND status = $6"
        ))
        .bind(update.status.as_str())
        .bind(update.status_reason.as_deref())
        .bind(update.evaluation_payload.clone().map(Json))
        .bind(update.updated_at)
        .bind(update.candidate_id)
        .bind(expected_status.as_str())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected)
    }

    /// Lists task-conditioned strategy success aggregates for one fingerprint.
    pub async fn list_task_strategy_success_rates(
        &self,
        tenant_id: &str,
        task_fingerprint: &str,
    ) -> Result<Vec<TaskStrategySuccessRate>> {
        let task_strategy_success_rates = self.table_name("task_strategy_success_rates");
        let rows = sqlx::query(&format!(
            "SELECT {TASK_STRATEGY_SUCCESS_RATE_COLUMNS} FROM {task_strategy_success_rates} \
             WHERE tenant_id = $1 AND task_fingerprint = $2 \
             ORDER BY success_rate DESC, uses DESC, subject_type ASC, subject_id ASC"
        ))
        .bind(tenant_id)
        .bind(task_fingerprint)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(task_strategy_success_rate_from_row)
            .collect()
    }
}

fn storage_partition_id(tenant_id: TenantId) -> String {
    StoragePartitionId::for_tenant(tenant_id).to_string()
}

async fn validate_expected_learning_compile_audit(
    conn: &mut PgConnection,
    tenant_id: &str,
    expected_operation_key: Option<&str>,
) -> Result<()> {
    let Some(expected_operation_key) = expected_operation_key else {
        return Ok(());
    };
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (\
             SELECT 1 FROM moa.execution_compile_audit \
             WHERE tenant_id = $1::UUID \
               AND contact_id IS NULL \
               AND source = 'skill_regression' \
               AND operation_key = $2\
         )",
    )
    .bind(tenant_id)
    .bind(expected_operation_key)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    if exists {
        Ok(())
    } else {
        Err(MoaError::StorageError(format!(
            "expected learning compile audit operation `{expected_operation_key}` was not persisted"
        )))
    }
}

fn merge_learning_evaluation_payload(
    mut current: serde_json::Value,
    terminal: serde_json::Value,
) -> Result<serde_json::Value> {
    let current_object = current.as_object_mut().ok_or_else(|| {
        MoaError::StorageError(
            "learning candidate evaluation_payload must be an object".to_string(),
        )
    })?;
    let terminal_object = terminal.as_object().ok_or_else(|| {
        MoaError::ValidationError(
            "terminal learning evaluation payload must be an object".to_string(),
        )
    })?;
    deep_merge_json_objects(current_object, terminal_object);
    Ok(current)
}

fn deep_merge_json_objects(
    current: &mut serde_json::Map<String, serde_json::Value>,
    terminal: &serde_json::Map<String, serde_json::Value>,
) {
    for (key, terminal_value) in terminal {
        match (current.get_mut(key), terminal_value) {
            (
                Some(serde_json::Value::Object(current_child)),
                serde_json::Value::Object(terminal_child),
            ) => {
                deep_merge_json_objects(current_child, terminal_child);
            }
            _ => {
                current.insert(key.clone(), terminal_value.clone());
            }
        }
    }
}

//! Experience-learning operations for the Postgres session store.

use super::*;
use sqlx::PgConnection;

impl PostgresSessionStore {
    /// Appends or idempotently refreshes one experience record.
    pub async fn append_experience_record(&self, experience: &ExperienceRecord) -> Result<()> {
        let experience_records = self.table_name("experience_records");
        sqlx::query(&format!(
            "INSERT INTO {experience_records} \
             (id, segment_id, session_id, storage_partition_id, user_id, tenant_id, task_summary, \
              task_fingerprint, task_fingerprint_payload, task_facets, actions, resources, \
              outcome, confidence, evidence, tools_used, skills_activated, turn_count, token_cost, \
              duration_ms, assessment_policy_version, extraction_policy_version, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
                     $18, $19, $20, $21, $22, $23) \
             ON CONFLICT (segment_id, extraction_policy_version) DO UPDATE SET \
                 task_summary = EXCLUDED.task_summary, \
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
        session_id: moa_core::SessionId,
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
        session_id: moa_core::SessionId,
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
                  effect, confidence, evidence, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                 ON CONFLICT (experience_id, subject_type, subject_id) DO UPDATE SET \
                     effect = EXCLUDED.effect, \
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

    /// Appends or idempotently refreshes one learning candidate.
    pub async fn append_learning_candidate(&self, candidate: &LearningCandidate) -> Result<()> {
        let mut conn = self.pool.acquire().await.map_err(map_sqlx_error)?;
        self.append_learning_candidate_with_conn(&mut conn, candidate)
            .await
    }

    /// Appends or idempotently refreshes one learning candidate using an existing connection.
    pub async fn append_learning_candidate_with_conn(
        &self,
        conn: &mut PgConnection,
        candidate: &LearningCandidate,
    ) -> Result<()> {
        let learning_candidates = self.table_name("learning_candidates");
        sqlx::query(&format!(
            "INSERT INTO {learning_candidates} \
             (id, tenant_id, storage_partition_id, user_id, candidate_type, status, target_id, target_label, \
              task_fingerprint, task_fingerprint_payload, task_facets, payload, evaluation_payload, \
              source_experience_ids, confidence, risk_class, promotion_requirements, status_reason, \
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
        .bind(candidate.status.as_str())
        .bind(candidate.target_id.as_deref())
        .bind(candidate.target_label.as_deref())
        .bind(candidate.task_fingerprint.as_ref().map(|value| value.hash.as_str()))
        .bind(candidate.task_fingerprint.clone().map(Json))
        .bind(candidate.task_facets.clone().map(Json))
        .bind(Json(candidate.payload.clone()))
        .bind(candidate.evaluation_payload.clone().map(Json))
        .bind(&candidate.source_experience_ids)
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
        Ok(())
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
        rows.iter().map(learning_candidate_from_row).collect()
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
        row.as_ref().map(learning_candidate_from_row).transpose()
    }

    /// Applies an explicit candidate status transition.
    pub async fn update_learning_candidate_status(
        &self,
        update: &LearningCandidateStatusUpdate,
    ) -> Result<()> {
        let affected = self
            .update_learning_candidate_status_with_expected(update, None)
            .await?;
        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "learning candidate `{}` was not found",
                update.candidate_id
            )));
        }
        Ok(())
    }

    /// Applies a status transition only when the candidate is still in the expected status.
    pub async fn update_learning_candidate_status_from(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: LearningCandidateStatus,
    ) -> Result<bool> {
        self.update_learning_candidate_status_with_expected(update, Some(expected_status))
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

    async fn update_learning_candidate_status_with_expected(
        &self,
        update: &LearningCandidateStatusUpdate,
        expected_status: Option<LearningCandidateStatus>,
    ) -> Result<u64> {
        let learning_candidates = self.table_name("learning_candidates");
        let affected = if let Some(expected_status) = expected_status {
            sqlx::query(&format!(
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
            .rows_affected()
        } else {
            sqlx::query(&format!(
                "UPDATE {learning_candidates} SET \
                 status = $1, \
                 status_reason = $2, \
                 evaluation_payload = COALESCE($3, evaluation_payload), \
                 updated_at = $4 \
                 WHERE id = $5"
            ))
            .bind(update.status.as_str())
            .bind(update.status_reason.as_deref())
            .bind(update.evaluation_payload.clone().map(Json))
            .bind(update.updated_at)
            .bind(update.candidate_id)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected()
        };
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

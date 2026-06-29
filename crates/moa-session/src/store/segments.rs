//! Task-segment operations for the Postgres session store.

use super::*;

impl PostgresSessionStore {
    /// Creates or refreshes one task segment metadata row.
    pub async fn create_segment(&self, segment: &TaskSegment) -> Result<()> {
        let task_segments = self.table_name("task_segments");
        let sessions = self.table_name("sessions");
        let affected = sqlx::query(&format!(
            "INSERT INTO {task_segments} \
             (id, session_id, storage_partition_id, user_id, tenant_id, segment_index, task_summary, \
              started_at, ended_at, outcome, assessment, outcome_confidence, \
              tools_used, skills_activated, turn_count, token_cost, previous_segment_id) \
             SELECT $1, $2, s.storage_partition_id, s.user_id, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15 \
             FROM {sessions} s WHERE s.id = $2 \
             ON CONFLICT (id) DO UPDATE SET \
                 storage_partition_id = EXCLUDED.storage_partition_id, \
                 user_id = EXCLUDED.user_id, \
                 tenant_id = EXCLUDED.tenant_id, \
                 task_summary = EXCLUDED.task_summary, \
                 ended_at = EXCLUDED.ended_at, \
                 outcome = EXCLUDED.outcome, \
                 assessment = EXCLUDED.assessment, \
                 outcome_confidence = EXCLUDED.outcome_confidence, \
                 tools_used = EXCLUDED.tools_used, \
                 skills_activated = EXCLUDED.skills_activated, \
                 turn_count = EXCLUDED.turn_count, \
                 token_cost = EXCLUDED.token_cost, \
                 previous_segment_id = EXCLUDED.previous_segment_id"
        ))
        .bind(segment.id.0)
        .bind(segment.session_id.0)
        .bind(&segment.tenant_id)
        .bind(segment.segment_index as i32)
        .bind(segment.task_summary.as_deref())
        .bind(segment.started_at)
        .bind(segment.ended_at)
        .bind(segment.outcome.as_deref())
        .bind(serialize_segment_assessment(
            segment.assessment.as_ref(),
        )?)
        .bind(segment.outcome_confidence)
        .bind(&segment.tools_used)
        .bind(&segment.skills_activated)
        .bind(segment.turn_count as i32)
        .bind(segment.token_cost as i64)
        .bind(segment.previous_segment_id.map(|id| id.0))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(segment.session_id));
        }
        Ok(())
    }

    /// Completes a task segment and stores its final counters.
    pub async fn complete_segment(
        &self,
        segment_id: SegmentId,
        update: SegmentCompletion,
    ) -> Result<()> {
        let task_segments = self.table_name("task_segments");
        let affected = sqlx::query(&format!(
            "UPDATE {task_segments} SET \
                 ended_at = $1, \
                 turn_count = $2, \
                 tools_used = $3, \
                 skills_activated = $4, \
                 token_cost = $5 \
             WHERE id = $6"
        ))
        .bind(update.ended_at)
        .bind(update.turn_count as i32)
        .bind(&update.tools_used)
        .bind(&update.skills_activated)
        .bind(update.token_cost as i64)
        .bind(segment_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "task segment `{segment_id}` was not found"
            )));
        }
        Ok(())
    }

    /// Loads the active task segment for a session, if present.
    pub async fn get_active_segment(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<TaskSegment>> {
        let task_segments = self.table_name("task_segments");
        let row = sqlx::query(&format!(
            "SELECT {TASK_SEGMENT_COLUMNS} FROM {task_segments} \
             WHERE session_id = $1 AND ended_at IS NULL \
             ORDER BY segment_index DESC \
             LIMIT 1"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.as_ref().map(task_segment_from_row).transpose()
    }

    /// Lists all task segments for a session in segment order.
    pub async fn list_segments(&self, session_id: moa_core::SessionId) -> Result<Vec<TaskSegment>> {
        let task_segments = self.table_name("task_segments");
        let rows = sqlx::query(&format!(
            "SELECT {TASK_SEGMENT_COLUMNS} FROM {task_segments} \
             WHERE session_id = $1 \
             ORDER BY segment_index ASC"
        ))
        .bind(session_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(task_segment_from_row).collect()
    }

    /// Updates a task segment outcome and assessment evidence.
    pub async fn update_segment_assessment(
        &self,
        segment_id: SegmentId,
        assessment: &SegmentAssessment,
    ) -> Result<()> {
        let task_segments = self.table_name("task_segments");
        let affected = sqlx::query(&format!(
            "UPDATE {task_segments} SET \
                 outcome = $1, \
                 assessment = $2, \
                 outcome_confidence = $3 \
             WHERE id = $4"
        ))
        .bind(assessment.outcome.as_str())
        .bind(serialize_segment_assessment(Some(assessment))?)
        .bind(assessment.confidence)
        .bind(segment_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "task segment `{segment_id}` was not found"
            )));
        }
        Ok(())
    }

    /// Loads the historical structural baseline for one tenant.
    pub async fn get_segment_baseline(&self, tenant_id: &str) -> Result<Option<SegmentBaseline>> {
        let segment_baselines = self.table_name("segment_baselines");
        let row = sqlx::query(&format!(
            "SELECT sample_count, avg_turns, stddev_turns, avg_cost, stddev_cost, \
                    avg_duration_secs, stddev_duration_secs \
             FROM {segment_baselines} \
             WHERE tenant_id = $1 \
             LIMIT 1"
        ))
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| {
            Ok(SegmentBaseline {
                sample_count: row.col::<i64>("sample_count")? as usize,
                avg_turns: row.col::<f64>("avg_turns")?,
                stddev_turns: row.col::<Option<f64>>("stddev_turns")?,
                avg_cost: row.col::<f64>("avg_cost")?,
                stddev_cost: row.col::<Option<f64>>("stddev_cost")?,
                avg_duration_secs: row.col::<f64>("avg_duration_secs")?,
                stddev_duration_secs: row.col::<Option<f64>>("stddev_duration_secs")?,
            })
        })
        .transpose()
    }

    /// Lists skill outcome-rate aggregates for ranking.
    pub async fn list_skill_resolution_rates(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SkillResolutionRate>> {
        let skill_resolution_rates = self.table_name("skill_resolution_rates");
        let rows = sqlx::query(&format!(
            "SELECT skill_name, uses, resolution_rate, avg_token_cost, avg_turn_count \
             FROM {skill_resolution_rates} \
             WHERE tenant_id = $1 \
             ORDER BY resolution_rate DESC, uses DESC, skill_name ASC"
        ))
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter()
            .map(|row| {
                Ok(SkillResolutionRate {
                    skill_name: row.col::<String>("skill_name")?,
                    uses: row.col::<i64>("uses")? as u64,
                    resolution_rate: row.col::<f64>("resolution_rate")?,
                    avg_token_cost: row.col::<f64>("avg_token_cost")?,
                    avg_turn_count: row.col::<f64>("avg_turn_count")?,
                })
            })
            .collect()
    }

    /// Refreshes task-segment derived materialized views.
    pub async fn refresh_segment_materialized_views(&self) -> Result<()> {
        for view_name in [
            "skill_resolution_rates",
            "segment_baselines",
            "task_strategy_success_rates",
        ] {
            let qualified = self.table_name(view_name);
            sqlx::query(&format!(
                "REFRESH MATERIALIZED VIEW CONCURRENTLY {qualified}"
            ))
            .execute(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        }
        Ok(())
    }
    /// Records a tool name on the active task segment for a session.
    pub async fn record_active_segment_tool_use(
        &self,
        session_id: moa_core::SessionId,
        tool_name: &str,
    ) -> Result<()> {
        self.append_unique_active_segment_value(session_id, "tools_used", tool_name)
            .await
    }

    /// Records a skill activation on the active task segment for a session.
    pub async fn record_active_segment_skill_activation(
        &self,
        session_id: moa_core::SessionId,
        skill_name: &str,
    ) -> Result<()> {
        self.append_unique_active_segment_value(session_id, "skills_activated", skill_name)
            .await
    }

    /// Adds one turn and token usage to the active task segment for a session.
    pub async fn record_active_segment_turn_usage(
        &self,
        session_id: moa_core::SessionId,
        token_cost: u64,
    ) -> Result<()> {
        let task_segments = self.table_name("task_segments");
        sqlx::query(&format!(
            "UPDATE {task_segments} SET \
                 turn_count = turn_count + 1, \
                 token_cost = token_cost + $1 \
             WHERE id = ( \
                 SELECT id FROM {task_segments} \
                 WHERE session_id = $2 AND ended_at IS NULL \
                 ORDER BY segment_index DESC \
                 LIMIT 1 \
             )"
        ))
        .bind(token_cost as i64)
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    async fn append_unique_active_segment_value(
        &self,
        session_id: moa_core::SessionId,
        column: &str,
        value: &str,
    ) -> Result<()> {
        let task_segments = self.table_name("task_segments");
        let column = match column {
            "tools_used" => "tools_used",
            "skills_activated" => "skills_activated",
            _ => {
                return Err(MoaError::StorageError(format!(
                    "unsupported task segment array column `{column}`"
                )));
            }
        };
        sqlx::query(&format!(
            "UPDATE {task_segments} SET \
                 {column} = CASE \
                     WHEN $1 = ANY({column}) THEN {column} \
                     ELSE array_append({column}, $1) \
                 END \
             WHERE id = ( \
                 SELECT id FROM {task_segments} \
                 WHERE session_id = $2 AND ended_at IS NULL \
                 ORDER BY segment_index DESC \
                 LIMIT 1 \
             )"
        ))
        .bind(value)
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }
}

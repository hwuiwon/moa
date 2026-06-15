//! `moa_core::SessionStore` implementation for `PostgresSessionStore`.

use super::*;

impl PostgresSessionStore {
    /// Insert a session metadata row using a caller-owned transaction.
    ///
    /// This lets higher-level handlers atomically persist the session and its
    /// authorization outbox tuples. The caller owns commit/rollback and should
    /// call [`PostgresSessionStore::refresh_active_session_metric`] after a
    /// successful commit.
    pub async fn create_session_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        meta: SessionMeta,
    ) -> Result<moa_core::SessionId> {
        let session_id = meta.id;
        let sessions = self.table_name("sessions");
        sqlx::query(&format!(
            "INSERT INTO {sessions} ({SESSION_INSERT_COLUMNS}) VALUES \
             ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)"
        ))
        .bind(session_id.0)
        .bind(meta.workspace_id.to_string())
        .bind(meta.user_id.to_string())
        .bind(meta.title)
        .bind(session_status_to_db(&meta.status))
        .bind(platform_to_db(&meta.platform))
        .bind(meta.platform_channel)
        .bind(meta.model.to_string())
        .bind(meta.created_at)
        .bind(meta.updated_at)
        .bind(meta.completed_at)
        .bind(meta.parent_session_id.map(|value| value.0))
        .bind(meta.total_input_tokens_uncached as i64)
        .bind(meta.total_input_tokens_cache_write as i64)
        .bind(meta.total_input_tokens_cache_read as i64)
        .bind(meta.total_output_tokens as i64)
        .bind(meta.total_cost_cents as i64)
        .bind(meta.event_count as i64)
        .bind(0_i64)
        .bind(meta.last_checkpoint_seq.map(|value| value as i64))
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;
        record_session_created(&meta.workspace_id, &meta.status);

        Ok(session_id)
    }
}

#[async_trait]
impl SessionStore for PostgresSessionStore {
    /// Creates a new session record.
    async fn create_session(&self, meta: SessionMeta) -> Result<moa_core::SessionId> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let session_id = self.create_session_in_tx(&mut transaction, meta).await?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        self.refresh_active_session_metric().await?;

        Ok(session_id)
    }

    /// Appends an event to the session log.
    async fn emit_event(&self, session_id: moa_core::SessionId, event: Event) -> Result<u64> {
        Ok(self
            .emit_event_record(session_id, event)
            .await?
            .sequence_num)
    }

    /// Appends an event and returns the persisted event record.
    async fn emit_event_record(
        &self,
        session_id: moa_core::SessionId,
        event: Event,
    ) -> Result<EventRecord> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let event_id = Uuid::now_v7();
        let event_type = event.type_name();
        let event_type_record = event.event_type();
        let hand_id = event_hand_id(&event);
        let token_count = event.token_count();
        let payload = encode_event_for_storage(
            self.blob_store.as_ref(),
            &session_id,
            &event,
            self.blob_threshold_bytes,
        )
        .await?;
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let events = self.table_name("events");

        let locked_session = sqlx::query(&format!(
            "SELECT event_count, workspace_id, user_id FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        .ok_or(MoaError::SessionNotFound(session_id))?;
        let sequence_num = locked_session
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as u64;
        let workspace_id = locked_session
            .try_get::<String, _>("workspace_id")
            .map_err(map_sqlx_error)?;
        let user_id = locked_session
            .try_get::<String, _>("user_id")
            .map_err(map_sqlx_error)?;

        sqlx::query(&format!(
            "INSERT INTO {events} \
             (id, session_id, workspace_id, user_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ))
        .bind(event_id)
        .bind(session_id.0)
        .bind(workspace_id)
        .bind(user_id)
        .bind(sequence_num as i64)
        .bind(event_type)
        .bind(Json(payload))
        .bind(now)
        .bind(Option::<Uuid>::None)
        .bind(&hand_id)
        .bind(token_count as i32)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        transaction.commit().await.map_err(map_sqlx_error)?;
        if let Event::BrainResponse {
            model, model_tier, ..
        } = &event
        {
            record_turn_completed(model, *model_tier);
        }
        record_session_event_append(event_type);
        Ok(EventRecord {
            id: event_id,
            session_id,
            sequence_num,
            event_type: event_type_record,
            event,
            timestamp: now,
            brain_id: None,
            hand_id,
            token_count: Some(token_count),
        })
    }

    async fn store_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        text: &str,
    ) -> Result<ClaimCheck> {
        let blob_id = self.blob_store.store(&session_id, text.as_bytes()).await?;
        Ok(ClaimCheck {
            blob_id,
            size: text.len(),
            preview: preview_text(text),
        })
    }

    async fn load_text_artifact(
        &self,
        session_id: moa_core::SessionId,
        claim_check: &ClaimCheck,
    ) -> Result<String> {
        let bytes = self
            .blob_store
            .get(&session_id, &claim_check.blob_id)
            .await?;
        String::from_utf8(bytes).map_err(|error| {
            MoaError::StorageError(format!(
                "blob `{}` did not contain valid UTF-8: {error}",
                claim_check.blob_id
            ))
        })
    }

    /// Retrieves events for a session within a sequence and type range.
    async fn get_events(
        &self,
        session_id: moa_core::SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        if matches!(range.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let started_at = std::time::Instant::now();
        let events = self.table_name("events");

        let use_recent_order =
            range.limit.is_some() && range.from_seq.is_none() && range.to_seq.is_none();

        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {EVENT_COLUMNS} FROM {events} WHERE session_id = "
        ));
        query.push_bind(session_id.0);

        if let Some(from_seq) = range.from_seq {
            query.push(" AND sequence_num >= ");
            query.push_bind(from_seq as i64);
        }
        if let Some(to_seq) = range.to_seq {
            query.push(" AND sequence_num <= ");
            query.push_bind(to_seq as i64);
        }
        if let Some(event_types) = range.event_types {
            query.push(" AND event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type_to_db(&event_type));
            }
            separated.push_unseparated(")");
        }

        if use_recent_order {
            query.push(" ORDER BY sequence_num DESC");
        } else {
            query.push(" ORDER BY sequence_num ASC");
        }
        if let Some(limit) = range.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let decoded_bytes = rows.iter().try_fold(0_u64, |total, row| {
            let payload_bytes = row
                .try_get_raw("payload")
                .map_err(map_sqlx_error)?
                .as_bytes()
                .map_err(|error| {
                    MoaError::StorageError(format!("failed to read event payload bytes: {error}"))
                })?;
            Ok::<_, MoaError>(total + payload_bytes.len() as u64)
        })?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        if use_recent_order {
            events.reverse();
        }
        record_session_event_load(events.len() as u64);
        record_session_event_decoded_bytes(decoded_bytes);
        record_session_event_replay(events.len(), decoded_bytes, started_at.elapsed());
        Ok(events)
    }

    /// Loads a persisted session metadata record.
    async fn get_session(&self, session_id: moa_core::SessionId) -> Result<SessionMeta> {
        let sessions = self.table_name("sessions");
        let query =
            format!("SELECT {SESSION_SELECT_COLUMNS} FROM {sessions} WHERE id = $1 LIMIT 1");
        let row = sqlx::query(&query)
            .bind(session_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(MoaError::SessionNotFound(session_id))?;
        session_meta_from_row(&row)
    }

    /// Updates the status of an existing session.
    async fn update_status(
        &self,
        session_id: moa_core::SessionId,
        status: SessionStatus,
    ) -> Result<()> {
        let now = Utc::now();
        let sessions = self.table_name("sessions");
        let completed_at = if matches!(
            status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            Some(now)
        } else {
            None
        };

        let affected = sqlx::query(&format!(
            "UPDATE {sessions} SET status = $1, updated_at = $2, completed_at = $3 WHERE id = $4"
        ))
        .bind(session_status_to_db(&status))
        .bind(now)
        .bind(completed_at)
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }
        self.refresh_active_session_metric().await?;

        Ok(())
    }

    /// Stores the latest context snapshot for a session.
    async fn put_snapshot(
        &self,
        session_id: moa_core::SessionId,
        snapshot: ContextSnapshot,
    ) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        let sessions = self.table_name("sessions");
        let affected = sqlx::query(&format!(
            "INSERT INTO {context_snapshots} \
             (session_id, workspace_id, user_id, format_version, last_sequence_num, payload, created_at) \
             SELECT $1, s.workspace_id, s.user_id, $2, $3, $4, $5 \
             FROM {sessions} s WHERE s.id = $1 \
             ON CONFLICT (session_id) DO UPDATE SET \
                 workspace_id = EXCLUDED.workspace_id, \
                 user_id = EXCLUDED.user_id, \
                 format_version = EXCLUDED.format_version, \
                 last_sequence_num = EXCLUDED.last_sequence_num, \
                 payload = EXCLUDED.payload, \
                 created_at = EXCLUDED.created_at"
        ))
        .bind(session_id.0)
        .bind(snapshot.format_version as i32)
        .bind(snapshot.last_sequence_num as i64)
        .bind(Json(snapshot))
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }
        Ok(())
    }

    /// Loads the latest context snapshot for a session when one exists.
    async fn get_snapshot(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<ContextSnapshot>> {
        let context_snapshots = self.table_name("context_snapshots");
        let row = sqlx::query(&format!(
            "SELECT payload FROM {context_snapshots} WHERE session_id = $1 LIMIT 1"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| {
            row.try_get::<Json<ContextSnapshot>, _>("payload")
                .map(|payload| payload.0)
                .map_err(map_sqlx_error)
        })
        .transpose()
    }

    /// Deletes the stored context snapshot for a session.
    async fn delete_snapshot(&self, session_id: moa_core::SessionId) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        sqlx::query(&format!(
            "DELETE FROM {context_snapshots} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Searches events using `PostgreSQL` full-text search and optional session filters.
    async fn search_events(
        &self,
        query_text: &str,
        filter: EventFilter,
    ) -> Result<Vec<EventRecord>> {
        let normalized_query = normalize_event_search_query(query_text);
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }
        if matches!(filter.event_types, Some(ref types) if types.is_empty()) {
            return Ok(Vec::new());
        }
        let events = self.table_name("events");
        let sessions = self.table_name("sessions");

        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT e.id, e.session_id, e.sequence_num, e.event_type, e.payload, \
             e.timestamp, e.brain_id, e.hand_id, e.token_count, \
             ts_rank(e.search_vector, plainto_tsquery('english', "
                .to_string(),
        );
        query.push_bind(normalized_query.clone());
        query.push(format!(
            ")) AS rank \
             FROM {events} e JOIN {sessions} s ON s.id = e.session_id \
             WHERE e.search_vector @@ plainto_tsquery('english', "
        ));
        query.push_bind(normalized_query);
        query.push(")");

        if let Some(session_id) = filter.session_id {
            query.push(" AND e.session_id = ");
            query.push_bind(session_id.0);
        }
        if let Some(workspace_id) = filter.workspace_id {
            query.push(" AND s.workspace_id = ");
            query.push_bind(workspace_id.to_string());
        }
        if let Some(user_id) = filter.user_id {
            query.push(" AND s.user_id = ");
            query.push_bind(user_id.to_string());
        }
        if let Some(from_time) = filter.from_time {
            query.push(" AND e.timestamp >= ");
            query.push_bind(from_time);
        }
        if let Some(to_time) = filter.to_time {
            query.push(" AND e.timestamp <= ");
            query.push_bind(to_time);
        }
        if let Some(event_types) = filter.event_types {
            query.push(" AND e.event_type IN (");
            let mut separated = query.separated(", ");
            for event_type in event_types {
                separated.push_bind(event_type_to_db(&event_type));
            }
            separated.push_unseparated(")");
        }

        query.push(" ORDER BY rank DESC, e.timestamp DESC, e.sequence_num DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in &rows {
            events.push(self.event_record_from_row(row).await?);
        }
        Ok(events)
    }

    /// Lists sessions filtered by workspace, user, status, or platform.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let sessions = self.table_name("sessions");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {SESSION_SUMMARY_COLUMNS} FROM {sessions} WHERE TRUE"
        ));

        if let Some(workspace_id) = filter.workspace_id {
            query.push(" AND workspace_id = ");
            query.push_bind(workspace_id.to_string());
        }
        if let Some(user_id) = filter.user_id {
            query.push(" AND user_id = ");
            query.push_bind(user_id.to_string());
        }
        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(session_status_to_db(&status));
        }
        if let Some(platform) = filter.platform {
            query.push(" AND platform = ");
            query.push_bind(platform_to_db(&platform));
        }

        query.push(" ORDER BY updated_at DESC");
        if let Some(limit) = filter.limit {
            query.push(" LIMIT ");
            query.push_bind(limit as i64);
        }

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(session_summary_from_row).collect()
    }

    /// Returns aggregate workspace spend in cents since the provided UTC timestamp.
    async fn workspace_cost_since(
        &self,
        workspace_id: &WorkspaceId,
        since: DateTime<Utc>,
    ) -> Result<u32> {
        let events = self.table_name("events");
        let sessions = self.table_name("sessions");
        let total = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COALESCE( \
                 SUM((e.payload -> 'data' ->> 'cost_cents')::BIGINT), \
                 0 \
             )::BIGINT \
             FROM {events} e \
             JOIN {sessions} s ON s.id = e.session_id \
             WHERE s.workspace_id = $1 \
               AND e.event_type = $2 \
               AND e.timestamp >= $3"
        ))
        .bind(workspace_id.to_string())
        .bind("BrainResponse")
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        u32::try_from(total)
            .map_err(|_| MoaError::StorageError("workspace spend exceeded u32 range".to_string()))
    }

    /// Deletes a session only when it has no append-only events.
    async fn delete_empty_session(&self, session_id: moa_core::SessionId) -> Result<()> {
        let events = self.table_name("events");
        let pending_signals = self.table_name("pending_signals");
        let context_snapshots = self.table_name("context_snapshots");
        let task_segments = self.table_name("task_segments");
        let sessions = self.table_name("sessions");

        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let event_count = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {events} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        if event_count > 0 {
            return Err(MoaError::Unsupported(format!(
                "session `{session_id}` has {event_count} append-only event(s); use privacy erase or tombstoning instead"
            )));
        }

        for sql in [
            format!("DELETE FROM {pending_signals} WHERE session_id = $1"),
            format!("DELETE FROM {context_snapshots} WHERE session_id = $1"),
            format!("DELETE FROM {task_segments} WHERE session_id = $1"),
        ] {
            sqlx::query(&sql)
                .bind(session_id.0)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx_error)?;
        }
        let deleted = sqlx::query(&format!("DELETE FROM {sessions} WHERE id = $1"))
            .bind(session_id.0)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();
        if deleted == 0 {
            return Err(MoaError::SessionNotFound(session_id));
        }

        transaction.commit().await.map_err(map_sqlx_error)?;
        self.refresh_active_session_metric().await?;

        if let Err(err) = self.blob_store.delete_session(&session_id).await {
            tracing::warn!(%err, session_id = %session_id, "blob cleanup failed after empty session delete");
        }

        Ok(())
    }

    async fn create_segment(&self, segment: &TaskSegment) -> Result<()> {
        PostgresSessionStore::create_segment(self, segment).await
    }

    async fn complete_segment(
        &self,
        segment_id: SegmentId,
        update: SegmentCompletion,
    ) -> Result<()> {
        PostgresSessionStore::complete_segment(self, segment_id, update).await
    }

    async fn get_active_segment(
        &self,
        session_id: moa_core::SessionId,
    ) -> Result<Option<TaskSegment>> {
        PostgresSessionStore::get_active_segment(self, session_id).await
    }

    async fn list_segments(&self, session_id: moa_core::SessionId) -> Result<Vec<TaskSegment>> {
        PostgresSessionStore::list_segments(self, session_id).await
    }

    async fn update_segment_resolution(
        &self,
        segment_id: SegmentId,
        resolution: &str,
        confidence: f64,
    ) -> Result<()> {
        PostgresSessionStore::update_segment_resolution(self, segment_id, resolution, confidence)
            .await
    }

    async fn update_segment_resolution_score(
        &self,
        segment_id: SegmentId,
        score: &ResolutionScore,
    ) -> Result<()> {
        PostgresSessionStore::update_segment_resolution_score(self, segment_id, score).await
    }

    async fn get_segment_baseline(&self, tenant_id: &str) -> Result<Option<SegmentBaseline>> {
        PostgresSessionStore::get_segment_baseline(self, tenant_id).await
    }

    async fn list_skill_resolution_rates(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SkillResolutionRate>> {
        PostgresSessionStore::list_skill_resolution_rates(self, tenant_id).await
    }

    async fn refresh_segment_materialized_views(&self) -> Result<()> {
        PostgresSessionStore::refresh_segment_materialized_views(self).await
    }

    async fn record_active_segment_tool_use(
        &self,
        session_id: moa_core::SessionId,
        tool_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_tool_use(self, session_id, tool_name).await
    }

    async fn record_active_segment_skill_activation(
        &self,
        session_id: moa_core::SessionId,
        skill_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_skill_activation(self, session_id, skill_name)
            .await
    }

    async fn record_active_segment_turn_usage(
        &self,
        session_id: moa_core::SessionId,
        token_cost: u64,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_turn_usage(self, session_id, token_cost).await
    }
}

//! `moa_core::traits::SessionStore` and related store-trait adapters.

use super::session_records::{agent_context_from_row, session_actor_id, session_actor_type};
use super::*;

#[async_trait]
impl SessionStore for PostgresSessionStore {
    /// Creates a new session record.
    async fn create_session(
        &self,
        meta: SessionMeta,
    ) -> Result<moa_core::types::identifiers::SessionId> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let outcome = self.create_session_in_tx(&mut transaction, meta).await?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        // The active-session gauge is refreshed off the write path on a timer
        // (see `spawn_active_session_gauge_refresher`); no COUNT(*) here.

        Ok(outcome.session_id)
    }

    /// Appends an event to the session log.
    async fn emit_event(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        event: Event,
    ) -> Result<u64> {
        Ok(self
            .emit_event_record(session_id, event, None)
            .await?
            .sequence_num)
    }

    /// Appends an event under the session-row lock, optionally deduplicated,
    /// returning the persisted record.
    ///
    /// When `dedupe_key` is `None` the event is always appended. When it is
    /// `Some` and a `session_event_dedupe` row already exists for
    /// `(session_id, dedupe_key)`, the previously persisted event is returned
    /// without inserting a second event; otherwise the event and a matching
    /// dedupe row are inserted together in the same transaction so a retry
    /// short-circuits.
    async fn emit_event_record(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        event: Event,
        dedupe_key: Option<String>,
    ) -> Result<EventRecord> {
        let mut records = self
            .append_events(session_id, vec![EventAppend { event, dedupe_key }])
            .await?;
        records.pop().ok_or_else(|| {
            MoaError::StorageError(
                "append_events returned no record for a single event".to_string(),
            )
        })
    }

    async fn store_text_artifact(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
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
        session_id: moa_core::types::identifiers::SessionId,
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
        session_id: moa_core::types::identifiers::SessionId,
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
                separated.push_bind(event_type.as_str());
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
        // Collect the distinct claim-checked blob ids across all rows and fetch
        // them once, instead of one blob `get` per event during decode.
        let mut payloads = Vec::with_capacity(rows.len());
        let mut blob_ids: Vec<String> = Vec::new();
        let mut seen_blob_ids = std::collections::HashSet::new();
        for row in &rows {
            let payload = row.col::<serde_json::Value>("payload")?;
            let mut ids = Vec::new();
            crate::blob::collect_claim_check_blob_ids(&payload, &mut ids)?;
            for id in ids {
                if seen_blob_ids.insert(id.clone()) {
                    blob_ids.push(id);
                }
            }
            payloads.push(payload);
        }
        let blob_cache = self.blob_store.get_many(&session_id, &blob_ids).await?;
        let mut events = Vec::with_capacity(rows.len());
        for (row, payload) in rows.iter().zip(payloads) {
            let event = crate::blob::decode_event_from_cache(payload, &blob_cache)?;
            events.push(Self::event_record_from_row_parts(row, event)?);
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
    async fn get_session(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<SessionMeta> {
        let sessions = self.table_name("sessions");
        let agent_contexts = self.table_name("session_agent_context");
        let session_columns = SESSION_SELECT_COLUMNS
            .split(", ")
            .map(|column| format!("s.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            r#"
            SELECT {session_columns},
                   ac.agent_id, ac.installation_uid, ac.deployment_uid,
                   ac.agent_definition_ref, ac.agent_revision_uid, ac.policy_hash,
                   ac.display_name, ac.policy_snapshot, ac.artifact_dependencies,
                   ac.tool_dependencies
            FROM {sessions} AS s
            LEFT JOIN {agent_contexts} AS ac ON ac.session_id = s.id
            WHERE s.id = $1
            LIMIT 1
            "#
        );
        let row = sqlx::query(&query)
            .bind(session_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(MoaError::SessionNotFound(session_id))?;
        let mut meta = session_meta_from_row(&row)?;
        let agent_context = agent_context_from_row(&row)?.ok_or_else(|| {
            MoaError::StorageError(format!(
                "session {session_id} is missing required agent context"
            ))
        })?;
        meta.agent_context = Some(agent_context);
        Ok(meta)
    }

    /// Updates the status of an existing session.
    async fn update_status(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
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
        .bind(status.as_str())
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
        // Active-session gauge is refreshed off the write path on a timer.

        Ok(())
    }

    async fn update_session_contact(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        contact: moa_core::types::contact::ContactRef,
        promoted_from: Option<moa_core::types::contact::ContactId>,
    ) -> Result<()> {
        PostgresSessionStore::update_session_contact(self, session_id, contact, promoted_from).await
    }

    /// Stores the latest context snapshot for a session.
    async fn put_snapshot(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        snapshot: ContextSnapshot,
    ) -> Result<()> {
        let context_snapshots = self.table_name("context_snapshots");
        let sessions = self.table_name("sessions");
        let affected = sqlx::query(&format!(
            "INSERT INTO {context_snapshots} \
             (session_id, storage_partition_id, user_id, format_version, last_sequence_num, payload, created_at) \
             SELECT $1, s.storage_partition_id, s.user_id, $2, $3, $4, $5 \
             FROM {sessions} s WHERE s.id = $1 \
             ON CONFLICT (session_id) DO UPDATE SET \
                 storage_partition_id = EXCLUDED.storage_partition_id, \
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
        session_id: moa_core::types::identifiers::SessionId,
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
            row.col::<Json<ContextSnapshot>>("payload")
                .map(|payload| payload.0)
        })
        .transpose()
    }

    /// Deletes the stored context snapshot for a session.
    async fn delete_snapshot(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<()> {
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

        // The `events` table carries no STORED tsvector column (dropped from the
        // append hot path). This rare, admin-only search computes the vector on
        // the fly over `event_type` + payload text.
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT e.id, e.session_id, e.sequence_num, e.event_type, e.payload, \
             e.timestamp, e.brain_id, e.hand_id, e.token_count, \
             ts_rank(to_tsvector('english', e.event_type || ' ' || e.payload::text), \
             plainto_tsquery('english', "
                .to_string(),
        );
        query.push_bind(normalized_query.clone());
        query.push(format!(
            ")) AS rank \
             FROM {events} e JOIN {sessions} s ON s.id = e.session_id \
             WHERE to_tsvector('english', e.event_type || ' ' || e.payload::text) \
             @@ plainto_tsquery('english', "
        ));
        query.push_bind(normalized_query);
        query.push(")");

        if let Some(session_id) = filter.session_id {
            query.push(" AND e.session_id = ");
            query.push_bind(session_id.0);
        }
        if let Some(tenant_id) = filter.tenant_id {
            query.push(" AND e.tenant_id = ");
            query.push_bind(tenant_id.0);
        }
        if let Some(contact_id) = filter.contact_id {
            query.push(" AND e.contact_id = ");
            query.push_bind(contact_id.0);
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
                separated.push_bind(event_type.as_str());
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

    /// Lists sessions filtered by tenant, contact, status, or channel.
    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<SessionSummary>> {
        let sessions = self.table_name("sessions");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {SESSION_SUMMARY_COLUMNS} FROM {sessions} WHERE TRUE"
        ));

        if let Some(tenant_id) = filter.tenant_id {
            query.push(" AND tenant_id = ");
            query.push_bind(tenant_id.0);
        }
        if let Some(contact_id) = filter.contact_id {
            query.push(" AND contact_id = ");
            query.push_bind(contact_id.0);
        }
        if let Some(created_by) = filter.created_by {
            query.push(" AND created_by_actor_type = ");
            query.push_bind(session_actor_type(&created_by));
            match session_actor_id(&created_by) {
                Some(actor_id) => {
                    query.push(" AND created_by_actor_id = ");
                    query.push_bind(actor_id);
                }
                None => {
                    query.push(" AND created_by_actor_id IS NULL");
                }
            }
        }
        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(status.as_str());
        }
        if let Some(channel) = filter.channel {
            query.push(" AND channel = ");
            query.push_bind(channel.as_str());
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

    /// Returns aggregate tenant spend in cents since the provided UTC timestamp.
    async fn tenant_cost_since(&self, tenant_id: &TenantId, since: DateTime<Utc>) -> Result<u32> {
        let events = self.table_name("events");
        let total = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COALESCE( \
                 SUM((e.payload -> 'data' ->> 'cost_cents')::BIGINT), \
                 0 \
             )::BIGINT \
             FROM {events} e \
             WHERE e.tenant_id = $1 \
               AND e.event_type = $2 \
               AND e.timestamp >= $3"
        ))
        .bind(tenant_id.0)
        .bind("BrainResponse")
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        u32::try_from(total)
            .map_err(|_| MoaError::StorageError("tenant spend exceeded u32 range".to_string()))
    }

    /// Deletes a session only when it has no append-only events.
    async fn delete_empty_session(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<()> {
        let events = self.table_name("events");
        let pending_signals = self.table_name("pending_signals");
        let context_snapshots = self.table_name("context_snapshots");
        let task_segments = self.table_name("task_segments");
        let session_attachments = self.table_name("session_attachments");
        let sessions = self.table_name("sessions");

        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let Some(tenant_uuid) = sqlx::query_scalar::<_, Uuid>(&format!(
            "SELECT tenant_id FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        else {
            return Err(MoaError::SessionNotFound(session_id));
        };
        let tenant_id = TenantId(tenant_uuid);
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

        let attachment_object_keys = sqlx::query(&format!(
            "DELETE FROM {session_attachments} \
             WHERE tenant_id = $1 AND session_id = $2 \
             RETURNING object_key"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .fetch_all(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?
        .into_iter()
        .map(|row| row.col::<String>("object_key"))
        .collect::<Result<Vec<_>>>()?;
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
        // Active-session gauge is refreshed off the write path on a timer.

        if let Err(err) = self.blob_store.delete_session(&session_id).await {
            tracing::warn!(%err, session_id = %session_id, "blob cleanup failed after empty session delete");
        }
        for object_key in attachment_object_keys {
            if let Err(err) = self.attachment_store.delete(&object_key).await {
                tracing::warn!(
                    %err,
                    session_id = %session_id,
                    object_key,
                    "attachment object cleanup failed after empty session delete"
                );
            }
        }

        Ok(())
    }
}

#[async_trait]
impl SegmentStore for PostgresSessionStore {
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
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Option<TaskSegment>> {
        PostgresSessionStore::get_active_segment(self, session_id).await
    }

    async fn list_segments(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Vec<TaskSegment>> {
        PostgresSessionStore::list_segments(self, session_id).await
    }

    async fn update_segment_assessment(
        &self,
        segment_id: SegmentId,
        assessment: &SegmentAssessment,
    ) -> Result<()> {
        PostgresSessionStore::update_segment_assessment(self, segment_id, assessment).await
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

    async fn list_task_strategy_success_rates(
        &self,
        tenant_id: &str,
        task_fingerprint: &str,
    ) -> Result<Vec<TaskStrategySuccessRate>> {
        PostgresSessionStore::list_task_strategy_success_rates(self, tenant_id, task_fingerprint)
            .await
    }

    async fn refresh_segment_materialized_views(&self) -> Result<()> {
        PostgresSessionStore::refresh_segment_materialized_views(self).await
    }

    async fn record_active_segment_tool_use(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        tool_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_tool_use(self, session_id, tool_name).await
    }

    async fn record_active_segment_skill_activation(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        skill_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_skill_activation(self, session_id, skill_name)
            .await
    }

    async fn record_active_segment_skill_use(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        skill_name: &str,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_skill_use(self, session_id, skill_name).await
    }

    async fn record_active_segment_turn_usage(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        token_cost: u64,
    ) -> Result<()> {
        PostgresSessionStore::record_active_segment_turn_usage(self, session_id, token_cost).await
    }
}

#[async_trait]
impl ExperienceStore for PostgresSessionStore {
    async fn append_experience_record(&self, experience: &ExperienceRecord) -> Result<()> {
        PostgresSessionStore::append_experience_record(self, experience).await
    }

    async fn get_experience_record(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        experience_id: uuid::Uuid,
    ) -> Result<Option<ExperienceRecord>> {
        PostgresSessionStore::get_experience_record(self, session_id, experience_id).await
    }

    async fn list_experience_records(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Vec<ExperienceRecord>> {
        PostgresSessionStore::list_experience_records(self, session_id).await
    }

    async fn append_experience_attributions(
        &self,
        attributions: &[ExperienceAttribution],
    ) -> Result<()> {
        PostgresSessionStore::append_experience_attributions(self, attributions).await
    }

    async fn list_experience_attributions(
        &self,
        experience_id: uuid::Uuid,
    ) -> Result<Vec<ExperienceAttribution>> {
        PostgresSessionStore::list_experience_attributions(self, experience_id).await
    }
}

#[async_trait]
impl LearningCandidateStore for PostgresSessionStore {
    async fn append_learning_candidate(&self, candidate: &LearningCandidate) -> Result<()> {
        PostgresSessionStore::append_learning_candidate(self, candidate).await
    }

    async fn get_learning_candidate(
        &self,
        tenant_id: &TenantId,
        candidate_id: Uuid,
    ) -> Result<Option<LearningCandidate>> {
        PostgresSessionStore::get_learning_candidate(self, tenant_id, candidate_id).await
    }

    async fn list_learning_candidates(
        &self,
        tenant_id: &str,
        status: Option<LearningCandidateStatus>,
        limit: usize,
    ) -> Result<Vec<LearningCandidate>> {
        PostgresSessionStore::list_learning_candidates(self, tenant_id, status, limit).await
    }

    async fn update_learning_candidate_status(
        &self,
        update: &LearningCandidateStatusUpdate,
    ) -> Result<()> {
        PostgresSessionStore::update_learning_candidate_status(self, update).await
    }
}

//! Append-only session event persistence and lookup.

use std::{collections::HashMap, time::Instant};

use super::*;
use moa_core::types::security::{ToolCapabilityId, ToolOutputAssessment};

/// One event prepared for insertion: blobs already offloaded, metadata computed.
struct PreparedAppend {
    id: Uuid,
    event: Event,
    event_type: &'static str,
    event_type_record: EventType,
    hand_id: Option<String>,
    token_count: usize,
    payload: serde_json::Value,
    dedupe_key: Option<String>,
}

/// Where an appended event's persisted identity comes from in the result set.
enum AppendPlan {
    /// Freshly inserted at this sequence number.
    Insert { sequence_num: u64 },
    /// A dedupe hit against an event already persisted in a prior transaction.
    DbHit { sequence_num: u64 },
    /// A dedupe hit against an earlier entry in this same batch.
    BatchDup { sequence_num: u64 },
}

/// Deltas folded into the session aggregate columns by a single UPDATE.
///
/// `BrainResponse`, `Checkpoint`, and `GuardrailCheck` all contribute
/// token/cost totals so `sessions.total_cost_cents` captures every billed
/// model call (the guardrail judge is auxiliary spend). Only `BrainResponse`
/// increments `turn_count`, since a turn is one visible response. This mirrors
/// the event set summed by the `session_summary` view and `tenant_cost_since`.
#[derive(Default)]
struct SessionAggregateDelta {
    event_count: i64,
    turn_count: i64,
    input_tokens_uncached: i64,
    input_tokens_cache_write: i64,
    input_tokens_cache_read: i64,
    output_tokens: i64,
    cost_cents: i64,
    last_checkpoint_seq: Option<i64>,
}

impl SessionAggregateDelta {
    fn add_event(&mut self, event: &Event, sequence_num: u64) {
        self.event_count += 1;
        match event {
            Event::BrainResponse {
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                cost_cents,
                ..
            } => {
                self.turn_count += 1;
                self.input_tokens_uncached += *input_tokens_uncached as i64;
                self.input_tokens_cache_write += *input_tokens_cache_write as i64;
                self.input_tokens_cache_read += *input_tokens_cache_read as i64;
                self.output_tokens += *output_tokens as i64;
                self.cost_cents += i64::from(*cost_cents);
            }
            Event::Checkpoint {
                input_tokens,
                output_tokens,
                cost_cents,
                ..
            } => {
                self.input_tokens_uncached += *input_tokens as i64;
                self.output_tokens += *output_tokens as i64;
                self.cost_cents += i64::from(*cost_cents);
                self.last_checkpoint_seq = Some(sequence_num as i64);
            }
            Event::GuardrailCheck {
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                cost_cents,
                ..
            } => {
                self.input_tokens_uncached += *input_tokens_uncached as i64;
                self.input_tokens_cache_write += *input_tokens_cache_write as i64;
                self.input_tokens_cache_read += *input_tokens_cache_read as i64;
                self.output_tokens += *output_tokens as i64;
                self.cost_cents += i64::from(*cost_cents);
            }
            _ => {}
        }
    }
}

fn record_append_phase(phase: SessionEventAppendPhase, started: Instant) {
    record_session_event_append_phase_duration(phase, started.elapsed());
}

impl PostgresSessionStore {
    /// Appends one event within a caller-owned transaction, returning its sequence.
    ///
    /// This is the single-event, transaction-scoped counterpart to
    /// [`PostgresSessionStore::append_events`]. It locks the session row, assigns
    /// the next sequence, encodes and inserts the event, honors an optional
    /// dedupe key, and folds the aggregate deltas — all inside the caller's
    /// transaction, so the append commits atomically with the caller's other
    /// writes (session creation or a channel-binding replacement) instead of in a
    /// separate commit. Blob offload runs before the row lock is taken. When
    /// `dedupe_key` matches a prior entry the existing sequence is returned
    /// without inserting a second event.
    pub async fn append_event_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        session_id: moa_core::types::identifiers::SessionId,
        event: Event,
        dedupe_key: Option<&str>,
    ) -> Result<u64> {
        let now = Utc::now();
        let payload = encode_event_for_storage(
            self.blob_store.as_ref(),
            &session_id,
            &event,
            self.blob_threshold_bytes,
        )
        .await?;
        let event_type = event.type_name().to_string();
        let hand_id = event_hand_id(&event);
        let token_count = event.token_count();

        let sessions = self.table_name("sessions");
        let events = self.table_name("events");
        let dedupe = self.table_name("session_event_dedupe");

        let locked = sqlx::query(&format!(
            "SELECT event_count, tenant_id, storage_partition_id, user_id, contact_id \
             FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_sqlx_error)?
        .ok_or(MoaError::SessionNotFound(session_id))?;
        let sequence_num = locked.col::<i64>("event_count")? as u64;
        let tenant_id = locked.col::<Uuid>("tenant_id")?;
        let storage_partition_id = locked.col::<String>("storage_partition_id")?;
        let actor_storage_key = locked.col::<String>("user_id")?;
        let session_contact_id = locked.col::<Option<Uuid>>("contact_id")?;

        if let Some(key) = dedupe_key
            && let Some(row) = sqlx::query(&format!(
                "SELECT sequence_num FROM {dedupe} WHERE session_id = $1 AND dedupe_key = $2"
            ))
            .bind(session_id.0)
            .bind(key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_sqlx_error)?
        {
            return Ok(row.col::<i64>("sequence_num")? as u64);
        }

        let payload_text = serde_json::to_string(&payload).map_err(|error| {
            MoaError::SerializationError(format!("failed to encode event payload: {error}"))
        })?;
        sqlx::query(&format!(
            "INSERT INTO {events} \
             (id, session_id, tenant_id, contact_id, storage_partition_id, user_id, \
              sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb, $10, NULL, $11, $12)"
        ))
        .bind(Uuid::now_v7())
        .bind(session_id.0)
        .bind(tenant_id)
        .bind(session_contact_id)
        .bind(&storage_partition_id)
        .bind(&actor_storage_key)
        .bind(sequence_num as i64)
        .bind(&event_type)
        .bind(&payload_text)
        .bind(now)
        .bind(hand_id)
        .bind(token_count as i32)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;

        if let Some(key) = dedupe_key {
            sqlx::query(&format!(
                "INSERT INTO {dedupe} (session_id, dedupe_key, sequence_num) VALUES ($1, $2, $3)"
            ))
            .bind(session_id.0)
            .bind(key)
            .bind(sequence_num as i64)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
        }

        let mut delta = SessionAggregateDelta::default();
        delta.add_event(&event, sequence_num);
        sqlx::query(&format!(
            "UPDATE {sessions} SET \
                 event_count = event_count + $2, \
                 turn_count = turn_count + $3, \
                 total_input_tokens_uncached = total_input_tokens_uncached + $4, \
                 total_input_tokens_cache_write = total_input_tokens_cache_write + $5, \
                 total_input_tokens_cache_read = total_input_tokens_cache_read + $6, \
                 total_output_tokens = total_output_tokens + $7, \
                 total_cost_cents = total_cost_cents + $8, \
                 last_checkpoint_seq = COALESCE($9, last_checkpoint_seq), \
                 updated_at = GREATEST(updated_at, $10) \
             WHERE id = $1"
        ))
        .bind(session_id.0)
        .bind(delta.event_count)
        .bind(delta.turn_count)
        .bind(delta.input_tokens_uncached)
        .bind(delta.input_tokens_cache_write)
        .bind(delta.input_tokens_cache_read)
        .bind(delta.output_tokens)
        .bind(delta.cost_cents)
        .bind(delta.last_checkpoint_seq)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;

        Ok(sequence_num)
    }

    /// Loads the currently active channel binding route for a session, when one exists.
    /// Returns whether a persisted tool event exists without decoding matching payloads.
    pub async fn tool_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        event_type: EventType,
        tool_call_id: ToolCallId,
    ) -> Result<bool> {
        let events = self.table_name("events");
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM {events} \
                 WHERE storage_partition_id = $1 \
                   AND event_type = $2 \
                   AND payload -> 'data' ? 'tool_id' \
                   AND payload -> 'data' ->> 'tool_id' = $3 \
                   AND session_id = $4\
             )"
        ))
        .bind(storage_partition_id.as_str())
        .bind(event_type.as_str())
        .bind(tool_call_id.to_string())
        .bind(session_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)
    }

    /// Loads the security metadata recorded on one durable `ToolResult` payload.
    ///
    /// Reads only the two closed-vocabulary security fields, never the output
    /// body, so a recovery path can rebuild an honest receipt without pulling
    /// tool bytes back out of storage.
    pub async fn tool_result_security_metadata(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        tool_call_id: ToolCallId,
    ) -> Result<Option<(ToolOutputAssessment, ToolCapabilityId)>> {
        let events = self.table_name("events");
        let row = sqlx::query_scalar::<_, serde_json::Value>(&format!(
            "SELECT jsonb_build_object(\
                 'assessment', payload -> 'data' -> 'assessment', \
                 'capability', payload -> 'data' -> 'capability'\
             ) \
             FROM {events} \
             WHERE storage_partition_id = $1 \
               AND event_type = $2 \
               AND payload -> 'data' ->> 'tool_id' = $3 \
               AND session_id = $4 \
             ORDER BY sequence_num DESC \
             LIMIT 1"
        ))
        .bind(storage_partition_id.as_str())
        .bind(EventType::ToolResult.as_str())
        .bind(tool_call_id.to_string())
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let Some(row) = row else {
            return Ok(None);
        };
        let assessment = row
            .get("assessment")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let capability = row
            .get("capability")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if assessment.is_null() || capability.is_null() {
            return Ok(None);
        }
        let assessment: ToolOutputAssessment =
            serde_json::from_value(assessment).map_err(|error| {
                MoaError::ValidationError(format!(
                    "durable tool result carries an undecodable assessment: {error}"
                ))
            })?;
        let capability: ToolCapabilityId = serde_json::from_value(capability).map_err(|error| {
            MoaError::ValidationError(format!(
                "durable tool result carries an undecodable capability: {error}"
            ))
        })?;
        Ok(Some((assessment, capability)))
    }

    /// Returns whether a persisted action-review event exists without decoding matching payloads.
    pub async fn action_review_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        event_type: EventType,
        review_id: Uuid,
    ) -> Result<bool> {
        let events = self.table_name("events");
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM {events} \
                 WHERE storage_partition_id = $1 \
                   AND event_type = $2 \
                   AND payload -> 'data' ? 'review_id' \
                   AND payload -> 'data' ->> 'review_id' = $3 \
                   AND session_id = $4\
             )"
        ))
        .bind(storage_partition_id.as_str())
        .bind(event_type.as_str())
        .bind(review_id.to_string())
        .bind(session_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)
    }

    /// Appends a batch of events to one session in a single transaction.
    ///
    /// All events are inserted under one `sessions ... FOR UPDATE` lock via a
    /// single multi-row INSERT and a single aggregate UPDATE. Large string fields
    /// are offloaded to the blob store *before* the transaction opens, so blob I/O
    /// never holds the session-row lock. Dedupe keys are honored per entry with
    /// the same semantics as the single append: an entry whose
    /// `(session_id, dedupe_key)` already exists — persisted earlier or repeated
    /// within this batch — is not re-inserted and its first persisted record is
    /// returned in place. Returns one [`EventRecord`] per input entry, in order.
    pub async fn append_events(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        appends: Vec<EventAppend>,
    ) -> Result<Vec<EventRecord>> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "failpoints")]
        if let Some(error) = crate::failpoints::hit("event_append_pre") {
            return Err(error);
        }

        // Offload blobs before opening the transaction (uses a second pooled
        // connection) so the append transaction only does index-friendly work.
        let now = Utc::now();
        let phase_started = Instant::now();
        let mut prepared = Vec::with_capacity(appends.len());
        for append in appends {
            let event = append.event;
            let payload = match encode_event_for_storage(
                self.blob_store.as_ref(),
                &session_id,
                &event,
                self.blob_threshold_bytes,
            )
            .await
            {
                Ok(payload) => payload,
                Err(error) => {
                    record_append_phase(SessionEventAppendPhase::Prepare, phase_started);
                    return Err(error);
                }
            };
            prepared.push(PreparedAppend {
                id: Uuid::now_v7(),
                event_type: event.type_name(),
                event_type_record: event.event_type(),
                hand_id: event_hand_id(&event),
                token_count: event.token_count(),
                payload,
                dedupe_key: append.dedupe_key,
                event,
            });
        }
        record_append_phase(SessionEventAppendPhase::Prepare, phase_started);

        let sessions = self.table_name("sessions");
        let events = self.table_name("events");
        let dedupe = self.table_name("session_event_dedupe");

        let phase_started = Instant::now();
        let mut connection = match self.pool.acquire().await {
            Ok(connection) => {
                record_append_phase(SessionEventAppendPhase::AcquireConnection, phase_started);
                connection
            }
            Err(error) => {
                record_append_phase(SessionEventAppendPhase::AcquireConnection, phase_started);
                return Err(map_sqlx_error(error));
            }
        };

        let phase_started = Instant::now();
        let mut transaction = match connection.begin().await {
            Ok(transaction) => {
                record_append_phase(SessionEventAppendPhase::BeginTransaction, phase_started);
                transaction
            }
            Err(error) => {
                record_append_phase(SessionEventAppendPhase::BeginTransaction, phase_started);
                return Err(map_sqlx_error(error));
            }
        };

        let phase_started = Instant::now();
        let locked_session = sqlx::query(&format!(
            "SELECT event_count, tenant_id, storage_partition_id, user_id, contact_id \
             FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *transaction)
        .await;
        record_append_phase(SessionEventAppendPhase::LockSession, phase_started);
        let locked_session = locked_session
            .map_err(map_sqlx_error)?
            .ok_or(MoaError::SessionNotFound(session_id))?;
        let base_sequence = locked_session.col::<i64>("event_count")? as u64;
        let tenant_id = locked_session.col::<Uuid>("tenant_id")?;
        let storage_partition_id = locked_session.col::<String>("storage_partition_id")?;
        let actor_storage_key = locked_session.col::<String>("user_id")?;
        let session_contact_id = locked_session.col::<Option<Uuid>>("contact_id")?;

        // Resolve prior-transaction dedupe hits with a single lookup.
        let lookup_keys: Vec<String> = prepared
            .iter()
            .filter_map(|entry| entry.dedupe_key.clone())
            .collect();
        let mut existing_by_key: HashMap<String, u64> = HashMap::new();
        if !lookup_keys.is_empty() {
            let phase_started = Instant::now();
            let rows = sqlx::query(&format!(
                "SELECT dedupe_key, sequence_num FROM {dedupe} \
                 WHERE session_id = $1 AND dedupe_key = ANY($2)"
            ))
            .bind(session_id.0)
            .bind(&lookup_keys)
            .fetch_all(&mut *transaction)
            .await;
            record_append_phase(SessionEventAppendPhase::DedupeLookup, phase_started);
            let rows = rows.map_err(map_sqlx_error)?;
            for row in &rows {
                existing_by_key.insert(
                    row.col::<String>("dedupe_key")?,
                    row.col::<i64>("sequence_num")? as u64,
                );
            }
        }

        // Plan each entry: fresh insert, prior-transaction hit, or in-batch dup.
        let mut plans = Vec::with_capacity(prepared.len());
        let mut insert_entries: Vec<(usize, u64)> = Vec::new();
        let mut in_batch_keys: HashMap<String, u64> = HashMap::new();
        let mut db_hit_seqs: Vec<u64> = Vec::new();
        let mut next_sequence = base_sequence;
        for (index, entry) in prepared.iter().enumerate() {
            if let Some(key) = entry.dedupe_key.as_deref() {
                if let Some(&sequence_num) = existing_by_key.get(key) {
                    db_hit_seqs.push(sequence_num);
                    plans.push(AppendPlan::DbHit { sequence_num });
                    continue;
                }
                if let Some(&sequence_num) = in_batch_keys.get(key) {
                    plans.push(AppendPlan::BatchDup { sequence_num });
                    continue;
                }
                in_batch_keys.insert(key.to_string(), next_sequence);
            }
            plans.push(AppendPlan::Insert {
                sequence_num: next_sequence,
            });
            insert_entries.push((index, next_sequence));
            next_sequence += 1;
        }

        // Fetch persisted records for prior-transaction dedupe hits so callers
        // receive the original event, not the retry payload.
        let mut db_records: HashMap<u64, EventRecord> = HashMap::new();
        if !db_hit_seqs.is_empty() {
            let seqs: Vec<i64> = db_hit_seqs.iter().map(|seq| *seq as i64).collect();
            let phase_started = Instant::now();
            let rows = sqlx::query(&format!(
                "SELECT id, session_id, sequence_num, event_type, payload, timestamp, brain_id, \
                        hand_id, token_count \
                 FROM {events} \
                 WHERE session_id = $1 AND sequence_num = ANY($2)"
            ))
            .bind(session_id.0)
            .bind(&seqs)
            .fetch_all(&mut *transaction)
            .await;
            record_append_phase(SessionEventAppendPhase::DedupeFetchRecords, phase_started);
            let rows = rows.map_err(map_sqlx_error)?;
            for row in &rows {
                let record = self.event_record_from_row(row).await?;
                db_records.insert(record.sequence_num, record);
            }
        }

        // Insert survivors with one multi-row statement and fold aggregate deltas.
        if !insert_entries.is_empty() {
            let phase_started = Instant::now();
            let count = insert_entries.len();
            let mut ids = Vec::with_capacity(count);
            let mut sequence_nums = Vec::with_capacity(count);
            let mut event_types = Vec::with_capacity(count);
            let mut payloads = Vec::with_capacity(count);
            let mut hand_ids: Vec<Option<String>> = Vec::with_capacity(count);
            let mut token_counts = Vec::with_capacity(count);
            let mut dedupe_rows: Vec<(String, i64)> = Vec::new();
            let mut delta = SessionAggregateDelta::default();
            for &(index, sequence_num) in &insert_entries {
                let entry = &prepared[index];
                ids.push(entry.id);
                sequence_nums.push(sequence_num as i64);
                event_types.push(entry.event_type.to_string());
                payloads.push(serde_json::to_string(&entry.payload).map_err(|error| {
                    MoaError::SerializationError(format!("failed to encode event payload: {error}"))
                })?);
                hand_ids.push(entry.hand_id.clone());
                token_counts.push(entry.token_count as i32);
                if let Some(key) = entry.dedupe_key.clone() {
                    dedupe_rows.push((key, sequence_num as i64));
                }
                delta.add_event(&entry.event, sequence_num);
            }
            record_append_phase(SessionEventAppendPhase::BuildInsertPayloads, phase_started);

            let phase_started = Instant::now();
            let insert_result = sqlx::query(&format!(
                "INSERT INTO {events} \
                 (id, session_id, tenant_id, contact_id, storage_partition_id, user_id, \
                  sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
                 SELECT u.id, $2, $3, $4, $5, $6, u.sequence_num, u.event_type, u.payload::jsonb, \
                        $7, NULL::uuid, u.hand_id, u.token_count \
                 FROM UNNEST($1::uuid[], $8::bigint[], $9::text[], $10::text[], $11::text[], $12::int[]) \
                      AS u(id, sequence_num, event_type, payload, hand_id, token_count)"
            ))
            .bind(&ids)
            .bind(session_id.0)
            .bind(tenant_id)
            .bind(session_contact_id)
            .bind(&storage_partition_id)
            .bind(&actor_storage_key)
            .bind(now)
            .bind(&sequence_nums)
            .bind(&event_types)
            .bind(&payloads)
            .bind(&hand_ids)
            .bind(&token_counts)
            .execute(&mut *transaction)
            .await;
            record_append_phase(SessionEventAppendPhase::InsertEvents, phase_started);
            insert_result.map_err(map_sqlx_error)?;

            if !dedupe_rows.is_empty() {
                let (keys, seqs): (Vec<String>, Vec<i64>) = dedupe_rows.into_iter().unzip();
                let phase_started = Instant::now();
                let insert_result = sqlx::query(&format!(
                    "INSERT INTO {dedupe} (session_id, dedupe_key, sequence_num) \
                     SELECT $1, u.dedupe_key, u.sequence_num \
                     FROM UNNEST($2::text[], $3::bigint[]) AS u(dedupe_key, sequence_num)"
                ))
                .bind(session_id.0)
                .bind(&keys)
                .bind(&seqs)
                .execute(&mut *transaction)
                .await;
                record_append_phase(SessionEventAppendPhase::InsertDedupeRows, phase_started);
                insert_result.map_err(map_sqlx_error)?;
            }

            // One aggregate UPDATE for the whole batch, replacing the retired
            // per-row AFTER INSERT trigger.
            let phase_started = Instant::now();
            let update_result = sqlx::query(&format!(
                "UPDATE {sessions} SET \
                     event_count = event_count + $2, \
                     turn_count = turn_count + $3, \
                     total_input_tokens_uncached = total_input_tokens_uncached + $4, \
                     total_input_tokens_cache_write = total_input_tokens_cache_write + $5, \
                     total_input_tokens_cache_read = total_input_tokens_cache_read + $6, \
                     total_output_tokens = total_output_tokens + $7, \
                     total_cost_cents = total_cost_cents + $8, \
                     last_checkpoint_seq = COALESCE($9, last_checkpoint_seq), \
                     updated_at = GREATEST(updated_at, $10) \
                 WHERE id = $1"
            ))
            .bind(session_id.0)
            .bind(delta.event_count)
            .bind(delta.turn_count)
            .bind(delta.input_tokens_uncached)
            .bind(delta.input_tokens_cache_write)
            .bind(delta.input_tokens_cache_read)
            .bind(delta.output_tokens)
            .bind(delta.cost_cents)
            .bind(delta.last_checkpoint_seq)
            .bind(now)
            .execute(&mut *transaction)
            .await;
            record_append_phase(
                SessionEventAppendPhase::UpdateSessionAggregates,
                phase_started,
            );
            update_result.map_err(map_sqlx_error)?;
        }

        let phase_started = Instant::now();
        let commit_result = transaction.commit().await;
        record_append_phase(SessionEventAppendPhase::Commit, phase_started);
        commit_result.map_err(map_sqlx_error)?;
        // Models an ack lost after commit: the row is durable but the caller
        // sees an error and will retry, exercising dedupe-key idempotency.
        #[cfg(feature = "failpoints")]
        if let Some(error) = crate::failpoints::hit("event_append_post_commit") {
            return Err(error);
        }

        // Build results in input order and record metrics for newly inserted events.
        let mut records = Vec::with_capacity(prepared.len());
        let mut batch_records: HashMap<u64, EventRecord> = HashMap::new();
        for (plan, entry) in plans.into_iter().zip(prepared) {
            match plan {
                AppendPlan::DbHit { sequence_num } => {
                    let record = db_records.get(&sequence_num).cloned().ok_or_else(|| {
                        MoaError::StorageError(format!(
                            "dedupe hit referenced missing persisted event at sequence {sequence_num}"
                        ))
                    })?;
                    records.push(record);
                }
                AppendPlan::BatchDup { sequence_num } => {
                    let record = batch_records.get(&sequence_num).cloned().ok_or_else(|| {
                        MoaError::StorageError(format!(
                            "batch dedupe referenced missing inserted event at sequence {sequence_num}"
                        ))
                    })?;
                    records.push(record);
                }
                AppendPlan::Insert { sequence_num } => {
                    let PreparedAppend {
                        id: entry_id,
                        event,
                        event_type,
                        event_type_record,
                        hand_id,
                        token_count,
                        ..
                    } = entry;
                    if let Event::BrainResponse {
                        model, model_tier, ..
                    } = &event
                    {
                        record_turn_completed(model, *model_tier);
                    }
                    record_session_event_append(event_type);

                    let record = EventRecord {
                        id: entry_id,
                        session_id,
                        sequence_num,
                        event_type: event_type_record,
                        event,
                        timestamp: now,
                        brain_id: None,
                        hand_id,
                        token_count: Some(token_count),
                    };
                    batch_records.insert(sequence_num, record.clone());
                    records.push(record);
                }
            }
        }
        Ok(records)
    }
}

#[async_trait]
impl SessionEventLookupStore for PostgresSessionStore {
    async fn tool_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        event_type: EventType,
        tool_call_id: ToolCallId,
    ) -> Result<bool> {
        PostgresSessionStore::tool_event_exists(
            self,
            storage_partition_id,
            session_id,
            event_type,
            tool_call_id,
        )
        .await
    }

    async fn tool_result_security_metadata(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        tool_call_id: ToolCallId,
    ) -> Result<Option<(ToolOutputAssessment, ToolCapabilityId)>> {
        PostgresSessionStore::tool_result_security_metadata(
            self,
            storage_partition_id,
            session_id,
            tool_call_id,
        )
        .await
    }

    async fn action_review_event_exists(
        &self,
        storage_partition_id: &StoragePartitionId,
        session_id: moa_core::types::identifiers::SessionId,
        event_type: EventType,
        review_id: Uuid,
    ) -> Result<bool> {
        PostgresSessionStore::action_review_event_exists(
            self,
            storage_partition_id,
            session_id,
            event_type,
            review_id,
        )
        .await
    }
}

#[cfg(test)]
mod aggregate_tests {
    use super::SessionAggregateDelta;
    use moa_core::events::Event;
    use moa_core::types::guardrails::{GuardrailDirection, GuardrailMode};
    use moa_core::types::identifiers::ModelId;
    use moa_core::types::provider::ModelTier;

    fn brain_response(cost_cents: u32) -> Event {
        Event::BrainResponse {
            text: "hi".to_string(),
            thought_signature: None,
            model: ModelId::new("gpt-5.4"),
            model_tier: ModelTier::Main,
            input_tokens_uncached: 100,
            input_tokens_cache_write: 10,
            input_tokens_cache_read: 5,
            output_tokens: 20,
            cost_cents,
            duration_ms: 1,
            llm_ttft_ms: None,
        }
    }

    fn guardrail_check(cost_cents: u32) -> Event {
        Event::GuardrailCheck {
            direction: GuardrailDirection::Input,
            mode: GuardrailMode::Enforce,
            passed: true,
            enforced: true,
            reason: None,
            model: Some(ModelId::new("guardrail-judge")),
            policy_hash: "hash".to_string(),
            input_tokens_uncached: 40,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 3,
            cost_cents,
            duration_ms: 1,
        }
    }

    #[test]
    fn guardrail_check_adds_cost_and_tokens_but_not_a_turn() {
        // Pins: GuardrailCheck is auxiliary spend — its cost and tokens fold
        // into the session aggregate (so total_cost_cents captures every billed
        // model call) but it does not count as a visible turn.
        let mut delta = SessionAggregateDelta::default();
        delta.add_event(&brain_response(7), 0);
        delta.add_event(&guardrail_check(2), 1);

        assert_eq!(delta.turn_count, 1, "only BrainResponse is a turn");
        assert_eq!(delta.cost_cents, 9, "7c response + 2c guardrail");
        assert_eq!(
            delta.input_tokens_uncached, 140,
            "100 response + 40 guardrail uncached input tokens"
        );
        assert_eq!(delta.output_tokens, 23, "20 response + 3 guardrail output");
    }

    #[test]
    fn guardrail_check_alone_records_cost_without_a_turn() {
        // Pins: a turn that is fully blocked at the input guardrail still bills
        // its judge cost even though no BrainResponse is emitted.
        let mut delta = SessionAggregateDelta::default();
        delta.add_event(&guardrail_check(5), 0);

        assert_eq!(delta.turn_count, 0);
        assert_eq!(delta.cost_cents, 5);
    }
}

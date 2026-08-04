//! Terminal-session event archival, verification, hydration, and purge.
//!
//! Retention here means one thing: a terminal session's history is copied into
//! one immutable, digest-verified archive row and the live rows are deleted in
//! the same transaction. Nothing is ever deleted unless Postgres returns the
//! exact bytes written and their digest matches both the returned digest and
//! the expected digest derived from the source events.
//!
//! The delete is always keyed by `session_id`. `events` is HASH-partitioned on
//! that column, so a whole-session delete prunes to one of sixteen partitions;
//! a retention pass expressed as a timestamp range would touch all sixteen and
//! cost more than leaving the data in place. The retention boundary decides
//! which sessions are eligible, never which rows are deleted.

use super::*;

use crate::archive::{
    ArchiveBody, ArchiveOutcome, ArchiveRefusal, ArchivedEvent, SESSION_ARCHIVE_DIGEST_LEN,
    SESSION_ARCHIVE_FORMAT_VERSION, SessionEventArchive, apply_archive_range, archive_digest,
    is_terminal_status,
};

/// Columns selected from `session_event_archives` when reading an archive row.
const ARCHIVE_COLUMNS: &str = "session_id, tenant_id, format_version, event_count, \
     first_sequence_num, last_sequence_num, payload, content_digest, archived_at";

/// Session state read under the row lock before any archival decision.
struct LockedSession {
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    status: String,
    terminal_at: DateTime<Utc>,
    events_archived_at: Option<DateTime<Utc>>,
}

impl PostgresSessionStore {
    /// Lists terminal sessions eligible for archival, oldest first.
    ///
    /// `boundary` is the retention edge supplied by the caller: a session is a
    /// candidate only when it reached its terminal state at or before it. The
    /// boundary is never derived from the wall clock inside this crate, so a
    /// retention pass is reproducible and testable against a controlled time.
    ///
    /// This is a scan, not a decision. Every condition it filters on is
    /// re-checked under the session row lock in
    /// [`PostgresSessionStore::archive_terminal_session`], because a session can
    /// resume, or a legal hold can land, between the scan and the archive.
    pub async fn list_session_archival_candidates(
        &self,
        tenant_id: TenantId,
        boundary: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<moa_core::types::identifiers::SessionId>> {
        let sessions = self.table_name("sessions");
        let rows = sqlx::query(&format!(
            "SELECT id FROM {sessions} \
             WHERE tenant_id = $1 \
               AND events_archived_at IS NULL \
               AND status IN ('completed', 'cancelled', 'failed') \
               AND COALESCE(completed_at, updated_at) <= $2 \
               AND event_count > 0 \
             ORDER BY COALESCE(completed_at, updated_at) ASC, id ASC \
             LIMIT $3"
        ))
        .bind(tenant_id.0)
        .bind(boundary)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(moa_core::types::identifiers::SessionId(
                    row.col::<Uuid>("id")?,
                ))
            })
            .collect()
    }

    /// Archives one terminal session's history and deletes the live rows.
    ///
    /// Refusals are returned, not raised: a session that is still running, still
    /// inside the retention boundary, under legal hold, or already owned by a
    /// durable erasure is reported and left exactly as it was. Only a storage
    /// fault or a failed integrity check is an error, and a failed integrity
    /// check aborts the transaction before anything is deleted.
    ///
    /// `now` is stamped on the archive row and on `sessions.events_archived_at`.
    pub async fn archive_terminal_session(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        boundary: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<ArchiveOutcome> {
        let sessions = self.table_name("sessions");
        let events = self.table_name("events");
        let archives = self.table_name("session_event_archives");

        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;

        let locked = sqlx::query(&format!(
            "SELECT tenant_id, contact_id, status, \
                    COALESCE(completed_at, updated_at) AS terminal_at, events_archived_at \
             FROM {sessions} WHERE id = $1 FOR UPDATE"
        ))
        .bind(session_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?
        .ok_or(MoaError::SessionNotFound(session_id))?;
        let locked = LockedSession {
            tenant_id: locked.col::<Uuid>("tenant_id")?,
            contact_id: locked.col::<Option<Uuid>>("contact_id")?,
            status: locked.col::<String>("status")?,
            terminal_at: locked.col::<DateTime<Utc>>("terminal_at")?,
            events_archived_at: locked.col::<Option<DateTime<Utc>>>("events_archived_at")?,
        };

        if locked.events_archived_at.is_some() {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::AlreadyArchived);
        }
        let status: SessionStatus = from_db("session status", &locked.status)?;
        if !is_terminal_status(&status) {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::NotTerminal {
                status: locked.status,
            }));
        }
        if locked.terminal_at > boundary {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::WithinRetention {
                boundary,
                terminal_at: locked.terminal_at,
            }));
        }

        // Serializes this transaction against `place_hold` and `start_destruction`,
        // which take the same advisory key before they check for in-flight
        // destruction. Without it a hold placed concurrently could commit after
        // the check below and before the delete, and the rows it was meant to
        // preserve would already be gone.
        sqlx::query(
            "SELECT pg_advisory_xact_lock_shared(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
        )
        .bind(locked.tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.legal_hold \
             WHERE tenant_id = $1 AND released_at IS NULL \
               AND (subject_id IS NULL OR subject_id = $2))",
        )
        .bind(locked.tenant_id)
        .bind(locked.contact_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        if held {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::LegalHold));
        }

        let fenced: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.destruction_operation_fence \
             WHERE tenant_id = $1 AND (subject_id IS NULL OR subject_id = $2))",
        )
        .bind(locked.tenant_id)
        .bind(locked.contact_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        if fenced {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::DestructionInFlight));
        }

        let rows = sqlx::query(&format!(
            "SELECT {EVENT_COLUMNS} FROM {events} WHERE session_id = $1 ORDER BY sequence_num ASC"
        ))
        .bind(session_id.0)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        if rows.is_empty() {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::NoEvents));
        }

        let mut archived_events = Vec::with_capacity(rows.len());
        for row in &rows {
            archived_events.push(ArchivedEvent {
                id: row.col::<Uuid>("id")?,
                sequence_num: row.col::<i64>("sequence_num")?,
                event_type: row.col::<String>("event_type")?,
                payload: row.col::<serde_json::Value>("payload")?,
                timestamp: row.col::<DateTime<Utc>>("timestamp")?,
                brain_id: row.col::<Option<Uuid>>("brain_id")?,
                hand_id: row.col::<Option<String>>("hand_id")?,
                token_count: row.col::<Option<i32>>("token_count")?,
            });
        }
        let event_count = archived_events.len() as i64;
        let first_sequence_num = archived_events[0].sequence_num;
        let last_sequence_num = archived_events[archived_events.len() - 1].sequence_num;
        let bytes = ArchiveBody {
            format_version: SESSION_ARCHIVE_FORMAT_VERSION,
            session_id: session_id.0,
            events: archived_events,
        }
        .to_bytes()?;
        let digest = archive_digest(&bytes);

        let stored = sqlx::query(&format!(
            "INSERT INTO {archives} \
             (session_id, tenant_id, format_version, event_count, first_sequence_num, \
              last_sequence_num, payload, content_digest, archived_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             RETURNING {ARCHIVE_COLUMNS}"
        ))
        .bind(session_id.0)
        .bind(locked.tenant_id)
        .bind(SESSION_ARCHIVE_FORMAT_VERSION)
        .bind(event_count)
        .bind(first_sequence_num)
        .bind(last_sequence_num)
        .bind(&bytes)
        .bind(digest.as_slice())
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Prove the row returned by Postgres before deleting live history. A
        // mismatch aborts the transaction, so an untrusted archive never
        // replaces the source events.
        let stored_bytes = stored.col::<Vec<u8>>("payload")?;
        let archive = Self::archive_metadata_from_row(&stored, stored_bytes.len() as i64)?;
        let stored_digest = archive_digest(&stored_bytes);
        if stored_digest != archive.content_digest || stored_digest != digest {
            return Err(MoaError::StorageError(format!(
                "session {session_id} archive digest mismatch on read-back: stored row digest {}, recomputed {}, expected {}",
                hex::encode(archive.content_digest),
                hex::encode(stored_digest),
                hex::encode(digest)
            )));
        }

        // `events` refuses UPDATE and DELETE through `events_append_only_guard`
        // unless a maintenance session opts in. Archival is the one path that
        // opts in, transaction-locally, after the archive above has been proven.
        // Active history is never touched by this and the guard stays in force
        // for every other writer.
        sqlx::query("SELECT set_config('moa.events_maintenance', 'on', true)")
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        let deleted = sqlx::query(&format!("DELETE FROM {events} WHERE session_id = $1"))
            .bind(session_id.0)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();
        if deleted != event_count as u64 {
            return Err(MoaError::StorageError(format!(
                "session {session_id} retention deleted {deleted} events but archived {event_count}"
            )));
        }

        sqlx::query(&format!(
            "UPDATE {sessions} SET events_archived_at = $2 WHERE id = $1"
        ))
        .bind(session_id.0)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Models a crash between the delete and the commit: the rows are gone
        // inside this transaction but nothing is durable yet. Everything above
        // must roll back together, because a session whose events were deleted
        // without its archive becoming durable is history that no longer exists.
        #[cfg(feature = "failpoints")]
        if let Some(error) = crate::failpoints::hit("session_archive_post_delete") {
            return Err(error);
        }

        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(ArchiveOutcome::Archived(Box::new(archive)))
    }

    /// Verifies a committed archive against its stored digest.
    ///
    /// Returns `None` when the session has no archive. Re-derives the digest
    /// from the bytes the database currently holds and decodes the body, so a
    /// corrupted or truncated archive is an error rather than a shorter history.
    pub async fn verify_session_archive(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Option<SessionEventArchive>> {
        let Some((archive, _)) = self.load_verified_archive(session_id).await? else {
            return Ok(None);
        };
        Ok(Some(archive))
    }

    /// Rebuilds an archived session's history as live replay would have seen it.
    ///
    /// Returns `None` when the session has no archive, so callers can fall
    /// through to whatever the live tables hold. Claim-check references resolve
    /// through the same blob store the live read path uses, because retention
    /// never touches `session_blobs`.
    ///
    /// Replay metrics are recorded here rather than by the caller: archived
    /// history is still replayed history, and a counter that silently stops
    /// moving once data is archived would make retention look like a drop in
    /// replay traffic instead of a change of storage.
    pub async fn hydrate_archived_events(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        range: &EventRange,
    ) -> Result<Option<Vec<EventRecord>>> {
        let started_at = std::time::Instant::now();
        let Some((archive, body)) = self.load_verified_archive(session_id).await? else {
            return Ok(None);
        };
        let archived_events = apply_archive_range(body.events, range);
        let mut blob_ids: Vec<String> = Vec::new();
        let mut seen_blob_ids = std::collections::HashSet::new();
        for event in &archived_events {
            let mut ids = Vec::new();
            crate::blob::collect_claim_check_blob_ids(&event.payload, &mut ids)?;
            for id in ids {
                if seen_blob_ids.insert(id.clone()) {
                    blob_ids.push(id);
                }
            }
        }
        let blob_cache = self.blob_store.get_many(&session_id, &blob_ids).await?;
        let mut records = Vec::with_capacity(archived_events.len());
        for archived in archived_events {
            let event = crate::blob::decode_event_from_cache(archived.payload, &blob_cache)?;
            records.push(EventRecord {
                id: archived.id,
                session_id,
                sequence_num: archived.sequence_num as u64,
                event_type: from_db("event type", &archived.event_type)?,
                event,
                timestamp: archived.timestamp,
                brain_id: archived.brain_id.map(moa_core::types::identifiers::BrainId),
                hand_id: archived.hand_id,
                token_count: archived.token_count.map(|count| count as usize),
            });
        }
        record_session_event_replay(
            records.len(),
            archive.payload_bytes.max(0) as u64,
            started_at.elapsed(),
        );
        Ok(Some(records))
    }

    /// Loads an archive row and proves it against its stored digest.
    async fn load_verified_archive(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Result<Option<(SessionEventArchive, ArchiveBody)>> {
        let archives = self.table_name("session_event_archives");
        let Some(row) = sqlx::query(&format!(
            "SELECT {ARCHIVE_COLUMNS} FROM {archives} WHERE session_id = $1"
        ))
        .bind(session_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        else {
            return Ok(None);
        };
        let bytes = row.col::<Vec<u8>>("payload")?;
        let archive = Self::archive_metadata_from_row(&row, bytes.len() as i64)?;
        let recomputed = archive_digest(&bytes);
        if recomputed != archive.content_digest {
            return Err(MoaError::StorageError(format!(
                "session {session_id} archive is corrupt: stored digest {}, recomputed {}",
                hex::encode(archive.content_digest),
                hex::encode(recomputed)
            )));
        }
        let body = ArchiveBody::from_bytes(&bytes)?;
        if body.session_id != session_id.0 {
            return Err(MoaError::StorageError(format!(
                "session {session_id} archive holds history for session {}",
                body.session_id
            )));
        }
        if body.events.len() as i64 != archive.event_count {
            return Err(MoaError::StorageError(format!(
                "session {session_id} archive claims {} events but holds {}",
                archive.event_count,
                body.events.len()
            )));
        }
        Ok(Some((archive, body)))
    }

    /// Builds archive metadata from a `session_event_archives` row.
    fn archive_metadata_from_row(
        row: &sqlx::postgres::PgRow,
        payload_bytes: i64,
    ) -> Result<SessionEventArchive> {
        let digest_bytes = row.col::<Vec<u8>>("content_digest")?;
        let content_digest: [u8; SESSION_ARCHIVE_DIGEST_LEN] =
            digest_bytes.as_slice().try_into().map_err(|_| {
                MoaError::StorageError(format!(
                    "session archive digest has {} bytes, expected {SESSION_ARCHIVE_DIGEST_LEN}",
                    digest_bytes.len()
                ))
            })?;
        Ok(SessionEventArchive {
            session_id: moa_core::types::identifiers::SessionId(row.col::<Uuid>("session_id")?),
            tenant_id: TenantId::from(row.col::<Uuid>("tenant_id")?),
            format_version: row.col::<i32>("format_version")?,
            event_count: row.col::<i64>("event_count")?,
            first_sequence_num: row.col::<i64>("first_sequence_num")?,
            last_sequence_num: row.col::<i64>("last_sequence_num")?,
            payload_bytes,
            content_digest,
            archived_at: row.col::<DateTime<Utc>>("archived_at")?,
        })
    }
}

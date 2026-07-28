//! Integration coverage for terminal-session archival and retention.
//!
//! Retention deletes conversation history. Every test here is written on the
//! assumption that a bug in this path is unrecoverable, so the survival cases
//! are pinned as hard as the deletion case, and the retention boundary is
//! always evaluated against a timestamp read back from the database rather than
//! the test process's wall clock.

use crate::shared::qualified;

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    events::Event,
    traits::SessionStore,
    types::contact::SessionActorRef,
    types::events_stream::{EventRange, EventRecord},
    types::identifiers::{ModelId, SessionId, TenantId},
    types::session::{SessionMeta, SessionStatus},
};
use moa_session::archive::{ArchiveOutcome, ArchiveRefusal};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use uuid::Uuid;

async fn test_db() -> TestDb {
    bootstrap_test_db().await.expect(
        "bootstrap Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    )
}

/// Creates one session owned by a per-test tenant so the lane stays parallel-safe.
async fn new_session(test_db: &TestDb, tenant_id: TenantId) -> SessionId {
    test_db
        .store()
        .create_session(SessionMeta {
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create retention test session")
}

/// Emits a short history including one payload large enough to be claim-checked.
async fn seed_history(test_db: &TestDb, session_id: SessionId) {
    for text in ["first turn", "second turn"] {
        test_db
            .store()
            .emit_event(
                session_id,
                Event::UserMessage {
                    text: text.to_string(),
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("emit retention test event");
    }
    // Above the 64 KiB claim-check threshold, so this event's payload is stored
    // as a blob reference. The archive copies the reference, never the bytes,
    // and hydration has to resolve it exactly as live replay does.
    test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "x".repeat(100_000),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit claim-checked retention test event");
}

/// Marks a session terminal and returns the `completed_at` the database stamped.
///
/// The retention boundary in every test is derived from this value, so the
/// decision under test is evaluated against database time and never against the
/// test process's own clock.
async fn complete_session(
    test_db: &TestDb,
    session_id: SessionId,
    status: SessionStatus,
) -> DateTime<Utc> {
    test_db
        .store()
        .update_status(session_id, status)
        .await
        .expect("update retention test session status");
    let sessions = qualified(test_db.schema_name(), "sessions");
    sqlx::query_scalar::<_, DateTime<Utc>>(&format!(
        "SELECT COALESCE(completed_at, updated_at) FROM {sessions} WHERE id = $1"
    ))
    .bind(session_id.0)
    .fetch_one(test_db.store().pool())
    .await
    .expect("read terminal timestamp stamped by the database")
}

/// Counts live `events` rows for one session.
async fn live_event_count(test_db: &TestDb, session_id: SessionId) -> i64 {
    let events = qualified(test_db.schema_name(), "events");
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT count(*) FROM {events} WHERE session_id = $1"
    ))
    .bind(session_id.0)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count live session events")
}

fn texts(records: &[EventRecord]) -> Vec<String> {
    records
        .iter()
        .map(|record| match &record.event {
            Event::UserMessage { text, .. } => format!("{}:{}", record.sequence_num, text.len()),
            other => format!("{}:{}", record.sequence_num, other.type_name()),
        })
        .collect()
}

// Pins: an archived terminal session reproduces its visible history exactly.
// The comparison is the whole `EventRecord` vector, including ids, sequence
// numbers, timestamps, and a claim-checked payload resolved through the blob
// store, because "roughly the same history" is indistinguishable from data loss
// once the live rows are gone.
#[tokio::test]
async fn archived_terminal_session_hydrates_its_history_unchanged_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;

    let before = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read history before archival");
    assert_eq!(
        before.len(),
        3,
        "expected the seeded history, observed {:?}",
        texts(&before)
    );

    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");
    let ArchiveOutcome::Archived(archive) = outcome else {
        panic!("expected the session to be archived, observed {outcome:?}");
    };
    assert_eq!(
        archive.event_count,
        before.len() as i64,
        "archive must cover every event, observed {archive:?}"
    );
    assert_eq!(
        (archive.first_sequence_num, archive.last_sequence_num),
        (0, before.len() as i64 - 1),
        "archive must record the full sequence span, observed {archive:?}"
    );

    assert_eq!(
        live_event_count(&test_db, session_id).await,
        0,
        "archived history must no longer occupy the live events table"
    );

    let after = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read history after archival");
    assert_eq!(
        after,
        before,
        "hydrated history must equal the live history it replaced; observed {:?} against {:?}",
        texts(&after),
        texts(&before)
    );

    let verified = test_db
        .store()
        .verify_session_archive(session_id)
        .await
        .expect("verify committed archive")
        .expect("archive row must exist after archival");
    assert_eq!(
        verified.content_digest, archive.content_digest,
        "committed archive digest must match the digest proven at write time"
    );
}

// Pins: a range read of archived history applies the same filters and limits the
// live query would have. A replay that asks for the last two turns of an
// archived session must not receive the first two.
#[tokio::test]
async fn archived_history_honours_range_filters_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;

    let live_recent = test_db
        .store()
        .get_events(session_id, EventRange::recent(2))
        .await
        .expect("read recent history before archival");
    let live_bounded = test_db
        .store()
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(1),
                to_seq: Some(1),
                event_types: None,
                limit: None,
            },
        )
        .await
        .expect("read bounded history before archival");

    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");
    assert!(
        matches!(outcome, ArchiveOutcome::Archived(_)),
        "expected the session to be archived, observed {outcome:?}"
    );

    let archived_recent = test_db
        .store()
        .get_events(session_id, EventRange::recent(2))
        .await
        .expect("read recent history after archival");
    assert_eq!(
        archived_recent,
        live_recent,
        "a bare limit must return the same events before and after archival; observed {:?} against {:?}",
        texts(&archived_recent),
        texts(&live_recent)
    );

    let archived_bounded = test_db
        .store()
        .get_events(
            session_id,
            EventRange {
                from_seq: Some(1),
                to_seq: Some(1),
                event_types: None,
                limit: None,
            },
        )
        .await
        .expect("read bounded history after archival");
    assert_eq!(
        archived_bounded,
        live_bounded,
        "a sequence-bounded read must return the same events before and after archival; observed {:?} against {:?}",
        texts(&archived_bounded),
        texts(&live_bounded)
    );
}

// Pins: history inside the retention boundary SURVIVES. A session that ended
// after the boundary is refused and its live rows are untouched. This is the
// failure that would delete data a tenant is still entitled to.
#[tokio::test]
async fn history_inside_the_retention_boundary_survives_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;

    let boundary = terminal_at - Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("evaluate retention for an in-boundary session");
    match outcome {
        ArchiveOutcome::Refused(ArchiveRefusal::WithinRetention {
            boundary: refused_boundary,
            terminal_at: refused_terminal_at,
        }) => {
            assert_eq!(
                (refused_boundary, refused_terminal_at),
                (boundary, terminal_at),
                "refusal must report the boundary it was evaluated against"
            );
        }
        other => panic!("expected a retention-boundary refusal, observed {other:?}"),
    }
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        3,
        "a session inside the retention boundary must keep every live event row"
    );
    assert!(
        test_db
            .store()
            .verify_session_archive(session_id)
            .await
            .expect("look up archive for an in-boundary session")
            .is_none(),
        "a refused session must not have an archive row"
    );
}

// Pins: a session that can still append is never archived, even when it is old
// enough. Archiving one would capture a prefix and delete rows the session is
// still writing.
#[tokio::test]
async fn a_session_that_can_still_append_is_never_archived_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    test_db
        .store()
        .update_status(session_id, SessionStatus::Running)
        .await
        .expect("mark the retention test session running");

    let far_future = Utc::now() + Duration::days(3650);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, far_future, far_future)
        .await
        .expect("evaluate retention for a running session");
    match outcome {
        ArchiveOutcome::Refused(ArchiveRefusal::NotTerminal { status }) => {
            assert_eq!(
                status, "running",
                "refusal must report the status observed under the row lock"
            );
        }
        other => panic!("expected a non-terminal refusal, observed {other:?}"),
    }
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        3,
        "a running session must keep every live event row"
    );
}

// Pins: an active legal hold blocks archival even for a session that is
// otherwise fully eligible, and the check is enforced inside the archival
// transaction rather than by whatever called it.
#[tokio::test]
async fn an_active_legal_hold_blocks_archival_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;

    sqlx::query(
        "INSERT INTO moa.legal_hold (tenant_id, subject_id, reason, placed_by) \
         VALUES ($1, NULL, 'retention lane hold', 'retention-test')",
    )
    .bind(tenant_id.0)
    .execute(test_db.store().pool())
    .await
    .expect("place a tenant-wide legal hold");

    let boundary = terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("evaluate retention under a legal hold");
    assert!(
        matches!(outcome, ArchiveOutcome::Refused(ArchiveRefusal::LegalHold)),
        "expected a legal-hold refusal, observed {outcome:?}"
    );
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        3,
        "a held session must keep every live event row"
    );

    // Releasing the hold makes exactly the same call succeed, so the refusal
    // above is attributable to the hold and not to anything else about this
    // session.
    sqlx::query(
        "UPDATE moa.legal_hold SET released_at = NOW(), released_by = 'retention-test' \
         WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .execute(test_db.store().pool())
    .await
    .expect("release the tenant-wide legal hold");
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive after the hold is released");
    assert!(
        matches!(outcome, ArchiveOutcome::Archived(_)),
        "expected archival once the hold is released, observed {outcome:?}"
    );
}

// Pins: a durable erasure or tenant purge already owns these rows, so retention
// must not race it. The fence row is the same one the privacy paths commit
// before their first destructive stage.
#[tokio::test]
async fn an_in_flight_destruction_blocks_archival_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;

    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
             (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, NULL, 'retention-lane-op', 'tenant_purge')",
    )
    .bind(tenant_id.0)
    .execute(test_db.store().pool())
    .await
    .expect("commit a tenant-wide destruction fence");

    let boundary = terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("evaluate retention against a destruction fence");
    assert!(
        matches!(
            outcome,
            ArchiveOutcome::Refused(ArchiveRefusal::DestructionInFlight)
        ),
        "expected a destruction-in-flight refusal, observed {outcome:?}"
    );
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        3,
        "a fenced session must keep every live event row"
    );
}

// Pins: retention is idempotent and closes the session's history for good. A
// second pass reports the session as already archived without writing anything,
// and an append afterwards is refused — an accepted append would write rows for
// a session whose live history is empty and permanently hide the archive from
// the read path.
#[tokio::test]
async fn a_second_retention_pass_is_a_no_op_and_appends_stay_refused_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);

    let first = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");
    assert!(
        matches!(first, ArchiveOutcome::Archived(_)),
        "expected the first pass to archive, observed {first:?}"
    );
    let second = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("re-run retention for an archived session");
    assert!(
        matches!(second, ArchiveOutcome::AlreadyArchived),
        "expected the second pass to be a no-op, observed {second:?}"
    );

    let error = test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "after the archive".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect_err("an append to an archived session must be refused");
    assert!(
        error.to_string().contains("appends are refused"),
        "refusal must name the archived history as the cause, observed: {error}"
    );
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        0,
        "a refused append must not resurrect rows for an archived session"
    );
}

// Pins: the candidate scan selects on the same conditions the archival decision
// enforces. A scan that offered running, recent, or already-archived sessions
// would send every retention pass at work it must then refuse.
#[tokio::test]
async fn the_candidate_scan_offers_only_eligible_sessions_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());

    let eligible = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, eligible).await;
    let eligible_terminal_at = complete_session(&test_db, eligible, SessionStatus::Failed).await;

    let running = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, running).await;
    test_db
        .store()
        .update_status(running, SessionStatus::Running)
        .await
        .expect("mark the running candidate running");

    let archived = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, archived).await;
    let archived_terminal_at = complete_session(&test_db, archived, SessionStatus::Cancelled).await;
    let archive_boundary = archived_terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(archived, archive_boundary, archive_boundary)
        .await
        .expect("archive the already-archived candidate");
    assert!(
        matches!(outcome, ArchiveOutcome::Archived(_)),
        "expected the third session to be archived, observed {outcome:?}"
    );

    let boundary = eligible_terminal_at.max(archived_terminal_at) + Duration::seconds(1);
    let candidates = test_db
        .store()
        .list_session_archival_candidates(tenant_id, boundary, 10)
        .await
        .expect("scan retention candidates");
    assert_eq!(
        candidates,
        vec![eligible],
        "only the terminal, unarchived, past-boundary session may be offered; observed {candidates:?}"
    );

    let tight_boundary = eligible_terminal_at - Duration::seconds(1);
    let none = test_db
        .store()
        .list_session_archival_candidates(tenant_id, tight_boundary, 10)
        .await
        .expect("scan retention candidates inside the boundary");
    assert!(
        none.is_empty(),
        "no session may be offered before the retention boundary, observed {none:?}"
    );
}

// Pins: the archive is the copy that replaced live history, so it cannot be
// rewritten in place afterwards.
#[tokio::test]
async fn an_archive_row_refuses_rewrite_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");

    let archives = qualified(test_db.schema_name(), "session_event_archives");
    let error = sqlx::query(&format!(
        "UPDATE {archives} SET payload = '\\x00'::BYTEA WHERE session_id = $1"
    ))
    .bind(session_id.0)
    .execute(test_db.store().pool())
    .await
    .expect_err("rewriting an archive must be refused");
    assert!(
        error
            .to_string()
            .contains("session event archive is immutable"),
        "refusal must come from the archive immutability guard, observed: {error}"
    );
}

// Pins: a corrupted archive is an error, never a shorter history. If the bytes
// the database holds no longer match their digest, replay must fail loudly
// rather than hand a caller a truncated conversation it cannot tell apart from
// a real one.
#[tokio::test]
async fn a_corrupted_archive_is_refused_rather_than_served_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");

    // The corruption is deliberately the hardest kind to notice: one byte
    // inside a message, so the body still decodes, still holds three events,
    // and still spans the same sequence numbers. Every structural check passes.
    // The digest is the only thing standing between this and a silently wrong
    // conversation being replayed as authentic history.
    let archives = qualified(test_db.schema_name(), "session_event_archives");
    let mut tx = test_db
        .store()
        .pool()
        .begin()
        .await
        .expect("begin archive corruption transaction");
    let (digest, payload): (Vec<u8>, Vec<u8>) = sqlx::query_as(&format!(
        "SELECT content_digest, payload FROM {archives} WHERE session_id = $1"
    ))
    .bind(session_id.0)
    .fetch_one(&mut *tx)
    .await
    .expect("read the intact archive row");
    let needle = b"first turn";
    let offset = payload
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("archived payload must contain the seeded message text");
    let mut corrupted = payload.clone();
    corrupted[offset + 6] = b'v';
    assert_ne!(
        corrupted, payload,
        "the corruption must actually change the stored bytes"
    );
    sqlx::query(&format!("DELETE FROM {archives} WHERE session_id = $1"))
        .bind(session_id.0)
        .execute(&mut *tx)
        .await
        .expect("remove the intact archive row");
    sqlx::query(&format!(
        "INSERT INTO {archives} \
             (session_id, tenant_id, format_version, event_count, first_sequence_num, \
              last_sequence_num, payload, content_digest, archived_at) \
         VALUES ($1, $2, 1, 3, 0, 2, $3, $4, NOW())"
    ))
    .bind(session_id.0)
    .bind(tenant_id.0)
    .bind(&corrupted)
    .bind(&digest)
    .execute(&mut *tx)
    .await
    .expect("insert the corrupted archive row");
    tx.commit().await.expect("commit archive corruption");

    let error = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect_err("a corrupted archive must not be served as history");
    assert!(
        error.to_string().contains("archive is corrupt"),
        "replay must fail on the digest mismatch, observed: {error}"
    );
}

// Pins: the transaction-scoped append refuses an archived session too. This is
// a SECOND call site for the same guard — `emit_event` never reaches it — so
// deleting the check here is invisible to every other test in this file. The
// assertion up front that the batch path is not involved keeps it that way.
#[tokio::test]
async fn the_transaction_scoped_append_refuses_an_archived_session_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");

    let mut tx = test_db
        .store()
        .pool()
        .begin()
        .await
        .expect("begin caller-owned append transaction");
    let error = test_db
        .store()
        .append_event_in_tx(
            &mut tx,
            session_id,
            Event::UserMessage {
                text: "appended inside a caller transaction".to_string(),
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .expect_err("a transaction-scoped append to an archived session must be refused");
    assert!(
        error.to_string().contains("appends are refused"),
        "refusal must name the archived history as the cause, observed: {error}"
    );
    tx.rollback().await.expect("roll back the refused append");
    assert_eq!(
        live_event_count(&test_db, session_id).await,
        0,
        "a refused transaction-scoped append must not resurrect rows"
    );
}

// Pins: the archive's foreign key to `sessions` does not cascade. The tenant
// purge catalog carries an explicit delete for the archive, and this is what
// makes that step falsifiable: without the archive delete, the tenant's session
// delete fails outright instead of quietly leaving a purged tenant's
// conversation history in the archive.
#[tokio::test]
async fn deleting_a_session_that_still_has_an_archive_is_refused_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archive terminal session");

    let sessions = qualified(test_db.schema_name(), "sessions");
    let error = sqlx::query(&format!("DELETE FROM {sessions} WHERE id = $1"))
        .bind(session_id.0)
        .execute(test_db.store().pool())
        .await
        .expect_err("deleting a session with a live archive must be refused");
    assert!(
        error.to_string().contains("session_event_archives"),
        "the refusal must come from the archive foreign key, observed: {error}"
    );

    let archives = qualified(test_db.schema_name(), "session_event_archives");
    sqlx::query(&format!("DELETE FROM {archives} WHERE session_id = $1"))
        .bind(session_id.0)
        .execute(test_db.store().pool())
        .await
        .expect("remove the archive as tenant purge does");
    sqlx::query(&format!("DELETE FROM {sessions} WHERE id = $1"))
        .bind(session_id.0)
        .execute(test_db.store().pool())
        .await
        .expect("the session deletes once its archive is gone");
}

// Pins: the `moa.events_maintenance` opt-in does not outlive the archival
// transaction that set it. Archival is the first user of that escape hatch, and
// the first user of an escape hatch is also the person who could quietly leave
// it open for everyone. A pooled connection is reused, so a leaked GUC would
// disarm the append-only guard for whatever ran next on it.
#[tokio::test]
async fn the_events_maintenance_optin_does_not_outlive_the_archival_transaction_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let archived_session = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, archived_session).await;
    let neighbour = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, neighbour).await;

    let terminal_at = complete_session(&test_db, archived_session, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);
    let outcome = test_db
        .store()
        .archive_terminal_session(archived_session, boundary, boundary)
        .await
        .expect("archive terminal session");
    assert!(
        matches!(outcome, ArchiveOutcome::Archived(_)),
        "expected the session to be archived, observed {outcome:?}"
    );

    let events = qualified(test_db.schema_name(), "events");
    let error = sqlx::query(&format!("DELETE FROM {events} WHERE session_id = $1"))
        .bind(neighbour.0)
        .execute(test_db.store().pool())
        .await
        .expect_err("the append-only guard must still refuse deletes after an archival");
    assert!(
        error.to_string().contains("events table is append-only"),
        "the refusal must come from the append-only guard, observed: {error}"
    );
    assert_eq!(
        live_event_count(&test_db, neighbour).await,
        3,
        "a neighbouring session must keep every live event row"
    );
}

// Pins: an archival that fails after deleting the live rows leaves NOTHING
// behind — the events are still there, no archive row exists, and the session is
// not marked. The archive and the delete are one transaction, so a crash between
// them cannot produce a session whose history was deleted without a durable
// archive to replace it.
#[cfg(feature = "failpoints")]
#[tokio::test]
async fn a_failure_after_the_delete_rolls_back_the_whole_archival_db() {
    use moa_session::failpoints;

    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;
    let before = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read history before the failed archival");
    let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
    let boundary = terminal_at + Duration::seconds(1);

    failpoints::arm("session_archive_post_delete", 1);
    let error = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect_err("the armed failpoint must fail the archival");
    failpoints::reset("session_archive_post_delete");
    assert!(
        error.to_string().contains("session_archive_post_delete"),
        "the failure must come from the armed failpoint, observed: {error}"
    );

    assert_eq!(
        live_event_count(&test_db, session_id).await,
        3,
        "a rolled-back archival must leave every live event row in place"
    );
    assert!(
        test_db
            .store()
            .session_events_archived_at(session_id)
            .await
            .expect("read the archived marker after a failed archival")
            .is_none(),
        "a rolled-back archival must not mark the session archived"
    );
    assert!(
        test_db
            .store()
            .verify_session_archive(session_id)
            .await
            .expect("look up the archive after a failed archival")
            .is_none(),
        "a rolled-back archival must not leave an archive row behind"
    );
    let after = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read history after the failed archival");
    assert_eq!(
        after,
        before,
        "history must be byte-identical after a rolled-back archival; observed {:?} against {:?}",
        texts(&after),
        texts(&before)
    );

    // The retry after the transient failure must still work, so a failed pass
    // leaves the session archivable rather than permanently stuck.
    let outcome = test_db
        .store()
        .archive_terminal_session(session_id, boundary, boundary)
        .await
        .expect("archival must succeed once the injected failure clears");
    assert!(
        matches!(outcome, ArchiveOutcome::Archived(_)),
        "the retry must archive the session, observed {outcome:?}"
    );
}

// Pins: the exact statement registered in the tenant-purge catalog removes the
// purged tenant's archived history and leaves a neighbouring tenant's alone.
// The registration line is the mechanism; this is the guarantee. Residue and
// survival are both asserted, because a purge that over-deletes is as wrong as
// one that under-deletes.
#[tokio::test]
async fn the_registered_purge_statement_removes_only_the_purged_tenants_archives_db() {
    let test_db = test_db().await;
    let purged_tenant = TenantId::from(Uuid::now_v7());
    let neighbour_tenant = TenantId::from(Uuid::now_v7());

    let mut archived = Vec::new();
    for tenant_id in [purged_tenant, neighbour_tenant] {
        let session_id = new_session(&test_db, tenant_id).await;
        seed_history(&test_db, session_id).await;
        let terminal_at = complete_session(&test_db, session_id, SessionStatus::Completed).await;
        let boundary = terminal_at + Duration::seconds(1);
        let outcome = test_db
            .store()
            .archive_terminal_session(session_id, boundary, boundary)
            .await
            .expect("archive terminal session");
        assert!(
            matches!(outcome, ArchiveOutcome::Archived(_)),
            "expected {tenant_id:?} session to be archived, observed {outcome:?}"
        );
        archived.push(session_id);
    }

    // The statement under test is the one the purge catalog registers, sourced
    // from the crate that owns the table -- not a copy restated here. A test
    // that spells out its own `DELETE` can only ever verify itself, and would
    // report green about a registered statement it never touched.
    assert_eq!(
        test_db.schema_name(),
        "public",
        "the purge statement is deliberately unqualified and resolves through \
         search_path; a non-public test schema would silently prove a different table"
    );
    let deleted = sqlx::query(moa_session::archive::TENANT_PURGE_SQL)
        .bind(purged_tenant.0)
        .execute(test_db.store().pool())
        .await
        .expect("the registered tenant-purge statement must run")
        .rows_affected();
    assert_eq!(
        deleted, 1,
        "the purge statement must remove exactly the purged tenant's archive"
    );

    assert!(
        test_db
            .store()
            .verify_session_archive(archived[0])
            .await
            .expect("look up the purged tenant's archive")
            .is_none(),
        "the purged tenant's archived history must be gone"
    );
    let survivor = test_db
        .store()
        .verify_session_archive(archived[1])
        .await
        .expect("look up the neighbouring tenant's archive")
        .expect("the neighbouring tenant's archive must survive the purge");
    assert_eq!(
        survivor.event_count, 3,
        "the neighbouring tenant's archived history must be untouched, observed {survivor:?}"
    );
}

// Pins: a session marked archived whose archive row is missing is an error, not
// an empty history. The marker and the archive are written together, so their
// disagreement means something destroyed one of them — and silently returning
// "this session has no messages" would present that as a fact about the
// conversation.
#[tokio::test]
async fn a_session_marked_archived_without_an_archive_row_is_an_error_db() {
    let test_db = test_db().await;
    let tenant_id = TenantId::from(Uuid::now_v7());
    let session_id = new_session(&test_db, tenant_id).await;
    seed_history(&test_db, session_id).await;

    let sessions = qualified(test_db.schema_name(), "sessions");
    sqlx::query(&format!(
        "UPDATE {sessions} SET events_archived_at = NOW() WHERE id = $1"
    ))
    .bind(session_id.0)
    .execute(test_db.store().pool())
    .await
    .expect("mark the session archived without writing an archive");

    let error = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect_err("a marked session with no archive must not read as empty history");
    assert!(
        error.to_string().contains("has no archive row"),
        "the error must name the missing archive, observed: {error}"
    );
}

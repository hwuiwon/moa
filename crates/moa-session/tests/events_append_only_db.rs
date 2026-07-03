//! Integration tests for the append-only `events` table invariant.

mod shared;

use moa_core::{
    Event, EventRange, MoaError, ModelId, SessionActorRef, SessionMeta, SessionStore, TenantId,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use uuid::Uuid;

const TENANT_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const USER_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

async fn test_db() -> TestDb {
    bootstrap_test_db().await.expect(
        "bootstrap Postgres test database; start the compose Postgres or set MOA_DATABASE_URL",
    )
}

fn tenant_id() -> TenantId {
    TenantId::from(Uuid::parse_str(TENANT_ID).expect("test tenant id should be a valid UUID"))
}

async fn new_session(test_db: &TestDb) -> moa_core::SessionId {
    test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: tenant_id(),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(42),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create dedupe test session")
}

fn user_message(text: &str) -> Event {
    Event::UserMessage {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}

async fn seeded_event(test_db: &TestDb) -> Uuid {
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: tenant_id(),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(42),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create append-only test session");
    test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "keep me immutable".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit append-only test event");
    test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read emitted event")
        .into_iter()
        .next()
        .expect("expected one emitted event")
        .id
}

#[tokio::test]
async fn delete_empty_session_removes_session_without_touching_events_table() {
    let test_db = test_db().await;
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: tenant_id(),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(42),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create empty session");

    test_db
        .store()
        .delete_empty_session(session_id)
        .await
        .expect("delete empty session");

    let error = test_db
        .store()
        .get_session(session_id)
        .await
        .expect_err("empty session should be deleted");
    assert!(matches!(error, MoaError::SessionNotFound(id) if id == session_id));
}

#[tokio::test]
async fn delete_empty_session_rejects_session_with_append_only_events() {
    let test_db = test_db().await;
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            tenant_id: tenant_id(),
            created_by: Some(SessionActorRef::Identity {
                id: Uuid::from_u128(42),
            }),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");
    test_db
        .store()
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "keep me immutable".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit event");

    let error = test_db
        .store()
        .delete_empty_session(session_id)
        .await
        .expect_err("non-empty session must not be destructively deleted");
    assert!(
        matches!(error, MoaError::Unsupported(ref message) if message.contains("append-only event")),
        "unexpected delete error: {error:?}"
    );
    let events = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read events after rejected delete");
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn update_on_events_is_blocked_for_app_role() {
    let test_db = test_db().await;
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_event_mutation(
        &test_db,
        TENANT_ID,
        USER_ID,
        &format!(
            "UPDATE {events} SET payload = jsonb_set(payload, '{{blocked}}', 'true'::jsonb) WHERE id = $1"
        ),
        event_id,
    )
    .await
    .expect_err("moa_app UPDATE on events must be blocked");
    shared::assert_events_append_only_error(&error);
}

#[tokio::test]
async fn delete_on_events_is_blocked_for_app_role() {
    let test_db = test_db().await;
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_event_mutation(
        &test_db,
        TENANT_ID,
        USER_ID,
        &format!("DELETE FROM {events} WHERE id = $1"),
        event_id,
    )
    .await
    .expect_err("moa_app DELETE on events must be blocked");
    shared::assert_events_append_only_error(&error);
}

#[tokio::test]
async fn truncate_on_events_is_blocked_for_app_role() {
    let test_db = test_db().await;
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_statement(
        &test_db,
        TENANT_ID,
        USER_ID,
        &format!("TRUNCATE TABLE {events}"),
    )
    .await
    .expect_err("moa_app TRUNCATE on events must be blocked");
    shared::assert_events_append_only_error(&error);
}

#[tokio::test]
async fn update_on_events_is_blocked_even_if_privilege_is_regranted() {
    let test_db = test_db().await;
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    sqlx::query(&format!("GRANT UPDATE ON TABLE {events} TO moa_app"))
        .execute(test_db.store().pool())
        .await
        .expect("temporarily regrant update for defense-in-depth test");

    let error = shared::execute_app_role_event_mutation(
        &test_db,
        TENANT_ID,
        USER_ID,
        &format!(
            "UPDATE {events} SET payload = jsonb_set(payload, '{{blocked}}', 'true'::jsonb) WHERE id = $1"
        ),
        event_id,
    )
    .await
    .expect_err("append-only trigger must block update after privilege regrant");
    shared::assert_events_append_only_error(&error);
    assert!(
        matches!(
            error
                .as_database_error()
                .and_then(|database_error| database_error.code())
                .as_deref(),
            Some("P0001")
        ),
        "defense-in-depth path should reach append-only trigger: {error}"
    );
}

#[tokio::test]
async fn repeated_dedupe_key_returns_first_sequence_and_inserts_once_db() {
    // Pins: a retried append with the same (session_id, dedupe_key) returns the
    // first persisted sequence number and never inserts a second event.
    let test_db = test_db().await;
    let session_id = new_session(&test_db).await;

    let first = test_db
        .store()
        .emit_event_record(
            session_id,
            user_message("idempotent-first"),
            Some("dedupe-key-1".to_string()),
        )
        .await
        .expect("first deduped append");
    let second = test_db
        .store()
        .emit_event_record(
            session_id,
            user_message("idempotent-second"),
            Some("dedupe-key-1".to_string()),
        )
        .await
        .expect("second deduped append with same key");

    assert_eq!(
        first.sequence_num, second.sequence_num,
        "same dedupe key must return the first persisted sequence number"
    );

    let events = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read events after deduped appends");
    assert_eq!(events.len(), 1, "dedupe must insert exactly one event");
}

#[tokio::test]
async fn distinct_dedupe_keys_append_separate_events_db() {
    // Pins: different dedupe keys append independently, each with its own sequence.
    let test_db = test_db().await;
    let session_id = new_session(&test_db).await;

    let first = test_db
        .store()
        .emit_event_record(session_id, user_message("key-a"), Some("key-a".to_string()))
        .await
        .expect("append under key-a");
    let second = test_db
        .store()
        .emit_event_record(session_id, user_message("key-b"), Some("key-b".to_string()))
        .await
        .expect("append under key-b");

    assert_eq!(first.sequence_num, 0);
    assert_eq!(second.sequence_num, 1);
    let events = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read events after distinct-key appends");
    assert_eq!(events.len(), 2);
}

#[cfg(feature = "failpoints")]
mod failpoint_db {
    //! Failpoint chaos coverage: transient storage failures around the append
    //! commit must never lose or duplicate events. Run with
    //! `cargo nextest run -p moa-session --features failpoints`.

    use super::*;
    use moa_session::failpoints;

    #[tokio::test]
    async fn append_pre_write_failure_then_retry_appends_exactly_once_db() {
        // Pins: a failure injected before any write surfaces as StorageError,
        // and the retried append persists exactly one event.
        let test_db = test_db().await;
        let session_id = new_session(&test_db).await;
        failpoints::arm("event_append_pre", 1);

        let first = test_db
            .store()
            .emit_event_record(
                session_id,
                user_message("pre-write-fault"),
                Some("failpoint-pre".to_string()),
            )
            .await;
        assert!(
            matches!(first, Err(MoaError::StorageError(_))),
            "armed pre-write failpoint must fail the first append: {first:?}"
        );

        let retried = test_db
            .store()
            .emit_event_record(
                session_id,
                user_message("pre-write-fault"),
                Some("failpoint-pre".to_string()),
            )
            .await
            .expect("retry after pre-write fault succeeds");
        assert_eq!(retried.sequence_num, 0);

        let events = test_db
            .store()
            .get_events(session_id, EventRange::all())
            .await
            .expect("read events after pre-write fault retry");
        assert_eq!(events.len(), 1, "exactly one event after retry");
        failpoints::reset("event_append_pre");
    }

    #[tokio::test]
    async fn append_post_commit_ack_loss_then_retry_dedupes_to_one_event_db() {
        // Pins: an ack lost AFTER commit (the ambiguous failure containers
        // rarely reproduce) leaves the row durable; the caller's retry with
        // the same dedupe key returns the original sequence instead of
        // appending a duplicate.
        let test_db = test_db().await;
        let session_id = new_session(&test_db).await;
        failpoints::arm("event_append_post_commit", 1);

        let first = test_db
            .store()
            .emit_event_record(
                session_id,
                user_message("post-commit-fault"),
                Some("failpoint-post".to_string()),
            )
            .await;
        assert!(
            matches!(first, Err(MoaError::StorageError(_))),
            "armed post-commit failpoint must fail the ack: {first:?}"
        );

        let retried = test_db
            .store()
            .emit_event_record(
                session_id,
                user_message("post-commit-fault"),
                Some("failpoint-post".to_string()),
            )
            .await
            .expect("retry after post-commit ack loss succeeds");
        assert_eq!(
            retried.sequence_num, 0,
            "retry must resolve to the already-committed event"
        );

        let events = test_db
            .store()
            .get_events(session_id, EventRange::all())
            .await
            .expect("read events after post-commit retry");
        assert_eq!(
            events.len(),
            1,
            "the committed-but-unacked event must not be duplicated"
        );
        failpoints::reset("event_append_post_commit");
    }
}

#[tokio::test]
async fn none_dedupe_key_always_appends_db() {
    // Pins: appends without a dedupe key keep inserting every time (today's behavior).
    let test_db = test_db().await;
    let session_id = new_session(&test_db).await;

    let first = test_db
        .store()
        .emit_event_record(session_id, user_message("no-key-1"), None)
        .await
        .expect("first keyless append");
    let second = test_db
        .store()
        .emit_event_record(session_id, user_message("no-key-2"), None)
        .await
        .expect("second keyless append");

    assert_eq!(first.sequence_num, 0);
    assert_eq!(second.sequence_num, 1);
    let events = test_db
        .store()
        .get_events(session_id, EventRange::all())
        .await
        .expect("read events after keyless appends");
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn append_only_triggers_exist_and_block_app_role_after_schema_bootstrap() {
    let test_db = test_db().await;

    let trigger_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) \
         FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 \
           AND c.relname = 'events' \
           AND t.tgname IN ('events_no_update', 'events_no_delete')",
    )
    .bind(test_db.schema_name())
    .fetch_one(test_db.store().pool())
    .await
    .expect("query append-only triggers");
    assert_eq!(trigger_count, 2);

    let event_id = seeded_event(&test_db).await;
    shared::assert_events_append_only_for_app_role(&test_db, event_id, TENANT_ID, USER_ID).await;
}

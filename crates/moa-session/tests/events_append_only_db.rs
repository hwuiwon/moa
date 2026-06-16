//! Integration tests for the append-only `events` table invariant.

mod shared;

use moa_core::{
    Event, EventRange, MoaError, ModelId, SessionMeta, SessionStore, UserId, WorkspaceId,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use uuid::Uuid;

const WORKSPACE_ID: &str = "events-append-only";
const USER_ID: &str = "append-only-user";

async fn configured_test_db() -> Option<TestDb> {
    if !shared::postgres_url_is_configured() {
        return None;
    }
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

async fn seeded_event(test_db: &TestDb) -> Uuid {
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new(WORKSPACE_ID),
            user_id: UserId::new(USER_ID),
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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new(WORKSPACE_ID),
            user_id: UserId::new(USER_ID),
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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let session_id = test_db
        .store()
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new(WORKSPACE_ID),
            user_id: UserId::new(USER_ID),
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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_event_mutation(
        &test_db,
        WORKSPACE_ID,
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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_event_mutation(
        &test_db,
        WORKSPACE_ID,
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
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let events = shared::qualified(test_db.schema_name(), "events");
    let error = shared::execute_app_role_statement(
        &test_db,
        WORKSPACE_ID,
        USER_ID,
        &format!("TRUNCATE TABLE {events}"),
    )
    .await
    .expect_err("moa_app TRUNCATE on events must be blocked");
    shared::assert_events_append_only_error(&error);
}

#[tokio::test]
async fn update_on_events_is_blocked_even_if_privilege_is_regranted() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let event_id = seeded_event(&test_db).await;
    let events = shared::qualified(test_db.schema_name(), "events");
    sqlx::query(&format!("GRANT UPDATE ON TABLE {events} TO moa_app"))
        .execute(test_db.store().pool())
        .await
        .expect("temporarily regrant update for defense-in-depth test");

    let error = shared::execute_app_role_event_mutation(
        &test_db,
        WORKSPACE_ID,
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
async fn migration_applied_idempotently_does_not_break_invariant() {
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    moa_session::schema::migrate(test_db.store().pool(), Some(test_db.schema_name()))
        .await
        .expect("first explicit migration replay");
    moa_session::schema::migrate(test_db.store().pool(), Some(test_db.schema_name()))
        .await
        .expect("second explicit migration replay");

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
    shared::assert_events_append_only_for_app_role(&test_db, event_id, WORKSPACE_ID, USER_ID).await;
}

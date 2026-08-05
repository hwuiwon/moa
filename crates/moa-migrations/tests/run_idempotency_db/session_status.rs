//! One-way session lifecycle status migration scenarios.

use super::support::*;

#[test]
fn session_status_idle_is_the_next_forward_only_migration_offline() {
    // Pins: the hard status cutover is one contiguous migration after typed
    // connector origins, and the runner embeds no compatibility migration.
    assert_eq!(
        migration_version("session_status_idle")
            .expect("the session status migration must be embedded"),
        54
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn session_status_idle_rewrites_live_and_archived_state_idempotently_db() {
    // Pins: the production runner performs the complete one-way rewrite across
    // sessions, append-only live events, and immutable archive bytes, re-derives
    // BLAKE3, restores archive immutability, and leaves no old exact value.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create session status migration database");

    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "typed_connector_origin").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;

        let tenant_id = uuid::Uuid::new_v4();
        let live_session_id = uuid::Uuid::new_v4();
        let archived_session_id = uuid::Uuid::new_v4();
        for (session_id, archived) in [(live_session_id, false), (archived_session_id, true)] {
            sqlx::query(
                "INSERT INTO sessions \
                    (id, tenant_id, storage_partition_id, user_id, status, model, \
                     event_count, events_archived_at) \
                 VALUES ($1, $2, $2::TEXT, $2::TEXT, 'paused', 'test-model', 1, \
                         CASE WHEN $3 THEN NOW() ELSE NULL END)",
            )
            .bind(session_id)
            .bind(tenant_id)
            .bind(archived)
            .execute(&target)
            .await?;
        }

        for (sequence_num, from, to) in [(0_i64, "running", "paused"), (1, "paused", "running")] {
            sqlx::query(
                "INSERT INTO events \
                    (id, session_id, tenant_id, storage_partition_id, user_id, \
                     sequence_num, turn_number, event_type, payload) \
                 VALUES ($1, $2, $3, $3::TEXT, $3::TEXT, $4, 1, \
                         'SessionStatusChanged', \
                         jsonb_build_object( \
                             'type', 'SessionStatusChanged', \
                             'data', jsonb_build_object( \
                                 'from', $5::TEXT, 'to', $6::TEXT, \
                                 'note', 'paused'))) ",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(live_session_id)
            .bind(tenant_id)
            .bind(sequence_num)
            .bind(from)
            .bind(to)
            .execute(&target)
            .await?;
        }

        let archive_body = serde_json::json!({
            "format_version": 1,
            "session_id": archived_session_id,
            "events": [{
                "id": uuid::Uuid::new_v4(),
                "sequence_num": 0,
                "event_type": "SessionStatusChanged",
                "payload": {
                    "type": "SessionStatusChanged",
                    "data": {"from": "running", "to": "paused", "note": "paused"}
                },
                "timestamp": "2026-08-04T00:00:00Z",
                "brain_id": null,
                "hand_id": null,
                "token_count": null
            }]
        });
        let archive_bytes = serde_json::to_vec(&archive_body)?;
        let archive_digest = blake3::hash(&archive_bytes).as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO session_event_archives \
                (session_id, tenant_id, format_version, event_count, \
                 first_sequence_num, last_sequence_num, payload, content_digest, archived_at) \
             VALUES ($1, $2, 1, 1, 0, 0, $3, $4, NOW())",
        )
        .bind(archived_session_id)
        .bind(tenant_id)
        .bind(archive_bytes)
        .bind(archive_digest)
        .execute(&target)
        .await?;
        target.close().await;

        let first = run_reporting_applied_serialized(database.target_url()).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM sessions ORDER BY id")
            .fetch_all(&target)
            .await?;
        let live_payloads: Vec<serde_json::Value> =
            sqlx::query_scalar("SELECT payload FROM events ORDER BY sequence_num")
                .fetch_all(&target)
                .await?;
        let (first_archive_bytes, first_archive_digest): (Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT payload, content_digest FROM session_event_archives \
             WHERE session_id = $1",
        )
        .bind(archived_session_id)
        .fetch_one(&target)
        .await?;
        let first_archive: serde_json::Value = serde_json::from_slice(&first_archive_bytes)?;

        assert_eq!(statuses, vec!["idle", "idle"]);
        assert_eq!(live_payloads[0]["data"]["to"], serde_json::json!("idle"));
        assert_eq!(live_payloads[1]["data"]["from"], serde_json::json!("idle"));
        assert_eq!(
            live_payloads[0]["data"]["note"],
            serde_json::json!("paused")
        );
        assert_eq!(
            first_archive["events"][0]["payload"]["data"]["to"],
            serde_json::json!("idle")
        );
        assert_eq!(
            first_archive["events"][0]["payload"]["data"]["note"],
            serde_json::json!("paused")
        );
        assert_eq!(
            first_archive_digest,
            blake3::hash(&first_archive_bytes).as_bytes().to_vec(),
            "archive digest must be re-derived from the exact rewritten bytes"
        );

        let immutable_error = sqlx::query(
            "UPDATE session_event_archives SET payload = payload WHERE session_id = $1",
        )
        .bind(archived_session_id)
        .execute(&target)
        .await
        .expect_err("archive immutable trigger must be restored");
        assert!(
            immutable_error.to_string().contains("archive is immutable"),
            "unexpected immutable trigger error: {immutable_error}"
        );
        target.close().await;

        let second = run_reporting_applied_serialized(database.target_url()).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let second_archive: (Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT payload, content_digest FROM session_event_archives \
             WHERE session_id = $1",
        )
        .bind(archived_session_id)
        .fetch_one(&target)
        .await?;
        let old_live_values: i64 = sqlx::query_scalar(
            "SELECT \
                 (SELECT COUNT(*) FROM sessions WHERE status = 'paused') + \
                 (SELECT COUNT(*) FROM events \
                  WHERE event_type = 'SessionStatusChanged' \
                    AND (payload #>> '{data,from}' = 'paused' \
                         OR payload #>> '{data,to}' = 'paused'))",
        )
        .fetch_one(&target)
        .await?;
        let cutover_receipts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM public.deployment_cutover_receipts")
                .fetch_one(&target)
                .await?;
        target.close().await;

        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            first_archive_bytes,
            first_archive_digest,
            second_archive,
            old_live_values,
            cutover_receipts,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (
        first,
        second,
        first_bytes,
        first_digest,
        second_archive,
        old_live_values,
        cutover_receipts,
    ) = outcome.expect("session status migration should complete");
    assert_eq!(
        first,
        vec![
            expected_migration_labels()
                .last()
                .expect("V54 label must exist")
                .clone()
        ]
    );
    assert!(second.is_empty(), "second migration run must apply no SQL");
    assert_eq!(second_archive, (first_bytes, first_digest));
    assert_eq!(old_live_values, 0);
    assert_eq!(
        cutover_receipts, 0,
        "SQL completion alone must not open the pre-runtime cutover gate"
    );
}

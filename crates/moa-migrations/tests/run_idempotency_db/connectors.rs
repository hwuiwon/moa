//! Connector-parent, replay-ledger, and credential migration scenarios.

use super::support::*;

#[test]
fn knowledge_connection_parent_migration_narrows_the_child_before_later_migrations_offline() {
    // Pins: V52 makes the connector parent authoritative before later migrations
    // build on the narrowed knowledge child.
    assert_eq!(
        migration_version("knowledge_connection_parent_constraint")
            .expect("the V52 parent migration must be embedded"),
        52,
    );
    let parent_backfill = KNOWLEDGE_CONNECTION_PARENT_MIGRATION
        .find("INSERT INTO moa.connector_connections")
        .expect("V52 must backfill generic connector parents");
    let child_narrowing = KNOWLEDGE_CONNECTION_PARENT_MIGRATION
        .find("DROP COLUMN credential_ref,\n    DROP COLUMN status")
        .expect("V52 must remove child-local credential and lifecycle state");
    assert!(
        child_narrowing > parent_backfill,
        "legacy parent backfill must read child lifecycle before the child is narrowed"
    );
    assert!(
        KNOWLEDGE_CONNECTION_PARENT_MIGRATION.contains(
            "DROP COLUMN previous_credential_ref,\n    ADD COLUMN parent_created_by_claim"
        )
    );
    assert!(
        !KNOWLEDGE_CONNECTION_PARENT_MIGRATION
            .contains("ADD COLUMN candidate_projection_credential_ref")
    );
    assert!(
        !KNOWLEDGE_CONNECTION_PARENT_MIGRATION
            .contains("knowledge_link_claims_projection_handle_bounded")
    );
    assert!(
        !KNOWLEDGE_CONNECTION_PARENT_MIGRATION
            .contains("knowledge_link_claims_previous_projection_handle_immutable")
    );
    assert!(KNOWLEDGE_CONNECTION_PARENT_MIGRATION.contains("candidate_credential_ref"));
    assert!(KNOWLEDGE_CONNECTION_PARENT_MIGRATION.contains("previous_vault_credential_ref"));
    for removed_table in [
        "connector_mcp_catalog_revisions",
        "connector_knowledge_source_bindings",
        "connector_knowledge_source_invocations",
    ] {
        assert!(
            !MIGRATION_OWNERSHIP.contains(&format!("name = \"{removed_table}\"")),
            "removed unshipped table {removed_table} must not remain in ownership inventory"
        );
    }
}

#[test]
fn token_vault_tables_are_absent_from_the_fresh_epoch_offline() {
    // Pins: the hard-reset epoch never creates either retired persistence owner.
    assert_eq!(
        migration_version("retired_token_vault_epoch_marker")
            .expect("the V29 epoch marker must remain embedded"),
        29,
    );
    assert!(!AUTH_BASELINE_MIGRATION.contains("CREATE TABLE IF NOT EXISTS linked_connections"));
    assert!(!RETIRED_TOKEN_VAULT_EPOCH_MARKER.contains("CREATE TABLE"));
    for removed_table in ["linked_connections", "token_vault_connections"] {
        assert!(
            !MIGRATION_OWNERSHIP.contains(&format!("name = \"{removed_table}\"")),
            "removed table {removed_table} must not remain in ownership inventory"
        );
    }
}

#[test]
fn typed_connector_origin_is_a_forward_only_hard_break_offline() {
    // Pins: V53 rejects ambiguous artifact rows, moves the canonical origin to
    // its typed column, removes the JSON copy, and keeps managed parents originless.
    assert_eq!(
        migration_version("typed_connector_origin")
            .expect("the V53 connector origin migration must be embedded"),
        53,
    );
    for required in [
        "ADD COLUMN origin TEXT",
        "artifact connector origin is missing or noncanonical",
        "SET origin = non_secret_config ->> 'origin'",
        "non_secret_config = non_secret_config - 'origin'",
        "connector_connections_origin_canonical",
        "connector_connections_definition_origin_consistent",
        "built_in_key IN ('knowledge:nango', 'knowledge:merge')",
    ] {
        assert!(
            TYPED_CONNECTOR_ORIGIN_MIGRATION.contains(required),
            "V53 must contain `{required}`"
        );
    }
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn typed_connector_origin_accepts_only_matching_definition_kinds_db() {
    // Pins: the migrated production schema requires one canonical origin for an
    // artifact connector, permits no origin for closed managed knowledge parents,
    // and rejects either definition kind when its origin shape is mismatched.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create typed connector-origin migration database");

    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "typed_connector_origin").await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(database.target_url())
            .await?;

        let tenant_id = uuid::Uuid::new_v4();
        let artifact_uid = uuid::Uuid::new_v4();
        let revision_uid = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.artifact \
                (artifact_uid, tenant_id, storage_partition_id, kind, name) \
             VALUES ($1, $2, $2::TEXT, 'connector', $3)",
        )
        .bind(artifact_uid)
        .bind(tenant_id)
        .bind(format!("typed-origin-connector-{artifact_uid}"))
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.artifact_revision \
                (revision_uid, artifact_uid, tenant_id, storage_partition_id, definition, \
                 canonical_hash, source_format, source_text, status, version) \
             VALUES ($1, $2, $3, $3::TEXT, '{}'::JSONB, $4, 'json', ''::BYTEA, 'published', 1)",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .bind(tenant_id)
        .bind(vec![1_u8; 32])
        .execute(&target)
        .await?;

        let artifact_connection = uuid::Uuid::new_v4();
        let artifact_insert = sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, artifact_uid, revision_uid, origin) \
             VALUES ($1, $2, 'reviewed-http', $3, $4, 'https://api.example.com')",
        )
        .bind(artifact_connection)
        .bind(tenant_id)
        .bind(artifact_uid)
        .bind(revision_uid)
        .execute(&target)
        .await?;

        let managed_connection = uuid::Uuid::new_v4();
        let managed_insert = sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, built_in_key, built_in_version) \
             VALUES ($1, $2, 'managed-nango', 'knowledge:nango', 1)",
        )
        .bind(managed_connection)
        .bind(tenant_id)
        .execute(&target)
        .await?;

        let accepted: Vec<(uuid::Uuid, Option<String>)> = sqlx::query_as(
            "SELECT connection_uid, origin FROM moa.connector_connections \
             WHERE connection_uid IN ($1, $2) ORDER BY connection_uid",
        )
        .bind(artifact_connection)
        .bind(managed_connection)
        .fetch_all(&target)
        .await?;
        let mut expected = vec![
            (
                artifact_connection,
                Some("https://api.example.com".to_string()),
            ),
            (managed_connection, None),
        ];
        expected.sort_by_key(|(connection_uid, _)| *connection_uid);

        let missing_artifact_origin = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, artifact_uid, revision_uid) \
                 VALUES ($1, $2, 'missing-origin', $3, $4)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(tenant_id)
            .bind(artifact_uid)
            .bind(revision_uid)
            .execute(&target)
            .await
            .expect_err("artifact connector without an origin must fail"),
        );
        let managed_with_origin = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, built_in_key, built_in_version, origin) \
                 VALUES ($1, $2, 'managed-with-origin', 'knowledge:merge', 1, \
                         'https://api.example.com')",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(tenant_id)
            .execute(&target)
            .await
            .expect_err("managed knowledge parent with an origin must fail"),
        );
        let noncanonical_artifact_origin = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, artifact_uid, revision_uid, origin) \
                 VALUES ($1, $2, 'noncanonical-origin', $3, $4, 'HTTPS://API.EXAMPLE.COM')",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(tenant_id)
            .bind(artifact_uid)
            .bind(revision_uid)
            .execute(&target)
            .await
            .expect_err("noncanonical artifact origin must fail"),
        );

        target.close().await;

        assert_eq!(artifact_insert.rows_affected(), 1);
        assert_eq!(managed_insert.rows_affected(), 1);
        assert_eq!(accepted, expected);
        assert_eq!(
            missing_artifact_origin,
            (
                Some("23514".to_string()),
                Some("connector_connections_definition_origin_consistent".to_string()),
            )
        );
        assert_eq!(managed_with_origin, missing_artifact_origin);
        assert_eq!(
            noncanonical_artifact_origin,
            (
                Some("23514".to_string()),
                Some("connector_connections_origin_canonical".to_string()),
            )
        );

        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;

    database.finish(outcome).await.expect(
        "typed connector-origin constraints should accept only matching definition/origin pairs",
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn tenant_connector_connections_backfill_rls_and_replay_schema_db() {
    // Pins: V50 maps every legacy Nango/Merge knowledge connection to one closed
    // built-in parent, preserves primary credential series, installs all three
    // tenant-isolated connector tables, and remains a no-op on replay.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let neighbour_tenant = uuid::Uuid::new_v4();
    let nango_connection = uuid::Uuid::new_v4();
    let merge_connection = uuid::Uuid::new_v4();
    let credential_uid = uuid::Uuid::new_v4();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect tenant connector migration maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create tenant connector migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "privacy_export_auditor_access").await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;
        for (connection_uid, provider, connector) in [
            (nango_connection, "nango", "google-drive"),
            (merge_connection, "merge", "sharepoint"),
        ] {
            sqlx::query(
                "INSERT INTO moa.knowledge_connections \
                    (connection_uid, tenant_id, storage_partition_id, provider, \
                     provider_config_key, provider_connection_id, connector, \
                     credential_ref, status) \
                 VALUES ($1, $2, $2::TEXT, $3, 'config', $1::TEXT, $4, \
                         'vault://legacy', 'active')",
            )
            .bind(connection_uid)
            .bind(tenant_id)
            .bind(provider)
            .bind(connector)
            .execute(&target)
            .await?;
        }
        sqlx::query(
            "INSERT INTO tenant_credential_versions \
                (credential_uid, tenant_id, connection_uid, kind, version, material_sealed, kms_key_id) \
             VALUES ($1, $2, $3, 'provider_api_key', 1, $4, 'kms-test')",
        )
        .bind(credential_uid)
        .bind(tenant_id)
        .bind(nango_connection)
        .bind(vec![1_u8, 2, 3])
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO tenant_credential_operations \
                (tenant_id, operation_id, request_hash, operation, credential_uid, \
                 connection_uid, kind, version, principal_kind, principal_id, outcome) \
             VALUES ($1, 'legacy-create', repeat('a', 64), 'create', $2, $3, \
                     'provider_api_key', 1, 'caller', $4, 'succeeded')",
        )
        .bind(tenant_id)
        .bind(credential_uid)
        .bind(nango_connection)
        .bind(uuid::Uuid::new_v4())
        .execute(&target)
        .await?;
        target.close().await;

        let first = apply_through_migration(&target_url, "tenant_connector_connections").await?;
        let second = apply_through_migration(&target_url, "tenant_connector_connections").await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;

        let parents: Vec<(uuid::Uuid, String, i64, String, String)> = sqlx::query_as(
            "SELECT connection_uid, built_in_key, built_in_version, lifecycle_status, health_status \
             FROM moa.connector_connections WHERE tenant_id = $1 ORDER BY built_in_key",
        )
        .bind(tenant_id)
        .fetch_all(&target)
        .await?;
        let legacy_slots: (String, String) = sqlx::query_as(
            "SELECT \
                (SELECT slot_name FROM tenant_credential_versions WHERE credential_uid = $1), \
                (SELECT slot_name FROM tenant_credential_operations \
                 WHERE tenant_id = $2 AND operation_id = 'legacy-create')",
        )
        .bind(credential_uid)
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;
        let rls_tables: Vec<(String, bool)> = sqlx::query_as(
            "SELECT relname::TEXT, relforcerowsecurity \
             FROM pg_class WHERE oid = ANY(ARRAY[ \
                 'moa.connector_connections'::REGCLASS, \
                 'moa.connector_action_bindings'::REGCLASS, \
                 'moa.connector_action_invocations'::REGCLASS \
             ]) ORDER BY relname",
        )
        .fetch_all(&target)
        .await?;
        let policies: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::TEXT || ':' || policyname::TEXT \
             FROM pg_policies WHERE schemaname = 'moa' \
               AND tablename LIKE 'connector_%' ORDER BY 1",
        )
        .fetch_all(&target)
        .await?;
        let stages: Vec<(i16, String)> = sqlx::query_as(
            "SELECT stage_order, stage_name FROM moa.tenant_purge_catalog \
             WHERE stage_order BETWEEN 26 AND 29 ORDER BY stage_order",
        )
        .fetch_all(&target)
        .await?;
        let purge_definition: String = sqlx::query_scalar(
            "SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)",
        )
        .fetch_one(&target)
        .await?;

        let neighbour_connection = uuid::Uuid::new_v4();
        let target_binding = uuid::Uuid::new_v4();
        let neighbour_binding = uuid::Uuid::new_v4();
        let target_invocation = uuid::Uuid::new_v4();
        let neighbour_invocation = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, built_in_key, built_in_version, \
                 lifecycle_status, health_status) \
             VALUES ($1, $2, 'neighbour', 'knowledge:nango', 1, 'active', 'ready')",
        )
        .bind(neighbour_connection)
        .bind(neighbour_tenant)
        .execute(&target)
        .await?;
        for (binding_uid, binding_tenant, connection_uid) in [
            (target_binding, tenant_id, nango_connection),
            (neighbour_binding, neighbour_tenant, neighbour_connection),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_action_bindings \
                    (binding_uid, tenant_id, connection_uid, action_id, connection_generation, \
                     compiled_contract, contract_hash, governed_contract_revision, minimum_effect) \
                 VALUES ($1, $2, $3, 'read', 1, '{}'::JSONB, repeat('b', 64), \
                         'runtime-v1', 'allow')",
            )
            .bind(binding_uid)
            .bind(binding_tenant)
            .bind(connection_uid)
            .execute(&target)
            .await?;
        }
        for (invocation_uid, invocation_tenant, connection_uid, binding_uid, tool_call) in [
            (target_invocation, tenant_id, nango_connection, target_binding, "target-call"),
            (
                neighbour_invocation,
                neighbour_tenant,
                neighbour_connection,
                neighbour_binding,
                "neighbour-call",
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_action_invocations \
                    (invocation_uid, tenant_id, connection_uid, binding_uid, \
                     connection_generation, tool_call_id, request_hash) \
                 VALUES ($1, $2, $3, $4, 1, $5, repeat('c', 64))",
            )
            .bind(invocation_uid)
            .bind(invocation_tenant)
            .bind(connection_uid)
            .bind(binding_uid)
            .bind(tool_call)
            .execute(&target)
            .await?;
        }

        // Pins: reservation is durable before any send, `transmitting` is the
        // one-way transport claim, and every terminal outcome closes the row.
        let succeeded_invocation = uuid::Uuid::new_v4();
        let failed_before_send_invocation = uuid::Uuid::new_v4();
        let failed_invocation = uuid::Uuid::new_v4();
        let unknown_invocation = uuid::Uuid::new_v4();
        let replay_invocation = uuid::Uuid::new_v4();
        let invalid_direct_invocation = uuid::Uuid::new_v4();
        for (invocation_uid, tool_call) in [
            (succeeded_invocation, "lifecycle-succeeded"),
            (failed_before_send_invocation, "lifecycle-failed-before-send"),
            (failed_invocation, "lifecycle-failed"),
            (unknown_invocation, "lifecycle-unknown"),
            (replay_invocation, "lifecycle-transmitting-replay"),
            (invalid_direct_invocation, "lifecycle-invalid-direct"),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_action_invocations \
                    (invocation_uid, tenant_id, connection_uid, binding_uid, \
                     connection_generation, tool_call_id, request_hash) \
                 VALUES ($1, $2, $3, $4, 1, $5, repeat('d', 64))",
            )
            .bind(invocation_uid)
            .bind(tenant_id)
            .bind(nango_connection)
            .bind(target_binding)
            .bind(tool_call)
            .execute(&target)
            .await?;
        }

        sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'transmitting', \
             updated_at = NOW() WHERE invocation_uid = $1",
        )
        .bind(succeeded_invocation)
        .execute(&target)
        .await?;
        sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'succeeded', \
             output_metadata = '{\"status\":200}'::JSONB, completed_at = NOW(), \
             updated_at = NOW() WHERE invocation_uid = $1",
        )
        .bind(succeeded_invocation)
        .execute(&target)
        .await?;
        sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'failed_before_send', \
             error_metadata = '{\"class\":\"admission\"}'::JSONB, completed_at = NOW(), \
             updated_at = NOW() WHERE invocation_uid = $1",
        )
        .bind(failed_before_send_invocation)
        .execute(&target)
        .await?;
        for (invocation_uid, state) in [
            (failed_invocation, "failed"),
            (unknown_invocation, "unknown_outcome"),
        ] {
            sqlx::query(
                "UPDATE moa.connector_action_invocations SET state = 'transmitting', \
                 updated_at = NOW() WHERE invocation_uid = $1",
            )
            .bind(invocation_uid)
            .execute(&target)
            .await?;
            sqlx::query(
                "UPDATE moa.connector_action_invocations SET state = $2, \
                 error_metadata = '{\"class\":\"transport\"}'::JSONB, completed_at = NOW(), \
                 updated_at = NOW() WHERE invocation_uid = $1",
            )
            .bind(invocation_uid)
            .bind(state)
            .execute(&target)
            .await?;
        }
        sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'transmitting', \
             updated_at = NOW() WHERE invocation_uid = $1",
        )
        .bind(replay_invocation)
        .execute(&target)
        .await?;

        // Pins: a direct reserved-to-terminal jump, a second transmitting
        // claim, and any terminal rewrite all fail at the database boundary.
        let invalid_direct_error = sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'succeeded', \
             completed_at = NOW() WHERE invocation_uid = $1",
        )
        .bind(invalid_direct_invocation)
        .execute(&target)
        .await
        .expect_err("reserved invocation cannot skip the transmitting boundary");
        let transmitting_replay_error = sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'transmitting' \
             WHERE invocation_uid = $1",
        )
        .bind(replay_invocation)
        .execute(&target)
        .await
        .expect_err("an identical transmitting replay cannot claim another send");
        let terminal_rewrite_error = sqlx::query(
            "UPDATE moa.connector_action_invocations SET state = 'failed', \
             error_metadata = '{\"class\":\"mutated\"}'::JSONB WHERE invocation_uid = $1",
        )
        .bind(succeeded_invocation)
        .execute(&target)
        .await
        .expect_err("a terminal invocation cannot be rewritten");
        let error_fact = |error: sqlx::Error| {
            (
                error
                    .as_database_error()
                    .and_then(|database| database.code().map(|code| code.into_owned())),
                error
                    .as_database_error()
                    .and_then(|database| database.constraint().map(ToOwned::to_owned)),
            )
        };
        let invocation_states: Vec<(String, String)> = sqlx::query_as(
            "SELECT tool_call_id, state FROM moa.connector_action_invocations \
             WHERE tenant_id = $1 AND tool_call_id LIKE 'lifecycle-%' ORDER BY tool_call_id",
        )
        .bind(tenant_id)
        .fetch_all(&target)
        .await?;
        let invocation_transition_facts = (
            invocation_states,
            error_fact(invalid_direct_error),
            error_fact(transmitting_replay_error),
            error_fact(terminal_rewrite_error),
        );

        let mut scoped = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *scoped)
            .await?;
        sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *scoped)
            .await?;
        let visible: (i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.connector_connections), \
                (SELECT count(*) FROM moa.connector_action_bindings), \
                (SELECT count(*) FROM moa.connector_action_invocations)",
        )
        .fetch_one(&mut *scoped)
        .await?;
        let neighbour_visible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.connector_connections WHERE connection_uid = $1",
        )
        .bind(neighbour_connection)
        .fetch_one(&mut *scoped)
        .await?;
        scoped.rollback().await?;

        let mut cross_tenant = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *cross_tenant)
            .await?;
        sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *cross_tenant)
            .await?;
        let cross_error = sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, built_in_key, built_in_version) \
             VALUES ($1, $2, 'forbidden', 'knowledge:nango', 1)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(neighbour_tenant)
        .execute(&mut *cross_tenant)
        .await
        .expect_err("cross-tenant connector write must fail RLS");
        let cross_sqlstate = cross_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        cross_tenant.rollback().await?;

        let mut ordinary_delete = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *ordinary_delete)
            .await?;
        sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *ordinary_delete)
            .await?;
        let delete_error = sqlx::query(
            "DELETE FROM moa.connector_action_invocations WHERE invocation_uid = $1",
        )
        .bind(target_invocation)
        .execute(&mut *ordinary_delete)
        .await
        .expect_err("ordinary connector lifecycle must not delete invocation audit");
        let delete_sqlstate = delete_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        ordinary_delete.rollback().await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            parents,
            legacy_slots,
            rls_tables,
            policies,
            stages,
            purge_definition,
            visible,
            neighbour_visible,
            cross_sqlstate,
            delete_sqlstate,
            invocation_transition_facts,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (
        first,
        second,
        parents,
        legacy_slots,
        rls_tables,
        policies,
        stages,
        purge_definition,
        visible,
        neighbour_visible,
        cross_sqlstate,
        delete_sqlstate,
        invocation_transition_facts,
    ) = outcome.expect("tenant connector V50 assertions should complete");

    assert_eq!(first, vec!["V50__tenant_connector_connections".to_string()]);
    assert!(second.is_empty(), "V50 replay must be a no-op: {second:?}");
    assert_eq!(
        parents,
        vec![
            (
                merge_connection,
                "knowledge:merge".to_string(),
                1,
                "active".to_string(),
                "ready".to_string()
            ),
            (
                nango_connection,
                "knowledge:nango".to_string(),
                1,
                "active".to_string(),
                "ready".to_string()
            ),
        ]
    );
    assert_eq!(legacy_slots, ("primary".to_string(), "primary".to_string()));
    assert_eq!(
        rls_tables,
        vec![
            ("connector_action_bindings".to_string(), true),
            ("connector_action_invocations".to_string(), true),
            ("connector_connections".to_string(), true),
        ]
    );
    assert_eq!(
        policies,
        vec![
            "connector_action_bindings:tenant_isolation".to_string(),
            "connector_action_invocations:rd_tenant".to_string(),
            "connector_action_invocations:up_tenant".to_string(),
            "connector_action_invocations:wr_tenant".to_string(),
            "connector_connections:tenant_isolation".to_string(),
        ]
    );
    assert_eq!(
        stages,
        vec![
            (26, "moa.connector_action_invocations".to_string()),
            (27, "moa.connector_action_bindings".to_string()),
            (28, "moa.connector_connections".to_string()),
            (29, "public.security_events".to_string()),
        ]
    );
    assert!(purge_definition.contains("catalog_count <> 130"));
    assert!(purge_definition.contains("exactly 130 tables"));
    assert_eq!(visible, (2, 1, 7));
    assert_eq!(neighbour_visible, 0);
    assert_eq!(cross_sqlstate.as_deref(), Some("42501"));
    assert_eq!(delete_sqlstate.as_deref(), Some("42501"));
    assert_eq!(
        invocation_transition_facts.0,
        vec![
            ("lifecycle-failed".to_string(), "failed".to_string()),
            (
                "lifecycle-failed-before-send".to_string(),
                "failed_before_send".to_string(),
            ),
            (
                "lifecycle-invalid-direct".to_string(),
                "reserved".to_string(),
            ),
            ("lifecycle-succeeded".to_string(), "succeeded".to_string()),
            (
                "lifecycle-transmitting-replay".to_string(),
                "transmitting".to_string(),
            ),
            (
                "lifecycle-unknown".to_string(),
                "unknown_outcome".to_string(),
            ),
        ]
    );
    assert_eq!(
        invocation_transition_facts.1,
        (
            Some("23514".to_string()),
            Some("connector_action_invocations_state_transition_valid".to_string()),
        )
    );
    assert_eq!(invocation_transition_facts.2, invocation_transition_facts.1);
    assert_eq!(
        invocation_transition_facts.3,
        (
            Some("23514".to_string()),
            Some("connector_action_invocations_terminal_immutable".to_string()),
        )
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn tenant_connector_use_grants_enforce_same_tenant_rls_and_restrict_deletion_db() {
    // Pins: V51 records each direct connector Use tuple once, rejects
    // cross-tenant subjects at the database boundary, hides neighboring grants
    // from moa_app, and keeps inverse registry rows ahead of every referenced
    // connection and subject in bounded tenant purge order.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let neighbour_tenant = uuid::Uuid::new_v4();
    let connection_uid = uuid::Uuid::new_v4();
    let neighbour_connection = uuid::Uuid::new_v4();
    let operator_id = uuid::Uuid::new_v4();
    let neighbour_operator = uuid::Uuid::new_v4();
    let agent_id = uuid::Uuid::new_v4();
    let neighbour_agent = uuid::Uuid::new_v4();
    let contact_id = uuid::Uuid::new_v4();
    let neighbour_contact = uuid::Uuid::new_v4();
    let expected_prior = uuid::Uuid::new_v4();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect connector-use-grant migration maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create connector-use-grant migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "tenant_connector_connections").await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;

        sqlx::query(
            "INSERT INTO tenants (id, slug, name) VALUES \
             ($1, $2, 'connector grant target'), \
             ($3, $4, 'connector grant neighbour')",
        )
        .bind(tenant_id)
        .bind(format!("connector-grant-target-{tenant_id}"))
        .bind(neighbour_tenant)
        .bind(format!("connector-grant-neighbour-{neighbour_tenant}"))
        .execute(&target)
        .await?;
        for (subject_id, subject_tenant, email) in [
            (operator_id, tenant_id, "target-operator@example.test"),
            (
                neighbour_operator,
                neighbour_tenant,
                "neighbour-operator@example.test",
            ),
        ] {
            sqlx::query("INSERT INTO users (id, tenant_id, email) VALUES ($1, $2, $3)")
                .bind(subject_id)
                .bind(subject_tenant)
                .bind(email)
                .execute(&target)
                .await?;
        }
        for (subject_id, subject_tenant, display_name) in [
            (agent_id, tenant_id, "target agent"),
            (neighbour_agent, neighbour_tenant, "neighbour agent"),
        ] {
            sqlx::query("INSERT INTO agents (id, tenant_id, display_name) VALUES ($1, $2, $3)")
                .bind(subject_id)
                .bind(subject_tenant)
                .bind(display_name)
                .execute(&target)
                .await?;
        }
        for (subject_id, subject_tenant) in [
            (contact_id, tenant_id),
            (neighbour_contact, neighbour_tenant),
        ] {
            sqlx::query(
                "INSERT INTO contacts \
                    (id, contact_id, tenant_id, storage_partition_id, state) \
                 VALUES ($1, $1, $2, $2::TEXT, 'verified')",
            )
            .bind(subject_id)
            .bind(subject_tenant)
            .execute(&target)
            .await?;
        }
        for (parent_id, parent_tenant, display_name) in [
            (connection_uid, tenant_id, "target connection"),
            (
                neighbour_connection,
                neighbour_tenant,
                "neighbour connection",
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, built_in_key, built_in_version) \
                 VALUES ($1, $2, $3, 'knowledge:nango', 1)",
            )
            .bind(parent_id)
            .bind(parent_tenant)
            .bind(display_name)
            .execute(&target)
            .await?;
        }
        target.close().await;

        let first = run_reporting_applied_serialized(&target_url).await?;
        let second = run_reporting_applied_serialized(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;

        for (grant_tenant, grant_connection, kind, subject_id) in [
            (tenant_id, connection_uid, "operator", operator_id),
            (tenant_id, connection_uid, "agent", agent_id),
            (tenant_id, connection_uid, "contact", contact_id),
            (
                neighbour_tenant,
                neighbour_connection,
                "operator",
                neighbour_operator,
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(grant_tenant)
            .bind(grant_connection)
            .bind(kind)
            .bind(subject_id)
            .execute(&target)
            .await?;
        }

        let database_error_fact = |error: sqlx::Error| {
            (
                error
                    .as_database_error()
                    .and_then(|database| database.code().map(|code| code.into_owned())),
                error
                    .as_database_error()
                    .and_then(|database| database.constraint().map(ToOwned::to_owned)),
            )
        };
        let mut tenant_mismatch_facts = Vec::new();
        for (kind, subject_id) in [
            ("operator", neighbour_operator),
            ("agent", neighbour_agent),
            ("contact", neighbour_contact),
        ] {
            let error = sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .bind(kind)
            .bind(subject_id)
            .execute(&target)
            .await
            .expect_err("a direct Use subject from another tenant must fail");
            tenant_mismatch_facts.push((kind.to_string(), database_error_fact(error)));
        }
        let connection_mismatch_fact = database_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, 'operator', $3)",
            )
            .bind(tenant_id)
            .bind(neighbour_connection)
            .bind(operator_id)
            .execute(&target)
            .await
            .expect_err("a direct Use connection from another tenant must fail"),
        );
        let invalid_kind_fact = database_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, 'service', $3)",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .bind(uuid::Uuid::new_v4())
            .execute(&target)
            .await
            .expect_err("the direct Use subject kind is a closed set"),
        );
        let duplicate_fact = database_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, 'operator', $3)",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .bind(operator_id)
            .execute(&target)
            .await
            .expect_err("one direct Use tuple has one desired-state registry row"),
        );

        let connection_delete_fact = database_error_fact(
            sqlx::query("DELETE FROM moa.connector_connections WHERE connection_uid = $1")
                .bind(connection_uid)
                .execute(&target)
                .await
                .expect_err("a connection with direct grants requires explicit inverse cleanup"),
        );
        let mut subject_delete_facts = Vec::new();
        for (table_name, subject_id) in [
            ("users", operator_id),
            ("agents", agent_id),
            ("contacts", contact_id),
        ] {
            let statement = format!("DELETE FROM {table_name} WHERE id = $1");
            let error = sqlx::query(&statement)
                .bind(subject_id)
                .execute(&target)
                .await
                .expect_err("a subject with a direct grant requires explicit inverse cleanup");
            subject_delete_facts.push((table_name.to_string(), database_error_fact(error)));
        }

        let table_contract: (String, bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT owner.rolname, relation.relrowsecurity, relation.relforcerowsecurity, \
                    has_table_privilege('moa_app', relation.oid, 'SELECT'), \
                    has_table_privilege('moa_app', relation.oid, 'INSERT'), \
                    has_table_privilege('moa_app', relation.oid, 'DELETE') \
             FROM pg_class AS relation \
             JOIN pg_roles AS owner ON owner.oid = relation.relowner \
             WHERE relation.oid = 'moa.connector_connection_use_grants'::REGCLASS",
        )
        .fetch_one(&target)
        .await?;
        let app_may_update: bool = sqlx::query_scalar(
            "SELECT has_table_privilege( \
                'moa_app', 'moa.connector_connection_use_grants', 'UPDATE')",
        )
        .fetch_one(&target)
        .await?;
        let identity_table_reads: (bool, bool) = sqlx::query_as(
            "SELECT has_table_privilege('moa_app', 'public.users', 'SELECT'), \
                    has_table_privilege('moa_app', 'public.agents', 'SELECT')",
        )
        .fetch_one(&target)
        .await?;
        let subject_validation_execute: (bool, bool) = sqlx::query_as(
            "SELECT has_function_privilege(\
                        'moa_app', \
                        'moa.connector_use_subject_exists(uuid,text,uuid)', \
                        'EXECUTE'), \
                    has_function_privilege(\
                        'moa_app', \
                        'moa.connector_use_subject_is_eligible(uuid,text,uuid)', \
                        'EXECUTE')",
        )
        .fetch_one(&target)
        .await?;
        let policy_names: Vec<String> = sqlx::query_scalar(
            "SELECT policyname::TEXT FROM pg_policies \
             WHERE schemaname = 'moa' \
               AND tablename = 'connector_connection_use_grants' ORDER BY policyname",
        )
        .fetch_all(&target)
        .await?;
        let foreign_keys: Vec<(String, String)> = sqlx::query_as(
            "SELECT conname::TEXT, confdeltype::TEXT FROM pg_constraint \
             WHERE conrelid = 'moa.connector_connection_use_grants'::REGCLASS \
               AND contype = 'f' ORDER BY conname",
        )
        .fetch_all(&target)
        .await?;
        let purge_stages: Vec<(i16, String)> = sqlx::query_as(
            "SELECT stage_order, stage_name FROM moa.tenant_purge_catalog \
             WHERE stage_name IN ( \
                'moa.connector_action_invocations', \
                'moa.connector_action_bindings', \
                'moa.connector_connection_use_grants', \
                'moa.connector_connections', \
                'public.contacts', 'public.agents', 'public.users' \
             ) ORDER BY stage_order",
        )
        .fetch_all(&target)
        .await?;
        let purge_definition: String = sqlx::query_scalar(
            "SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)",
        )
        .fetch_one(&target)
        .await?;

        let mut scoped = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *scoped)
            .await?;
        sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *scoped)
            .await?;
        let visible_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM moa.connector_connection_use_grants")
                .fetch_one(&mut *scoped)
                .await?;
        let neighbour_visible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.connector_connection_use_grants \
             WHERE connection_uid = $1",
        )
        .bind(neighbour_connection)
        .fetch_one(&mut *scoped)
        .await?;
        let mut subject_validation_facts = Vec::new();
        for (kind, subject_id) in [
            ("operator", operator_id),
            ("agent", agent_id),
            ("contact", contact_id),
        ] {
            let fact: (bool, bool) = sqlx::query_as(
                "SELECT moa.connector_use_subject_exists($1, $2, $3), \
                        moa.connector_use_subject_is_eligible($1, $2, $3)",
            )
            .bind(tenant_id)
            .bind(kind)
            .bind(subject_id)
            .fetch_one(&mut *scoped)
            .await?;
            subject_validation_facts.push((kind.to_string(), fact));
        }
        let cross_tenant_subject_probe: (bool, bool) = sqlx::query_as(
            "SELECT moa.connector_use_subject_exists($1, 'operator', $2), \
                    moa.connector_use_subject_is_eligible($1, 'operator', $2)",
        )
        .bind(neighbour_tenant)
        .bind(neighbour_operator)
        .fetch_one(&mut *scoped)
        .await?;
        let cross_rls_fact = database_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_connection_use_grants \
                    (tenant_id, connection_uid, subject_kind, subject_id) \
                 VALUES ($1, $2, 'operator', $3)",
            )
            .bind(neighbour_tenant)
            .bind(neighbour_connection)
            .bind(neighbour_operator)
            .execute(&mut *scoped)
            .await
            .expect_err("moa_app cannot write a neighboring tenant grant"),
        );
        scoped.rollback().await?;

        let mut missing_scope = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *missing_scope)
            .await?;
        let missing_scope_visible: i64 =
            sqlx::query_scalar("SELECT count(*) FROM moa.connector_connection_use_grants")
                .fetch_one(&mut *missing_scope)
                .await?;
        missing_scope.rollback().await?;

        for (operation_id, operation) in [
            ("v51-stage-operation", "stage"),
            ("v51-activate-operation", "activate"),
        ] {
            sqlx::query(
                "INSERT INTO tenant_credential_operations \
                    (tenant_id, operation_id, request_hash, operation, \
                     expected_prior_credential_uid, principal_kind, principal_id, outcome) \
                 VALUES ($1, $2, repeat('a', 64), $3, $4, 'caller', $5, 'succeeded')",
            )
            .bind(tenant_id)
            .bind(operation_id)
            .bind(operation)
            .bind(expected_prior)
            .bind(operator_id)
            .execute(&target)
            .await?;
        }
        let staged_operation_facts: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT operation, expected_prior_credential_uid \
             FROM tenant_credential_operations \
             WHERE operation_id LIKE 'v51-%-operation' ORDER BY operation",
        )
        .fetch_all(&target)
        .await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            tenant_mismatch_facts,
            connection_mismatch_fact,
            invalid_kind_fact,
            duplicate_fact,
            connection_delete_fact,
            subject_delete_facts,
            table_contract,
            app_may_update,
            identity_table_reads,
            subject_validation_execute,
            policy_names,
            foreign_keys,
            purge_stages,
            purge_definition,
            visible_count,
            neighbour_visible,
            cross_rls_fact,
            subject_validation_facts,
            cross_tenant_subject_probe,
            missing_scope_visible,
            staged_operation_facts,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (
        first,
        second,
        tenant_mismatch_facts,
        connection_mismatch_fact,
        invalid_kind_fact,
        duplicate_fact,
        connection_delete_fact,
        subject_delete_facts,
        table_contract,
        app_may_update,
        identity_table_reads,
        subject_validation_execute,
        policy_names,
        foreign_keys,
        purge_stages,
        purge_definition,
        visible_count,
        neighbour_visible,
        cross_rls_fact,
        subject_validation_facts,
        cross_tenant_subject_probe,
        missing_scope_visible,
        staged_operation_facts,
    ) = outcome.expect("connector-use-grant V51 assertions should complete");

    assert_eq!(
        first,
        expected_migration_labels_from("connector_connection_use_grants")
    );
    assert!(second.is_empty(), "V51 replay must be a no-op: {second:?}");
    assert_eq!(
        tenant_mismatch_facts,
        vec![
            (
                "operator".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_operator_fk".to_string()),
                ),
            ),
            (
                "agent".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_agent_fk".to_string()),
                ),
            ),
            (
                "contact".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_contact_fk".to_string()),
                ),
            ),
        ]
    );
    assert_eq!(
        connection_mismatch_fact,
        (
            Some("23503".to_string()),
            Some("connector_connection_use_grants_connection_fk".to_string()),
        )
    );
    assert_eq!(
        invalid_kind_fact,
        (
            Some("23514".to_string()),
            Some("connector_connection_use_grants_subject_kind_valid".to_string()),
        )
    );
    assert_eq!(
        duplicate_fact,
        (
            Some("23505".to_string()),
            Some("connector_connection_use_grants_desired_state_key".to_string()),
        )
    );
    assert_eq!(connection_delete_fact, connection_mismatch_fact);
    assert_eq!(
        subject_delete_facts,
        vec![
            (
                "users".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_operator_fk".to_string()),
                ),
            ),
            (
                "agents".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_agent_fk".to_string()),
                ),
            ),
            (
                "contacts".to_string(),
                (
                    Some("23503".to_string()),
                    Some("connector_connection_use_grants_contact_fk".to_string()),
                ),
            ),
        ]
    );
    assert_eq!(
        table_contract,
        ("moa_owner".to_string(), true, true, true, true, true)
    );
    assert!(
        !app_may_update,
        "direct-use grants are immutable desired state"
    );
    assert_eq!(
        identity_table_reads,
        (false, false),
        "the runtime role must not receive broad identity-table reads"
    );
    assert_eq!(subject_validation_execute, (true, true));
    assert_eq!(policy_names, vec!["tenant_isolation".to_string()]);
    assert_eq!(
        foreign_keys,
        vec![
            (
                "connector_connection_use_grants_agent_fk".to_string(),
                "a".to_string(),
            ),
            (
                "connector_connection_use_grants_connection_fk".to_string(),
                "a".to_string(),
            ),
            (
                "connector_connection_use_grants_contact_fk".to_string(),
                "a".to_string(),
            ),
            (
                "connector_connection_use_grants_operator_fk".to_string(),
                "a".to_string(),
            ),
        ],
        "every registry parent must use NO ACTION so inverse rows cannot disappear by cascade"
    );
    assert_eq!(
        purge_stages,
        vec![
            (26, "moa.connector_action_invocations".to_string()),
            (27, "moa.connector_action_bindings".to_string()),
            (28, "moa.connector_connection_use_grants".to_string()),
            (29, "moa.connector_connections".to_string()),
            (57, "public.contacts".to_string()),
            (66, "public.agents".to_string()),
            (69, "public.users".to_string()),
        ]
    );
    assert!(purge_definition.contains("catalog_count <> 131"));
    assert!(purge_definition.contains("exactly 131 tables"));
    assert_eq!(visible_count, 3);
    assert_eq!(neighbour_visible, 0);
    assert_eq!(cross_rls_fact.0.as_deref(), Some("42501"));
    assert_eq!(
        subject_validation_facts,
        vec![
            ("operator".to_string(), (true, true)),
            ("agent".to_string(), (true, true)),
            ("contact".to_string(), (true, true)),
        ]
    );
    assert_eq!(
        cross_tenant_subject_probe,
        (false, false),
        "SECURITY DEFINER validation must not become a cross-tenant oracle"
    );
    assert_eq!(missing_scope_visible, 0);
    assert_eq!(
        staged_operation_facts,
        vec![
            ("activate".to_string(), Some(expected_prior)),
            ("stage".to_string(), Some(expected_prior)),
        ]
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_connection_parent_constraint_and_replay_ledgers_are_strict_db() {
    // Pins: V52 performs a closed post-V50 parent catch-up, emits exact tenant
    // authz desired state, installs the tenant-safe parent FK, and persists
    // immutable managed-parent and one-way provider-delete replay state.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let neighbour_tenant = uuid::Uuid::new_v4();
    let nango_connection = uuid::Uuid::new_v4();
    let merge_connection = uuid::Uuid::new_v4();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect V52 migration maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create V52 migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "connector_connection_use_grants").await?;
        let target = PgPoolOptions::new()
            .max_connections(3)
            .connect(&target_url)
            .await?;

        let unknown_connection = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.knowledge_connections \
                (connection_uid, tenant_id, storage_partition_id, provider, \
                 provider_config_key, provider_connection_id, connector, credential_ref, status) \
             VALUES ($1, $2, $2::TEXT, 'unknown', 'config', $1::TEXT, \
                     'unknown', 'vault://unknown', 'active')",
        )
        .bind(unknown_connection)
        .bind(tenant_id)
        .execute(&target)
        .await?;
        let unknown_error =
            apply_through_migration(&target_url, "knowledge_connection_parent_constraint")
                .await
                .expect_err("unknown providers must fail the closed V52 catch-up")
                .to_string();
        sqlx::query("DELETE FROM moa.knowledge_connections WHERE connection_uid = $1")
            .bind(unknown_connection)
            .execute(&target)
            .await?;

        for (connection_uid, provider, connector) in [
            (nango_connection, "nango", "google-drive"),
            (merge_connection, "merge", "sharepoint"),
        ] {
            sqlx::query(
                "INSERT INTO moa.knowledge_connections \
                    (connection_uid, tenant_id, storage_partition_id, provider, \
                     provider_config_key, provider_connection_id, connector, \
                     credential_ref, status, source_selection) \
                 VALUES ($1, $2, $2::TEXT, $3, 'config', $1::TEXT, $4, \
                         'vault://legacy', 'active', '{\"selected\":[]}'::JSONB)",
            )
            .bind(connection_uid)
            .bind(tenant_id)
            .bind(provider)
            .bind(connector)
            .execute(&target)
            .await?;
        }
        sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, built_in_key, built_in_version, \
                 non_secret_config, lifecycle_status, health_status) \
             VALUES ($1, $2, 'sharepoint', 'knowledge:nango', 1, \
                     jsonb_build_object( \
                        'provider_config_key', 'config', \
                        'provider_connection_id', $1::TEXT, \
                        'connector', 'sharepoint'), 'active', 'ready')",
        )
        .bind(merge_connection)
        .bind(tenant_id)
        .execute(&target)
        .await?;
        let incompatible_error =
            apply_through_migration(&target_url, "knowledge_connection_parent_constraint")
                .await
                .expect_err("an incompatible pre-existing parent must fail closed")
                .to_string();
        sqlx::query(
            "UPDATE moa.connector_connections SET built_in_key = 'knowledge:merge' \
             WHERE connection_uid = $1",
        )
        .bind(merge_connection)
        .execute(&target)
        .await?;

        sqlx::query(
            "INSERT INTO public.authz_outbox \
                (op, tuple_user, tuple_relation, tuple_object, model_version, \
                 generation, status, attempts, tenant_id) \
             VALUES ('delete', $1, 'tenant', $2, 6, 7, 'dead_letter', 4, $3)",
        )
        .bind(format!("tenant:{tenant_id}"))
        .bind(format!("connector_connection:{merge_connection}"))
        .bind(tenant_id)
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.knowledge_link_claims \
                (tenant_id, operation_id, request_hash, owner_identity_id, \
                 connection_uid, candidate_credential_ref, state, sync_run_uid) \
             VALUES ($1, 'legacy-finalized', repeat('a', 64), $2, $3, \
                     'vault://legacy-candidate', 'finalized', $4)",
        )
        .bind(tenant_id)
        .bind(uuid::Uuid::new_v4())
        .bind(nango_connection)
        .bind(uuid::Uuid::new_v4())
        .execute(&target)
        .await?;

        let first =
            apply_through_migration(&target_url, "knowledge_connection_parent_constraint").await?;
        let second =
            apply_through_migration(&target_url, "knowledge_connection_parent_constraint").await?;

        let credential_projection_columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT table_name::TEXT, column_name::TEXT \
             FROM information_schema.columns \
             WHERE table_schema = 'moa' AND ( \
                (table_name = 'knowledge_connections' \
                 AND column_name IN ('credential_ref', 'status')) \
                OR (table_name = 'knowledge_link_claims' \
                    AND column_name IN ( \
                        'previous_credential_ref', \
                        'candidate_projection_credential_ref', \
                        'candidate_credential_ref', \
                        'previous_vault_credential_ref'))) \
             ORDER BY table_name, column_name",
        )
        .fetch_all(&target)
        .await?;

        let parents: Vec<(uuid::Uuid, String, i64, String, String, String)> = sqlx::query_as(
            "SELECT connection_uid, built_in_key, built_in_version, \
                        non_secret_config ->> 'provider_config_key', \
                        non_secret_config ->> 'provider_connection_id', \
                        non_secret_config ->> 'connector' \
                 FROM moa.connector_connections WHERE tenant_id = $1 ORDER BY built_in_key",
        )
        .bind(tenant_id)
        .fetch_all(&target)
        .await?;
        let authz_rows: Vec<AuthzOutboxFact> = sqlx::query_as(
            "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, \
                        generation, status, tenant_id FROM public.authz_outbox \
                 WHERE tuple_object IN ($1, $2) \
                 ORDER BY CASE WHEN tuple_object = $1 THEN 0 ELSE 1 END",
        )
        .bind(format!("connector_connection:{merge_connection}"))
        .bind(format!("connector_connection:{nango_connection}"))
        .fetch_all(&target)
        .await?;
        let legacy_claim: (bool, Option<i64>) = sqlx::query_as(
            "SELECT parent_created_by_claim, credential_expected_generation \
             FROM moa.knowledge_link_claims \
             WHERE tenant_id = $1 AND operation_id = 'legacy-finalized'",
        )
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;

        let missing_parent = uuid::Uuid::new_v4();
        let child_without_parent_fact = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.knowledge_connections \
                    (connection_uid, tenant_id, storage_partition_id, provider, \
                     provider_config_key, provider_connection_id, connector) \
                 VALUES ($1, $2, $2::TEXT, 'nango', 'config', $1::TEXT, 'drive')",
            )
            .bind(missing_parent)
            .bind(tenant_id)
            .execute(&target)
            .await
            .expect_err("a knowledge child without its same-tenant parent must fail"),
        );
        let parent_delete_fact = postgres_error_fact(
            sqlx::query("DELETE FROM moa.connector_connections WHERE connection_uid = $1")
                .bind(nango_connection)
                .execute(&target)
                .await
                .expect_err("the knowledge child must restrict parent deletion"),
        );

        let link_owner = uuid::Uuid::new_v4();
        let null_parent_generation_fact = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.knowledge_link_claims \
                    (tenant_id, operation_id, request_hash, owner_identity_id, \
                     connection_uid, state) \
                 VALUES ($1, 'v52-null-parent-generation', repeat('b', 64), $2, $3, \
                         'parent_claimed')",
            )
            .bind(tenant_id)
            .bind(link_owner)
            .bind(nango_connection)
            .execute(&target)
            .await
            .expect_err("parent_claimed must pin the expected credential generation"),
        );
        sqlx::query(
            "INSERT INTO moa.knowledge_link_claims \
                (tenant_id, operation_id, request_hash, owner_identity_id, connection_uid, \
                 state, parent_created_by_claim, credential_expected_generation) \
             VALUES ($1, 'v52-parent-generation', repeat('c', 64), $2, $3, \
                     'parent_claimed', TRUE, 2)",
        )
        .bind(tenant_id)
        .bind(link_owner)
        .bind(nango_connection)
        .execute(&target)
        .await?;
        let generation_rewrite_fact = postgres_error_fact(
            sqlx::query(
                "UPDATE moa.knowledge_link_claims SET credential_expected_generation = 3 \
                 WHERE tenant_id = $1 AND operation_id = 'v52-parent-generation'",
            )
            .bind(tenant_id)
            .execute(&target)
            .await
            .expect_err("a pinned credential generation must be immutable"),
        );
        let parallel_link_fact = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.knowledge_link_claims \
                    (tenant_id, operation_id, request_hash, owner_identity_id, connection_uid, \
                     state) \
                 VALUES ($1, 'v52-parallel-link', repeat('9', 64), $2, $3, 'reserved')",
            )
            .bind(tenant_id)
            .bind(link_owner)
            .bind(nango_connection)
            .execute(&target)
            .await
            .expect_err("only one nonterminal link claim may own a connection"),
        );
        let nonterminal_index_definition: String = sqlx::query_scalar(
            "SELECT indexdef::TEXT FROM pg_indexes \
             WHERE schemaname = 'moa' \
               AND indexname = 'knowledge_link_claims_one_nonterminal_per_connection'",
        )
        .fetch_one(&target)
        .await?;
        sqlx::query(
            "UPDATE moa.knowledge_link_claims SET state = 'compensated', updated_at = NOW() \
             WHERE tenant_id = $1 AND operation_id = 'v52-parent-generation'",
        )
        .bind(tenant_id)
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.knowledge_link_claims \
                (tenant_id, operation_id, request_hash, owner_identity_id, connection_uid, state) \
             VALUES ($1, 'v52-after-compensated', repeat('8', 64), $2, $3, 'reserved')",
        )
        .bind(tenant_id)
        .bind(link_owner)
        .bind(nango_connection)
        .execute(&target)
        .await?;
        let link_claim_states: Vec<String> = sqlx::query_scalar(
            "SELECT state FROM moa.knowledge_link_claims \
             WHERE tenant_id = $1 AND connection_uid = $2 ORDER BY state",
        )
        .bind(tenant_id)
        .bind(nango_connection)
        .fetch_all(&target)
        .await?;

        let neighbour_connection = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.connector_connections \
                (connection_uid, tenant_id, display_name, built_in_key, built_in_version) \
             VALUES ($1, $2, 'neighbour', 'knowledge:nango', 1)",
        )
        .bind(neighbour_connection)
        .bind(neighbour_tenant)
        .execute(&target)
        .await?;
        for (claim_tenant, operation_id, connection_uid, created) in [
            (tenant_id, "managed-target", merge_connection, false),
            (
                neighbour_tenant,
                "managed-neighbour",
                neighbour_connection,
                true,
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.connector_managed_parent_claims \
                    (tenant_id, operation_id, request_hash, connection_uid, \
                     parent_created_by_claim) \
                 VALUES ($1, $2, repeat('d', 64), $3, $4)",
            )
            .bind(claim_tenant)
            .bind(operation_id)
            .bind(connection_uid)
            .bind(created)
            .execute(&target)
            .await?;
        }
        let managed_rewrite_fact = postgres_error_fact(
            sqlx::query(
                "UPDATE moa.connector_managed_parent_claims \
                 SET parent_created_by_claim = TRUE \
                 WHERE tenant_id = $1 AND operation_id = 'managed-target'",
            )
            .bind(tenant_id)
            .execute(&target)
            .await
            .expect_err("managed parent ownership must be immutable"),
        );
        let managed_parent_delete_fact = postgres_error_fact(
            sqlx::query("DELETE FROM moa.connector_connections WHERE connection_uid = $1")
                .bind(neighbour_connection)
                .execute(&target)
                .await
                .expect_err("managed claims must purge before connector parents"),
        );

        // Use five connections to pin every legal terminal transition and two
        // illegal paths without sharing mutable state between assertions.
        let absent_connection = uuid::Uuid::new_v4();
        let failed_connection = uuid::Uuid::new_v4();
        let illegal_connection = uuid::Uuid::new_v4();
        for connection_uid in [absent_connection, failed_connection, illegal_connection] {
            sqlx::query(
                "INSERT INTO moa.connector_connections \
                    (connection_uid, tenant_id, display_name, built_in_key, built_in_version, \
                     non_secret_config) \
                 VALUES ($1, $2, 'drive', 'knowledge:nango', 1, \
                         jsonb_build_object( \
                            'provider_config_key', 'config', \
                            'provider_connection_id', $1::TEXT, \
                            'connector', 'drive'))",
            )
            .bind(connection_uid)
            .bind(tenant_id)
            .execute(&target)
            .await?;
            sqlx::query(
                "INSERT INTO moa.knowledge_connections \
                    (connection_uid, tenant_id, storage_partition_id, provider, \
                     provider_config_key, provider_connection_id, connector) \
                 VALUES ($1, $2, $2::TEXT, 'nango', 'config', $1::TEXT, 'drive')",
            )
            .bind(connection_uid)
            .bind(tenant_id)
            .execute(&target)
            .await?;
        }
        for (connection_uid, operation_id) in [
            (nango_connection, "disconnect-deleted"),
            (merge_connection, "disconnect-unknown"),
            (absent_connection, "disconnect-absent"),
            (failed_connection, "disconnect-failed"),
            (illegal_connection, "disconnect-illegal"),
        ] {
            sqlx::query(
                "INSERT INTO moa.knowledge_connection_disconnect_progress \
                    (tenant_id, connection_uid, operation_id, request_hash, provider_operation_id) \
                 VALUES ($1, $2, $3, repeat('e', 64), $4)",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .bind(operation_id)
            .bind(uuid::Uuid::new_v4())
            .execute(&target)
            .await?;
        }
        for connection_uid in [nango_connection, merge_connection, absent_connection] {
            sqlx::query(
                "UPDATE moa.knowledge_connection_disconnect_progress \
                 SET state = 'transmitting', updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .execute(&target)
            .await?;
        }
        for (connection_uid, state, error_code) in [
            (nango_connection, "deleted", None),
            (merge_connection, "unknown_outcome", Some("transport_lost")),
            (absent_connection, "already_absent", None),
        ] {
            sqlx::query(
                "UPDATE moa.knowledge_connection_disconnect_progress \
                 SET state = $3, error_code = $4, completed_at = NOW(), updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2",
            )
            .bind(tenant_id)
            .bind(connection_uid)
            .bind(state)
            .bind(error_code)
            .execute(&target)
            .await?;
        }
        sqlx::query(
            "UPDATE moa.knowledge_connection_disconnect_progress \
             SET state = 'failed_before_send', error_code = 'admission_denied', \
                 completed_at = NOW(), updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2",
        )
        .bind(tenant_id)
        .bind(failed_connection)
        .execute(&target)
        .await?;
        let illegal_transition_fact = postgres_error_fact(
            sqlx::query(
                "UPDATE moa.knowledge_connection_disconnect_progress \
                 SET state = 'deleted', completed_at = NOW(), updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2",
            )
            .bind(tenant_id)
            .bind(illegal_connection)
            .execute(&target)
            .await
            .expect_err("reserved disconnect cannot skip transmitting"),
        );
        let terminal_rewrite_fact = postgres_error_fact(
            sqlx::query(
                "UPDATE moa.knowledge_connection_disconnect_progress \
                 SET state = 'already_absent' \
                 WHERE tenant_id = $1 AND connection_uid = $2",
            )
            .bind(tenant_id)
            .bind(merge_connection)
            .execute(&target)
            .await
            .expect_err("unknown outcome is terminal for automatic retry"),
        );
        let invalid_error_code_fact = postgres_error_fact(
            sqlx::query(
                "UPDATE moa.knowledge_connection_disconnect_progress \
                 SET state = 'failed_before_send', error_code = 'raw provider body', \
                     completed_at = NOW(), updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2",
            )
            .bind(tenant_id)
            .bind(illegal_connection)
            .execute(&target)
            .await
            .expect_err("only typed bounded error codes may persist"),
        );
        let disconnect_states: Vec<(String, String)> = sqlx::query_as(
            "SELECT operation_id, state \
             FROM moa.knowledge_connection_disconnect_progress \
             WHERE tenant_id = $1 ORDER BY operation_id",
        )
        .bind(tenant_id)
        .fetch_all(&target)
        .await?;

        let table_contracts: Vec<TableContractFact> = sqlx::query_as(
            "SELECT relation.relname::TEXT, owner.rolname::TEXT, \
                        relation.relrowsecurity, relation.relforcerowsecurity, \
                        has_table_privilege('moa_app', relation.oid, 'SELECT'), \
                        has_table_privilege('moa_app', relation.oid, 'INSERT'), \
                        has_table_privilege('moa_app', relation.oid, 'UPDATE'), \
                        has_table_privilege('moa_app', relation.oid, 'DELETE') \
                 FROM pg_class AS relation \
                 JOIN pg_roles AS owner ON owner.oid = relation.relowner \
                 WHERE relation.oid IN ( \
                    'moa.connector_managed_parent_claims'::REGCLASS, \
                    'moa.knowledge_connection_disconnect_progress'::REGCLASS) \
                 ORDER BY relation.relname",
        )
        .fetch_all(&target)
        .await?;
        let policies: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::TEXT || ':' || policyname::TEXT \
             FROM pg_policies WHERE schemaname = 'moa' \
               AND tablename IN ( \
                    'connector_managed_parent_claims', \
                    'knowledge_connection_disconnect_progress') ORDER BY 1",
        )
        .fetch_all(&target)
        .await?;
        let fences: Vec<String> = sqlx::query_scalar(
            "SELECT relation.relname::TEXT || ':' || trigger.tgname::TEXT \
             FROM pg_trigger AS trigger \
             JOIN pg_class AS relation ON relation.oid = trigger.tgrelid \
             WHERE NOT trigger.tgisinternal \
               AND relation.relname IN ( \
                    'connector_managed_parent_claims', \
                    'knowledge_connection_disconnect_progress') \
               AND trigger.tgname LIKE 'moa_tenant_purge_fence_%' ORDER BY 1",
        )
        .fetch_all(&target)
        .await?;
        let purge_stages: Vec<(i16, String)> = sqlx::query_as(
            "SELECT stage_order, stage_name FROM moa.tenant_purge_catalog \
             WHERE stage_name IN ( \
                'moa.knowledge_sync_runs', \
                'moa.knowledge_connection_disconnect_progress', \
                'moa.knowledge_connections', \
                'moa.connector_action_invocations', \
                'moa.connector_action_bindings', \
                'moa.connector_connection_use_grants', \
                'moa.connector_managed_parent_claims', \
                'moa.connector_connections') ORDER BY stage_order",
        )
        .fetch_all(&target)
        .await?;
        let purge_count: i64 = sqlx::query_scalar("SELECT count(*) FROM moa.tenant_purge_catalog")
            .fetch_one(&target)
            .await?;
        let purge_definition: String = sqlx::query_scalar(
            "SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)",
        )
        .fetch_one(&target)
        .await?;

        let mut scoped = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *scoped)
            .await?;
        sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
            .bind(tenant_id.to_string())
            .execute(&mut *scoped)
            .await?;
        let visible: (i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.connector_managed_parent_claims), \
                (SELECT count(*) FROM moa.knowledge_connection_disconnect_progress)",
        )
        .fetch_one(&mut *scoped)
        .await?;
        let neighbour_visible: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.connector_managed_parent_claims \
             WHERE connection_uid = $1",
        )
        .bind(neighbour_connection)
        .fetch_one(&mut *scoped)
        .await?;
        let cross_tenant_fact = postgres_error_fact(
            sqlx::query(
                "INSERT INTO moa.connector_managed_parent_claims \
                    (tenant_id, operation_id, request_hash, connection_uid, \
                     parent_created_by_claim) \
                 VALUES ($1, 'cross-tenant', repeat('f', 64), $2, FALSE)",
            )
            .bind(neighbour_tenant)
            .bind(neighbour_connection)
            .execute(&mut *scoped)
            .await
            .expect_err("moa_app cannot write a neighboring tenant claim"),
        );
        scoped.rollback().await?;

        let mut missing_scope = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *missing_scope)
            .await?;
        let missing_scope_visible: (i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.connector_managed_parent_claims), \
                (SELECT count(*) FROM moa.knowledge_connection_disconnect_progress)",
        )
        .fetch_one(&mut *missing_scope)
        .await?;
        missing_scope.rollback().await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            unknown_error,
            incompatible_error,
            first,
            second,
            parents,
            authz_rows,
            legacy_claim,
            credential_projection_columns,
            child_without_parent_fact,
            parent_delete_fact,
            null_parent_generation_fact,
            generation_rewrite_fact,
            parallel_link_fact,
            nonterminal_index_definition,
            link_claim_states,
            managed_rewrite_fact,
            managed_parent_delete_fact,
            illegal_transition_fact,
            terminal_rewrite_fact,
            invalid_error_code_fact,
            disconnect_states,
            table_contracts,
            policies,
            fences,
            purge_stages,
            purge_count,
            purge_definition,
            visible,
            neighbour_visible,
            cross_tenant_fact,
            missing_scope_visible,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        unknown_error,
        incompatible_error,
        first,
        second,
        parents,
        authz_rows,
        legacy_claim,
        credential_projection_columns,
        child_without_parent_fact,
        parent_delete_fact,
        null_parent_generation_fact,
        generation_rewrite_fact,
        parallel_link_fact,
        nonterminal_index_definition,
        link_claim_states,
        managed_rewrite_fact,
        managed_parent_delete_fact,
        illegal_transition_fact,
        terminal_rewrite_fact,
        invalid_error_code_fact,
        disconnect_states,
        table_contracts,
        policies,
        fences,
        purge_stages,
        purge_count,
        purge_definition,
        visible,
        neighbour_visible,
        cross_tenant_fact,
        missing_scope_visible,
    ) = outcome.expect("V52 migration assertions should complete");

    assert!(unknown_error.contains("no closed connector parent mapping"));
    assert!(incompatible_error.contains("incompatible connector parent"));
    assert_eq!(
        first,
        expected_migration_labels_from("knowledge_connection_parent_constraint")
    );
    assert!(second.is_empty(), "V52 replay must be a no-op: {second:?}");
    assert_eq!(
        parents,
        vec![
            (
                merge_connection,
                "knowledge:merge".to_string(),
                1,
                "config".to_string(),
                merge_connection.to_string(),
                "sharepoint".to_string(),
            ),
            (
                nango_connection,
                "knowledge:nango".to_string(),
                1,
                "config".to_string(),
                nango_connection.to_string(),
                "google-drive".to_string(),
            ),
        ]
    );
    assert_eq!(authz_rows.len(), 2);
    assert_eq!(authz_rows[0].0, "write");
    assert_eq!(authz_rows[0].1, format!("tenant:{tenant_id}"));
    assert_eq!(authz_rows[0].2, "tenant");
    assert_eq!(authz_rows[0].4, 6);
    assert_eq!(authz_rows[0].5, 8);
    assert_eq!(authz_rows[0].6, "pending");
    assert_eq!(authz_rows[0].7, tenant_id);
    assert_eq!(authz_rows[1].0, "write");
    assert_eq!(authz_rows[1].1, format!("tenant:{tenant_id}"));
    assert_eq!(authz_rows[1].2, "tenant");
    assert_eq!(authz_rows[1].4, 6);
    assert_eq!(authz_rows[1].5, 1);
    assert_eq!(authz_rows[1].6, "pending");
    assert_eq!(authz_rows[1].7, tenant_id);
    assert_eq!(legacy_claim, (false, None));
    assert_eq!(
        credential_projection_columns,
        vec![
            (
                "knowledge_link_claims".to_string(),
                "candidate_credential_ref".to_string(),
            ),
            (
                "knowledge_link_claims".to_string(),
                "previous_vault_credential_ref".to_string(),
            ),
        ],
        "the final knowledge child must have no lifecycle/credential columns and link claims must retain only vault receipts",
    );
    assert_eq!(
        child_without_parent_fact,
        (
            Some("23503".to_string()),
            Some("knowledge_connections_connector_parent_fk".to_string()),
        )
    );
    assert_eq!(parent_delete_fact, child_without_parent_fact);
    assert_eq!(
        null_parent_generation_fact,
        (
            Some("23514".to_string()),
            Some("knowledge_link_claims_parent_generation_recorded".to_string()),
        )
    );
    assert_eq!(
        generation_rewrite_fact,
        (
            Some("23514".to_string()),
            Some("knowledge_link_claims_credential_generation_immutable".to_string()),
        )
    );
    assert_eq!(
        parallel_link_fact,
        (
            Some("23505".to_string()),
            Some("knowledge_link_claims_one_nonterminal_per_connection".to_string()),
        )
    );
    assert!(
        nonterminal_index_definition.starts_with(
            "CREATE UNIQUE INDEX knowledge_link_claims_one_nonterminal_per_connection"
        )
    );
    assert!(
        nonterminal_index_definition.contains("(tenant_id, connection_uid)")
            && nonterminal_index_definition.contains("finalized")
            && nonterminal_index_definition.contains("compensated"),
        "nonterminal link claim predicate drifted: {nonterminal_index_definition}"
    );
    assert_eq!(
        link_claim_states,
        vec![
            "compensated".to_string(),
            "finalized".to_string(),
            "reserved".to_string(),
        ],
        "both terminal states must release the connection while exactly one new claim remains nonterminal"
    );
    assert_eq!(
        managed_rewrite_fact,
        (
            Some("23514".to_string()),
            Some("connector_managed_parent_claims_immutable".to_string()),
        )
    );
    assert_eq!(
        managed_parent_delete_fact,
        (
            Some("23503".to_string()),
            Some("connector_managed_parent_claims_connection_fk".to_string()),
        )
    );
    assert_eq!(
        illegal_transition_fact,
        (
            Some("23514".to_string()),
            Some("knowledge_connection_disconnect_progress_transition_valid".to_string()),
        )
    );
    assert_eq!(
        terminal_rewrite_fact,
        (
            Some("23514".to_string()),
            Some("knowledge_connection_disconnect_progress_terminal_immutable".to_string()),
        )
    );
    assert_eq!(
        invalid_error_code_fact,
        (
            Some("23514".to_string()),
            Some("knowledge_connection_disconnect_progress_error_code_valid".to_string()),
        )
    );
    assert_eq!(
        disconnect_states,
        vec![
            (
                "disconnect-absent".to_string(),
                "already_absent".to_string()
            ),
            ("disconnect-deleted".to_string(), "deleted".to_string()),
            (
                "disconnect-failed".to_string(),
                "failed_before_send".to_string()
            ),
            ("disconnect-illegal".to_string(), "reserved".to_string()),
            (
                "disconnect-unknown".to_string(),
                "unknown_outcome".to_string()
            ),
        ]
    );
    assert_eq!(
        table_contracts,
        vec![
            (
                "connector_managed_parent_claims".to_string(),
                "moa_owner".to_string(),
                true,
                true,
                true,
                true,
                false,
                false,
            ),
            (
                "knowledge_connection_disconnect_progress".to_string(),
                "moa_owner".to_string(),
                true,
                true,
                true,
                true,
                true,
                false,
            ),
        ]
    );
    assert_eq!(
        policies,
        vec![
            "connector_managed_parent_claims:tenant_isolation".to_string(),
            "knowledge_connection_disconnect_progress:tenant_isolation".to_string(),
        ]
    );
    assert_eq!(
        fences,
        vec![
            "connector_managed_parent_claims:moa_tenant_purge_fence_insert".to_string(),
            "connector_managed_parent_claims:moa_tenant_purge_fence_update".to_string(),
            "knowledge_connection_disconnect_progress:moa_tenant_purge_fence_insert".to_string(),
            "knowledge_connection_disconnect_progress:moa_tenant_purge_fence_update".to_string(),
        ]
    );
    assert_eq!(
        purge_stages,
        vec![
            (24, "moa.knowledge_sync_runs".to_string()),
            (
                25,
                "moa.knowledge_connection_disconnect_progress".to_string()
            ),
            (26, "moa.knowledge_connections".to_string()),
            (27, "moa.connector_action_invocations".to_string()),
            (28, "moa.connector_action_bindings".to_string()),
            (29, "moa.connector_connection_use_grants".to_string()),
            (30, "moa.connector_managed_parent_claims".to_string()),
            (31, "moa.connector_connections".to_string()),
        ]
    );
    assert_eq!(purge_count, 134);
    assert!(purge_definition.contains("catalog_count <> 134"));
    assert!(purge_definition.contains("exactly 134 tables"));
    assert_eq!(visible, (1, 5));
    assert_eq!(neighbour_visible, 0);
    assert_eq!(cross_tenant_fact.0.as_deref(), Some("42501"));
    assert_eq!(missing_scope_visible, (0, 0));
}

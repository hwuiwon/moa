//! Clean-apply and idempotency coverage for the central refinery migration runner.
//!
//! These run against a throwaway database created on the configured Postgres
//! instance, so the assertions are independent of any checksum/version drift in
//! the shared central schema. Requires a superuser-capable `MOA_DATABASE_URL`
//! (the local dev `moa_owner`) able to `CREATE DATABASE` and `CREATE EXTENSION`.

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};

mod embedded_for_cutover_proof {
    use refinery::embed_migrations;

    embed_migrations!("migrations/postgres");
}

/// Default Docker Compose Postgres URL used by local MOA tests.
const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Serializes cluster-global role DDL across throwaway databases and test processes.
const CLUSTER_CATALOG_TEST_LOCK_ID: i64 = 0x4d4f_415f_5445_5354;

const RETIRED_INDEXES: [&str; 10] = [
    "public.idx_events_tenant_session",
    "analytics.score_run_partition_identity_idx",
    "moa.knowledge_source_acl_entries_lookup_idx",
    "public.idx_task_segments_session",
    "public.idx_experience_attributions_experience",
    "public.idx_users_tenant_email_unique",
    "moa.idx_hand_leases_session_worker",
    "public.idx_token_vault_connections_expires",
    "public.idx_token_vault_connections_refresh_lease",
    "public.token_vault_connections_tenant_user_conn_key",
];

const RETAINED_INDEXES: [&str; 10] = [
    "public.events_session_id_sequence_num_key",
    "analytics.score_run_id_partition_key",
    "moa.knowledge_source_acl_entries_uniq",
    "public.task_segments_session_id_segment_index_key",
    "public.experience_attributions_experience_id_subject_type_subject__key",
    "public.idx_users_tenant_email_lower_unique",
    "moa.hand_leases_pkey",
    "public.idx_token_vault_connections_user",
    "public.idx_token_vault_connections_tenant",
    "public.token_vault_connections_pkey",
];

const PRIVACY_AUDITOR_TABLES: [&str; 14] = [
    "public.contacts",
    "moa.edge_index",
    "public.sessions",
    "public.task_segments",
    "public.experience_records",
    "public.experience_attributions",
    "public.learning_candidates",
    "public.learning_candidate_source",
    "public.learning_candidate_decision",
    "public.learning_log",
    "public.learning_log_source",
    "moa.artifact_revision_contribution",
    "moa.artifact_suite_contribution",
    "moa.privacy_erasure_record_decision",
];

const FINAL_AUDITOR_GRANT_TABLES: [&str; 24] = [
    "moa.artifact",
    "moa.artifact_file",
    "moa.artifact_revision",
    "moa.artifact_revision_contribution",
    "moa.artifact_suite_contribution",
    "moa.audit_jti_used",
    "moa.edge_index",
    "moa.embeddings",
    "moa.erasure_jobs",
    "moa.graph_changelog",
    "moa.node_index",
    "moa.privacy_erasure_record_decision",
    "moa.tenant_purge_catalog",
    "moa.tenant_purge_operations",
    "public.contacts",
    "public.experience_attributions",
    "public.experience_records",
    "public.learning_candidate_decision",
    "public.learning_candidate_source",
    "public.learning_candidates",
    "public.learning_log",
    "public.learning_log_source",
    "public.sessions",
    "public.task_segments",
];

const FINAL_AUDITOR_POLICY_TABLES: [&str; 20] = [
    "moa.artifact",
    "moa.artifact_file",
    "moa.artifact_revision",
    "moa.artifact_revision_contribution",
    "moa.artifact_suite_contribution",
    "moa.edge_index",
    "moa.embeddings",
    "moa.graph_changelog",
    "moa.node_index",
    "moa.privacy_erasure_record_decision",
    "public.contacts",
    "public.experience_attributions",
    "public.experience_records",
    "public.learning_candidate_decision",
    "public.learning_candidate_source",
    "public.learning_candidates",
    "public.learning_log",
    "public.learning_log_source",
    "public.sessions",
    "public.task_segments",
];

const TENANT_PURGE_SCOPE_INDEXES: [(&str, &str, &str, &str, Option<&str>); 19] = [
    (
        "moa",
        "tenant_purge_dual_control_request_idx",
        "dual_control_request",
        "tenant_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_knowledge_contact_group_memberships_idx",
        "knowledge_contact_group_memberships",
        "tenant_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_knowledge_source_acl_entries_idx",
        "knowledge_source_acl_entries",
        "tenant_id",
        None,
    ),
    (
        "public",
        "tenant_purge_builtin_pending_approvals_idx",
        "builtin_pending_approvals",
        "tenant_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_execution_action_review_outbox_idx",
        "execution_action_review_outbox",
        "tenant_id",
        None,
    ),
    (
        "public",
        "tenant_purge_contact_verification_challenges_idx",
        "contact_verification_challenges",
        "tenant_id",
        None,
    ),
    (
        "public",
        "tenant_purge_password_reset_tokens_idx",
        "password_reset_tokens",
        "tenant_id",
        None,
    ),
    (
        "public",
        "tenant_purge_user_session_tokens_idx",
        "user_session_tokens",
        "tenant_id",
        None,
    ),
    (
        "public",
        "tenant_purge_auth0_user_map_idx",
        "auth0_user_map",
        "tenant_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_artifact_suite_contribution_idx",
        "artifact_suite_contribution",
        "storage_partition_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_artifact_revision_contribution_idx",
        "artifact_revision_contribution",
        "storage_partition_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_artifact_release_eval_overlay_idx",
        "artifact_release_eval_overlay",
        "storage_partition_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_artifact_release_case_pack_idx",
        "artifact_release_case_pack",
        "storage_partition_id",
        Some("storage_partition_id IS NOT NULL"),
    ),
    (
        "moa",
        "tenant_purge_artifact_activation_attestation_idx",
        "artifact_activation_attestation",
        "storage_partition_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_artifact_release_policy_idx",
        "artifact_release_policy",
        "storage_partition_id",
        Some("storage_partition_id IS NOT NULL"),
    ),
    (
        "moa",
        "tenant_purge_artifact_idx",
        "artifact",
        "storage_partition_id",
        Some("storage_partition_id IS NOT NULL"),
    ),
    (
        "moa",
        "tenant_purge_artifact_revision_idx",
        "artifact_revision",
        "storage_partition_id",
        Some("storage_partition_id IS NOT NULL"),
    ),
    (
        "moa",
        "tenant_purge_embeddings_idx",
        "embeddings",
        "tenant_id",
        None,
    ),
    (
        "moa",
        "tenant_purge_legal_hold_idx",
        "legal_hold",
        "tenant_id",
        Some("released_at IS NOT NULL"),
    ),
];

/// Current migration ownership inventory.
const MIGRATION_OWNERSHIP: &str = include_str!("../migration-ownership.toml");

fn removed_serialized_value(parts: &[&str]) -> String {
    parts.concat()
}

/// Returns the Postgres URL used by integration tests, mirroring the runtime
/// `MOA_DATABASE_URL` setting and falling back to the compose default.
fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Returns a process-and-UUID-unique throwaway database name.
fn unique_db_name() -> String {
    format!(
        "moa_mig_idem_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Rewrites the database name in a Postgres URL, preserving any query string.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base.rsplit_once('/').map_or(base, |(prefix, _)| prefix);
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// Installs the bootstrap extensions docker initdb would provide, then runs the
/// central migrations twice, returning the applied-migration labels from each run.
async fn clean_apply_then_reapply(
    target_url: &str,
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error + Send + Sync>> {
    {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(target_url)
            .await?;
        target
            .execute(
                "CREATE EXTENSION IF NOT EXISTS vector; \
                 CREATE EXTENSION IF NOT EXISTS pgaudit;",
            )
            .await?;
        target.close().await;
    }

    let first = run_reporting_applied_serialized(target_url).await?;
    let second = run_reporting_applied_serialized(target_url).await?;
    Ok((first, second))
}

/// Runs the public migration API while serializing cluster-global role DDL.
async fn run_reporting_applied_serialized(
    target_url: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let catalog_lock = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url())
        .await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(CLUSTER_CATALOG_TEST_LOCK_ID)
        .execute(&catalog_lock)
        .await?;
    let result = moa_migrations::run_reporting_applied(target_url).await;
    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(CLUSTER_CATALOG_TEST_LOCK_ID)
        .execute(&catalog_lock)
        .await;
    catalog_lock.close().await;
    unlock_result?;
    Ok(result?)
}

/// Returns the exact embedded migration labels in version order.
fn expected_migration_labels() -> Vec<String> {
    let mut migrations = embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .iter()
        .map(|migration| (migration.version(), migration.to_string()))
        .collect::<Vec<_>>();
    migrations.sort_by_key(|(version, _)| *version);
    migrations.into_iter().map(|(_, label)| label).collect()
}

/// Resolves an embedded migration by semantic name.
fn migration_version(migration_name: &str) -> Result<i32, std::io::Error> {
    embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .iter()
        .find(|migration| migration.name() == migration_name)
        .map(|migration| migration.version())
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "embedded migration named {migration_name:?} does not exist"
            ))
        })
}

/// Applies a central migration prefix selected by its semantic migration name.
async fn apply_through_migration(
    target_url: &str,
    migration_name: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let version = migration_version(migration_name)?;
    let mut migrations = embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .clone();
    migrations.sort_by_key(refinery::Migration::version);
    let runner = refinery::Runner::new(&migrations);
    let catalog_lock = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url())
        .await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(CLUSTER_CATALOG_TEST_LOCK_ID)
        .execute(&catalog_lock)
        .await?;
    let (mut client, connection) =
        tokio_postgres::connect(target_url, tokio_postgres::NoTls).await?;
    let connection_task = tokio::spawn(connection);
    let result = runner
        .set_target(refinery::Target::Version(version))
        .run_async(&mut client)
        .await;
    drop(client);
    connection_task.await??;
    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(CLUSTER_CATALOG_TEST_LOCK_ID)
        .execute(&catalog_lock)
        .await;
    catalog_lock.close().await;
    unlock_result?;
    let report = result?;
    Ok(report
        .applied_migrations()
        .iter()
        .map(ToString::to_string)
        .collect())
}

/// Installs the extensions provided by the normal compose bootstrap.
async fn install_required_extensions(
    target_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target = PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url)
        .await?;
    target
        .execute(
            "CREATE EXTENSION IF NOT EXISTS vector; \
             CREATE EXTENSION IF NOT EXISTS pgaudit;",
        )
        .await?;
    target.close().await;
    Ok(())
}

/// Installs a database-local event trigger that records every later DDL start.
async fn install_ddl_sentinel(
    target_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target = PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url)
        .await?;
    sqlx::raw_sql(
        r#"
        CREATE SCHEMA migration_test_sentinel;
        CREATE TABLE migration_test_sentinel.ddl_start (
            ordinal BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            command_tag TEXT NOT NULL
        );
        CREATE FUNCTION migration_test_sentinel.record_ddl_start()
        RETURNS event_trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            INSERT INTO migration_test_sentinel.ddl_start (command_tag)
            VALUES (tg_tag);
        END
        $$;
        CREATE EVENT TRIGGER migration_protocol_no_ddl
            ON ddl_command_start
            EXECUTE FUNCTION migration_test_sentinel.record_ddl_start();
        "#,
    )
    .execute(&target)
    .await?;
    target
        .execute("TRUNCATE migration_test_sentinel.ddl_start")
        .await?;
    target.close().await;
    Ok(())
}

/// Captures a runner rejection and the amount of migration DDL it started.
async fn reset_rejection_and_ddl_count(
    target_url: &str,
) -> Result<(String, i64), Box<dyn std::error::Error + Send + Sync>> {
    install_ddl_sentinel(target_url).await?;
    let error = match moa_migrations::run_reporting_applied(target_url).await {
        Ok(applied) => {
            return Err(std::io::Error::other(format!(
                "invalid migration history unexpectedly applied {applied:?}"
            ))
            .into());
        }
        Err(error) => error,
    };
    let rendered = format!("{error:#}");

    let target = PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url)
        .await?;
    let ddl_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM migration_test_sentinel.ddl_start")
            .fetch_one(&target)
            .await?;
    target.close().await;
    Ok((rendered, ddl_count))
}

/// Asserts the exact destructive-reset failure contract after cleanup.
fn assert_destructive_reset_rejection(
    rendered_error: &str,
    ddl_count: i64,
    expected_error_fragment: &str,
) {
    assert!(
        rendered_error.contains(expected_error_fragment),
        "migration rejection did not identify {expected_error_fragment:?}: {rendered_error}"
    );
    assert!(
        rendered_error.contains("destructively rebuilt or reset"),
        "migration rejection must prescribe the destructive reset boundary: {rendered_error}"
    );
    assert_eq!(
        ddl_count, 0,
        "the history guard must reject before any migration DDL starts"
    );
}

/// Drops a throwaway database only after proving every test connection closed.
async fn drop_database_with_zero_connections(admin: &PgPool, database: &str) {
    let mut active_connections = i64::MAX;
    for _ in 0..50 {
        active_connections = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(database)
        .fetch_one(admin)
        .await
        .expect("inspect throwaway database connections before cleanup");
        if active_connections == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        active_connections, 0,
        "throwaway database {database} still has active test connections after close convergence"
    );
    admin
        .execute(format!("DROP DATABASE \"{database}\"").as_str())
        .await
        .expect("drop disconnected throwaway migration database");
    let still_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(database)
            .fetch_one(admin)
            .await
            .expect("confirm throwaway migration database was dropped");
    assert!(
        !still_exists,
        "throwaway database {database} survived cleanup"
    );
}

/// One deliberately invalid execution-route audit matrix cell.
#[derive(Clone, Copy)]
struct InvalidRouteAuditCell<'a> {
    sequence: i64,
    stage: &'a str,
    decision: &'a str,
    strategy: Option<&'a str>,
    source: &'a str,
    classifier_outcome: &'a str,
    classifier_evidence: bool,
}

/// Proves that PostgreSQL rejects one invalid execution-route audit row at the
/// table boundary with a check-constraint violation.
async fn assert_route_audit_insert_rejected(
    pool: &PgPool,
    cell: InvalidRouteAuditCell<'_>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let error = sqlx::query(
        r#"
        INSERT INTO moa.execution_route_audit (
            audit_uid,tenant_id,contact_id,session_id,originating_sequence,
            stage,decision,strategy,source,classifier_outcome,
            provider_model,prompt_version,objective_hash,response_hash,
            confidence_bps,missing_input_count,input_tokens_uncached,
            input_tokens_cache_write,input_tokens_cache_read,output_tokens,
            cost_microusd,duration_micros,accepted_at,created_at
        )
        SELECT
            moa.execution_route_audit_uid(
                '00000000-0000-0000-0000-000000337001',NULL,
                '00000000-0000-0000-0000-000000337002',$1,$2
            ),
            '00000000-0000-0000-0000-000000337001',NULL,
            '00000000-0000-0000-0000-000000337002',$1,$2,$3,$4,$5,$6,
            CASE WHEN $7 THEN 'route-model' END,
            CASE WHEN $7 THEN 'execution-router' END,
            repeat('a',64),
            CASE WHEN $7 THEN repeat('b',64) END,
            (CASE WHEN $7 THEN 9500 END)::SMALLINT,
            (CASE WHEN $3 = 'needs_input' THEN 1 ELSE 0 END)::SMALLINT,
            (CASE WHEN $7 THEN 1 ELSE 0 END)::BIGINT,
            0::BIGINT,0::BIGINT,0::BIGINT,
            (CASE WHEN $7 THEN 1 ELSE 0 END)::BIGINT,
            (CASE WHEN $7 THEN 1 ELSE 0 END)::BIGINT,
            accepted_at,accepted_at
        FROM (SELECT NOW() AS accepted_at) accepted
        "#,
    )
    .bind(cell.sequence)
    .bind(cell.stage)
    .bind(cell.decision)
    .bind(cell.strategy)
    .bind(cell.source)
    .bind(cell.classifier_outcome)
    .bind(cell.classifier_evidence)
    .execute(pool)
    .await
    .expect_err("invalid route-audit matrix cell must be rejected");
    let sql_state = error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .map(|code| code.into_owned());
    assert_eq!(
        sql_state.as_deref(),
        Some("23514"),
        "invalid route-audit cell must fail its CHECK constraint: {error}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn final_schema_omits_retired_relations_columns_and_indexes_db() {
    // Pins: a pristine database never creates compatibility-only relations,
    // columns, or redundant indexes retired by the contiguous epoch.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let relations: Vec<Option<String>> =
            sqlx::query_scalar("SELECT to_regclass(name)::TEXT FROM unnest($1::TEXT[]) AS name")
                .bind([
                    "moa.knowledge_rebuild_operation",
                    "moa.knowledge_rebuild_generation",
                    "moa.knowledge_active_generation",
                    "moa.knowledge_rebuild_candidate_vector",
                    "moa.knowledge_rechunk_staging",
                    "moa.artifact_run",
                    "moa.artifact_node_run",
                    "public.tenant_mcp_connection_bindings",
                ])
                .fetch_all(&target)
                .await?;
        let reembed_column: Option<String> = sqlx::query_scalar(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = 'moa' AND table_name = 'storage_partition_state' \
             AND column_name = 'reembed_state'",
        )
        .fetch_optional(&target)
        .await?;
        let event_mutation_privileges: (bool, bool, bool) = sqlx::query_as(
            "SELECT \
                has_table_privilege('moa_app', 'public.events', 'UPDATE'), \
                has_table_privilege('moa_app', 'public.events', 'DELETE'), \
                has_table_privilege('moa_app', 'public.events', 'TRUNCATE')",
        )
        .fetch_one(&target)
        .await?;
        let indexes = final_index_catalog(&target).await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            relations,
            reembed_column,
            event_mutation_privileges,
            indexes,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (relations, reembed_column, event_mutation_privileges, indexes) =
        outcome.expect("final-schema retirement assertions should complete");
    assert!(
        relations.iter().all(Option::is_none),
        "rebuild relations remain: {relations:?}"
    );
    assert!(
        reembed_column.is_none(),
        "rebuild write-fence column remains"
    );
    assert_eq!(
        event_mutation_privileges,
        (false, false, false),
        "moa_app must not bypass append-only event storage through table privileges"
    );
    for retired in RETIRED_INDEXES {
        assert!(
            !indexes.contains_key(retired),
            "retired index {retired} must never be created by the pristine epoch"
        );
    }
    for retained in RETAINED_INDEXES {
        let row = indexes
            .get(retained)
            .unwrap_or_else(|| panic!("required final index {retained} is absent"));
        assert!(
            row.is_valid && row.is_ready && row.is_live,
            "required final index {retained} is not usable: {row:?}"
        );
    }
    let attribution_identity = indexes
        .get("public.experience_attributions_experience_id_subject_type_subject__key")
        .expect("experience-attribution identity index must exist");
    assert!(
        attribution_identity.is_unique,
        "experience-attribution identity must remain unique"
    );
    assert_eq!(
        attribution_identity.definition,
        "CREATE UNIQUE INDEX experience_attributions_experience_id_subject_type_subject__key ON public.experience_attributions USING btree (experience_id, subject_type, subject_id)",
        "the retained identity index must cover WHERE experience_id plus the production ORDER BY subject_type, subject_id"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_pristine_apply_is_exact_and_idempotent_db() {
    // Pins: a pristine database applies the exact contiguous V1..V49 epoch,
    // validates as complete, and reports no work on a second public-runner call.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        moa_migrations::validate_complete_history(&target).await?;
        let history: Vec<(i32, String)> = sqlx::query_as(
            "SELECT version, name FROM public.refinery_schema_history ORDER BY version",
        )
        .fetch_all(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, history))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, history) =
        outcome.expect("central migration runs should complete on a fresh database");
    let expected_labels = expected_migration_labels();
    assert_eq!(
        expected_labels.len(),
        49,
        "the epoch must contain exactly 49 migrations"
    );
    assert_eq!(
        first, expected_labels,
        "the pristine apply must be exact and ordered"
    );
    assert_eq!(
        history
            .iter()
            .map(|(version, _)| *version)
            .collect::<Vec<_>>(),
        (1..=49).collect::<Vec<_>>(),
        "refinery history must be exactly contiguous from V1 through V49"
    );
    assert!(
        second.is_empty(),
        "second apply must report no newly applied migrations, got {second:?}"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn baseline_generated_identifiers_are_by_default_identity_db() {
    // Pins: the two baseline-generated identifiers use modern BY DEFAULT
    // identity columns, accept explicit import values, and still generate values
    // when callers omit the identifier.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect identity-catalog maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create identity-catalog throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let identity_catalog: Vec<(String, String, String)> = sqlx::query_as(
            r#"
            SELECT table_schema::TEXT || '.' || table_name::TEXT,
                   column_name::TEXT,
                   identity_generation::TEXT
            FROM information_schema.columns
            WHERE (table_schema, table_name, column_name) IN (
                ('moa', 'graph_changelog', 'change_id'),
                ('moa', 'ingest_dlq', 'dlq_id')
            )
              AND is_identity = 'YES'
            ORDER BY table_schema, table_name, column_name
            "#,
        )
        .fetch_all(&target)
        .await?;
        let sequences: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT pg_get_serial_sequence('moa.graph_changelog', 'change_id'), \
                    pg_get_serial_sequence('moa.ingest_dlq', 'dlq_id')",
        )
        .fetch_one(&target)
        .await?;

        let tenant_id = uuid::Uuid::new_v4();
        let explicit_graph_id = 9_000_001_i64;
        let returned_explicit_graph_id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO moa.graph_changelog (
                change_id, tenant_id, storage_partition_id, actor_kind, op,
                target_kind, target_label, target_uid, payload
            )
            VALUES ($1, $2, $3, 'system', 'create', 'node', 'Fact', $4, '{}'::JSONB)
            RETURNING change_id
            "#,
        )
        .bind(explicit_graph_id)
        .bind(tenant_id)
        .bind(tenant_id.to_string())
        .bind(uuid::Uuid::new_v4())
        .fetch_one(&target)
        .await?;
        let generated_graph_id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO moa.graph_changelog (
                tenant_id, storage_partition_id, actor_kind, op,
                target_kind, target_label, target_uid, payload
            )
            VALUES ($1, $2, 'system', 'create', 'node', 'Fact', $3, '{}'::JSONB)
            RETURNING change_id
            "#,
        )
        .bind(tenant_id)
        .bind(tenant_id.to_string())
        .bind(uuid::Uuid::new_v4())
        .fetch_one(&target)
        .await?;

        let explicit_dlq_id = 9_000_002_i64;
        let returned_explicit_dlq_id: i64 = sqlx::query_scalar(
            "INSERT INTO moa.ingest_dlq \
                (dlq_id, storage_partition_id, tenant_id, payload, error) \
             VALUES ($1, $2, $3, '{}'::JSONB, 'explicit identity test') \
             RETURNING dlq_id",
        )
        .bind(explicit_dlq_id)
        .bind(tenant_id.to_string())
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;
        let generated_dlq_id: i64 = sqlx::query_scalar(
            "INSERT INTO moa.ingest_dlq \
                (storage_partition_id, tenant_id, payload, error) \
             VALUES ($1, $2, '{}'::JSONB, 'generated identity test') \
             RETURNING dlq_id",
        )
        .bind(tenant_id.to_string())
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            identity_catalog,
            sequences,
            returned_explicit_graph_id,
            generated_graph_id,
            returned_explicit_dlq_id,
            generated_dlq_id,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (
        identity_catalog,
        sequences,
        returned_explicit_graph_id,
        generated_graph_id,
        returned_explicit_dlq_id,
        generated_dlq_id,
    ) = outcome.expect("baseline identity assertions should complete");
    assert_eq!(
        identity_catalog,
        vec![
            (
                "moa.graph_changelog".to_string(),
                "change_id".to_string(),
                "BY DEFAULT".to_string(),
            ),
            (
                "moa.ingest_dlq".to_string(),
                "dlq_id".to_string(),
                "BY DEFAULT".to_string(),
            ),
        ],
        "both baseline-generated identifiers must be BY DEFAULT identity columns"
    );
    assert_eq!(
        sequences,
        (
            Some("moa.graph_changelog_change_id_seq".to_string()),
            Some("moa.ingest_dlq_dlq_id_seq".to_string()),
        ),
        "identity columns must retain their stable owned sequence names"
    );
    assert_eq!(returned_explicit_graph_id, 9_000_001);
    assert!(
        generated_graph_id > 0 && generated_graph_id != returned_explicit_graph_id,
        "graph changelog must generate a distinct positive identifier"
    );
    assert_eq!(returned_explicit_dlq_id, 9_000_002);
    assert!(
        generated_dlq_id > 0 && generated_dlq_id != returned_explicit_dlq_id,
        "ingest DLQ must generate a distinct positive identifier"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_parallel_fresh_databases_retry_shared_role_catalog_races_db() {
    // Pins: independent fresh databases in one Postgres cluster can migrate in
    // parallel even though role DDL writes the cluster-global authorization
    // catalog. The runner retries only PostgreSQL's exact concurrent-tuple error.
    let admin_url = test_database_url();
    let first_db = unique_db_name();
    let second_db = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for parallel migration test");
    for db_name in [&first_db, &second_db] {
        admin
            .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
            .await
            .expect("create parallel-migration throwaway database");
    }
    let first_url = with_database(&admin_url, &first_db);
    let second_url = with_database(&admin_url, &second_db);

    let outcome = async {
        install_required_extensions(&first_url).await?;
        install_required_extensions(&second_url).await?;
        let (first, second) = tokio::join!(
            moa_migrations::run_reporting_applied(&first_url),
            moa_migrations::run_reporting_applied(&second_url)
        );
        let first = first?;
        let second = second?;
        let expected = expected_migration_labels();
        assert_eq!(
            first, expected,
            "first database must report the whole epoch"
        );
        assert_eq!(
            second, expected,
            "second database must report the whole epoch"
        );
        for target_url in [&first_url, &second_url] {
            let target = PgPoolOptions::new()
                .max_connections(1)
                .connect(target_url)
                .await?;
            moa_migrations::validate_complete_history(&target).await?;
            target.close().await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;

    drop_database_with_zero_connections(&admin, &first_db).await;
    drop_database_with_zero_connections(&admin, &second_db).await;
    admin.close().await;

    outcome.expect("parallel fresh-database migrations must both complete");
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_exact_prefix_resumes_db() {
    // Pins: a database with an exact new-epoch prefix resumes at the next
    // semantic migration and becomes a complete V1..V49 history.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for prefix-resume test");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create prefix-resume throwaway database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        let prefix = apply_through_migration(&target_url, "execution_analytics").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let partial_error = moa_migrations::validate_complete_history(&target)
            .await
            .expect_err("an exact prefix is valid for resume but not complete")
            .to_string();
        target.close().await;

        let resumed = run_reporting_applied_serialized(&target_url).await?;
        let second = run_reporting_applied_serialized(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        moa_migrations::validate_complete_history(&target).await?;
        let versions: Vec<i32> = sqlx::query_scalar(
            "SELECT version FROM public.refinery_schema_history ORDER BY version",
        )
        .fetch_all(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            prefix,
            resumed,
            second,
            partial_error,
            versions,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (prefix, resumed, second, partial_error, versions) =
        outcome.expect("exact contiguous prefix should resume successfully");
    let expected = expected_migration_labels();
    let prefix_len = usize::try_from(
        migration_version("execution_analytics").expect("execution analytics must be embedded"),
    )
    .expect("migration version must be positive");
    assert_eq!(prefix, expected[..prefix_len]);
    assert_eq!(resumed, expected[prefix_len..]);
    assert!(
        second.is_empty(),
        "completed history must not reapply: {second:?}"
    );
    assert!(
        partial_error.contains("incomplete: found 28 of 49 expected rows"),
        "complete-history validation must distinguish a valid prefix: {partial_error}"
    );
    assert_eq!(versions, (1..=49).collect::<Vec<_>>());
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_legacy_sparse_rejects_before_ddl_db() {
    // Pins: a sparse-epoch history cannot be adopted or partially rewritten.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for sparse-history guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create sparse-history throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE public.refinery_schema_history (\
                 version INT4 PRIMARY KEY, name VARCHAR(255), \
                 applied_on VARCHAR(255), checksum VARCHAR(255)); \
             INSERT INTO public.refinery_schema_history VALUES \
                 (101, 'auth_baseline', 'legacy', '0');",
        )
        .execute(&target)
        .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect sparse-history rejection");
    assert_destructive_reset_rejection(&error, ddl_count, "diverges at row 1");
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_legacy_v1_only_rejects_before_ddl_db() {
    // Pins: the retired V1 session baseline must not masquerade as the new
    // contiguous epoch marker that now owns version one.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for legacy-V1 guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create legacy-V1 throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE public.refinery_schema_history (\
                 version INT4 PRIMARY KEY, name VARCHAR(255), \
                 applied_on VARCHAR(255), checksum VARCHAR(255)); \
             INSERT INTO public.refinery_schema_history VALUES \
                 (1, 'session_baseline', 'legacy', '0');",
        )
        .execute(&target)
        .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect legacy-V1 rejection");
    assert_destructive_reset_rejection(&error, ddl_count, "diverges at row 1");
    assert!(
        error.contains("expected V000001__contiguous_history_epoch"),
        "legacy V1 rejection must name the new epoch marker: {error}"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_divergent_name_rejects_before_ddl_db() {
    // Pins: matching versions alone cannot authorize a resume.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for name-divergence guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create name-divergence throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        apply_through_migration(&target_url, "contiguous_history_epoch").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query("UPDATE public.refinery_schema_history SET name = 'renamed_epoch'")
            .execute(&target)
            .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect name-divergence rejection");
    assert_destructive_reset_rejection(&error, ddl_count, "diverges at row 1");
    assert!(error.contains("V000001__renamed_epoch"));
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_divergent_checksum_rejects_before_ddl_db() {
    // Pins: a rewritten migration cannot reuse an accepted version and name.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for checksum-divergence guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create checksum-divergence throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        apply_through_migration(&target_url, "contiguous_history_epoch").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query("UPDATE public.refinery_schema_history SET checksum = '0'")
            .execute(&target)
            .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect checksum-divergence rejection");
    assert_destructive_reset_rejection(&error, ddl_count, "diverges at row 1");
    assert!(error.contains("checksum 0"));
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_product_relations_without_history_reject_before_ddl_db() {
    // Pins: an apparently untracked product database is never adopted as fresh.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for untracked-relation guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create untracked-relation throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        target
            .execute("CREATE TABLE public.untracked_product_relation (id BIGINT PRIMARY KEY)")
            .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect untracked-product rejection");
    assert_destructive_reset_rejection(
        &error,
        ddl_count,
        "product relations exist without contiguous central migration history",
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_pii_vault_relations_without_history_reject_before_ddl_db() {
    // Pins: the privacy vault is product state, so an untracked vault-only
    // database is never mistaken for a pristine migration target.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for untracked-vault guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create untracked-vault throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        target
            .execute(
                "CREATE SCHEMA pii_vault; \
                 CREATE TABLE pii_vault.untracked_product_relation (id BIGINT PRIMARY KEY)",
            )
            .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect untracked-vault rejection");
    assert_destructive_reset_rejection(
        &error,
        ddl_count,
        "product relations exist without contiguous central migration history",
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_pii_vault_relations_with_empty_history_reject_before_ddl_db() {
    // Pins: truncating refinery metadata cannot make an existing privacy vault
    // look like a pristine database that is safe to adopt into the new epoch.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for empty-history vault guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create empty-history vault throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        apply_through_migration(&target_url, "contiguous_history_epoch").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        target
            .execute(
                "TRUNCATE public.refinery_schema_history; \
                 CREATE SCHEMA pii_vault; \
                 CREATE TABLE pii_vault.untracked_product_relation (id BIGINT PRIMARY KEY)",
            )
            .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect empty-history vault rejection");
    assert_destructive_reset_rejection(
        &error,
        ddl_count,
        "product relations exist without contiguous central migration history",
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_empty_history_without_relations_recovers_db() {
    // Pins: an empty history table with no product relations is equivalent to a
    // pristine database and can safely receive the whole contiguous epoch.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for empty-history recovery");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create empty-history throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        apply_through_migration(&target_url, "contiguous_history_epoch").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        target
            .execute("TRUNCATE public.refinery_schema_history")
            .await?;
        target.close().await;
        install_required_extensions(&target_url).await?;
        let applied = run_reporting_applied_serialized(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        moa_migrations::validate_complete_history(&target).await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(applied)
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    assert_eq!(
        outcome.expect("empty history should recover as pristine"),
        expected_migration_labels()
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn migration_protocol_malformed_history_rejects_before_ddl_db() {
    // Pins: malformed history metadata fails closed rather than being parsed as
    // a partial epoch or handed to refinery.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for malformed-history guard");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create malformed-history throwaway database");
    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE public.refinery_schema_history (\
                 version TEXT, name TEXT, applied_on TEXT, checksum TEXT); \
             INSERT INTO public.refinery_schema_history VALUES \
                 ('not-a-version', 'contiguous_history_epoch', 'malformed', '0');",
        )
        .execute(&target)
        .await?;
        target.close().await;
        reset_rejection_and_ddl_count(&target_url).await
    }
    .await;
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (error, ddl_count) = outcome.expect("inspect malformed-history rejection");
    assert_destructive_reset_rejection(&error, ddl_count, "malformed version");
}

async fn assert_tenant_purge_graph_scope_uses_typed_tenant(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tenant_id = uuid::Uuid::new_v4();
    let neighbor_tenant_id = uuid::Uuid::new_v4();
    let operation_id = format!("tenant-purge-graph-{tenant_id}");
    let opaque_partition = format!("opaque-graph-{tenant_id}");
    let neighbor_partition = format!("opaque-graph-{neighbor_tenant_id}");
    let first_node = uuid::Uuid::new_v4();
    let second_node = uuid::Uuid::new_v4();
    let edge_id = uuid::Uuid::new_v4();

    sqlx::query(
        "INSERT INTO tenants (id, slug, name) VALUES \
         ($1, $2, 'bounded tenant purge opaque graph target'), \
         ($3, $4, 'bounded tenant purge opaque graph neighbor')",
    )
    .bind(tenant_id)
    .bind(format!("tenant-purge-opaque-target-{tenant_id}"))
    .bind(neighbor_tenant_id)
    .bind(format!("tenant-purge-opaque-neighbor-{neighbor_tenant_id}"))
    .execute(pool)
    .await?;

    let mut graph_write = pool.begin().await?;
    sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *graph_write)
        .await?;
    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'opaque fact one', 'none'), \
                ($4, 'Fact', $2, $3, $3, 'opaque fact two', 'none')",
    )
    .bind(first_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(second_node)
    .execute(&mut *graph_write)
    .await?;
    sqlx::query(
        "INSERT INTO moa.edge_index \
            (uid, label, start_uid, end_uid, storage_partition_id, tenant_id) \
         VALUES ($1, 'RELATES_TO', $2, $3, $4, $5)",
    )
    .bind(edge_id)
    .bind(first_node)
    .bind(second_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .execute(&mut *graph_write)
    .await?;
    let zero_embedding = format!("[{}]", vec!["0"; 1024].join(","));
    sqlx::query(
        "INSERT INTO moa.embeddings \
            (uid, storage_partition_id, tenant_id, label, pii_class, embedding, \
             embedding_model, embedding_model_version) \
         VALUES ($1, $2, $3, 'Fact', 'none', $4::public.halfvec, 'test', 1)",
    )
    .bind(first_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(&zero_embedding)
    .execute(&mut *graph_write)
    .await?;
    sqlx::query(
        "INSERT INTO moa.graph_changelog \
            (storage_partition_id, tenant_id, actor_id, actor_kind, op, \
             target_kind, target_label, target_uid, payload) \
         VALUES ($1, $2, 'tenant-purge-test', 'system', 'create', 'node', 'Fact', $3, '{}'::JSONB)",
    )
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(first_node)
    .execute(&mut *graph_write)
    .await?;
    graph_write.commit().await?;

    let seeded_graph_rows: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.embeddings WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.edge_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.storage_partition_state WHERE tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(seeded_graph_rows, (1, 1, 1, 2, 1));

    let storage_only_error = sqlx::query(
        "INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op) \
         VALUES ($1, $2, 'delete')",
    )
    .bind(format!("opaque-vector-{tenant_id}"))
    .bind(uuid::Uuid::new_v4())
    .execute(pool)
    .await
    .expect_err("true storage-only scope must fail closed for a non-UUID value");
    let storage_only_sqlstate = storage_only_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(storage_only_sqlstate.as_deref(), Some("22P02"));

    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(pool)
        .await?;

    let fenced_node = uuid::Uuid::new_v4();
    let atomic_neighbor_node = uuid::Uuid::new_v4();
    let fenced_insert_error = sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'must roll back', 'none'), \
                ($4, 'Fact', $5, $6, $6, 'neighbor must roll back atomically', 'none')",
    )
    .bind(fenced_node)
    .bind(&opaque_partition)
    .bind(tenant_id)
    .bind(atomic_neighbor_node)
    .bind(&neighbor_partition)
    .bind(neighbor_tenant_id)
    .execute(pool)
    .await
    .expect_err("a typed fenced tenant in a multirow graph write must reject the statement");
    let fenced_insert_sqlstate = fenced_insert_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(fenced_insert_sqlstate.as_deref(), Some("55000"));
    let atomic_insert_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(vec![fenced_node, atomic_neighbor_node])
            .fetch_one(pool)
            .await?;
    assert_eq!(
        atomic_insert_count, 0,
        "the rejected statement must be atomic"
    );

    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, tenant_id, data_subject_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, $3, 'writable neighbor', 'none')",
    )
    .bind(atomic_neighbor_node)
    .bind(&neighbor_partition)
    .bind(neighbor_tenant_id)
    .execute(pool)
    .await?;
    let fenced_update_error = sqlx::query(
        "UPDATE moa.node_index SET tenant_id = $1, name = 'must not move' WHERE uid = $2",
    )
    .bind(tenant_id)
    .bind(atomic_neighbor_node)
    .execute(pool)
    .await
    .expect_err("UPDATE must derive both old and new typed tenant identities");
    let fenced_update_sqlstate = fenced_update_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    assert_eq!(fenced_update_sqlstate.as_deref(), Some("55000"));
    let neighbor_after_update: (uuid::Uuid, String) =
        sqlx::query_as("SELECT tenant_id, name FROM moa.node_index WHERE uid = $1")
            .bind(atomic_neighbor_node)
            .fetch_one(pool)
            .await?;
    assert_eq!(
        neighbor_after_update,
        (neighbor_tenant_id, "writable neighbor".to_string())
    );
    sqlx::query("UPDATE moa.node_index SET name = 'neighbor updated' WHERE uid = $1")
        .bind(atomic_neighbor_node)
        .execute(pool)
        .await?;

    sqlx::query(
        "UPDATE moa.tenant_purge_operations \
         SET current_stage = 'moa.embeddings' \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(&operation_id)
    .execute(pool)
    .await?;
    let mut positive_batches = Vec::new();
    for _ in 0..16 {
        let batch: (String, String, i64) = sqlx::query_as(
            "SELECT batch_state, stage, affected \
             FROM moa.run_tenant_purge_batch($1, $2)",
        )
        .bind(tenant_id)
        .bind(&operation_id)
        .fetch_one(pool)
        .await?;
        if batch.2 > 0 {
            positive_batches.push((batch.1.clone(), batch.2));
        }
        if batch.1 == "public.session_event_dedupe" {
            break;
        }
    }
    assert_eq!(
        positive_batches,
        vec![
            ("moa.embeddings".to_string(), 1),
            ("moa.graph_changelog".to_string(), 1),
            ("moa.edge_index".to_string(), 1),
            ("moa.node_index".to_string(), 2),
            ("moa.storage_partition_state".to_string(), 1),
        ]
    );
    let graph_residue: (i64, i64, i64, i64, i64, i64, String) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.embeddings WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.edge_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.storage_partition_state WHERE tenant_id = $1), \
            (SELECT count(*) FROM moa.node_index WHERE tenant_id = $2), \
            (SELECT name FROM moa.node_index WHERE uid = $3)",
    )
    .bind(tenant_id)
    .bind(neighbor_tenant_id)
    .bind(atomic_neighbor_node)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        graph_residue,
        (0, 0, 0, 0, 0, 1, "neighbor updated".to_string())
    );

    Ok(())
}

async fn assert_tenant_purge_purge_index_catalog(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pins: every bounded candidate query has a valid, ready index whose first
    // key is the purge scope; nullable scopes and retained legal holds stay narrow.
    for (expected_schema, index, expected_table, expected_key, expected_predicate) in
        TENANT_PURGE_SCOPE_INDEXES
    {
        let actual: (String, String, bool, bool, String, Option<String>) = sqlx::query_as(
            r#"
            SELECT table_namespace.nspname,
                   table_relation.relname,
                   index_row.indisvalid,
                   index_row.indisready,
                   pg_get_indexdef(index_row.indexrelid, 1, TRUE),
                   pg_get_expr(index_row.indpred, index_row.indrelid)
            FROM pg_index AS index_row
            JOIN pg_class AS index_relation ON index_relation.oid = index_row.indexrelid
            JOIN pg_namespace AS index_namespace
              ON index_namespace.oid = index_relation.relnamespace
            JOIN pg_class AS table_relation ON table_relation.oid = index_row.indrelid
            JOIN pg_namespace AS table_namespace
              ON table_namespace.oid = table_relation.relnamespace
            WHERE index_namespace.nspname = $1
              AND index_relation.relname = $2
            "#,
        )
        .bind(expected_schema)
        .bind(index)
        .fetch_one(pool)
        .await?;

        if actual.0 != expected_schema
            || actual.1 != expected_table
            || !actual.2
            || !actual.3
            || actual.4 != expected_key
        {
            return Err(std::io::Error::other(format!(
                "purge index {index} is not a ready leading {expected_schema}.{expected_table}({expected_key}) path: {actual:?}"
            ))
            .into());
        }
        match expected_predicate {
            None if actual.5.is_some() => {
                return Err(std::io::Error::other(format!(
                    "purge index {index} unexpectedly has predicate {:?}",
                    actual.5
                ))
                .into());
            }
            Some(fragment)
                if !actual
                    .5
                    .as_deref()
                    .is_some_and(|predicate| predicate.contains(fragment)) =>
            {
                return Err(std::io::Error::other(format!(
                    "purge index {index} is missing predicate fragment {fragment}: {:?}",
                    actual.5
                ))
                .into());
            }
            _ => {}
        }
        if index == "tenant_purge_legal_hold_idx"
            && ![
                "subject_id IS NOT NULL",
                "reason <> '[REDACTED]'::text",
                "placed_by <> '[REDACTED]'::text",
                "released_by <> '[REDACTED]'::text",
            ]
            .iter()
            .all(|fragment| {
                actual
                    .5
                    .as_deref()
                    .is_some_and(|predicate| predicate.contains(fragment))
            })
        {
            return Err(std::io::Error::other(format!(
                "legal-hold purge index is broader than the released/redactable candidate set: {:?}",
                actual.5
            ))
            .into());
        }
    }

    let embedding_children: (i64, bool, bool, bool) = sqlx::query_as(
        r#"
        SELECT count(*),
               bool_and(child_index.indisvalid),
               bool_and(child_index.indisready),
               bool_and(pg_get_indexdef(child_index.indexrelid, 1, TRUE) = 'tenant_id')
        FROM pg_class AS parent_relation
        JOIN pg_namespace AS parent_namespace
          ON parent_namespace.oid = parent_relation.relnamespace
        JOIN pg_inherits AS attachment ON attachment.inhparent = parent_relation.oid
        JOIN pg_index AS child_index ON child_index.indexrelid = attachment.inhrelid
        WHERE parent_namespace.nspname = 'moa'
          AND parent_relation.relname = 'tenant_purge_embeddings_idx'
        "#,
    )
    .fetch_one(pool)
    .await?;
    if embedding_children != (32, true, true, true) {
        return Err(std::io::Error::other(format!(
            "partitioned embeddings purge index must attach 32 valid ready tenant-leading child paths: {embedding_children:?}"
        ))
        .into());
    }

    let authz_index: (bool, bool, String, String, Option<String>) = sqlx::query_as(
        r#"
        SELECT index_row.indisvalid,
               index_row.indisready,
               pg_get_indexdef(index_row.indexrelid, 1, TRUE),
               pg_get_indexdef(index_row.indexrelid, 2, TRUE),
               pg_get_expr(index_row.indpred, index_row.indrelid)
        FROM pg_index AS index_row
        JOIN pg_class AS index_relation ON index_relation.oid = index_row.indexrelid
        JOIN pg_namespace AS index_namespace
          ON index_namespace.oid = index_relation.relnamespace
        WHERE index_namespace.nspname = 'public'
          AND index_relation.relname = 'idx_authz_outbox_tenant'
        "#,
    )
    .fetch_one(pool)
    .await?;
    if !authz_index.0
        || !authz_index.1
        || authz_index.2 != "tenant_id"
        || authz_index.3 != "id"
        || !authz_index
            .4
            .as_deref()
            .is_some_and(|predicate| predicate.contains("tenant_id IS NOT NULL"))
    {
        return Err(std::io::Error::other(format!(
            "authz purge index must be the valid ready partial (tenant_id, id) path: {authz_index:?}"
        ))
        .into());
    }

    let index_presence: (i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*) FILTER (
                   WHERE relation.relname IN (
                       'linked_connections_user_idx',
                       'scim_group_members_user_idx',
                       'scim_group_members_group_idx'
                   )
               ),
               count(*) FILTER (
                   WHERE relation.relname IN (
                       'auth0_ciba_approvals_session_idx',
                       'auth0_ciba_approvals_deciding_user_idx'
                   )
               )
        FROM pg_class AS relation
        JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname IN ('public', 'moa')
          AND relation.relkind IN ('i', 'I')
        "#,
    )
    .fetch_one(pool)
    .await?;
    if index_presence != (0, 2) {
        return Err(std::io::Error::other(format!(
            "bounded tenant purge redundant/CIBA index presence is wrong: {index_presence:?}"
        ))
        .into());
    }

    Ok(())
}

async fn assert_tenant_purge_function_arity_and_tenant_attribution(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pins: bounded tenant purge is a hard two-argument API break with no callable legacy overload.
    let function_arities: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT \
            to_regprocedure('moa.invert_tenant_authz_batch(uuid,text)') IS NOT NULL, \
            to_regprocedure('moa.invert_tenant_authz_batch(uuid,text,integer)') IS NOT NULL, \
            to_regprocedure('moa.run_tenant_purge_batch(uuid,text)') IS NOT NULL, \
            to_regprocedure('moa.run_tenant_purge_batch(uuid,text,integer)') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if function_arities != (true, false, true, false) {
        return Err(std::io::Error::other(format!(
            "bounded tenant purge purge function arities are not an exact two-argument hard break: {function_arities:?}"
        ))
        .into());
    }

    // Pins: ON CONFLICT may never move an existing tuple identity between tenants.
    let original_tenant = uuid::Uuid::new_v4();
    let conflicting_tenant = uuid::Uuid::new_v4();
    let tuple_user = format!("operator:{}", uuid::Uuid::new_v4());
    let tuple_object = format!("tenant:{original_tenant}");
    sqlx::query(
        "INSERT INTO authz_outbox \
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
         VALUES ('write', $1, 'operator', $2, 5, $3)",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .bind(original_tenant)
    .execute(pool)
    .await?;
    let conflict = sqlx::query(
        "INSERT INTO authz_outbox \
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
         VALUES ('delete', $1, 'operator', $2, 5, $3) \
         ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE \
         SET tenant_id = EXCLUDED.tenant_id",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .bind(conflicting_tenant)
    .execute(pool)
    .await
    .expect_err("cross-tenant ON CONFLICT attribution must fail closed");
    let conflict_sqlstate = conflict
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    if conflict_sqlstate.as_deref() != Some("55000") {
        return Err(std::io::Error::other(format!(
            "cross-tenant ON CONFLICT returned {conflict_sqlstate:?}, expected 55000"
        ))
        .into());
    }
    let unchanged: (uuid::Uuid, String, String, i64) = sqlx::query_as(
        "SELECT tenant_id, op, status, generation \
         FROM authz_outbox \
         WHERE tuple_user = $1 AND tuple_relation = 'operator' \
           AND tuple_object = $2 AND model_version = 5",
    )
    .bind(&tuple_user)
    .bind(&tuple_object)
    .fetch_one(pool)
    .await?;
    if unchanged
        != (
            original_tenant,
            "write".to_string(),
            "pending".to_string(),
            1,
        )
    {
        return Err(std::io::Error::other(format!(
            "cross-tenant ON CONFLICT changed the original outbox row: {unchanged:?}"
        ))
        .into());
    }
    let trigger_source: String = sqlx::query_scalar(
        "SELECT pg_get_functiondef('moa.guard_authz_outbox_during_tenant_purge()'::REGPROCEDURE)",
    )
    .fetch_one(pool)
    .await?;
    if !trigger_source.contains("NEW.tenant_id IS DISTINCT FROM OLD.tenant_id") {
        return Err(std::io::Error::other(
            "authz outbox trigger source no longer protects immutable tenant attribution",
        )
        .into());
    }

    Ok(())
}

async fn seed_tenant_purge_release_policies(
    pool: &PgPool,
    tenant_id: uuid::Uuid,
    count: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        WITH policies AS (
            SELECT
                gen_random_uuid() AS policy_uid,
                $1::TEXT AS storage_partition_id,
                format('tenant-purge-bounded-%s-%s', $1::TEXT, ordinal) AS name,
                ordinal AS revision,
                '[{"id":"target_completed","version":"v1"}]'::JSONB
                    AS blocking_assertions,
                '[{"metric":"target_completed"}]'::JSONB AS primary_gate_family,
                3600::BIGINT AS attestation_ttl_secs,
                digest(format('tenant-purge-resource-%s-%s', $1::TEXT, ordinal), 'sha256')
                    AS resource_policy_hash
            FROM generate_series(1, $2::INT) AS ordinal
        )
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash, valid_to
        )
        SELECT
            policy_uid, storage_partition_id, NULL, name, revision, 'skill_visibility',
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash,
            moa.artifact_release_policy_content_hash(
                name, revision, 'skill_visibility', blocking_assertions,
                primary_gate_family, attestation_ttl_secs, resource_policy_hash
            ),
            now()
        FROM policies
        "#,
    )
    .bind(tenant_id)
    .bind(count)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_tenant_purge_activated_release_chain(
    pool: &PgPool,
    tenant_id: uuid::Uuid,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let artifact_uid = uuid::Uuid::new_v4();
    let revision_uid = uuid::Uuid::new_v4();
    let policy_uid = uuid::Uuid::new_v4();
    let attestation_uid = uuid::Uuid::new_v4();
    let audit_uid = uuid::Uuid::new_v4();
    let revision_hash = vec![1_u8; 32];
    let resource_policy_hash = vec![2_u8; 32];
    let subject_digest = vec![3_u8; 32];
    let partition = tenant_id.to_string();
    let policy_name = format!("tenant-purge-active-{tenant_id}");

    sqlx::query(
        "INSERT INTO moa.artifact \
            (artifact_uid, tenant_id, storage_partition_id, user_id, kind, name) \
         VALUES ($1, $2, $2::TEXT, NULL, 'skill', $3)",
    )
    .bind(artifact_uid)
    .bind(tenant_id)
    .bind(format!("tenant-purge-activated-{tenant_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_revision (\
            revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition, \
            canonical_hash, source_format, source_text, status, version\
         ) VALUES ($1, $2, $3, $3::TEXT, NULL, '{}'::JSONB, $4, 'json', ''::BYTEA, 'ready', 1)",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(tenant_id)
    .bind(&revision_hash)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE moa.artifact SET latest_revision_uid = $1 WHERE artifact_uid = $2")
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_release_policy (
            policy_uid, storage_partition_id, user_id, name, revision, target_class,
            blocking_assertions, primary_gate_family, attestation_ttl_secs,
            resource_policy_hash, policy_hash
        ) VALUES (
            $1, $2, NULL, $3, 1, 'skill_visibility',
            '[{"id":"target_completed","version":"v1"}]'::JSONB,
            '[{"metric":"target_completed"}]'::JSONB,
            3600, $4,
            moa.artifact_release_policy_content_hash(
                $3, 1, 'skill_visibility',
                '[{"id":"target_completed","version":"v1"}]'::JSONB,
                '[{"metric":"target_completed"}]'::JSONB,
                3600, $4
            )
        )
        "#,
    )
    .bind(policy_uid)
    .bind(&partition)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_release_candidate (\
            revision_uid, artifact_uid, storage_partition_id, user_id, activation_target, \
            target_installation_uid, subject, subject_digest, candidate_revision_hash, \
            policy_uid, policy_revision, policy_hash, slot, generation\
         ) VALUES (\
            $1, $2, $3, NULL, 'skill_visibility', NULL, '{}'::JSONB, $4, $5, \
            $6, 1, moa.artifact_release_policy_content_hash(\
                $7, 1, 'skill_visibility', \
                '[{\"id\":\"target_completed\",\"version\":\"v1\"}]'::JSONB, \
                '[{\"metric\":\"target_completed\"}]'::JSONB, 3600, $8\
            ), 'released', 1\
         )",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(&partition)
    .bind(&subject_digest)
    .bind(&revision_hash)
    .bind(policy_uid)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.artifact_activation_attestation (\
            attestation_uid, storage_partition_id, user_id, artifact_uid, \
            candidate_revision_uid, activation_target, target_installation_uid, \
            subject_digest, verdict, run_uid, trial_uids, evidence_ids, decision, \
            policy_uid, policy_revision, policy_hash, decided_by, expires_at\
         ) VALUES (\
            $1, $2, NULL, $3, $4, 'skill_visibility', NULL, $5, 'pass', $6, \
            ARRAY[$7]::UUID[], ARRAY[$8]::UUID[], '{}'::JSONB, $9, 1, \
            moa.artifact_release_policy_content_hash(\
                $10, 1, 'skill_visibility', \
                '[{\"id\":\"target_completed\",\"version\":\"v1\"}]'::JSONB, \
                '[{\"metric\":\"target_completed\"}]'::JSONB, 3600, $11\
            ), 'tenant-purge-test', now() + interval '1 hour'\
         )",
    )
    .bind(attestation_uid)
    .bind(&partition)
    .bind(artifact_uid)
    .bind(revision_uid)
    .bind(&subject_digest)
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(uuid::Uuid::new_v4())
    .bind(policy_uid)
    .bind(&policy_name)
    .bind(&resource_policy_hash)
    .execute(pool)
    .await?;

    let mut activation = pool.begin().await?;
    sqlx::query("SELECT set_config('moa.storage_partition_id', $1, true)")
        .bind(&partition)
        .execute(&mut *activation)
        .await?;
    let affected: i64 = sqlx::query_scalar(
        "SELECT moa.apply_artifact_activation_transition(\
            $1, $2, $3, 'skill', 'skill_visibility', NULL, $4, $5, NULL, 0, \
            $6, 1, $7, 1, 'tenant-purge-test', 'activated-chain proof', now()\
         )",
    )
    .bind(audit_uid)
    .bind(&partition)
    .bind(artifact_uid)
    .bind(attestation_uid)
    .bind(&subject_digest)
    .bind(revision_uid)
    .bind(&revision_hash)
    .fetch_one(&mut *activation)
    .await?;
    assert_eq!(
        affected, 1,
        "fixture activation must move one serving pointer"
    );
    activation.commit().await?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, sqlx::FromRow)]
struct FinalIndexCatalogRow {
    qualified_name: String,
    table_schema: String,
    table_name: String,
    is_unique: bool,
    is_primary: bool,
    is_valid: bool,
    is_ready: bool,
    is_live: bool,
    definition: String,
    parent_index: Option<String>,
}

async fn final_index_catalog(
    pool: &PgPool,
) -> Result<
    std::collections::BTreeMap<String, FinalIndexCatalogRow>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    let rows: Vec<FinalIndexCatalogRow> = sqlx::query_as(
        r#"
        SELECT
            index_namespace.nspname || '.' || index_relation.relname AS qualified_name,
            table_namespace.nspname AS table_schema,
            table_relation.relname AS table_name,
            index_row.indisunique AS is_unique,
            index_row.indisprimary AS is_primary,
            index_row.indisvalid AS is_valid,
            index_row.indisready AS is_ready,
            index_row.indislive AS is_live,
            pg_get_indexdef(index_relation.oid) AS definition,
            CASE WHEN parent_relation.oid IS NULL THEN NULL
                 ELSE parent_namespace.nspname || '.' || parent_relation.relname
            END AS parent_index
        FROM pg_index AS index_row
        JOIN pg_class AS index_relation ON index_relation.oid = index_row.indexrelid
        JOIN pg_namespace AS index_namespace ON index_namespace.oid = index_relation.relnamespace
        JOIN pg_class AS table_relation ON table_relation.oid = index_row.indrelid
        JOIN pg_namespace AS table_namespace ON table_namespace.oid = table_relation.relnamespace
        LEFT JOIN pg_inherits AS attachment ON attachment.inhrelid = index_relation.oid
        LEFT JOIN pg_class AS parent_relation ON parent_relation.oid = attachment.inhparent
        LEFT JOIN pg_namespace AS parent_namespace ON parent_namespace.oid = parent_relation.relnamespace
        WHERE index_namespace.nspname !~ '^pg_'
          AND index_namespace.nspname <> 'information_schema'
        ORDER BY qualified_name
        "#,
    )
    .fetch_all(pool)
    .await?;
    let mut catalog = std::collections::BTreeMap::new();
    for row in rows {
        if catalog.insert(row.qualified_name.clone(), row).is_some() {
            return Err(std::io::Error::other("index catalog contained a duplicate name").into());
        }
    }
    Ok(catalog)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PrivacyAuditorSecurityCatalog {
    auditor_grants: std::collections::BTreeSet<String>,
    policies: std::collections::BTreeSet<String>,
}

async fn privacy_auditor_security_catalog(
    pool: &PgPool,
) -> Result<PrivacyAuditorSecurityCatalog, Box<dyn std::error::Error + Send + Sync>> {
    let auditor_grants = sqlx::query_scalar(
        r#"
        SELECT namespace.nspname || '.' || relation.relname
               || '|' || acl.privilege_type
               || '|' || acl.is_grantable::TEXT
        FROM pg_class AS relation
        JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace
        CROSS JOIN LATERAL aclexplode(
            COALESCE(relation.relacl, acldefault('r', relation.relowner))
        ) AS acl
        JOIN pg_roles AS grantee ON grantee.oid = acl.grantee
        WHERE relation.relkind IN ('r', 'p')
          AND grantee.rolname = 'moa_auditor'
        ORDER BY 1
        "#,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let policies = sqlx::query_scalar(
        r#"
        SELECT schemaname || '.' || tablename
               || '|' || policyname
               || '|' || permissive
               || '|' || roles::TEXT
               || '|' || cmd
               || '|' || COALESCE(qual, '')
               || '|' || COALESCE(with_check, '')
        FROM pg_policies
        WHERE 'moa_auditor' = ANY(roles)
        ORDER BY 1
        "#,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(PrivacyAuditorSecurityCatalog {
        auditor_grants,
        policies,
    })
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn bounded_tenant_purge_final_schema_executes_bounded_batches_db() {
    // Pins: a pristine final schema persists exactly 129 purge stages, installs
    // statement fences, and advances a real purge in fixed-size batches.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let operation_id = format!("tenant-purge-{tenant_id}");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect bounded tenant purge maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create bounded tenant purge throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(&target)
        .await?;
        assert_tenant_purge_purge_index_catalog(&target).await?;
        assert_tenant_purge_function_arity_and_tenant_attribution(&target).await?;
        assert_tenant_purge_graph_scope_uses_typed_tenant(&target).await?;
        let migrated: (String, String, i64, i64, i64, bool, bool) = sqlx::query_as(
            "SELECT status, current_stage, stage_deleted_count, total_deleted_count, \
                    batch_count, started_at IS NOT NULL, updated_at IS NOT NULL \
             FROM moa.tenant_purge_operations WHERE tenant_id = $1",
        )
        .bind(tenant_id)
        .fetch_one(&target)
        .await?;
        let catalog_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM moa.tenant_purge_catalog")
                .fetch_one(&target)
                .await?;
        let trigger_kinds: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT trigger_name FROM information_schema.triggers \
             WHERE trigger_name IN (\
                'moa_tenant_purge_fence_insert', \
                'moa_tenant_purge_fence_update'\
             ) ORDER BY trigger_name",
        )
        .fetch_all(&target)
        .await?;
        let global_exemptions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.tenant_purge_catalog \
             WHERE table_name IN (\
                'simulator_certification_mandate', \
                'simulator_certification_evidence_import'\
             )",
        )
        .fetch_one(&target)
        .await?;
        let fence_helper_contract: (String, bool, Vec<String>, bool, bool, bool) = sqlx::query_as(
            r#"
                SELECT owner.rolname,
                       function_row.prosecdef,
                       COALESCE(function_row.proconfig, ARRAY[]::TEXT[]),
                       NOT EXISTS (
                           SELECT 1
                           FROM aclexplode(COALESCE(
                               function_row.proacl,
                               acldefault('f', function_row.proowner)
                           )) AS function_acl
                           WHERE function_acl.grantee = 0
                             AND function_acl.privilege_type = 'EXECUTE'
                       ),
                       (
                           SELECT array_agg(grantee.rolname::TEXT ORDER BY grantee.rolname) = ARRAY[
                               'moa_app',
                               'moa_artifact_activator',
                               'moa_privacy_eraser',
                               'moa_promoter'
                           ]::TEXT[]
                           FROM aclexplode(COALESCE(
                               function_row.proacl,
                               acldefault('f', function_row.proowner)
                           )) AS function_acl
                           JOIN pg_roles grantee ON grantee.oid = function_acl.grantee
                           WHERE function_acl.privilege_type = 'EXECUTE'
                             AND grantee.rolname <> 'moa_owner'
                       ),
                       NOT guard_row.prosecdef
                FROM pg_proc function_row
                JOIN pg_namespace namespace
                  ON namespace.oid = function_row.pronamespace
                JOIN pg_roles owner ON owner.oid = function_row.proowner
                JOIN pg_proc guard_row ON guard_row.oid =
                    'moa.guard_tenant_write_statement()'::REGPROCEDURE
                WHERE namespace.nspname = 'moa'
                  AND function_row.oid = 'moa.tenant_write_fenced(uuid)'::REGPROCEDURE
                "#,
        )
        .fetch_one(&target)
        .await?;
        let restricted_fence_select_grants: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)
            FROM (VALUES
                ('moa_artifact_activator'::NAME),
                ('moa_privacy_eraser'::NAME)
            ) AS restricted(role_name)
            WHERE has_table_privilege(
                restricted.role_name,
                'moa.destruction_operation_fence',
                'SELECT'
            )
               OR has_table_privilege(
                    restricted.role_name,
                    'moa.tenant_purge_operations',
                    'SELECT'
               )
            "#,
        )
        .fetch_one(&target)
        .await?;
        let legacy_release_cleanup: (bool, i64, i64, bool, String) = sqlx::query_as(
            r#"
            WITH legacy_tables(table_name) AS (
                VALUES
                    ('artifact_release_eval_overlay'),
                    ('artifact_release_attempt'),
                    ('artifact_release_dispatch_outbox'),
                    ('artifact_release_case_pack'),
                    ('artifact_serving_pointer'),
                    ('artifact_activation_audit'),
                    ('artifact_activation_attestation'),
                    ('artifact_release_candidate'),
                    ('artifact_release_policy')
            )
            SELECT
                to_regprocedure('moa.purge_artifact_release_partition(text)') IS NULL,
                (
                    SELECT count(*)
                    FROM pg_policies AS policy
                    JOIN legacy_tables AS legacy
                      ON legacy.table_name = policy.tablename
                    WHERE policy.schemaname = 'moa'
                      AND policy.policyname IN (
                          'artifact_release_partition_purge_read',
                          'artifact_release_partition_purge'
                      )
                ),
                (
                    SELECT count(*)
                    FROM legacy_tables AS legacy
                    WHERE has_table_privilege(
                              'moa_artifact_releaser',
                              format('moa.%I', legacy.table_name),
                              'SELECT'
                          )
                       OR has_table_privilege(
                              'moa_artifact_releaser',
                              format('moa.%I', legacy.table_name),
                              'DELETE'
                          )
                ),
                has_schema_privilege('moa_artifact_releaser', 'moa', 'USAGE'),
                pg_get_functiondef('moa.artifact_activation_audit_guard()'::REGPROCEDURE)
            "#,
        )
        .fetch_one(&target)
        .await?;
        let audit_guard_contract: (String, bool, Vec<String>, bool, bool) = sqlx::query_as(
            r#"
            SELECT owner.rolname,
                   function_row.prosecdef,
                   COALESCE(function_row.proconfig, ARRAY[]::TEXT[]),
                   NOT EXISTS (
                       SELECT 1
                       FROM aclexplode(COALESCE(
                           function_row.proacl,
                           acldefault('f', function_row.proowner)
                       )) AS function_acl
                       WHERE function_acl.grantee = 0
                         AND function_acl.privilege_type = 'EXECUTE'
                   ),
                   NOT EXISTS (
                       SELECT 1
                       FROM aclexplode(COALESCE(
                           function_row.proacl,
                           acldefault('f', function_row.proowner)
                       )) AS function_acl
                       WHERE function_acl.grantee <> function_row.proowner
                         AND function_acl.privilege_type = 'EXECUTE'
                   )
            FROM pg_proc AS function_row
            JOIN pg_roles AS owner ON owner.oid = function_row.proowner
            WHERE function_row.oid =
                'moa.artifact_activation_audit_guard()'::REGPROCEDURE
            "#,
        )
        .fetch_one(&target)
        .await?;

        let purge_tenant = uuid::Uuid::new_v4();
        let neighbor_tenant = uuid::Uuid::new_v4();
        let purge_operation = format!("tenant-purge-{purge_tenant}");
        sqlx::query(
            "INSERT INTO tenants (id, slug, name) VALUES \
             ($1, $2, 'bounded tenant purge purge target'), ($3, $4, 'bounded tenant purge neighbor')",
        )
        .bind(purge_tenant)
        .bind(format!("tenant-purge-target-{purge_tenant}"))
        .bind(neighbor_tenant)
        .bind(format!("tenant-purge-neighbor-{neighbor_tenant}"))
        .execute(&target)
        .await?;
        seed_tenant_purge_release_policies(&target, purge_tenant, 1000).await?;
        seed_tenant_purge_activated_release_chain(&target, purge_tenant).await?;
        seed_tenant_purge_activated_release_chain(&target, neighbor_tenant).await?;
        let ordinary_activation_delete = sqlx::query(
            "DELETE FROM moa.artifact_activation_audit WHERE storage_partition_id = $1::TEXT",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await
        .expect_err("ordinary activation-audit deletion must remain append-only");
        let ordinary_activation_delete_sqlstate = ordinary_activation_delete
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, active) \
             SELECT gen_random_uuid(), $1, 'tenant-purge-' || ordinal || '@example.test', true \
             FROM generate_series(1, 1001) AS ordinal",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO authz_outbox \
                (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
             SELECT 'write', 'operator:' || gen_random_uuid(), 'operator', \
                    'tenant:' || $1::TEXT, 5, $1 \
             FROM generate_series(1, 1001)",
        )
        .bind(purge_tenant)
        .execute(&target)
        .await?;
        sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
            .bind(purge_tenant)
            .bind(&purge_operation)
            .execute(&target)
            .await?;

        let subject_only_tenant = uuid::Uuid::new_v4();
        let committed_tenant = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.destruction_operation_fence \
                (tenant_id, subject_id, operation_id, operation_kind) \
             VALUES ($1, $2, 'subject-only-probe', 'privacy.erase')",
        )
        .bind(subject_only_tenant)
        .bind(uuid::Uuid::new_v4())
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.destruction_operation_fence \
                (tenant_id, subject_id, operation_id, operation_kind, status, committed_at) \
             VALUES ($1, NULL, 'committed-probe', 'privacy.erase', 'committed', now())",
        )
        .bind(committed_tenant)
        .execute(&target)
        .await?;
        let helper_scope_facts: (bool, bool, bool) = sqlx::query_as(
            "SELECT moa.tenant_write_fenced($1), \
                    moa.tenant_write_fenced($2), \
                    moa.tenant_write_fenced($3)",
        )
        .bind(purge_tenant)
        .bind(subject_only_tenant)
        .bind(committed_tenant)
        .fetch_one(&target)
        .await?;
        let mut activator = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_artifact_activator")
            .execute(&mut *activator)
            .await?;
        let activator_fenced: bool = sqlx::query_scalar("SELECT moa.tenant_write_fenced($1)")
            .bind(purge_tenant)
            .fetch_one(&mut *activator)
            .await?;
        activator.rollback().await?;
        let mut eraser = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_privacy_eraser")
            .execute(&mut *eraser)
            .await?;
        let eraser_fenced: bool = sqlx::query_scalar("SELECT moa.tenant_write_fenced($1)")
            .bind(purge_tenant)
            .fetch_one(&mut *eraser)
            .await?;
        eraser.rollback().await?;

        sqlx::raw_sql(
            "GRANT INSERT, SELECT ON authz_outbox TO moa_app; \
             GRANT SELECT ON moa.tenant_purge_operations TO moa_app; \
             GRANT SELECT ON moa.destruction_operation_fence TO moa_app;",
        )
        .execute(&target)
        .await?;
        let mut spoof = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *spoof)
            .await?;
        sqlx::query(
            "SELECT set_config('moa.tenant_id', $1, true), \
                    set_config('moa.tenant_purge_operation_id', $2, true)",
        )
        .bind(purge_tenant.to_string())
        .bind(&purge_operation)
        .execute(&mut *spoof)
        .await?;
        let spoof_error = sqlx::query(
            "INSERT INTO authz_outbox \
                (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id) \
             VALUES ('write', $1, 'operator', $2, 5, $3)",
        )
        .bind(format!("operator:{}", uuid::Uuid::new_v4()))
        .bind(format!("tenant:{purge_tenant}"))
        .bind(purge_tenant)
        .execute(&mut *spoof)
        .await
        .expect_err("a spoofed purge GUC must not authorize a desired write");
        let spoof_sqlstate = spoof_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()));
        spoof.rollback().await?;

        let first_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;
        let second_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;
        let final_authz: (i32, i32, bool) = sqlx::query_as(
            "SELECT scanned, inverted, exhausted \
             FROM moa.invert_tenant_authz_batch($1, $2)",
        )
        .bind(purge_tenant)
        .bind(&purge_operation)
        .fetch_one(&target)
        .await?;

        let mut terminal = None;
        let mut release_policy_batches = Vec::new();
        for _ in 0..300 {
            let batch: (String, String, i64) = sqlx::query_as(
                "SELECT batch_state, stage, affected \
                 FROM moa.run_tenant_purge_batch($1, $2)",
            )
            .bind(purge_tenant)
            .bind(&purge_operation)
            .fetch_one(&target)
            .await?;
            if batch.1 == "moa.artifact_release_policy" && batch.2 > 0 {
                release_policy_batches.push(batch.2);
            }
            if batch.0 == "committed" || batch.0 == "already_committed" {
                terminal = Some(batch);
                break;
            }
        }
        let bounded_facts: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM users WHERE tenant_id = $1), \
                (SELECT count(*) FROM tenants WHERE id = $1), \
                (SELECT count(*) FROM tenants WHERE id = $2), \
                (SELECT count(*) FROM authz_outbox \
                 WHERE tenant_id = $1 AND op = 'delete' AND status = 'pending'), \
                (SELECT total_deleted_count FROM moa.tenant_purge_operations \
                 WHERE tenant_id = $1 AND status = 'relationally_committed'), \
                (SELECT count(*) FROM moa.artifact_release_policy \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_release_policy \
                 WHERE storage_partition_id = $2::TEXT)",
        )
        .bind(purge_tenant)
        .bind(neighbor_tenant)
        .fetch_one(&target)
        .await?;
        let activation_chain_facts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT count(*) FROM moa.artifact_activation_audit \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_activation_audit \
                 WHERE storage_partition_id = $2::TEXT), \
                (SELECT count(*) FROM moa.artifact_serving_pointer \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact_serving_pointer \
                 WHERE storage_partition_id = $2::TEXT), \
                (SELECT count(*) FROM moa.artifact \
                 WHERE storage_partition_id = $1::TEXT), \
                (SELECT count(*) FROM moa.artifact \
                 WHERE storage_partition_id = $2::TEXT)",
        )
        .bind(purge_tenant)
        .bind(neighbor_tenant)
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            migrated,
            catalog_count,
            trigger_kinds,
            global_exemptions,
            fence_helper_contract,
            restricted_fence_select_grants,
            legacy_release_cleanup,
            audit_guard_contract,
            helper_scope_facts,
            activator_fenced,
            eraser_fenced,
            first_authz,
            second_authz,
            final_authz,
            release_policy_batches,
            terminal,
            bounded_facts,
            activation_chain_facts,
            spoof_sqlstate,
            ordinary_activation_delete_sqlstate,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    let (
        first,
        second,
        migrated,
        catalog_count,
        trigger_kinds,
        global_exemptions,
        fence_helper_contract,
        restricted_fence_select_grants,
        legacy_release_cleanup,
        audit_guard_contract,
        helper_scope_facts,
        activator_fenced,
        eraser_fenced,
        first_authz,
        second_authz,
        final_authz,
        release_policy_batches,
        terminal,
        bounded_facts,
        activation_chain_facts,
        spoof_sqlstate,
        ordinary_activation_delete_sqlstate,
    ) = outcome.expect("bounded tenant-purge assertions should complete");
    assert_eq!(first, expected_migration_labels());
    assert!(
        second.is_empty(),
        "the final schema must not reapply: {second:?}"
    );
    assert_eq!(
        migrated,
        (
            "in_progress".to_string(),
            "authz".to_string(),
            0,
            0,
            0,
            true,
            true,
        )
    );
    assert_eq!(catalog_count, 129);
    assert_eq!(
        trigger_kinds,
        vec![
            "moa_tenant_purge_fence_insert".to_string(),
            "moa_tenant_purge_fence_update".to_string(),
        ]
    );
    assert_eq!(global_exemptions, 0);
    assert_eq!(
        fence_helper_contract,
        (
            "moa_owner".to_string(),
            true,
            vec!["search_path=pg_catalog, pg_temp".to_string()],
            true,
            true,
            true,
        ),
        "the fence helper must remain owner-defined and least privilege while the statement guard remains invoker-rights"
    );
    assert_eq!(
        restricted_fence_select_grants, 0,
        "restricted definer roles must not read tenant purge control tables directly"
    );
    assert!(
        legacy_release_cleanup.0,
        "the legacy monolithic release purge function must be absent"
    );
    assert_eq!(
        legacy_release_cleanup.1, 0,
        "all legacy release read/delete policies must be absent"
    );
    assert_eq!(
        legacy_release_cleanup.2, 0,
        "the inert releaser role must retain no release-table privileges"
    );
    assert!(
        !legacy_release_cleanup.3,
        "the inert releaser role must retain no moa schema usage"
    );
    assert!(
        legacy_release_cleanup
            .4
            .contains("moa.tenant_purge_bypass_valid")
            && !legacy_release_cleanup.4.contains("moa_artifact_releaser")
            && !legacy_release_cleanup
                .4
                .contains("artifact_release_purge_partition"),
        "the audit guard must admit deletion only through the validated bounded purge: {}",
        legacy_release_cleanup.4
    );
    assert_eq!(
        audit_guard_contract,
        (
            "moa_owner".to_string(),
            true,
            vec!["search_path=pg_catalog, pg_temp".to_string()],
            true,
            true,
        ),
        "the activation-audit purge exception must run under a hardened owner-only trigger function with no direct non-owner execution"
    );
    assert_eq!(
        helper_scope_facts,
        (true, false, false),
        "only an in-progress tenant-wide fence may trip the restricted-writer helper"
    );
    assert!(
        activator_fenced,
        "artifact activator must execute the helper"
    );
    assert!(eraser_fenced, "privacy eraser must execute the helper");
    assert_eq!(first_authz, (1000, 1000, false));
    assert_eq!(second_authz, (1, 1, false));
    assert_eq!(final_authz, (0, 0, true));
    assert_eq!(
        release_policy_batches,
        vec![1000, 1],
        "the release-policy stage must cross the fixed 1,000-row boundary"
    );
    assert_eq!(
        terminal,
        Some(("committed".to_string(), "complete".to_string(), 0))
    );
    assert_eq!(
        bounded_facts,
        (0, 0, 1, 1001, 2010, 0, 1),
        "the target release-policy set must be gone while the neighboring policy survives"
    );
    assert_eq!(
        activation_chain_facts,
        (0, 1, 0, 1, 0, 1),
        "the target activation chain must be gone while the neighboring chain survives"
    );
    assert_eq!(spoof_sqlstate.as_deref(), Some("55000"));
    assert_eq!(
        ordinary_activation_delete_sqlstate.as_deref(),
        Some("P0001"),
        "ordinary callers must not bypass the activation audit's append-only guard"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn privacy_export_auditor_final_catalog_reads_typed_surface_db() {
    // Pins: the final schema gives the dedicated auditor only the typed export
    // read surface and exposes structured subject-access audit rows to that role.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let tenant_id = uuid::Uuid::new_v4();
    let subject_user_id = uuid::Uuid::new_v4();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect privacy-export auditor maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create privacy-export auditor throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;
        let catalog = privacy_auditor_security_catalog(&target).await?;

        let audit_payload = format!(
            r#"{{"reason":"subject access request","subject_user_id":"{subject_user_id}","subjects":[{{"user_id":"{subject_user_id}","target_uid":"{subject_user_id}","provenance":"requested"}}],"storage_partition":"{tenant_id}","artifact_counts":{{"nodes":0}},"files":1}}"#
        );
        let audit_metadata = format!(
            r#"{{"approval_token_jti":"privacy-export-jti-{subject_user_id}","approval_token_sub":"privacy-export-admin","subject_user_id":"{subject_user_id}","subjects":[{{"user_id":"{subject_user_id}","target_uid":"{subject_user_id}","provenance":"requested"}}],"op":"export"}}"#
        );
        sqlx::query(
            r#"
            INSERT INTO moa.graph_changelog (
                storage_partition_id, tenant_id, actor_id, actor_kind, op,
                target_kind, target_label, target_uid, payload,
                pii_class, audit_metadata
            )
            VALUES ($1, $2, 'privacy-export-admin', 'admin', 'export',
                    'user', 'User', $3, $4::JSONB, 'phi', $5::JSONB)
            "#,
        )
        .bind(tenant_id.to_string())
        .bind(tenant_id)
        .bind(subject_user_id)
        .bind(&audit_payload)
        .bind(&audit_metadata)
        .execute(&target)
        .await?;

        let mut auditor = target.begin().await?;
        sqlx::query("SET LOCAL ROLE moa_auditor")
            .execute(&mut *auditor)
            .await?;
        for table in PRIVACY_AUDITOR_TABLES {
            sqlx::query(&format!("SELECT 1 FROM {table} LIMIT 0"))
                .fetch_optional(&mut *auditor)
                .await?;
        }
        let visible_audit: (String, String, uuid::Uuid, bool, bool) = sqlx::query_as(
            "SELECT op, target_kind, target_uid, \
                    payload = $2::JSONB, audit_metadata = $3::JSONB \
                 FROM moa.graph_changelog \
                 WHERE target_uid = $1 AND op = 'export'",
        )
        .bind(subject_user_id)
        .bind(&audit_payload)
        .bind(&audit_metadata)
        .fetch_one(&mut *auditor)
        .await?;
        auditor.rollback().await?;
        target.close().await;

        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            catalog,
            first,
            second,
            visible_audit,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (catalog, first, second, visible_audit) =
        outcome.expect("privacy-export auditor assertions should complete");
    assert_eq!(first, expected_migration_labels());
    assert!(
        second.is_empty(),
        "the migration runner must not reapply the final migration: {second:?}"
    );

    let expected_grants = FINAL_AUDITOR_GRANT_TABLES
        .iter()
        .map(|table| format!("{table}|SELECT|false"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        catalog.auditor_grants, expected_grants,
        "moa_auditor must have exactly the typed non-grantable SELECT surface"
    );
    let expected_policies = FINAL_AUDITOR_POLICY_TABLES
        .iter()
        .map(|table| format!("{table}|rd_auditor|PERMISSIVE|{{moa_auditor}}|SELECT|true|"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        catalog.policies, expected_policies,
        "moa_auditor must have exactly one typed SELECT policy per export relation"
    );

    assert_eq!(visible_audit.0, "export");
    assert_eq!(visible_audit.1, "user");
    assert_eq!(visible_audit.2, subject_user_id);
    assert!(
        visible_audit.3,
        "the structured export payload must round-trip"
    );
    assert!(
        visible_audit.4,
        "the structured audit metadata must round-trip"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn graph_changelog_final_schema_installs_statement_transition_trigger_db() {
    // Pins: the installed catalog, not only migration source text, owns one
    // statement-level trigger with a named NEW transition relation.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let (is_row_trigger, definition): (bool, String) = sqlx::query_as(
            "SELECT (trigger_row.tgtype & 1) = 1, pg_get_triggerdef(trigger_row.oid) \
             FROM pg_trigger AS trigger_row \
             WHERE trigger_row.tgrelid = 'moa.graph_changelog'::REGCLASS \
               AND trigger_row.tgname = 'graph_changelog_bump_storage_partition_state' \
               AND NOT trigger_row.tgisinternal",
        )
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((is_row_trigger, definition))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (is_row_trigger, definition) =
        outcome.expect("graph changelog generation catalog probe should complete");
    assert!(
        !is_row_trigger,
        "generation trigger must be statement-level"
    );
    assert!(
        definition.contains("REFERENCING NEW TABLE AS inserted_graph_changelog_rows"),
        "generation trigger must expose the graph changelog generation transition relation: {definition}"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn artifact_release_activation_boundary_is_execute_only_db() {
    // Pins: only the dedicated non-login role owns release-transition functions,
    // the application role may execute them, and raw pointer/audit writes remain
    // revoked.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create artifact release boundary database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        target
            .execute(
                "CREATE EXTENSION IF NOT EXISTS vector; \
                 CREATE EXTENSION IF NOT EXISTS pgaudit;",
            )
            .await?;
        run_reporting_applied_serialized(&target_url).await?;

        let boundary_ok: bool = sqlx::query_scalar(
            r#"
            SELECT
                NOT activator.rolcanlogin
                AND NOT activator.rolinherit
                AND NOT activator.rolbypassrls
                AND (
                    SELECT count(*) = 3
                    FROM pg_proc function_row
                    JOIN pg_namespace namespace ON namespace.oid = function_row.pronamespace
                    JOIN pg_roles owner ON owner.oid = function_row.proowner
                    WHERE namespace.nspname = 'moa'
                      AND function_row.proname IN (
                          'lock_artifact_serving_pointer',
                          'apply_artifact_activation_transition',
                          'apply_artifact_rollback_transition'
                      )
                      AND owner.rolname = 'moa_artifact_activator'
                      AND 'search_path=pg_catalog, pg_temp' =
                          ANY(COALESCE(function_row.proconfig, ARRAY[]::TEXT[]))
                      AND has_function_privilege('moa_app', function_row.oid, 'EXECUTE')
                      AND NOT has_function_privilege(
                          'moa_promoter', function_row.oid, 'EXECUTE'
                      )
                )
                AND NOT has_table_privilege(
                    'moa_app', 'moa.artifact_serving_pointer', 'INSERT'
                )
                AND NOT has_table_privilege(
                    'moa_app', 'moa.artifact_serving_pointer', 'UPDATE'
                )
                AND NOT has_table_privilege(
                    'moa_app', 'moa.artifact_serving_pointer', 'DELETE'
                )
                AND NOT has_table_privilege(
                    'moa_app', 'moa.artifact_activation_audit', 'INSERT'
                )
            FROM pg_roles activator
            WHERE activator.rolname = 'moa_artifact_activator'
            "#,
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(boundary_ok)
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;
    assert!(
        outcome.expect("artifact release boundary assertions should complete"),
        "artifact activation role, function ownership, or raw-DML revocation drifted"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn execution_analytics_fresh_cutover_and_exact_contract_db() {
    // Pins: execution analytics starts normalized audit storage empty, installs every finite SQL matrix and
    // immutable trace/high-water boundary, rebuilds execution-only facts, and applies once.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create execution analytics contract database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let audit_counts: (i64, i64, i64) = sqlx::query_as(
            "SELECT \
                (SELECT COUNT(*) FROM moa.execution_route_audit), \
                (SELECT COUNT(*) FROM moa.execution_planner_call_audit), \
                (SELECT COUNT(*) FROM moa.execution_compile_audit)",
        )
        .fetch_one(&target)
        .await?;
        let removed_run_mode_column = removed_serialized_value(&["route_", "mode"]);
        let route_schema_contract: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT \
                NOT EXISTS (SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = 'moa' AND table_name = 'execution_run' \
                      AND column_name = $1), \
                NOT EXISTS (SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = 'moa' AND table_name = 'execution_run' \
                      AND column_name = 'route_rationale'), \
                NOT EXISTS (SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = 'moa' AND table_name = 'execution_route_audit' \
                      AND column_name = 'mode'), \
                NOT EXISTS (SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = 'moa' AND table_name = 'execution_route_audit' \
                      AND column_name = 'rationale'), \
                EXISTS (SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = 'moa' AND table_name = 'execution_route_audit' \
                      AND column_name = 'strategy'), \
                to_regprocedure('moa.execution_route_rationale_is_valid(text)') IS NULL",
        )
        .bind(&removed_run_mode_column)
        .fetch_one(&target)
        .await?;
        let valid_route_cells: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM (
                VALUES
                ('initial','needs_input',NULL,'blank_objective'),
                ('initial','needs_input',NULL,'classifier'),
                ('initial','respond',NULL,'classifier'),
                ('initial','execute','inline','classifier'),
                ('initial','execute','durable','classifier'),
                ('initial','execute','durable','selected_execution_template'),
                ('durable_upgrade','execute','durable','durable_upgrade')
            ) cell(stage,decision,strategy,source)
            WHERE moa.execution_route_audit_row_is_valid(
                stage,decision,strategy,source,
                CASE WHEN source = 'classifier' THEN 'accepted' ELSE 'not_called' END,
                CASE WHEN source = 'classifier' THEN 'route-model' END,
                CASE WHEN source = 'classifier' THEN 'execution-router' END,
                repeat('a', 64),
                CASE WHEN source = 'classifier' THEN repeat('b', 64) END,
                (CASE WHEN source = 'classifier' THEN 9500 END)::SMALLINT,
                (CASE WHEN decision = 'needs_input' THEN 1 ELSE 0 END)::SMALLINT,
                (CASE WHEN source = 'classifier' THEN 1 ELSE 0 END)::BIGINT,
                0::BIGINT,0::BIGINT,0::BIGINT,0::BIGINT,
                (CASE WHEN source = 'classifier' THEN 1 ELSE 0 END)::BIGINT
            )
            "#,
        )
        .fetch_one(&target)
        .await?;
        let invalid_route_cell: bool = sqlx::query_scalar(
            "SELECT moa.execution_route_audit_row_is_valid(\
             'initial','respond','inline','classifier',\
             'accepted','route-model','execution-router',repeat('a',64),repeat('b',64),\
             9500::SMALLINT,0::SMALLINT,1::BIGINT,0::BIGINT,0::BIGINT,0::BIGINT,1::BIGINT,1::BIGINT)",
        )
        .fetch_one(&target)
        .await?;

        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 101,
                stage: "initial",
                decision: "respond",
                strategy: Some("inline"),
                source: "classifier",
                classifier_outcome: "accepted",
                classifier_evidence: true,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 102,
                stage: "initial",
                decision: "needs_input",
                strategy: Some("durable"),
                source: "blank_objective",
                classifier_outcome: "not_called",
                classifier_evidence: false,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 103,
                stage: "initial",
                decision: "execute",
                strategy: None,
                source: "classifier",
                classifier_outcome: "accepted",
                classifier_evidence: true,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 106,
                stage: "durable_upgrade",
                decision: "execute",
                strategy: Some("durable"),
                source: "classifier",
                classifier_outcome: "accepted",
                classifier_evidence: true,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 107,
                stage: "initial",
                decision: "execute",
                strategy: Some("durable"),
                source: "selected_execution_template",
                classifier_outcome: "not_called",
                classifier_evidence: true,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 108,
                stage: "initial",
                decision: "routed",
                strategy: None,
                source: "classifier",
                classifier_outcome: "accepted",
                classifier_evidence: true,
            },
        )
        .await?;
        let removed_upgrade_value = removed_serialized_value(&["act_", "escalation"]);
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 109,
                stage: &removed_upgrade_value,
                decision: "execute",
                strategy: Some("durable"),
                source: "classifier",
                classifier_outcome: "accepted",
                classifier_evidence: true,
            },
        )
        .await?;
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 111,
                stage: "initial",
                decision: "execute",
                strategy: Some("durable"),
                source: &removed_upgrade_value,
                classifier_outcome: "not_called",
                classifier_evidence: false,
            },
        )
        .await?;
        let removed_context_fallback =
            removed_serialized_value(&["context_forced_", "act"]);
        assert_route_audit_insert_rejected(
            &target,
            InvalidRouteAuditCell {
                sequence: 112,
                stage: "initial",
                decision: "execute",
                strategy: Some("inline"),
                source: "classifier",
                classifier_outcome: &removed_context_fallback,
                classifier_evidence: true,
            },
        )
        .await?;
        let removed_mode_insert = format!(
            "INSERT INTO moa.execution_route_audit ({removed_run_mode_column}) VALUES ('run')"
        );
        let removed_mode_error = target
            .execute(removed_mode_insert.as_str())
            .await
            .expect_err("removed route-audit mode column must reject SQL writes");
        let removed_mode_sql_state = removed_mode_error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .map(|code| code.into_owned());
        let invalid_insert_residue: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_route_audit")
                .fetch_one(&target)
                .await?;
        let old_route_envelope_valid: bool = sqlx::query_scalar(
            r#"
            SELECT moa.execution_planning_audit_envelope_is_valid(
                jsonb_build_object(
                    'schema_version',1,
                    'tenant_id','00000000-0000-0000-0000-000000337001',
                    'contact_id',NULL,
                    'session_id','00000000-0000-0000-0000-000000337002',
                    'originating_sequence',1,
                    'payload',jsonb_build_object(
                        'kind','route','stage','initial','decision','routed',
                        'mode','run','reason','explicit_run',
                        'accepted_at','2026-01-01T00:00:00Z'
                    )
                )
            )
            "#,
        )
        .fetch_one(&target)
        .await?;

        let valid_terminal_cells: i64 = sqlx::query_scalar(
            r#"
            WITH completion(status,limit_stop,expected) AS (
                VALUES
                ('completed',NULL::TEXT,'completed'),
                ('blocked',NULL,'blocked'),
                ('unsupported',NULL,'unsupported_plan'),
                ('partial',NULL,'goal_incomplete'),
                ('partial','budget_exceeded','budget_exceeded'),
                ('partial','deadline_exceeded','deadline_exceeded'),
                ('failed',NULL,'goal_incomplete'),
                ('failed','budget_exceeded','budget_exceeded'),
                ('failed','deadline_exceeded','deadline_exceeded')
            ), failure_class(value) AS (
                VALUES
                ('retryable'),('dependency_failed'),('invalid_input'),
                ('invalid_output'),('authorization_denied'),('budget_exceeded'),
                ('deadline_exceeded'),('cancelled'),('unsupported'),('terminal')
            ), task_failure AS (
                SELECT
                    status,
                    jsonb_build_object(
                        'kind','task_failure','class',failure_class.value
                    ) AS cause,
                    CASE status
                        WHEN 'partial' THEN CASE failure_class.value
                            WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                            WHEN 'budget_exceeded' THEN 'budget_exceeded'
                            ELSE 'goal_incomplete'
                        END
                        WHEN 'blocked' THEN 'blocked'
                        WHEN 'unsupported' THEN 'unsupported_plan'
                        WHEN 'failed' THEN CASE failure_class.value
                            WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                            WHEN 'budget_exceeded' THEN 'budget_exceeded'
                            ELSE 'task_failure'
                        END
                    END AS expected
                FROM (
                    VALUES ('partial'),('blocked'),('unsupported'),('failed')
                ) projection(status)
                CROSS JOIN failure_class
            ), replan_reason(value) AS (
                VALUES
                ('duplicate_plan'),('duplicate_amendment'),('repeated_failure'),
                ('no_progress'),('deadline_exceeded'),('budget_exhausted')
            ), cells(status,cause,source_kind,expected) AS (
                SELECT
                    status,
                    jsonb_build_object(
                        'kind','completion','limit_stop',limit_stop
                    ),
                    'generated_plan',
                    expected
                FROM completion
                UNION ALL
                SELECT status,cause,'generated_plan',expected FROM task_failure
                UNION ALL
                SELECT
                    status,'{"kind":"scheduler_no_progress"}'::JSONB,
                    'generated_plan',
                    CASE status
                        WHEN 'unsupported' THEN 'unsupported_plan'
                        ELSE 'no_progress'
                    END
                FROM (
                    VALUES ('partial'),('blocked'),('unsupported'),('failed')
                ) projection(status)
                UNION ALL
                SELECT
                    status,
                    jsonb_build_object(
                        'kind','replan_stop','reason',replan_reason.value
                    ),
                    'generated_plan',
                    replan_reason.value
                FROM (VALUES ('partial'),('blocked')) projection(status)
                CROSS JOIN replan_reason
                UNION ALL
                SELECT
                    status,
                    jsonb_build_object('kind','limit_stop','reason',reason),
                    'generated_plan',
                    reason
                FROM (VALUES ('partial'),('failed')) projection(status)
                CROSS JOIN (
                    VALUES ('deadline_exceeded'),('budget_exceeded')
                ) limit_reason(reason)
                UNION ALL
                VALUES
                ('cancelled','{"kind":"cancellation"}'::JSONB,
                    'skill_template','cancelled'),
                ('failed','{"kind":"internal_failure"}'::JSONB,
                    'experiment_template','internal_failure')
            )
            SELECT COUNT(*)
            FROM cells
            WHERE moa.execution_terminal_reason_for(status,cause,source_kind)
                = expected
            "#,
        )
        .fetch_one(&target)
        .await?;
        let invalid_terminal_cells: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM (
                VALUES
                ('completed',
                    '{"kind":"task_failure","class":"retryable"}'::JSONB,
                    'generated_plan'),
                ('failed','{"kind":"cancellation"}','generated_plan'),
                ('partial','{"kind":"internal_failure"}','generated_plan'),
                ('completed',
                    '{"kind":"legacy_migration","was_nonterminal":true}',
                    'legacy_migration'),
                ('unsupported',
                    '{"kind":"replan_stop","reason":"no_progress"}',
                    'generated_plan'),
                ('completed',
                    '{"kind":"completion","limit_stop":null,"extra":true}',
                    'generated_plan'),
                ('failed',
                    '{"kind":"task_failure","class":"not_a_class"}',
                    'generated_plan')
            ) cell(status,cause,source_kind)
            WHERE moa.execution_terminal_reason_for(
                status,cause,source_kind
            ) IS NULL
            "#,
        )
        .fetch_one(&target)
        .await?;

        let provenance_matrix: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','generated_plan',
                        'planner',jsonb_build_object(
                            'model','m','prompt_version','p',
                            'candidate_hash',repeat('1',64),
                            'compiler_report_hash',repeat('2',64),
                            'final_plan_hash',repeat('3',64),
                            'repair_attempts',0
                        )
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','skill_template',
                        'skill_template_ref','skill://proof',
                        'skill_template_revision_uid',
                            '00000000-0000-0000-0000-000000337031'
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','experiment_template',
                        'skill_template_ref','skill://proof',
                        'skill_template_revision_uid',
                            '00000000-0000-0000-0000-000000337031',
                        'experiment_run_uid',
                            '00000000-0000-0000-0000-000000337032',
                        'score_run_id',
                            '00000000-0000-0000-0000-000000337033',
                        'trial_uid',NULL
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','generated_plan',
                        'route_rationale','The workflow requires durable execution.',
                        'planner',jsonb_build_object(
                            'model','m','prompt_version','p',
                            'candidate_hash',repeat('1',64),
                            'compiler_report_hash',repeat('2',64),
                            'final_plan_hash',repeat('3',64),
                            'repair_attempts',0
                        )
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','generated_plan',
                        'planner',jsonb_build_object(
                            'model','m','prompt_version','p',
                            'candidate_hash',repeat('1',64),
                            'compiler_report_hash',repeat('2',64),
                            'final_plan_hash',repeat('3',64),
                            'repair_attempts',0
                        ),
                        'skill_template_ref','skill://cross-cohort'
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','experiment_template',
                        'skill_template_ref','skill://proof',
                        'skill_template_revision_uid',
                            '00000000-0000-0000-0000-000000337031',
                        'experiment_run_uid',
                            '00000000-0000-0000-0000-000000337032',
                        'score_run_id',
                            '00000000-0000-0000-0000-000000337032',
                        'trial_uid',NULL
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                )
            "#,
        )
        .fetch_one(&target)
        .await?;

        let json_vectors: (bool, bool, bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                moa.execution_json_text_is_canonical('{"a":1,"b":[0,true]}'),
                moa.execution_json_text_is_canonical('{"a":1,"a":2}'),
                moa.execution_json_text_is_canonical('{"a":1.0}'),
                moa.execution_json_text_is_canonical('{"a":-0}'),
                moa.execution_json_text_is_canonical('{"a":0}')
            "#,
        )
        .fetch_one(&target)
        .await?;
        let trace_vectors: (bool, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                moa.execution_traceparent_is_valid(
                    '00-11111111111111111111111111111111-2222222222222222-01'
                ),
                moa.execution_traceparent_is_valid(
                    '00-00000000000000000000000000000000-2222222222222222-01'
                ),
                moa.execution_traceparent_is_valid(
                    '00-11111111111111111111111111111111-2222222222222222-04'
                ),
                moa.execution_tracestate_is_valid(
                    '1foo=bar,,a@b@c= value'
                ),
                moa.execution_tracestate_is_normalized(E', \t,'),
                moa.execution_tracestate_is_valid('a=1,a=2'),
                moa.execution_tracestate_is_valid(repeat(',',32))
            "#,
        )
        .fetch_one(&target)
        .await?;

        sqlx::raw_sql(
            r#"
            INSERT INTO moa.execution_planning_context (
                planning_context_uid,tenant_id,contact_id,session_id,
                originating_user_sequence_num,originating_user_event_hash,
                owner_user_id,planning_context_hash,snapshot
            ) VALUES (
                '00000000-0000-0000-0000-000000337040',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337010',11,repeat('4',64),
                'owner',repeat('5',64),'{}'
            );
            INSERT INTO moa.execution_run (
                run_uid,tenant_id,contact_id,session_id,
                originating_user_sequence_num,planning_context_uid,
                planning_context_hash,owner_user_id,goal_contract,
                initial_plan,active_plan,initial_plan_hash,active_plan_hash,
                capability_catalog,authorization_envelope,source_provenance,input,
                status,source_kind
            ) VALUES (
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337010',11,
                '00000000-0000-0000-0000-000000337040',repeat('5',64),
                'owner','{"requirements":[],"completion_checks":[]}',
                '{}','{}',repeat('3',64),repeat('3',64),'{}','{}',
                jsonb_build_object(
                    'kind','generated_plan',
                    'planner',jsonb_build_object(
                        'model','m','prompt_version','p',
                        'candidate_hash',repeat('1',64),
                        'compiler_report_hash',repeat('2',64),
                        'final_plan_hash',repeat('3',64),
                        'repair_attempts',0
                    )
                ),
                '{}','queued','generated_plan'
            );
            INSERT INTO moa.execution_task (
                task_id,run_uid,tenant_id,contact_id,node_id,item_key,
                plan_revision,status,input,task_kind,retry_policy,
                estimate_cost_microusd,estimate_tokens,estimate_tasks,
                estimate_tool_calls,estimate_retrieved_bytes
            ) VALUES (
                '00000000-0000-0000-0000-000000337042',
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337020',NULL,
                'output','result',1,'pending','{}',
                '{"kind":"output","value":null}',
                '{"max_attempts":1,"initial_backoff_ms":0,"max_backoff_ms":0}',
                0,0,1,0,0
            );
            INSERT INTO moa.execution_action_review_outbox (
                review_uid,tenant_id,contact_id,run_uid,task_id,generation,
                resolution,traceparent,tracestate,task_traceparent,task_tracestate
            ) VALUES (
                '00000000-0000-0000-0000-000000337043',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337042',1,'{}',
                '00-11111111111111111111111111111111-2222222222222222-01',
                'a=one',
                '00-33333333333333333333333333333333-4444444444444444-00',
                'b=two'
            );
            INSERT INTO tenant_action_reviews (
                id,storage_partition_id,user_id,session_id,worker_id,tool_call_id,
                tool_name,action_class,risk_level,input_summary,normalized_input,
                envelope,preview,tool_request,requested_by,tenant_id,
                execution_task_traceparent,execution_task_tracestate
            ) VALUES (
                '00000000-0000-0000-0000-000000337044',
                '00000000-0000-0000-0000-000000337020',NULL,NULL,NULL,
                '00000000-0000-0000-0000-000000337045',
                'proof','write','high','proof','{}','{}','{}','{}','owner',
                '00000000-0000-0000-0000-000000337020',
                '00-33333333333333333333333333333333-4444444444444444-00',
                'b=two'
            );
            "#,
        )
        .execute(&target)
        .await?;

        let first_run_seq: i64 = sqlx::query_scalar(
            "SELECT analytics_change_seq FROM moa.execution_run \
             WHERE run_uid = '00000000-0000-0000-0000-000000337041'",
        )
        .fetch_one(&target)
        .await?;
        target
            .execute(
                "UPDATE moa.execution_run SET idempotency_key = 'seq-proof' \
                 WHERE run_uid = '00000000-0000-0000-0000-000000337041'",
            )
            .await?;
        let second_run_seq: i64 = sqlx::query_scalar(
            "SELECT analytics_change_seq FROM moa.execution_run \
             WHERE run_uid = '00000000-0000-0000-0000-000000337041'",
        )
        .fetch_one(&target)
        .await?;

        target
            .execute(
                "INSERT INTO moa.execution_planning_context (\
                    planning_context_uid,tenant_id,contact_id,session_id,\
                    originating_user_sequence_num,originating_user_event_hash,\
                    owner_user_id,planning_context_hash,snapshot\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337050',\
                    '00000000-0000-0000-0000-000000337020',\
                    '00000000-0000-0000-0000-000000337051',\
                    '00000000-0000-0000-0000-000000337010',12,repeat('6',64),\
                    'owner',repeat('7',64),'{}'\
                 )",
            )
            .await?;
        let planning_context_scope_rejected = target
            .execute(
                "INSERT INTO moa.execution_run (\
                    run_uid,tenant_id,contact_id,session_id,\
                    originating_user_sequence_num,planning_context_uid,\
                    planning_context_hash,owner_user_id,goal_contract,\
                    initial_plan,active_plan,initial_plan_hash,active_plan_hash,\
                    capability_catalog,authorization_envelope,source_provenance,input,\
                    status,source_kind\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337052',\
                    '00000000-0000-0000-0000-000000337020',NULL,\
                    '00000000-0000-0000-0000-000000337010',12,\
                    '00000000-0000-0000-0000-000000337050',repeat('7',64),\
                    'owner','{\"requirements\":[],\"completion_checks\":[]}',\
                    '{}','{}',repeat('3',64),repeat('3',64),'{}','{}',\
                    jsonb_build_object(\
                        'kind','generated_plan',\
                        'planner',jsonb_build_object(\
                            'model','m','prompt_version','p',\
                            'candidate_hash',repeat('1',64),\
                            'compiler_report_hash',repeat('2',64),\
                            'final_plan_hash',repeat('3',64),\
                            'repair_attempts',0\
                        )\
                    ),\
                    '{}','queued','generated_plan'\
                 )",
            )
            .await
            .is_err();
        let task_scope_rejected = target
            .execute(
                "INSERT INTO moa.execution_task (\
                    task_id,run_uid,tenant_id,contact_id,node_id,item_key,\
                    plan_revision,status,input,task_kind,retry_policy,\
                    estimate_cost_microusd,estimate_tokens,estimate_tasks,\
                    estimate_tool_calls,estimate_retrieved_bytes\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337053',\
                    '00000000-0000-0000-0000-000000337041',\
                    '00000000-0000-0000-0000-000000337020',\
                    '00000000-0000-0000-0000-000000337051',\
                    'peer','peer',1,'pending','{}',\
                    '{\"kind\":\"output\",\"value\":null}',\
                    '{\"max_attempts\":1,\"initial_backoff_ms\":0,\"max_backoff_ms\":0}',\
                    0,0,1,0,0\
                 )",
            )
            .await
            .is_err();
        let outbox_scope_rejected = target
            .execute(
                "INSERT INTO moa.execution_action_review_outbox (\
                    review_uid,tenant_id,contact_id,run_uid,task_id,generation,resolution\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337054',\
                    '00000000-0000-0000-0000-000000337020',\
                    '00000000-0000-0000-0000-000000337051',\
                    '00000000-0000-0000-0000-000000337041',\
                    '00000000-0000-0000-0000-000000337042',1,'{}'\
                 )",
            )
            .await
            .is_err();

        target
            .execute(
                "UPDATE moa.execution_action_review_outbox \
                 SET attempt_count = attempt_count + 1 \
                 WHERE review_uid = '00000000-0000-0000-0000-000000337043'",
            )
            .await?;
        let outbox_trace_mutation_rejected = target
            .execute(
                "UPDATE moa.execution_action_review_outbox \
                 SET tracestate = 'a=changed' \
                 WHERE review_uid = '00000000-0000-0000-0000-000000337043'",
            )
            .await
            .is_err();
        target
            .execute(
                "UPDATE tenant_action_reviews SET status = 'cleared' \
                 WHERE id = '00000000-0000-0000-0000-000000337044'",
            )
            .await?;
        let review_trace_mutation_rejected = target
            .execute(
                "UPDATE tenant_action_reviews \
                 SET execution_task_tracestate = 'b=changed' \
                 WHERE id = '00000000-0000-0000-0000-000000337044'",
            )
            .await
            .is_err();
        target
            .execute(
                "REFRESH MATERIALIZED VIEW analytics.execution_run_fact; \
                 REFRESH MATERIALIZED VIEW analytics.execution_task_fact;",
            )
            .await?;
        let run_fact: (String, i64, i64, String) = sqlx::query_as(
            "SELECT source_kind, requirement_count, completion_check_count, \
                    active_plan_hash \
             FROM analytics.execution_run_fact \
             WHERE run_uid = '00000000-0000-0000-0000-000000337041'",
        )
        .fetch_one(&target)
        .await?;
        let task_fact_id: String = sqlx::query_scalar(
            "SELECT task_id::TEXT FROM analytics.execution_task_fact \
             WHERE task_id = '00000000-0000-0000-0000-000000337042'",
        )
        .fetch_one(&target)
        .await?;

        target
            .execute(
                "INSERT INTO analytics.clickhouse_schema_upgrade_state (\
                    upgrade_key,database_uuid,run_table_uuid,task_table_uuid,\
                    stage,upgrade_version,export_version_floor,\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    run_page_seq,run_page_id,task_page_seq,task_page_id\
                 ) VALUES (\
                    'execution_dimensions',\
                    '00000000-0000-0000-0000-000000337001',\
                    '00000000-0000-0000-0000-000000337002',\
                    '00000000-0000-0000-0000-000000337003',\
                    'pending',NOW(),NOW(),\
                    10,'00000000-0000-0000-0000-000000000010',\
                    20,'00000000-0000-0000-0000-000000000020',\
                    0,'00000000-0000-0000-0000-000000000000',\
                    0,'00000000-0000-0000-0000-000000000000'\
                 )",
            )
            .await?;
        let skipped_stage_rejected = target
            .execute(
                "UPDATE analytics.clickhouse_schema_upgrade_state \
                 SET stage = 'runs_exported', updated_at = NOW() \
                 WHERE upgrade_key = 'execution_dimensions'",
            )
            .await
            .is_err();
        let backward_page_rejected = target
            .execute(
                "UPDATE analytics.clickhouse_schema_upgrade_state \
                 SET run_page_seq = -1, updated_at = NOW() \
                 WHERE upgrade_key = 'execution_dimensions'",
            )
            .await
            .is_err();

        let database_identity_change_rejected = target
            .execute(
                "INSERT INTO analytics.clickhouse_schema_upgrade_state (\
                    upgrade_key,generation,database_uuid,run_table_uuid,task_table_uuid,\
                    stage,upgrade_version,export_version_floor,\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    run_page_seq,run_page_id,task_page_seq,task_page_id\
                 ) SELECT \
                    upgrade_key,2,'00000000-0000-0000-0000-000000337011',\
                    '00000000-0000-0000-0000-000000337012',\
                    '00000000-0000-0000-0000-000000337013',\
                    'pending',export_version_floor + INTERVAL '1 microsecond',\
                    export_version_floor + INTERVAL '1 microsecond',\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    0,'00000000-0000-0000-0000-000000000000',\
                    0,'00000000-0000-0000-0000-000000000000'\
                 FROM analytics.clickhouse_schema_upgrade_state\
                 WHERE upgrade_key = 'execution_dimensions' AND generation = 1",
            )
            .await
            .is_err();
        let partial_table_identity_change_rejected = target
            .execute(
                "INSERT INTO analytics.clickhouse_schema_upgrade_state (\
                    upgrade_key,generation,database_uuid,run_table_uuid,task_table_uuid,\
                    stage,upgrade_version,export_version_floor,\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    run_page_seq,run_page_id,task_page_seq,task_page_id\
                 ) SELECT \
                    upgrade_key,2,database_uuid,\
                    '00000000-0000-0000-0000-000000337012',task_table_uuid,\
                    'pending',export_version_floor + INTERVAL '1 microsecond',\
                    export_version_floor + INTERVAL '1 microsecond',\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    0,'00000000-0000-0000-0000-000000000000',\
                    0,'00000000-0000-0000-0000-000000000000'\
                 FROM analytics.clickhouse_schema_upgrade_state\
                 WHERE upgrade_key = 'execution_dimensions' AND generation = 1",
            )
            .await
            .is_err();

        let partial_pass_rejected = target
            .execute(
                "INSERT INTO analytics.clickhouse_export_state (\
                    table_name,cursor_ts,cursor_id,exported_at,cursor_seq,\
                    pass_high_water_seq\
                 ) VALUES (\
                    'invalid_execution_pass',to_timestamp(0),\
                    '00000000-0000-0000-0000-000000000000',to_timestamp(0),0,1\
                 )",
            )
            .await
            .is_err();

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            audit_counts,
            route_schema_contract,
            valid_route_cells,
            invalid_route_cell,
            removed_mode_sql_state,
            invalid_insert_residue,
            old_route_envelope_valid,
            valid_terminal_cells,
            invalid_terminal_cells,
            provenance_matrix,
            json_vectors,
            trace_vectors,
            first_run_seq,
            second_run_seq,
            planning_context_scope_rejected,
            task_scope_rejected,
            outbox_scope_rejected,
            outbox_trace_mutation_rejected,
            review_trace_mutation_rejected,
            run_fact,
            task_fact_id,
            skipped_stage_rejected,
            backward_page_rejected,
            database_identity_change_rejected,
            partial_table_identity_change_rejected,
            partial_pass_rejected,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        first,
        second,
        audit_counts,
        route_schema_contract,
        valid_route_cells,
        invalid_route_cell,
        removed_mode_sql_state,
        invalid_insert_residue,
        old_route_envelope_valid,
        valid_terminal_cells,
        invalid_terminal_cells,
        provenance_matrix,
        json_vectors,
        trace_vectors,
        first_run_seq,
        second_run_seq,
        planning_context_scope_rejected,
        task_scope_rejected,
        outbox_scope_rejected,
        outbox_trace_mutation_rejected,
        review_trace_mutation_rejected,
        run_fact,
        task_fact_id,
        skipped_stage_rejected,
        backward_page_rejected,
        database_identity_change_rejected,
        partial_table_identity_change_rejected,
        partial_pass_rejected,
    ) = outcome.expect("execution analytics staged contract should execute on PostgreSQL");

    assert_eq!(
        first,
        expected_migration_labels(),
        "execution analytics behavior must be exercised on the complete final schema"
    );
    assert!(second.is_empty(), "the complete schema must not reapply");
    assert_eq!(audit_counts, (0, 0, 0));
    assert_eq!(route_schema_contract, (true, true, true, true, true, true));
    assert_eq!(valid_route_cells, 7);
    assert!(!invalid_route_cell);
    assert_eq!(removed_mode_sql_state.as_deref(), Some("42703"));
    assert_eq!(invalid_insert_residue, 0);
    assert!(!old_route_envelope_valid);
    assert_eq!(valid_terminal_cells, 71);
    assert_eq!(invalid_terminal_cells, 7);
    assert_eq!(provenance_matrix, (true, true, true, false, false, false));
    assert_eq!(json_vectors, (true, false, false, false, true));
    assert_eq!(
        trace_vectors,
        (true, false, false, true, false, false, false)
    );
    assert!(second_run_seq > first_run_seq);
    assert!(planning_context_scope_rejected);
    assert!(task_scope_rejected);
    assert!(outbox_scope_rejected);
    assert!(outbox_trace_mutation_rejected);
    assert!(review_trace_mutation_rejected);
    assert_eq!(
        run_fact,
        ("generated_plan".to_string(), 0, 0, "3".repeat(64))
    );
    assert_eq!(task_fact_id, "00000000-0000-0000-0000-000000337042");
    assert!(skipped_stage_rejected);
    assert!(backward_page_rejected);
    assert!(database_identity_change_rejected);
    assert!(partial_table_identity_change_rejected);
    assert!(partial_pass_rejected);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn refinery_clean_apply_gives_agent_principals_generated_ids_db() {
    // Pins: the agent baseline installs the ID default that the production
    // registration repository relies on.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create agent-default migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let default: Option<String> = sqlx::query_scalar(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = 'agents' AND column_name = 'id'",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(default)
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let default = outcome.expect("inspect clean agent migration");
    assert_eq!(default.as_deref(), Some("gen_random_uuid()"));
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn full_database_runner_installs_execution_schema_and_foreign_keys_db() {
    // Pins: the canonical runner installs the final execution relations and
    // experiment foreign keys without recreating procedure-era relations.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create full-database migration proof");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;
        let recorded_cutovers: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM refinery_schema_history \
             WHERE name IN ('execution_runs', 'execution_analytics') ORDER BY version",
        )
        .fetch_all(&target)
        .await?;
        let legacy_tables_absent: bool = sqlx::query_scalar(
            "SELECT to_regclass('moa.artifact_run') IS NULL \
                 AND to_regclass('moa.artifact_node_run') IS NULL",
        )
        .fetch_one(&target)
        .await?;
        let post_cutover_relations_present: bool = sqlx::query_scalar(
            "SELECT to_regclass('moa.execution_run') IS NOT NULL \
                 AND to_regclass('moa.execution_task') IS NOT NULL \
                 AND to_regclass('moa.execution_route_audit') IS NOT NULL \
                 AND to_regclass('analytics.execution_run_fact') IS NOT NULL \
                 AND to_regclass('analytics.execution_task_fact') IS NOT NULL",
        )
        .fetch_one(&target)
        .await?;
        let normalized_columns_present: bool = sqlx::query_scalar(
            "SELECT COUNT(*) = 4 FROM information_schema.columns \
             WHERE table_schema = 'moa' \
               AND ((table_name = 'execution_run' \
                     AND column_name IN ('source_kind', 'terminal_reason')) \
                 OR (table_name IN ('experiment_run', 'experiment_trial') \
                     AND column_name = 'execution_run_uid'))",
        )
        .fetch_one(&target)
        .await?;
        let session_fk_targets = foreign_key_targets(
            &target,
            &[
                "experiment_run_session_id_fkey",
                "experiment_trial_session_id_fkey",
            ],
        )
        .await?;
        let execution_fk_targets = foreign_key_targets(
            &target,
            &[
                "experiment_run_execution_scope_fkey",
                "experiment_trial_execution_scope_fkey",
            ],
        )
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            recorded_cutovers,
            legacy_tables_absent,
            post_cutover_relations_present,
            normalized_columns_present,
            session_fk_targets,
            execution_fk_targets,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        first,
        second,
        recorded_cutovers,
        legacy_tables_absent,
        post_cutover_relations_present,
        normalized_columns_present,
        session_fk_targets,
        execution_fk_targets,
    ) = outcome.expect("canonical full-database migration should install the final schema");
    assert_eq!(
        recorded_cutovers,
        vec![
            "execution_runs".to_string(),
            "execution_analytics".to_string(),
        ],
        "the semantic final execution migrations must be recorded exactly once"
    );
    assert_eq!(first, expected_migration_labels());
    assert!(
        second.is_empty(),
        "the second canonical apply must be a no-op: {second:?}"
    );
    assert!(legacy_tables_absent);
    assert!(post_cutover_relations_present);
    assert!(normalized_columns_present);
    assert_eq!(session_fk_targets, vec!["public.sessions"; 2]);
    assert_eq!(execution_fk_targets, vec!["moa.execution_run"; 2]);
}

async fn foreign_key_targets(
    pool: &PgPool,
    constraints: &[&str],
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut targets = Vec::with_capacity(constraints.len());
    for constraint in constraints {
        let target: String = sqlx::query_scalar(
            "SELECT referenced_ns.nspname || '.' || referenced.relname \
             FROM pg_constraint c \
             JOIN pg_class referenced ON referenced.oid = c.confrelid \
             JOIN pg_namespace referenced_ns ON referenced_ns.oid = referenced.relnamespace \
             WHERE c.conname = $1 AND c.connamespace = 'moa'::regnamespace",
        )
        .bind(constraint)
        .fetch_one(pool)
        .await?;
        targets.push(target);
    }
    Ok(targets)
}

/// Collects the credential-vault schema facts a fresh database must expose.
///
/// Returns `(versions_forced_rls, operations_forced_rls, policy_names,
/// active_index_is_partial_unique, moa_app_update_on_audit)`.
async fn tenant_credential_vault_schema_facts(
    pool: &PgPool,
) -> Result<(bool, bool, Vec<String>, bool, bool), Box<dyn std::error::Error + Send + Sync>> {
    let versions_forced: bool = sqlx::query_scalar(
        "SELECT relforcerowsecurity FROM pg_class WHERE relname = 'tenant_credential_versions'",
    )
    .fetch_one(pool)
    .await?;
    let operations_forced: bool = sqlx::query_scalar(
        "SELECT relforcerowsecurity FROM pg_class WHERE relname = 'tenant_credential_operations'",
    )
    .fetch_one(pool)
    .await?;
    let policies: Vec<String> = sqlx::query_scalar(
        "SELECT policyname::TEXT FROM pg_policies
         WHERE tablename IN ('tenant_credential_versions', 'tenant_credential_operations')
         ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;
    let active_partial_unique: bool = sqlx::query_scalar(
        "SELECT COUNT(*) = 1 FROM pg_indexes
         WHERE indexname = 'tenant_credential_versions_one_active'
           AND indexdef LIKE '%UNIQUE%'
           AND indexdef LIKE '%WHERE active%'",
    )
    .fetch_one(pool)
    .await?;
    let audit_update_granted: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM information_schema.role_table_grants
         WHERE table_name = 'tenant_credential_operations'
           AND grantee = 'moa_app'
           AND privilege_type = 'UPDATE'",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        versions_forced,
        operations_forced,
        policies,
        active_partial_unique,
        audit_update_granted,
    ))
}

async fn knowledge_link_claim_schema_facts(
    pool: &PgPool,
) -> Result<(bool, Vec<String>, bool, bool), Box<dyn std::error::Error + Send + Sync>> {
    let claims_forced: bool = sqlx::query_scalar(
        "SELECT relforcerowsecurity FROM pg_class WHERE relname = 'knowledge_link_claims'",
    )
    .fetch_one(pool)
    .await?;
    let policies: Vec<String> = sqlx::query_scalar(
        "SELECT policyname::TEXT FROM pg_policies
         WHERE tablename = 'knowledge_link_claims'
         ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;
    // A finalized claim must name the run whose trigger proved durable.
    let finalized_requires_run: bool = sqlx::query_scalar(
        "SELECT COUNT(*) = 1 FROM pg_constraint
         WHERE conname = 'knowledge_link_claims_finalized_has_sync_run'",
    )
    .fetch_one(pool)
    .await?;
    let trigger_boundary_column: bool = sqlx::query_scalar(
        "SELECT COUNT(*) = 1 FROM information_schema.columns
         WHERE table_schema = 'moa'
           AND table_name = 'knowledge_sync_runs'
           AND column_name = 'provider_trigger_completed_at'",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        claims_forced,
        policies,
        finalized_requires_run,
        trigger_boundary_column,
    ))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_link_claims_final_schema_is_strict_and_idempotent_db() {
    // Pins: knowledge-link claims bootstraps the link claim table on a pristine database and
    // re-applies as a no-op, and installs the two properties the durable link
    // depends on — strict forced-RLS tenant isolation with no control-plane
    // branch, and a database-owned rule that a finalized claim always names the
    // sync run whose provider trigger was proven durable.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = knowledge_link_claim_schema_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("link claim migration should apply on a fresh database");
    let (claims_forced, policies, finalized_requires_run, trigger_boundary_column) = facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("knowledge_link_claims")),
        "a pristine database must apply knowledge-link claims, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(
        claims_forced,
        "knowledge_link_claims must FORCE row level security"
    );
    assert_eq!(
        policies,
        vec!["tenant_isolation".to_string()],
        "the claim table must expose exactly one strict tenant-isolation policy"
    );
    assert!(
        finalized_requires_run,
        "a finalized claim must be unable to exist without its durable sync run"
    );
    assert!(
        trigger_boundary_column,
        "sync runs must carry the durable provider-trigger boundary"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn tenant_credential_vault_final_schema_is_strict_and_idempotent_db() {
    // Pins: tenant credential vault bootstraps the durable credential owner on a pristine
    // database and re-applies as a no-op, and the schema it installs carries the
    // security properties the vault depends on — forced RLS on both tables, one
    // active version per series, and an audit table an ordinary role cannot
    // rewrite (no UPDATE grant, no UPDATE policy).
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = tenant_credential_vault_schema_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("credential-vault migration should apply on a fresh database");
    let (versions_forced, operations_forced, policies, active_partial_unique, audit_update_granted) =
        facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("tenant_credential_vault")),
        "a pristine database must apply tenant credential vault, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(
        versions_forced,
        "tenant_credential_versions must FORCE row level security"
    );
    assert!(
        operations_forced,
        "tenant_credential_operations must FORCE row level security"
    );
    assert_eq!(
        policies,
        vec![
            "audit_purge_delete".to_string(),
            "audit_tenant_append".to_string(),
            "audit_tenant_read".to_string(),
            "tenant_isolation".to_string(),
        ],
        "the audit table must expose exactly read/append/purge-delete policies and no UPDATE policy"
    );
    assert!(
        active_partial_unique,
        "one active credential version per series must be database-owned"
    );
    assert!(
        !audit_update_granted,
        "the append-only audit must not grant UPDATE to the application role"
    );
}

async fn knowledge_occurrence_schema_facts(
    pool: &PgPool,
) -> Result<(bool, bool, bool, bool, bool), Box<dyn std::error::Error + Send + Sync>> {
    let not_null: bool = sqlx::query_scalar(
        "SELECT attnotnull FROM pg_attribute \
          WHERE attrelid = 'moa.knowledge_chunks'::REGCLASS AND attname = 'graph_node_uid'",
    )
    .fetch_one(pool)
    .await?;
    let equality_constraint: bool = sqlx::query_scalar(
        "SELECT count(*) = 1 FROM pg_constraint \
          WHERE conname = 'knowledge_chunks_graph_node_is_occurrence' \
            AND pg_get_constraintdef(oid) LIKE '%graph_node_uid = chunk_uid%'",
    )
    .fetch_one(pool)
    .await?;
    let occurrence_unique: bool = sqlx::query_scalar(
        "SELECT count(*) = 1 FROM pg_indexes \
          WHERE indexname = 'knowledge_chunks_graph_node_occurrence_uniq' \
            AND indexdef LIKE '%UNIQUE%'",
    )
    .fetch_one(pool)
    .await?;
    let content_hash_unique_removed: bool = sqlx::query_scalar(
        "SELECT count(*) = 0 FROM pg_indexes WHERE indexname = 'knowledge_chunks_hash_uniq'",
    )
    .fetch_one(pool)
    .await?;
    let force_rls: bool = sqlx::query_scalar(
        "SELECT relforcerowsecurity FROM pg_class WHERE oid = 'moa.knowledge_chunks'::REGCLASS",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        not_null,
        equality_constraint,
        occurrence_unique,
        content_hash_unique_removed,
        force_rls,
    ))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_graph_occurrences_final_schema_owns_identity_db() {
    // Pins: knowledge occurrence identity installs the occurrence invariant on a pristine database and
    // re-applies as a no-op. The invariant is database-owned — `graph_node_uid` is
    // NOT NULL and constrained equal to `chunk_uid`, one graph uid can belong to
    // exactly one chunk row, and content-hash uniqueness no longer constrains how
    // many occurrences a document version may hold.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = knowledge_occurrence_schema_facts(&pool).await?;
        let policies: Vec<String> = sqlx::query_scalar(
            "SELECT policyname::TEXT FROM pg_policies \
              WHERE schemaname = 'moa' AND tablename = 'knowledge_chunks' ORDER BY policyname",
        )
        .fetch_all(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts, policies))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts, policies) =
        outcome.expect("occurrence migration should apply on a fresh database");
    let (not_null, equality_constraint, occurrence_unique, content_hash_unique_removed, force_rls) =
        facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("tenant_knowledge_base")),
        "the occurrence invariant must originate in the tenant knowledge baseline: {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(not_null, "graph_node_uid must be NOT NULL");
    assert!(
        equality_constraint,
        "the database must own `graph_node_uid = chunk_uid`"
    );
    assert!(
        occurrence_unique,
        "one graph uid must belong to exactly one chunk row"
    );
    assert!(
        content_hash_unique_removed,
        "content-hash uniqueness must no longer limit occurrences per document version"
    );
    assert!(
        force_rls,
        "knowledge_chunks must keep forced row level security"
    );
    assert_eq!(
        policies,
        vec!["tenant_isolation".to_string()],
        "tenant isolation must survive the occurrence migration"
    );
}

async fn source_acl_schema_facts(
    pool: &PgPool,
) -> Result<
    (
        Vec<(String, bool)>,
        Vec<String>,
        Vec<String>,
        bool,
        bool,
        Vec<(
            String,
            String,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
        )>,
        bool,
        bool,
        bool,
        bool,
        bool,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let forced_rls = sqlx::query_as::<_, (String, bool)>(
        "SELECT relname::TEXT, relforcerowsecurity FROM pg_class AS class \
           JOIN pg_namespace AS namespace ON namespace.oid = class.relnamespace \
          WHERE namespace.nspname = 'moa' \
            AND relname IN ( \
                'knowledge_source_acl_keys', \
                'knowledge_source_acl_epochs', \
                'knowledge_source_acl_snapshots', \
                'knowledge_source_acl_entries', \
                'knowledge_source_principal_bindings', \
                'knowledge_source_principal_group_bindings') \
          ORDER BY relname",
    )
    .fetch_all(pool)
    .await?;
    let snapshot_policies: Vec<String> = sqlx::query_scalar(
        "SELECT policyname::TEXT FROM pg_policies \
          WHERE schemaname = 'moa' AND tablename = 'knowledge_source_acl_snapshots' \
          ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;
    let entry_policies: Vec<String> = sqlx::query_scalar(
        "SELECT policyname::TEXT FROM pg_policies \
          WHERE schemaname = 'moa' AND tablename = 'knowledge_source_acl_entries' \
          ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;
    let snapshot_update_granted: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('moa_app', 'moa.knowledge_source_acl_snapshots', 'UPDATE')",
    )
    .fetch_one(pool)
    .await?;
    let entry_update_granted: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('moa_app', 'moa.knowledge_source_acl_entries', 'UPDATE')",
    )
    .fetch_one(pool)
    .await?;
    let epoch_triggers = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT class.relname::TEXT, \
                trigger_row.tgname::TEXT, \
                proc.proname::TEXT, \
                concat_ws(',', \
                    CASE WHEN trigger_row.tgtype & 4 <> 0 THEN 'INSERT' END, \
                    CASE WHEN trigger_row.tgtype & 8 <> 0 THEN 'DELETE' END, \
                    CASE WHEN trigger_row.tgtype & 16 <> 0 THEN 'UPDATE' END \
                ), \
                trigger_row.tgtype & 1 = 0, \
                trigger_row.tgoldtable::TEXT, \
                trigger_row.tgnewtable::TEXT \
           FROM pg_trigger AS trigger_row \
           JOIN pg_class AS class ON class.oid = trigger_row.tgrelid \
           JOIN pg_namespace AS namespace ON namespace.oid = class.relnamespace \
           JOIN pg_proc AS proc ON proc.oid = trigger_row.tgfoid \
          WHERE namespace.nspname = 'moa' \
            AND trigger_row.tgname IN ( \
                'source_acl_epoch_insert', \
                'source_acl_epoch_update', \
                'source_acl_epoch_delete') \
          ORDER BY class.relname, trigger_row.tgname",
    )
    .fetch_all(pool)
    .await?;
    let redundant_acl_columns_absent: bool = sqlx::query_scalar(
        "SELECT count(*) = 0 FROM information_schema.columns \
          WHERE table_schema = 'moa' AND ( \
                (table_name = 'knowledge_connections' AND column_name = 'acl_mode') OR \
                (table_name = 'knowledge_source_acl_snapshots' AND column_name = 'provenance'))",
    )
    .fetch_one(pool)
    .await?;
    let acl_state_not_null: bool = sqlx::query_scalar(
        "SELECT attnotnull FROM pg_attribute \
          WHERE attrelid = 'moa.knowledge_objects'::REGCLASS AND attname = 'acl_state'",
    )
    .fetch_one(pool)
    .await?;
    let current_acl_complete: bool = sqlx::query_scalar(
        "SELECT count(*) = 1 FROM pg_constraint \
          WHERE conname = 'knowledge_objects_current_acl_complete'",
    )
    .fetch_one(pool)
    .await?;
    let current_acl_fk_restrictive: bool = sqlx::query_scalar(
        "SELECT count(*) = 1 FROM pg_constraint \
          WHERE conname = 'knowledge_objects_current_acl_snapshot_tenant_partition_fkey' \
            AND confdeltype = 'a'",
    )
    .fetch_one(pool)
    .await?;
    let document_node_unique: bool = sqlx::query_scalar(
        "SELECT count(*) = 1 FROM pg_indexes \
          WHERE indexname = 'knowledge_document_versions_graph_node_uniq' \
            AND indexdef LIKE '%UNIQUE%'",
    )
    .fetch_one(pool)
    .await?;
    Ok((
        forced_rls,
        snapshot_policies,
        entry_policies,
        snapshot_update_granted,
        entry_update_granted,
        epoch_triggers,
        redundant_acl_columns_absent,
        acl_state_not_null,
        current_acl_complete,
        current_acl_fk_restrictive,
        document_node_unique,
    ))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_source_acl_final_schema_fails_closed_db() {
    // Pins: knowledge source ACL installs the source-ACL boundary on a pristine database and
    // re-applies as a no-op. The properties asserted here are the ones admission
    // cannot be trusted without — forced RLS on every new table, snapshots and
    // their entries immutable (no UPDATE policy AND no UPDATE grant, so a
    // permission set cannot be edited under an unchanged revision), epoch
    // triggers only on visibility-changing object/principal rows, and
    // database-owned totality of `acl_state`, with no redundant single-value
    // mode or provenance columns.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = source_acl_schema_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("source ACL migration should apply on a fresh database");
    let (
        forced_rls,
        snapshot_policies,
        entry_policies,
        snapshot_update_granted,
        entry_update_granted,
        epoch_triggers,
        redundant_acl_columns_absent,
        acl_state_not_null,
        current_acl_complete,
        current_acl_fk_restrictive,
        document_node_unique,
    ) = facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("knowledge_source_acl")),
        "a pristine database must apply knowledge source ACL, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert_eq!(
        forced_rls,
        vec![
            ("knowledge_source_acl_entries".to_string(), true),
            ("knowledge_source_acl_epochs".to_string(), true),
            ("knowledge_source_acl_keys".to_string(), true),
            ("knowledge_source_acl_snapshots".to_string(), true),
            ("knowledge_source_principal_bindings".to_string(), true),
            (
                "knowledge_source_principal_group_bindings".to_string(),
                true
            ),
        ],
        "every source-ACL table must exist with FORCE ROW LEVEL SECURITY"
    );
    assert_eq!(
        snapshot_policies,
        vec![
            "rd_tenant".to_string(),
            "rm_tenant".to_string(),
            "wr_tenant".to_string()
        ],
        "snapshots must expose read/insert/delete policies and no update policy"
    );
    assert_eq!(
        entry_policies,
        vec![
            "rd_tenant".to_string(),
            "rm_tenant".to_string(),
            "wr_tenant".to_string()
        ],
        "entries must expose read/insert/delete policies and no update policy"
    );
    assert!(
        !snapshot_update_granted,
        "the app role must not be able to edit a stored snapshot"
    );
    assert!(
        !entry_update_granted,
        "the app role must not be able to edit a stored ACL entry"
    );
    assert_eq!(
        epoch_triggers,
        vec![
            (
                "knowledge_objects".to_string(),
                "source_acl_epoch_update".to_string(),
                "source_acl_epoch_after_object_update".to_string(),
                "UPDATE".to_string(),
                true,
                Some("source_acl_old_rows".to_string()),
                Some("source_acl_new_rows".to_string()),
            ),
            (
                "knowledge_source_principal_bindings".to_string(),
                "source_acl_epoch_delete".to_string(),
                "source_acl_epoch_after_delete".to_string(),
                "DELETE".to_string(),
                true,
                Some("source_acl_old_rows".to_string()),
                None,
            ),
            (
                "knowledge_source_principal_bindings".to_string(),
                "source_acl_epoch_insert".to_string(),
                "source_acl_epoch_after_insert".to_string(),
                "INSERT".to_string(),
                true,
                None,
                Some("source_acl_new_rows".to_string()),
            ),
            (
                "knowledge_source_principal_bindings".to_string(),
                "source_acl_epoch_update".to_string(),
                "source_acl_epoch_after_update".to_string(),
                "UPDATE".to_string(),
                true,
                Some("source_acl_old_rows".to_string()),
                Some("source_acl_new_rows".to_string()),
            ),
            (
                "knowledge_source_principal_group_bindings".to_string(),
                "source_acl_epoch_delete".to_string(),
                "source_acl_epoch_after_delete".to_string(),
                "DELETE".to_string(),
                true,
                Some("source_acl_old_rows".to_string()),
                None,
            ),
            (
                "knowledge_source_principal_group_bindings".to_string(),
                "source_acl_epoch_insert".to_string(),
                "source_acl_epoch_after_insert".to_string(),
                "INSERT".to_string(),
                true,
                None,
                Some("source_acl_new_rows".to_string()),
            ),
            (
                "knowledge_source_principal_group_bindings".to_string(),
                "source_acl_epoch_update".to_string(),
                "source_acl_epoch_after_update".to_string(),
                "UPDATE".to_string(),
                true,
                Some("source_acl_old_rows".to_string()),
                Some("source_acl_new_rows".to_string()),
            ),
        ],
        "source-ACL invalidation must use operation-specific statement triggers with transition tables"
    );
    assert!(
        redundant_acl_columns_absent,
        "single-value ACL mode and capture provenance columns must be absent"
    );
    assert!(
        acl_state_not_null,
        "an object without an ACL state must be impossible"
    );
    assert!(
        current_acl_complete,
        "a `current` object must name its snapshot and revision"
    );
    assert!(
        current_acl_fk_restrictive,
        "a current snapshot cannot be deleted until the object pointer is cleared"
    );
    assert!(
        document_node_unique,
        "one document graph node must belong to exactly one document version"
    );
}

/// Facts hand-lease effective profile must install on `moa.hand_leases` and `moa.tenant_sandbox_policy`.
struct HandLeaseProfileFacts {
    has_idle_expires_at: bool,
    idle_is_nullable: bool,
    has_hard_expires_at: bool,
    dropped_legacy_expires_at: bool,
    policy_identity_columns: Vec<String>,
    reap_claim_columns: Vec<String>,
    dropped_legacy_reaper_index: bool,
    has_reaper_index: bool,
    accepts_reaping_status: bool,
    rejects_reaping_without_claim: bool,
    rejects_active_row_without_policy: bool,
    rejects_idle_past_hard: bool,
    has_tenant_sandbox_policy_table: bool,
    tenant_policy_rls_enabled: bool,
    tenant_policy_rls_forced: bool,
    tenant_policy_visible_rows_for_tenant: i64,
    tenant_policy_visible_rows_without_scope: i64,
}

async fn hand_lease_profile_facts(
    pool: &sqlx::PgPool,
) -> Result<HandLeaseProfileFacts, Box<dyn std::error::Error + Send + Sync>> {
    let column_exists = |name: &'static str| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'moa' AND table_name = 'hand_leases' AND column_name = $1)",
        )
        .bind(name)
        .fetch_one(pool)
        .await
    };

    let has_idle_expires_at = column_exists("idle_expires_at").await?;
    let has_hard_expires_at = column_exists("hard_expires_at").await?;
    let dropped_legacy_expires_at = !column_exists("expires_at").await?;
    let idle_is_nullable = sqlx::query_scalar::<_, String>(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
           AND column_name = 'idle_expires_at'",
    )
    .fetch_one(pool)
    .await?
        == "YES";

    let policy_identity_columns = sqlx::query_scalar::<_, String>(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
           AND column_name IN ('profile', 'profile_hash', 'source_deployment_revision', \
                               'source_tenant_revision', 'source_agent_revision', \
                               'source_route_revision', 'capability_revision') \
         ORDER BY column_name",
    )
    .fetch_all(pool)
    .await?;

    let reap_claim_columns = sqlx::query_scalar::<_, String>(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
           AND column_name IN ('reap_claim_token', 'reap_claim_expires_at') \
         ORDER BY column_name",
    )
    .fetch_all(pool)
    .await?;

    let index_exists = |name: &'static str| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
             WHERE schemaname = 'moa' AND tablename = 'hand_leases' AND indexname = $1)",
        )
        .bind(name)
        .fetch_one(pool)
        .await
    };
    let dropped_legacy_reaper_index = !index_exists("idx_hand_leases_status_expires").await?;
    let has_reaper_index = index_exists("idx_hand_leases_reaper").await?;

    let has_tenant_sandbox_policy_table = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'moa' AND table_name = 'tenant_sandbox_policy')",
    )
    .fetch_one(pool)
    .await?;

    let (tenant_policy_rls_enabled, tenant_policy_rls_forced) = sqlx::query_as::<_, (bool, bool)>(
        "SELECT class.relrowsecurity, class.relforcerowsecurity \
             FROM pg_class AS class \
             JOIN pg_namespace AS namespace ON namespace.oid = class.relnamespace \
             WHERE namespace.nspname = 'moa' AND class.relname = 'tenant_sandbox_policy'",
    )
    .fetch_one(pool)
    .await?;

    let tenant_a = uuid::Uuid::new_v4();
    let tenant_b = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO moa.tenant_sandbox_policy (tenant_id, revision, profile) \
         VALUES ($1, 'tenant-a-v1', '{}'::jsonb), ($2, 'tenant-b-v1', '{}'::jsonb)",
    )
    .bind(tenant_a)
    .bind(tenant_b)
    .execute(pool)
    .await?;

    let mut tenant_tx = pool.begin().await?;
    sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
        .bind(tenant_a.to_string())
        .execute(&mut *tenant_tx)
        .await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tenant_tx)
        .await?;
    let tenant_policy_visible_rows_for_tenant =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.tenant_sandbox_policy")
            .fetch_one(&mut *tenant_tx)
            .await?;
    tenant_tx.commit().await?;

    let mut unscoped_tx = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *unscoped_tx)
        .await?;
    let tenant_policy_visible_rows_without_scope =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.tenant_sandbox_policy")
            .fetch_one(&mut *unscoped_tx)
            .await?;
    unscoped_tx.commit().await?;

    // A `reaping` row must carry a complete expiring ownership claim. An active
    // row missing its policy identity and an idle deadline past the hard
    // deadline must also be rejected by the database itself.
    let accepts_reaping_status = insert_hand_lease(pool, "reaping", false, false, true)
        .await
        .is_ok();
    let rejects_reaping_without_claim = insert_hand_lease(pool, "reaping", false, false, false)
        .await
        .is_err();
    let rejects_active_row_without_policy = insert_hand_lease(pool, "active", false, false, false)
        .await
        .is_err();
    let rejects_idle_past_hard = insert_hand_lease(pool, "active", true, true, false)
        .await
        .is_err();

    Ok(HandLeaseProfileFacts {
        has_idle_expires_at,
        idle_is_nullable,
        has_hard_expires_at,
        dropped_legacy_expires_at,
        policy_identity_columns,
        reap_claim_columns,
        dropped_legacy_reaper_index,
        has_reaper_index,
        accepts_reaping_status,
        rejects_reaping_without_claim,
        rejects_active_row_without_policy,
        rejects_idle_past_hard,
        has_tenant_sandbox_policy_table,
        tenant_policy_rls_enabled,
        tenant_policy_rls_forced,
        tenant_policy_visible_rows_for_tenant,
        tenant_policy_visible_rows_without_scope,
    })
}

/// Inserts one hand lease row, optionally with full policy identity and
/// optionally with an idle deadline deliberately past the hard deadline or a
/// complete reaper ownership claim.
async fn insert_hand_lease(
    pool: &sqlx::PgPool,
    status: &str,
    with_policy: bool,
    idle_past_hard: bool,
    with_reap_claim: bool,
) -> Result<(), sqlx::Error> {
    let (idle, hard) = if idle_past_hard {
        ("now() + interval '2 hours'", "now() + interval '1 hour'")
    } else {
        ("now() + interval '1 hour'", "now() + interval '2 hours'")
    };
    let policy_columns = if with_policy || idle_past_hard {
        ", profile, profile_hash, source_deployment_revision, source_tenant_revision, \
         source_agent_revision, source_route_revision, capability_revision"
    } else {
        ""
    };
    let policy_values = if with_policy || idle_past_hard {
        ", '{}'::jsonb, 'sha256:test', 'd', 't', 'a', 'r', 'c'"
    } else {
        ""
    };
    let claim_columns = if with_reap_claim {
        ", reap_claim_token, reap_claim_expires_at"
    } else {
        ""
    };
    let claim_values = if with_reap_claim {
        ", gen_random_uuid(), now() + interval '5 minutes'"
    } else {
        ""
    };
    sqlx::query(&format!(
        "INSERT INTO moa.hand_leases \
         (session_id, worker_id, tenant_id, provider, tier, status, generation, \
          idle_expires_at, hard_expires_at{policy_columns}{claim_columns}) \
         VALUES (gen_random_uuid(), '', gen_random_uuid(), 'local', 'local', $1, 1, \
                 {idle}, {hard}{policy_values}{claim_values})"
    ))
    .bind(status)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_lease_effective_profile_final_schema_is_strict_db() {
    // Pins: hand-lease effective profile installs the sandbox policy contract on a pristine database
    // and re-applies as a no-op. The single renewable deadline becomes an idle
    // deadline plus an immutable hard one, the policy identity columns exist,
    // the reaper index replaces the old status/expiry index, and the database
    // itself refuses an active lease with no policy identity or an idle deadline
    // that outlives its hard deadline.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = hand_lease_profile_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("hand lease profile migration should apply on a fresh database");

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("hand_lease_effective_profile")),
        "a pristine database must apply hand-lease effective profile, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(facts.has_idle_expires_at, "idle_expires_at must exist");
    assert!(
        facts.idle_is_nullable,
        "an explicitly unbounded idle timeout maps to NULL, so the column must be nullable"
    );
    assert!(facts.has_hard_expires_at, "hard_expires_at must exist");
    assert!(
        facts.dropped_legacy_expires_at,
        "the single renewable expires_at must be gone, not shadowed"
    );
    assert_eq!(
        facts.policy_identity_columns,
        vec![
            "capability_revision".to_string(),
            "profile".to_string(),
            "profile_hash".to_string(),
            "source_agent_revision".to_string(),
            "source_deployment_revision".to_string(),
            "source_route_revision".to_string(),
            "source_tenant_revision".to_string(),
        ],
        "every policy identity column must be persisted on the lease"
    );
    assert_eq!(
        facts.reap_claim_columns,
        vec![
            "reap_claim_expires_at".to_string(),
            "reap_claim_token".to_string(),
        ],
        "a reaper claim must persist both its ownership token and expiry"
    );
    assert!(
        facts.dropped_legacy_reaper_index,
        "the old status/expiry index must be replaced, not left behind"
    );
    assert!(facts.has_reaper_index, "the reaper claim index must exist");
    assert!(
        facts.accepts_reaping_status,
        "the status check must admit `reaping` with a complete ownership claim"
    );
    assert!(
        facts.rejects_reaping_without_claim,
        "a `reaping` row without an ownership token and expiry must be rejected"
    );
    assert!(
        facts.rejects_active_row_without_policy,
        "an active lease with no policy identity must be rejected by the database"
    );
    assert!(
        facts.rejects_idle_past_hard,
        "an idle deadline past the hard deadline must be rejected by the database"
    );
    assert!(
        facts.has_tenant_sandbox_policy_table,
        "the tenant policy layer must have a durable owner"
    );
    assert!(
        facts.tenant_policy_rls_enabled,
        "tenant sandbox policy must enable row-level security"
    );
    assert!(
        facts.tenant_policy_rls_forced,
        "tenant sandbox policy must force row-level security"
    );
    assert_eq!(
        facts.tenant_policy_visible_rows_for_tenant, 1,
        "moa_app must see only the sandbox policy for its scoped tenant"
    );
    assert_eq!(
        facts.tenant_policy_visible_rows_without_scope, 0,
        "moa_app without a tenant scope must fail closed"
    );
}

/// Typed Behavior Lab score provenance.
const EXPERIMENT_SCORE_PROVENANCE_SQL: &str =
    include_str!("../migrations/postgres/V000041__experiment_score_provenance.sql");

#[test]
fn experiment_score_provenance_ownership_is_registered_offline() {
    // Pins: a tenant-scoped table with no ownership row is a table nothing is
    // accountable for, and the tenant-purge catalog scan would only notice it at
    // runtime against a live database.
    assert!(
        MIGRATION_OWNERSHIP.contains("name = \"experiment_score_provenance\""),
        "experiment-score provenance's table must be registered in migration-ownership.toml"
    );
    // The trial foreign key must not cascade: the tenant purge carries an
    // explicit delete for this table, and a cascade would make that step
    // unfalsifiable because the trial delete would remove the same rows anyway.
    assert!(
        !EXPERIMENT_SCORE_PROVENANCE_SQL.contains("ON DELETE CASCADE"),
        "no foreign key here may cascade over the explicit tenant-purge step"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn experiment_score_provenance_enforces_linkage_and_immutability_db() {
    // Pins the experiment-score provenance guarantees the database owns rather than the writer:
    // provenance cannot name a trial from another tenant, run, or pinned plan
    // revision; it cannot claim both targets or neither; and it cannot be
    // rewritten after the fact. An application that "checked first" would pass a
    // unit test and still admit a mislinked or mutated row on a concurrent path.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        let (_, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let forced: Option<bool> = sqlx::query_scalar(
            "SELECT relrowsecurity AND relforcerowsecurity
               FROM pg_catalog.pg_class AS relation
               JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace
              WHERE namespace.nspname = 'moa'
                AND relation.relname = 'experiment_score_provenance'",
        )
        .fetch_optional(&target)
        .await?;

        seed_provenance_fixture(&target).await?;

        let correct = insert_provenance(&target, ProvenanceCell::default()).await;
        let replay = insert_provenance(&target, ProvenanceCell::default()).await;
        let wrong_run = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 20,
                experiment_run_uid: "22222222-2222-2222-2222-222222222222",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let wrong_plan = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 21,
                plan_revision_uid: "33333333-3333-3333-3333-333333333333",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let wrong_tenant = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 22,
                storage_partition_id: "99999999-9999-9999-9999-999999999999",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let both_targets = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 23,
                target_execution_run_uid: Some("44444444-4444-4444-4444-444444444444"),
                ..ProvenanceCell::default()
            },
        )
        .await;
        let no_target = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 24,
                target_session_id: None,
                ..ProvenanceCell::default()
            },
        )
        .await;
        let short_hash = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 25,
                evidence_hash: "\\x00",
                ..ProvenanceCell::default()
            },
        )
        .await;

        let updated = sqlx::query(
            "UPDATE moa.experiment_score_provenance
                SET evidence_ref = 'rewritten'
              WHERE score_id = '00000000-0000-0000-0000-000000000010'",
        )
        .execute(&target)
        .await;

        let stored_ref: String = sqlx::query_scalar(
            "SELECT evidence_ref FROM moa.experiment_score_provenance
              WHERE score_id = '00000000-0000-0000-0000-000000000010'",
        )
        .fetch_one(&target)
        .await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(ProvenanceOutcome {
            second_apply_count: second.len(),
            forced,
            correct_accepted: correct.is_ok(),
            replay_refused: replay.is_err(),
            wrong_run_refused: wrong_run.is_err(),
            wrong_plan_refused: wrong_plan.is_err(),
            wrong_tenant_refused: wrong_tenant.is_err(),
            both_targets_refused: both_targets.is_err(),
            no_target_refused: no_target.is_err(),
            short_hash_refused: short_hash.is_err(),
            update_refused: updated.is_err(),
            stored_ref,
        })
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let outcome = outcome.expect("provenance assertions should complete on a fresh database");

    assert_eq!(
        outcome.second_apply_count, 0,
        "experiment-score provenance must be idempotent: a second run applied {} migrations",
        outcome.second_apply_count
    );
    assert_eq!(
        outcome.forced,
        Some(true),
        "experiment score provenance must force row-level security"
    );
    assert!(
        outcome.correct_accepted,
        "a correctly linked provenance row must be accepted"
    );
    assert!(
        outcome.replay_refused,
        "a second row for the same score id must be refused by the primary key"
    );
    assert!(
        outcome.wrong_run_refused,
        "provenance naming another experiment run must be refused"
    );
    assert!(
        outcome.wrong_plan_refused,
        "provenance naming another pinned plan revision must be refused"
    );
    assert!(
        outcome.wrong_tenant_refused,
        "provenance naming another tenant's partition must be refused"
    );
    assert!(
        outcome.both_targets_refused,
        "provenance claiming both a session and an execution run must be refused"
    );
    assert!(
        outcome.no_target_refused,
        "provenance claiming no target at all must be refused"
    );
    assert!(
        outcome.short_hash_refused,
        "an evidence hash that is not a 32-byte digest must be refused"
    );
    assert!(
        outcome.update_refused,
        "provenance must be immutable: the UPDATE trigger must refuse every rewrite"
    );
    assert_eq!(
        outcome.stored_ref, "session:00000000-0000-0000-0000-000000000005#seq=1",
        "the refused UPDATE must have left the stored evidence reference untouched"
    );
}

struct ProvenanceOutcome {
    second_apply_count: usize,
    forced: Option<bool>,
    correct_accepted: bool,
    replay_refused: bool,
    wrong_run_refused: bool,
    wrong_plan_refused: bool,
    wrong_tenant_refused: bool,
    both_targets_refused: bool,
    no_target_refused: bool,
    short_hash_refused: bool,
    update_refused: bool,
    stored_ref: String,
}

struct ProvenanceCell {
    score_id: u8,
    storage_partition_id: &'static str,
    experiment_run_uid: &'static str,
    plan_revision_uid: &'static str,
    target_session_id: Option<&'static str>,
    target_execution_run_uid: Option<&'static str>,
    evidence_hash: &'static str,
}

impl Default for ProvenanceCell {
    fn default() -> Self {
        Self {
            score_id: 16,
            storage_partition_id: "11111111-1111-1111-1111-111111111111",
            experiment_run_uid: "00000000-0000-0000-0000-000000000003",
            plan_revision_uid: "00000000-0000-0000-0000-000000000004",
            target_session_id: Some("00000000-0000-0000-0000-000000000005"),
            target_execution_run_uid: None,
            evidence_hash: "\\x0000000000000000000000000000000000000000000000000000000000000001",
        }
    }
}

async fn insert_provenance(
    pool: &PgPool,
    cell: ProvenanceCell,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let score_id = format!("00000000-0000-0000-0000-0000000000{:02x}", cell.score_id);
    let score_ts = "2026-01-01T00:00:00Z";
    sqlx::query(
        "INSERT INTO analytics.scores (
             score_id, ts, storage_partition_id, target_kind, session_id, run_id, name,
             value_type, value_boolean, source, model_or_evaluator
         ) VALUES (
             $1::UUID, $2::TIMESTAMPTZ, '11111111-1111-1111-1111-111111111111',
             'session', '00000000-0000-0000-0000-000000000005'::UUID,
             '00000000-0000-0000-0000-000000000006'::UUID,
             'target_completed', 'boolean', TRUE, 'product_evaluator', 'target_completed@v1'
         ) ON CONFLICT (score_id, ts) DO NOTHING",
    )
    .bind(&score_id)
    .bind(score_ts)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.experiment_score_provenance (
             score_id, score_ts, storage_partition_id, user_id, score_run_id, experiment_run_uid,
             plan_revision_uid, trial_uid, target_session_id, target_execution_run_uid,
             evaluator_id, evaluator_version, score_name, value_type, evidence_ref, evidence_hash
         ) VALUES (
             $1::UUID, $2::TIMESTAMPTZ, $3, NULL, '00000000-0000-0000-0000-000000000006'::UUID, $4::UUID,
             $5::UUID, '00000000-0000-0000-0000-000000000002'::UUID, $6::UUID, $7::UUID,
             'target_completed', 'v1', 'target_completed', 'boolean',
             'session:00000000-0000-0000-0000-000000000005#seq=1', $8::BYTEA
         )",
    )
    .bind(&score_id)
    .bind(score_ts)
    .bind(cell.storage_partition_id)
    .bind(cell.experiment_run_uid)
    .bind(cell.plan_revision_uid)
    .bind(cell.target_session_id)
    .bind(cell.target_execution_run_uid)
    .bind(cell.evidence_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seeds the exact score-run, experiment-run, and trial rows the linkage
/// constraints reference, for one tenant and one neighbour.
async fn seed_provenance_fixture(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for statement in [
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
         VALUES ('00000000-0000-0000-0000-000000000006', '11111111-1111-1111-1111-111111111111', NULL, 'experiment_trial')",
        "INSERT INTO moa.experiment_run (
             run_uid, storage_partition_id, user_id, name, target_kind, status, target, variant,
             scorecard, score_run_id, artifact_revision_uids, created_by_identity,
             resource_envelope
         ) VALUES (
             '00000000-0000-0000-0000-000000000003', '11111111-1111-1111-1111-111111111111',
             NULL, 'fixture run', 'agent_loop', 'running', '{}'::jsonb, '{}'::jsonb, '{}'::jsonb,
             '00000000-0000-0000-0000-000000000006', '{}', '{}'::jsonb,
             '{\"version\": 1,
                     \"run_limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"trial_limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"deadline_at\": \"1970-01-01T00:00:00Z\"}'::jsonb
         )",
        "INSERT INTO moa.experiment_trial (
             trial_uid, run_uid, storage_partition_id, user_id, trial_key, status, target_kind,
             variant_key, plan_revision_uid, simulator, simulator_model, score_run_id,
             resource_envelope
         ) VALUES (
             '00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000003',
             '11111111-1111-1111-1111-111111111111', NULL, 'fixture/0', 'running',
             'agent_loop', 'baseline', '00000000-0000-0000-0000-000000000004', '{}'::jsonb,
             'sim-model', '00000000-0000-0000-0000-000000000006',
             '{\"version\": 1,
                     \"limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"deadline\": \"1970-01-01T00:00:00Z\"}'::jsonb
         )",
    ] {
        pool.execute(statement).await?;
    }
    Ok(())
}

/// Seeds the minimum row set a learning candidate's foreign keys require.
///
/// A candidate now stands on real referents, so a constraint test cannot use
/// fabricated uuids: the insert would fail for the wrong reason and the test
/// would pass while proving nothing about the state machine. A contact is the
/// cheapest valid referent — a session would additionally drag in the
/// agent-context commit trigger, which has nothing to do with what this test
/// pins.
async fn seed_learning_candidate_fixture(
    pool: &PgPool,
    partition: &str,
    tenant: &str,
) -> Result<uuid::Uuid, Box<dyn std::error::Error + Send + Sync>> {
    let contact_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO contacts (id, contact_id, tenant_id, storage_partition_id, state) \
         VALUES ($1, $1, $2::UUID, $3, 'verified')",
    )
    .bind(contact_id)
    .bind(tenant)
    .bind(partition)
    .execute(pool)
    .await?;
    Ok(contact_id)
}

/// Inserts one candidate of `kind` at `status`, returning whether the write was accepted.
///
/// Candidate and source commit in ONE transaction, and that is not a convenience:
/// learning privacy provenance installs a DEFERRED constraint trigger that refuses to let a
/// transaction commit a candidate with no normalized source. Writing them as two
/// autocommitted statements fails at the first commit — which is the trigger
/// doing its job, and is why the production store writes them together too.
async fn try_insert_candidate(
    pool: &PgPool,
    partition: &str,
    tenant: &str,
    contact_id: uuid::Uuid,
    kind: &str,
    status: &str,
) -> Result<(bool, uuid::Uuid), Box<dyn std::error::Error + Send + Sync>> {
    let candidate_id = uuid::Uuid::now_v7();
    let mut tx = pool.begin().await?;
    let candidate_written = sqlx::query(
        "INSERT INTO learning_candidates \
         (id, tenant_id, storage_partition_id, candidate_type, proposal_kind, status, payload, risk_class) \
         VALUES ($1, $2, $3, 'skill', $4, $5, '{}'::JSONB, 'low')",
    )
    .bind(candidate_id)
    .bind(tenant)
    .bind(partition)
    .bind(kind)
    .bind(status)
    .execute(tx.as_mut())
    .await
    .is_ok();
    if !candidate_written {
        tx.rollback().await?;
        return Ok((false, candidate_id));
    }
    sqlx::query(
        "INSERT INTO learning_candidate_source \
         (id, candidate_id, tenant_id, storage_partition_id, source_kind, contact_id) \
         VALUES ($1, $2, $3, $4, 'contact', $5)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(candidate_id)
    .bind(tenant)
    .bind(partition)
    .bind(contact_id)
    .execute(tx.as_mut())
    .await?;
    let committed = tx.commit().await.is_ok();
    Ok((committed, candidate_id))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn learning_privacy_provenance_rejects_forbidden_transitions_db() {
    // Pins the two database-level guarantees the review contract rests on, on a
    // fresh database carrying the whole migration set:
    //
    //  1. An informational proposal kind cannot hold a reviewable status. Before
    //     learning privacy provenance, memory/policy/prompt/eval suggestions were written as
    //     `Proposed` and sat on the review queue beside skill drafts that could
    //     actually be accepted, so a reviewer could press accept on something no
    //     code could apply.
    //  2. An advisory item cannot be walked to `Promoted` one legal-looking step
    //     at a time. A CHECK constraint sees one row version; only the transition
    //     trigger sees the pair, and the pair is where that escape lives.
    //
    // Repository-level compare-and-set is defense in depth on top of this, not a
    // substitute: it does not constrain a direct SQL writer.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let tenant = uuid::Uuid::now_v7().to_string();
        let partition = tenant.clone();
        let contact_id = seed_learning_candidate_fixture(&pool, &partition, &tenant).await?;

        // Every reviewable status is refused for an advisory kind, and every
        // informational status is refused for a draft.
        let mut forbidden_accepted = Vec::new();
        for (kind, status) in [
            ("memory_advisory", "proposed"),
            ("memory_advisory", "evaluating"),
            ("memory_advisory", "promoted"),
            ("memory_advisory", "rejected"),
            ("memory_advisory", "rolled_back"),
            ("skill_authoring", "proposed"),
            ("skill_authoring", "promoted"),
            ("policy_authoring", "evaluating"),
            ("prompt_authoring", "rejected"),
            ("eval_authoring", "rolled_back"),
            ("skill_draft", "advisory"),
            ("skill_draft", "needs_authoring"),
            ("skill_draft", "dismissed"),
            ("skill_rollback", "rolled_back"),
            ("skill_rollback", "advisory"),
        ] {
            let (accepted, _) =
                try_insert_candidate(&pool, &partition, &tenant, contact_id, kind, status).await?;
            if accepted {
                forbidden_accepted.push(format!("{kind}/{status}"));
            }
        }

        // Every legal pair is still accepted, so the constraint is not simply
        // refusing everything.
        let mut legal_rejected = Vec::new();
        for (kind, status) in [
            ("skill_draft", "proposed"),
            ("skill_rollback", "proposed"),
            ("memory_advisory", "advisory"),
            ("memory_advisory", "dismissed"),
            ("skill_authoring", "needs_authoring"),
            ("eval_authoring", "dismissed"),
        ] {
            let (accepted, _) =
                try_insert_candidate(&pool, &partition, &tenant, contact_id, kind, status).await?;
            if !accepted {
                legal_rejected.push(format!("{kind}/{status}"));
            }
        }

        // An advisory item may only be dismissed, and its kind may not be
        // rewritten into a reviewable one to escape that.
        let (_, advisory_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "memory_advisory",
            "advisory",
        )
        .await?;
        let promoted_directly =
            sqlx::query("UPDATE learning_candidates SET status = 'promoted' WHERE id = $1")
                .bind(advisory_id)
                .execute(&pool)
                .await
                .is_ok();
        let kind_rewritten = sqlx::query(
            "UPDATE learning_candidates SET proposal_kind = 'skill_draft', status = 'proposed' \
             WHERE id = $1",
        )
        .bind(advisory_id)
        .execute(&pool)
        .await
        .is_ok();
        let dismissed =
            sqlx::query("UPDATE learning_candidates SET status = 'dismissed' WHERE id = $1")
                .bind(advisory_id)
                .execute(&pool)
                .await
                .is_ok();

        // A candidate with no normalized source must not be committable at all.
        // Without this, a producer could file learning that no erasure could ever
        // reach and no export could ever explain — the original defect, reachable
        // again through a single forgotten insert.
        let sourceless_committed = {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_candidates \
                 (id, tenant_id, storage_partition_id, candidate_type, proposal_kind, status, \
                  payload, risk_class) \
                 VALUES ($1, $2, $3, 'skill', 'skill_draft', 'proposed', '{}'::JSONB, 'low')",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        // A skill draft may not skip the claim: `Proposed -> Promoted` directly
        // would let two reviewers both succeed at contradictory decisions.
        let (_, draft_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "skill_draft",
            "proposed",
        )
        .await?;
        let skipped_claim =
            sqlx::query("UPDATE learning_candidates SET status = 'promoted' WHERE id = $1")
                .bind(draft_id)
                .execute(&pool)
                .await
                .is_ok();

        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            forbidden_accepted,
            legal_rejected,
            promoted_directly,
            kind_rewritten,
            dismissed,
            skipped_claim,
            sourceless_committed,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        forbidden_accepted,
        legal_rejected,
        promoted_directly,
        kind_rewritten,
        dismissed,
        skipped_claim,
        sourceless_committed,
    ) = outcome.expect("proposal-kind constraint probe should complete");

    assert!(
        forbidden_accepted.is_empty(),
        "these (proposal_kind, status) pairs must be rejected but were accepted: {forbidden_accepted:?}"
    );
    assert!(
        legal_rejected.is_empty(),
        "these legal (proposal_kind, status) pairs were rejected: {legal_rejected:?}"
    );
    assert!(
        !promoted_directly,
        "an advisory item must never reach `promoted`; no materializer exists for it"
    );
    assert!(
        !kind_rewritten,
        "rewriting proposal_kind must be refused, or an advisory item could be laundered into a reviewable draft"
    );
    assert!(
        dismissed,
        "dismissal is the one transition an advisory item admits and it must still work"
    );
    assert!(
        !skipped_claim,
        "a skill draft must pass through `evaluating`; skipping the claim would let two reviewers both succeed"
    );
    assert!(
        !sourceless_committed,
        "a candidate with no normalized source must not be committable: it could never be erased or explained"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn learning_log_final_schema_requires_normalized_source_db() {
    // Pins: every committed learning-log row has normalized provenance, while a
    // row and its source may still be committed atomically in one transaction.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for learning-log provenance");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create learning-log provenance throwaway database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let tenant = uuid::Uuid::now_v7().to_string();
        let partition = tenant.clone();
        let contact_id = seed_learning_candidate_fixture(&pool, &partition, &tenant).await?;
        let (_, candidate_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "skill_draft",
            "proposed",
        )
        .await?;

        let sourceless_committed = {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_log \
                 (id, tenant_id, storage_partition_id, learning_type, target_id, payload, actor, \
                  valid_from, version) \
                 VALUES ($1, $2, $3, 'skill_created', 'target', '{}'::JSONB, 'test', now(), 1)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        let attributed_committed = {
            let learning_id = uuid::Uuid::now_v7();
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_log \
                 (id, tenant_id, storage_partition_id, learning_type, target_id, payload, actor, \
                  valid_from, version) \
                 VALUES ($1, $2, $3, 'skill_created', 'target', '{}'::JSONB, 'test', now(), 1)",
            )
            .bind(learning_id)
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            sqlx::query(
                "INSERT INTO learning_log_source \
                 (id, learning_id, tenant_id, storage_partition_id, source_kind, candidate_id) \
                 VALUES ($1, $2, $3, $4, 'candidate', $5)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(learning_id)
            .bind(&tenant)
            .bind(&partition)
            .bind(candidate_id)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            sourceless_committed,
            attributed_committed,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (sourceless_committed, attributed_committed) =
        outcome.expect("learning-log completeness probe should complete");
    assert!(
        !sourceless_committed,
        "a learning-log entry with no normalized source must not commit"
    );
    assert!(
        attributed_committed,
        "a learning-log row and normalized source must commit atomically"
    );
}

/// Durable lineage acceptance queue.
const LINEAGE_JOURNAL_SQL: &str =
    include_str!("../migrations/postgres/V000042__lineage_journal.sql");

#[test]
fn lineage_journal_ownership_is_registered_offline() {
    // Pins: the queue is tenant-scoped, so it needs an ownership row. Without one
    // the tenant-purge catalog scan only discovers it at runtime against a live
    // database, which is where the last six unregistered tables were found.
    assert!(
        MIGRATION_OWNERSHIP.contains("name = \"lineage_journal\""),
        "lineage journal's table must be registered in migration-ownership.toml"
    );
    // Row-level security admits the control plane only. A tenant-scoped request
    // connection has no legitimate reason to read pending lineage payloads, and
    // the queue is deliberately cross-tenant so one drain can batch across
    // partitions.
    assert!(
        LINEAGE_JOURNAL_SQL.contains("FORCE ROW LEVEL SECURITY")
            && LINEAGE_JOURNAL_SQL.contains("moa.current_control_plane()"),
        "the queue must be FORCE-RLS behind the control-plane predicate"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn lineage_journal_final_schema_is_durable_and_idempotent_db() {
    // Pins: lineage journal installs the durable acceptance queue on a pristine database
    // and re-applies as a no-op, and the database itself enforces the two
    // properties the writer's correctness rests on: claim eligibility is derived
    // from the lease pair (so it cannot drift into permanently unclaimable), and
    // a half-leased row cannot exist.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = lineage_journal_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("lineage journal migration should apply on a fresh database");

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("lineage_journal")),
        "a pristine database must apply lineage journal, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(
        facts.claim_index_exists,
        "the drain reads through lineage_journal_claim_idx on every poll; without it every claim \
         is a sequential scan of the whole backlog"
    );
    assert!(
        facts.forces_row_level_security,
        "the queue holds cross-tenant lineage payloads; RLS must be FORCED, not merely enabled"
    );
    assert_eq!(
        facts.policy_names,
        vec!["lineage_journal_runtime_only".to_string()],
        "exactly one policy may exist, and it is the control-plane-only one"
    );
    assert_eq!(
        facts.unleased_claimable_at, facts.unleased_available_at,
        "an unleased row must be claimable at available_at"
    );
    assert_eq!(
        facts.leased_claimable_at, facts.leased_lease_expires_at,
        "stamping a lease in the future must push claimable_at to the lease expiry, with no \
         separate column for a claimant to forget to update"
    );
    assert_eq!(
        facts.half_lease_sqlstate.as_deref(),
        Some("23514"),
        "a half-leased row must fail with a check-constraint violation"
    );
    assert_eq!(
        facts.half_lease_constraint.as_deref(),
        Some("lineage_journal_lease_pair_check"),
        "the lease-pair constraint, not an unrelated tenant fence, must reject the row"
    );
}

/// Observable facts about the installed lineage acceptance queue.
struct LineageJournalFacts {
    claim_index_exists: bool,
    forces_row_level_security: bool,
    policy_names: Vec<String>,
    half_lease_sqlstate: Option<String>,
    half_lease_constraint: Option<String>,
    unleased_claimable_at: chrono::DateTime<chrono::Utc>,
    unleased_available_at: chrono::DateTime<chrono::Utc>,
    leased_claimable_at: chrono::DateTime<chrono::Utc>,
    leased_lease_expires_at: chrono::DateTime<chrono::Utc>,
}

async fn lineage_journal_facts(
    pool: &PgPool,
) -> Result<LineageJournalFacts, Box<dyn std::error::Error + Send + Sync>> {
    let claim_index_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'analytics' \
         AND tablename = 'lineage_journal' AND indexname = 'lineage_journal_claim_idx')",
    )
    .fetch_one(pool)
    .await?;
    let forces_row_level_security: bool = sqlx::query_scalar(
        "SELECT relrowsecurity AND relforcerowsecurity FROM pg_class \
         WHERE oid = 'analytics.lineage_journal'::regclass",
    )
    .fetch_one(pool)
    .await?;
    let policy_names: Vec<String> = sqlx::query_scalar(
        "SELECT policyname FROM pg_policies WHERE schemaname = 'analytics' \
         AND tablename = 'lineage_journal' ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;

    let facts_partition = uuid::Uuid::now_v7().to_string();
    let unleased_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO analytics.lineage_journal \
         (journal_id, storage_partition_id, event_class, payload, available_at) \
         VALUES ($1, $2, 'lineage', '{}'::jsonb, now() + interval '30 seconds')",
    )
    .bind(unleased_id)
    .bind(&facts_partition)
    .execute(pool)
    .await?;
    let (unleased_claimable_at, unleased_available_at) = sqlx::query_as(
        "SELECT claimable_at, available_at FROM analytics.lineage_journal WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        "UPDATE analytics.lineage_journal SET lease_owner = gen_random_uuid(), \
         lease_expires_at = now() + interval '10 minutes' WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .execute(pool)
    .await?;
    let (leased_claimable_at, leased_lease_expires_at) = sqlx::query_as(
        "SELECT claimable_at, lease_expires_at FROM analytics.lineage_journal \
         WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .fetch_one(pool)
    .await?;

    let half_lease_error = sqlx::query(
        "INSERT INTO analytics.lineage_journal \
         (journal_id, storage_partition_id, event_class, payload, lease_owner) \
         VALUES (gen_random_uuid(), $1, 'lineage', '{}'::jsonb, gen_random_uuid())",
    )
    .bind(&facts_partition)
    .execute(pool)
    .await
    .expect_err("a half-leased lineage row must violate the lease-pair constraint");
    let half_lease_sqlstate = half_lease_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    let half_lease_constraint = half_lease_error
        .as_database_error()
        .and_then(|error| error.constraint().map(ToOwned::to_owned));

    Ok(LineageJournalFacts {
        claim_index_exists,
        forces_row_level_security,
        policy_names,
        half_lease_sqlstate,
        half_lease_constraint,
        unleased_claimable_at,
        unleased_available_at,
        leased_claimable_at,
        leased_lease_expires_at,
    })
}

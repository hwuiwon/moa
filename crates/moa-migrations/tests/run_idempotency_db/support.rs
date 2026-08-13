//! Shared fixtures and catalog helpers for the migration database test lane.

use anyhow::Context;

pub(super) use sqlx::postgres::PgPoolOptions;
pub(super) use sqlx::{Executor, PgPool};

mod embedded_for_cutover_proof {
    use refinery::embed_migrations;

    embed_migrations!("migrations/postgres");
}

/// Default Docker Compose Postgres URL used by local MOA tests.
pub(super) const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Serializes cluster-global role DDL across throwaway databases and test processes.
pub(super) const CLUSTER_CATALOG_TEST_LOCK_ID: i64 = 0x4d4f_415f_5445_5354;

pub(super) type AuthzOutboxFact = (String, String, String, String, i32, i64, String, uuid::Uuid);
pub(super) type TableContractFact = (String, String, bool, bool, bool, bool, bool, bool);

pub(super) const RETIRED_INDEXES: [&str; 7] = [
    "public.idx_events_tenant_session",
    "analytics.score_run_partition_identity_idx",
    "moa.knowledge_source_acl_entries_lookup_idx",
    "public.idx_task_segments_session",
    "public.idx_experience_attributions_experience",
    "public.idx_users_tenant_email_unique",
    "moa.idx_hand_leases_session_worker",
];

pub(super) const RETAINED_INDEXES: [&str; 7] = [
    "public.events_session_id_sequence_num_key",
    "analytics.score_run_id_partition_key",
    "moa.knowledge_source_acl_entries_uniq",
    "public.task_segments_session_id_segment_index_key",
    "public.experience_attributions_experience_id_subject_type_subject__key",
    "public.idx_users_tenant_email_lower_unique",
    "moa.hand_leases_pkey",
];

pub(super) const PRIVACY_AUDITOR_TABLES: [&str; 14] = [
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

pub(super) const FINAL_AUDITOR_GRANT_TABLES: [&str; 24] = [
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

pub(super) const FINAL_AUDITOR_POLICY_TABLES: [&str; 20] = [
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

pub(super) const TENANT_PURGE_SCOPE_INDEXES: [(&str, &str, &str, &str, Option<&str>); 19] = [
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
pub(super) const MIGRATION_OWNERSHIP: &str = include_str!("../../migration-ownership.toml");

/// V3 auth baseline source for retired-table absence checks.
pub(super) const AUTH_BASELINE_MIGRATION: &str =
    include_str!("../../migrations/postgres/V000003__auth_baseline.sql");

/// V29 no-op marker preserving the hard-reset epoch's contiguous numbering.
pub(super) const RETIRED_TOKEN_VAULT_EPOCH_MARKER: &str =
    include_str!("../../migrations/postgres/V000029__retired_token_vault_epoch_marker.sql");

/// V52 parent/projection migration source for final-schema contraction checks.
pub(super) const KNOWLEDGE_CONNECTION_PARENT_MIGRATION: &str =
    include_str!("../../migrations/postgres/V000052__knowledge_connection_parent_constraint.sql");

/// V53 typed connector-origin migration source for hard-break checks.
pub(super) const TYPED_CONNECTOR_ORIGIN_MIGRATION: &str =
    include_str!("../../migrations/postgres/V000053__typed_connector_origin.sql");

pub(super) fn removed_serialized_value(parts: &[&str]) -> String {
    parts.concat()
}

/// Returns the Postgres URL used by integration tests, mirroring the runtime
/// `MOA_DATABASE_URL` setting and falling back to the compose default.
pub(super) fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Returns a process-and-UUID-unique throwaway database name.
pub(super) fn unique_db_name() -> String {
    format!(
        "moa_mig_idem_{}_{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Rewrites the database name in a Postgres URL, preserving any query string.
pub(super) fn with_database(url: &str, database: &str) -> String {
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
pub(super) async fn clean_apply_then_reapply(
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
pub(super) async fn run_reporting_applied_serialized(
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
    match result {
        Ok(applied) => Ok(applied),
        Err(error) => Err(std::io::Error::other(format!("{error:#}")).into()),
    }
}

/// Returns the exact embedded migration labels in version order.
pub(super) fn expected_migration_labels() -> Vec<String> {
    let mut migrations = embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .iter()
        .map(|migration| (migration.version(), migration.to_string()))
        .collect::<Vec<_>>();
    migrations.sort_by_key(|(version, _)| *version);
    migrations.into_iter().map(|(_, label)| label).collect()
}

/// Returns the current maximum embedded migration version.
pub(super) fn current_migration_version() -> i32 {
    embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .iter()
        .map(refinery::Migration::version)
        .max()
        .expect("the central migration epoch must not be empty")
}

/// Returns the embedded migration labels from one semantic migration onward.
///
/// A scenario that applies through the preceding migration and then runs the
/// public runner to completion applies exactly this contiguous tail. Pinning the
/// tail rather than a hand-written single label keeps the assertion exact while
/// staying correct when a later migration is appended to the epoch.
pub(super) fn expected_migration_labels_from(migration_name: &str) -> Vec<String> {
    let version = migration_version(migration_name)
        .unwrap_or_else(|error| panic!("migration `{migration_name}` must be embedded: {error}"));
    let mut migrations = embedded_for_cutover_proof::migrations::runner()
        .get_migrations()
        .iter()
        .filter(|migration| migration.version() >= version)
        .map(|migration| (migration.version(), migration.to_string()))
        .collect::<Vec<_>>();
    migrations.sort_by_key(|(version, _)| *version);
    migrations.into_iter().map(|(_, label)| label).collect()
}

/// Resolves an embedded migration by semantic name.
pub(super) fn migration_version(migration_name: &str) -> Result<i32, std::io::Error> {
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
pub(super) async fn apply_through_migration(
    target_url: &str,
    migration_name: &str,
) -> anyhow::Result<Vec<String>> {
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
        .await
        .with_context(|| format!("apply migrations through {migration_name}"));
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
pub(super) async fn install_required_extensions(
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
pub(super) async fn install_ddl_sentinel(
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
pub(super) async fn reset_rejection_and_ddl_count(
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
pub(super) fn assert_destructive_reset_rejection(
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
pub(super) async fn drop_database_with_zero_connections(admin: &PgPool, database: &str) {
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

/// A newly created throwaway database whose explicit async cleanup preserves
/// the lane's zero-connection assertion before dropping the database.
pub(super) struct FreshMigrationDatabase {
    admin: PgPool,
    name: String,
    target_url: String,
}

impl FreshMigrationDatabase {
    /// Creates one isolated database on the configured maintenance cluster.
    pub(super) async fn create() -> Result<Self, sqlx::Error> {
        let admin_url = test_database_url();
        let name = unique_db_name();
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await?;
        admin
            .execute(format!("CREATE DATABASE \"{name}\"").as_str())
            .await?;
        let target_url = with_database(&admin_url, &name);
        Ok(Self {
            admin,
            name,
            target_url,
        })
    }

    /// Returns the connection URL for the isolated database.
    pub(super) fn target_url(&self) -> &str {
        &self.target_url
    }

    /// Cleans up the database after an async scenario and returns its outcome.
    pub(super) async fn finish<T>(self, outcome: T) -> T {
        drop_database_with_zero_connections(&self.admin, &self.name).await;
        self.admin.close().await;
        outcome
    }
}

/// One deliberately invalid execution-route audit matrix cell.
#[derive(Clone, Copy)]
pub(super) struct InvalidRouteAuditCell<'a> {
    pub(super) sequence: i64,
    pub(super) stage: &'a str,
    pub(super) decision: &'a str,
    pub(super) strategy: Option<&'a str>,
    pub(super) source: &'a str,
    pub(super) classifier_outcome: &'a str,
    pub(super) classifier_evidence: bool,
}

/// Proves that PostgreSQL rejects one invalid execution-route audit row at the
/// table boundary with a check-constraint violation.
pub(super) async fn assert_route_audit_insert_rejected(
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

#[derive(Clone, Debug, Eq, PartialEq, sqlx::FromRow)]
pub(super) struct FinalIndexCatalogRow {
    pub(super) qualified_name: String,
    pub(super) table_schema: String,
    pub(super) table_name: String,
    pub(super) is_unique: bool,
    pub(super) is_primary: bool,
    pub(super) is_valid: bool,
    pub(super) is_ready: bool,
    pub(super) is_live: bool,
    pub(super) definition: String,
    pub(super) parent_index: Option<String>,
}

pub(super) async fn final_index_catalog(
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
pub(super) struct PrivacyAuditorSecurityCatalog {
    pub(super) auditor_grants: std::collections::BTreeSet<String>,
    pub(super) policies: std::collections::BTreeSet<String>,
}

pub(super) async fn privacy_auditor_security_catalog(
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

pub(super) async fn foreign_key_targets(
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

/// Extracts stable PostgreSQL error fields for schema-boundary assertions.
pub(super) fn postgres_error_fact(error: sqlx::Error) -> (Option<String>, Option<String>) {
    (
        error
            .as_database_error()
            .and_then(|database| database.code().map(|code| code.into_owned())),
        error
            .as_database_error()
            .and_then(|database| database.constraint().map(ToOwned::to_owned)),
    )
}

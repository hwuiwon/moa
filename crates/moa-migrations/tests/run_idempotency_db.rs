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

/// Destructive migration under focused PostgreSQL validation.
const V000336_SQL: &str =
    include_str!("../migrations/postgres/V000336__remove_legacy_procedure_runs.sql");

/// Unified execute-routing analytics and audit cutover migration.
const V000337_SQL: &str = include_str!("../migrations/postgres/V000337__execution_analytics.sql");

/// Current migration ownership inventory.
const MIGRATION_OWNERSHIP: &str = include_str!("../migration-ownership.toml");

fn removed_serialized_value(parts: &[&str]) -> String {
    parts.concat()
}

#[test]
fn execution_analytics_source_contract_is_exact_offline() {
    // Pins: V337 owns the decision-plus-strategy execution audit, materialization, fact,
    // sequence/high-water, trace-context, and archive-cutover contracts without
    // recreating any procedure-era compatibility surface.
    for table in [
        "CREATE TABLE moa.execution_route_audit",
        "CREATE TABLE moa.execution_planner_call_audit",
        "CREATE TABLE moa.execution_compile_audit",
        "CREATE TABLE moa.execution_node_materialization",
        "CREATE TABLE analytics.clickhouse_schema_upgrade_state",
    ] {
        assert!(V000337_SQL.contains(table), "missing V337 table: {table}");
    }
    for contract in [
        "7b83c5c2-5cf7-5fa0-8eb6-2d7c6e0f1d11",
        "moa.execution.route-audit",
        "'source','classifier_outcome','provider_model','prompt_version'",
        "'objective_hash','response_hash','confidence_bps'",
        "'missing_input_count','usage','cost_microusd','duration_micros'",
        "'kind','stage','decision','strategy','provenance','accepted_at'",
        "stage TEXT,\n    decision TEXT,\n    strategy TEXT,",
        "decision = 'execute' AND strategy = 'inline'",
        "decision = 'execute' AND strategy = 'durable'",
        "IF route_valid IS NOT TRUE THEN",
        "'respond','execute','needs_input'",
        "'initial','durable_upgrade'",
        "'low_confidence','context_forced_inline'",
        "moa.execution.planner-audit",
        "moa.execution.compile-audit",
        "octet_length(candidate_json::TEXT) <= 1048576",
        "octet_length(compiler_report::TEXT) <= 262144",
        "octet_length(validation_report::TEXT) <= 262144",
        "pg_advisory_xact_lock_shared(1297047877, 337)",
        "CREATE SEQUENCE moa.execution_analytics_change_seq",
        "CREATE TRIGGER execution_route_audit_immutable_guard",
        "ADD COLUMN cursor_seq BIGINT",
        "pass_high_water_seq",
        "execution_dimensions",
        "execution_planning_context_normalized_scope_key",
        "execution_template_admission_run_normalized_scope_fkey",
        "DROP MATERIALIZED VIEW analytics.execution_task_fact",
        "DROP MATERIALIZED VIEW analytics.execution_run_fact",
        "task_id AS task_id",
        "sac.agent_id",
        "traceparent",
        "tracestate",
    ] {
        assert!(
            V000337_SQL.contains(contract),
            "missing V337 source contract: {contract}"
        );
    }
    let task_fact_drop = V000337_SQL
        .find("DROP MATERIALIZED VIEW analytics.execution_task_fact")
        .expect("V337 drops the dependent task fact first");
    let run_fact_drop = V000337_SQL
        .find("DROP MATERIALIZED VIEW analytics.execution_run_fact")
        .expect("V337 drops the run fact second");
    let normalized_columns = V000337_SQL
        .find("ADD COLUMN source_kind")
        .expect("V337 adds normalized run columns");
    let run_fact_create = V000337_SQL
        .find("CREATE MATERIALIZED VIEW analytics.execution_run_fact")
        .expect("V337 recreates the run fact");
    let task_fact_create = V000337_SQL
        .find("CREATE MATERIALIZED VIEW analytics.execution_task_fact")
        .expect("V337 recreates the task fact");
    assert!(
        task_fact_drop < run_fact_drop
            && run_fact_drop < normalized_columns
            && normalized_columns < run_fact_create
            && run_fact_create < task_fact_create,
        "V337 fact rebuild order must remain dependency-safe"
    );
    for forbidden in [
        "procedure_run_fact",
        "procedure_node_run_fact",
        "artifact_run",
        "artifact_node_run",
        "task_uid",
        "source_ref",
        "capability_ref",
        "decision = 'routed'",
        "route_rationale",
        "execution_route_rationale_is_valid",
    ] {
        assert!(
            !V000337_SQL.contains(forbidden),
            "V337 must not recreate superseded surface `{forbidden}`"
        );
    }
    for forbidden_parts in [
        ["route_", "mode"].as_slice(),
        ["act_", "escalation"].as_slice(),
        ["context_forced_", "act"].as_slice(),
        ["explicit_", "run"].as_slice(),
    ] {
        let forbidden = removed_serialized_value(forbidden_parts);
        assert!(
            !V000337_SQL.contains(&forbidden),
            "V337 must not recreate superseded surface `{forbidden}`"
        );
    }
    for ownership in [
        "name = \"execution_route_audit\"\nschema = \"moa\"\nowner = \"moa-execution\"",
        "name = \"execution_planner_call_audit\"\nschema = \"moa\"\nowner = \"moa-execution\"",
        "name = \"execution_compile_audit\"\nschema = \"moa\"\nowner = \"moa-execution\"",
        "name = \"execution_node_materialization\"\nschema = \"moa\"\nowner = \"moa-execution\"",
        "name = \"clickhouse_schema_upgrade_state\"\nschema = \"analytics\"\nowner = \"moa-analytics\"",
    ] {
        assert!(
            MIGRATION_OWNERSHIP.contains(ownership),
            "missing V337 ownership row: {ownership}"
        );
    }
}

#[test]
fn procedure_runtime_cutover_discards_history_without_compatibility_paths_offline() {
    // Pins: V336 removes the procedure runtime without importing, translating,
    // aliasing, or preserving procedure-era execution data.
    let node_drop = V000336_SQL
        .find("DROP TABLE moa.artifact_node_run;")
        .expect("V000336 drops procedure child history");
    let run_drop = V000336_SQL
        .find("DROP TABLE moa.artifact_run;")
        .expect("V000336 drops procedure parent history");
    assert!(
        node_drop < run_drop,
        "procedure child table must drop first"
    );
    assert!(!V000336_SQL.contains("INSERT INTO moa.execution_run"));
    assert!(!V000336_SQL.contains("INSERT INTO moa.execution_task"));
    assert!(!V000336_SQL.contains("legacy_migration"));
    assert!(V000336_SQL.contains("DELETE FROM moa.experiment_trial"));
    assert!(V000336_SQL.contains("DELETE FROM moa.experiment_run"));
    assert!(V000336_SQL.contains("DROP COLUMN procedure_run_uid"));
    assert!(!V000336_SQL.contains("CREATE VIEW moa.artifact_run"));
    assert!(!V000336_SQL.contains("CREATE VIEW moa.artifact_node_run"));
}

/// Returns the Postgres URL used by integration tests, mirroring the runtime
/// `MOA_DATABASE_URL` setting and falling back to the compose default.
fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Returns a process-and-time-unique throwaway database name.
fn unique_db_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("moa_mig_idem_{}_{nanos}", std::process::id())
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

    let first = moa_migrations::run_reporting_applied(target_url).await?;
    let second = moa_migrations::run_reporting_applied(target_url).await?;
    Ok((first, second))
}

/// Applies a central migration prefix and reports only migrations applied by the call.
async fn apply_through_version(
    target_url: &str,
    version: i32,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let (mut client, connection) =
        tokio_postgres::connect(target_url, tokio_postgres::NoTls).await?;
    let connection_task = tokio::spawn(connection);
    let report = embedded_for_cutover_proof::migrations::runner()
        .set_target(refinery::Target::Version(version))
        .run_async(&mut client)
        .await?;
    drop(client);
    connection_task.await??;
    Ok(report
        .applied_migrations()
        .iter()
        .map(ToString::to_string)
        .collect())
}

/// Applies the complete Task 10 migration prefix and stops before V000337.
async fn apply_through_v000336(
    target_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    apply_through_version(target_url, 336).await?;
    Ok(())
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
async fn rebuild_state_enforces_single_operation_and_generation_on_a_fresh_database_db() {
    // Pins the V000351 guarantees that are the database's job, not the
    // application's, verified against a database built from the full migration
    // set rather than the long-running Compose one: forced row-level security
    // on every rebuild table, one nonterminal operation per storage partition,
    // and one active generation per storage partition. An application that
    // "checked first" would pass a unit test and still admit two concurrent
    // rebuilds; these indexes are what actually refuse the second.
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

        let unforced: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT relation.relname
              FROM pg_catalog.pg_class AS relation
              JOIN pg_catalog.pg_namespace AS namespace
                ON namespace.oid = relation.relnamespace
             WHERE namespace.nspname = 'moa'
               AND relation.relname IN (
                   'knowledge_rebuild_operation',
                   'knowledge_rebuild_generation',
                   'knowledge_active_generation',
                   'knowledge_rebuild_candidate_vector',
                   'knowledge_rechunk_staging'
               )
               AND NOT (relation.relrowsecurity AND relation.relforcerowsecurity)
             ORDER BY relation.relname
            "#,
        )
        .fetch_all(&target)
        .await?;

        let tenant_id = uuid::Uuid::now_v7();
        let partition = tenant_id.to_string();
        sqlx::query(
            "INSERT INTO moa.knowledge_rebuild_operation \
             (operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token) \
             VALUES ($1, $2, $3, 'reembed', 'building', $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(tenant_id)
        .bind(&partition)
        .bind(uuid::Uuid::now_v7())
        .execute(&target)
        .await?;
        let second_operation = sqlx::query(
            "INSERT INTO moa.knowledge_rebuild_operation \
             (operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token) \
             VALUES ($1, $2, $3, 'rechunk', 'planning', $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(tenant_id)
        .bind(&partition)
        .bind(uuid::Uuid::now_v7())
        .execute(&target)
        .await
        .is_err();

        // A terminal operation releases the partition for the next rebuild.
        sqlx::query(
            "UPDATE moa.knowledge_rebuild_operation SET lifecycle = 'finalized' \
             WHERE storage_partition_id = $1",
        )
        .bind(&partition)
        .execute(&target)
        .await?;
        let after_terminal = sqlx::query(
            "INSERT INTO moa.knowledge_rebuild_operation \
             (operation_uid, tenant_id, storage_partition_id, kind, lifecycle, owner_token) \
             VALUES ($1, $2, $3, 'rechunk', 'planning', $4)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(tenant_id)
        .bind(&partition)
        .bind(uuid::Uuid::now_v7())
        .execute(&target)
        .await
        .is_ok();

        for sequence in [1_i64, 2] {
            sqlx::query(
                "INSERT INTO moa.knowledge_rebuild_generation \
                 (generation_uid, tenant_id, storage_partition_id, generation_seq, \
                  embedding_model, embedding_model_version, embedding_dimension, \
                  turbopuffer_namespace, state) \
                 VALUES ($1, $2, $3, $4, 'embed-v4.0', 1, 1024, $5, \
                         CASE WHEN $4 = 1 THEN 'active' ELSE 'candidate' END)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(tenant_id)
            .bind(&partition)
            .bind(sequence)
            .bind(format!("{partition}__g{sequence}"))
            .execute(&target)
            .await?;
        }
        let second_active = sqlx::query(
            "UPDATE moa.knowledge_rebuild_generation SET state = 'active' \
             WHERE storage_partition_id = $1 AND generation_seq = 2",
        )
        .bind(&partition)
        .execute(&target)
        .await
        .is_err();

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            unforced,
            second_operation,
            after_terminal,
            second_active,
        ))
    }
    .await;

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (unforced, second_operation_refused, after_terminal_allowed, second_active_refused) =
        outcome.expect("rebuild-state assertions should complete on a fresh database");

    assert!(
        unforced.is_empty(),
        "every rebuild table must force row-level security, missing on: {unforced:?}"
    );
    assert!(
        second_operation_refused,
        "a second nonterminal rebuild for one partition must be refused by the index"
    );
    assert!(
        after_terminal_allowed,
        "a terminal rebuild must release the partition for the next one"
    );
    assert!(
        second_active_refused,
        "a second active generation for one partition must be refused by the index"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn refinery_clean_apply_then_second_apply_reports_no_new_migrations_db() {
    // Pins clean-apply + idempotency of the central migration runner on a pristine
    // database: the first run applies the full embedded set, and a second run reports
    // zero newly applied migrations. Refinery's schema-history bookkeeping is what
    // makes the re-run a no-op; a migration rewritten to re-run unconditionally, or a
    // non-clean-appliable migration set, would fail one of these assertions.
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
    let outcome = clean_apply_then_reapply(&target_url).await;

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second) =
        outcome.expect("central migration runs should complete on a fresh database");
    assert!(
        !first.is_empty(),
        "a pristine database must apply migrations on the first run"
    );
    assert!(
        second.is_empty(),
        "second apply must report no newly applied migrations, got {second:?}"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn execution_analytics_fresh_cutover_and_exact_contract_db() {
    // Pins: V337 starts normalized audit storage empty, installs every finite SQL matrix and
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
        .expect("create V337 contract database");

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
        target.close().await;
        apply_through_v000336(&target_url).await?;

        let first = moa_migrations::run_reporting_applied(&target_url).await?;
        let second = moa_migrations::run_reporting_applied(&target_url).await?;
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

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
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
    ) = outcome.expect("V337 staged contract should execute on PostgreSQL");

    assert_eq!(
        first.len(),
        1,
        "targeted apply must contain only V337, got {first:?}"
    );
    assert!(
        first[0].contains("337") && first[0].contains("execution_analytics"),
        "targeted apply must report V337 exactly, got {first:?}"
    );
    assert!(second.is_empty(), "second V337 apply must be empty");
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
async fn execution_analytics_preflight_abort_preserves_task10_state_db() {
    // Pins: an invalid Task 10 source-provenance cohort aborts V337 before any
    // normalized column/table survives, preserves the source JSON bytes, and
    // leaves refinery history unchanged.
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
        .expect("create V337 abort database");

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
        target.close().await;
        apply_through_v000336(&target_url).await?;

        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::raw_sql(
            r#"
            SET session_replication_role = replica;
            INSERT INTO moa.execution_run (
                run_uid,tenant_id,contact_id,session_id,
                originating_user_sequence_num,planning_context_uid,
                planning_context_hash,owner_user_id,goal_contract,
                initial_plan,active_plan,initial_plan_hash,active_plan_hash,
                capability_catalog,authorization_envelope,source_provenance,input,
                status,queued_at
            ) VALUES (
                '00000000-0000-0000-0000-000000337900',
                '00000000-0000-0000-0000-000000337901',NULL,
                '00000000-0000-0000-0000-000000337902',0,
                '00000000-0000-0000-0000-000000337903',repeat('1',64),
                'owner','{}','{}','{}',repeat('2',64),repeat('3',64),
                '{}','{}','{"kind":"generated_plan"}','{}','queued',NOW()
            );
            SET session_replication_role = origin;
            "#,
        )
        .execute(&target)
        .await?;
        let before_bytes: String = sqlx::query_scalar(
            "SELECT source_provenance::TEXT FROM moa.execution_run \
             WHERE run_uid = '00000000-0000-0000-0000-000000337900'",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;

        let apply_error = moa_migrations::run_reporting_applied(&target_url)
            .await
            .expect_err("invalid provenance must abort V337");
        let apply_error = format!("{apply_error:#}");
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let source_kind_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                SELECT 1 FROM information_schema.columns \
                WHERE table_schema = 'moa' AND table_name = 'execution_run' \
                  AND column_name = 'source_kind'\
             )",
        )
        .fetch_one(&target)
        .await?;
        let audit_table_exists: bool =
            sqlx::query_scalar("SELECT to_regclass('moa.execution_route_audit') IS NOT NULL")
                .fetch_one(&target)
                .await?;
        let after_bytes: String = sqlx::query_scalar(
            "SELECT source_provenance::TEXT FROM moa.execution_run \
             WHERE run_uid = '00000000-0000-0000-0000-000000337900'",
        )
        .fetch_one(&target)
        .await?;
        let recorded_v337: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                SELECT 1 FROM refinery_schema_history WHERE version = 337\
             )",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            apply_error,
            before_bytes,
            after_bytes,
            source_kind_exists,
            audit_table_exists,
            recorded_v337,
        ))
    }
    .await;

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (error, before_bytes, after_bytes, source_kind_exists, audit_table_exists, recorded_v337) =
        outcome.expect("inspect V337 unchanged-state abort");
    assert!(
        error.contains("00000000-0000-0000-0000-000000337900"),
        "preflight error must report the complete offending run set: {error}"
    );
    assert_eq!(after_bytes, before_bytes);
    assert!(!source_kind_exists);
    assert!(!audit_table_exists);
    assert!(!recorded_v337);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn procedure_runtime_cutover_discards_rows_without_execution_translation_db() {
    // Pins: adopting V336 is a destructive reset boundary. Procedure-backed
    // experiment state disappears and does not become an execution run.
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
        .expect("create destructive cutover database");

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
        target.close().await;
        apply_through_version(&target_url, 335).await?;

        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::raw_sql(
            r#"
            INSERT INTO analytics.score_run (
                run_id,storage_partition_id,user_id,source
            ) VALUES (
                '00000000-0000-0000-0000-000000033602',
                '00000000-0000-0000-0000-000000003360',
                '00000000-0000-0000-0000-000000003361',
                'experiment_run'
            );
            INSERT INTO moa.experiment_run (
                run_uid,storage_partition_id,user_id,name,target_kind,status,
                target,variant,score_run_id,created_by_identity
            ) VALUES (
                '00000000-0000-0000-0000-000000033603',
                '00000000-0000-0000-0000-000000003360',
                '00000000-0000-0000-0000-000000003361',
                'discard proof','procedure','completed',
                '{"kind":"procedure","procedure_ref":"skill://discard-proof"}',
                '{}',
                '00000000-0000-0000-0000-000000033602',
                '{"kind":"operator","id":"cutover-proof"}'
            );
            "#,
        )
        .execute(&target)
        .await?;
        target.close().await;

        let applied = apply_through_version(&target_url, 336).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let state: (bool, i64, i64) = sqlx::query_as(
            "SELECT \
                 to_regclass('moa.artifact_run') IS NULL \
                     AND to_regclass('moa.artifact_node_run') IS NULL, \
                 (SELECT COUNT(*) FROM moa.experiment_run \
                  WHERE run_uid = '00000000-0000-0000-0000-000000033603'), \
                 (SELECT COUNT(*) FROM moa.execution_run)",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((applied, state))
    }
    .await;

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (applied, (procedure_tables_absent, experiment_rows, execution_rows)) =
        outcome.expect("destructive V336 cutover should execute");
    assert_eq!(applied.len(), 1, "targeted apply must contain only V336");
    assert!(applied[0].contains("336"));
    assert!(procedure_tables_absent);
    assert_eq!(experiment_rows, 0);
    assert_eq!(execution_rows, 0);
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

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let default = outcome.expect("inspect clean agent migration");
    assert_eq!(default.as_deref(), Some("gen_random_uuid()"));
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn full_database_runner_installs_post_cutover_schema_and_foreign_keys_db() {
    // Pins: a pristine physical database reaches the complete post-V336/V337
    // schema through the canonical runner, preserves the final experiment FKs,
    // removes legacy procedure tables, and is idempotent on re-apply.
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
        let recorded_cutovers: Vec<(i32, String)> = sqlx::query_as(
            "SELECT version, name FROM refinery_schema_history \
             WHERE version IN (336, 337) ORDER BY version",
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

    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
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
            (336, "remove_legacy_procedure_runs".to_string()),
            (337, "execution_analytics".to_string()),
        ],
        "the final two cutovers must be recorded exactly once"
    );
    assert!(
        first.iter().any(|migration| migration.contains("336"))
            && first.iter().any(|migration| migration.contains("337")),
        "the pristine apply must include V336 and V337: {first:?}"
    );
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
async fn knowledge_link_claims_v000358_fresh_and_idempotent_db() {
    // Pins: V000358 bootstraps the link claim table on a pristine database and
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

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("link claim migration should apply on a fresh database");
    let (claims_forced, policies, finalized_requires_run, trigger_boundary_column) = facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("knowledge_link_claims")),
        "a pristine database must apply V000358, got {first:?}"
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
async fn tenant_credential_vault_v000346_fresh_and_idempotent_db() {
    // Pins: V000346 bootstraps the durable credential owner on a pristine
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

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("credential-vault migration should apply on a fresh database");
    let (versions_forced, operations_forced, policies, active_partial_unique, audit_update_granted) =
        facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("tenant_credential_vault")),
        "a pristine database must apply V000346, got {first:?}"
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

/// Legacy content-hash graph state seeded at V000346 so the V000347 backfill has
/// real work to do.
///
/// The shape is the defect this migration exists to remove: two different
/// documents whose identical paragraph collapsed onto ONE shared chunk node, plus
/// a tombstoned occurrence of the same text under a superseded version, plus a
/// chunk that never reached the graph at all.
#[derive(Debug, Clone, Copy)]
struct LegacyChunkGraph {
    tenant_id: uuid::Uuid,
    shared_chunk_node: uuid::Uuid,
    document_node: uuid::Uuid,
    entity_node: uuid::Uuid,
    fact_node: uuid::Uuid,
    alpha_version: uuid::Uuid,
    alpha_chunk: uuid::Uuid,
    alpha_unlinked_chunk: uuid::Uuid,
    alpha_superseded_chunk: uuid::Uuid,
    beta_chunk: uuid::Uuid,
}

impl LegacyChunkGraph {
    fn new() -> Self {
        Self {
            tenant_id: uuid::Uuid::now_v7(),
            shared_chunk_node: uuid::Uuid::now_v7(),
            document_node: uuid::Uuid::now_v7(),
            entity_node: uuid::Uuid::now_v7(),
            fact_node: uuid::Uuid::now_v7(),
            alpha_version: uuid::Uuid::now_v7(),
            alpha_chunk: uuid::Uuid::now_v7(),
            alpha_unlinked_chunk: uuid::Uuid::now_v7(),
            alpha_superseded_chunk: uuid::Uuid::now_v7(),
            beta_chunk: uuid::Uuid::now_v7(),
        }
    }

    /// Returns every seeded chunk uid in a stable order.
    fn chunk_uids(&self) -> Vec<uuid::Uuid> {
        vec![
            self.alpha_chunk,
            self.alpha_unlinked_chunk,
            self.alpha_superseded_chunk,
            self.beta_chunk,
        ]
    }
}

/// Applies `SET LOCAL ROLE moa_app` plus the tenant RLS session variables that
/// `ScopedConn` installs at runtime, so seeds and reads go through the same
/// forced-RLS path production uses.
async fn begin_as_app(
    pool: &PgPool,
    tenant_id: Option<uuid::Uuid>,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, Box<dyn std::error::Error + Send + Sync>> {
    let mut tx = pool.begin().await?;
    let tenant_text = tenant_id
        .map(|tenant| tenant.to_string())
        .unwrap_or_default();
    sqlx::query(
        "SELECT pg_catalog.set_config('moa.tenant_id', $1, true), \
                pg_catalog.set_config('moa.storage_partition_id', $1, true), \
                pg_catalog.set_config('moa.contact_id', '', true), \
                pg_catalog.set_config('moa.control_plane', 'false', true)",
    )
    .bind(&tenant_text)
    .execute(&mut *tx)
    .await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Seeds the pre-V000347 shared-chunk-node world through forced RLS.
async fn seed_legacy_shared_chunk_graph(
    pool: &PgPool,
) -> Result<LegacyChunkGraph, Box<dyn std::error::Error + Send + Sync>> {
    let seed = LegacyChunkGraph::new();
    let tenant_text = seed.tenant_id.to_string();
    let connection_uid = uuid::Uuid::now_v7();
    let alpha_object = uuid::Uuid::now_v7();
    let beta_object = uuid::Uuid::now_v7();
    let alpha_superseded_version = uuid::Uuid::now_v7();
    let beta_version = uuid::Uuid::now_v7();
    let mut tx = begin_as_app(pool, Some(seed.tenant_id)).await?;

    // An external vector backend, so the outbox backfill has an addressee.
    sqlx::query(
        "INSERT INTO moa.storage_partition_state (storage_partition_id, vector_backend) \
         VALUES ($1, 'turbopuffer')",
    )
    .bind(&tenant_text)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO moa.knowledge_connections ( \
             connection_uid, tenant_id, storage_partition_id, provider, provider_config_key, \
             provider_connection_id, connector, credential_ref, status, metadata) \
         VALUES ($1, $2, $3, 'merge', 'occurrence-config', 'occurrence-account', 'drive', \
                 'occurrence-credential', 'active', '{}'::JSONB)",
    )
    .bind(connection_uid)
    .bind(seed.tenant_id)
    .bind(&tenant_text)
    .execute(&mut *tx)
    .await?;

    for (object_uid, external_id) in [(alpha_object, "doc-alpha"), (beta_object, "doc-beta")] {
        sqlx::query(
            "INSERT INTO moa.knowledge_objects ( \
                 object_uid, tenant_id, storage_partition_id, connection_id, object_type, \
                 external_object_id, title, change_token, source_uri, status, metadata) \
             VALUES ($1, $2, $3, $4, 'document', $5, $5, 'etag-1', \
                     'https://example.test/' || $5, 'active', '{}'::JSONB)",
        )
        .bind(object_uid)
        .bind(seed.tenant_id)
        .bind(&tenant_text)
        .bind(connection_uid)
        .bind(external_id)
        .execute(&mut *tx)
        .await?;
    }

    for (version_uid, object_uid, content_hash, age_seconds) in [
        (
            alpha_superseded_version,
            alpha_object,
            "hash-alpha-v0",
            120_i32,
        ),
        (seed.alpha_version, alpha_object, "hash-alpha-v1", 60),
        (beta_version, beta_object, "hash-beta-v1", 60),
    ] {
        sqlx::query(
            "INSERT INTO moa.knowledge_document_versions ( \
                 document_version_uid, tenant_id, storage_partition_id, object_id, \
                 parser_provider, content_hash, metadata, created_at) \
             VALUES ($1, $2, $3, $4, 'native', $5, '{}'::JSONB, \
                     now() - make_interval(secs => $6))",
        )
        .bind(version_uid)
        .bind(seed.tenant_id)
        .bind(&tenant_text)
        .bind(object_uid)
        .bind(content_hash)
        .bind(f64::from(age_seconds))
        .execute(&mut *tx)
        .await?;
    }

    // Two documents' identical paragraph share one content-hash node; the
    // superseded occurrence is tombstoned; one chunk never reached the graph.
    for (chunk_uid, version_uid, graph_node_uid, chunk_hash, metadata) in [
        (
            seed.alpha_chunk,
            seed.alpha_version,
            Some(seed.shared_chunk_node),
            "shared-content-hash",
            "{}",
        ),
        (
            seed.alpha_unlinked_chunk,
            seed.alpha_version,
            None,
            "unlinked-content-hash",
            r#"{"active": true}"#,
        ),
        (
            seed.alpha_superseded_chunk,
            alpha_superseded_version,
            Some(seed.shared_chunk_node),
            "shared-content-hash",
            r#"{"active": false}"#,
        ),
        (
            seed.beta_chunk,
            beta_version,
            Some(seed.shared_chunk_node),
            "shared-content-hash",
            r#"{"active": true}"#,
        ),
    ] {
        sqlx::query(
            "INSERT INTO moa.knowledge_chunks ( \
                 chunk_uid, tenant_id, storage_partition_id, document_version_id, \
                 graph_node_uid, chunk_hash, block_hashes, heading_path, text, ordinal, \
                 token_count, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6, ARRAY['block-1']::TEXT[], \
                     ARRAY['Policies']::TEXT[], 'Reimbursement requires manager approval.', \
                     0, 6, $7::JSONB)",
        )
        .bind(chunk_uid)
        .bind(seed.tenant_id)
        .bind(&tenant_text)
        .bind(version_uid)
        .bind(graph_node_uid)
        .bind(chunk_hash)
        .bind(metadata)
        .execute(&mut *tx)
        .await?;
    }

    for (uid, label, name) in [
        (seed.shared_chunk_node, "Chunk", "shared-content-hash"),
        (seed.document_node, "Document", "Alpha policy"),
        (seed.entity_node, "Entity", "Manager approval"),
        (
            seed.fact_node,
            "Fact",
            "Reimbursement requires manager approval",
        ),
    ] {
        sqlx::query(
            "INSERT INTO moa.node_index ( \
                 uid, label, storage_partition_id, tenant_id, name, pii_class, confidence, \
                 properties_summary, data_subject_id) \
             VALUES ($1, $2, $3, $4, $5, 'none', 0.95, \
                     jsonb_build_object('chunk_hash', 'shared-content-hash'), $4)",
        )
        .bind(uid)
        .bind(label)
        .bind(&tenant_text)
        .bind(seed.tenant_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }

    for (start_uid, end_uid, label) in [
        (seed.document_node, seed.shared_chunk_node, "CONTAINS"),
        (seed.shared_chunk_node, seed.entity_node, "MENTIONED_IN"),
        (seed.shared_chunk_node, seed.fact_node, "DERIVED_FROM"),
    ] {
        sqlx::query(
            "INSERT INTO moa.edge_index ( \
                 uid, label, start_uid, end_uid, storage_partition_id, tenant_id, properties) \
             VALUES ($1, $2, $3, $4, $5, $6, '{}'::JSONB)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(label)
        .bind(start_uid)
        .bind(end_uid)
        .bind(&tenant_text)
        .bind(seed.tenant_id)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "INSERT INTO moa.embeddings ( \
             uid, storage_partition_id, tenant_id, label, pii_class, embedding, \
             embedding_model, embedding_model_version) \
         VALUES ($1, $2, $3, 'Chunk', 'none', \
                 ('[' || array_to_string(array_fill(0.0125::REAL, ARRAY[1024]), ',') || ']')::public.halfvec(1024), \
                 'embed-v4.0', 7)",
    )
    .bind(seed.shared_chunk_node)
    .bind(&tenant_text)
    .bind(seed.tenant_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(seed)
}

/// Collects the occurrence facts the V000347 backfill must produce.
#[allow(clippy::type_complexity)]
async fn occurrence_backfill_facts(
    pool: &PgPool,
    seed: &LegacyChunkGraph,
) -> Result<
    (
        Vec<(uuid::Uuid, uuid::Uuid, bool)>,
        i64,
        Vec<(uuid::Uuid, String)>,
        Vec<(uuid::Uuid, String, i32)>,
        Vec<(uuid::Uuid, String)>,
        i64,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let mut tx = begin_as_app(pool, Some(seed.tenant_id)).await?;
    // (chunk uid, persisted graph identity, occurrence is active in the graph)
    let occurrences = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, bool)>(
        "SELECT chunk.chunk_uid, chunk.graph_node_uid, occurrence.valid_to IS NULL \
           FROM moa.knowledge_chunks AS chunk \
           JOIN moa.node_index AS occurrence ON occurrence.uid = chunk.chunk_uid \
          WHERE occurrence.label = 'Chunk' \
            AND occurrence.storage_partition_id = chunk.storage_partition_id \
            AND occurrence.tenant_id = chunk.tenant_id \
          ORDER BY chunk.chunk_uid",
    )
    .fetch_all(&mut *tx)
    .await?;
    let surviving_shared_nodes =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
            .bind(seed.shared_chunk_node)
            .fetch_one(&mut *tx)
            .await?;
    // Every edge now incident to an occurrence, as (occurrence uid, label).
    let occurrence_edges = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT chunk.chunk_uid, edge.label \
           FROM moa.knowledge_chunks AS chunk \
           JOIN moa.edge_index AS edge \
             ON edge.start_uid = chunk.chunk_uid OR edge.end_uid = chunk.chunk_uid \
          ORDER BY chunk.chunk_uid, edge.label",
    )
    .fetch_all(&mut *tx)
    .await?;
    let occurrence_embeddings = sqlx::query_as::<_, (uuid::Uuid, String, i32)>(
        "SELECT uid, embedding_model, embedding_model_version FROM moa.embeddings ORDER BY uid",
    )
    .fetch_all(&mut *tx)
    .await?;
    let queued_vector_sync = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT uid, op FROM moa.vector_sync_outbox WHERE processed_at IS NULL ORDER BY op, uid",
    )
    .fetch_all(&mut *tx)
    .await?;
    let shared_entities = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.node_index WHERE label IN ('Entity', 'Fact', 'Document')",
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        occurrences,
        surviving_shared_nodes,
        occurrence_edges,
        occurrence_embeddings,
        queued_vector_sync,
        shared_entities,
    ))
}

/// Collects the schema facts the occurrence invariant depends on.
///
/// Returns `(graph_node_uid_not_null, equality_constraint, occurrence_unique_index,
/// content_hash_unique_index_removed, chunks_force_rls)`.
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
async fn knowledge_graph_occurrences_v000347_fresh_and_idempotent_db() {
    // Pins: V000347 installs the occurrence invariant on a pristine database and
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

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second, facts, policies) =
        outcome.expect("occurrence migration should apply on a fresh database");
    let (not_null, equality_constraint, occurrence_unique, content_hash_unique_removed, force_rls) =
        facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("knowledge_graph_occurrences")),
        "a pristine database must apply V000347, got {first:?}"
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

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_graph_occurrence_backfill_v346_to_v347_db() {
    // Pins: upgrading a V000346 database that already collapsed two documents onto
    // one content-hash chunk node splits it into one occurrence per chunk row —
    // including the tombstoned occurrence and the chunk that never reached the
    // graph — clones the occurrence-specific edges and the current embedding,
    // queues the external-vector upserts plus the retirement deletion, retires the
    // shared node last, and leaves forced tenant RLS effective for `moa_app`.
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
        {
            let bootstrap = PgPoolOptions::new()
                .max_connections(1)
                .connect(&target_url)
                .await?;
            bootstrap
                .execute(
                    "CREATE EXTENSION IF NOT EXISTS vector; \
                     CREATE EXTENSION IF NOT EXISTS pgaudit;",
                )
                .await?;
            bootstrap.close().await;
        }
        apply_through_version(&target_url, 346).await?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;
        let seed = seed_legacy_shared_chunk_graph(&pool).await?;
        let applied = apply_through_version(&target_url, 347).await?;
        let facts = occurrence_backfill_facts(&pool, &seed).await?;
        let schema = knowledge_occurrence_schema_facts(&pool).await?;

        // Correct, wrong, and missing tenant visibility of the backfilled rows.
        let mut correct_tenant = begin_as_app(&pool, Some(seed.tenant_id)).await?;
        let visible_for_tenant = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.knowledge_chunks WHERE chunk_uid = ANY($1)",
        )
        .bind(seed.chunk_uids())
        .fetch_one(&mut *correct_tenant)
        .await?;
        // The occurrence invariant is enforced against the application role, not
        // just the migration role.
        let rejected_identity = sqlx::query(
            "INSERT INTO moa.knowledge_chunks ( \
                 chunk_uid, tenant_id, storage_partition_id, document_version_id, \
                 graph_node_uid, chunk_hash, text, ordinal, token_count, metadata) \
             SELECT gen_random_uuid(), chunk.tenant_id, chunk.storage_partition_id, \
                    chunk.document_version_id, gen_random_uuid(), 'forged', 'forged', \
                    99, 1, '{}'::JSONB \
               FROM moa.knowledge_chunks AS chunk WHERE chunk.chunk_uid = $1",
        )
        .bind(seed.alpha_chunk)
        .execute(&mut *correct_tenant)
        .await
        .expect_err("a chunk row may not claim another graph identity")
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.to_string()))
        .unwrap_or_default();
        correct_tenant.rollback().await?;

        // Content identity no longer constrains occurrences: a document version may
        // hold two occurrences of the same text.
        let mut repeated = begin_as_app(&pool, Some(seed.tenant_id)).await?;
        let repeated_occurrence = sqlx::query(
            "INSERT INTO moa.knowledge_chunks ( \
                 chunk_uid, tenant_id, storage_partition_id, document_version_id, \
                 graph_node_uid, chunk_hash, text, ordinal, token_count, metadata) \
             SELECT repeated.uid, chunk.tenant_id, chunk.storage_partition_id, \
                    chunk.document_version_id, repeated.uid, chunk.chunk_hash, chunk.text, \
                    42, chunk.token_count, '{}'::JSONB \
               FROM moa.knowledge_chunks AS chunk \
               CROSS JOIN (SELECT gen_random_uuid() AS uid) AS repeated \
              WHERE chunk.chunk_uid = $1",
        )
        .bind(seed.alpha_chunk)
        .execute(&mut *repeated)
        .await
        .map(|done| done.rows_affected());
        repeated.rollback().await?;

        let mut wrong_tenant = begin_as_app(&pool, Some(uuid::Uuid::now_v7())).await?;
        let visible_for_wrong_tenant = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.knowledge_chunks WHERE chunk_uid = ANY($1)",
        )
        .bind(seed.chunk_uids())
        .fetch_one(&mut *wrong_tenant)
        .await?;
        wrong_tenant.rollback().await?;

        let mut no_tenant = begin_as_app(&pool, None).await?;
        let visible_without_tenant = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.knowledge_chunks WHERE chunk_uid = ANY($1)",
        )
        .bind(seed.chunk_uids())
        .fetch_one(&mut *no_tenant)
        .await?;
        no_tenant.rollback().await?;

        // The remaining migrations still apply on top of the backfilled state.
        let remainder = moa_migrations::run_reporting_applied(&target_url).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            seed,
            applied,
            facts,
            schema,
            visible_for_tenant,
            rejected_identity,
            repeated_occurrence,
            visible_for_wrong_tenant,
            visible_without_tenant,
            remainder,
        ))
    }
    .await;

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (
        seed,
        applied,
        facts,
        schema,
        visible_for_tenant,
        rejected_identity,
        repeated_occurrence,
        visible_for_wrong_tenant,
        visible_without_tenant,
        remainder,
    ) = outcome.expect("V000346 to V000347 upgrade should complete");
    let (
        occurrences,
        surviving_shared_nodes,
        occurrence_edges,
        occurrence_embeddings,
        queued_vector_sync,
        shared_entities,
    ) = facts;

    assert!(
        applied
            .iter()
            .any(|migration| migration.contains("knowledge_graph_occurrences")),
        "the upgrade must apply V000347, got {applied:?}"
    );

    // One occurrence node per chunk row, identity equal to the chunk uid, and the
    // tombstoned chunk's occurrence invalidated even though the shared node was
    // alive for two other documents.
    let mut expected_occurrences = vec![
        (seed.alpha_chunk, seed.alpha_chunk, true),
        (seed.alpha_unlinked_chunk, seed.alpha_unlinked_chunk, true),
        (
            seed.alpha_superseded_chunk,
            seed.alpha_superseded_chunk,
            false,
        ),
        (seed.beta_chunk, seed.beta_chunk, true),
    ];
    expected_occurrences.sort();
    assert_eq!(
        occurrences, expected_occurrences,
        "every chunk row must own an occurrence node with its own identity and state"
    );
    assert_eq!(
        surviving_shared_nodes, 0,
        "the content-hash chunk node must be retired"
    );
    assert_eq!(
        shared_entities, 3,
        "document, entity, and fact nodes stay shared"
    );

    // Occurrence-specific edges are cloned per occurrence; the chunk that never
    // reached the graph gains none, because there was nothing to clone.
    let mut expected_edges = Vec::new();
    for chunk_uid in [
        seed.alpha_chunk,
        seed.alpha_superseded_chunk,
        seed.beta_chunk,
    ] {
        expected_edges.push((chunk_uid, "CONTAINS".to_string()));
        expected_edges.push((chunk_uid, "DERIVED_FROM".to_string()));
        expected_edges.push((chunk_uid, "MENTIONED_IN".to_string()));
    }
    expected_edges.sort();
    assert_eq!(
        occurrence_edges, expected_edges,
        "containment, provenance, and evidence edges must be rewired per occurrence"
    );

    // The current embedding is cloned beneath every ACTIVE occurrence, model and
    // version preserved. The tombstoned occurrence gets none (runtime
    // invalidation deletes vectors), and neither does the never-embedded chunk.
    let mut expected_embeddings = vec![
        (seed.alpha_chunk, "embed-v4.0".to_string(), 7),
        (seed.beta_chunk, "embed-v4.0".to_string(), 7),
    ];
    expected_embeddings.sort();
    assert_eq!(
        occurrence_embeddings, expected_embeddings,
        "each active occurrence owns its own embedding row"
    );

    let mut expected_sync = vec![
        (seed.shared_chunk_node, "delete".to_string()),
        (seed.alpha_chunk, "upsert".to_string()),
        (seed.beta_chunk, "upsert".to_string()),
    ];
    expected_sync.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    assert_eq!(
        queued_vector_sync, expected_sync,
        "external vector sync must gain the new occurrence upserts and the retirement delete"
    );

    let (not_null, equality_constraint, occurrence_unique, content_hash_unique_removed, force_rls) =
        schema;
    assert!(not_null && equality_constraint && occurrence_unique);
    assert!(content_hash_unique_removed);
    assert!(force_rls);

    assert_eq!(
        visible_for_tenant, 4,
        "the owning tenant still reads its own occurrences"
    );
    assert_eq!(
        rejected_identity, "23514",
        "the application role cannot write a chunk whose graph identity is not its own uid"
    );
    assert_eq!(
        repeated_occurrence.expect("a repeated paragraph is a legal second occurrence"),
        1
    );
    assert_eq!(
        visible_for_wrong_tenant, 0,
        "another tenant must not see backfilled occurrences"
    );
    assert_eq!(
        visible_without_tenant, 0,
        "a missing tenant scope must fail closed after the backfill"
    );
    assert!(
        !remainder
            .iter()
            .any(|migration| migration.contains("knowledge_graph_occurrences")),
        "V000347 must not re-apply once recorded, got {remainder:?}"
    );
}

/// Collects the schema facts the source-ACL admission boundary depends on.
///
/// Returns `(forced_rls, snapshot_policies, entry_policies, snapshot_update_granted,
/// entry_update_granted, epoch_trigger_tables, acl_mode_not_null, acl_state_not_null,
/// current_acl_complete_constraint, document_node_unique_index)`.
#[allow(clippy::type_complexity)]
async fn source_acl_schema_facts(
    pool: &PgPool,
) -> Result<
    (
        Vec<(String, bool)>,
        Vec<String>,
        Vec<String>,
        bool,
        bool,
        Vec<String>,
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
    let epoch_trigger_tables: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT class.relname::TEXT FROM pg_trigger AS trigger_row \
           JOIN pg_class AS class ON class.oid = trigger_row.tgrelid \
           JOIN pg_proc AS proc ON proc.oid = trigger_row.tgfoid \
          WHERE proc.proname = 'source_acl_epoch_trigger' \
          ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let acl_mode_not_null: bool = sqlx::query_scalar(
        "SELECT attnotnull FROM pg_attribute \
          WHERE attrelid = 'moa.knowledge_connections'::REGCLASS AND attname = 'acl_mode'",
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
        epoch_trigger_tables,
        acl_mode_not_null,
        acl_state_not_null,
        current_acl_complete,
        document_node_unique,
    ))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_source_acl_v000348_fresh_and_idempotent_db() {
    // Pins: V000348 installs the source-ACL boundary on a pristine database and
    // re-applies as a no-op. The properties asserted here are the ones admission
    // cannot be trusted without — forced RLS on every new table, snapshots and
    // their entries immutable (no UPDATE policy AND no UPDATE grant, so a
    // permission set cannot be edited under an unchanged revision), an epoch
    // trigger on every ACL-affecting table, and database-owned totality of
    // `acl_mode` / `acl_state`.
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

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("source ACL migration should apply on a fresh database");
    let (
        forced_rls,
        snapshot_policies,
        entry_policies,
        snapshot_update_granted,
        entry_update_granted,
        epoch_trigger_tables,
        acl_mode_not_null,
        acl_state_not_null,
        current_acl_complete,
        document_node_unique,
    ) = facts;

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("knowledge_source_acl")),
        "a pristine database must apply V000348, got {first:?}"
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
        epoch_trigger_tables,
        vec![
            "knowledge_connections".to_string(),
            "knowledge_objects".to_string(),
            "knowledge_source_acl_entries".to_string(),
            "knowledge_source_acl_snapshots".to_string(),
            "knowledge_source_principal_bindings".to_string(),
            "knowledge_source_principal_group_bindings".to_string(),
        ],
        "every ACL-affecting write must bump the tenant source-ACL epoch"
    );
    assert!(
        acl_mode_not_null,
        "a connection without an ACL mode must be impossible"
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
        document_node_unique,
        "one document graph node must belong to exactly one document version"
    );
}

/// A tenant knowledge world seeded at V000347, before source ACLs existed.
struct LegacySourceAclWorld {
    tenant_id: uuid::Uuid,
    other_tenant_id: uuid::Uuid,
    connection_uid: uuid::Uuid,
    object_uid: uuid::Uuid,
    version_uid: uuid::Uuid,
    chunk_uid: uuid::Uuid,
    document_node: uuid::Uuid,
}

impl LegacySourceAclWorld {
    fn new() -> Self {
        Self {
            tenant_id: uuid::Uuid::now_v7(),
            other_tenant_id: uuid::Uuid::now_v7(),
            connection_uid: uuid::Uuid::now_v7(),
            object_uid: uuid::Uuid::now_v7(),
            version_uid: uuid::Uuid::now_v7(),
            chunk_uid: uuid::Uuid::now_v7(),
            document_node: uuid::Uuid::now_v7(),
        }
    }
}

/// Seeds one permission-bearing connection with one document, one version, one
/// chunk occurrence, and the graph nodes ingestion would have written — all at
/// the V000347 schema, where nothing records who may read it.
async fn seed_pre_source_acl_world(
    pool: &PgPool,
) -> Result<LegacySourceAclWorld, Box<dyn std::error::Error + Send + Sync>> {
    let world = LegacySourceAclWorld::new();
    let tenant_text = world.tenant_id.to_string();
    let mut tx = begin_as_app(pool, Some(world.tenant_id)).await?;

    sqlx::query(
        "INSERT INTO moa.knowledge_connections ( \
             connection_uid, tenant_id, storage_partition_id, provider, provider_config_key, \
             provider_connection_id, connector, credential_ref, status, metadata) \
         VALUES ($1, $2, $3, 'nango', 'acl-config', 'acl-account', 'google-drive', \
                 'acl-credential', 'active', '{}'::JSONB)",
    )
    .bind(world.connection_uid)
    .bind(world.tenant_id)
    .bind(&tenant_text)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO moa.knowledge_objects ( \
             object_uid, tenant_id, storage_partition_id, connection_id, object_type, \
             external_object_id, title, change_token, source_uri, status, metadata) \
         VALUES ($1, $2, $3, $4, 'document', 'acl-doc', 'Board compensation memo', 'etag-1', \
                 'https://drive.example.test/acl-doc', 'active', '{}'::JSONB)",
    )
    .bind(world.object_uid)
    .bind(world.tenant_id)
    .bind(&tenant_text)
    .bind(world.connection_uid)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO moa.knowledge_document_versions ( \
             document_version_uid, tenant_id, storage_partition_id, object_id, \
             parser_provider, content_hash, metadata) \
         VALUES ($1, $2, $3, $4, 'native', 'acl-hash-v1', '{}'::JSONB)",
    )
    .bind(world.version_uid)
    .bind(world.tenant_id)
    .bind(&tenant_text)
    .bind(world.object_uid)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO moa.knowledge_chunks ( \
             chunk_uid, tenant_id, storage_partition_id, document_version_id, \
             graph_node_uid, chunk_hash, block_hashes, heading_path, text, ordinal, \
             token_count, metadata) \
         VALUES ($1, $2, $3, $4, $1, 'acl-chunk-hash', ARRAY['block-1']::TEXT[], \
                 ARRAY['Compensation']::TEXT[], 'Executive bonuses are approved quarterly.', \
                 0, 6, '{}'::JSONB)",
    )
    .bind(world.chunk_uid)
    .bind(world.tenant_id)
    .bind(&tenant_text)
    .bind(world.version_uid)
    .execute(&mut *tx)
    .await?;

    // The chunk occurrence node and the document node ingestion writes. The
    // document node carries `version_uid` in its properties, which is the only
    // link back to its governing object before V000348 stores it as a column.
    sqlx::query(
        "INSERT INTO moa.node_index ( \
             uid, label, storage_partition_id, tenant_id, name, pii_class, confidence, \
             properties_summary, data_subject_id) \
         VALUES ($1, 'Chunk', $2, $3, 'acl-chunk-hash', 'none', 0.95, \
                 jsonb_build_object('chunk_hash', 'acl-chunk-hash'), $3)",
    )
    .bind(world.chunk_uid)
    .bind(&tenant_text)
    .bind(world.tenant_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO moa.node_index ( \
             uid, label, storage_partition_id, tenant_id, name, pii_class, confidence, \
             properties_summary, data_subject_id) \
         VALUES ($1, 'Document', $2, $3, 'Board compensation memo', 'none', 0.95, \
                 jsonb_build_object('version_uid', $4::TEXT), $3)",
    )
    .bind(world.document_node)
    .bind(&tenant_text)
    .bind(world.tenant_id)
    .bind(world.version_uid)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(world)
}

/// Runs the PRODUCTION source-ACL admission predicate over `candidates` through
/// the app role at the given tenant scope, returning the admitted uids.
async fn admitted_uids(
    pool: &PgPool,
    tenant_id: Option<uuid::Uuid>,
    acl: &moa_core::types::memory::SourceAclContext,
    candidates: &[uuid::Uuid],
) -> Result<Vec<uuid::Uuid>, Box<dyn std::error::Error + Send + Sync>> {
    let mut tx = begin_as_app(pool, tenant_id).await?;
    let mut builder = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT node.uid FROM moa.node_index AS node WHERE node.uid = ANY(",
    );
    builder.push_bind(candidates.to_vec());
    builder.push(") AND ");
    moa_db::push_source_acl_predicate(&mut builder, "node.uid", acl);
    builder.push(" ORDER BY node.uid");
    let admitted = builder
        .build_query_scalar::<uuid::Uuid>()
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(admitted)
}

/// Reads one tenant's source-ACL epoch through the app role.
async fn source_acl_epoch(
    pool: &PgPool,
    tenant_id: uuid::Uuid,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let mut tx = begin_as_app(pool, Some(tenant_id)).await?;
    let epoch: i64 = sqlx::query_scalar(
        "SELECT COALESCE( \
             (SELECT epoch FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1), 0)",
    )
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(epoch)
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn knowledge_source_acl_v347_to_v348_hides_legacy_content_until_resync_db() {
    // Pins the whole upgrade contract through the PRODUCTION admission predicate,
    // executed as the non-BYPASSRLS app role:
    //
    //   * every pre-existing connection lands on `provider_managed` and every
    //     object on `incomplete`, so content ingested before ACLs existed is
    //     invisible to everyone — including a caller who later turns out to be
    //     authorized — until a resync captures real permissions;
    //   * the document node's governing object is recovered, so a denied
    //     document's title is not still retrievable through its graph node;
    //   * once a complete, revision-matched snapshot exists, an allowed principal
    //     is admitted, a wrong principal and an empty principal set are not, an
    //     explicit deny beats the allow, and a revision drift denies again;
    //   * the tenant epoch moves on every ACL write, and tenant scope stays a
    //     boundary: a missing or wrong `moa.tenant_id` sees nothing at all.
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
    let outcome = source_acl_upgrade_probe(&target_url).await;

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    outcome.expect("source ACL upgrade probe should succeed");
}

#[allow(clippy::too_many_lines)]
async fn source_acl_upgrade_probe(
    target_url: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use moa_core::types::memory::{SourceAclContext, SourcePrincipalFingerprint};

    {
        let bootstrap = PgPoolOptions::new()
            .max_connections(1)
            .connect(target_url)
            .await?;
        bootstrap
            .execute(
                "CREATE EXTENSION IF NOT EXISTS vector; \
                 CREATE EXTENSION IF NOT EXISTS pgaudit;",
            )
            .await?;
        bootstrap.close().await;
    }

    apply_through_version(target_url, 347).await?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(target_url)
        .await?;
    let world = seed_pre_source_acl_world(&pool).await?;
    pool.close().await;

    let remainder = moa_migrations::run_reporting_applied(target_url).await?;
    assert!(
        remainder
            .iter()
            .any(|applied| applied.contains("knowledge_source_acl")),
        "the upgrade must apply V000348, got {remainder:?}"
    );

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(target_url)
        .await?;
    let tenant_text = world.tenant_id.to_string();

    // --- Backfill determinism -------------------------------------------------
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    let acl_mode: String = sqlx::query_scalar(
        "SELECT acl_mode FROM moa.knowledge_connections WHERE connection_uid = $1",
    )
    .bind(world.connection_uid)
    .fetch_one(&mut *tx)
    .await?;
    let acl_state: String =
        sqlx::query_scalar("SELECT acl_state FROM moa.knowledge_objects WHERE object_uid = $1")
            .bind(world.object_uid)
            .fetch_one(&mut *tx)
            .await?;
    let backfilled_document_node: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT graph_node_uid FROM moa.knowledge_document_versions WHERE document_version_uid = $1",
    )
    .bind(world.version_uid)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    assert_eq!(
        acl_mode, "provider_managed",
        "a permission-bearing connection must not be promoted to tenant-public"
    );
    assert_eq!(
        acl_state, "incomplete",
        "an object whose permissions were never captured must be incomplete"
    );
    assert_eq!(
        backfilled_document_node,
        Some(world.document_node),
        "the document node must be tied back to its governing object"
    );

    // --- Nothing is visible before a resync ----------------------------------
    let candidates = vec![world.chunk_uid, world.document_node];
    let allowed_principal = SourcePrincipalFingerprint::from_digest(1, [0xA1; 32]);
    let denied_principal = SourcePrincipalFingerprint::from_digest(1, [0xD1; 32]);
    let stranger_principal = SourcePrincipalFingerprint::from_digest(1, [0x5E; 32]);

    let holder = SourceAclContext::new([allowed_principal.clone()], 0);
    assert!(
        admitted_uids(&pool, Some(world.tenant_id), &holder, &candidates)
            .await?
            .is_empty(),
        "legacy content must stay hidden even from a principal who will later be allowed"
    );

    // --- A complete, revision-matched snapshot admits the allowed principal ---
    let epoch_before = source_acl_epoch(&pool, world.tenant_id).await?;
    let snapshot_uid = uuid::Uuid::now_v7();
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    sqlx::query(
        "INSERT INTO moa.knowledge_source_acl_snapshots ( \
             snapshot_uid, tenant_id, storage_partition_id, connection_id, object_id, \
             provider_revision, snapshot_hash, provenance, complete, entry_count, captured_at) \
         VALUES ($1, $2, $3, $4, $5, 'rev-1', 'hash-1', 'provider_listing', TRUE, 2, now())",
    )
    .bind(snapshot_uid)
    .bind(world.tenant_id)
    .bind(&tenant_text)
    .bind(world.connection_uid)
    .bind(world.object_uid)
    .execute(&mut *tx)
    .await?;
    for (kind, principal) in [
        ("allow", &allowed_principal),
        ("allow", &denied_principal),
        ("deny", &denied_principal),
    ] {
        sqlx::query(
            "INSERT INTO moa.knowledge_source_acl_entries ( \
                 entry_uid, tenant_id, storage_partition_id, snapshot_id, entry_kind, \
                 principal_kind, principal_fingerprint, fingerprint_key_version) \
             VALUES ($1, $2, $3, $4, $5, 'user', $6, 1)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(world.tenant_id)
        .bind(&tenant_text)
        .bind(snapshot_uid)
        .bind(kind)
        .bind(principal.as_bytes())
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE moa.knowledge_objects \
            SET acl_state = 'current', acl_revision = 'rev-1', current_acl_snapshot_id = $2 \
          WHERE object_uid = $1",
    )
    .bind(world.object_uid)
    .bind(snapshot_uid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let epoch_after = source_acl_epoch(&pool, world.tenant_id).await?;
    assert!(
        epoch_after > epoch_before,
        "capturing an ACL must move the tenant epoch: {epoch_before} -> {epoch_after}"
    );

    assert_eq!(
        admitted_uids(&pool, Some(world.tenant_id), &holder, &candidates).await?,
        sorted(vec![world.chunk_uid, world.document_node]),
        "an allowed principal must see both the chunk occurrence and its document node"
    );
    assert!(
        admitted_uids(
            &pool,
            Some(world.tenant_id),
            &SourceAclContext::new([denied_principal.clone()], epoch_after),
            &candidates
        )
        .await?
        .is_empty(),
        "an explicit deny must beat the same principal's allow"
    );
    assert!(
        admitted_uids(
            &pool,
            Some(world.tenant_id),
            &SourceAclContext::new([allowed_principal.clone(), denied_principal], epoch_after),
            &candidates
        )
        .await?
        .is_empty(),
        "a deny anywhere in the caller's principal set must deny"
    );
    assert!(
        admitted_uids(
            &pool,
            Some(world.tenant_id),
            &SourceAclContext::new([stranger_principal], epoch_after),
            &candidates
        )
        .await?
        .is_empty(),
        "a principal absent from the snapshot must not be admitted"
    );
    assert!(
        admitted_uids(
            &pool,
            Some(world.tenant_id),
            &SourceAclContext::empty(epoch_after),
            &candidates
        )
        .await?
        .is_empty(),
        "an empty principal set must never be admitted"
    );

    // --- Tenant scope stays a boundary ---------------------------------------
    assert!(
        admitted_uids(&pool, Some(world.other_tenant_id), &holder, &candidates)
            .await?
            .is_empty(),
        "another tenant must not see this tenant's admitted content"
    );
    assert!(
        admitted_uids(&pool, None, &holder, &candidates)
            .await?
            .is_empty(),
        "a missing tenant scope must fail closed"
    );

    // --- A stale announcement hides content before the resync lands ----------
    // The snapshot is still complete and still revision-matched; only the
    // object's state changed. This is the window between "the provider told us
    // permissions moved" and "we captured what they moved to", and content must
    // be invisible for its whole duration.
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    sqlx::query("UPDATE moa.knowledge_objects SET acl_state = 'stale' WHERE object_uid = $1")
        .bind(world.object_uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    assert!(
        admitted_uids(&pool, Some(world.tenant_id), &holder, &candidates)
            .await?
            .is_empty(),
        "a stale ACL must hide content even though its snapshot still matches"
    );
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    sqlx::query("UPDATE moa.knowledge_objects SET acl_state = 'current' WHERE object_uid = $1")
        .bind(world.object_uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    assert_eq!(
        admitted_uids(&pool, Some(world.tenant_id), &holder, &candidates).await?,
        sorted(vec![world.chunk_uid, world.document_node]),
        "returning to `current` restores exactly what the snapshot allows"
    );

    // --- Revision drift denies without touching content ----------------------
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    sqlx::query("UPDATE moa.knowledge_objects SET acl_revision = 'rev-2' WHERE object_uid = $1")
        .bind(world.object_uid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    assert!(
        admitted_uids(&pool, Some(world.tenant_id), &holder, &candidates)
            .await?
            .is_empty(),
        "a snapshot for an older revision must not admit the current object"
    );
    let epoch_after_drift = source_acl_epoch(&pool, world.tenant_id).await?;
    assert!(
        epoch_after_drift > epoch_after,
        "an object ACL change must move the epoch so warm caches cannot survive it"
    );

    // --- Stored snapshots are immutable --------------------------------------
    let mut tx = begin_as_app(&pool, Some(world.tenant_id)).await?;
    let update_error = sqlx::query(
        "UPDATE moa.knowledge_source_acl_entries SET entry_kind = 'allow' WHERE snapshot_id = $1",
    )
    .bind(snapshot_uid)
    .execute(&mut *tx)
    .await
    .expect_err("editing a stored ACL entry must be refused");
    let sql_state = update_error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .map(|code| code.into_owned());
    assert_eq!(
        sql_state.as_deref(),
        Some("42501"),
        "a stored ACL entry must be un-editable by the app role: {update_error}"
    );
    drop(tx);

    pool.close().await;
    Ok(())
}

/// Returns a sorted copy so admitted-uid comparisons are order-independent.
fn sorted(mut uids: Vec<uuid::Uuid>) -> Vec<uuid::Uuid> {
    uids.sort();
    uids
}

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

/// Task 11 execution analytics and audit cutover migration.
const V000337_SQL: &str = include_str!("../migrations/postgres/V000337__execution_analytics.sql");

/// Current migration ownership inventory.
const MIGRATION_OWNERSHIP: &str = include_str!("../migration-ownership.toml");

#[test]
fn execution_analytics_source_contract_is_exact_offline() {
    // Pins: V337 owns the normalized execution audit, materialization, fact,
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
        "moa.execution.route-audit.v1",
        "'source','classifier_outcome','provider_model','prompt_version'",
        "'objective_hash','response_hash','confidence_bps'",
        "'missing_input_count','usage','cost_microusd','duration_micros'",
        "moa.execution.planner-audit.v1",
        "moa.execution.compile-audit.v1",
        "octet_length(candidate_json::TEXT) <= 1048576",
        "octet_length(compiler_report::TEXT) <= 262144",
        "octet_length(validation_report::TEXT) <= 262144",
        "pg_advisory_xact_lock_shared(1297047877, 337)",
        "CREATE SEQUENCE moa.execution_analytics_change_seq",
        "ADD COLUMN cursor_seq BIGINT",
        "pass_high_water_seq",
        "execution_dimensions_v2",
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
    ] {
        assert!(
            !V000337_SQL.contains(forbidden),
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
        let valid_route_cells: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM (
                VALUES
                ('initial','needs_input',NULL,'preflight_input_missing','blank_objective'),
                ('initial','routed','respond','simple_response','classifier'),
                ('initial','routed','act','bounded_interactive_work','classifier'),
                ('initial','routed','run','explicit_run','classifier'),
                ('initial','routed','run','bulk_collection','classifier'),
                ('initial','routed','run','durable_or_resumable','classifier'),
                ('initial','routed','run','high_fanout','classifier'),
                ('initial','routed','run','approval_or_signal','classifier'),
                ('initial','routed','run','selected_execution_template','selected_execution_template'),
                ('act_escalation','routed','run','act_escalation','act_escalation')
            ) cell(stage,decision,mode,reason,source)
            WHERE moa.execution_route_audit_row_is_valid(
                stage,decision,mode,reason,source,
                CASE WHEN source = 'classifier' THEN 'accepted' ELSE 'not_called' END,
                CASE WHEN source = 'classifier' THEN 'route-model' END,
                CASE WHEN source = 'classifier' THEN 'execution-router-v1' END,
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
             'act_escalation','routed','act','act_escalation','act_escalation',\
             'not_called',NULL,NULL,repeat('a',64),NULL,NULL::SMALLINT,\
             0::SMALLINT,0::BIGINT,0::BIGINT,0::BIGINT,0::BIGINT,0::BIGINT,0::BIGINT)",
        )
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
                        'kind','generated_plan','route_reason','explicit_run',
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
                        'route_reason','selected_execution_template',
                        'skill_template_ref','skill://proof',
                        'skill_template_revision_uid',
                            '00000000-0000-0000-0000-000000337031'
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','experiment_template','route_reason','explicit_run',
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
                        'kind','legacy_migration','route_reason','explicit_run',
                        'legacy',jsonb_build_object(
                            'storage_partition_id',
                                '00000000-0000-0000-0000-000000337020',
                            'user_id',NULL,'artifact_uid',NULL,'revision_uid',NULL,
                            'procedure_ref','skill://legacy',
                            'original_status','completed',
                            'original_idempotency_key',NULL,
                            'migrated_idempotency_key',NULL,
                            'was_nonterminal',false,'error',NULL,'nodes','[]'::JSONB
                        )
                    ),
                    '00000000-0000-0000-0000-000000337020',NULL,
                    '00000000-0000-0000-0000-000000337030',repeat('3',64)
                ),
                moa.execution_source_provenance_is_valid(
                    jsonb_build_object(
                        'kind','generated_plan','route_reason','explicit_run',
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
                        'kind','experiment_template','route_reason','explicit_run',
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
                status,source_kind,route_mode,route_reason
            ) VALUES (
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337010',11,
                '00000000-0000-0000-0000-000000337040',repeat('5',64),
                'owner','{"requirements":[],"completion_checks":[]}',
                '{}','{}',repeat('3',64),repeat('3',64),'{}','{}',
                jsonb_build_object(
                    'kind','generated_plan','route_reason','explicit_run',
                    'planner',jsonb_build_object(
                        'model','m','prompt_version','p',
                        'candidate_hash',repeat('1',64),
                        'compiler_report_hash',repeat('2',64),
                        'final_plan_hash',repeat('3',64),
                        'repair_attempts',0
                    )
                ),
                '{}','queued','generated_plan','run','explicit_run'
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
                    status,source_kind,route_mode,route_reason\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337052',\
                    '00000000-0000-0000-0000-000000337020',NULL,\
                    '00000000-0000-0000-0000-000000337010',12,\
                    '00000000-0000-0000-0000-000000337050',repeat('7',64),\
                    'owner','{\"requirements\":[],\"completion_checks\":[]}',\
                    '{}','{}',repeat('3',64),repeat('3',64),'{}','{}',\
                    jsonb_build_object(\
                        'kind','generated_plan','route_reason','explicit_run',\
                        'planner',jsonb_build_object(\
                            'model','m','prompt_version','p',\
                            'candidate_hash',repeat('1',64),\
                            'compiler_report_hash',repeat('2',64),\
                            'final_plan_hash',repeat('3',64),\
                            'repair_attempts',0\
                        )\
                    ),\
                    '{}','queued','generated_plan','run','explicit_run'\
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
                    upgrade_key,stage,upgrade_version,export_version_floor,\
                    run_high_water_seq,run_high_water_id,\
                    task_high_water_seq,task_high_water_id,\
                    run_page_seq,run_page_id,task_page_seq,task_page_id\
                 ) VALUES (\
                    'execution_dimensions_v2','pending',NOW(),NOW(),\
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
                 WHERE upgrade_key = 'execution_dimensions_v2'",
            )
            .await
            .is_err();
        let backward_page_rejected = target
            .execute(
                "UPDATE analytics.clickhouse_schema_upgrade_state \
                 SET run_page_seq = -1, updated_at = NOW() \
                 WHERE upgrade_key = 'execution_dimensions_v2'",
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
            valid_route_cells,
            invalid_route_cell,
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
        valid_route_cells,
        invalid_route_cell,
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
    assert_eq!(valid_route_cells, 10);
    assert!(!invalid_route_cell);
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

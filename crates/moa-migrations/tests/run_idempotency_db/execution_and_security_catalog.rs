//! Execution schema and final security-catalog scenarios.

use super::support::*;

const LONG_HORIZON_EXECUTION_SQL: &str =
    include_str!("../../migrations/postgres/V000059__long_horizon_execution.sql");

#[test]
fn long_horizon_task_guard_source_is_canonical_offline() {
    // Pins: V59 owns one complete task-update guard instead of editing the
    // inherited function body and layering a second ordinary trigger over it.
    assert_eq!(
        LONG_HORIZON_EXECUTION_SQL
            .matches("CREATE OR REPLACE FUNCTION moa.enforce_execution_task_update()")
            .count(),
        1
    );
    assert!(!LONG_HORIZON_EXECUTION_SQL.contains("$execution_task_long_horizon_transitions$"));
    assert!(!LONG_HORIZON_EXECUTION_SQL.contains("enforce_execution_task_long_horizon_update"));
    for required_clause in [
        "OLD.status = 'running'\n          AND NEW.status = 'ready'",
        "OLD.status = 'waiting_input' AND NEW.status = 'ready'",
        "NEW.attempt_generation <> OLD.attempt_generation + 1",
        "NEW.status <> 'ready'",
        "NEW.attempt_state <> 'idle'",
    ] {
        assert!(
            LONG_HORIZON_EXECUTION_SQL.contains(required_clause),
            "canonical task guard is missing: {required_clause}"
        );
    }
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
                ('retryable'),('invalid_input'),
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
                    'generated_plan'),
                -- The two terminal-vocabulary values V59 retires. Both were accepted by
                -- the V27 baseline, so these cells fail if the V59 patch to
                -- moa.execution_terminal_reason_for stops applying.
                ('blocked',
                    '{"kind":"scheduler_no_progress"}',
                    'generated_plan'),
                ('failed',
                    '{"kind":"task_failure","class":"dependency_failed"}',
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
                admitted_identity,
                status,source_kind
            ) VALUES (
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337010',11,
                '00000000-0000-0000-0000-000000337040',repeat('5',64),
                'owner','{"requirements":[],"completion_checks":[]}',
                jsonb_build_object(
                    'definition',jsonb_build_object(
                        'cancel_policy','retain_effects','input_schema','{}'::JSONB,
                        'output_schema','{}'::JSONB,
                        'nodes','[]'::JSONB
                    ),
                    'plan_hash',repeat('3',64),'catalog_hash',repeat('0',64),
                    'estimate','{}'::JSONB,'report','{}'::JSONB
                ),
                jsonb_build_object(
                    'definition',jsonb_build_object(
                        'cancel_policy','retain_effects','input_schema','{}'::JSONB,
                        'output_schema','{}'::JSONB,
                        'nodes','[]'::JSONB
                    ),
                    'plan_hash',repeat('3',64),'catalog_hash',repeat('0',64),
                    'estimate','{}'::JSONB,'report','{}'::JSONB
                ),repeat('3',64),repeat('3',64),'{}','{}',
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
                '{}',jsonb_build_object(
                    'identity_type','operator',
                    'id','00000000-0000-0000-0000-000000337021',
                    'tenant_id','00000000-0000-0000-0000-000000337020',
                    'api_key_id',NULL,
                    'acting_on_behalf_of',NULL
                ),'queued','generated_plan'
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
                review_uid,tenant_id,contact_id,run_uid,operation_id,owner_kind,generation,
                resolution,traceparent,tracestate,task_traceparent,task_tracestate
            ) VALUES (
                '00000000-0000-0000-0000-000000337043',
                '00000000-0000-0000-0000-000000337020',NULL,
                '00000000-0000-0000-0000-000000337041',
                '00000000-0000-0000-0000-000000337042','task',1,'{}',
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
                    admitted_identity,\
                    status,source_kind\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337052',\
                    '00000000-0000-0000-0000-000000337020',NULL,\
                    '00000000-0000-0000-0000-000000337010',12,\
                    '00000000-0000-0000-0000-000000337050',repeat('7',64),\
                    'owner','{\"requirements\":[],\"completion_checks\":[]}',\
                    jsonb_build_object(\
                        'definition',jsonb_build_object(\
                            'cancel_policy','retain_effects',\
                            'input_schema','{}'::JSONB,'output_schema','{}'::JSONB,\
                            'nodes','[]'::JSONB\
                        ),\
                        'plan_hash',repeat('3',64),'catalog_hash',repeat('0',64),\
                        'estimate','{}'::JSONB,'report','{}'::JSONB\
                    ),\
                    jsonb_build_object(\
                        'definition',jsonb_build_object(\
                            'cancel_policy','retain_effects',\
                            'input_schema','{}'::JSONB,'output_schema','{}'::JSONB,\
                            'nodes','[]'::JSONB\
                        ),\
                        'plan_hash',repeat('3',64),'catalog_hash',repeat('0',64),\
                        'estimate','{}'::JSONB,'report','{}'::JSONB\
                    ),repeat('3',64),repeat('3',64),'{}','{}',\
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
                    '{}',jsonb_build_object(\
                        'identity_type','operator',\
                        'id','00000000-0000-0000-0000-000000337021',\
                        'tenant_id','00000000-0000-0000-0000-000000337020',\
                        'api_key_id',NULL,\
                        'acting_on_behalf_of',NULL\
                    ),'queued','generated_plan'\
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
        let outbox_scope_rejected = postgres_error_fact(target
            .execute(
                "INSERT INTO moa.execution_action_review_outbox (\
                    review_uid,tenant_id,contact_id,run_uid,operation_id,owner_kind,generation,resolution\
                 ) VALUES (\
                    '00000000-0000-0000-0000-000000337054',\
                    '00000000-0000-0000-0000-000000337020',\
                    '00000000-0000-0000-0000-000000337051',\
                    '00000000-0000-0000-0000-000000337041',\
                    '00000000-0000-0000-0000-000000337042','task',1,'{}'\
                 )",
            )
            .await
            .expect_err("a cross-scope action review row must be rejected"));

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
    assert_eq!(valid_terminal_cells, 63);
    assert_eq!(invalid_terminal_cells, 9);
    assert_eq!(provenance_matrix, (true, true, true, false, false, false));
    assert_eq!(json_vectors, (true, false, false, false, true));
    assert_eq!(
        trace_vectors,
        (true, false, false, true, false, false, false)
    );
    assert!(second_run_seq > first_run_seq);
    assert!(planning_context_scope_rejected);
    assert!(task_scope_rejected);
    // Pinned by constraint, not just `is_err`: while the column was still named
    // `task_id` this probe passed on an undefined-column error, so it proved
    // nothing about scoping. Assert the composite
    // (run_uid, tenant_id, contact_scope_id) fence is what rejects the row.
    assert_eq!(
        outbox_scope_rejected,
        (
            Some("23503".to_string()),
            Some("execution_action_review_outbox_run_normalized_scope_fkey".to_string())
        ),
        "a cross-scope action review row must be rejected by the scope fence"
    );
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

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn long_horizon_execution_cutover_rejects_live_runs_and_installs_fenced_catalog_db() {
    // Pins: V59 refuses to reinterpret a live legacy workflow, then preserves
    // terminal evidence while installing the tenant-fenced activation catalog.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect long-horizon migration maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create long-horizon migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "sandbox_workspaces").await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect(&target_url)
            .await?;

        let tenant_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        let planning_context_uid = uuid::Uuid::new_v4();
        let run_uid = uuid::Uuid::new_v4();
        let plan_hash = "1".repeat(64);
        let plan = serde_json::json!({
            "definition": {
                "cancel_policy": "retain_effects",
                "input_schema": {},
                "output_schema": {},
                "nodes": [{
                    "id": "output",
                    "requirement_ids": [],
                    "depends_on": [],
                    "when": null,
                    "input": {},
                    "output_schema": {},
                    "operation": {"kind": "output", "value": {}},
                    "compensation": null,
                    "retry": {
                        "max_attempts": 1,
                        "initial_backoff_ms": 1,
                        "max_backoff_ms": 1
                    },
                    "budget": null
                }]
            },
            "plan_hash": plan_hash,
            "catalog_hash": "0".repeat(64),
            "estimate": {
                "cost_microusd": 0,
                "tokens": 0,
                "tool_calls": 0,
                "retrieved_bytes": 0,
                "tasks": 1
            },
            "report": {"issues": []}
        });
        sqlx::query(
            "INSERT INTO moa.execution_planning_context ( \
                planning_context_uid, tenant_id, session_id, \
                originating_user_sequence_num, originating_user_event_hash, \
                owner_user_id, planning_context_hash, snapshot \
             ) VALUES ($1, $2, $3, 0, $4, 'migration-test', $4, '{}'::JSONB)",
        )
        .bind(planning_context_uid)
        .bind(tenant_id)
        .bind(session_id)
        .bind("2".repeat(64))
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.execution_run ( \
                run_uid, tenant_id, session_id, originating_user_sequence_num, \
                planning_context_uid, planning_context_hash, owner_user_id, goal_contract, \
                initial_plan, active_plan, initial_plan_hash, active_plan_hash, \
                capability_catalog, authorization_envelope, source_provenance, source_kind, \
                input, status \
             ) VALUES ( \
                $1, $2, $3, 0, $4, $5, 'migration-test', $6, $7, $7, $8, $8, \
                $9, $10, $11, 'generated_plan', '{}'::JSONB, 'queued' \
             )",
        )
        .bind(run_uid)
        .bind(tenant_id)
        .bind(session_id)
        .bind(planning_context_uid)
        .bind("2".repeat(64))
        .bind(serde_json::json!({
            "objective": "migration",
            "requirements": [],
            "deliverables": [],
            "coverage": [],
            "constraints": [],
            "completion_checks": []
        }))
        .bind(&plan)
        .bind(&plan_hash)
        .bind(serde_json::json!({
            "capabilities": [],
            "catalog_hash": "0".repeat(64)
        }))
        .bind(serde_json::json!({"capability_refs": [], "skill_refs": []}))
        .bind(serde_json::json!({
            "kind": "generated_plan",
            "planner": {
                "model": "migration-test",
                "prompt_version": "planner",
                "candidate_hash": "3".repeat(64),
                "compiler_report_hash": "4".repeat(64),
                "final_plan_hash": plan_hash,
                "repair_attempts": 0
            }
        }))
        .execute(&target)
        .await?;

        let cutover_error = run_reporting_applied_serialized(&target_url)
            .await
            .expect_err("V59 must reject a nonterminal legacy execution run")
            .to_string();
        let schema_not_partially_installed: bool = sqlx::query_scalar(
            "SELECT to_regclass('moa.execution_trigger') IS NULL \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM information_schema.columns \
                     WHERE table_schema = 'moa' AND table_name = 'execution_run' \
                       AND column_name = 'controller_generation' \
                 )",
        )
        .fetch_one(&target)
        .await?;

        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = 'cancelled', cancellation_reason = 'cutover test', \
                 terminal_cause = '{\"kind\":\"cancellation\"}'::JSONB, \
                 terminal_reason = 'cancelled', \
                 terminal_satisfied_requirement_count = 0, \
                 terminal_requirement_count = 0, completed_at = now() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(&target)
        .await?;

        let applied = run_reporting_applied_serialized(&target_url).await?;
        let second = run_reporting_applied_serialized(&target_url).await?;

        let retry_task_id = uuid::Uuid::new_v4();
        let input_task_id = uuid::Uuid::new_v4();
        let invalid_attempt_task_id = uuid::Uuid::new_v4();
        for (task_id, status, attempt_state) in [
            (retry_task_id, "running", "running"),
            (input_task_id, "waiting_input", "waiting"),
            (invalid_attempt_task_id, "waiting_review", "waiting"),
        ] {
            sqlx::query(
                "INSERT INTO moa.execution_task ( \
                    task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, \
                    input, task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, \
                    estimate_tasks, estimate_tool_calls, estimate_retrieved_bytes, \
                    attempt_state \
                 ) VALUES ( \
                    $1, $2, $3, $4, $4, 1, $5, '{}', \
                    '{\"kind\":\"output\",\"value\":null}', \
                    '{\"max_attempts\":2,\"initial_backoff_ms\":1,\"max_backoff_ms\":1}', \
                    0, 0, 1, 0, 0, $6 \
                 )",
            )
            .bind(task_id)
            .bind(run_uid)
            .bind(tenant_id)
            .bind(format!("counter-guard-{task_id}"))
            .bind(status)
            .bind(attempt_state)
            .execute(&target)
            .await?;
        }

        let retry_counters: (i32, i64, i64) = sqlx::query_as(
            "UPDATE moa.execution_task \
             SET status='ready', attempt_state='idle', attempt=attempt+1, \
                 generation=generation+1, attempt_generation=attempt_generation+1 \
             WHERE task_id=$1 RETURNING attempt, generation, attempt_generation",
        )
        .bind(retry_task_id)
        .fetch_one(&target)
        .await?;
        let input_resume_counters: (i32, i64, i64) = sqlx::query_as(
            "UPDATE moa.execution_task \
             SET status='ready', attempt_state='idle', generation=generation+1, \
                 attempt_generation=attempt_generation+1 \
             WHERE task_id=$1 RETURNING attempt, generation, attempt_generation",
        )
        .bind(input_task_id)
        .fetch_one(&target)
        .await?;
        let invalid_attempt_generation_rejected = sqlx::query(
            "UPDATE moa.execution_task \
             SET status='ready', attempt_state='idle', \
                 attempt_generation=attempt_generation+2 \
             WHERE task_id=$1",
        )
        .bind(invalid_attempt_task_id)
        .execute(&target)
        .await
        .is_err();

        let catalog_shape: (bool, bool, bool, bool, bool, bool, bool) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) = 170
                 FROM information_schema.columns
                 WHERE table_schema = 'moa'
                   AND (
                     (table_name = 'execution_run' AND column_name IN (
                        'admitted_identity', 'controller_generation', 'activation_state',
                        'next_wake_at', 'waiting_since', 'last_progress_at',
                        'budget_deadline_suspended_at',
                        'pause_requested_at', 'paused_at',
                        'activation_failure_count', 'ready_task_count',
                        'active_task_count', 'waiting_task_count',
                        'waiting_input_task_count', 'waiting_input_user_task_count',
                        'waiting_input_tenant_admin_task_count',
                        'waiting_input_external_task_count', 'waiting_review_task_count',
                        'waiting_signal_task_count', 'waiting_timer_task_count',
                        'waiting_external_task_count', 'waiting_replan_task_count',
                        'waiting_reasons_truncated'
                        ,'schedule_uid', 'schedule_incarnation',
                        'schedule_occurrence_sequence', 'terminal_archive_uid',
                        'terminal_archive_hash', 'terminal_details_archived_at'
                     ))
                     OR
                     (table_name = 'execution_planner_call_audit' AND column_name IN (
                        'input_tokens_uncached', 'input_tokens_cache_write',
                        'input_tokens_cache_read', 'output_tokens', 'cost_microusd'
                     ))
                     OR
                     (table_name = 'execution_amendment_planning_reservation' AND column_name IN (
                        'reservation_uid', 'tenant_id', 'contact_id', 'contact_scope_id',
                        'run_uid', 'base_plan_revision', 'call_ordinal',
                        'reserved_cost_microusd', 'reserved_tokens', 'created_at'
                     ))
                     OR
                     (table_name = 'execution_amendment_planning_settlement' AND column_name IN (
                        'settlement_uid', 'reservation_uid', 'tenant_id', 'contact_id',
                        'contact_scope_id', 'run_uid', 'actual_cost_microusd',
                        'actual_tokens', 'budget_overrun', 'settled_at'
                     ))
                     OR
                     (table_name = 'execution_task' AND column_name IN (
                        'attempt_generation', 'attempt_state', 'attempt_started_at',
                        'last_progress_at', 'progress_step_bound_seconds',
                        'attempt_deadline_at', 'waiting_since',
                        'ready_at', 'active_dispatch_uid', 'dispatch_sequence',
                        'external_job_uid', 'failure_fingerprint'
                     ))
                     OR
                     (table_name = 'execution_compensation' AND column_name IN (
                        'attempt_generation', 'attempt_state', 'attempt_started_at',
                        'last_progress_at', 'attempt_deadline_at', 'waiting_since',
                        'active_dispatch_uid', 'dispatch_sequence', 'external_job_uid',
                        'release_intent'
                     ))
                     OR
                     (table_name = 'execution_external_job' AND column_name IN (
                        'compensation_id', 'compensation_generation',
                        'compensation_attempt_generation',
                        'declared_provider', 'provider_contract_violation'
                     ))
                     OR
                     (table_name = 'execution_node_state' AND column_name IN (
                        'aggregate_output', 'aggregate_output_hash', 'reduce_round',
                        'reduce_batch_cursor', 'reduce_round_input_count',
                        'reduce_round_task_count', 'reduce_round_terminal_task_count',
                        'materialization_complete', 'aggregate_cursor_item_key',
                        'aggregate_complete'
                     ))
                     OR
                     (table_name = 'execution_completion_scan' AND column_name IN (
                        'plan_revision', 'controller_generation', 'scan_kind',
                        'excluded_task_id', 'source_progress_at', 'task_cursor',
                        'node_cursor', 'scanned_task_count', 'task_evidence', 'scan_complete',
                        'node_scan_complete', 'completion_evidence',
                        'verifiers_materialized', 'created_at', 'updated_at'
                     ))
                     OR
                     (table_name = 'execution_amendment_receipt' AND column_name IN (
                        'base_plan_revision', 'amendment_hash', 'receipt_kind',
                        'superseded_task_id', 'task_generation',
                        'task_ids_to_release', 'created_at'
                     ))
                     OR
                     (table_name = 'execution_replan_stop_intent' AND column_name IN (
                        'controller_generation', 'wake_epoch', 'origin_task_id',
                        'task_generation', 'base_plan_revision', 'stop_reason',
                        'detail', 'amendment_hash', 'created_at', 'updated_at'
                     ))
                     OR
                     (table_name = 'execution_schedule' AND column_name IN (
                        'template_revision_uid', 'run_as_identity', 'creation_origin',
                        'schedule_incarnation', 'start_at', 'next_occurrence_local'
                     ))
                     OR
                     (table_name = 'execution_trigger' AND column_name = 'schedule_incarnation')
                     OR
                     (table_name = 'execution_capacity_reservation' AND column_name IN (
                        'trigger_uid', 'external_job_uid'
                     ))
                     OR
                     (table_name = 'execution_task_checkpoint' AND column_name IN (
                        'checkpoint_sequence', 'controller_generation',
                        'task_generation', 'attempt_generation', 'dispatch_uid',
                        'checkpoint_kind', 'schema_version', 'payload', 'payload_hash',
                        'workspace_release_receipt', 'superseded_at'
                     ))
                     OR
                     (table_name = 'execution_terminal_archive' AND column_name IN (
                        'format_version', 'terminal_status', 'terminal_completed_at',
                        'goal_hash', 'initial_plan_hash', 'active_plan_hash',
                        'source_record_count', 'source_logical_bytes', 'segment_count',
                        'source_cursor', 'rolling_chain_digest', 'root_digest',
                        'archive_generation', 'finalized_at',
                        'details_deleted_at'
                     ))
                     OR
                     (table_name = 'execution_terminal_archive_segment' AND column_name IN (
                        'archive_uid', 'segment_kind', 'segment_sequence',
                        'format_version', 'record_count', 'payload', 'content_digest'
                     ))
                     OR
                     (table_name = 'execution_maintenance_checkpoint' AND column_name IN (
                        'next_run_at', 'scheduled_generation', 'claim_owner',
                        'claimed_generation', 'claim_expires_at'
                     ))
                   )
                   AND EXISTS (
                     SELECT 1
                     FROM pg_constraint
                     WHERE conrelid = 'moa.execution_task'::REGCLASS
                       AND contype = 'c'
                       AND pg_get_constraintdef(oid)
                           LIKE '%progress_step_bound_seconds IS NULL%progress_step_bound_seconds > 0%'
                   )
                   AND EXISTS (
                     SELECT 1
                     FROM pg_constraint
                     WHERE conrelid = 'moa.execution_run'::REGCLASS
                       AND conname = 'execution_run_pending_terminal_check'
                       AND convalidated
                       AND pg_get_constraintdef(oid) LIKE '%waiting_signal%'
                       AND pg_get_constraintdef(oid) LIKE '%waiting_timer%'
                       AND pg_get_constraintdef(oid) LIKE '%waiting_external%'
                       AND pg_get_constraintdef(oid) LIKE '%pause_requested%'
                       AND pg_get_constraintdef(oid) LIKE '%compensating%'
                   )),
                (SELECT count(*) = 17
                 FROM pg_class relation
                 JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
                 WHERE namespace.nspname = 'moa'
                   AND relation.relkind = 'r'
                   AND relation.relname IN (
                     'execution_node_state', 'execution_trigger',
                     'execution_dispatch_outbox', 'execution_external_job',
                     'execution_capacity_reservation', 'execution_schedule',
                     'execution_capacity_bucket', 'execution_tenant_dispatch_state',
                     'execution_external_job_callback_receipt',
                     'execution_completion_scan',
                     'execution_amendment_receipt',
                     'execution_amendment_planning_reservation',
                     'execution_amendment_planning_settlement',
                     'execution_replan_stop_intent',
                     'execution_task_checkpoint', 'execution_terminal_archive',
                     'execution_terminal_archive_segment'
                   )),
                (SELECT count(*) = 17 AND bool_and(relrowsecurity AND relforcerowsecurity)
                 FROM pg_class relation
                 JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
                 WHERE namespace.nspname = 'moa'
                   AND relation.relname IN (
                     'execution_node_state', 'execution_trigger',
                     'execution_dispatch_outbox', 'execution_external_job',
                     'execution_capacity_reservation', 'execution_schedule',
                     'execution_capacity_bucket', 'execution_tenant_dispatch_state',
                     'execution_external_job_callback_receipt',
                     'execution_completion_scan',
                     'execution_amendment_receipt',
                     'execution_amendment_planning_reservation',
                     'execution_amendment_planning_settlement',
                     'execution_replan_stop_intent',
                     'execution_task_checkpoint', 'execution_terminal_archive',
                     'execution_terminal_archive_segment'
                   )),
                (SELECT count(*) = 49
                 FROM pg_indexes
                 WHERE schemaname = 'moa'
                   AND indexname IN (
                     'execution_run_terminal_retention_idx',
                     'execution_task_ready_idx',
                     'execution_task_tenant_ready_order_idx',
                     'execution_trigger_due_idx',
                     'execution_dispatch_outbox_pending_idx',
                     'execution_dispatch_outbox_task_attempt_uidx',
                     'execution_trigger_schedule_occurrence_uidx',
                     'execution_schedule_due_idx',
                     'execution_task_terminal_retention_idx',
                     'execution_dispatch_outbox_compensation_attempt_uidx',
                     'execution_capacity_bucket_lock_order_idx',
                     'execution_tenant_dispatch_fairness_idx',
                     'execution_dispatch_outbox_claim_expiry_idx'
                     ,'execution_run_schedule_occurrence_uidx'
                     ,'execution_external_job_callback_receipt_retention_idx'
                     ,'execution_dispatch_outbox_dead_letter_idx'
                     ,'execution_dispatch_outbox_task_attempt_cancel_uidx'
                     ,'execution_dispatch_outbox_compensation_attempt_cancel_uidx'
                     ,'execution_task_checkpoint_current_uidx'
                     ,'execution_task_checkpoint_retention_idx'
                     ,'execution_terminal_archive_retention_idx'
                     ,'execution_terminal_archive_segment_scan_idx'
                     ,'execution_terminal_archive_segment_sequence_key'
                     ,'execution_maintenance_checkpoint_due_idx'
                     ,'execution_run_activation_idx'
                     ,'execution_node_state_actionable_idx'
                     ,'execution_node_state_aggregate_actionable_idx'
                     ,'execution_capacity_reservation_active_run_owner_uidx'
                     ,'execution_capacity_reservation_parked_run_owner_uidx'
                     ,'execution_capacity_reservation_trigger_owner_uidx'
                     ,'execution_capacity_reservation_external_job_owner_uidx'
                     ,'execution_completion_scan_actionable_idx'
                     ,'execution_task_failure_fingerprint_idx'
                     ,'execution_amendment_receipt_retention_idx'
                     ,'execution_replan_stop_intent_current_idx'
                     ,'execution_node_state_run_order_uidx'
                     ,'execution_external_job_task_attempt_uidx'
                     ,'execution_external_job_compensation_attempt_uidx'
                     ,'execution_task_waiting_projection_idx'
                     ,'execution_trigger_run_wake_idx'
                     ,'execution_dispatch_outbox_external_cancel_uidx'
                     ,'execution_run_overdue_deadline_idx'
                     ,'execution_task_active_attempt_started_idx'
                     ,'execution_compensation_active_attempt_started_idx'
                     ,'execution_amendment_planning_reservation_pkey'
                     ,'execution_amendment_planning_reservation_logical_key'
                     ,'execution_amendment_planning_reservation_scope_key'
                     ,'execution_amendment_planning_settlement_pkey'
                     ,'execution_amendment_planning_settlement_reservation_uid_key'
                   )),
                (SELECT indexdef LIKE '%budget_deadline_suspended_at IS NULL%'
                 FROM pg_indexes
                 WHERE schemaname = 'moa'
                   AND indexname = 'execution_run_overdue_deadline_idx'),
                moa.execution_admitted_identity_is_valid(admitted_identity, tenant_id)
                    AND activation_state = 'terminal'
                    AND status = 'cancelled',
                (SELECT count(*) = 8
                 FROM pg_constraint
                 WHERE conname IN (
                    'execution_trigger_compensation_tenant_fk',
                    'execution_dispatch_outbox_compensation_tenant_fk',
                    'execution_capacity_reservation_compensation_tenant_fk',
                    'execution_capacity_reservation_trigger_tenant_fk',
                    'execution_capacity_reservation_external_job_tenant_fk',
                    'execution_external_job_compensation_tenant_fk',
                    'execution_compensation_external_job_tenant_fk',
                    'execution_completion_scan_excluded_task_tenant_fk'
                 )
                   AND (
                     (conname IN (
                        'execution_trigger_compensation_tenant_fk',
                        'execution_dispatch_outbox_compensation_tenant_fk',
                        'execution_capacity_reservation_compensation_tenant_fk',
                        'execution_external_job_compensation_tenant_fk'
                      ) AND pg_get_constraintdef(oid)
                            LIKE '%compensation_id, run_uid, tenant_id%')
                     OR
                     (conname = 'execution_capacity_reservation_trigger_tenant_fk'
                      AND pg_get_constraintdef(oid) LIKE '%trigger_uid, tenant_id%')
                     OR
                     (conname = 'execution_capacity_reservation_external_job_tenant_fk'
                      AND pg_get_constraintdef(oid) LIKE '%external_job_uid, tenant_id%')
                     OR
                     (conname = 'execution_compensation_external_job_tenant_fk'
                      AND pg_get_constraintdef(oid) LIKE '%external_job_uid, tenant_id%')
                     OR
                     (conname = 'execution_completion_scan_excluded_task_tenant_fk'
                      AND pg_get_constraintdef(oid)
                            LIKE '%excluded_task_id, run_uid, tenant_id%')
                   ))
                AND
                (SELECT count(*) = 3
                        AND bool_and(privilege_type IN ('SELECT', 'INSERT', 'UPDATE'))
                 FROM information_schema.role_table_grants
                 WHERE table_schema = 'moa'
                   AND table_name = 'execution_maintenance_checkpoint'
                   AND grantee = 'moa_app')
                AND
                (SELECT count(*) = 11
                 FROM pg_constraint
                 WHERE conname IN (
                    'execution_compensation_release_intent_shape_check',
                    'execution_run_waiting_task_counts_check',
                    'execution_run_waiting_input_audience_counts_check',
                    'execution_run_waiting_reasons_bounded_check',
                    'execution_external_job_binding_shape_check',
                    'execution_external_job_contract_violation_shape_check',
                    'execution_amendment_receipt_release_shape_check',
                    'execution_task_output_inline_size_check',
                    'execution_trigger_start_recovery_shape_check',
                    'execution_completion_scan_kind_shape_check',
                    'execution_run_budget_deadline_state_check'
                 ))
                AND
                (SELECT count(*) = 2
                        AND bool_and(convalidated)
                        AND bool_and(
                            pg_get_constraintdef(oid)
                                LIKE '%execution_plan_snapshot_is_current%'
                            AND pg_get_constraintdef(oid) NOT LIKE '%completed%'
                            AND pg_get_constraintdef(oid) NOT LIKE '%cancelled%'
                        )
                 FROM pg_constraint
                 WHERE conrelid = 'moa.execution_run'::regclass
                   AND conname IN (
                       'execution_run_initial_plan_check',
                       'execution_run_active_plan_check'
                   ))
                AND
                (SELECT pg_get_constraintdef(oid) LIKE '%waiting_external%'
                 FROM pg_constraint
                 WHERE conname = 'execution_compensation_attempt_state_check')
                AND
                (SELECT pg_get_constraintdef(oid)
                            LIKE '%capability_external_start%'
                 FROM pg_constraint
                 WHERE conrelid = 'moa.execution_task_checkpoint'::regclass
                   AND conname = 'execution_task_checkpoint_checkpoint_kind_check')
                AND
                (SELECT count(*) = 4
                        AND count(*) FILTER (WHERE cmd = 'ALL') = 1
                        AND count(*) FILTER (WHERE cmd = 'SELECT') = 1
                        AND count(*) FILTER (WHERE cmd = 'INSERT') = 1
                        AND count(*) FILTER (WHERE cmd = 'UPDATE') = 1
                        AND bool_and(
                            policyname = 'execution_capacity_bucket_control_plane'
                            OR COALESCE(qual, with_check, '') LIKE '%scope_kind%fleet%'
                        )
                        AND bool_and(
                            policyname = 'execution_capacity_bucket_control_plane'
                            OR COALESCE(qual, with_check, '') LIKE '%current_tenant_id%'
                        )
                 FROM pg_policies
                 WHERE schemaname = 'moa'
                   AND tablename = 'execution_capacity_bucket')
                AND
                EXISTS (
                    SELECT 1 FROM pg_trigger
                    WHERE tgname = 'execution_capacity_bucket_owner_immutable'
                      AND NOT tgisinternal
                )
                AND
                (SELECT indexdef LIKE 'CREATE UNIQUE INDEX%'
                 FROM pg_indexes
                 WHERE schemaname = 'moa'
                   AND indexname = 'execution_node_state_run_order_uidx')
                AND
                (SELECT indexdef LIKE 'CREATE UNIQUE INDEX%'
                 FROM pg_indexes
                 WHERE schemaname = 'moa'
                   AND indexname = 'execution_terminal_archive_segment_sequence_key')
                AND
                (SELECT indexdef LIKE '%WHERE (provider IS NOT NULL)%'
                 FROM pg_indexes
                 WHERE schemaname = 'moa'
                   AND indexname = 'execution_external_job_provider_identity_key')
                AND
                (SELECT tgdeferrable AND tginitdeferred
                 FROM pg_trigger
                 WHERE tgname = 'execution_external_job_intent_capacity_guard')
                AND
                EXISTS (
                    SELECT 1 FROM pg_trigger
                    WHERE tgname = 'execution_node_aggregate_cursor_update_guard'
                      AND NOT tgisinternal
                )
                AND
                EXISTS (
                    SELECT 1 FROM pg_trigger
                    WHERE tgname = 'execution_replan_stop_intent_immutable_guard'
                      AND NOT tgisinternal
                )
                AND
                EXISTS (
                    SELECT 1 FROM pg_trigger
                    WHERE tgname = 'execution_completion_scan_update_guard'
                      AND NOT tgisinternal
                )
                AND
                EXISTS (
                    SELECT 1 FROM pg_trigger
                    WHERE tgname = 'execution_terminal_archive_segment_mutation_guard'
                      AND NOT tgisinternal
                )
                AND
                (SELECT count(*) = 1
                        AND bool_and(trigger.tgname = 'execution_task_update_guard')
                        AND bool_and(proc.proname = 'enforce_execution_task_update')
                 FROM pg_trigger AS trigger
                 JOIN pg_proc AS proc ON proc.oid = trigger.tgfoid
                 WHERE trigger.tgrelid = 'moa.execution_task'::REGCLASS
                   AND NOT trigger.tgisinternal
                   AND proc.proname LIKE 'enforce_execution_task%')
                AND
                to_regprocedure('moa.enforce_execution_task_long_horizon_update()') IS NULL
                AND
                (SELECT regexp_replace(
                            pg_get_functiondef(
                                'moa.enforce_execution_task_update()'::REGPROCEDURE
                            ),
                            '[[:space:]]+', ' ', 'g'
                        ) LIKE '%OLD.status = ''running'' AND NEW.status = ''ready''%'
                        AND regexp_replace(
                            pg_get_functiondef(
                                'moa.enforce_execution_task_update()'::REGPROCEDURE
                            ),
                            '[[:space:]]+', ' ', 'g'
                        ) LIKE '%OLD.status = ''waiting_input'' AND NEW.status = ''ready''%'
                        AND regexp_replace(
                            pg_get_functiondef(
                                'moa.enforce_execution_task_update()'::REGPROCEDURE
                            ),
                            '[[:space:]]+', ' ', 'g'
                        ) LIKE '%NEW.attempt_generation <> OLD.attempt_generation + 1%')
            FROM moa.execution_run
            WHERE run_uid = $1
            "#,
        )
        .bind(run_uid)
        .fetch_one(&target)
        .await?;

        // A tenant-scoped table that is not registered in moa.tenant_purge_catalog
        // makes run_tenant_purge_batch raise 55000 in its last stage, for every
        // tenant, after rows are already deleted -- so right-to-erasure can never
        // discharge. Reproduce both halves of that gate here: the count constant
        // compiled into the function, and the drift scan it runs.
        let purge_catalog: (i64, Option<i64>, Option<Vec<String>>, bool) = sqlx::query_as(
            r#"
            SELECT
                (SELECT count(*) FROM moa.tenant_purge_catalog),
                (substring(
                    pg_get_functiondef(
                        'moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE
                    )
                    FROM 'catalog_count <> ([0-9]+)'
                ))::BIGINT,
                (SELECT array_agg(
                            format('%I.%I', namespace.nspname, table_row.relname)
                            ORDER BY 1
                        )
                 FROM pg_class AS table_row
                 JOIN pg_namespace AS namespace
                   ON namespace.oid = table_row.relnamespace
                 JOIN pg_attribute AS column_row ON column_row.attrelid = table_row.oid
                 WHERE table_row.relkind IN ('r', 'p')
                   AND NOT table_row.relispartition
                   AND namespace.nspname IN ('public', 'moa', 'analytics', 'pii_vault')
                   AND column_row.attnum > 0
                   AND NOT column_row.attisdropped
                   AND column_row.attname IN ('tenant_id', 'storage_partition_id')
                   AND NOT (
                     namespace.nspname = 'moa'
                     AND table_row.relname IN (
                       'simulator_certification_mandate',
                       'simulator_certification_evidence_import'
                     )
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM moa.tenant_purge_catalog AS catalog
                     WHERE catalog.table_schema = namespace.nspname
                       AND catalog.table_name = table_row.relname
                   )),
                COALESCE(
                    (SELECT count(*) = 4 AND bool_and(
                        parent.stage_order > (
                            SELECT stage_order FROM moa.tenant_purge_catalog
                            WHERE stage_name
                                = 'moa.sandbox_execution_hand_release_receipts'
                        )
                     )
                     FROM moa.tenant_purge_catalog AS parent
                     WHERE parent.stage_name IN (
                       'moa.execution_task', 'moa.execution_compensation',
                       'moa.sandbox_workspaces', 'moa.sandbox_workspace_checkpoints'
                     )),
                    FALSE
                )
            "#,
        )
        .fetch_one(&target)
        .await?;

        sqlx::query(
            "INSERT INTO moa.execution_dispatch_outbox ( \
                dispatch_uid, tenant_id, run_uid, dispatch_kind, \
                controller_generation, wake_epoch \
             ) VALUES ($1, $2, $3, 'run_activation', 1, 1)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(tenant_id)
        .bind(run_uid)
        .execute(&target)
        .await?;
        let duplicate_activation_rejected = sqlx::query(
            "INSERT INTO moa.execution_dispatch_outbox ( \
                dispatch_uid, tenant_id, run_uid, dispatch_kind, \
                controller_generation, wake_epoch \
             ) VALUES ($1, $2, $3, 'run_activation', 1, 1)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(tenant_id)
        .bind(run_uid)
        .execute(&target)
        .await
        .is_err();

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            cutover_error,
            schema_not_partially_installed,
            applied,
            second,
            retry_counters,
            input_resume_counters,
            invalid_attempt_generation_rejected,
            catalog_shape,
            purge_catalog,
            duplicate_activation_rejected,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        cutover_error,
        schema_not_partially_installed,
        applied,
        second,
        retry_counters,
        input_resume_counters,
        invalid_attempt_generation_rejected,
        catalog_shape,
        purge_catalog,
        duplicate_activation_rejected,
    ) = outcome.expect("long-horizon migration assertions should complete");
    assert!(
        cutover_error.contains("legacy execution run(s) are nonterminal"),
        "cutover diagnostic must identify the live-run precondition: {cutover_error}"
    );
    assert!(
        schema_not_partially_installed,
        "the failed migration must leave no partial V59 catalog"
    );
    assert_eq!(
        applied,
        expected_migration_labels_from("long_horizon_execution")
    );
    assert!(second.is_empty(), "V59 must not reapply: {second:?}");
    assert_eq!(retry_counters, (2, 2, 2));
    assert_eq!(input_resume_counters, (1, 2, 2));
    assert!(
        invalid_attempt_generation_rejected,
        "attempt generation may only advance one fence at a time"
    );
    assert_eq!(catalog_shape, (true, true, true, true, true, true, true));
    let (purge_catalog_count, purge_batch_constant, purge_catalog_drift, receipts_drain_first) =
        purge_catalog;
    assert_eq!(
        purge_batch_constant,
        Some(purge_catalog_count),
        "run_tenant_purge_batch's catalog-count constant must equal the catalog it guards"
    );
    assert_eq!(
        purge_catalog_drift, None,
        "every tenant-scoped table must be registered in moa.tenant_purge_catalog"
    );
    assert!(
        receipts_drain_first,
        "sandbox hand-release receipts must purge before every ON DELETE RESTRICT parent"
    );
    assert!(
        duplicate_activation_rejected,
        "one run generation/wake epoch must have exactly one dispatch"
    );
}

/// Seeds one confirmed-shape queued execution run and returns its `run_uid`.
async fn seed_queued_execution_run(
    target: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    session_id: uuid::Uuid,
    planning_context_uid: uuid::Uuid,
) -> Result<uuid::Uuid, Box<dyn std::error::Error + Send + Sync>> {
    let run_uid = uuid::Uuid::new_v4();
    let plan_hash = "1".repeat(64);
    let plan = serde_json::json!({
        "definition": {
            "cancel_policy": "retain_effects",
            "input_schema": {},
            "output_schema": {},
            "nodes": [{
                "id": "output",
                "requirement_ids": [],
                "depends_on": [],
                "when": null,
                "input": {},
                "output_schema": {},
                "operation": {"kind": "output", "value": {}},
                "compensation": null,
                "retry": {
                    "max_attempts": 1,
                    "initial_backoff_ms": 1,
                    "max_backoff_ms": 1
                },
                "budget": null
            }]
        },
        "plan_hash": plan_hash,
        "catalog_hash": "0".repeat(64),
        "estimate": {
            "cost_microusd": 0,
            "tokens": 0,
            "tool_calls": 0,
            "retrieved_bytes": 0,
            "tasks": 1
        },
        "report": {"issues": []}
    });
    sqlx::query(
        "INSERT INTO moa.execution_run ( \
            run_uid, tenant_id, session_id, originating_user_sequence_num, \
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract, \
            initial_plan, active_plan, initial_plan_hash, active_plan_hash, \
            capability_catalog, authorization_envelope, source_provenance, source_kind, \
            input, status, admitted_identity \
         ) VALUES ( \
            $1, $2, $3, 0, $4, $5, 'migration-test', $6, $7, $7, $8, $8, \
            $9, $10, $11, 'generated_plan', '{}'::JSONB, 'queued', $12 \
         )",
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind("2".repeat(64))
    .bind(serde_json::json!({
        "objective": "migration",
        "requirements": [],
        "deliverables": [],
        "coverage": [],
        "constraints": [],
        "completion_checks": []
    }))
    .bind(&plan)
    .bind(&plan_hash)
    .bind(serde_json::json!({
        "capabilities": [],
        "catalog_hash": "0".repeat(64)
    }))
    .bind(serde_json::json!({"capability_refs": [], "skill_refs": []}))
    .bind(serde_json::json!({
        "kind": "generated_plan",
        "planner": {
            "model": "migration-test",
            "prompt_version": "planner",
            "candidate_hash": "3".repeat(64),
            "compiler_report_hash": "4".repeat(64),
            "final_plan_hash": plan_hash,
            "repair_attempts": 0
        }
    }))
    .bind(serde_json::json!({
        "identity_type": "operator",
        "id": uuid::Uuid::new_v4(),
        "tenant_id": tenant_id,
        "api_key_id": null,
        "acting_on_behalf_of": null
    }))
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status = 'pause_requested', \
             activation_state = 'paused', active_task_count = 1, \
             pause_requested_at = now(), updated_at = now() WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status = 'pausing', updated_at = now() \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(target)
    .await?;
    Ok(run_uid)
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn draining_pausing_run_promotes_only_an_unchosen_status_db() {
    // Pins: attempt settlement only adjusts counters, so draining the last active
    // attempt must still complete the pause; but a writer that chooses its own
    // status out of `pausing` keeps it. Swallowing a terminal write into `paused`
    // wedges the run, because `paused` admits only `queued` and `cancelled`.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect pause-promotion maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create pause-promotion database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        install_required_extensions(&target_url).await?;
        run_reporting_applied_serialized(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let tenant_id = uuid::Uuid::new_v4();
        let session_id = uuid::Uuid::new_v4();
        let planning_context_uid = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.execution_planning_context ( \
                planning_context_uid, tenant_id, session_id, \
                originating_user_sequence_num, originating_user_event_hash, \
                owner_user_id, planning_context_hash, snapshot \
             ) VALUES ($1, $2, $3, 0, $4, 'migration-test', $4, '{}'::JSONB)",
        )
        .bind(planning_context_uid)
        .bind(tenant_id)
        .bind(session_id)
        .bind("2".repeat(64))
        .execute(&target)
        .await?;

        let drained =
            seed_queued_execution_run(&target, tenant_id, session_id, planning_context_uid).await?;
        let terminalized =
            seed_queued_execution_run(&target, tenant_id, session_id, planning_context_uid).await?;

        // Attempt settlement writes counters only; the run keeps `pausing`.
        let promoted: (String, String, bool) = sqlx::query_as(
            "UPDATE moa.execution_run SET active_task_count = 0, updated_at = now() \
             WHERE run_uid = $1 \
             RETURNING status, activation_state, paused_at IS NOT NULL",
        )
        .bind(drained)
        .fetch_one(&target)
        .await?;

        // A pending terminal commits its status alongside the same zeroed counter.
        let terminal: (String, i64) = sqlx::query_as(
            "UPDATE moa.execution_run SET status = 'failed', active_task_count = 0, \
                 terminal_reason = 'internal_failure', \
                 terminal_cause = '{\"kind\":\"internal_failure\"}'::JSONB, \
                 terminal_satisfied_requirement_count = 0, \
                 terminal_requirement_count = 0, activation_state = 'terminal', \
                 completed_at = now(), updated_at = now() \
             WHERE run_uid = $1 RETURNING status, active_task_count",
        )
        .bind(terminalized)
        .fetch_one(&target)
        .await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((promoted, terminal))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (promoted, terminal) = outcome.expect("pause promotion assertions should complete");
    assert_eq!(
        promoted,
        ("paused".to_string(), "paused".to_string(), true),
        "draining the last active attempt must complete the pause"
    );
    assert_eq!(
        terminal,
        ("failed".to_string(), 0),
        "a chosen terminal status must survive the pausing promotion"
    );
}

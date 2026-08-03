//! Execution schema and final security-catalog scenarios.

use super::support::*;

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

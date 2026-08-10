//! Migration protocol and pristine-schema regression scenarios.

use super::support::*;

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
    // Pins: a pristine database applies the exact contiguous V1..V58 epoch,
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
        let removed_token_vault_tables_absent: (bool, bool) = sqlx::query_as(
            "SELECT to_regclass('public.token_vault_connections') IS NULL, \
                    to_regclass('public.linked_connections') IS NULL",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            history,
            removed_token_vault_tables_absent,
        ))
    }
    .await;

    // Always prove the throwaway database is disconnected before cleanup.
    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, history, removed_token_vault_tables_absent) =
        outcome.expect("central migration runs should complete on a fresh database");
    let expected_labels = expected_migration_labels();
    assert_eq!(
        expected_labels.len(),
        58,
        "the epoch must contain exactly 58 migrations"
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
        (1..=58).collect::<Vec<_>>(),
        "refinery history must be exactly contiguous from V1 through V58"
    );
    assert!(
        second.is_empty(),
        "second apply must report no newly applied migrations, got {second:?}"
    );
    assert_eq!(removed_token_vault_tables_absent, (true, true));
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
    // semantic migration and becomes a complete V1..V58 history.
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
        partial_error.contains("incomplete: found 28 of 57 expected rows"),
        "complete-history validation must distinguish a valid prefix: {partial_error}"
    );
    assert_eq!(versions, (1..=58).collect::<Vec<_>>());
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

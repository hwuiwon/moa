//! Knowledge graph, connection-link, credential, and ACL schema scenarios.

use super::support::*;

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
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
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

    let outcome = database.finish(outcome).await;

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
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
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

    let outcome = database.finish(outcome).await;

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
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
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

    let outcome = database.finish(outcome).await;

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
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
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

    let outcome = database.finish(outcome).await;

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

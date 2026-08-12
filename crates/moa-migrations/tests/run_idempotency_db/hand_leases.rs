//! Hand-lease effective-profile schema scenarios.

use super::support::*;

/// Seeds a session and its required agent context in one deferred-constraint transaction.
async fn seed_session(
    pool: &sqlx::PgPool,
    session_id: uuid::Uuid,
    tenant_id: uuid::Uuid,
    label: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO public.sessions \
         (id, tenant_id, storage_partition_id, user_id, model) \
         VALUES ($1, $2, $3, $4, 'migration-test-model')",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(label)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO public.session_agent_context (\
             session_id, tenant_id, storage_partition_id, user_id,\
             agent_definition_ref, agent_revision_uid, policy_hash, display_name, policy_snapshot\
         ) VALUES (\
             $1, $2, $3, $4, 'agent://system-default',\
             '00000000-0000-4000-8000-000000000a02',\
             'hand-lease-test-policy', 'Hand Lease Test Agent', '{}'::jsonb\
         )",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(label)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Facts hand-lease effective profile must install on `moa.hand_leases` and `moa.tenant_sandbox_policy`.
struct HandLeaseProfileFacts {
    provisioning_operation_id_is_required: bool,
    provisioning_deadline_is_required: bool,
    has_unique_provisioning_operation_index: bool,
    has_generation_rotation_guard: bool,
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
    rejects_active_row_without_handle: bool,
    rejects_destroyed_row_with_handle: bool,
    rejects_unattached_provisioning_intent: bool,
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
    let provisioning_operation_id_is_required = sqlx::query_scalar::<_, String>(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
           AND column_name = 'provisioning_operation_id'",
    )
    .fetch_one(pool)
    .await?
        == "NO";
    let provisioning_deadline_is_required = sqlx::query_scalar::<_, String>(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
           AND column_name = 'provisioning_deadline_at'",
    )
    .fetch_one(pool)
    .await?
        == "NO";
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
                               'source_route_revision', 'source_origin_revision', \
                               'capability_revision') \
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
    let has_unique_provisioning_operation_index = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
         WHERE schemaname = 'moa' AND tablename = 'hand_leases' \
           AND indexname = 'idx_hand_leases_provisioning_operation' \
           AND indexdef LIKE 'CREATE UNIQUE INDEX%')",
    )
    .fetch_one(pool)
    .await?;
    let has_generation_rotation_guard = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger \
         WHERE tgrelid = 'moa.hand_leases'::regclass \
           AND tgname = 'hand_lease_generation_rotation_guard' \
           AND NOT tgisinternal)",
    )
    .fetch_one(pool)
    .await?;

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
    let accepts_reaping_status = insert_hand_lease(pool, "reaping", false, false, true, true)
        .await
        .is_ok();
    let rejects_reaping_without_claim =
        insert_hand_lease(pool, "reaping", false, false, false, true)
            .await
            .is_err();
    let rejects_active_row_without_policy =
        insert_hand_lease(pool, "active", false, false, false, true)
            .await
            .is_err();
    let rejects_idle_past_hard = insert_hand_lease(pool, "active", true, true, false, true)
        .await
        .is_err();
    let rejects_active_row_without_handle =
        insert_hand_lease(pool, "active", true, false, false, false)
            .await
            .is_err();
    let rejects_destroyed_row_with_handle =
        insert_hand_lease(pool, "destroyed", false, false, false, true)
            .await
            .is_err();
    let rejects_unattached_provisioning_intent =
        insert_hand_lease(pool, "provisioning", true, false, false, false)
            .await
            .is_err();

    Ok(HandLeaseProfileFacts {
        provisioning_operation_id_is_required,
        provisioning_deadline_is_required,
        has_unique_provisioning_operation_index,
        has_generation_rotation_guard,
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
        rejects_active_row_without_handle,
        rejects_destroyed_row_with_handle,
        rejects_unattached_provisioning_intent,
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
    with_handle: bool,
) -> Result<(), sqlx::Error> {
    let session_id = uuid::Uuid::new_v4();
    let tenant_id = uuid::Uuid::new_v4();
    seed_session(
        pool,
        session_id,
        tenant_id,
        &format!("hand-lease-migration-{session_id}"),
    )
    .await?;
    let (idle, hard) = if idle_past_hard {
        ("now() + interval '2 hours'", "now() + interval '1 hour'")
    } else {
        ("now() + interval '1 hour'", "now() + interval '2 hours'")
    };
    let policy_columns = if with_policy || idle_past_hard {
        ", profile, profile_hash, source_deployment_revision, source_tenant_revision, \
         source_agent_revision, source_route_revision, source_origin_revision, capability_revision"
    } else {
        ""
    };
    let policy_values = if with_policy || idle_past_hard {
        ", '{}'::jsonb, 'sha256:test', 'd', 't', 'a', 'r', 'o', 'c'"
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
    let cleanup_columns = if matches!(status, "provisioning" | "failed") {
        ", reap_not_before"
    } else {
        ""
    };
    let cleanup_values = if matches!(status, "provisioning" | "failed") {
        ", $3 + interval '30 seconds'"
    } else {
        ""
    };
    let provisioning_operation_id = uuid::Uuid::new_v4();
    let provisioning_deadline_at = chrono::Utc::now() + chrono::Duration::minutes(5);
    let handle = with_handle.then(|| {
        sqlx::types::Json(serde_json::json!({
            "provisioning_operation_id": provisioning_operation_id,
            "handle": {
                "type": "local",
                "sandbox_dir": "/tmp/moa-migration-hand"
            }
        }))
    });
    sqlx::query(&format!(
        "INSERT INTO moa.hand_leases \
         (session_id, worker_id, tenant_id, provider, tier, status, generation, \
          provisioning_operation_id, provisioning_deadline_at, handle, \
          idle_expires_at, hard_expires_at{policy_columns}{claim_columns}{cleanup_columns}) \
         VALUES ($5, '', $6, 'local', 'local', $1, 1, $2, $3, $4, \
                 {idle}, {hard}{policy_values}{claim_values}{cleanup_values})"
    ))
    .bind(status)
    .bind(provisioning_operation_id)
    .bind(provisioning_deadline_at)
    .bind(handle)
    .bind(session_id)
    .bind(tenant_id)
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
        let facts = hand_lease_profile_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    let outcome = database.finish(outcome).await;

    let (first, second, facts) =
        outcome.expect("hand lease profile migration should apply on a fresh database");

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("hand_lease_effective_profile")),
        "a pristine database must apply hand-lease effective profile, got {first:?}"
    );
    assert!(
        first
            .iter()
            .any(|applied| applied.contains("hand_provisioning_operation_intents")),
        "a pristine database must apply hand provisioning operation intents, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(facts.has_idle_expires_at, "idle_expires_at must exist");
    assert!(
        facts.provisioning_operation_id_is_required,
        "every hand lease must carry a durable provisioning operation identity"
    );
    assert!(
        facts.provisioning_deadline_is_required,
        "every hand lease must carry the absolute provider-create deadline"
    );
    assert!(
        facts.has_unique_provisioning_operation_index,
        "provisioning operation identities must be unique"
    );
    assert!(
        facts.has_generation_rotation_guard,
        "generation rotation must reject writers that retain the old operation identity"
    );
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
            "source_origin_revision".to_string(),
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
        facts.rejects_active_row_without_handle,
        "an active lease without a handle must be rejected"
    );
    assert!(
        facts.rejects_destroyed_row_with_handle,
        "a destroyed lease retaining a handle must be rejected"
    );
    assert!(
        facts.rejects_unattached_provisioning_intent,
        "a provisioning intent must be attached to its durable workspace"
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

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_provisioning_v57_backfills_known_legacy_handles_db() {
    // Pins: a V56 lease with a known handle gains one matching operation ID in
    // both the lease column and handle JSON plus an absolute provisioning
    // deadline, so the new reaper can still destroy the known resource.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "ingest_apply_outcome").await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let session_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO moa.hand_leases \
             (session_id, worker_id, tenant_id, provider, tier, status, generation, handle) \
             VALUES ($1, '', gen_random_uuid(), 'local', 'local', 'stale', 7, $2)",
        )
        .bind(session_id)
        .bind(sqlx::types::Json(serde_json::json!({
            "handle": {
                "type": "local",
                "sandbox_dir": "/tmp/moa-v56-known-hand"
            }
        })))
        .execute(&pool)
        .await?;
        pool.close().await;

        let applied =
            apply_through_migration(&target_url, "hand_provisioning_operation_intents").await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let (operation_id, deadline, handle_operation_id) =
            sqlx::query_as::<_, (uuid::Uuid, chrono::DateTime<chrono::Utc>, String)>(
                "SELECT provisioning_operation_id, provisioning_deadline_at, \
                    handle ->> 'provisioning_operation_id' \
             FROM moa.hand_leases WHERE session_id = $1",
            )
            .bind(session_id)
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            applied,
            operation_id,
            deadline,
            handle_operation_id,
        ))
    }
    .await;
    let outcome = database.finish(outcome).await;
    let (applied, operation_id, deadline, handle_operation_id) =
        outcome.expect("V57 should backfill a known legacy handle");
    assert!(
        applied
            .iter()
            .any(|migration| migration.contains("hand_provisioning_operation_intents")),
        "the targeted V57 apply must install provisioning operation intents: {applied:?}"
    );
    assert_eq!(handle_operation_id, operation_id.to_string());
    assert!(
        deadline <= chrono::Utc::now(),
        "the legacy create is already terminal, so its backfilled deadline must not be future"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_provisioning_v57_rejects_handleless_unresolved_legacy_intent_db() {
    // Pins: V57 cannot invent provider correlation for a V56 create whose
    // handle was never recorded, so it aborts rather than declaring that
    // operation recoverable.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "ingest_apply_outcome").await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query(
            "INSERT INTO moa.hand_leases (\
                 session_id, worker_id, tenant_id, provider, tier, status, generation,\
                 profile, profile_hash, source_deployment_revision, source_tenant_revision,\
                 source_agent_revision, source_route_revision, source_origin_revision,\
                 capability_revision\
             ) VALUES (\
                 gen_random_uuid(), '', gen_random_uuid(), 'e2b', 'microvm', 'provisioning', 1,\
                 '{}'::jsonb, 'sha256:v56', 'd', 't', 'a', 'r', 'o', 'c'\
             )",
        )
        .execute(&pool)
        .await?;
        pool.close().await;

        let error = run_reporting_applied_serialized(&target_url)
            .await
            .expect_err("V57 must reject an unresolved handleless legacy create")
            .to_string();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let deadline_column_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
             WHERE table_schema = 'moa' AND table_name = 'hand_leases' \
               AND column_name = 'provisioning_deadline_at')",
        )
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((error, deadline_column_exists))
    }
    .await;
    let outcome = database.finish(outcome).await;
    let (error, deadline_column_exists) =
        outcome.expect("inspect fail-closed V57 migration outcome");
    assert!(
        error.contains("unresolved legacy provisioning or failed leases lack handles"),
        "migration should name the unrecoverable legacy state: {error}"
    );
    assert!(
        !deadline_column_exists,
        "the failed migration must roll back all V57 schema changes"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_provisioning_v57_rejects_old_writer_generation_rotation_db() {
    // Pins: an already-running V56 writer cannot advance a generation while
    // retaining the backfilled operation identity, which would make an
    // untagged provider resource permanently undiscoverable.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
    let outcome = async {
        let _ = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let session_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        let operation_id = uuid::Uuid::new_v4();
        seed_session(
            &pool,
            session_id,
            tenant_id,
            &format!("hand-generation-{session_id}"),
        )
        .await?;
        sqlx::query(
            "INSERT INTO moa.hand_leases (\
                 session_id, worker_id, tenant_id, provider, tier, status, generation,\
                 provisioning_operation_id, provisioning_deadline_at\
             ) VALUES ($1, '', $3, 'local', 'local', 'destroyed', 4, $2, now())",
        )
        .bind(session_id)
        .bind(operation_id)
        .bind(tenant_id)
        .execute(&pool)
        .await?;
        let error = sqlx::query(
            "UPDATE moa.hand_leases SET generation = generation + 1 WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect_err("V56-style generation rotation must fail before provider I/O");
        let (generation, persisted_operation_id) = sqlx::query_as::<_, (i64, uuid::Uuid)>(
            "SELECT generation, provisioning_operation_id \
                 FROM moa.hand_leases WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            error.to_string(),
            generation,
            persisted_operation_id,
            operation_id,
        ))
    }
    .await;
    let outcome = database.finish(outcome).await;
    let (error, generation, persisted_operation_id, operation_id) =
        outcome.expect("inspect old-writer generation guard");
    assert!(
        error.contains("generation rotation requires a new provisioning operation id"),
        "generation guard should identify the retained operation ID: {error}"
    );
    assert_eq!(generation, 4);
    assert_eq!(persisted_operation_id, operation_id);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_storage_v58_rejects_unresolved_provisioning_before_schema_change_db() {
    // Pins: V58 does not fabricate workspace ownership for an ambiguous
    // provider create. The whole migration rolls back until cleanup resolves
    // the pre-existing provisioning generation.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "hand_provisioning_operation_intents").await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let session_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        seed_session(
            &pool,
            session_id,
            tenant_id,
            &format!("v58-unresolved-{session_id}"),
        )
        .await?;
        let operation_id = uuid::Uuid::new_v4();
        let deadline = chrono::Utc::now() + chrono::Duration::minutes(5);
        sqlx::query(
            "INSERT INTO moa.hand_leases (\
                 session_id, worker_id, tenant_id, provider, tier, status, generation,\
                 provisioning_operation_id, provisioning_deadline_at, reap_not_before,\
                 profile, profile_hash, source_deployment_revision, source_tenant_revision,\
                 source_agent_revision, source_route_revision, source_origin_revision,\
                 capability_revision\
             ) VALUES (\
                 $1, 'worker', $2, 'e2b', 'microvm', 'provisioning', 1, $3, $4,\
                 $4 + interval '30 seconds', '{}'::jsonb, 'sha256:v58',\
                 'd', 't', 'a', 'r', 'o', 'c'\
             )",
        )
        .bind(session_id)
        .bind(tenant_id)
        .bind(operation_id)
        .bind(deadline)
        .execute(&pool)
        .await?;
        pool.close().await;

        let error = run_reporting_applied_serialized(&target_url)
            .await
            .expect_err("V58 must reject an unresolved provisioning generation")
            .to_string();
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let workspace_table_exists = sqlx::query_scalar::<_, bool>(
            "SELECT to_regclass('moa.sandbox_workspaces') IS NOT NULL",
        )
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((error, workspace_table_exists))
    }
    .await;
    let outcome = database.finish(outcome).await;
    let (error, workspace_table_exists) = outcome.expect("inspect fail-closed V58 migration");
    assert!(
        error.contains("unresolved provisioning or failed hand leases"),
        "migration should name the ambiguous provider state: {error}"
    );
    assert!(
        !workspace_table_exists,
        "a failed preflight must roll back every V58 schema change"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn hand_storage_v58_requires_legacy_drain_and_installs_tenant_schema_db() {
    // Pins: a live legacy hand blocks V58. After the hand is proven destroyed,
    // V58 retains it only as an unattached terminal lease, creates no fake
    // workspace, and installs forced RLS plus the composite session tenant FK.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create throwaway migration database");
    let target_url = database.target_url().to_string();
    let outcome = async {
        install_required_extensions(&target_url).await?;
        apply_through_migration(&target_url, "hand_provisioning_operation_intents").await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let session_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        let operation_id = uuid::Uuid::new_v4();
        seed_session(
            &pool,
            session_id,
            tenant_id,
            &format!("v58-active-{session_id}"),
        )
        .await?;
        sqlx::query(
            "INSERT INTO moa.hand_leases (\
                 session_id, worker_id, tenant_id, provider, tier, status, generation,\
                 provisioning_operation_id, provisioning_deadline_at, handle,\
                 idle_expires_at, hard_expires_at, profile, profile_hash,\
                 source_deployment_revision, source_tenant_revision, source_agent_revision,\
                 source_route_revision, source_origin_revision, capability_revision\
             ) VALUES (\
                 $1, 'worker', $2, 'local', 'local', 'active', 1, $3, now(), $4,\
                 now() + interval '1 hour', now() + interval '2 hours',\
                 '{}'::jsonb, 'sha256:v58-active', 'd', 't', 'a', 'r', 'o', 'c'\
             )",
        )
        .bind(session_id)
        .bind(tenant_id)
        .bind(operation_id)
        .bind(sqlx::types::Json(serde_json::json!({
            "provisioning_operation_id": operation_id,
            "handle": {"type": "local", "sandbox_dir": "/tmp/v58-active"}
        })))
        .execute(&pool)
        .await?;
        pool.close().await;

        let first_error = run_reporting_applied_serialized(&target_url)
            .await
            .expect_err("V58 must reject an active legacy hand")
            .to_string();

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        sqlx::query(
            "UPDATE moa.hand_leases \
             SET status = 'destroyed', handle = NULL \
             WHERE session_id = $1 AND tenant_id = $2",
        )
        .bind(session_id)
        .bind(tenant_id)
        .execute(&pool)
        .await?;
        pool.close().await;

        let applied = run_reporting_applied_serialized(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let workspace_count =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.sandbox_workspaces")
                .fetch_one(&pool)
                .await?;
        let attachment = sqlx::query_as::<_, (Option<uuid::Uuid>, Option<i64>, Option<i64>)>(
            "SELECT workspace_id, workspace_writer_epoch, workspace_instance_generation \
             FROM moa.hand_leases WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await?;
        let rls_tables = sqlx::query_as::<_, (String, bool, bool)>(
            "SELECT class.relname, class.relrowsecurity, class.relforcerowsecurity \
             FROM pg_class AS class \
             JOIN pg_namespace AS namespace ON namespace.oid = class.relnamespace \
             WHERE namespace.nspname = 'moa' \
               AND class.relname IN (\
                   'hand_leases', 'sandbox_workspaces', 'sandbox_workspace_operations',\
                   'sandbox_workspace_checkpoints', 'sandbox_workspace_grants',\
                   'sandbox_storage_resources', 'sandbox_capacity_reservations',\
                   'sandbox_execution_hand_release_receipts'\
               ) ORDER BY class.relname",
        )
        .fetch_all(&pool)
        .await?;
        let composite_fk = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint \
             WHERE conrelid = 'moa.hand_leases'::regclass \
               AND conname = 'hand_leases_session_tenant_fk')",
        )
        .fetch_one(&pool)
        .await?;
        let release_receipt_fks = sqlx::query_scalar::<_, bool>(
            "SELECT count(*) = 4 FROM pg_constraint \
             WHERE conrelid = 'moa.sandbox_execution_hand_release_receipts'::regclass \
               AND conname IN (\
                   'sandbox_execution_hand_release_receipts_task_fk',\
                   'sandbox_execution_hand_release_receipts_compensation_fk',\
                   'sandbox_execution_hand_release_receipts_workspace_fk',\
                   'sandbox_execution_hand_release_receipts_checkpoint_fk'\
               )",
        )
        .fetch_one(&pool)
        .await?;
        let release_receipt_retention_guards = sqlx::query_scalar::<_, bool>(
            "SELECT count(*) = 2 FROM pg_trigger AS trigger \
             JOIN pg_proc AS procedure ON procedure.oid = trigger.tgfoid \
             JOIN pg_namespace AS namespace ON namespace.oid = procedure.pronamespace \
             WHERE trigger.tgrelid = 'moa.sandbox_execution_hand_release_receipts'::regclass \
               AND NOT trigger.tgisinternal \
               AND namespace.nspname = 'moa' \
               AND (trigger.tgname, procedure.proname) IN (\
                   ('sandbox_execution_hand_release_receipt_archived_write_guard',\
                    'reject_execution_archived_detail_write'),\
                   ('sandbox_execution_hand_release_receipt_delete_guard',\
                    'reject_execution_immutable_payload')\
               )",
        )
        .fetch_one(&pool)
        .await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first_error,
            applied,
            workspace_count,
            attachment,
            rls_tables,
            composite_fk,
            release_receipt_fks,
            release_receipt_retention_guards,
        ))
    }
    .await;
    let outcome = database.finish(outcome).await;
    let (
        first_error,
        applied,
        workspace_count,
        attachment,
        rls_tables,
        composite_fk,
        release_receipt_fks,
        release_receipt_retention_guards,
    ) = outcome.expect("V58 should apply after legacy compute is drained");
    assert!(
        first_error.contains("legacy hands remain live"),
        "preflight should require an explicit drain: {first_error}"
    );
    assert!(
        applied
            .iter()
            .any(|migration| migration.contains("sandbox_workspaces")),
        "V58 should be the applied migration: {applied:?}"
    );
    assert_eq!(workspace_count, 0, "V58 must not fabricate workspace state");
    assert_eq!(attachment, (None, None, None));
    assert_eq!(rls_tables.len(), 8);
    assert!(
        rls_tables
            .iter()
            .all(|(_, enabled, forced)| *enabled && *forced),
        "every tenant table must force RLS: {rls_tables:?}"
    );
    assert!(
        composite_fk,
        "hand leases must reference session plus tenant"
    );
    assert!(
        release_receipt_fks,
        "task hand release receipts must retain task, workspace, and checkpoint ownership"
    );
    assert!(
        release_receipt_retention_guards,
        "task hand release receipts must reject post-archive writes and fence retention deletes"
    );
}

//! Adversarial tenant-isolation checks for durable hand leases and workspaces.
//!
//! These tests use the real `moa_app` role. The migration owner is a superuser
//! in local development, so queries through that role alone cannot prove that
//! forced RLS protects production request connections.

use std::num::NonZeroU64;
use std::time::Duration;

use moa_core::types::action_policy::CallOrigin;
use moa_core::types::hands::{
    BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, HandHandle, LifetimeLimit,
    MemoryLimit, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
    resolve_effective_sandbox_profile,
};
use moa_core::types::identifiers::{
    ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId, WorkspaceCheckpointId,
};
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use moa_hands::core::leases::{
    HandLeaseActivateRequest, HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseRenewRequest,
    HandLeaseStatus, HandLeaseStore, HandLeaseWorkspaceAttachment, LeaseHandle,
    PostgresHandLeaseStore,
};
use moa_hands::core::reaper::{ExpiredHandLeaseClaims, PostgresExpiredHandLeaseClaims};
use moa_hands::core::sandbox_workspace::repository::PostgresWorkspaceRepository;
use moa_hands::core::sandbox_workspace::storage_resources::PostgresWorkspaceStorageResourceRepository;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::{database_url, seed_session};

fn policy() -> HandLeasePolicy {
    let bounded = |seconds| LifetimeLimit::Bounded {
        seconds: NonZeroU64::new(seconds).expect("test lifetime is nonzero"),
    };
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        bounded(300),
        bounded(3600),
    )
    .expect("test profile is valid");
    let effective = resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("rls-deployment", profile).expect("deployment policy"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "rls-provider-v1",
    )
    .expect("effective policy resolves");
    HandLeasePolicy::from_effective(&effective)
}

async fn seed_workspace(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    session_id: SessionId,
    worker_id: &str,
) -> (HandLeaseWorkspaceAttachment, ProviderAccountId) {
    let provider_account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    sqlx::query(
        "INSERT INTO moa.sandbox_provider_accounts (\
             provider_account_id, provider, isolation_cell, organization_fingerprint\
         ) VALUES ($1, 'local', $2, $3)",
    )
    .bind(provider_account_id)
    .bind(format!("lease-rls-{workspace_id}"))
    .bind(format!("lease-rls-org-{workspace_id}"))
    .execute(pool)
    .await
    .expect("seed provider account");
    sqlx::query(
        "INSERT INTO moa.sandbox_workspaces (\
             workspace_id, tenant_id, scope_kind, scope_session_id, scope_worker_id,\
             provider, provider_account_id, provider_account_generation, durability_class\
         ) VALUES ($1, $2, 'worker', $3, $4, 'local', $5, 1, 'portable_filesystem')",
    )
    .bind(workspace_id)
    .bind(tenant_id)
    .bind(session_id)
    .bind(worker_id)
    .bind(provider_account_id)
    .execute(pool)
    .await
    .expect("seed sandbox workspace");
    (
        HandLeaseWorkspaceAttachment::new(workspace_id, 0, 0, None)
            .expect("seeded attachment validates"),
        provider_account_id,
    )
}

async fn cleanup_workspace(
    pool: &sqlx::PgPool,
    workspace_id: SandboxWorkspaceId,
    provider_account_id: ProviderAccountId,
) {
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await
        .expect("clean up sandbox workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(provider_account_id)
        .execute(pool)
        .await
        .expect("clean up provider account");
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn hand_lease_activation_and_renewal_require_exact_workspace_fences_db() {
    // Pins: provider success cannot activate or renew a Postgres lease after
    // its workspace writer, instance, or restored-revision fence changes.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_id).await;
    let (attachment, provider_account_id) =
        seed_workspace(&pool, tenant_id, session_id, "worker-fenced").await;
    let store = PostgresHandLeaseStore::new(pool.clone());
    let lease_policy = policy();
    let claim = store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: "worker-fenced",
            tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: attachment.clone(),
            policy: &lease_policy,
            caller_deadline: None,
        })
        .await
        .expect("claim fenced lease")
        .expect("claim is owned");
    assert_eq!(claim.attachment, Some(attachment.clone()));
    let stale = HandLeaseWorkspaceAttachment::new(
        attachment.workspace_id,
        attachment.workspace_writer_epoch + 1,
        attachment.workspace_instance_generation,
        attachment.restored_checkpoint_id,
    )
    .expect("stale attachment validates structurally");
    let handle = LeaseHandle::new(
        claim.provisioning_operation_id,
        HandHandle::local(std::path::PathBuf::from("/tmp/moa-lease-fenced-db")),
    );

    assert!(
        !store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "worker-fenced",
                provider: "local",
                generation: claim.generation,
                handle: handle.clone(),
                attachment: stale,
            })
            .await
            .expect("stale activation is a fenced no-op")
    );
    let stale_instance = HandLeaseWorkspaceAttachment::new(
        attachment.workspace_id,
        attachment.workspace_writer_epoch,
        attachment.workspace_instance_generation + 1,
        attachment.restored_checkpoint_id,
    )
    .expect("stale instance attachment validates structurally");
    assert!(
        !store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "worker-fenced",
                provider: "local",
                generation: claim.generation,
                handle: handle.clone(),
                attachment: stale_instance,
            })
            .await
            .expect("stale instance activation is a fenced no-op")
    );
    assert!(
        store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "worker-fenced",
                provider: "local",
                generation: claim.generation,
                handle,
                attachment: attachment.clone(),
            })
            .await
            .expect("exact activation succeeds")
    );

    let wrong_revision = HandLeaseWorkspaceAttachment::new(
        attachment.workspace_id,
        attachment.workspace_writer_epoch,
        attachment.workspace_instance_generation,
        Some(WorkspaceCheckpointId::new()),
    )
    .expect("wrong revision attachment validates structurally");
    assert!(
        !store
            .renew_active(HandLeaseRenewRequest {
                tenant_id,
                session_id,
                worker_id: "worker-fenced",
                provider: "local",
                generation: claim.generation,
                provisioning_operation_id: claim.provisioning_operation_id,
                attachment: wrong_revision,
                idle_expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            })
            .await
            .expect("wrong revision renewal is a fenced no-op")
    );
    assert!(
        !store
            .renew_active(HandLeaseRenewRequest {
                tenant_id,
                session_id,
                worker_id: "worker-fenced",
                provider: "local",
                generation: claim.generation + 1,
                provisioning_operation_id: claim.provisioning_operation_id,
                attachment: attachment.clone(),
                idle_expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            })
            .await
            .expect("stale lease-generation renewal is a fenced no-op")
    );
    let loaded = store
        .get(tenant_id, session_id, "worker-fenced", "local")
        .await
        .expect("load exact active lease")
        .expect("active lease exists");
    assert_eq!(loaded.status, HandLeaseStatus::Active);
    assert_eq!(loaded.attachment, Some(attachment.clone()));

    let partial = sqlx::query(
        "UPDATE moa.hand_leases SET workspace_writer_epoch = NULL WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(&pool)
    .await;
    assert!(
        partial.is_err(),
        "the database must reject a partial active attachment"
    );

    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean up lease");
    cleanup_workspace(&pool, attachment.workspace_id, provider_account_id).await;
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean up session");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn opaque_provider_reference_collision_stays_account_and_tenant_scoped_db() {
    // Pins: equal opaque provider IDs in two isolation cells do not become an
    // authorization token, and a rotated account generation cannot resolve by
    // its old generation.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    seed_session(&pool, session_a, tenant_a).await;
    seed_session(&pool, session_b, tenant_b).await;
    let (attachment_a, account_a) = seed_workspace(&pool, tenant_a, session_a, "worker-a").await;
    let (attachment_b, account_b) = seed_workspace(&pool, tenant_b, session_b, "worker-b").await;
    let operation_a = uuid::Uuid::now_v7();
    let operation_b = uuid::Uuid::now_v7();
    let resource_a = uuid::Uuid::now_v7();
    let resource_b = uuid::Uuid::now_v7();
    let shared_opaque_reference = format!("provider-volume-{}", Uuid::new_v4());

    for (operation_id, tenant_id, workspace_id, account_id) in [
        (operation_a, tenant_a, attachment_a.workspace_id, account_a),
        (operation_b, tenant_b, attachment_b.workspace_id, account_b),
    ] {
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_workspace_operations (
                operation_id, tenant_id, workspace_id, provider_account_id,
                provider_account_generation, operation_kind, request_hash,
                expected_writer_epoch, expected_instance_generation,
                expected_checkpoint_generation, deadline_at, reconcile_not_before,
                outcome_class, confirmed_disposition
            ) VALUES (
                $1, $2, $3, $4, 1, 'create', $5,
                0, 0, 0, now() + interval '1 minute', now() + interval '2 minutes',
                'confirmed', 'resource_present'
            )
            "#,
        )
        .bind(operation_id)
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(account_id)
        .bind(format!("sha256:opaque-collision-{operation_id}"))
        .execute(&pool)
        .await
        .expect("seed confirmed storage create operation");
    }
    for (resource_id, tenant_id, account_id, operation_id) in [
        (resource_a, tenant_a, account_a, operation_a),
        (resource_b, tenant_b, account_b, operation_b),
    ] {
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_storage_resources (
                storage_resource_id, tenant_id, provider_account_id,
                provider_account_generation, resource_kind, security_class,
                deterministic_name, provider_reference, lifecycle_state,
                generation, create_operation_id, verified_owner_fingerprint
            ) VALUES ($1, $2, $3, 1, 'volume', 'tenant-isolated', $4, $5,
                      'ready', 1, $6, $7)
            "#,
        )
        .bind(resource_id)
        .bind(tenant_id)
        .bind(account_id)
        .bind(format!("moa-volume-{resource_id}"))
        .bind(&shared_opaque_reference)
        .bind(operation_id)
        .bind(format!("owner-{resource_id}"))
        .execute(&pool)
        .await
        .expect("seed account-scoped provider reference collision");
    }

    let resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());
    let own_a = resources
        .by_provider_reference(tenant_a, account_a, 1, &shared_opaque_reference)
        .await
        .expect("tenant A exact lookup succeeds")
        .expect("tenant A resource exists");
    let own_b = resources
        .by_provider_reference(tenant_b, account_b, 1, &shared_opaque_reference)
        .await
        .expect("tenant B exact lookup succeeds")
        .expect("tenant B resource exists");
    assert_eq!(own_a.storage_resource_id, resource_a);
    assert_eq!(own_b.storage_resource_id, resource_b);
    assert_eq!(own_a.provider_reference, own_b.provider_reference);
    assert!(
        resources
            .by_provider_reference(tenant_a, account_b, 1, &shared_opaque_reference)
            .await
            .expect("cross-account lookup is a filtered miss")
            .is_none()
    );
    assert!(
        resources
            .by_provider_reference(tenant_b, account_a, 1, &shared_opaque_reference)
            .await
            .expect("cross-tenant lookup is a filtered miss")
            .is_none()
    );

    let rotated_account = ProviderAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint
        ) VALUES ($1, 2, 'local', $2, $3)
        "#,
    )
    .bind(rotated_account)
    .bind(format!("rotated-cell-{rotated_account}"))
    .bind(format!("rotated-org-{rotated_account}"))
    .execute(&pool)
    .await
    .expect("seed explicitly rotated provider account");
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    assert!(
        workspaces
            .resolve_provider_account(rotated_account, 1)
            .await
            .expect("stale account generation lookup is safe")
            .is_none(),
        "an old account generation must not resolve after rotation"
    );
    assert_eq!(
        workspaces
            .resolve_provider_account(rotated_account, 2)
            .await
            .expect("current account generation lookup succeeds")
            .expect("current rotated account exists")
            .generation,
        2
    );

    sqlx::query("DELETE FROM moa.sandbox_storage_resources WHERE storage_resource_id = ANY($1)")
        .bind(vec![resource_a, resource_b])
        .execute(&pool)
        .await
        .expect("clean storage resources");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = ANY($1)")
        .bind(vec![operation_a, operation_b])
        .execute(&pool)
        .await
        .expect("clean workspace operations");
    cleanup_workspace(&pool, attachment_a.workspace_id, account_a).await;
    cleanup_workspace(&pool, attachment_b.workspace_id, account_b).await;
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(rotated_account)
        .execute(&pool)
        .await
        .expect("clean rotated provider account");
    sqlx::query("DELETE FROM public.sessions WHERE id = ANY($1)")
        .bind(vec![session_a, session_b])
        .execute(&pool)
        .await
        .expect("clean sessions");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn hand_leases_force_rls_and_reject_missing_or_cross_tenant_scope_db() {
    // Pins: an absent tenant GUC and tenant B both observe zero tenant A rows,
    // and neither can mutate the row through the production application role.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_a).await;
    let (attachment, provider_account_id) =
        seed_workspace(&pool, tenant_a, session_id, "worker-a").await;

    let store = PostgresHandLeaseStore::new(pool.clone());
    let policy = policy();
    let claim = store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: "worker-a",
            tenant_id: tenant_a,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: attachment.clone(),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("tenant A provisioning claim")
        .expect("tenant A owns the claim");

    let (rls_enabled, rls_forced) = sqlx::query_as::<_, (bool, bool)>(
        "SELECT relrowsecurity, relforcerowsecurity \
         FROM pg_class WHERE oid = 'moa.hand_leases'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect hand lease RLS flags");
    assert_eq!((rls_enabled, rls_forced), (true, true));

    let mut unscoped = pool.begin().await.expect("begin unscoped transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *unscoped)
        .await
        .expect("assume application role without a tenant");
    let missing_scope_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.hand_leases WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&mut *unscoped)
            .await
            .expect("unscoped request query is filtered");
    let missing_scope_updates =
        sqlx::query("UPDATE moa.hand_leases SET updated_at = now() WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *unscoped)
            .await
            .expect("unscoped request update is filtered")
            .rows_affected();
    unscoped.rollback().await.expect("rollback unscoped check");
    assert_eq!(missing_scope_count, 0);
    assert_eq!(missing_scope_updates, 0);

    let mut tenant_b_conn = ScopedConn::begin_as_app(&pool, &RlsContext::tenant(tenant_b), true)
        .await
        .expect("begin tenant B application scope");
    let tenant_b_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.hand_leases WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(tenant_b_conn.as_mut())
            .await
            .expect("tenant B query is filtered");
    let tenant_b_updates =
        sqlx::query("UPDATE moa.hand_leases SET updated_at = now() WHERE session_id = $1")
            .bind(session_id)
            .execute(tenant_b_conn.as_mut())
            .await
            .expect("tenant B update is filtered")
            .rows_affected();
    tenant_b_conn
        .rollback()
        .await
        .expect("rollback tenant B check");
    assert_eq!(tenant_b_count, 0);
    assert_eq!(tenant_b_updates, 0);

    assert!(
        store
            .get(tenant_b, session_id, "worker-a", "local")
            .await
            .expect("cross-tenant load is a filtered read")
            .is_none(),
        "tenant B must not load tenant A's lease"
    );
    let cross_tenant_claim = store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: "worker-a",
            tenant_id: tenant_b,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: attachment.clone(),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("cross-tenant RLS conflict is reported as an unowned claim");
    assert!(
        cross_tenant_claim.is_none(),
        "tenant B must not acquire tenant A's lease"
    );

    let persisted_tenant = sqlx::query_scalar::<_, TenantId>(
        "SELECT tenant_id FROM moa.hand_leases WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("migration owner verifies immutable tenant");
    assert_eq!(persisted_tenant, tenant_a);
    assert_eq!(claim.tenant_id, tenant_a);

    let has_composite_session_fk = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (\
             SELECT 1 FROM pg_constraint \
             WHERE conrelid = 'moa.hand_leases'::regclass \
               AND conname = 'hand_leases_session_tenant_fk' \
               AND contype = 'f'\
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect hand lease composite foreign key");
    assert!(has_composite_session_fk);

    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean up lease");
    cleanup_workspace(&pool, attachment.workspace_id, provider_account_id).await;
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean up session");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn maintenance_reaper_crosses_tenants_without_exposing_a_foreground_bypass_db() {
    // Pins: the fleet reaper's dedicated control-plane path can claim expired
    // generations across tenants, while the foreground store remains tenant
    // scoped and cannot list its sibling tenant.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let store = PostgresHandLeaseStore::new(pool.clone());
    let policy = policy();
    let tenants = [TenantId::new(), TenantId::new()];
    let sessions = [SessionId::new(), SessionId::new()];
    let mut seeded_workspaces = Vec::new();

    for (tenant_id, session_id) in tenants.into_iter().zip(sessions) {
        seed_session(&pool, session_id, tenant_id).await;
        let (attachment, provider_account_id) =
            seed_workspace(&pool, tenant_id, session_id, "worker").await;
        seeded_workspaces.push((attachment.clone(), provider_account_id));
        let claim = store
            .claim_for_provisioning(HandLeaseProvisionRequest {
                session_id,
                worker_id: "worker",
                tenant_id,
                provider: "local",
                tier: SandboxTier::Local,
                attachment: attachment.clone(),
                policy: &policy,
                caller_deadline: None,
            })
            .await
            .expect("claim lease")
            .expect("claim is owned");
        store
            .activate(HandLeaseActivateRequest {
                tenant_id,
                session_id,
                worker_id: "worker",
                provider: "local",
                generation: claim.generation,
                handle: LeaseHandle::new(
                    claim.provisioning_operation_id,
                    HandHandle::local(std::path::PathBuf::from(format!(
                        "/tmp/moa-rls-{session_id}"
                    ))),
                ),
                attachment,
            })
            .await
            .expect("activate lease");
    }

    sqlx::query(
        "UPDATE moa.hand_leases \
         SET idle_expires_at = now() - interval '1 minute', \
             hard_expires_at = now() - interval '1 minute' \
         WHERE session_id = ANY($1)",
    )
    .bind(sessions.to_vec())
    .execute(&pool)
    .await
    .expect("expire both tenant leases");

    let tenant_a_rows = store
        .list_live_session_page(tenants[0], sessions[0], None)
        .await
        .expect("tenant A lists its own session");
    let tenant_a_cross_rows = store
        .list_live_session_page(tenants[0], sessions[1], None)
        .await
        .expect("tenant A cross-session query is filtered");
    assert_eq!(tenant_a_rows.leases.len(), 1);
    assert!(tenant_a_cross_rows.leases.is_empty());

    let claims = PostgresExpiredHandLeaseClaims::new(pool.clone())
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("maintenance sweep claims expired leases");
    let ours = claims
        .iter()
        .filter(|claim| sessions.contains(&claim.session_id))
        .map(|claim| (claim.tenant_id, claim.session_id))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        ours,
        tenants.into_iter().zip(sessions).collect(),
        "maintenance path must retain exact tenant identity for every claim"
    );

    for claim in claims
        .iter()
        .filter(|claim| sessions.contains(&claim.session_id))
    {
        assert!(
            PostgresExpiredHandLeaseClaims::new(pool.clone())
                .finalize_destroyed(claim)
                .await
                .expect("finalize maintenance claim")
        );
    }
    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = ANY($1)")
        .bind(sessions.to_vec())
        .execute(&pool)
        .await
        .expect("clean up leases");
    for (attachment, provider_account_id) in seeded_workspaces {
        cleanup_workspace(&pool, attachment.workspace_id, provider_account_id).await;
    }
    sqlx::query("DELETE FROM public.sessions WHERE id = ANY($1)")
        .bind(sessions.to_vec())
        .execute(&pool)
        .await
        .expect("clean up sessions");
    pool.close().await;
}

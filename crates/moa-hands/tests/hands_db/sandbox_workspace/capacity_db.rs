//! Atomic sandbox-workspace capacity admission against Postgres.

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::MoaError,
    types::{
        action_policy::CallOrigin,
        hands::{
            BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit,
            SandboxPolicySnapshot, SandboxProfile, SandboxTier,
        },
        identifiers::{
            ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
            SessionId, TenantId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, SandboxWorkspaceScope, SandboxWorkspaceState,
            WorkspaceCapacityDimension, WorkspaceOperationKind,
        },
    },
};
use moa_hands::core::{
    leases::{
        HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStore, HandLeaseWorkspaceAttachment,
        PostgresHandLeaseStore,
    },
    sandbox_workspace::{
        capacity::{
            ActiveHandCapacityRequest, CapacityQuantity, CapacityReservationRequest,
            PostgresWorkspaceCapacityRepository,
        },
        model::{CreateWorkspaceRequest, WorkspaceTransition, WorkspaceWriterClaim},
        operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
        repository::PostgresWorkspaceRepository,
        storage_resources::{
            PostgresWorkspaceStorageResourceRepository, StorageResourceCreateIntent,
        },
    },
};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::{database_url, seed_session};

#[derive(Debug, Clone)]
struct VolumeCandidate {
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    operation_id: WorkspaceOperationId,
    storage_resource_id: Uuid,
    request: CapacityReservationRequest,
}

async fn seed_volume_candidate(
    pool: &sqlx::PgPool,
    account_id: ProviderAccountId,
    ordinal: &str,
) -> VolumeCandidate {
    let tenant_id = TenantId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let operation_id = WorkspaceOperationId::new();
    let storage_resource_id = Uuid::now_v7();
    PostgresWorkspaceRepository::new(pool.clone())
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: SandboxWorkspaceScope::ExecutionTask {
                run_id: ExecutionRunScopeId::new(),
                task_id: ExecutionTaskScopeId::new(),
            },
            provider: "daytona".to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("persist capacity-test workspace");
    let now = Utc::now();
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: format!("sha256:volume-capacity-{ordinal}"),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(10),
            reconcile_not_before: now + ChronoDuration::seconds(20),
        })
        .await
        .expect("persist capacity-test create operation");
    assert!(
        operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("fence capacity-test provider attempt")
    );
    PostgresWorkspaceStorageResourceRepository::new(pool.clone())
        .persist_create_intent(&StorageResourceCreateIntent {
            storage_resource_id,
            tenant_id,
            workspace_id,
            create_operation_id: operation_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            security_class: "tenant-isolated".to_string(),
            deterministic_name: format!("moa-capacity-{storage_resource_id}"),
            verified_owner_fingerprint: format!("owner-{tenant_id}"),
        })
        .await
        .expect("persist capacity-test storage create intent");
    VolumeCandidate {
        tenant_id,
        workspace_id,
        operation_id,
        storage_resource_id,
        request: CapacityReservationRequest {
            tenant_id,
            workspace_id,
            operation_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            quantities: vec![CapacityQuantity {
                dimension: WorkspaceCapacityDimension::Volumes,
                quantity: 1,
            }],
        },
    }
}

async fn cleanup_volume_account(pool: &sqlx::PgPool, account_id: ProviderAccountId) {
    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean provider capacity reservations");
    sqlx::query("DELETE FROM moa.sandbox_storage_resources WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean provider storage resources");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean provider workspace operations");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean provider workspaces");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean provider account");
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn exact_capacity_succeeds_and_exact_limit_plus_one_is_deterministic_db() {
    // Pins: workspace creation atomically consumes the exact tenant/provider
    // limit, and only finalized deletion at the exact generation releases it.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'local', $2, $3, '{"workspaces": 1}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("capacity-{account_id}"))
    .bind(format!("org-{account_id}"))
    .execute(&pool)
    .await
    .expect("seed provider capacity");
    sqlx::query(
        "INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits) VALUES ($1, '{\"workspaces\": 1}'::jsonb)",
    )
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed tenant capacity");

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let create = |workspace_id| CreateWorkspaceRequest {
        workspace_id,
        tenant_id,
        scope: SandboxWorkspaceScope::ExecutionTask {
            run_id: ExecutionRunScopeId::new(),
            task_id: ExecutionTaskScopeId::new(),
        },
        provider: "local".to_string(),
        provider_account_id: account_id,
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        retention_deadline_at: None,
    };
    let first_workspace_id = SandboxWorkspaceId::new();
    workspaces
        .create(&create(first_workspace_id))
        .await
        .expect("workspace creation atomically reserves the exact capacity");
    let second_workspace_id = SandboxWorkspaceId::new();
    let error = workspaces
        .create(&create(second_workspace_id))
        .await
        .expect_err("limit plus one must roll back workspace creation");
    assert!(
        matches!(error, MoaError::StorageError(ref detail) if detail.contains("tenant workspaces capacity exceeded")),
        "workspace admission must report the exact exhausted dimension: {error}"
    );
    let reservation_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1 AND resource_dimension = 'workspaces' AND reservation_state = 'committed'",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("count atomic reservations");
    assert_eq!(
        reservation_count, 1,
        "a rejected batch leaves no partial row"
    );
    let workspace_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.sandbox_workspaces WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("count atomically admitted workspaces");
    assert_eq!(
        workspace_count, 1,
        "capacity failure rolls back workspace metadata"
    );
    assert_ne!(first_workspace_id, second_workspace_id);

    assert!(
        workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id: first_workspace_id,
                from: SandboxWorkspaceState::Creating,
                to: SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("make admitted workspace ready for deletion")
    );
    assert!(
        workspaces
            .fence_for_deletion(tenant_id, first_workspace_id, 0, 0)
            .await
            .expect("fence exact workspace generation for deletion")
    );
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    assert!(
        !capacity
            .release_workspace(tenant_id, first_workspace_id, 1)
            .await
            .expect("deleting workspace cannot release capacity before finalized absence")
    );

    let delete_operation_id = WorkspaceOperationId::new();
    let now = Utc::now();
    PostgresWorkspaceOperationRepository::new(pool.clone())
        .persist_intent(&WorkspaceOperationIntent {
            operation_id: delete_operation_id,
            tenant_id,
            workspace_id: first_workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Delete,
            request_hash: format!("sha256:workspace-delete-{delete_operation_id}"),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(10),
            reconcile_not_before: now + ChronoDuration::seconds(20),
        })
        .await
        .expect("persist exact delete operation");
    sqlx::query(
        r#"
        UPDATE moa.sandbox_workspace_operations
        SET outcome_class = 'confirmed', confirmed_disposition = 'resource_absent',
            absence_observation_count = 2,
            absence_first_observed_at = now() - interval '2 seconds',
            absence_last_observed_at = now(),
            absence_inventory_digest = 'sha256:capacity-delete-absence'
        WHERE operation_id = $1
        "#,
    )
    .bind(delete_operation_id)
    .execute(&pool)
    .await
    .expect("record verified provider absence for deletion");
    assert!(
        workspaces
            .finalize_deleted(tenant_id, first_workspace_id, 1, delete_operation_id)
            .await
            .expect("finalize exact deleted workspace")
    );
    assert!(
        !capacity
            .release_workspace(tenant_id, first_workspace_id, 2)
            .await
            .expect("future delete generation is a fenced miss")
    );
    assert!(
        !capacity
            .release_workspace(tenant_id, first_workspace_id, 1)
            .await
            .expect("finalize atomically released the exact workspace owner")
    );
    let released_state = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE workspace_id = $1 AND resource_dimension = 'workspaces'",
    )
    .bind(first_workspace_id)
    .fetch_one(&pool)
    .await
    .expect("load released lifetime workspace reservation");
    assert_eq!(released_state, "released");

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean reservations");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean workspace operations");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean workspaces");
    sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean tenant limits");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn volume_inventory_overlap_headroom_and_exact_limit_are_atomic_db() {
    // Pins: one volume seen in both durable state and Daytona inventory counts
    // once, while provider-only inventory and reserved headroom still make the
    // exact ceiling pass and ceiling plus one fail before reservation insert.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let account_id = ProviderAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits, admission_headroom
        ) VALUES (
            $1, 1, 'daytona', $2, $3,
            '{"volumes": 4}'::jsonb, '{"volumes": 2}'::jsonb
        )
        "#,
    )
    .bind(account_id)
    .bind(format!("volume-capacity-{account_id}"))
    .bind(format!("org-{account_id}"))
    .execute(&pool)
    .await
    .expect("seed Daytona volume ceiling and headroom");

    let resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let first = seed_volume_candidate(&pool, account_id, "first").await;
    assert!(
        resources
            .confirm_created(
                first.tenant_id,
                first.storage_resource_id,
                1,
                first.operation_id,
                "daytona-volume-overlap",
            )
            .await
            .expect("confirm first durable provider volume")
    );

    let observed = vec![
        "daytona-volume-overlap".to_string(),
        "daytona-provider-only".to_string(),
    ];
    let admitted = capacity
        .reserve_lifetime_volume(&first.request, first.storage_resource_id, 4, 2, &observed)
        .await
        .expect("overlapping durable/provider identity should count once");
    assert_eq!(admitted.dimension, WorkspaceCapacityDimension::Volumes);
    assert_eq!(admitted.quantity, 1);
    assert!(
        capacity
            .commit_lifetime_volume(&first.request, first.storage_resource_id)
            .await
            .expect("commit first linked lifetime reservation")
    );

    let second = seed_volume_candidate(&pool, account_id, "second").await;
    let error = capacity
        .reserve_lifetime_volume(&second.request, second.storage_resource_id, 4, 2, &observed)
        .await
        .expect_err("one more create intent must exceed ceiling plus headroom");
    assert!(
        matches!(
            error,
            MoaError::ProviderError(ref detail)
                if detail == "Daytona volume capacity exhausted: effective=3, headroom=2, ceiling=4"
        ),
        "limit plus one must report the exact effective inventory boundary: {error}"
    );
    let reservations = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.sandbox_capacity_reservations WHERE provider_account_id = $1 AND resource_dimension = 'volumes'",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .expect("count atomic provider volume reservations");
    assert_eq!(
        reservations, 1,
        "rejected admission cannot leave a second partial reservation"
    );
    let second_reservation = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.sandbox_capacity_reservations WHERE operation_id = $1)",
    )
    .bind(second.operation_id)
    .fetch_one(&pool)
    .await
    .expect("inspect rejected operation reservation");
    assert!(!second_reservation);
    assert_ne!(first.workspace_id, second.workspace_id);

    cleanup_volume_account(&pool, account_id).await;
    pool.close().await;
}

fn active_hand_lease_policy() -> HandLeasePolicy {
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        LifetimeLimit::Unbounded,
        LifetimeLimit::Unbounded,
    )
    .expect("test profile validates");
    HandLeasePolicy::from_effective(
        &moa_core::types::hands::resolve_effective_sandbox_profile(
            &SandboxPolicySnapshot::new("workspace-capacity-deployment", profile)
                .expect("deployment snapshot"),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
            &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
            &SandboxPolicySnapshot::origin(CallOrigin::Production),
            "workspace-capacity-capabilities-v1",
        )
        .expect("test resolution succeeds"),
    )
}

/// Drives one workspace to a hydrating writer with a provisioning lease, which
/// is the exact state `reserve_active_hand` admits from.
async fn seed_hydrating_hand(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    account_id: ProviderAccountId,
    worker_id: &str,
) -> ActiveHandCapacityRequest {
    let session_id = SessionId::new();
    seed_session(pool, session_id, tenant_id).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let workspace_id = SandboxWorkspaceId::new();
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: SandboxWorkspaceScope::ExecutionTask {
                run_id: ExecutionRunScopeId::new(),
                task_id: ExecutionTaskScopeId::new(),
            },
            provider: "local".to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("persist active-hand test workspace");
    assert!(
        workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Creating,
                to: SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("make the workspace claimable")
    );
    let restoring = workspaces
        .claim_writer(WorkspaceWriterClaim {
            tenant_id,
            workspace_id,
            expected_state: SandboxWorkspaceState::Ready,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
        })
        .await
        .expect("claim the single writer")
        .expect("claimed workspace exists");
    let attachment = HandLeaseWorkspaceAttachment::new(
        restoring.workspace_id,
        restoring.writer_epoch,
        restoring.instance_generation,
        None,
    )
    .expect("claimed workspace attachment validates");
    let provisioning = PostgresHandLeaseStore::new(pool.clone())
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment,
            policy: &active_hand_lease_policy(),
            caller_deadline: None,
        })
        .await
        .expect("claim provisioning lease")
        .expect("provisioning lease exists");
    ActiveHandCapacityRequest {
        tenant_id,
        workspace_id,
        provider_account_id: account_id,
        provider_account_generation: 1,
        provisioning_operation_id: provisioning.provisioning_operation_id,
        hand_lease_generation: provisioning.generation,
        expected_writer_epoch: restoring.writer_epoch,
        expected_instance_generation: restoring.instance_generation,
    }
}

#[tokio::test]
#[ignore = "requires a fresh V60 compose Postgres via MOA_DATABASE_URL"]
async fn concurrent_hand_limit_plus_one_is_rejected_in_both_scopes_db() {
    // Pins: `max_active_hands` is charged against the active_hands dimension in
    // both the tenant and the provider-account scope, so hand number
    // limit-plus-one is refused instead of admitted against an absent limit.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let account_id = ProviderAccountId::new();
    let saturating_tenant = TenantId::new();
    let starved_tenant = TenantId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'local', $2, $3, '{"active_hands": 1}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("active-hands-{account_id}"))
    .bind(format!("org-{account_id}"))
    .execute(&pool)
    .await
    .expect("seed provider-account concurrent-hand ceiling");
    for (tenant_id, limit) in [(saturating_tenant, 1), (starved_tenant, 2)] {
        sqlx::query(
            "INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits) VALUES ($1, $2)",
        )
        .bind(tenant_id)
        .bind(serde_json::json!({ "active_hands": limit }))
        .execute(&pool)
        .await
        .expect("seed tenant concurrent-hand quota");
    }

    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let admitted =
        seed_hydrating_hand(&pool, saturating_tenant, account_id, "capacity-first").await;
    let reservation = capacity
        .reserve_active_hand(&admitted)
        .await
        .expect("the exact concurrent-hand limit is admitted");
    assert_eq!(
        reservation.dimension,
        WorkspaceCapacityDimension::ActiveHands
    );
    assert_eq!(reservation.quantity, 1);

    let over_tenant_limit =
        seed_hydrating_hand(&pool, saturating_tenant, account_id, "capacity-second").await;
    let tenant_error = capacity
        .reserve_active_hand(&over_tenant_limit)
        .await
        .expect_err("limit plus one must be refused for the saturating tenant");
    assert!(
        matches!(
            tenant_error,
            MoaError::ValidationError(ref detail)
                if detail.contains("tenant active_hands capacity exceeded")
        ),
        "concurrent-hand admission must name the exhausted dimension: {tenant_error}"
    );

    let over_account_limit =
        seed_hydrating_hand(&pool, starved_tenant, account_id, "capacity-third").await;
    let account_error = capacity
        .reserve_active_hand(&over_account_limit)
        .await
        .expect_err("a second tenant cannot exceed the shared provider-account ceiling");
    assert!(
        matches!(
            account_error,
            MoaError::ValidationError(ref detail)
                if detail.contains("provider account active_hands capacity exceeded")
        ),
        "the provider-account ceiling must be the reported scope: {account_error}"
    );

    let charged = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.sandbox_capacity_reservations WHERE provider_account_id = $1 AND resource_dimension = 'active_hands'",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .expect("count durable concurrent-hand charges");
    assert_eq!(
        charged, 1,
        "a refused admission cannot leave a partial concurrent-hand charge"
    );

    for tenant_id in [saturating_tenant, starved_tenant] {
        sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
            .bind(tenant_id)
            .execute(&pool)
            .await
            .expect("clean tenant limits");
    }
    sqlx::query("DELETE FROM moa.hand_leases WHERE tenant_id = ANY($1)")
        .bind(vec![saturating_tenant, starved_tenant])
        .execute(&pool)
        .await
        .expect("clean hand leases");
    cleanup_volume_account(&pool, account_id).await;
    pool.close().await;
}

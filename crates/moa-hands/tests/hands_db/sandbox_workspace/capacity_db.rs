//! Atomic sandbox-workspace capacity admission against Postgres.

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::MoaError,
    types::{
        identifiers::{
            ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
            TenantId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, SandboxWorkspaceScope, WorkspaceCapacityDimension,
            WorkspaceOperationKind,
        },
    },
};
use moa_hands::core::sandbox_workspace::{
    capacity::{CapacityQuantity, CapacityReservationRequest, PostgresWorkspaceCapacityRepository},
    model::CreateWorkspaceRequest,
    operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
    repository::PostgresWorkspaceRepository,
    storage_resources::{PostgresWorkspaceStorageResourceRepository, StorageResourceCreateIntent},
};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::database_url;

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
    PostgresWorkspaceOperationRepository::new(pool.clone())
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
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn exact_capacity_succeeds_and_exact_limit_plus_one_is_deterministic_db() {
    // Pins: one atomic reservation consumes the exact tenant/provider limit and
    // the next replica is rejected without a partial reservation.
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
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let now = Utc::now();
    let mut requests = Vec::new();
    for ordinal in 0..2 {
        let workspace_id = SandboxWorkspaceId::new();
        let operation_id = WorkspaceOperationId::new();
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
            .expect("create logical workspace");
        operations
            .persist_intent(&WorkspaceOperationIntent {
                operation_id,
                tenant_id,
                workspace_id,
                provider_account_id: account_id,
                provider_account_generation: 1,
                kind: WorkspaceOperationKind::Create,
                request_hash: format!("sha256:capacity-{ordinal}"),
                expected_writer_epoch: 0,
                expected_instance_generation: 0,
                expected_checkpoint_generation: 0,
                deadline_at: now + ChronoDuration::seconds(10),
                reconcile_not_before: now + ChronoDuration::seconds(20),
            })
            .await
            .expect("persist create intent");
        requests.push(CapacityReservationRequest {
            tenant_id,
            workspace_id,
            operation_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            quantities: vec![CapacityQuantity {
                dimension: WorkspaceCapacityDimension::Workspaces,
                quantity: 1,
            }],
        });
    }

    let first = capacity
        .reserve(&requests[0])
        .await
        .expect("exact capacity is admitted");
    assert_eq!(first.len(), 1);
    assert!(
        capacity.reserve(&requests[1]).await.is_err(),
        "exact limit plus one must be rejected"
    );
    let reservation_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("count atomic reservations");
    assert_eq!(
        reservation_count, 1,
        "a rejected batch leaves no partial row"
    );

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean reservations");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&pool)
        .await
        .expect("clean operations");
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
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
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

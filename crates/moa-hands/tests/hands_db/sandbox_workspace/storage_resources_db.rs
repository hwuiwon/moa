//! Daytona tenant-volume ownership and lifetime-reservation behavior against Postgres.

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
            WorkspaceConfirmedDisposition, WorkspaceOperationKind,
        },
    },
};
use moa_hands::core::sandbox_workspace::{
    capacity::{CapacityQuantity, CapacityReservationRequest, PostgresWorkspaceCapacityRepository},
    model::CreateWorkspaceRequest,
    operations::{
        AbsenceObservation, ClaimedWorkspaceOperation, PostgresWorkspaceOperationRepository,
        WorkspaceOperationIntent,
    },
    repository::PostgresWorkspaceRepository,
    storage_resources::{
        PostgresWorkspaceStorageResourceRepository, StorageResourceCreateIntent,
        StorageResourceState,
    },
};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::database_url;

#[derive(Debug, Clone, Copy)]
struct SeededOperation {
    tenant_id: TenantId,
    account_id: ProviderAccountId,
    workspace_id: SandboxWorkspaceId,
    operation_id: WorkspaceOperationId,
}

async fn seed_account(pool: &sqlx::PgPool, account_id: ProviderAccountId) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'daytona', $2, $3, '{"volumes": 100}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("storage-db-{account_id}"))
    .bind(format!("org-{account_id}"))
    .execute(pool)
    .await
    .expect("seed Daytona provider account");
}

async fn seed_create_operation(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    account_id: ProviderAccountId,
    ordinal: &str,
) -> SeededOperation {
    let workspace_id = SandboxWorkspaceId::new();
    let operation_id = WorkspaceOperationId::new();
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
        .expect("persist logical workspace before provider storage I/O");
    let now = Utc::now();
    PostgresWorkspaceOperationRepository::new(pool.clone())
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: format!("sha256:storage-create-{ordinal}"),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(10),
            reconcile_not_before: now + ChronoDuration::seconds(20),
        })
        .await
        .expect("persist exact create operation before provider storage I/O");
    SeededOperation {
        tenant_id,
        account_id,
        workspace_id,
        operation_id,
    }
}

fn storage_intent(
    seeded: SeededOperation,
    storage_resource_id: Uuid,
    security_class: &str,
    deterministic_name: &str,
) -> StorageResourceCreateIntent {
    StorageResourceCreateIntent {
        storage_resource_id,
        tenant_id: seeded.tenant_id,
        workspace_id: seeded.workspace_id,
        create_operation_id: seeded.operation_id,
        provider_account_id: seeded.account_id,
        provider_account_generation: 1,
        security_class: security_class.to_string(),
        deterministic_name: deterministic_name.to_string(),
        verified_owner_fingerprint: format!("owner-{}", seeded.tenant_id),
    }
}

fn lifetime_request(seeded: SeededOperation) -> CapacityReservationRequest {
    CapacityReservationRequest {
        tenant_id: seeded.tenant_id,
        workspace_id: seeded.workspace_id,
        operation_id: seeded.operation_id,
        provider_account_id: seeded.account_id,
        provider_account_generation: 1,
        expected_writer_epoch: 0,
        expected_instance_generation: 0,
        quantities: vec![CapacityQuantity {
            dimension: WorkspaceCapacityDimension::Volumes,
            quantity: 1,
        }],
    }
}

async fn persist_delete_operation(
    pool: &sqlx::PgPool,
    seeded: SeededOperation,
) -> WorkspaceOperationId {
    let operation_id = WorkspaceOperationId::new();
    let now = Utc::now();
    PostgresWorkspaceOperationRepository::new(pool.clone())
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id: seeded.tenant_id,
            workspace_id: seeded.workspace_id,
            provider_account_id: seeded.account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Delete,
            request_hash: format!("sha256:storage-delete-{operation_id}"),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now - ChronoDuration::seconds(3),
            reconcile_not_before: now - ChronoDuration::seconds(2),
        })
        .await
        .expect("persist exact delete operation before provider deletion");
    operation_id
}

async fn cleanup_account(pool: &sqlx::PgPool, account_id: ProviderAccountId) {
    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean volume capacity reservations");
    sqlx::query("DELETE FROM moa.sandbox_storage_resources WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean storage resources");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean storage operations");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean storage workspaces");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await
        .expect("clean Daytona provider account");
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn deterministic_create_replay_cannot_create_a_second_live_tenant_volume_db() {
    // Pins: replay returns the exact create row, while a changed replay or a
    // second live volume for the same tenant/account generation/security class
    // fails at the durable ownership boundary.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&pool, account_id).await;
    let resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());

    let first = seed_create_operation(&pool, tenant_id, account_id, "first").await;
    let first_resource_id = Uuid::now_v7();
    let first_intent = storage_intent(
        first,
        first_resource_id,
        "tenant-isolated",
        &format!("moa-volume-{first_resource_id}"),
    );
    let created = resources
        .persist_create_intent(&first_intent)
        .await
        .expect("first deterministic create intent should persist");
    let replayed = resources
        .persist_create_intent(&first_intent)
        .await
        .expect("exact deterministic replay should resolve the same row");
    assert_eq!(replayed, created);

    let mut conflicting_replay = first_intent.clone();
    conflicting_replay.deterministic_name = format!("changed-{first_resource_id}");
    assert!(
        matches!(
            resources.persist_create_intent(&conflicting_replay).await,
            Err(MoaError::StorageError(_))
        ),
        "a replay-stable resource id cannot be rebound to a changed create name"
    );

    let second = seed_create_operation(&pool, tenant_id, account_id, "second").await;
    let second_resource_id = Uuid::now_v7();
    let duplicate = storage_intent(
        second,
        second_resource_id,
        "tenant-isolated",
        &format!("moa-volume-{second_resource_id}"),
    );
    assert!(
        matches!(
            resources.persist_create_intent(&duplicate).await,
            Err(MoaError::StorageError(_))
        ),
        "V58 must reject a duplicate live tenant volume"
    );
    let live = resources
        .live_tenant_volume(tenant_id, account_id, 1, "tenant-isolated")
        .await
        .expect("load the sole live tenant volume")
        .expect("the first volume remains authoritative");
    assert_eq!(live.storage_resource_id, first_resource_id);
    assert_eq!(live.state, StorageResourceState::Creating);

    cleanup_account(&pool, account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn ambiguous_create_retains_its_lifetime_charge_and_rejects_stale_callbacks_db() {
    // Pins: an ambiguous Daytona create remains owned and charged, and only
    // the exact resource/operation/account generation can make it ready.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&pool, account_id).await;
    let seeded = seed_create_operation(&pool, tenant_id, account_id, "ambiguous").await;
    let storage_resource_id = Uuid::now_v7();
    let resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    resources
        .persist_create_intent(&storage_intent(
            seeded,
            storage_resource_id,
            "tenant-isolated",
            &format!("moa-volume-{storage_resource_id}"),
        ))
        .await
        .expect("persist storage resource before reserving provider capacity");
    let request = lifetime_request(seeded);
    capacity
        .reserve_lifetime_volume(&request, storage_resource_id, 100, 5, &[])
        .await
        .expect("reserve the tenant volume for its full lifetime");

    assert!(
        !resources
            .confirm_created(
                tenant_id,
                storage_resource_id,
                2,
                seeded.operation_id,
                "daytona-volume-ambiguous",
            )
            .await
            .expect("stale resource generation is a fenced miss")
    );
    assert!(
        !resources
            .confirm_created(
                tenant_id,
                storage_resource_id,
                1,
                WorkspaceOperationId::new(),
                "daytona-volume-ambiguous",
            )
            .await
            .expect("wrong create operation is a fenced miss")
    );
    assert!(
        resources
            .mark_unknown(tenant_id, storage_resource_id, 1)
            .await
            .expect("mark ambiguous storage resource")
    );
    assert!(
        capacity
            .mark_lifetime_volume_reconciling(&request, storage_resource_id)
            .await
            .expect("retain the lifetime charge while reconciling")
    );
    assert!(
        operations
            .mark_unknown(tenant_id, seeded.operation_id)
            .await
            .expect("mark ambiguous create operation")
    );

    let ambiguous = resources
        .get(tenant_id, storage_resource_id)
        .await
        .expect("load ambiguous storage resource")
        .expect("ambiguous resource remains durably owned");
    assert_eq!(ambiguous.state, StorageResourceState::Unknown);
    let reservation_state = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE storage_resource_id = $1",
    )
    .bind(storage_resource_id)
    .fetch_one(&pool)
    .await
    .expect("load ambiguous lifetime reservation");
    assert_eq!(reservation_state, "reconciling");

    assert!(
        resources
            .confirm_created(
                tenant_id,
                storage_resource_id,
                1,
                seeded.operation_id,
                "daytona-volume-ambiguous",
            )
            .await
            .expect("exact reconciliation callback confirms the resource")
    );
    assert!(
        capacity
            .commit_lifetime_volume(&request, storage_resource_id)
            .await
            .expect("commit the exact linked lifetime reservation")
    );
    assert!(
        operations
            .confirm_disposition(
                tenant_id,
                seeded.operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
            )
            .await
            .expect("confirm the ambiguous provider create")
    );
    let ready = resources
        .get(tenant_id, storage_resource_id)
        .await
        .expect("load reconciled resource")
        .expect("reconciled resource remains present");
    assert_eq!(ready.state, StorageResourceState::Ready);
    assert_eq!(
        ready.provider_reference.as_deref(),
        Some("daytona-volume-ambiguous")
    );
    assert!(
        !resources
            .mark_unknown(tenant_id, storage_resource_id, 2)
            .await
            .expect("stale generation cannot regress the ready resource")
    );

    cleanup_account(&pool, account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn lifetime_volume_releases_only_after_exact_two_observation_delete_db() {
    // Pins: neither a stale delete callback nor one empty observation releases
    // a tenant volume; the exact fenced resource releases only after two
    // separated empty observations confirm provider absence.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&pool, account_id).await;
    let seeded = seed_create_operation(&pool, tenant_id, account_id, "delete").await;
    let storage_resource_id = Uuid::now_v7();
    let resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());
    let capacity = PostgresWorkspaceCapacityRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    resources
        .persist_create_intent(&storage_intent(
            seeded,
            storage_resource_id,
            "tenant-isolated",
            &format!("moa-volume-{storage_resource_id}"),
        ))
        .await
        .expect("persist storage resource before provider creation");
    let request = lifetime_request(seeded);
    capacity
        .reserve_lifetime_volume(&request, storage_resource_id, 100, 5, &[])
        .await
        .expect("reserve the volume for its full lifetime");
    assert!(
        resources
            .confirm_created(
                tenant_id,
                storage_resource_id,
                1,
                seeded.operation_id,
                "daytona-volume-delete",
            )
            .await
            .expect("confirm created resource")
    );
    assert!(
        capacity
            .commit_lifetime_volume(&request, storage_resource_id)
            .await
            .expect("commit linked lifetime reservation")
    );
    assert!(
        operations
            .confirm_disposition(
                tenant_id,
                seeded.operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
            )
            .await
            .expect("confirm create operation")
    );

    let delete_operation_id = persist_delete_operation(&pool, seeded).await;
    assert!(
        !resources
            .begin_delete(tenant_id, storage_resource_id, 2, delete_operation_id)
            .await
            .expect("stale resource generation is a fenced delete miss")
    );
    assert!(
        !resources
            .begin_delete(tenant_id, storage_resource_id, 1, seeded.operation_id,)
            .await
            .expect("a create operation cannot own deletion")
    );
    assert!(
        resources
            .begin_delete(tenant_id, storage_resource_id, 1, delete_operation_id)
            .await
            .expect("exact delete operation fences the resource")
    );
    assert!(
        operations
            .mark_unknown(tenant_id, delete_operation_id)
            .await
            .expect("provider delete outcome remains ambiguous")
    );

    let claim_token = Uuid::now_v7();
    let claimed_rows = sqlx::query(
        r#"
        UPDATE moa.sandbox_workspace_operations
        SET claim_token = $2, claim_expires_at = now() + interval '1 minute'
        WHERE operation_id = $1 AND outcome_class = 'unknown'
        "#,
    )
    .bind(delete_operation_id)
    .bind(claim_token)
    .execute(&pool)
    .await
    .expect("fixture claims the exact delete operation without global test races")
    .rows_affected();
    assert_eq!(claimed_rows, 1);
    let claimed = ClaimedWorkspaceOperation {
        operation: operations
            .get(tenant_id, delete_operation_id)
            .await
            .expect("load claimed delete operation")
            .expect("delete operation remains present"),
        claim_token,
    };
    let first_observed_at = Utc::now();
    assert_eq!(
        operations
            .record_inventory_observation(
                &claimed,
                true,
                "sha256:empty-delete-inventory",
                first_observed_at,
            )
            .await
            .expect("persist first empty observation"),
        AbsenceObservation::First
    );
    assert!(
        !operations
            .confirm_absent(&claimed)
            .await
            .expect("one observation cannot confirm absence")
    );
    assert!(
        !resources
            .confirm_deleted_and_release_lifetime(
                tenant_id,
                storage_resource_id,
                1,
                delete_operation_id,
            )
            .await
            .expect("one observation cannot delete the storage row")
    );
    let state_after_first = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE storage_resource_id = $1",
    )
    .bind(storage_resource_id)
    .fetch_one(&pool)
    .await
    .expect("load lifetime reservation after one observation");
    assert_eq!(state_after_first, "committed");

    assert_eq!(
        operations
            .record_inventory_observation(
                &claimed,
                true,
                "sha256:empty-delete-inventory",
                first_observed_at + ChronoDuration::seconds(2),
            )
            .await
            .expect("persist independently separated empty observation"),
        AbsenceObservation::Proven
    );
    assert!(
        operations
            .confirm_absent(&claimed)
            .await
            .expect("two observations confirm exact provider absence")
    );
    assert!(
        !resources
            .confirm_deleted_and_release_lifetime(
                tenant_id,
                storage_resource_id,
                2,
                delete_operation_id,
            )
            .await
            .expect("stale generation cannot release the lifetime charge")
    );
    let still_committed = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE storage_resource_id = $1",
    )
    .bind(storage_resource_id)
    .fetch_one(&pool)
    .await
    .expect("load lifetime reservation after stale finalization");
    assert_eq!(still_committed, "committed");
    assert!(
        resources
            .confirm_deleted_and_release_lifetime(
                tenant_id,
                storage_resource_id,
                1,
                delete_operation_id,
            )
            .await
            .expect("exact proven delete releases its lifetime charge")
    );
    let final_row = resources
        .get(tenant_id, storage_resource_id)
        .await
        .expect("load finalized resource")
        .expect("deleted resource remains as durable ownership evidence");
    assert_eq!(final_row.state, StorageResourceState::Deleted);
    assert!(final_row.provider_reference.is_none());
    let released = sqlx::query_scalar::<_, String>(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE storage_resource_id = $1",
    )
    .bind(storage_resource_id)
    .fetch_one(&pool)
    .await
    .expect("load released lifetime reservation");
    assert_eq!(released, "released");

    cleanup_account(&pool, account_id).await;
    pool.close().await;
}

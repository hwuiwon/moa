//! External-first sandbox workspace tenant purge and absence fencing against Postgres.

use chrono::{Duration as ChronoDuration, Utc};
use moa_config::CheckpointRetentionConfig;
use moa_core::{
    error::MoaError,
    types::{
        identifiers::{ProviderAccountId, TenantId, WorkspaceOperationId},
        sandbox_workspace::{
            ProviderInventoryResource, ProviderInventoryResourceKind,
            WorkspaceConfirmedDisposition, WorkspaceOperationKind,
        },
    },
};
use moa_hands::core::sandbox_workspace::{
    maintenance::WorkspaceMaintenanceCoordinator,
    operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
    storage_resources::{PostgresWorkspaceStorageResourceRepository, StorageResourceCreateIntent},
};
use sqlx::Row;
use uuid::Uuid;

use super::sandbox_workspace_retention_db::{
    create_workspace, maintenance_fixture, pools, seed_account,
};

#[tokio::test]
#[ignore = "requires a fresh V58 database and distinct runtime/workspace-maintenance logins"]
async fn tenant_purge_fences_access_before_external_delete_and_requires_exact_absence_proof_db() {
    // Pins: only the dedicated maintenance role may own purge, logical access
    // is fenced before provider deletion, external metadata survives until an
    // exact proof is confirmed, and relational purge stays closed beforehand.
    let (runtime, maintenance) = pools().await;
    let runtime_role_error = WorkspaceMaintenanceCoordinator::verify_maintenance_pool(&runtime)
        .await
        .expect_err("ordinary runtime login must not assume workspace maintenance ownership");
    assert!(
        matches!(runtime_role_error, MoaError::ConfigError(ref detail)
            if detail == "workspace maintenance database login must be a distinct NOINHERIT member of moa_workspace_maintenance"),
        "runtime role must fail at the exact maintenance boundary: {runtime_role_error}"
    );
    WorkspaceMaintenanceCoordinator::verify_maintenance_pool(&maintenance)
        .await
        .expect("dedicated maintenance login should assume the exact NOLOGIN role");

    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&runtime, account_id).await;
    let workspace_id = create_workspace(&runtime, tenant_id, account_id).await;
    let operation_id = WorkspaceOperationId::new();
    let now = Utc::now();
    let operations = PostgresWorkspaceOperationRepository::new(runtime.clone());
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: format!("sha256:purge-create-{operation_id}"),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(30),
            reconcile_not_before: now + ChronoDuration::seconds(60),
        })
        .await
        .expect("persist storage create operation through production repository");
    assert!(
        operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("fence the provider create before storage I/O")
    );
    let storage_resource_id = Uuid::now_v7();
    let provider_reference = format!("purge-volume-{storage_resource_id}");
    let resources = PostgresWorkspaceStorageResourceRepository::new(runtime.clone());
    resources
        .persist_create_intent(&StorageResourceCreateIntent {
            storage_resource_id,
            tenant_id,
            workspace_id,
            create_operation_id: operation_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            security_class: "tenant-isolated".to_string(),
            deterministic_name: format!("moa-purge-{storage_resource_id}"),
            verified_owner_fingerprint: format!("sha256:owner-{workspace_id}"),
        })
        .await
        .expect("persist storage ownership before provider mutation");
    assert!(
        resources
            .confirm_created(
                tenant_id,
                storage_resource_id,
                1,
                operation_id,
                &provider_reference,
            )
            .await
            .expect("confirm exact provider storage reference")
    );
    assert!(
        operations
            .confirm_disposition(
                tenant_id,
                operation_id,
                WorkspaceConfirmedDisposition::ResourcePresent,
            )
            .await
            .expect("settle the create operation before tenant purge")
    );

    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        CheckpointRetentionConfig::default(),
    )
    .await;
    fixture
        .storage
        .set_inventory(vec![ProviderInventoryResource {
            kind: ProviderInventoryResourceKind::MutableFilesystem,
            provider_reference: provider_reference.clone(),
            resource_fingerprint: format!("sha256:purge-resource-{storage_resource_id}"),
            evidence_digest: format!("sha256:purge-evidence-{storage_resource_id}"),
            verified_owner: None,
        }])
        .await;
    let purge_operation_id = format!("sandbox-purge-{tenant_id}");
    let proof = fixture
        .coordinator
        .purge_tenant_external(tenant_id, &purge_operation_id)
        .await
        .expect("fence tenant and prove every external sandbox resource absent");
    assert_eq!(proof.tenant_id, tenant_id);
    assert_eq!(proof.operation_id, purge_operation_id);
    assert_eq!(
        (proof.hands, proof.storage_resources, proof.checkpoints),
        (0, 1, 0)
    );
    assert_eq!(proof.provider_accounts, 1);
    assert!(proof.provider_inventory_digest.starts_with("sha256:"));

    let deletes = fixture.storage.delete_observations().await;
    assert_eq!(deletes.len(), 1);
    assert!(
        deletes[0].workspace_was_fenced,
        "provider deletion must observe the logical access fence first"
    );
    assert_eq!(deletes[0].request.tenant_id, tenant_id);
    assert_eq!(deletes[0].request.purge_operation_id, purge_operation_id);
    assert_eq!(deletes[0].request.provider_account_id, account_id);
    assert_eq!(deletes[0].request.provider_account_generation, 1);
    assert_eq!(deletes[0].request.storage.resource_id, provider_reference);

    let retained = sqlx::query(
        "SELECT workspace.lifecycle_state, workspace.access_fenced_at IS NOT NULL AS fenced, \
         resource.lifecycle_state AS resource_state, resource.provider_reference \
         FROM moa.sandbox_workspaces AS workspace \
         JOIN moa.sandbox_storage_resources AS resource \
           ON resource.tenant_id = workspace.tenant_id \
          AND resource.storage_resource_id = $3 \
         WHERE workspace.tenant_id = $1 AND workspace.workspace_id = $2",
    )
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(storage_resource_id)
    .fetch_one(&runtime)
    .await
    .expect("external purge must retain durable reconciliation metadata");
    assert_eq!(
        retained
            .try_get::<String, _>("lifecycle_state")
            .expect("workspace state"),
        "deleting"
    );
    assert!(retained.try_get::<bool, _>("fenced").expect("access fence"));
    assert_eq!(
        retained
            .try_get::<String, _>("resource_state")
            .expect("storage state"),
        "ready"
    );
    assert_eq!(
        retained
            .try_get::<Option<String>, _>("provider_reference")
            .expect("provider reference")
            .as_deref(),
        Some(provider_reference.as_str())
    );

    let mut before_confirmation = maintenance
        .begin()
        .await
        .expect("begin pre-confirmation role fence check");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *before_confirmation)
        .await
        .expect("activate maintenance role for absence gate");
    let gate_error =
        sqlx::query("SELECT moa.require_sandbox_external_absence_for_tenant_purge($1, $2)")
            .bind(tenant_id)
            .bind(&purge_operation_id)
            .execute(&mut *before_confirmation)
            .await
            .expect_err("relational purge must remain fenced before durable absence confirmation");
    assert_eq!(
        gate_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("55000")),
        "pre-confirmation fence must fail with object-not-in-prerequisite-state"
    );
    before_confirmation
        .rollback()
        .await
        .expect("rollback expected absence-gate failure");

    fixture
        .coordinator
        .confirm_tenant_external_absence(tenant_id, &purge_operation_id, &proof)
        .await
        .expect("journal the exact external absence proof");
    let mut after_confirmation = maintenance
        .begin()
        .await
        .expect("begin confirmed role fence check");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(&mut *after_confirmation)
        .await
        .expect("activate maintenance role after proof confirmation");
    sqlx::query("SELECT moa.require_sandbox_external_absence_for_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&purge_operation_id)
        .execute(&mut *after_confirmation)
        .await
        .expect("confirmed external absence opens the relational purge gate");
    after_confirmation
        .rollback()
        .await
        .expect("rollback read-only confirmed gate check");

    runtime.close().await;
    maintenance.close().await;
}

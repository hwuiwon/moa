//! Scheduled-only sandbox-workspace fleet-capacity soak.
//!
//! The lane intentionally uses the production Postgres repositories and a
//! fresh migrated database. It does not contact a cloud provider or spend
//! provider credit.

#![cfg(feature = "integration")]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use moa_core::{
    error::MoaError,
    traits::SandboxStorageProvider,
    types::{
        identifiers::{
            ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
            TenantId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, ProviderAccountStorageInventory, ProviderInventoryOwner,
            ProviderInventoryResource, ProviderInventoryResourceKind, ProviderStorageRef,
            SandboxWorkspaceScope, WorkspaceAttachRequest, WorkspaceCheckpointPublishRequest,
            WorkspaceOperationKind, WorkspaceReconcileRequest, WorkspaceRestoreRequest,
            WorkspaceStorageDeleteRequest, WorkspaceStorageOperationResult,
            WorkspaceStoragePrepareRequest,
        },
    },
};
use moa_crypto::LocalKmsProvider;
use moa_hands::LocalHandProvider;
use moa_hands::core::sandbox_workspace::{
    checkpoint::{
        archive::ArchiveLimits,
        store::{CheckpointObjectStore, ObservedCheckpointBucketVersioning},
    },
    maintenance::WorkspaceMaintenanceCoordinator,
    model::CreateWorkspaceRequest,
    operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
    repository::PostgresWorkspaceRepository,
    storage_resources::{PostgresWorkspaceStorageResourceRepository, StorageResourceCreateIntent},
};
use object_store::memory::InMemory;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const TENANT_COUNT: usize = 1_000;
const SCRIPTED_PROVIDER: &str = "scripted-soak";

struct Candidate {
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    operation_id: WorkspaceOperationId,
}

struct ScriptedSoakStorageProvider {
    create_calls: AtomicUsize,
    resources: tokio::sync::Mutex<Vec<ProviderInventoryResource>>,
}

impl ScriptedSoakStorageProvider {
    fn new() -> Self {
        Self {
            create_calls: AtomicUsize::new(0),
            resources: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn provision(&self, candidate: &Candidate) -> String {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        let provider_reference = format!("soak-storage/{}", candidate.workspace_id);
        let resource_fingerprint = format!("sha256:soak-resource-{}", candidate.workspace_id);
        self.resources.lock().await.push(ProviderInventoryResource {
            kind: ProviderInventoryResourceKind::MutableFilesystem,
            provider_reference: provider_reference.clone(),
            resource_fingerprint,
            evidence_digest: format!("sha256:soak-evidence-{}", candidate.workspace_id),
            verified_owner: Some(ProviderInventoryOwner {
                tenant_id: candidate.tenant_id,
                workspace_id: candidate.workspace_id,
                provisioning_operation_id: None,
                writer_epoch: Some(0),
                instance_generation: Some(0),
            }),
        });
        provider_reference
    }

    fn unsupported<T>() -> moa_core::error::Result<T> {
        Err(MoaError::Unsupported(
            "scripted soak provider supports inventory only".to_string(),
        ))
    }
}

#[async_trait]
impl SandboxStorageProvider for ScriptedSoakStorageProvider {
    fn storage_provider_name(&self) -> &str {
        SCRIPTED_PROVIDER
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
    ) -> moa_core::error::Result<ProviderAccountStorageInventory> {
        Ok(ProviderAccountStorageInventory {
            provider_account_id,
            provider_account_generation,
            observed_at: Utc::now(),
            resources: self.resources.lock().await.clone(),
        })
    }

    async fn prepare_workspace_storage(
        &self,
        _request: WorkspaceStoragePrepareRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn attach_workspace(
        &self,
        _request: WorkspaceAttachRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn publish_workspace_checkpoint(
        &self,
        _request: WorkspaceCheckpointPublishRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn restore_workspace(
        &self,
        _request: WorkspaceRestoreRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn delete_workspace_storage(
        &self,
        _request: WorkspaceStorageDeleteRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn reconcile_workspace_operation(
        &self,
        _request: WorkspaceReconcileRequest,
    ) -> moa_core::error::Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn verify_workspace_storage(
        &self,
        _storage: &ProviderStorageRef,
    ) -> moa_core::error::Result<bool> {
        Self::unsupported()
    }
}

async fn seed_candidate(
    workspaces: &PostgresWorkspaceRepository,
    operations: &PostgresWorkspaceOperationRepository,
    pool: &sqlx::PgPool,
    account_id: ProviderAccountId,
    candidate: &Candidate,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits) \
         VALUES ($1, '{\"workspaces\": 1}'::jsonb)",
    )
    .bind(candidate.tenant_id)
    .execute(pool)
    .await
    .context("seed one-tenant workspace quota")?;
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id: candidate.workspace_id,
            tenant_id: candidate.tenant_id,
            scope: SandboxWorkspaceScope::ExecutionTask {
                run_id: ExecutionRunScopeId::new(),
                task_id: ExecutionTaskScopeId::new(),
            },
            provider: SCRIPTED_PROVIDER.to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .context("create soak workspace through production repository")?;
    let now = Utc::now();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id: candidate.operation_id,
            tenant_id: candidate.tenant_id,
            workspace_id: candidate.workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: format!("sha256:soak-{}", candidate.operation_id),
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
            deadline_at: now + Duration::hours(1),
            reconcile_not_before: now + Duration::hours(2),
        })
        .await
        .context("persist soak create intent through production repository")?;
    Ok(())
}

#[tokio::test]
#[ignore = "scheduled-only; requires a fresh migrated Postgres via MOA_DATABASE_URL"]
async fn sandbox_workspace_1000_tenant_soak_exact_capacity_and_zero_drift_service_e2e() -> Result<()>
{
    // Pins: 1,000 isolated tenants can consume the exact explicit provider
    // ceiling once each; tenant 1,001 is rejected atomically before provider I/O,
    // and the durable fleet has no duplicate ownership or inventory finding.
    let database_url = std::env::var("MOA_DATABASE_URL")
        .context("MOA_DATABASE_URL must name the scheduled soak database")?;
    let pool = PgPoolOptions::new()
        .max_connections(12)
        .connect(&database_url)
        .await
        .context("connect workspace soak Postgres")?;
    let account_id = ProviderAccountId::new();
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits, admission_headroom
        ) VALUES (
            $1, 1, $2, $3, $4,
            '{"workspaces": 1000}'::jsonb, '{"workspaces": 0}'::jsonb
        )
        "#,
    )
    .bind(account_id)
    .bind(SCRIPTED_PROVIDER)
    .bind(format!("soak-{account_id}"))
    .bind(format!("soak-org-{account_id}"))
    .execute(&pool)
    .await
    .context("seed exact provider ceiling and explicit zero headroom")?;

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    let storage_resources = PostgresWorkspaceStorageResourceRepository::new(pool.clone());
    let scripted_provider = Arc::new(ScriptedSoakStorageProvider::new());
    let local_root =
        tempfile::tempdir().context("create scripted local-provider inventory root")?;
    let checkpoint_store = std::sync::Arc::new(CheckpointObjectStore::new(
        std::sync::Arc::new(InMemory::new()),
        std::sync::Arc::new(LocalKmsProvider::new()),
        format!("sandbox-workspace-soak/{account_id}"),
        ArchiveLimits::default(),
        ObservedCheckpointBucketVersioning::Unversioned,
    )?);
    let local_provider = std::sync::Arc::new(
        LocalHandProvider::new_with_docker_detection(local_root.path(), false).await?,
    );
    let maintenance = WorkspaceMaintenanceCoordinator::new(
        pool.clone(),
        checkpoint_store,
        vec![scripted_provider.clone()],
        vec![local_provider],
        moa_config::CheckpointRetentionConfig::default(),
        std::time::Duration::from_secs(60),
    )?;
    let mut admitted = Vec::with_capacity(TENANT_COUNT);
    for _ in 0..TENANT_COUNT {
        let candidate = Candidate {
            tenant_id: TenantId::new(),
            workspace_id: SandboxWorkspaceId::new(),
            operation_id: WorkspaceOperationId::new(),
        };
        seed_candidate(&workspaces, &operations, &pool, account_id, &candidate).await?;
        let storage_resource_id = Uuid::now_v7();
        let deterministic_name = format!("soak-storage-{}", candidate.workspace_id);
        let verified_owner_fingerprint = format!("sha256:soak-owner-{}", candidate.workspace_id);
        storage_resources
            .persist_create_intent(&StorageResourceCreateIntent {
                storage_resource_id,
                tenant_id: candidate.tenant_id,
                workspace_id: candidate.workspace_id,
                create_operation_id: candidate.operation_id,
                provider_account_id: account_id,
                provider_account_generation: 1,
                security_class: "scheduled-soak".to_string(),
                deterministic_name,
                verified_owner_fingerprint,
            })
            .await
            .context("persist scripted provider storage intent")?;
        let provider_reference = scripted_provider.provision(&candidate).await;
        assert!(
            storage_resources
                .confirm_created(
                    candidate.tenant_id,
                    storage_resource_id,
                    1,
                    candidate.operation_id,
                    &provider_reference,
                )
                .await
                .context("confirm scripted provider storage")?
        );
        admitted.push(candidate);
    }
    assert_eq!(
        scripted_provider.create_calls.load(Ordering::SeqCst),
        TENANT_COUNT
    );

    let rejected = Candidate {
        tenant_id: TenantId::new(),
        workspace_id: SandboxWorkspaceId::new(),
        operation_id: WorkspaceOperationId::new(),
    };
    let error = seed_candidate(&workspaces, &operations, &pool, account_id, &rejected)
        .await
        .expect_err("provider ceiling plus one must fail during workspace creation");
    assert_eq!(
        scripted_provider.create_calls.load(Ordering::SeqCst),
        TENANT_COUNT,
        "limit+1 cannot enter the scripted provider"
    );
    assert!(
        matches!(
            error.downcast_ref::<MoaError>(),
            Some(MoaError::StorageError(detail))
                if detail.contains("provider account workspaces capacity exceeded: 1000 + 1 > 1000")
        ),
        "atomic create rejection must identify the exact provider-account boundary: {error}"
    );

    let rejected_state: (i64, i64) = sqlx::query_as(
        r#"
        SELECT (SELECT count(*) FROM moa.sandbox_workspaces WHERE workspace_id = $1)::BIGINT,
               (SELECT count(*) FROM moa.sandbox_capacity_reservations
                WHERE workspace_id = $1 AND resource_dimension = 'workspaces')::BIGINT
        "#,
    )
    .bind(rejected.workspace_id)
    .fetch_one(&pool)
    .await
    .context("verify atomic create admission rollback")?;
    assert_eq!(rejected_state, (0, 0));

    let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*)::BIGINT,
               count(DISTINCT tenant_id)::BIGINT,
               count(DISTINCT workspace_id)::BIGINT,
               count(*) FILTER (WHERE reservation_state = 'committed')::BIGINT,
               count(operation_id)::BIGINT
        FROM moa.sandbox_capacity_reservations
        WHERE provider_account_id = $1
          AND resource_dimension = 'workspaces'
          AND reservation_state IN ('pending', 'committed', 'reconciling')
        "#,
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .context("measure exact non-double-counted capacity")?;
    assert_eq!(row, (1_000, 1_000, 1_000, 1_000, 0));

    let workspace_fences: (i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*)::BIGINT,
               count(DISTINCT tenant_id)::BIGINT,
               min(writer_epoch)::BIGINT,
               max(writer_epoch)::BIGINT,
               min(instance_generation)::BIGINT,
               max(instance_generation)::BIGINT,
               min(current_checkpoint_generation)::BIGINT,
               max(current_checkpoint_generation)::BIGINT
        FROM moa.sandbox_workspaces
        WHERE provider_account_id = $1
        "#,
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .context("measure workspace ownership and monotonic head fences")?;
    assert_eq!(workspace_fences, (1_000, 1_000, 0, 0, 0, 0, 0, 0));

    let durable_storage: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*)::BIGINT,
               count(DISTINCT tenant_id)::BIGINT,
               count(DISTINCT provider_reference)::BIGINT
        FROM moa.sandbox_storage_resources
        WHERE provider_account_id = $1
          AND lifecycle_state = 'ready'
        "#,
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .context("measure exact durable scripted-provider inventory owners")?;
    assert_eq!(durable_storage, (1_000, 1_000, 1_000));

    let inventory = maintenance
        .reconcile_claimed_provider_inventory_once(1)
        .await
        .context(
            "claim and reconcile the production provider-account shard after soak admission",
        )?;
    assert_eq!(inventory.accounts, 1);
    assert_eq!(inventory.resources, TENANT_COUNT as u64);
    assert_eq!(inventory.unresolved_findings, 0);
    let backlog = maintenance.backlog().await?;
    assert_eq!(backlog.count, 0);
    assert_eq!(backlog.oldest_age, std::time::Duration::ZERO);

    let findings: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.sandbox_provider_inventory_findings \
         WHERE provider_account_id = $1 AND resolved_at IS NULL",
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .context("count unresolved soak inventory findings")?;
    assert_eq!(
        findings, 0,
        "the scripted zero-drift fleet must converge cleanly"
    );

    let tenant_overages: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*) FROM (
            SELECT tenant_id
            FROM moa.sandbox_capacity_reservations
            WHERE provider_account_id = $1
              AND resource_dimension = 'workspaces'
              AND reservation_state IN ('pending', 'committed', 'reconciling')
            GROUP BY tenant_id
            HAVING sum(quantity) > 1
        ) AS overages
        "#,
    )
    .bind(account_id)
    .fetch_one(&pool)
    .await
    .context("check per-tenant capacity ceilings")?;
    assert_eq!(tenant_overages, 0);

    let admitted_tenant_count = admitted
        .iter()
        .map(|candidate| candidate.tenant_id.0)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert_eq!(admitted_tenant_count, TENANT_COUNT);

    // Cleanup is explicitly scoped to this unique account and its tenant set.
    sqlx::query("DELETE FROM moa.sandbox_storage_resources WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await?;
    for candidate in admitted.iter().chain(std::iter::once(&rejected)) {
        sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
            .bind(candidate.tenant_id)
            .execute(&pool)
            .await?;
    }
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

//! Checkpoint-retention claims and durable tombstones against Postgres.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use moa_config::CheckpointRetentionConfig;
use moa_core::{
    error::{MoaError, Result},
    traits::SandboxStorageProvider,
    types::{
        identifiers::{
            ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
            TenantId, WorkspaceCheckpointId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, ProviderAccountStorageInventory, ProviderInventoryResource,
            ProviderStorageRef, SandboxWorkspaceScope, TenantStoragePurgeRequest,
            WorkspaceAttachRequest, WorkspaceCheckpointPublishRequest,
            WorkspaceConfirmedDisposition, WorkspaceOperationOutcome, WorkspaceReconcileRequest,
            WorkspaceRestoreRequest, WorkspaceStorageDeleteRequest,
            WorkspaceStorageOperationResult, WorkspaceStoragePrepareRequest,
        },
    },
};
use moa_crypto::LocalKmsProvider;
use moa_hands::{
    LocalHandProvider,
    core::sandbox_workspace::{
        checkpoint::{
            archive::ArchiveLimits,
            store::{CheckpointObjectStore, ObservedCheckpointBucketVersioning},
        },
        maintenance::WorkspaceMaintenanceCoordinator,
        model::CreateWorkspaceRequest,
        repository::PostgresWorkspaceRepository,
    },
};
use object_store::memory::InMemory;
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::sync::oneshot;

/// Provider registry key shared by the three maintenance DB behavior modules.
pub(super) const SCRIPTED_PROVIDER: &str = "00-scripted-maintenance-db";

/// Returns a required database URL or fails the ignored DB test with its name.
pub(super) fn required_url(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be configured for this DB test"))
}

/// Connects independent runtime and dedicated maintenance pools for one test.
pub(super) async fn pools() -> (PgPool, PgPool) {
    let runtime = PgPoolOptions::new()
        .max_connections(5)
        .connect(&required_url("MOA_DATABASE_URL"))
        .await
        .expect("connect ordinary runtime database login");
    let maintenance = PgPoolOptions::new()
        .max_connections(5)
        .connect(&required_url("MOA_DATABASE_MAINTENANCE_URL"))
        .await
        .expect("connect dedicated workspace-maintenance login");
    (runtime, maintenance)
}

/// Inserts one unique provider-account generation used only by this test.
pub(super) async fn seed_account(pool: &PgPool, account_id: ProviderAccountId) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits, health
        ) VALUES ($1, 1, $2, $3, $4, '{}'::jsonb, 'disabled')
        "#,
    )
    .bind(account_id)
    .bind(SCRIPTED_PROVIDER)
    .bind(format!("maintenance-db-{account_id}"))
    .bind(format!("maintenance-db-org-{account_id}"))
    .execute(pool)
    .await
    .expect("seed scripted provider account");
}

/// Creates one tenant workspace through the production RLS-scoped repository.
pub(super) async fn create_workspace(
    pool: &PgPool,
    tenant_id: TenantId,
    account_id: ProviderAccountId,
) -> SandboxWorkspaceId {
    let workspace_id = SandboxWorkspaceId::new();
    PostgresWorkspaceRepository::new(pool.clone())
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
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
        .expect("create workspace through production repository");
    workspace_id
}

/// Provider-side observation recorded while deleting one tenant storage resource.
#[derive(Debug, Clone)]
pub(super) struct DeleteObservation {
    /// Exact provider-account-fenced purge request received by the adapter.
    pub request: TenantStoragePurgeRequest,
    /// Whether the logical workspace was already access-fenced at provider dispatch.
    pub workspace_was_fenced: bool,
}

/// Deterministic storage provider used to drive the production maintenance coordinator.
pub(super) struct ScriptedMaintenanceStorageProvider {
    account_id: ProviderAccountId,
    runtime_pool: PgPool,
    inventory: Mutex<Vec<ProviderInventoryResource>>,
    inventory_started: Mutex<Option<oneshot::Sender<()>>>,
    inventory_release: Mutex<Option<oneshot::Receiver<()>>>,
    delete_observations: Mutex<Vec<DeleteObservation>>,
}

impl ScriptedMaintenanceStorageProvider {
    /// Builds an account-fenced provider whose state is isolated to one test.
    pub(super) fn new(account_id: ProviderAccountId, runtime_pool: PgPool) -> Self {
        Self {
            account_id,
            runtime_pool,
            inventory: Mutex::new(Vec::new()),
            inventory_started: Mutex::new(None),
            inventory_release: Mutex::new(None),
            delete_observations: Mutex::new(Vec::new()),
        }
    }

    /// Replaces the complete provider-account inventory returned on the next observation.
    pub(super) async fn set_inventory(&self, resources: Vec<ProviderInventoryResource>) {
        *self.inventory.lock().await = resources;
    }

    /// Gates the next inventory call so a second maintenance replica can race its claim.
    pub(super) async fn gate_next_inventory(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self.inventory_started.lock().await = Some(started_tx);
        *self.inventory_release.lock().await = Some(release_rx);
        (started_rx, release_tx)
    }

    /// Returns the exact tenant-purge delete calls observed by the provider.
    pub(super) async fn delete_observations(&self) -> Vec<DeleteObservation> {
        self.delete_observations.lock().await.clone()
    }

    fn unsupported<T>() -> Result<T> {
        Err(MoaError::Unsupported(
            "scripted maintenance DB provider supports inventory and tenant purge only".to_string(),
        ))
    }
}

#[async_trait]
impl SandboxStorageProvider for ScriptedMaintenanceStorageProvider {
    fn storage_provider_name(&self) -> &str {
        SCRIPTED_PROVIDER
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderAccountStorageInventory> {
        if provider_account_id != self.account_id || provider_account_generation != 1 {
            return Err(MoaError::ValidationError(
                "scripted inventory crossed its provider-account generation".to_string(),
            ));
        }
        if let Some(started) = self.inventory_started.lock().await.take() {
            let _ = started.send(());
        }
        if let Some(release) = self.inventory_release.lock().await.take() {
            release.await.map_err(|_| {
                MoaError::StorageError("inventory claim gate disappeared".to_string())
            })?;
        }
        Ok(ProviderAccountStorageInventory {
            provider_account_id,
            provider_account_generation,
            observed_at: Utc::now(),
            resources: self.inventory.lock().await.clone(),
        })
    }

    async fn prepare_workspace_storage(
        &self,
        _request: WorkspaceStoragePrepareRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn attach_workspace(
        &self,
        _request: WorkspaceAttachRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn publish_workspace_checkpoint(
        &self,
        _request: WorkspaceCheckpointPublishRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn restore_workspace(
        &self,
        _request: WorkspaceRestoreRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn delete_workspace_storage(
        &self,
        _request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn delete_tenant_storage_resource(
        &self,
        request: TenantStoragePurgeRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        if request.provider_account_id != self.account_id
            || request.provider_account_generation != 1
            || request.storage.provider_account_id != self.account_id
            || request.storage.provider_account_generation != 1
        {
            return Err(MoaError::ValidationError(
                "tenant purge crossed its exact provider-account generation".to_string(),
            ));
        }
        let workspace_was_fenced: bool = sqlx::query_scalar(
            "SELECT access_fenced_at IS NOT NULL FROM moa.sandbox_workspaces \
             WHERE tenant_id = $1 AND workspace_id = (\
                 SELECT workspace_id FROM moa.sandbox_storage_resources \
                 WHERE tenant_id = $1 AND provider_reference = $2\
             )",
        )
        .bind(request.tenant_id)
        .bind(&request.storage.resource_id)
        .fetch_one(&self.runtime_pool)
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
        self.inventory
            .lock()
            .await
            .retain(|resource| resource.provider_reference != request.storage.resource_id);
        self.delete_observations
            .lock()
            .await
            .push(DeleteObservation {
                request,
                workspace_was_fenced,
            });
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourceAbsent),
            storage: None,
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn reconcile_workspace_operation(
        &self,
        _request: WorkspaceReconcileRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Self::unsupported()
    }

    async fn verify_workspace_storage(&self, _storage: &ProviderStorageRef) -> Result<bool> {
        Self::unsupported()
    }
}

/// Production coordinator plus the deterministic providers that back one DB test.
pub(super) struct MaintenanceFixture {
    /// Coordinator under test, connected through the dedicated maintenance pool.
    pub coordinator: WorkspaceMaintenanceCoordinator,
    /// Scripted provider used to control and inspect external inventory behavior.
    pub storage: Arc<ScriptedMaintenanceStorageProvider>,
    _local_root: TempDir,
}

/// Constructs the production maintenance coordinator with isolated in-memory providers.
pub(super) async fn maintenance_fixture(
    runtime: &PgPool,
    maintenance: &PgPool,
    account_id: ProviderAccountId,
    retention: CheckpointRetentionConfig,
) -> MaintenanceFixture {
    let checkpoint_store = Arc::new(
        CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            format!("sandbox-maintenance-db/{account_id}"),
            ArchiveLimits::default(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct in-memory checkpoint object store"),
    );
    let local_root = tempfile::tempdir().expect("create local hand root");
    let local = Arc::new(
        LocalHandProvider::new_with_docker_detection(local_root.path(), false)
            .await
            .expect("construct local hand inventory provider"),
    );
    let storage = Arc::new(ScriptedMaintenanceStorageProvider::new(
        account_id,
        runtime.clone(),
    ));
    let coordinator = WorkspaceMaintenanceCoordinator::new(
        maintenance.clone(),
        checkpoint_store,
        vec![storage.clone(), local.clone()],
        vec![local],
        retention,
        Duration::from_secs(30),
    )
    .expect("construct production workspace maintenance coordinator");
    MaintenanceFixture {
        coordinator,
        storage,
        _local_root: local_root,
    }
}

async fn seed_available_checkpoint_chain(
    pool: &PgPool,
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
) -> Vec<(WorkspaceCheckpointId, WorkspaceOperationId)> {
    let mut checkpoints = Vec::new();
    let mut parent = None;
    for generation in 1_i64..=3 {
        let checkpoint_id = WorkspaceCheckpointId::new();
        let operation_id = WorkspaceOperationId::new();
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_workspace_operations (
                operation_id, tenant_id, workspace_id, provider_account_id,
                provider_account_generation, operation_kind, request_hash,
                expected_writer_epoch, expected_instance_generation,
                expected_checkpoint_generation, deadline_at, reconcile_not_before,
                outcome_class, confirmed_disposition
            ) VALUES (
                $1, $2, $3, $4, 1, 'checkpoint', $5,
                0, 0, $6, now(), now(), 'confirmed', 'resource_present'
            )
            "#,
        )
        .bind(operation_id)
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(account_id)
        .bind(format!("sha256:retention-{operation_id}"))
        .bind(generation - 1)
        .execute(pool)
        .await
        .expect("seed confirmed checkpoint operation");
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_workspace_checkpoints (
                checkpoint_id, tenant_id, workspace_id, generation,
                parent_checkpoint_id, parent_generation, source_writer_epoch,
                source_instance_generation, source_checkpoint_generation,
                object_reference, manifest_digest, logical_bytes,
                operation_id, lifecycle_state, verified_at, created_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, 0, 0, $7,
                $8, $9, $10, $11, 'available', now(), now() - interval '2 days'
            )
            "#,
        )
        .bind(checkpoint_id)
        .bind(tenant_id)
        .bind(workspace_id)
        .bind(generation)
        .bind(parent)
        .bind(parent.map(|_| generation - 1))
        .bind(generation - 1)
        .bind(format!("portable-{checkpoint_id}"))
        .bind(format!("sha256:manifest-{checkpoint_id}"))
        .bind(generation * 10)
        .bind(operation_id)
        .execute(pool)
        .await
        .expect("seed available checkpoint");
        checkpoints.push((checkpoint_id, operation_id));
        parent = Some(checkpoint_id);
    }
    let head = checkpoints.last().expect("three checkpoints have a head").0;
    sqlx::query(
        "UPDATE moa.sandbox_workspaces SET lifecycle_state = 'ready', \
         current_checkpoint_generation = 3, current_checkpoint_id = $3 \
         WHERE tenant_id = $1 AND workspace_id = $2",
    )
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(head)
    .execute(pool)
    .await
    .expect("publish fixture workspace head");
    checkpoints
}

#[tokio::test]
#[ignore = "requires a fresh V58 database and distinct runtime/workspace-maintenance logins"]
async fn retention_claims_once_preserves_head_and_tombstones_only_expired_ancestor_db() {
    // Pins: competing retention passes claim one eligible checkpoint exactly
    // once, preserve the head and configured ancestor, then retain immutable
    // audit fields while clearing payload references after two empty proofs.
    let (runtime, maintenance) = pools().await;
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_account(&runtime, account_id).await;
    let workspace_id = create_workspace(&runtime, tenant_id, account_id).await;
    let checkpoints =
        seed_available_checkpoint_chain(&runtime, tenant_id, workspace_id, account_id).await;
    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        CheckpointRetentionConfig {
            retained_ancestor_count: 1,
            minimum_age_seconds: 86_400,
            gc_batch_size: 10,
            claim_ttl_seconds: 30,
            retry_backoff_seconds: 1,
        },
    )
    .await;

    let (left, right) = tokio::join!(
        fixture.coordinator.run_retention_once(),
        fixture.coordinator.run_retention_once()
    );
    let left = left.expect("first competing retention pass");
    let right = right.expect("second competing retention pass");
    assert_eq!(left.claimed + right.claimed, 1);
    assert_eq!(left.awaiting_absence + right.awaiting_absence, 1);
    assert_eq!(
        left.deleted + right.deleted + left.retrying + right.retrying,
        0
    );

    let rows = sqlx::query(
        "SELECT checkpoint_id, generation, lifecycle_state, retention_state, gc_attempts, \
         deletion_absence_observation_count, object_reference, manifest_digest, logical_bytes \
         FROM moa.sandbox_workspace_checkpoints WHERE tenant_id = $1 AND workspace_id = $2 \
         ORDER BY generation",
    )
    .bind(tenant_id)
    .bind(workspace_id)
    .fetch_all(&runtime)
    .await
    .expect("load checkpoint chain after first retention pass");
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0]
            .try_get::<String, _>("lifecycle_state")
            .expect("oldest state"),
        "deleting"
    );
    assert_eq!(
        rows[0]
            .try_get::<i32, _>("gc_attempts")
            .expect("oldest attempts"),
        1
    );
    assert_eq!(
        rows[0]
            .try_get::<i32, _>("deletion_absence_observation_count")
            .expect("oldest proof count"),
        1
    );
    assert_eq!(
        rows[1]
            .try_get::<String, _>("lifecycle_state")
            .expect("retained ancestor state"),
        "available"
    );
    assert_eq!(
        rows[2]
            .try_get::<String, _>("lifecycle_state")
            .expect("head state"),
        "available"
    );

    let crashed_claim = uuid::Uuid::now_v7();
    sqlx::query(
        "UPDATE moa.sandbox_workspace_checkpoints \
         SET gc_claim_token = $2, gc_claim_expires_at = now() + interval '30 seconds', \
             gc_retry_not_before = now() - interval '1 second' \
         WHERE checkpoint_id = $1 AND lifecycle_state = 'deleting'",
    )
    .bind(checkpoints[0].0)
    .bind(crashed_claim)
    .execute(&runtime)
    .await
    .expect("simulate a crashed retention owner with a live claim");
    let fenced = fixture
        .coordinator
        .run_retention_once()
        .await
        .expect("a competing pass must skip another replica's live claim");
    assert_eq!(
        (
            fenced.claimed,
            fenced.deleted,
            fenced.awaiting_absence,
            fenced.retrying
        ),
        (0, 0, 0, 0)
    );

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    sqlx::query(
        "UPDATE moa.sandbox_workspace_checkpoints \
         SET gc_claim_expires_at = now() - interval '1 second' \
         WHERE checkpoint_id = $1 AND gc_claim_token = $2",
    )
    .bind(checkpoints[0].0)
    .bind(crashed_claim)
    .execute(&runtime)
    .await
    .expect("expire the simulated crashed retention claim");
    let completed = fixture
        .coordinator
        .run_retention_once()
        .await
        .expect("retention reclaims the checkpoint for its second empty proof");
    assert_eq!(
        (
            completed.claimed,
            completed.deleted,
            completed.awaiting_absence
        ),
        (1, 1, 0)
    );

    let tombstone = sqlx::query(
        "SELECT checkpoint_id, generation, operation_id, lifecycle_state, retention_state, \
         gc_claim_token, gc_claim_expires_at, gc_attempts, \
         deletion_absence_observation_count, object_reference, \
         manifest_digest, logical_bytes \
         FROM moa.sandbox_workspace_checkpoints WHERE checkpoint_id = $1",
    )
    .bind(checkpoints[0].0)
    .fetch_one(&runtime)
    .await
    .expect("load durable checkpoint tombstone");
    assert_eq!(
        tombstone
            .try_get::<WorkspaceCheckpointId, _>("checkpoint_id")
            .expect("checkpoint id"),
        checkpoints[0].0
    );
    assert_eq!(
        tombstone
            .try_get::<i64, _>("generation")
            .expect("generation"),
        1
    );
    assert_eq!(
        tombstone
            .try_get::<WorkspaceOperationId, _>("operation_id")
            .expect("operation id"),
        checkpoints[0].1
    );
    assert_eq!(
        tombstone
            .try_get::<String, _>("lifecycle_state")
            .expect("lifecycle state"),
        "deleted"
    );
    assert_eq!(
        tombstone
            .try_get::<String, _>("retention_state")
            .expect("retention state"),
        "deleted"
    );
    assert_eq!(
        tombstone
            .try_get::<i32, _>("gc_attempts")
            .expect("attempt count"),
        2
    );
    assert_eq!(
        tombstone
            .try_get::<i32, _>("deletion_absence_observation_count")
            .expect("proof count"),
        2
    );
    assert_eq!(
        tombstone
            .try_get::<Option<uuid::Uuid>, _>("gc_claim_token")
            .expect("claim token"),
        None
    );
    assert_eq!(
        tombstone
            .try_get::<Option<chrono::DateTime<Utc>>, _>("gc_claim_expires_at")
            .expect("claim expiry"),
        None
    );
    assert_eq!(
        tombstone
            .try_get::<Option<String>, _>("object_reference")
            .expect("object reference"),
        None
    );
    assert_eq!(
        tombstone
            .try_get::<String, _>("manifest_digest")
            .expect("manifest digest"),
        format!("sha256:manifest-{}", checkpoints[0].0)
    );
    assert_eq!(
        tombstone
            .try_get::<i64, _>("logical_bytes")
            .expect("logical bytes"),
        10
    );

    let workspace = PostgresWorkspaceRepository::new(runtime.clone())
        .get(tenant_id, workspace_id)
        .await
        .expect("load retained workspace")
        .expect("retained workspace exists");
    assert_eq!(workspace.checkpoint_generation, 3);
    assert_eq!(workspace.checkpoint_id, Some(checkpoints[2].0));

    runtime.close().await;
    maintenance.close().await;
}

// No offline counterpart possible because: this live file verifies real Daytona sandbox provisioning, lifecycle, and proxy execution semantics that a local HTTP mock cannot emulate.

//! Live Daytona integration tests.
//!
//! These tests are ignored by default because they provision real Daytona
//! sandboxes and require valid credentials in the environment.

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use futures_util::FutureExt;
use moa_config::MoaConfig;
use moa_config::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
    DaytonaStorageAccountConfig, DaytonaStorageConfig, ProviderSecretFileSelector,
};
use moa_core::types::action_policy::CallOrigin;
use moa_core::types::identifiers::ToolCallId;
use moa_core::{
    error::MoaError,
    error::Result,
    traits::{HandProvider, Identity, IdentityType, SandboxStorageProvider, SessionStore},
    types::completion::ToolInvocation,
    types::hands::{HandHandle, HandSpec, HandStatus},
    types::identifiers::TenantId,
    types::sandbox_workspace::{
        DurabilityClass, ProviderStorageRef, SandboxWorkspaceScope, WorkspaceBinding,
        WorkspaceOperationKind, WorkspaceStorageOperation,
    },
    types::session::SessionMeta,
};
use moa_crypto::{
    DataKeyDecryptRequest, GeneratedDataKey, KeyHandle, KeyManagementProvider, LocalKmsProvider,
    PlaintextDek,
};
use moa_hands::adapters::daytona::storage::DaytonaStorageDependencies;
use moa_hands::core::sandbox_workspace::{
    capacity::PostgresWorkspaceCapacityRepository,
    checkpoint::{
        archive::ArchiveLimits,
        store::{CheckpointObjectStore, ObservedCheckpointBucketVersioning},
    },
    model::CreateWorkspaceRequest,
    operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
    repository::PostgresWorkspaceRepository,
    storage_resources::PostgresWorkspaceStorageResourceRepository,
};
use moa_hands::{DaytonaHandProvider, FileProviderCredentialSource, ToolRouter};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tempfile::tempdir;
use tokio::time::sleep;
use uuid::Uuid;

fn session(_label: &str) -> SessionMeta {
    let identity = identity();
    SessionMeta {
        tenant_id: identity.tenant_id,
        model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
        created_by: Some(moa_core::types::contact::SessionActorRef::Identity { id: identity.id }),
        ..SessionMeta::default()
    }
}

fn router_workspace_scope(session: &SessionMeta) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::Worker {
        session_id: session.id,
        worker_id: "daytona-live-router-worker".to_string(),
    }
}

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c331),
        tenant_id: TenantId::from(Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c332)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn live_provider() -> DaytonaHandProvider {
    let config = live_config();
    let hands = config.cloud.hands.as_ref().expect("cloud hands config");
    DaytonaHandProvider::new(Arc::new(
        FileProviderCredentialSource::from_config(hands)
            .expect("failed to build Daytona credential source"),
    ))
}

fn live_daytona_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_DAYTONA_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_daytona_credentials() {
    assert!(
        std::env::var("DAYTONA_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        "MOA_RUN_LIVE_DAYTONA_TESTS=1 requires DAYTONA_API_KEY"
    );
}

fn live_config() -> MoaConfig {
    live_config_for_account(live_provider_account_id())
}

fn live_config_for_account(
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
) -> MoaConfig {
    let api_key = std::env::var("DAYTONA_API_KEY").expect("DAYTONA_API_KEY");
    let credential_dir = tempdir().expect("Daytona credential tempdir").keep();
    let credential_path = credential_dir.join("daytona");
    std::fs::write(&credential_path, api_key).expect("write Daytona credential file");
    std::fs::set_permissions(&credential_path, std::fs::Permissions::from_mode(0o400))
        .expect("chmod Daytona credential file");
    let owner_uid = std::fs::metadata(&credential_path)
        .expect("Daytona credential metadata")
        .uid();
    let mut config = MoaConfig::default();
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("daytona".to_string()),
        provider_accounts: vec![CloudHandProviderAccountConfig {
            provider_account_id,
            generation: 1,
            provider: CloudHandProviderKind::Daytona,
            isolation_cell: "daytona-live".to_string(),
            api_origin: std::env::var("DAYTONA_API_ORIGIN")
                .unwrap_or_else(|_| "https://app.daytona.io".to_string()),
            toolbox_origin: Some("https://proxy.app.daytona.io".to_string()),
            sandbox_domain: None,
            default_runtime: Some("daytonaio/workspace:latest".to_string()),
            project_fingerprint: None,
            credential: ProviderSecretFileSelector {
                path: credential_path,
                owner_uid,
            },
        }],
        ..CloudHandsConfig::default()
    });
    config.cloud.daytona_storage = DaytonaStorageConfig {
        accounts: vec![DaytonaStorageAccountConfig {
            provider_account_id,
            security_class: "live-tenant-isolated".to_string(),
            volume_ceiling: 100,
            admission_headroom: 1,
        }],
        consistency_window_seconds: 1,
    };
    config
}

struct DurableTestKms(LocalKmsProvider);

impl DurableTestKms {
    fn new() -> Self {
        Self(LocalKmsProvider::new())
    }
}

#[async_trait]
impl KeyManagementProvider for DurableTestKms {
    async fn generate_data_keys(
        &self,
        contexts: &[moa_crypto::EncryptionContext],
    ) -> std::result::Result<Vec<GeneratedDataKey>, moa_crypto::Error> {
        self.0.generate_data_keys(contexts).await
    }

    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> std::result::Result<Vec<PlaintextDek>, moa_crypto::Error> {
        self.0.decrypt_data_keys(requests).await
    }

    async fn destroy_key(&self, handle: &KeyHandle) -> std::result::Result<(), moa_crypto::Error> {
        self.0.destroy_key(handle).await
    }

    async fn destroy_subject_key(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> std::result::Result<(), moa_crypto::Error> {
        self.0.destroy_subject_key(tenant_id, subject_id).await
    }

    fn is_durable(&self) -> bool {
        true
    }
}

fn live_provider_account_id() -> moa_core::types::identifiers::ProviderAccountId {
    moa_core::types::identifiers::ProviderAccountId(Uuid::new_v5(&Uuid::NAMESPACE_URL, b"daytona"))
}

fn live_database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .expect("MOA_RUN_LIVE_DAYTONA_TESTS=1 requires a fresh V58 MOA_DATABASE_URL")
}

fn live_storage_provider(
    pool: &sqlx::PgPool,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
) -> DaytonaHandProvider {
    let config = live_config_for_account(provider_account_id);
    let credentials = Arc::new(
        FileProviderCredentialSource::from_config(
            config.cloud.hands.as_ref().expect("cloud hands config"),
        )
        .expect("Daytona credential source"),
    );
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());
    let checkpoint_store = Arc::new(
        CheckpointObjectStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            Arc::clone(&kms),
            format!("daytona-live-{provider_account_id}"),
            ArchiveLimits::default(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("live checkpoint store"),
    );
    DaytonaHandProvider::new_with_storage(
        credentials,
        DaytonaStorageDependencies {
            config: DaytonaStorageConfig {
                accounts: vec![DaytonaStorageAccountConfig {
                    provider_account_id,
                    security_class: "live-tenant-isolated".to_string(),
                    volume_ceiling: 100,
                    admission_headroom: 1,
                }],
                consistency_window_seconds: 1,
            },
            checkpoint_store,
            workspaces: Arc::new(PostgresWorkspaceRepository::new(pool.clone())),
            storage_resources: Arc::new(PostgresWorkspaceStorageResourceRepository::new(
                pool.clone(),
            )),
            operations: Arc::new(PostgresWorkspaceOperationRepository::new(pool.clone())),
            capacity: Arc::new(PostgresWorkspaceCapacityRepository::new(pool.clone())),
            kms,
        },
    )
    .expect("Daytona storage provider")
}

async fn seed_live_workspace(pool: &sqlx::PgPool, binding: &WorkspaceBinding) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'daytona', $2, $3,
                  '{"volumes":100,"checkpoints":100,"logical_bytes":1073741824}'::jsonb)
        "#,
    )
    .bind(binding.provider_account_id)
    .bind(format!("daytona-live-{}", binding.provider_account_id))
    .bind(format!("daytona-live-org-{}", binding.provider_account_id))
    .execute(pool)
    .await
    .expect("seed live Daytona provider account");
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits)
        VALUES ($1, '{"volumes":2,"checkpoints":10,"logical_bytes":1073741824}'::jsonb)
        "#,
    )
    .bind(binding.tenant_id)
    .execute(pool)
    .await
    .expect("seed live tenant capacity limits");
    PostgresWorkspaceRepository::new(pool.clone())
        .create(&CreateWorkspaceRequest {
            workspace_id: binding.workspace_id,
            tenant_id: binding.tenant_id,
            scope: binding.scope.clone(),
            provider: "daytona".to_string(),
            provider_account_id: binding.provider_account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("seed live logical workspace");
}

async fn seed_router_capacity(
    pool: &sqlx::PgPool,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'daytona', 'daytona-live', $2,
                  '{"volumes":100,"checkpoints":100,"logical_bytes":1073741824}'::jsonb)
        ON CONFLICT (provider_account_id, generation) DO NOTHING
        "#,
    )
    .bind(provider_account_id)
    .bind(format!("daytona-live-org-{provider_account_id}"))
    .execute(pool)
    .await
    .expect("seed router Daytona provider account");
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits)
        VALUES ($1, '{"volumes":2,"checkpoints":10,"logical_bytes":1073741824}'::jsonb)
        ON CONFLICT (tenant_id) DO NOTHING
        "#,
    )
    .bind(identity().tenant_id)
    .execute(pool)
    .await
    .expect("seed router tenant capacity");
}

async fn seed_router_workspace(pool: &sqlx::PgPool, scope: &SandboxWorkspaceScope) {
    PostgresWorkspaceRepository::new(pool.clone())
        .create(&CreateWorkspaceRequest {
            workspace_id: moa_core::types::identifiers::SandboxWorkspaceId::new(),
            tenant_id: identity().tenant_id,
            scope: scope.clone(),
            provider: "daytona".to_string(),
            provider_account_id: live_provider_account_id(),
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("seed router Daytona logical workspace");
}

async fn persist_live_operation(
    pool: &sqlx::PgPool,
    kind: WorkspaceOperationKind,
    binding: WorkspaceBinding,
) -> WorkspaceStorageOperation {
    let operation_id = moa_core::types::identifiers::WorkspaceOperationId::new();
    let operation = WorkspaceStorageOperation {
        operation_id,
        kind,
        binding,
        deadline: Utc::now() + ChronoDuration::minutes(10),
        request_hash: format!("daytona-live-{operation_id}"),
    };
    PostgresWorkspaceOperationRepository::new(pool.clone())
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id: operation.binding.tenant_id,
            workspace_id: operation.binding.workspace_id,
            provider_account_id: operation.binding.provider_account_id,
            provider_account_generation: 1,
            kind,
            request_hash: operation.request_hash.clone(),
            expected_writer_epoch: i64::try_from(operation.binding.writer_epoch)
                .expect("live writer epoch"),
            expected_instance_generation: i64::try_from(operation.binding.instance_generation)
                .expect("live instance generation"),
            expected_checkpoint_generation: i64::try_from(
                operation
                    .binding
                    .current_revision
                    .as_ref()
                    .map_or(0, |revision| revision.generation),
            )
            .expect("live checkpoint generation"),
            deadline_at: operation.deadline,
            reconcile_not_before: operation.deadline + ChronoDuration::minutes(1),
        })
        .await
        .expect("persist live Daytona operation before provider I/O");
    operation
}

async fn forget_deleted_live_volume(
    pool: &sqlx::PgPool,
    binding: &WorkspaceBinding,
    storage: &ProviderStorageRef,
) {
    sqlx::query(
        "DELETE FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1 AND storage_resource_id IN (SELECT storage_resource_id FROM moa.sandbox_storage_resources WHERE tenant_id = $1 AND provider_reference = $2)",
    )
    .bind(binding.tenant_id)
    .bind(&storage.resource_id)
    .execute(pool)
    .await
    .expect("release deleted live volume fixture reservation");
    sqlx::query(
        "DELETE FROM moa.sandbox_storage_resources WHERE tenant_id = $1 AND provider_reference = $2",
    )
    .bind(binding.tenant_id)
    .bind(&storage.resource_id)
    .execute(pool)
    .await
    .expect("retire deleted live volume fixture row");
}

async fn cleanup_live_workspace_database(
    pool: &sqlx::PgPool,
    binding: &WorkspaceBinding,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1")
        .bind(binding.tenant_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_storage_resources WHERE tenant_id = $1")
        .bind(binding.tenant_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE tenant_id = $1")
        .bind(binding.tenant_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE tenant_id = $1")
        .bind(binding.tenant_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
        .bind(binding.tenant_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(binding.provider_account_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn whole_volume(storage: &ProviderStorageRef) -> ProviderStorageRef {
    let mut whole = storage.clone();
    whole.workspace_locator = None;
    whole
}

async fn cleanup_live_daytona_workspace(
    provider: &DaytonaHandProvider,
    pool: &sqlx::PgPool,
    binding: &WorkspaceBinding,
    handles: &[HandHandle],
    checkpoint: Option<(ProviderStorageRef, WorkspaceBinding)>,
    volumes: &[ProviderStorageRef],
) -> Result<()> {
    let mut first_error = None;
    for handle in handles {
        if let Err(error) = destroy_and_wait(provider, handle).await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some((checkpoint, checkpoint_binding)) = checkpoint
        && let Err(error) = provider
            .delete_workspace_storage(
                moa_core::types::sandbox_workspace::WorkspaceStorageDeleteRequest {
                    operation: WorkspaceStorageOperation {
                        operation_id: moa_core::types::identifiers::WorkspaceOperationId::new(),
                        kind: WorkspaceOperationKind::Delete,
                        binding: checkpoint_binding,
                        deadline: Utc::now() + ChronoDuration::minutes(10),
                        request_hash: format!("daytona-live-checkpoint-delete-{}", Uuid::now_v7()),
                    },
                    storage: checkpoint,
                },
            )
            .await
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    let mut deleted = HashSet::new();
    for storage in volumes {
        if !deleted.insert(storage.resource_id.clone()) {
            continue;
        }
        let already_absent = provider
            .enumerate_account_storage(binding.provider_account_id, 1)
            .await
            .map(|inventory| {
                inventory
                    .resources
                    .iter()
                    .all(|resource| resource.provider_reference != storage.resource_id)
            });
        match already_absent {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
        }
        let delete_result = delete_tenant_volume_with_retry(
            provider,
            moa_core::types::sandbox_workspace::TenantStoragePurgeRequest {
                tenant_id: binding.tenant_id,
                purge_operation_id: format!("daytona-live-cleanup-{}", Uuid::now_v7()),
                provider_account_id: binding.provider_account_id,
                provider_account_generation: binding.provider_account_generation,
                storage: whole_volume(storage),
            },
            Duration::from_secs(60),
        )
        .await;
        if let Err(error) = delete_result
            && first_error.is_none()
        {
            first_error = Some(error);
            continue;
        }
        if let Err(error) = wait_for_volume_absent(
            provider,
            binding.provider_account_id,
            &storage.resource_id,
            Duration::from_secs(60),
        )
        .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Err(error) = cleanup_live_workspace_database(pool, binding).await
        && first_error.is_none()
    {
        first_error = Some(MoaError::StorageError(format!(
            "clean live Daytona database fixture: {error}"
        )));
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn delete_tenant_volume_with_retry(
    provider: &DaytonaHandProvider,
    request: moa_core::types::sandbox_workspace::TenantStoragePurgeRequest,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        match provider
            .delete_tenant_storage_resource(request.clone())
            .await
        {
            Ok(_) => return Ok(()),
            Err(MoaError::HttpStatus { status: 409, .. }) if started.elapsed() < timeout => {
                sleep(Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn wait_for_destroyed(
    provider: &DaytonaHandProvider,
    handle: &HandHandle,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(
                "timed out waiting for Daytona sandbox destruction".to_string(),
            ));
        }
        if matches!(provider.status(handle).await?, HandStatus::Destroyed) {
            return Ok(());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn wait_for_status(
    provider: &DaytonaHandProvider,
    handle: &HandHandle,
    expected: &[HandStatus],
    timeout: Duration,
) -> Result<HandStatus> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for Daytona status {expected:?}"
            )));
        }
        let status = provider.status(handle).await?;
        if expected.contains(&status) {
            return Ok(status);
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn destroy_and_wait(provider: &DaytonaHandProvider, handle: &HandHandle) -> Result<()> {
    provider.destroy(handle).await?;
    wait_for_destroyed(provider, handle, Duration::from_secs(30)).await
}

/// Waits until a durable provisioning operation resolves to no live sandbox.
///
/// The list API is only bounded-consistent after a destroy, so a destroyed
/// sandbox is allowed to linger in the label-filtered listing briefly.
async fn wait_for_no_provisioned_hands(
    provider: &DaytonaHandProvider,
    operation_id: moa_core::types::identifiers::HandProvisioningOperationId,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let discovered = provider
            .provisioned_hands(live_provider_account_id(), 1, operation_id)
            .await?;
        if discovered.is_empty() {
            return Ok(());
        }
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(format!(
                "durable provisioning operation `{operation_id}` still resolves to {discovered:?}"
            )));
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn wait_for_volume_absent(
    provider: &DaytonaHandProvider,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
    volume_id: &str,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let inventory = provider
            .enumerate_account_storage(provider_account_id, 1)
            .await?;
        if inventory
            .resources
            .iter()
            .all(|resource| resource.provider_reference != volume_id)
        {
            return Ok(());
        }
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(format!(
                "Daytona live volume `{volume_id}` remained in provider inventory"
            )));
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1 and DAYTONA_API_KEY"]
async fn daytona_provider_round_trip() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let provider = live_provider();

    let unsupported = provider
        .provision(live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM))
        .await;
    assert!(matches!(unsupported, Err(MoaError::Unsupported(_))));

    let handle = provider
        .provision(live_hand_spec(
            moa_core::types::hands::SandboxTier::Container,
        ))
        .await
        .expect("failed to provision Daytona sandbox");

    let file_path = format!("tmp/moa-daytona-live-{}.txt", Uuid::now_v7().simple());
    let marker = format!("marker-{}", Uuid::now_v7().simple());

    let result = AssertUnwindSafe(async {
        let status = provider.status(&handle).await?;
        assert!(
            matches!(
                status,
                HandStatus::Provisioning
                    | HandStatus::Running
                    | HandStatus::Stopped
                    | HandStatus::Paused
            ),
            "unexpected initial status: {status:?}"
        );

        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": format!("sh -lc 'printf {marker}'"),
                    "timeout_secs": 60_u64
                })
                .to_string(),
            )
            .await?;
        assert_eq!(
            bash.process_exit_code(),
            Some(0),
            "bash stderr: {}",
            bash.process_stderr().unwrap_or_default()
        );
        assert!(
            bash.process_stdout().unwrap_or_default().contains(&marker),
            "bash output missing marker: {}",
            bash.to_text()
        );

        let write = provider
            .execute(
                &handle,
                "file_write",
                &json!({ "path": file_path, "content": marker }).to_string(),
            )
            .await?;
        assert_eq!(
            write.to_text(),
            format!("[new file created: {file_path}, 1 lines]")
        );

        let read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert_eq!(read.to_text(), marker);

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": file_path.rsplit('/').next().unwrap_or_default() }).to_string(),
            )
            .await?;
        assert!(!search.is_error);
        assert!(
            search.to_text().contains(&file_path),
            "search output missing path: {}",
            search.to_text()
        );

        provider.pause(&handle).await?;
        let _ = wait_for_status(
            &provider,
            &handle,
            &[HandStatus::Stopped, HandStatus::Paused],
            Duration::from_secs(60),
        )
        .await?;
        let resumed_read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert_eq!(resumed_read.to_text(), marker);

        let unsupported_tool = provider
            .execute(
                &handle,
                "web_search",
                &json!({ "query": "test" }).to_string(),
            )
            .await;
        assert!(matches!(unsupported_tool, Err(MoaError::ToolError(_))));

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = destroy_and_wait(&provider, &handle).await;
    let second_destroy = provider.destroy(&handle).await;

    match result {
        Ok(Ok(())) => {
            cleanup_result.expect("sandbox cleanup should succeed");
            assert!(
                second_destroy.is_ok(),
                "destroy should be idempotent, got: {second_destroy:?}"
            );
        }
        Ok(Err(error)) => {
            cleanup_result.expect("sandbox cleanup should succeed after provider failure");
            panic!("live Daytona provider test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

// Pins: a live sandbox created under a durable provisioning operation ID is
// discoverable by that ID through Daytona's real label-filtered list API;
// re-provisioning the same operation resolves to the same sandbox by its
// deterministic name instead of leaking a second one; an unrelated operation ID
// resolves to nothing; and a destroyed sandbox leaves the operation with no live
// resource. This is the crash-window recovery contract, and only the live API
// can prove that the label filter and `nextCursor` paging shape are real.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1 and DAYTONA_API_KEY"]
async fn daytona_provisioning_operation_is_discoverable_and_idempotent() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let provider = live_provider();
    let spec = live_hand_spec(moa_core::types::hands::SandboxTier::Container);
    let operation_id = spec.provisioning_operation_id;

    let handle = provider
        .provision(spec.clone())
        .await
        .expect("failed to provision Daytona sandbox");

    let result = AssertUnwindSafe(async {
        let discovered = provider
            .provisioned_hands(live_provider_account_id(), 1, operation_id)
            .await?;
        assert_eq!(
            discovered,
            vec![handle.clone()],
            "the durable operation must resolve to exactly the sandbox it created"
        );

        // Resolve-before-create must return the live sandbox, so a retry after a
        // crash between provider create and durable handle persistence cannot
        // strand a second sandbox under the same operation.
        let reprovisioned = provider.provision(spec.clone()).await?;
        assert_eq!(
            reprovisioned, handle,
            "re-provisioning one operation must resolve to its existing sandbox"
        );
        assert_eq!(
            provider
                .provisioned_hands(live_provider_account_id(), 1, operation_id)
                .await?,
            vec![handle.clone()],
            "re-provisioning must not create a second sandbox for the operation"
        );

        // An unrelated operation must resolve to nothing. If Daytona ignored or
        // misparsed the label filter, the live sandbox above would appear here,
        // so this is what makes the positive match meaningful.
        let unrelated = provider
            .provisioned_hands(
                live_provider_account_id(),
                1,
                moa_core::types::identifiers::HandProvisioningOperationId::new(),
            )
            .await?;
        assert!(
            unrelated.is_empty(),
            "an unrelated provisioning operation resolved to {unrelated:?}"
        );

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = destroy_and_wait(&provider, &handle).await;

    match result {
        Ok(Ok(())) => {
            cleanup_result.expect("sandbox cleanup should succeed");
            wait_for_no_provisioned_hands(&provider, operation_id, Duration::from_secs(60))
                .await
                .expect("a destroyed sandbox must leave the operation with no live resource");
        }
        Ok(Err(error)) => {
            cleanup_result.expect("sandbox cleanup should succeed after provider failure");
            panic!("live Daytona provisioning operation test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

// Pins: Daytona durability is owned by the tenant volume rather than one
// sandbox. A committed marker must remain visible after the source sandbox is
// destroyed and the exact volume subpath is mounted into different compute.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1, DAYTONA_API_KEY, and fresh V58 MOA_DATABASE_URL"]
async fn daytona_volume_workspace_survives_compute_replacement_live() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&live_database_url())
        .await
        .expect("live Daytona Postgres should be reachable");
    let account_id = moa_core::types::identifiers::ProviderAccountId::new();
    let binding = live_workspace_binding_for_account("daytona-live-volume", account_id);
    seed_live_workspace(&pool, &binding).await;
    let provider = live_storage_provider(&pool, account_id);
    let mut handles = Vec::new();
    let mut volume = None;
    let mut checkpoint_cleanup = None;

    let result = AssertUnwindSafe(async {
        let prepared = provider
            .prepare_workspace_storage(
                moa_core::types::sandbox_workspace::WorkspaceStoragePrepareRequest {
                    operation: persist_live_operation(
                        &pool,
                        WorkspaceOperationKind::Create,
                        binding.clone(),
                    )
                    .await,
                },
            )
            .await?;
        let mutable_storage = prepared
            .storage
            .expect("Daytona must return the prepared tenant volume");
        volume = Some(mutable_storage.clone());

        let source = provider
            .provision(live_hand_spec_with_binding(binding.clone()))
            .await?;
        handles.push(source.clone());
        provider
            .attach_workspace(moa_core::types::sandbox_workspace::WorkspaceAttachRequest {
                operation: persist_live_operation(
                    &pool,
                    WorkspaceOperationKind::Attach,
                    binding.clone(),
                )
                .await,
                hand: source.clone(),
                storage: Some(mutable_storage.clone()),
            })
            .await?;

        let marker = format!("daytona-volume-{}", Uuid::now_v7().simple());
        let write = provider
            .execute(
                &source,
                "bash",
                &json!({"cmd": format!("printf '%s' '{marker}' > /workspace/marker.txt")})
                    .to_string(),
            )
            .await?;
        assert_eq!(write.process_exit_code(), Some(0));
        let commit = provider
            .publish_workspace_checkpoint(
                moa_core::types::sandbox_workspace::WorkspaceCheckpointPublishRequest {
                    operation: persist_live_operation(
                        &pool,
                        WorkspaceOperationKind::Commit,
                        binding.clone(),
                    )
                    .await,
                    hand: source.clone(),
                    parent_revision: None,
                },
            )
            .await?;
        let publication = commit
            .checkpoint_publication
            .expect("Daytona commit must publish a portable checkpoint");
        let mut committed_binding = binding.clone();
        committed_binding.current_revision = Some(publication.revision.clone());
        checkpoint_cleanup = Some((publication.storage, committed_binding));

        destroy_and_wait(&provider, &source).await?;

        let replacement = provider
            .provision(live_hand_spec_with_binding(binding.clone()))
            .await?;
        assert_ne!(replacement, source, "replacement must be fresh compute");
        handles.push(replacement.clone());
        provider
            .attach_workspace(moa_core::types::sandbox_workspace::WorkspaceAttachRequest {
                operation: persist_live_operation(
                    &pool,
                    WorkspaceOperationKind::Attach,
                    binding.clone(),
                )
                .await,
                hand: replacement.clone(),
                storage: Some(mutable_storage),
            })
            .await?;
        let read = provider
            .execute(
                &replacement,
                "bash",
                &json!({"cmd": "cat /workspace/marker.txt"}).to_string(),
            )
            .await?;
        assert_eq!(read.process_exit_code(), Some(0));
        assert_eq!(read.process_stdout().map(str::trim), Some(marker.as_str()));
        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let volumes = volume.into_iter().collect::<Vec<_>>();
    let cleanup = cleanup_live_daytona_workspace(
        &provider,
        &pool,
        &binding,
        &handles,
        checkpoint_cleanup,
        &volumes,
    )
    .await;
    match result {
        Ok(Ok(())) => cleanup.expect("Daytona volume replacement cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup.expect("Daytona volume replacement cleanup should succeed after error");
            panic!("live Daytona volume replacement failed: {error}");
        }
        Err(panic) => {
            cleanup.expect("Daytona volume replacement cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

// Pins: a portable checkpoint is the recovery authority after loss of the
// tenant volume. Restore must target a newly created volume mounted into fresh
// compute, and cleanup must prove every volume allocated by this case absent.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1, DAYTONA_API_KEY, and fresh V58 MOA_DATABASE_URL"]
async fn daytona_workspace_restores_after_tenant_volume_replacement_live() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&live_database_url())
        .await
        .expect("live Daytona Postgres should be reachable");
    let account_id = moa_core::types::identifiers::ProviderAccountId::new();
    let binding = live_workspace_binding_for_account("daytona-live-restore", account_id);
    seed_live_workspace(&pool, &binding).await;
    let provider = live_storage_provider(&pool, account_id);
    let mut handles = Vec::new();
    let mut volumes = Vec::new();
    let mut checkpoint_cleanup = None;

    let result = AssertUnwindSafe(async {
        let prepared = provider
            .prepare_workspace_storage(
                moa_core::types::sandbox_workspace::WorkspaceStoragePrepareRequest {
                    operation: persist_live_operation(
                        &pool,
                        WorkspaceOperationKind::Create,
                        binding.clone(),
                    )
                    .await,
                },
            )
            .await?;
        let original_volume = prepared.storage.expect("original Daytona tenant volume");
        volumes.push(original_volume.clone());
        let source = provider
            .provision(live_hand_spec_with_binding(binding.clone()))
            .await?;
        handles.push(source.clone());
        provider
            .attach_workspace(moa_core::types::sandbox_workspace::WorkspaceAttachRequest {
                operation: persist_live_operation(
                    &pool,
                    WorkspaceOperationKind::Attach,
                    binding.clone(),
                )
                .await,
                hand: source.clone(),
                storage: Some(original_volume.clone()),
            })
            .await?;

        let marker = format!("daytona-restore-{}", Uuid::now_v7().simple());
        let write = provider
            .execute(
                &source,
                "bash",
                &json!({"cmd": format!("printf '%s' '{marker}' > /workspace/marker.txt")})
                    .to_string(),
            )
            .await?;
        assert_eq!(write.process_exit_code(), Some(0));
        let publication = provider
            .publish_workspace_checkpoint(
                moa_core::types::sandbox_workspace::WorkspaceCheckpointPublishRequest {
                    operation: persist_live_operation(
                        &pool,
                        WorkspaceOperationKind::Commit,
                        binding.clone(),
                    )
                    .await,
                    hand: source.clone(),
                    parent_revision: None,
                },
            )
            .await?
            .checkpoint_publication
            .expect("Daytona commit must publish a portable checkpoint");
        let mut restored_binding = binding.clone();
        restored_binding.current_revision = Some(publication.revision.clone());
        checkpoint_cleanup = Some((publication.storage.clone(), restored_binding.clone()));

        destroy_and_wait(&provider, &source).await?;
        delete_tenant_volume_with_retry(
            &provider,
            moa_core::types::sandbox_workspace::TenantStoragePurgeRequest {
                tenant_id: binding.tenant_id,
                purge_operation_id: format!("daytona-live-replace-{}", Uuid::now_v7()),
                provider_account_id: account_id,
                provider_account_generation: 1,
                storage: whole_volume(&original_volume),
            },
            Duration::from_secs(60),
        )
        .await?;
        wait_for_volume_absent(
            &provider,
            account_id,
            &original_volume.resource_id,
            Duration::from_secs(60),
        )
        .await?;

        // The production purge coordinator normally retires the durable row
        // after its provider-absence proof. This live fixture performs that
        // already-proven bookkeeping directly so it can exercise replacement
        // within one test without deleting the logical tenant.
        forget_deleted_live_volume(&pool, &binding, &original_volume).await;
        let replacement = provider
            .prepare_workspace_storage(
                moa_core::types::sandbox_workspace::WorkspaceStoragePrepareRequest {
                    operation: persist_live_operation(
                        &pool,
                        WorkspaceOperationKind::Create,
                        binding.clone(),
                    )
                    .await,
                },
            )
            .await?
            .storage
            .expect("replacement Daytona tenant volume");
        assert_ne!(replacement.resource_id, original_volume.resource_id);
        volumes.push(replacement.clone());

        let fresh = provider
            .provision(live_hand_spec_with_binding(binding.clone()))
            .await?;
        assert_ne!(fresh, source, "restore must use fresh compute");
        handles.push(fresh.clone());
        provider
            .restore_workspace(
                moa_core::types::sandbox_workspace::WorkspaceRestoreRequest {
                    operation: WorkspaceStorageOperation {
                        operation_id: moa_core::types::identifiers::WorkspaceOperationId::new(),
                        kind: WorkspaceOperationKind::Restore,
                        binding: restored_binding,
                        deadline: Utc::now() + ChronoDuration::minutes(10),
                        request_hash: format!("daytona-live-restore-{}", Uuid::now_v7()),
                    },
                    hand: fresh.clone(),
                    revision: publication.revision,
                    checkpoint: publication.storage,
                },
            )
            .await?;
        let read = provider
            .execute(
                &fresh,
                "bash",
                &json!({"cmd": "cat /workspace/marker.txt"}).to_string(),
            )
            .await?;
        assert_eq!(read.process_exit_code(), Some(0));
        assert_eq!(read.process_stdout().map(str::trim), Some(marker.as_str()));
        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup = cleanup_live_daytona_workspace(
        &provider,
        &pool,
        &binding,
        &handles,
        checkpoint_cleanup,
        &volumes,
    )
    .await;
    match result {
        Ok(Ok(())) => cleanup.expect("Daytona restore cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup.expect("Daytona restore cleanup should succeed after error");
            panic!("live Daytona volume-loss restore failed: {error}");
        }
        Err(panic) => {
            cleanup.expect("Daytona restore cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1, DAYTONA_API_KEY, and fresh V58 MOA_DATABASE_URL"]
async fn daytona_router_reuses_and_isolates() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let mut config = live_config();
    let temp = tempdir().expect("tempdir");
    config.local.sandbox_dir = temp.path().join("sandbox").display().to_string();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&live_database_url())
        .await
        .expect("live Daytona Postgres should be reachable");
    let session_one = session("one");
    let session_two = session("two");
    let session_store =
        moa_session::PostgresSessionStore::from_existing_pool_with_config(&config, pool.clone())
            .await
            .expect("live Daytona session store");
    seed_router_capacity(&pool, live_provider_account_id()).await;
    for session in [&session_one, &session_two] {
        let created = session_store
            .create_session(session.clone())
            .await
            .expect("seed live Daytona router session");
        assert_eq!(created, session.id);
        seed_router_workspace(&pool, &router_workspace_scope(session)).await;
    }
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());
    let checkpoint_store = Arc::new(
        CheckpointObjectStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            Arc::clone(&kms),
            "daytona-live-router-checkpoints",
            ArchiveLimits::default(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("router checkpoint store"),
    );

    let router = ToolRouter::from_config_with_checkpoint_store(
        &config,
        None,
        None,
        Some(checkpoint_store),
        Some(pool.clone()),
        Some(kms),
        true,
    )
    .await
    .expect("router should load Daytona from config")
    .with_hand_lease_store(Arc::new(
        moa_hands::core::leases::PostgresHandLeaseStore::new(pool.clone()),
    ))
    .with_hand_lease_reaper();
    let provider = DaytonaHandProvider::new(Arc::new(
        FileProviderCredentialSource::from_config(
            config.cloud.hands.as_ref().expect("cloud hands config"),
        )
        .expect("provider credential source from config"),
    ));

    let file_one = format!("tmp/moa-router-one-{}.txt", Uuid::now_v7().simple());
    let file_two = format!("tmp/moa-router-two-{}.txt", Uuid::now_v7().simple());
    let content_one = format!("router-one-{}", Uuid::now_v7().simple());
    let content_two = format!("router-two-{}", Uuid::now_v7().simple());

    let handle_one_id = {
        let secured = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_one)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_one, "content": content_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await
            .expect("first router write should provision a hand");
        let hand_id = secured.hand_id.clone();
        let write = secured.safe_output;
        assert_eq!(
            write.to_text(),
            format!("[new file created: {file_one}, 1 lines]")
        );
        hand_id.expect("cloud hand execution should return a hand id")
    };

    let handle_one = HandHandle::daytona(handle_one_id.clone(), live_provider_account_id(), 1);
    let mut handle_two: Option<HandHandle> = None;
    let test_result = AssertUnwindSafe(async {
        let secured_2 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_one)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;
        let same_hand_id = secured_2.hand_id.clone();
        let read = secured_2.safe_output;
        assert_eq!(same_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert_eq!(read.to_text(), content_one);

        provider.pause(&handle_one).await?;
        let _ = wait_for_status(
            &provider,
            &handle_one,
            &[HandStatus::Stopped, HandStatus::Paused],
            Duration::from_secs(60),
        )
        .await?;
        let secured_3 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_one)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;
        let resumed_hand_id = secured_3.hand_id.clone();
        let resumed_read = secured_3.safe_output;
        assert_eq!(resumed_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert_eq!(resumed_read.to_text(), content_one);

        let secured_4 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_two)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_two, "content": content_two }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;

        let hand_two_id = secured_4.hand_id.clone();

        let second_write = secured_4.safe_output;
        assert_eq!(
            second_write.to_text(),
            format!("[new file created: {file_two}, 1 lines]")
        );
        let hand_two_id = hand_two_id.expect("second session should receive a distinct hand");
        assert_ne!(hand_two_id, handle_one_id);
        handle_two = Some(HandHandle::daytona(
            hand_two_id.clone(),
            live_provider_account_id(),
            1,
        ));

        let secured_5 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_two)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": "sh -lc 'printf router-bash'", "timeout_secs": 60 }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;

        let bash = secured_5.safe_output;
        assert_eq!(bash.process_exit_code(), Some(0));
        assert!(bash.to_text().contains("router-bash"));

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = async {
        if let Some(handle_two) = &handle_two {
            destroy_and_wait(&provider, handle_two).await?;
        }
        destroy_and_wait(&provider, &handle_one).await
    }
    .await;

    match test_result {
        Ok(Ok(())) => cleanup_result.expect("router cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup_result.expect("router cleanup should succeed after provider failure");
            panic!("live Daytona router test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("router cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

/// A live-provider hand spec: unrestricted egress with a 5-minute idle window
/// inside a 10-minute hard lifetime, which is what both cloud providers can
/// actually enforce.
fn live_hand_spec(tier: moa_core::types::hands::SandboxTier) -> HandSpec {
    use moa_core::types::hands::{
        BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit,
        SandboxPolicySnapshot, SandboxProfile, resolve_effective_sandbox_profile,
    };

    let seconds = |value: u64| LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(value).expect("nonzero seconds"),
    };
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::Unrestricted,
        seconds(300),
        seconds(600),
    )
    .expect("live profile should validate");
    let effective_profile = resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("live-deployment", profile).expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "live-capabilities-v1",
    )
    .expect("live policy resolution should succeed");
    HandSpec {
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        workspace: live_workspace_binding("daytona-live-worker"),
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        filesystem: moa_core::types::sandbox_workspace::SandboxFilesystemLayout::standard(),
        effective_profile,
    }
}

fn live_hand_spec_with_binding(binding: WorkspaceBinding) -> HandSpec {
    let mut spec = live_hand_spec(moa_core::types::hands::SandboxTier::Container);
    spec.workspace = binding;
    spec
}

fn live_workspace_binding_for_account(
    worker_id: &str,
    provider_account_id: moa_core::types::identifiers::ProviderAccountId,
) -> WorkspaceBinding {
    WorkspaceBinding {
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        scope: moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
            session_id: moa_core::types::identifiers::SessionId::new(),
            worker_id: worker_id.to_string(),
        },
        workspace_id: moa_core::types::identifiers::SandboxWorkspaceId::new(),
        provider_account_id,
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        writer_epoch: 0,
        instance_generation: 0,
        current_revision: None,
    }
}

fn live_workspace_binding(worker_id: &str) -> moa_core::types::sandbox_workspace::WorkspaceBinding {
    moa_core::types::sandbox_workspace::WorkspaceBinding {
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        scope: moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
            session_id: moa_core::types::identifiers::SessionId::new(),
            worker_id: worker_id.to_string(),
        },
        workspace_id: moa_core::types::identifiers::SandboxWorkspaceId::new(),
        provider_account_id: live_provider_account_id(),
        provider_account_generation: 1,
        durability_class: moa_core::types::sandbox_workspace::DurabilityClass::PortableFilesystem,
        writer_epoch: 1,
        instance_generation: 1,
        current_revision: None,
    }
}

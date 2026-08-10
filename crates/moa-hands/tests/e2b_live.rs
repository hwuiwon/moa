// No offline counterpart possible because: this live file verifies real E2B sandbox provisioning, lifecycle, and filesystem isolation semantics that a local HTTP mock cannot emulate.

//! Live E2B integration tests.
//!
//! These tests are ignored by default because they provision real E2B sandboxes
//! and require valid credentials in the environment.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;
use std::time::Duration;
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use futures_util::FutureExt;
use moa_config::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig, MoaConfig,
    ProviderSecretFileSelector, SandboxProfileConfig,
};
use moa_core::types::identifiers::ToolCallId;
use moa_core::{
    error::MoaError,
    error::Result,
    traits::{HandProvider, Identity, IdentityType, SandboxStorageProvider, SessionStore},
    types::completion::ToolInvocation,
    types::hands::{HandHandle, HandSpec, HandStatus},
    types::identifiers::TenantId,
    types::sandbox_workspace::SandboxWorkspaceScope,
    types::session::SessionMeta,
};
use moa_hands::{E2BHandProvider, FileProviderCredentialSource, ToolRouter};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tempfile::tempdir;
use tokio::time::{Instant, sleep};
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
        worker_id: "e2b-live-router-worker".to_string(),
    }
}

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c341),
        tenant_id: TenantId::from(Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c342)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn live_provider() -> E2BHandProvider {
    let config = live_config();
    E2BHandProvider::new(Arc::new(
        FileProviderCredentialSource::from_config(
            config.cloud.hands.as_ref().expect("cloud hands config"),
        )
        .expect("failed to build E2B credential source"),
    ))
}

fn live_e2b_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_E2B_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_e2b_credentials() {
    assert!(
        std::env::var("E2B_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        "MOA_RUN_LIVE_E2B_TESTS=1 requires E2B_API_KEY"
    );
}

fn live_config() -> MoaConfig {
    let credential_dir = tempdir().expect("E2B credential tempdir").keep();
    let credential_path = credential_dir.join("e2b");
    std::fs::write(
        &credential_path,
        std::env::var("E2B_API_KEY").expect("E2B_API_KEY"),
    )
    .expect("write E2B credential file");
    std::fs::set_permissions(&credential_path, std::fs::Permissions::from_mode(0o400))
        .expect("chmod E2B credential file");
    let owner_uid = std::fs::metadata(&credential_path)
        .expect("E2B credential metadata")
        .uid();
    let mut config = MoaConfig::default();
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        provider_accounts: vec![CloudHandProviderAccountConfig {
            provider_account_id: live_provider_account_id(),
            generation: 1,
            provider: CloudHandProviderKind::E2b,
            isolation_cell: "e2b-live".to_string(),
            api_origin: std::env::var("E2B_API_ORIGIN")
                .unwrap_or_else(|_| "https://api.e2b.dev".to_string()),
            toolbox_origin: None,
            sandbox_domain: Some(
                std::env::var("E2B_DOMAIN").unwrap_or_else(|_| "e2b.app".to_string()),
            ),
            default_runtime: Some(
                std::env::var("E2B_TEMPLATE").unwrap_or_else(|_| "base".to_string()),
            ),
            project_fingerprint: None,
            credential: ProviderSecretFileSelector {
                path: credential_path,
                owner_uid,
            },
        }],
        ..CloudHandsConfig::default()
    });
    config.sandbox_policy.deployment = live_sandbox_profile_config();
    config
}

fn live_provider_account_id() -> moa_core::types::identifiers::ProviderAccountId {
    moa_core::types::identifiers::ProviderAccountId(Uuid::new_v5(&Uuid::NAMESPACE_URL, b"e2b"))
}

fn live_database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .expect("MOA_RUN_LIVE_E2B_TESTS=1 requires a fresh V58 MOA_DATABASE_URL")
}

fn live_checkpoint_store(
    namespace: &str,
) -> Arc<moa_hands::core::sandbox_workspace::checkpoint::store::CheckpointObjectStore> {
    Arc::new(
        moa_hands::core::sandbox_workspace::checkpoint::store::CheckpointObjectStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            Arc::new(moa_crypto::LocalKmsProvider::new()),
            namespace,
            moa_hands::core::sandbox_workspace::checkpoint::archive::ArchiveLimits::default(),
            moa_hands::core::sandbox_workspace::checkpoint::store::ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("live checkpoint store"),
    )
}

async fn seed_router_capacity(pool: &sqlx::PgPool) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'e2b', 'e2b-live', 'e2b-live-project',
                  '{"volumes":0,"checkpoints":100,"logical_bytes":1073741824}'::jsonb)
        ON CONFLICT (provider_account_id, generation) DO NOTHING
        "#,
    )
    .bind(live_provider_account_id())
    .execute(pool)
    .await
    .expect("seed router E2B provider account");
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits)
        VALUES ($1, '{"volumes":0,"checkpoints":10,"logical_bytes":1073741824}'::jsonb)
        ON CONFLICT (tenant_id) DO NOTHING
        "#,
    )
    .bind(identity().tenant_id)
    .execute(pool)
    .await
    .expect("seed router tenant capacity");
}

async fn seed_router_workspace(pool: &sqlx::PgPool, scope: &SandboxWorkspaceScope) {
    use moa_core::types::{identifiers::SandboxWorkspaceId, sandbox_workspace::DurabilityClass};
    use moa_hands::core::sandbox_workspace::{
        model::CreateWorkspaceRequest, repository::PostgresWorkspaceRepository,
    };

    PostgresWorkspaceRepository::new(pool.clone())
        .create(&CreateWorkspaceRequest {
            workspace_id: SandboxWorkspaceId::new(),
            tenant_id: identity().tenant_id,
            scope: scope.clone(),
            provider: "e2b".to_string(),
            provider_account_id: live_provider_account_id(),
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("seed router E2B logical workspace");
}

fn live_sandbox_profile_config() -> SandboxProfileConfig {
    use moa_core::types::hands::{CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit};

    let seconds = |value: u64| LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(value).expect("nonzero seconds"),
    };
    SandboxProfileConfig {
        revision: "e2b-live-sandbox-v1".to_string(),
        cpu: CpuLimit::Unbounded,
        memory: MemoryLimit::Unbounded,
        ephemeral_disk: DiskLimit::Unbounded,
        egress: EgressPolicy::Unrestricted,
        idle_timeout: seconds(300),
        max_lifetime: seconds(600),
    }
}

async fn wait_for_destroyed(
    provider: &E2BHandProvider,
    handle: &HandHandle,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(
                "timed out waiting for E2B sandbox destruction".to_string(),
            ));
        }
        if matches!(provider.status(handle).await?, HandStatus::Destroyed) {
            return Ok(());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn destroy_and_wait(provider: &E2BHandProvider, handle: &HandHandle) -> Result<()> {
    provider.destroy(handle).await?;
    wait_for_destroyed(provider, handle, Duration::from_secs(30)).await
}

/// Waits until a durable provisioning operation resolves to no live sandbox.
///
/// The list API is only bounded-consistent after a destroy, so a destroyed
/// sandbox is allowed to linger in the metadata-filtered listing briefly.
async fn wait_for_no_provisioned_hands(
    provider: &E2BHandProvider,
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

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_provider_round_trip() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let provider = live_provider();

    let unsupported = provider
        .provision(live_hand_spec(
            moa_core::types::hands::SandboxTier::Container,
        ))
        .await;
    assert!(matches!(unsupported, Err(MoaError::Unsupported(_))));

    let handle = provider
        .provision(live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM))
        .await
        .expect("failed to provision E2B sandbox");

    let file_path = format!("tmp/moa-e2b-live-{}.txt", Uuid::now_v7().simple());
    let marker = format!("marker-{}", Uuid::now_v7().simple());

    let result = AssertUnwindSafe(async {
        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": format!("printf {marker}"),
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

        let failing = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": "printf live-out; printf live-err >&2; exit 7",
                    "timeout_secs": 60_u64
                })
                .to_string(),
            )
            .await?;
        assert_eq!(failing.process_exit_code(), Some(7));
        assert!(
            failing
                .process_stdout()
                .unwrap_or_default()
                .contains("live-out")
        );
        assert!(
            failing
                .process_stderr()
                .unwrap_or_default()
                .contains("live-err")
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
        assert!(
            read.to_text().contains(&marker),
            "read output missing marker: {}",
            read.to_text()
        );

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": file_path.rsplit('/').next().unwrap_or_default() }).to_string(),
            )
            .await?;
        assert_eq!(search.process_exit_code(), Some(0));
        assert!(
            search.to_text().contains(&file_path)
                || search
                    .to_text()
                    .contains(file_path.rsplit('/').next().unwrap_or_default()),
            "search output missing path: {}",
            search.to_text()
        );

        assert!(matches!(
            provider.pause(&handle).await,
            Err(MoaError::Unsupported(_))
        ));
        assert!(matches!(
            provider.resume(&handle).await,
            Err(MoaError::Unsupported(_))
        ));
        let subsequent_read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert!(subsequent_read.to_text().contains(&marker));

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
            panic!("live E2B provider test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

// Pins: a live sandbox created under a durable provisioning operation ID is
// discoverable by that ID through E2B's real metadata-filtered list API;
// re-provisioning the same operation resolves to the same sandbox instead of
// leaking a second one; an unrelated operation ID resolves to nothing; and a
// destroyed sandbox leaves the operation with no live resource. This is the
// crash-window recovery contract, and only the live API can prove that the
// metadata filter is real rather than silently ignored.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_provisioning_operation_is_discoverable_and_idempotent() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let provider = live_provider();
    let spec = live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM);
    let operation_id = spec.provisioning_operation_id;

    let handle = provider
        .provision(spec.clone())
        .await
        .expect("failed to provision E2B sandbox");

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

        // An unrelated operation must resolve to nothing. If E2B ignored or
        // misparsed the metadata filter, the live sandbox above would appear
        // here, so this is what makes the positive match meaningful.
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
            panic!("live E2B provisioning operation test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

// Pins: E2B durability comes only from a verified portable filesystem
// checkpoint restored into a different fresh sandbox; killing the source must
// not destroy the committed marker or reintroduce process-memory persistence.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_workspace_restores_into_fresh_compute_live() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let config = live_config();
    let checkpoint_store = live_checkpoint_store("e2b-live-workspace-checkpoints");
    let provider = E2BHandProvider::new(Arc::new(
        FileProviderCredentialSource::from_config(
            config.cloud.hands.as_ref().expect("cloud hands config"),
        )
        .expect("E2B credential source"),
    ))
    .with_checkpoint_store(Arc::clone(&checkpoint_store));

    let source_binding = live_workspace_binding("e2b-live-checkpoint-source");
    let source_spec = live_hand_spec_with_binding(source_binding.clone());
    let source_operation_id = source_spec.provisioning_operation_id;
    let source = provider
        .provision(source_spec)
        .await
        .expect("provision source E2B sandbox");
    let mut fresh = None;
    let mut fresh_operation_id = None;
    let mut checkpoint_cleanup = None;
    let result = AssertUnwindSafe(async {
        let attach = live_workspace_operation(
            moa_core::types::sandbox_workspace::WorkspaceOperationKind::Attach,
            source_binding.clone(),
        );
        provider
            .attach_workspace(moa_core::types::sandbox_workspace::WorkspaceAttachRequest {
                operation: attach,
                hand: source.clone(),
                storage: None,
            })
            .await
            .expect("attach fresh E2B data root");

        let marker = format!("e2b-portable-{}", Uuid::now_v7().simple());
        let marker_path = "/workspace/tmp/marker.txt";
        provider
            .execute(
                &source,
                "file_write",
                &json!({"path": marker_path, "content": marker}).to_string(),
            )
            .await
            .expect("write source marker");
        let commit = provider
            .publish_workspace_checkpoint(
                moa_core::types::sandbox_workspace::WorkspaceCheckpointPublishRequest {
                    operation: live_workspace_operation(
                        moa_core::types::sandbox_workspace::WorkspaceOperationKind::Commit,
                        source_binding.clone(),
                    ),
                    hand: source.clone(),
                    parent_revision: source_binding.current_revision.clone(),
                },
            )
            .await
            .expect("commit portable E2B checkpoint");
        wait_for_destroyed(&provider, &source, Duration::from_secs(30))
            .await
            .expect("source sandbox should be killed after commit");
        let publication = commit
            .checkpoint_publication
            .expect("portable checkpoint publication");
        let checkpoint = publication.storage;
        let revision = publication.revision;

        let mut fresh_binding = source_binding.clone();
        fresh_binding.instance_generation += 1;
        fresh_binding.current_revision = Some(revision.clone());
        checkpoint_cleanup = Some((checkpoint.clone(), fresh_binding.clone()));
        let fresh_spec = live_hand_spec_with_binding(fresh_binding.clone());
        fresh_operation_id = Some(fresh_spec.provisioning_operation_id);
        let fresh_handle = provider
            .provision(fresh_spec)
            .await
            .expect("provision fresh E2B sandbox");
        fresh = Some(fresh_handle.clone());
        assert_ne!(fresh_handle, source, "restore must use fresh compute");
        provider
            .attach_workspace(moa_core::types::sandbox_workspace::WorkspaceAttachRequest {
                operation: live_workspace_operation(
                    moa_core::types::sandbox_workspace::WorkspaceOperationKind::Attach,
                    fresh_binding.clone(),
                ),
                hand: fresh_handle.clone(),
                storage: None,
            })
            .await
            .expect("attach fresh restore root");
        provider
            .restore_workspace(
                moa_core::types::sandbox_workspace::WorkspaceRestoreRequest {
                    operation: live_workspace_operation(
                        moa_core::types::sandbox_workspace::WorkspaceOperationKind::Restore,
                        fresh_binding,
                    ),
                    hand: fresh_handle.clone(),
                    revision,
                    checkpoint,
                },
            )
            .await
            .expect("restore checkpoint into fresh E2B sandbox");
        let restored = provider
            .execute(
                &fresh_handle,
                "file_read",
                &json!({"path": marker_path}).to_string(),
            )
            .await
            .expect("read restored marker");
        assert!(restored.to_text().contains(&marker));
        let mode = provider
            .execute(
                &fresh_handle,
                "bash",
                &json!({"cmd": format!("stat -c '%a' {marker_path}")}).to_string(),
            )
            .await
            .expect("inspect restored marker mode");
        assert_eq!(mode.process_stdout().map(str::trim), Some("644"));
        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = async {
        if let Some(fresh) = &fresh {
            destroy_and_wait(&provider, fresh).await?;
        }
        destroy_and_wait(&provider, &source).await?;
        if let Some((checkpoint, binding)) = checkpoint_cleanup {
            provider
                .delete_workspace_storage(
                    moa_core::types::sandbox_workspace::WorkspaceStorageDeleteRequest {
                        operation: live_workspace_operation(
                            moa_core::types::sandbox_workspace::WorkspaceOperationKind::Delete,
                            binding,
                        ),
                        storage: checkpoint,
                    },
                )
                .await?;
        }
        wait_for_no_provisioned_hands(&provider, source_operation_id, Duration::from_secs(60))
            .await?;
        if let Some(fresh_operation_id) = fresh_operation_id {
            wait_for_no_provisioned_hands(&provider, fresh_operation_id, Duration::from_secs(60))
                .await?;
        }
        Ok::<(), MoaError>(())
    }
    .await;

    match result {
        Ok(Ok(())) => cleanup_result.expect("workspace restore cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup_result.expect("workspace restore cleanup should succeed after provider error");
            panic!("live E2B workspace restore test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("workspace restore cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1, E2B_API_KEY, and fresh V58 MOA_DATABASE_URL"]
async fn e2b_router_reuses_and_isolates() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let mut config = live_config();
    let temp = tempdir().expect("tempdir");
    config.local.sandbox_dir = temp.path().join("sandbox").display().to_string();

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&live_database_url())
        .await
        .expect("live E2B Postgres should be reachable");
    let session_one = session("one");
    let session_two = session("two");
    let session_store =
        moa_session::PostgresSessionStore::from_existing_pool_with_config(&config, pool.clone())
            .await
            .expect("live E2B session store");
    seed_router_capacity(&pool).await;
    for session in [&session_one, &session_two] {
        let created = session_store
            .create_session(session.clone())
            .await
            .expect("seed live E2B router session");
        assert_eq!(created, session.id);
        seed_router_workspace(&pool, &router_workspace_scope(session)).await;
    }
    let checkpoint_store = live_checkpoint_store("e2b-live-router-checkpoints");

    // This fixture manually destroys every live sandbox below, so it owns the
    // cleanup obligation that the production composition root assigns to the
    // durable reaper. Declare that owner before bounded-idle admission.
    let router = ToolRouter::from_config_with_checkpoint_store(
        &config,
        None,
        None,
        Some(checkpoint_store),
        Some(pool.clone()),
        None,
        true,
    )
    .await
    .expect("router should load E2B from config")
    .with_hand_lease_store(Arc::new(
        moa_hands::core::leases::PostgresHandLeaseStore::new(pool.clone()),
    ))
    .with_hand_lease_reaper();
    let provider = E2BHandProvider::new(Arc::new(
        FileProviderCredentialSource::from_config(
            config.cloud.hands.as_ref().expect("cloud hands config"),
        )
        .expect("provider credential source from config"),
    ));

    let file_one = format!("tmp/moa-e2b-router-one-{}.txt", Uuid::now_v7().simple());
    let file_two = format!("tmp/moa-e2b-router-two-{}.txt", Uuid::now_v7().simple());
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

    let handle_one = HandHandle::e2b(handle_one_id.clone(), live_provider_account_id(), 1);
    let mut cleanup_handles = vec![handle_one.clone()];
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
        let restored_hand_id = secured_2
            .hand_id
            .clone()
            .expect("checkpoint recovery should return a fresh E2B hand");
        cleanup_handles.push(HandHandle::e2b(
            restored_hand_id.clone(),
            live_provider_account_id(),
            1,
        ));
        let read = secured_2.safe_output;
        assert_ne!(restored_hand_id, handle_one_id);
        assert!(read.to_text().contains(&content_one));

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
        let reused_hand_id = secured_3.hand_id.clone();
        let subsequent_read = secured_3.safe_output;
        assert_eq!(reused_hand_id.as_deref(), Some(restored_hand_id.as_str()));
        assert!(subsequent_read.to_text().contains(&content_one));

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
        assert_ne!(hand_two_id, restored_hand_id);
        cleanup_handles.push(HandHandle::e2b(
            hand_two_id.clone(),
            live_provider_account_id(),
            1,
        ));

        let restored_two = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_two)),
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_two }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;
        let restored_two_id = restored_two
            .hand_id
            .clone()
            .expect("second checkpoint recovery should return a fresh E2B hand");
        cleanup_handles.push(HandHandle::e2b(
            restored_two_id.clone(),
            live_provider_account_id(),
            1,
        ));
        assert_ne!(restored_two_id, hand_two_id);
        assert_ne!(restored_two_id, restored_hand_id);
        assert!(restored_two.safe_output.to_text().contains(&content_two));

        let missing_read = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                workspace_scope: Some(&router_workspace_scope(&session_two)),
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
            .await;
        match missing_read {
            Ok(secured) => {
                let output = secured.safe_output;
                assert_ne!(
                    output.process_exit_code(),
                    Some(0),
                    "second sandbox unexpectedly read first sandbox file: {}",
                    output.to_text()
                );
            }
            Err(error) => match error {
                MoaError::HttpStatus { status, .. } => assert_eq!(status, 404),
                other => panic!("unexpected second-sandbox read failure: {other}"),
            },
        }

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = async {
        for handle in &cleanup_handles {
            destroy_and_wait(&provider, handle).await?;
        }
        Ok::<(), MoaError>(())
    }
    .await;

    match test_result {
        Ok(Ok(())) => cleanup_result.expect("router cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup_result.expect("router cleanup should succeed after provider failure");
            panic!("live E2B router test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("router cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

/// A live-provider hand spec with unrestricted egress, a 5-minute idle window,
/// and a 10-minute provider-enforced hard lifetime.
fn live_hand_spec(tier: moa_core::types::hands::SandboxTier) -> HandSpec {
    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::hands::{
        BuiltinPolicyRevision, SandboxPolicySnapshot, resolve_effective_sandbox_profile,
    };

    let effective_profile = resolve_effective_sandbox_profile(
        &live_sandbox_profile_config()
            .snapshot()
            .expect("live deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "live-capabilities-v1",
    )
    .expect("live policy resolution should succeed");
    HandSpec {
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        workspace: live_workspace_binding("e2b-live-worker"),
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        filesystem: moa_core::types::sandbox_workspace::SandboxFilesystemLayout::standard(),
        effective_profile,
    }
}

fn live_hand_spec_with_binding(
    binding: moa_core::types::sandbox_workspace::WorkspaceBinding,
) -> HandSpec {
    let mut spec = live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM);
    spec.workspace = binding;
    spec
}

fn live_workspace_operation(
    kind: moa_core::types::sandbox_workspace::WorkspaceOperationKind,
    binding: moa_core::types::sandbox_workspace::WorkspaceBinding,
) -> moa_core::types::sandbox_workspace::WorkspaceStorageOperation {
    let operation_id = moa_core::types::identifiers::WorkspaceOperationId::new();
    moa_core::types::sandbox_workspace::WorkspaceStorageOperation {
        operation_id,
        kind,
        binding,
        deadline: chrono::Utc::now() + chrono::Duration::minutes(10),
        request_hash: format!("e2b-live-{operation_id}"),
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

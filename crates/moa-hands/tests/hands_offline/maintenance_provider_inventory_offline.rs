use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;

use async_trait::async_trait;
use moa_config::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
    DaytonaStorageAccountConfig, DaytonaStorageConfig, LocalHandProviderAccountConfig, MoaConfig,
    ProviderSecretFileSelector, SandboxWorkspaceMode, SecurityProfile,
};
use moa_core::error::MoaError;
use moa_crypto::{
    DataKeyDecryptRequest, GeneratedDataKey, KeyHandle, KeyManagementProvider, LocalKmsProvider,
    PlaintextDek,
};
use moa_hands::{
    SandboxProviderInventory,
    core::sandbox_workspace::checkpoint::{
        archive::ArchiveLimits,
        store::{CheckpointObjectStore, ObservedCheckpointBucketVersioning},
    },
};
use object_store::memory::InMemory;
use sqlx::postgres::PgPoolOptions;
use tempfile::{TempDir, tempdir};
use uuid::Uuid;

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

fn lazy_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgresql://moa:moa@127.0.0.1:1/moa")
        .expect("offline provider construction should accept a lazy Postgres pool")
}

fn checkpoint_store(kms: Arc<dyn KeyManagementProvider>) -> Arc<CheckpointObjectStore> {
    Arc::new(
        CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            kms,
            format!(
                "maintenance-provider-inventory/{}/checkpoints",
                Uuid::new_v4()
            ),
            ArchiveLimits::default(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("offline checkpoint store should construct"),
    )
}

fn credential_account(
    provider: CloudHandProviderKind,
    credential_dir: &TempDir,
) -> CloudHandProviderAccountConfig {
    let provider_name = provider.as_str();
    let path = credential_dir.path().join(provider_name);
    std::fs::write(
        &path,
        format!("MOA_TEST_{}_KEY", provider_name.to_uppercase()),
    )
    .expect("write provider credential");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
        .expect("restrict provider credential permissions");
    let owner_uid = std::fs::metadata(&path)
        .expect("read provider credential metadata")
        .uid();
    CloudHandProviderAccountConfig {
        provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
        generation: 1,
        provider,
        isolation_cell: format!("{provider_name}-offline"),
        api_origin: match provider {
            CloudHandProviderKind::Daytona => "https://api.daytona.io".to_string(),
            CloudHandProviderKind::E2b => "https://api.e2b.dev".to_string(),
        },
        toolbox_origin: (provider == CloudHandProviderKind::Daytona)
            .then(|| "https://proxy.app.daytona.io".to_string()),
        sandbox_domain: (provider == CloudHandProviderKind::E2b).then(|| "e2b.app".to_string()),
        default_runtime: Some("base".to_string()),
        project_fingerprint: Some(format!("sha256:{provider_name}-offline")),
        credential: ProviderSecretFileSelector { path, owner_uid },
    }
}

fn provider_names(inventory: &SandboxProviderInventory) -> (Vec<String>, Vec<String>) {
    let hands = inventory
        .hand_providers()
        .iter()
        .map(|provider| provider.provider_name().to_string())
        .collect();
    let storage = inventory
        .storage_providers()
        .iter()
        .map(|provider| provider.storage_provider_name().to_string())
        .collect();
    (hands, storage)
}

fn config_error_message(error: MoaError) -> String {
    match error {
        MoaError::ConfigError(message) => message,
        other => panic!("expected a configuration error, got {other}"),
    }
}

#[tokio::test]
async fn cloud_maintenance_inventory_does_not_require_tool_policy_owners_offline() {
    // Pins: a cloud maintenance process constructs provider inventory without
    // inheriting ToolRouter's action-policy/session/connector dependencies.
    let credentials = tempdir().expect("credential tempdir");
    let mut config = MoaConfig {
        security_profile: SecurityProfile::Cloud,
        ..MoaConfig::default()
    };
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        provider_accounts: vec![credential_account(CloudHandProviderKind::E2b, &credentials)],
        ..CloudHandsConfig::default()
    });
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());

    let inventory = SandboxProviderInventory::for_maintenance(
        &config,
        &lazy_pool(),
        checkpoint_store(Arc::clone(&kms)),
        kms,
    )
    .await
    .expect("cloud maintenance inventory should not require an action-policy owner");

    assert_eq!(
        provider_names(&inventory),
        (vec!["e2b".to_string()], vec!["e2b".to_string()])
    );
}

#[tokio::test]
async fn maintenance_inventory_constructs_each_configured_provider_kind_once_offline() {
    // Pins: maintenance follows configured durable account kinds, not the
    // default/fallback tool route, and returns stable unique registries.
    let credentials = tempdir().expect("credential tempdir");
    let local_root = tempdir().expect("local sandbox tempdir");
    let daytona = credential_account(CloudHandProviderKind::Daytona, &credentials);
    let daytona_account_id = daytona.provider_account_id;
    let e2b = credential_account(CloudHandProviderKind::E2b, &credentials);
    let mut config = MoaConfig::default();
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.local.docker_enabled = false;
    config.local.sandbox_dir = local_root.path().display().to_string();
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
        generation: 1,
        isolation_cell: "local-offline".to_string(),
    });
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        provider_accounts: vec![daytona, e2b],
        ..CloudHandsConfig::default()
    });
    config.cloud.daytona_storage = DaytonaStorageConfig {
        accounts: vec![DaytonaStorageAccountConfig {
            provider_account_id: daytona_account_id,
            security_class: "tenant-isolated".to_string(),
            volume_ceiling: 100,
            admission_headroom: 1,
        }],
        consistency_window_seconds: 1,
    };
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());

    let inventory = SandboxProviderInventory::for_maintenance(
        &config,
        &lazy_pool(),
        checkpoint_store(Arc::clone(&kms)),
        kms,
    )
    .await
    .expect("all configured maintenance providers should construct");

    let expected = vec![
        "daytona".to_string(),
        "e2b".to_string(),
        "local".to_string(),
    ];
    assert_eq!(provider_names(&inventory), (expected.clone(), expected));
}

#[tokio::test]
async fn cloud_maintenance_inventory_validates_credentials_before_registration_offline() {
    // Pins: maintenance cannot register a cloud provider whose configured
    // credential file is absent, even though construction performs no API call.
    let credentials = tempdir().expect("credential tempdir");
    let account = credential_account(CloudHandProviderKind::E2b, &credentials);
    std::fs::remove_file(&account.credential.path).expect("remove provider credential");
    let mut config = MoaConfig::default();
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.cloud.hands = Some(CloudHandsConfig {
        provider_accounts: vec![account],
        ..CloudHandsConfig::default()
    });
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());

    let result = SandboxProviderInventory::for_maintenance(
        &config,
        &lazy_pool(),
        checkpoint_store(Arc::clone(&kms)),
        kms,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("missing cloud credentials must fail provider registration"),
        Err(error) => error,
    };

    assert_eq!(config_error_message(error), "credential file is missing");
}

#[tokio::test]
async fn maintenance_inventory_rejects_ephemeral_kms_before_provider_side_effects_offline() {
    // Pins: every maintenance inventory has durable checkpoint authority even
    // when the selected provider itself is local and makes no cloud API call.
    let parent = tempdir().expect("local sandbox parent tempdir");
    let sandbox_root = parent.path().join("must-not-exist");
    let mut config = MoaConfig::default();
    config.sandbox_workspaces.mode = SandboxWorkspaceMode::Maintenance;
    config.local.docker_enabled = false;
    config.local.sandbox_dir = sandbox_root.display().to_string();
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
        generation: 1,
        isolation_cell: "ephemeral-kms-local".to_string(),
    });
    let ephemeral_kms: Arc<dyn KeyManagementProvider> = Arc::new(LocalKmsProvider::new());

    let result = SandboxProviderInventory::for_maintenance(
        &config,
        &lazy_pool(),
        checkpoint_store(Arc::clone(&ephemeral_kms)),
        ephemeral_kms,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("ephemeral KMS must fail maintenance provider construction"),
        Err(error) => error,
    };

    assert_eq!(
        config_error_message(error),
        "sandbox provider maintenance inventory requires durable KMS authority"
    );
    assert!(
        !sandbox_root.exists(),
        "KMS validation must run before local provider side effects"
    );
}

#[tokio::test]
async fn disabled_mode_rejects_before_provider_side_effects_offline() {
    // Pins: disabled mode remains dark and does not create the configured local
    // sandbox directory while trying to assemble maintenance providers.
    let parent = tempdir().expect("local sandbox parent tempdir");
    let sandbox_root = parent.path().join("must-not-exist");
    let mut config = MoaConfig::default();
    config.local.docker_enabled = false;
    config.local.sandbox_dir = sandbox_root.display().to_string();
    config.local.provider_account = Some(LocalHandProviderAccountConfig {
        provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
        generation: 1,
        isolation_cell: "disabled-local".to_string(),
    });
    let kms: Arc<dyn KeyManagementProvider> = Arc::new(DurableTestKms::new());

    let result = SandboxProviderInventory::for_maintenance(
        &config,
        &lazy_pool(),
        checkpoint_store(Arc::clone(&kms)),
        kms,
    )
    .await;
    let error = match result {
        Ok(_) => panic!("disabled mode must reject maintenance provider construction"),
        Err(error) => error,
    };

    assert_eq!(
        config_error_message(error),
        "sandbox provider maintenance inventory requires maintenance or admit mode"
    );
    assert!(
        !sandbox_root.exists(),
        "disabled construction must not create a local sandbox directory"
    );
}

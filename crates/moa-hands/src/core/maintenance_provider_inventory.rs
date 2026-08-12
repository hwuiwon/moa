//! Maintenance-only construction of sandbox compute and storage providers.

use std::sync::Arc;

use moa_config::{CloudHandProviderKind, MoaConfig};
use moa_core::{
    error::{MoaError, Result},
    traits::{HandProvider, SandboxStorageProvider},
};
use moa_crypto::KeyManagementProvider;
use sqlx::PgPool;

use super::{
    DEFAULT_TOOL_TIMEOUT,
    normalization::expand_local_path,
    provider_credentials::{FileProviderCredentialSource, ProviderCredentialSource},
    sandbox_workspace::{
        capacity::PostgresWorkspaceCapacityRepository, checkpoint::store::CheckpointObjectStore,
        operations::PostgresWorkspaceOperationRepository, repository::PostgresWorkspaceRepository,
        storage_resources::PostgresWorkspaceStorageResourceRepository,
    },
};
use crate::adapters::{
    daytona::{DaytonaHandProvider, storage::DaytonaStorageDependencies},
    e2b::E2BHandProvider,
    local::LocalHandProvider,
};

/// Exact provider adapters needed by durable sandbox maintenance.
///
/// This inventory intentionally contains no tool catalog, action-policy owner,
/// session store, connector state, or background task. The orchestrator owns
/// maintenance loops separately and uses these adapters only for durable
/// provider inventory, reconciliation, and destruction.
pub struct SandboxProviderInventory {
    hand_providers: Vec<Arc<dyn HandProvider>>,
    storage_providers: Vec<Arc<dyn SandboxStorageProvider>>,
}

impl SandboxProviderInventory {
    /// Constructs every provider kind with a configured durable account mapping.
    ///
    /// Maintenance selection follows provider-account inventory rather than the
    /// active tool route. An account generation may still own resources after it
    /// stops being the default or fallback admission route, so cleanup must keep
    /// its provider adapter available until the durable state is drained.
    pub async fn for_maintenance(
        config: &MoaConfig,
        workspace_pool: &PgPool,
        checkpoint_store: Arc<CheckpointObjectStore>,
        workspace_kms: Arc<dyn KeyManagementProvider>,
    ) -> Result<Self> {
        if !config.sandbox_workspaces.mode.maintenance_enabled() {
            return Err(MoaError::ConfigError(
                "sandbox provider maintenance inventory requires maintenance or admit mode"
                    .to_string(),
            ));
        }
        if !workspace_kms.is_durable() {
            return Err(MoaError::ConfigError(
                "sandbox provider maintenance inventory requires durable KMS authority".to_string(),
            ));
        }

        let capacity = Arc::new(PostgresWorkspaceCapacityRepository::new(
            workspace_pool.clone(),
        ));
        let mut hand_providers: Vec<Arc<dyn HandProvider>> = Vec::new();
        let mut storage_providers: Vec<Arc<dyn SandboxStorageProvider>> = Vec::new();

        if config.local.provider_account.is_some() {
            let sandbox_root = expand_local_path(&config.local.sandbox_dir)?;
            let provider = Arc::new(
                LocalHandProvider::new_with_docker_detection(
                    sandbox_root,
                    config.local.docker_enabled,
                )
                .await?
                .with_command_timeout(DEFAULT_TOOL_TIMEOUT)
                .with_checkpoint_store(Arc::clone(&checkpoint_store))
                .with_checkpoint_capacity(Arc::clone(&capacity)),
            );
            hand_providers.push(provider.clone());
            storage_providers.push(provider);
        }

        if let Some(hands) = &config.cloud.hands {
            let has_daytona = hands
                .provider_accounts
                .iter()
                .any(|account| account.provider == CloudHandProviderKind::Daytona);
            let has_e2b = hands
                .provider_accounts
                .iter()
                .any(|account| account.provider == CloudHandProviderKind::E2b);
            let credentials = if has_daytona || has_e2b {
                let source = Arc::new(FileProviderCredentialSource::from_config(hands)?);
                source.validate_all().await?;
                Some(source as Arc<dyn ProviderCredentialSource>)
            } else {
                None
            };

            if has_daytona {
                let credentials = credentials.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "Daytona maintenance credential source is unavailable".to_string(),
                    )
                })?;
                let provider = Arc::new(DaytonaHandProvider::new_with_storage(
                    Arc::clone(credentials),
                    DaytonaStorageDependencies {
                        config: config.cloud.daytona_storage.clone(),
                        checkpoint_store: Arc::clone(&checkpoint_store),
                        workspaces: Arc::new(PostgresWorkspaceRepository::new(
                            workspace_pool.clone(),
                        )),
                        storage_resources: Arc::new(
                            PostgresWorkspaceStorageResourceRepository::new(workspace_pool.clone()),
                        ),
                        operations: Arc::new(PostgresWorkspaceOperationRepository::new(
                            workspace_pool.clone(),
                        )),
                        capacity: Arc::clone(&capacity),
                        kms: Arc::clone(&workspace_kms),
                    },
                )?);
                hand_providers.push(provider.clone());
                storage_providers.push(provider);
            }

            if has_e2b {
                let credentials = credentials.as_ref().ok_or_else(|| {
                    MoaError::ConfigError(
                        "E2B maintenance credential source is unavailable".to_string(),
                    )
                })?;
                let provider = Arc::new(
                    E2BHandProvider::new(Arc::clone(credentials))
                        .with_checkpoint_store(Arc::clone(&checkpoint_store))
                        .with_checkpoint_capacity(Arc::clone(&capacity)),
                );
                hand_providers.push(provider.clone());
                storage_providers.push(provider);
            }
        }

        if hand_providers.is_empty() || storage_providers.is_empty() {
            return Err(MoaError::ConfigError(
                "sandbox provider maintenance inventory requires a configured provider account"
                    .to_string(),
            ));
        }
        hand_providers.sort_by(|left, right| left.provider_name().cmp(right.provider_name()));
        storage_providers.sort_by(|left, right| {
            left.storage_provider_name()
                .cmp(right.storage_provider_name())
        });

        Ok(Self {
            hand_providers,
            storage_providers,
        })
    }

    /// Returns hand providers in stable provider-name order.
    #[must_use]
    pub fn hand_providers(&self) -> Vec<Arc<dyn HandProvider>> {
        self.hand_providers.clone()
    }

    /// Returns workspace-storage providers in stable provider-name order.
    #[must_use]
    pub fn storage_providers(&self) -> Vec<Arc<dyn SandboxStorageProvider>> {
        self.storage_providers.clone()
    }
}

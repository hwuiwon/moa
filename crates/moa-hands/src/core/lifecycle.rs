//! Workspace-root tracking and lazy hand lifecycle management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use moa_core::{
    HandHandle, HandResources, HandSpec, MoaError, Result, SandboxFile, SandboxTier, SessionMeta,
    WorkspaceId, record_sandbox_provision_duration,
};

use super::{DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, ToolRouter};

impl ToolRouter {
    /// Remembers the filesystem root for a logical workspace id.
    pub async fn remember_workspace_root(
        &self,
        workspace_id: WorkspaceId,
        workspace_root: PathBuf,
    ) {
        self.workspace_roots
            .write()
            .await
            .insert(workspace_id, workspace_root);
    }

    /// Returns the remembered filesystem root for a logical workspace id.
    pub async fn workspace_root(&self, workspace_id: &WorkspaceId) -> Option<PathBuf> {
        self.workspace_roots.read().await.get(workspace_id).cloned()
    }

    /// Provisions a hand if needed and installs trusted files into its sandbox.
    pub async fn install_files(
        &self,
        session: &SessionMeta,
        provider: &str,
        tier: SandboxTier,
        files: &[SandboxFile],
    ) -> Result<HandHandle> {
        let handle = self.get_or_provision_hand(provider, tier, session).await?;
        self.install_files_on_handle(session, provider, &handle, files)
            .await?;
        Ok(handle)
    }

    /// Stores trusted files that should be installed lazily before hand tool execution.
    pub async fn set_trusted_sandbox_files(&self, session: &SessionMeta, files: Vec<SandboxFile>) {
        if files.is_empty() {
            self.trusted_sandbox_files.write().await.remove(&session.id);
            let session_prefix = format!("{}:", session.id);
            self.installed_files
                .write()
                .await
                .retain(|key, _| !key.starts_with(&session_prefix));
            return;
        }

        self.trusted_sandbox_files
            .write()
            .await
            .insert(session.id, files);
    }

    pub(super) async fn install_trusted_files_for_hand(
        &self,
        session: &SessionMeta,
        provider: &str,
        handle: &HandHandle,
    ) -> Result<()> {
        let files = self
            .trusted_sandbox_files
            .read()
            .await
            .get(&session.id)
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            return Ok(());
        }
        self.install_files_on_handle(session, provider, handle, &files)
            .await
    }

    async fn install_files_on_handle(
        &self,
        session: &SessionMeta,
        provider: &str,
        handle: &HandHandle,
        files: &[SandboxFile],
    ) -> Result<()> {
        let key = session_provider_key(session, provider);
        let already_installed = self
            .installed_files
            .read()
            .await
            .get(&key)
            .is_some_and(|installed| installed == files);
        if already_installed {
            return Ok(());
        }
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        provider_impl.install_files(handle, files).await?;
        self.installed_files
            .write()
            .await
            .insert(key, files.to_vec());
        Ok(())
    }

    /// Destroys and removes all cached hands associated with the provided session.
    pub async fn destroy_session_hands(&self, session_id: &moa_core::SessionId) {
        let session_prefix = format!("{session_id}:");
        let cached = {
            let mut active_hands = self.active_hands.write().await;
            let keys = active_hands
                .keys()
                .filter(|key| key.starts_with(&session_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| active_hands.remove(&key).map(|handle| (key, handle)))
                .collect::<Vec<_>>()
        };
        self.installed_files
            .write()
            .await
            .retain(|key, _| !key.starts_with(&session_prefix));
        self.trusted_sandbox_files.write().await.remove(session_id);

        for (key, handle) in cached {
            let provider_name = key
                .strip_prefix(&session_prefix)
                .unwrap_or_default()
                .to_string();
            let handle_id = hand_id(&handle);
            let Some(provider) = self.providers.get(&provider_name) else {
                tracing::warn!(
                    session_id = %session_id,
                    provider = %provider_name,
                    hand_id = %handle_id,
                    "cached hand provider missing during cleanup"
                );
                continue;
            };

            match provider.destroy(&handle).await {
                Ok(()) => {
                    tracing::info!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        "destroyed cached session hand"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        error = %error,
                        "failed to destroy cached session hand"
                    );
                }
            }
        }
    }

    pub(super) async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }

        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&tenant_workspace_key(session))
                    .cloned()
            } else {
                None
            };
        let tier_label = sandbox_tier_label(&tier);
        let started_at = Instant::now();
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier,
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;
        record_sandbox_provision_duration(provider, tier_label, started_at.elapsed());

        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }

    pub(super) async fn reprovision_hand(
        &self,
        session: &SessionMeta,
        provider: &str,
        tier: &SandboxTier,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        let old_handle = self.active_hands.write().await.remove(&key);
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;

        if let Some(handle) = old_handle.as_ref()
            && let Err(error) = provider_impl.destroy(handle).await
        {
            tracing::warn!(
                session_id = %session.id,
                provider,
                hand_id = %hand_id(handle),
                error = %error,
                "failed to destroy unhealthy hand before re-provisioning"
            );
        }

        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&tenant_workspace_key(session))
                    .cloned()
            } else {
                None
            };
        let started_at = Instant::now();
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier.clone(),
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;
        record_sandbox_provision_duration(provider, sandbox_tier_label(tier), started_at.elapsed());
        if let Some(files) = self.installed_files.read().await.get(&key).cloned() {
            provider_impl.install_files(&handle, &files).await?;
        }
        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }
}

pub(super) fn session_provider_key(session: &SessionMeta, provider: &str) -> String {
    format!("{}:{provider}", session.id)
}

fn tenant_workspace_key(session: &SessionMeta) -> WorkspaceId {
    WorkspaceId::new(session.tenant_id.to_string())
}

pub(super) fn sandbox_tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

pub(super) fn hand_id(handle: &HandHandle) -> String {
    match handle {
        HandHandle::Local { sandbox_dir } => sandbox_dir.display().to_string(),
        HandHandle::Docker { container_id } => container_id.clone(),
        HandHandle::Daytona { workspace_id } => workspace_id.clone(),
        HandHandle::E2B { sandbox_id } => sandbox_id.clone(),
    }
}

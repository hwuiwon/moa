//! Workspace-root tracking and lazy hand lifecycle management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    HandHandle, HandResources, HandSpec, HandStatus, MoaError, Result, SandboxFile, SandboxTier,
    SessionMeta, TenantId,
};
use moa_observability::record_sandbox_provision_duration;

use super::leases::{HandLease, HandLeaseStatus, LeaseHandle};
use super::{DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, ToolRouter};

const HAND_LEASE_TTL_SECS: i64 = 60 * 60;
const HAND_LEASE_PROVISION_WAIT_MS: u64 = 25;

impl ToolRouter {
    /// Remembers the filesystem workspace root for one tenant.
    pub async fn remember_workspace_root(&self, tenant_id: TenantId, workspace_root: PathBuf) {
        self.workspace_roots
            .write()
            .await
            .insert(tenant_id, workspace_root);
    }

    /// Returns the remembered filesystem workspace root for one tenant.
    pub async fn workspace_root(&self, tenant_id: &TenantId) -> Option<PathBuf> {
        self.workspace_roots.read().await.get(tenant_id).cloned()
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

    /// Destroys and removes all cached and durably leased hands associated with the session.
    pub async fn destroy_session_hands(&self, session_id: &moa_core::SessionId) {
        let session_prefix = format!("{session_id}:");
        let mut hands = {
            let mut active_hands = self.active_hands.write().await;
            let keys = active_hands
                .keys()
                .filter(|key| key.starts_with(&session_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| active_hands.remove(&key).map(|handle| (key, handle)))
                .collect::<HashMap<_, _>>()
        };
        self.installed_files
            .write()
            .await
            .retain(|key, _| !key.starts_with(&session_prefix));
        self.trusted_sandbox_files.write().await.remove(session_id);

        if let Some(lease_store) = &self.hand_leases {
            match lease_store.list_session(*session_id).await {
                Ok(leases) => {
                    for lease in leases {
                        if lease.status == HandLeaseStatus::Destroyed {
                            continue;
                        }
                        let Some(lease_handle) = lease.handle.as_ref() else {
                            continue;
                        };
                        let key = format!("{}:{}", lease.session_id, lease.provider);
                        if let std::collections::hash_map::Entry::Vacant(entry) = hands.entry(key) {
                            match self
                                .hydrate_lease_handle(&lease.provider, lease_handle)
                                .await
                            {
                                Ok(handle) => {
                                    entry.insert(handle);
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        provider = %lease.provider,
                                        generation = lease.generation,
                                        error = %error,
                                        "failed to hydrate durable hand lease during cleanup"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to list durable hand leases during session cleanup"
                    );
                }
            }
        }

        for (key, handle) in hands {
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
                    if let Some(lease_store) = &self.hand_leases
                        && let Ok(Some(lease)) = lease_store.get(*session_id, &provider_name).await
                        && let Err(error) = lease_store
                            .mark_status(
                                *session_id,
                                &provider_name,
                                lease.generation,
                                HandLeaseStatus::Destroyed,
                            )
                            .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            provider = %provider_name,
                            generation = lease.generation,
                            error = %error,
                            "failed to mark durable hand lease destroyed"
                        );
                    }
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
        if self.hand_leases.is_some() {
            let cached_handle = self.active_hands.read().await.get(&key).cloned();
            if let Some(handle) = cached_handle {
                if let Some(validated) = self
                    .validate_cached_durable_hand(provider, session, &key, &handle)
                    .await?
                {
                    return Ok(validated);
                }
            }
            return self
                .get_or_provision_durable_hand(provider, tier, session, key)
                .await;
        }

        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }

        self.provision_uncached_hand(provider, tier, session, key)
            .await
    }

    async fn validate_cached_durable_hand(
        &self,
        provider: &str,
        session: &SessionMeta,
        key: &str,
        cached_handle: &HandHandle,
    ) -> Result<Option<HandHandle>> {
        let Some(lease_store) = &self.hand_leases else {
            return Ok(Some(cached_handle.clone()));
        };
        let Some(lease) = lease_store.get(session.id, provider).await? else {
            self.remove_cached_hand_if_matches(key, cached_handle).await;
            return Ok(None);
        };
        if lease.status != HandLeaseStatus::Active || lease.expires_at <= Utc::now() {
            self.remove_cached_hand_if_matches(key, cached_handle).await;
            return Ok(None);
        }
        let Some(lease_handle) = lease.handle.as_ref() else {
            self.remove_cached_hand_if_matches(key, cached_handle).await;
            return Ok(None);
        };
        let durable_handle = match self.hydrate_lease_handle(provider, lease_handle).await {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id,
                    provider,
                    generation = lease.generation,
                    error = %error,
                    "failed to hydrate cached durable hand lease; marking stale"
                );
                self.remove_cached_hand_if_matches(key, cached_handle).await;
                lease_store
                    .mark_status(
                        session.id,
                        provider,
                        lease.generation,
                        HandLeaseStatus::Stale,
                    )
                    .await?;
                return Ok(None);
            }
        };
        if durable_handle != *cached_handle {
            self.remove_cached_hand_if_matches(key, cached_handle).await;
            return Ok(None);
        }
        if !lease_store
            .renew_active(
                session.id,
                provider,
                lease.generation,
                hand_lease_expires_at(),
            )
            .await?
        {
            self.remove_cached_hand_if_matches(key, cached_handle).await;
            return Ok(None);
        }
        Ok(Some(cached_handle.clone()))
    }

    async fn remove_cached_hand_if_matches(&self, key: &str, expected_handle: &HandHandle) {
        let mut active_hands = self.active_hands.write().await;
        if active_hands.get(key) == Some(expected_handle) {
            active_hands.remove(key);
        }
    }

    async fn get_or_provision_durable_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
        key: String,
    ) -> Result<HandHandle> {
        let lease_store = self.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let wait_started = Instant::now();
        let wait_budget = provisioning_wait_budget(DEFAULT_TOOL_TIMEOUT);

        loop {
            let expires_at = hand_lease_expires_at();
            if let Some(claim) = lease_store
                .claim_for_provisioning(
                    session.id,
                    session.tenant_id,
                    provider,
                    tier.clone(),
                    expires_at,
                )
                .await?
            {
                match self
                    .provision_uncached_hand(provider, tier.clone(), session, key.clone())
                    .await
                {
                    Ok(handle) => {
                        let lease_handle =
                            match self.lease_handle_for_provider(provider, &handle).await {
                                Ok(lease_handle) => lease_handle,
                                Err(error) => {
                                    self.destroy_provisioned_hand(provider, &key, &handle).await;
                                    if let Err(mark_error) = lease_store
                                        .mark_status(
                                            session.id,
                                            provider,
                                            claim.generation,
                                            HandLeaseStatus::Failed,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            session_id = %session.id,
                                            provider,
                                            generation = claim.generation,
                                            error = %mark_error,
                                            "failed to mark hand lease provisioning failure"
                                        );
                                    }
                                    return Err(error);
                                }
                            };
                        if let Err(error) = lease_store
                            .activate(
                                session.id,
                                provider,
                                claim.generation,
                                lease_handle,
                                expires_at,
                            )
                            .await
                        {
                            self.destroy_provisioned_hand(provider, &key, &handle).await;
                            return Err(error);
                        }
                        return Ok(handle);
                    }
                    Err(error) => {
                        if let Err(mark_error) = lease_store
                            .mark_status(
                                session.id,
                                provider,
                                claim.generation,
                                HandLeaseStatus::Failed,
                            )
                            .await
                        {
                            tracing::warn!(
                                session_id = %session.id,
                                provider,
                                generation = claim.generation,
                                error = %mark_error,
                                "failed to mark hand lease provisioning failure"
                            );
                        }
                        return Err(error);
                    }
                }
            }

            if let Some(lease) = lease_store.get(session.id, provider).await? {
                match lease.status {
                    HandLeaseStatus::Active if lease.expires_at > Utc::now() => {
                        match self.resume_durable_lease(provider, &lease, &key).await {
                            Ok(handle) => {
                                if lease_store
                                    .renew_active(
                                        session.id,
                                        provider,
                                        lease.generation,
                                        hand_lease_expires_at(),
                                    )
                                    .await?
                                {
                                    return Ok(handle);
                                }
                                self.remove_cached_hand_if_matches(&key, &handle).await;
                                continue;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    session_id = %session.id,
                                    provider,
                                    generation = lease.generation,
                                    error = %error,
                                    "durable hand lease could not be resumed; marking stale"
                                );
                                lease_store
                                    .mark_status(
                                        session.id,
                                        provider,
                                        lease.generation,
                                        HandLeaseStatus::Stale,
                                    )
                                    .await?;
                                continue;
                            }
                        }
                    }
                    HandLeaseStatus::Provisioning => {
                        if wait_started.elapsed() >= wait_budget {
                            break;
                        }
                        tokio::time::sleep(provisioning_poll_delay(wait_started, wait_budget))
                            .await;
                        continue;
                    }
                    HandLeaseStatus::Active => {
                        lease_store
                            .mark_status(
                                session.id,
                                provider,
                                lease.generation,
                                HandLeaseStatus::Stale,
                            )
                            .await?;
                        continue;
                    }
                    HandLeaseStatus::Stale
                    | HandLeaseStatus::Destroyed
                    | HandLeaseStatus::Failed => {
                        continue;
                    }
                }
            }

            if wait_started.elapsed() >= wait_budget {
                break;
            }
            tokio::time::sleep(provisioning_poll_delay(wait_started, wait_budget)).await;
        }

        Err(MoaError::ProviderError(format!(
            "timed out waiting for durable hand lease for session {} provider {provider}",
            session.id
        )))
    }

    async fn provision_uncached_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
        key: String,
    ) -> Result<HandHandle> {
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&tenant_key(session))
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

    async fn destroy_provisioned_hand(&self, provider: &str, key: &str, handle: &HandHandle) {
        self.remove_cached_hand_if_matches(key, handle).await;
        let Some(provider_impl) = self.providers.get(provider) else {
            tracing::warn!(
                provider,
                hand_id = %hand_id(handle),
                "provisioned hand provider missing during activation cleanup"
            );
            return;
        };
        if let Err(error) = provider_impl.destroy(handle).await {
            tracing::warn!(
                provider,
                hand_id = %hand_id(handle),
                error = %error,
                "failed to destroy provisioned hand after activation fence loss"
            );
        }
    }

    async fn resume_durable_lease(
        &self,
        provider: &str,
        lease: &HandLease,
        key: &str,
    ) -> Result<HandHandle> {
        let lease_handle = lease.handle.as_ref().ok_or_else(|| {
            MoaError::StorageError(format!(
                "active hand lease for session {} provider {provider} is missing a handle",
                lease.session_id
            ))
        })?;
        let handle = self.hydrate_lease_handle(provider, lease_handle).await?;
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        match provider_impl.status(&handle).await? {
            HandStatus::Running | HandStatus::Provisioning => {}
            HandStatus::Paused | HandStatus::Stopped => {
                provider_impl.resume(&handle).await?;
            }
            HandStatus::Destroyed | HandStatus::Failed => {
                return Err(MoaError::ProviderError(format!(
                    "durable hand lease {} for provider {provider} is not resumable",
                    hand_id(&handle)
                )));
            }
        }
        self.active_hands
            .write()
            .await
            .insert(key.to_string(), handle.clone());
        Ok(handle)
    }

    async fn lease_handle_for_provider(
        &self,
        provider: &str,
        handle: &HandHandle,
    ) -> Result<LeaseHandle> {
        if provider == DEFAULT_PROVIDER_NAME
            && let Some(local_provider) = &self.local_provider
        {
            return local_provider.lease_handle(handle).await;
        }
        Ok(LeaseHandle::new(handle.clone()))
    }

    async fn hydrate_lease_handle(
        &self,
        provider: &str,
        lease_handle: &LeaseHandle,
    ) -> Result<HandHandle> {
        if provider == DEFAULT_PROVIDER_NAME
            && let Some(local_provider) = &self.local_provider
        {
            return local_provider.adopt_lease_handle(lease_handle).await;
        }
        Ok(lease_handle.handle.clone())
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

        if let Some(lease_store) = &self.hand_leases
            && let Ok(Some(lease)) = lease_store.get(session.id, provider).await
            && let Err(error) = lease_store
                .mark_status(
                    session.id,
                    provider,
                    lease.generation,
                    HandLeaseStatus::Stale,
                )
                .await
        {
            tracing::warn!(
                session_id = %session.id,
                provider,
                generation = lease.generation,
                error = %error,
                "failed to mark durable hand lease stale before re-provisioning"
            );
        }

        let handle = self
            .get_or_provision_hand(provider, tier.clone(), session)
            .await?;
        if let Some(files) = self.installed_files.read().await.get(&key).cloned() {
            provider_impl.install_files(&handle, &files).await?;
        }
        Ok(handle)
    }
}

pub(super) fn session_provider_key(session: &SessionMeta, provider: &str) -> String {
    format!("{}:{provider}", session.id)
}

fn tenant_key(session: &SessionMeta) -> TenantId {
    session.tenant_id
}

fn hand_lease_expires_at() -> chrono::DateTime<Utc> {
    Utc::now() + ChronoDuration::seconds(HAND_LEASE_TTL_SECS)
}

fn provisioning_wait_budget(tool_timeout: StdDuration) -> StdDuration {
    tool_timeout.max(StdDuration::from_millis(HAND_LEASE_PROVISION_WAIT_MS))
}

fn provisioning_poll_delay(started_at: Instant, budget: StdDuration) -> StdDuration {
    let poll = StdDuration::from_millis(HAND_LEASE_PROVISION_WAIT_MS);
    budget
        .checked_sub(started_at.elapsed())
        .map_or(poll, |remaining| remaining.min(poll))
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        ActionClass, ActionPolicyEffect, HandProvider, IdempotencyClass, RiskLevel,
        ToolDiffStrategy, ToolInputShape, ToolInvocation, ToolOutput, ToolPolicySpec,
    };
    use serde_json::json;

    use crate::core::leases::{HandLeaseStore, MemoryHandLeaseStore};
    use crate::core::{ToolRegistry, ToolRouter};

    use super::*;

    struct CountingProvider {
        name: String,
        provision_delay: Duration,
        stale_generation_on_provision: Option<(Arc<MemoryHandLeaseStore>, moa_core::SessionId)>,
        destroy_fails: bool,
        provision_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        destroy_calls: AtomicUsize,
    }

    impl CountingProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                provision_delay: Duration::ZERO,
                stale_generation_on_provision: None,
                destroy_fails: false,
                provision_calls: AtomicUsize::new(0),
                execute_calls: AtomicUsize::new(0),
                destroy_calls: AtomicUsize::new(0),
            }
        }

        fn with_provision_delay(mut self, provision_delay: Duration) -> Self {
            self.provision_delay = provision_delay;
            self
        }

        fn with_stale_generation_on_provision(
            mut self,
            lease_store: Arc<MemoryHandLeaseStore>,
            session_id: moa_core::SessionId,
        ) -> Self {
            self.stale_generation_on_provision = Some((lease_store, session_id));
            self
        }

        fn with_destroy_failure(mut self) -> Self {
            self.destroy_fails = true;
            self
        }

        fn provision_calls(&self) -> usize {
            self.provision_calls.load(Ordering::SeqCst)
        }

        fn execute_calls(&self) -> usize {
            self.execute_calls.load(Ordering::SeqCst)
        }

        fn destroy_calls(&self) -> usize {
            self.destroy_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl HandProvider for CountingProvider {
        fn provider_name(&self) -> &str {
            &self.name
        }

        async fn provision(&self, _spec: HandSpec) -> Result<HandHandle> {
            if !self.provision_delay.is_zero() {
                tokio::time::sleep(self.provision_delay).await;
            }
            let count = self.provision_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((lease_store, session_id)) = &self.stale_generation_on_provision {
                lease_store
                    .mark_status(*session_id, &self.name, 1, HandLeaseStatus::Stale)
                    .await?;
            }
            Ok(HandHandle::docker(format!("{}-{count}", self.name)))
        }

        async fn execute(
            &self,
            _handle: &HandHandle,
            _tool: &str,
            _input: &str,
        ) -> Result<ToolOutput> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("ok", Duration::from_millis(1)))
        }

        async fn status(&self, _handle: &HandHandle) -> Result<HandStatus> {
            Ok(HandStatus::Running)
        }

        async fn pause(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn resume(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn destroy(&self, _handle: &HandHandle) -> Result<()> {
            self.destroy_calls.fetch_add(1, Ordering::SeqCst);
            if self.destroy_fails {
                return Err(MoaError::ProviderError("destroy failed".to_string()));
            }
            Ok(())
        }
    }

    fn router(
        provider: Arc<CountingProvider>,
        lease_store: Arc<MemoryHandLeaseStore>,
    ) -> ToolRouter {
        let mut registry = ToolRegistry::default_local();
        registry.register_hand(
            "bash",
            "test shell command",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }),
            ToolPolicySpec {
                risk_level: RiskLevel::High,
                default_effect: ActionPolicyEffect::Allow,
                action_class: ActionClass::CommandExecution,
                input_shape: ToolInputShape::Json,
                diff_strategy: ToolDiffStrategy::None,
            },
            IdempotencyClass::Idempotent,
        );
        registry.retarget_hand_tools(provider.provider_name(), SandboxTier::Container);
        registry.retain_only(["bash"]);
        let provider_trait: Arc<dyn HandProvider> = provider;
        let mut providers = HashMap::new();
        providers.insert(provider_trait.provider_name().to_string(), provider_trait);
        ToolRouter::new(registry, providers).with_hand_lease_store(lease_store)
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: moa_core::SessionId::new(),
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        }
    }

    fn bash_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: "bash".to_string(),
            input: json!({ "cmd": "printf ok" }),
        }
    }

    #[tokio::test]
    async fn lifecycle_new_router_reuses_durable_active_lease() {
        // Pins: a fresh ToolRouter instance must reuse the durable session/provider lease.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("durable-reuse"));
        let session = session();
        let first_router = router(provider.clone(), lease_store.clone());
        let second_router = router(provider.clone(), lease_store);

        first_router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("first router provisions and executes");
        second_router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("second router reuses durable lease");

        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(provider.execute_calls(), 2);
    }

    #[tokio::test]
    async fn lifecycle_racing_routers_share_one_durable_lease() {
        // Pins: concurrent replicas cannot double-provision the same session/provider lease.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(
            CountingProvider::new("durable-race").with_provision_delay(Duration::from_millis(75)),
        );
        let session = session();
        let left_router = router(provider.clone(), lease_store.clone());
        let right_router = router(provider.clone(), lease_store);
        let left_session = session.clone();
        let right_session = session;
        let left_invocation = bash_invocation();
        let right_invocation = bash_invocation();

        let (left, right) = tokio::join!(
            left_router.execute_authorized_with_recovery(&left_session, &left_invocation),
            right_router.execute_authorized_with_recovery(&right_session, &right_invocation)
        );

        left.expect("left router should execute");
        right.expect("right router should execute");
        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(provider.execute_calls(), 2);
    }

    #[tokio::test]
    async fn lifecycle_destroy_session_reads_durable_leases_not_only_cache() {
        // Pins: cleanup from a different router still destroys the hand recorded in Postgres.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("durable-cleanup"));
        let session = session();
        let first_router = router(provider.clone(), lease_store.clone());
        let cleanup_router = router(provider.clone(), lease_store);

        first_router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("first router provisions and executes");
        cleanup_router.destroy_session_hands(&session.id).await;

        assert_eq!(provider.destroy_calls(), 1);
    }

    #[tokio::test]
    async fn lifecycle_cached_active_hand_is_renewed_and_stale_cache_not_reused() {
        // Pins: cached durable hands are revalidated and renewed before reuse.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("durable-cache-fence"));
        let session = session();
        let router = router(provider.clone(), lease_store.clone());

        router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("first execution provisions");
        let first = lease_store
            .get(session.id, provider.provider_name())
            .await
            .expect("load first lease")
            .expect("first lease should exist");
        let short_expiry = Utc::now() + ChronoDuration::seconds(5);
        assert!(
            lease_store
                .renew_active(
                    session.id,
                    provider.provider_name(),
                    first.generation,
                    short_expiry,
                )
                .await
                .expect("shrink active lease expiry")
        );

        router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("second execution reuses renewed lease");
        let renewed = lease_store
            .get(session.id, provider.provider_name())
            .await
            .expect("load renewed lease")
            .expect("renewed lease should exist");
        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(renewed.generation, first.generation);
        assert!(
            renewed.expires_at > short_expiry,
            "reuse should renew the active durable lease"
        );

        lease_store
            .mark_status(
                session.id,
                provider.provider_name(),
                renewed.generation,
                HandLeaseStatus::Stale,
            )
            .await
            .expect("mark lease stale");
        let replacement_result = tokio::time::timeout(
            Duration::from_secs(1),
            router.execute_authorized_with_recovery(&session, &bash_invocation()),
        )
        .await;
        match replacement_result {
            Ok(result) => {
                result.expect("stale durable lease should be replaced");
            }
            Err(error) => {
                let lease = lease_store
                    .get(session.id, provider.provider_name())
                    .await
                    .expect("load lease after replacement timeout");
                panic!(
                    "stale durable lease replacement should not wait on provisioning; timeout={error:?}; lease={lease:?}"
                );
            }
        }

        let replacement = lease_store
            .get(session.id, provider.provider_name())
            .await
            .expect("load replacement lease")
            .expect("replacement lease should exist");
        assert_eq!(provider.provision_calls(), 2);
        assert_eq!(replacement.generation, renewed.generation + 1);
        assert_eq!(replacement.status, HandLeaseStatus::Active);
    }

    #[tokio::test]
    async fn lifecycle_activation_fence_loss_destroys_new_hand() {
        // Pins: a hand created after a lost activation fence is destroyed before returning error.
        let lease_store = MemoryHandLeaseStore::shared();
        let session = session();
        let provider = Arc::new(
            CountingProvider::new("activation-fence")
                .with_stale_generation_on_provision(lease_store.clone(), session.id),
        );
        let router = router(provider.clone(), lease_store.clone());

        let error = router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect_err("activation fence loss should fail execution");

        assert!(
            error.to_string().contains("generation fence"),
            "error should report activation fence loss: {error}"
        );
        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, provider.provider_name())
            .await
            .expect("load lease after fence loss")
            .expect("lease row should remain");
        assert_eq!(lease.status, HandLeaseStatus::Stale);
    }

    #[tokio::test]
    async fn lifecycle_destroy_session_failed_destroy_remains_retryable() {
        // Pins: cleanup marks a durable lease destroyed only after provider destroy succeeds.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("destroy-retry").with_destroy_failure());
        let session = session();
        let first_router = router(provider.clone(), lease_store.clone());
        let cleanup_router = router(provider.clone(), lease_store.clone());

        first_router
            .execute_authorized_with_recovery(&session, &bash_invocation())
            .await
            .expect("provision before cleanup");
        cleanup_router.destroy_session_hands(&session.id).await;

        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, provider.provider_name())
            .await
            .expect("load lease after failed cleanup")
            .expect("lease should remain");
        assert_eq!(lease.status, HandLeaseStatus::Active);
    }

    #[test]
    fn lifecycle_provisioning_wait_budget_tracks_tool_timeout() {
        // Pins: durable lease wait budget is tied to provider/tool timeout, not a fixed 2 seconds.
        assert_eq!(
            provisioning_wait_budget(Duration::from_secs(7)),
            Duration::from_secs(7)
        );
    }
}

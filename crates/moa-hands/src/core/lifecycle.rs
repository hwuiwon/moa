//! Workspace-root tracking and lazy hand lifecycle management.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::MoaError, error::Result, types::hands::HandHandle, types::hands::HandResources,
    types::hands::HandSpec, types::hands::HandStatus, types::hands::SandboxFile,
    types::hands::SandboxTier, types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_observability::{current_turn_root_span, record_sandbox_provision_duration};
use tracing::Instrument;

use super::leases::{HandLease, HandLeaseStatus, LeaseHandle};
use super::{DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, ToolRouter};

/// Builds a sandbox-provisioning span parented to the active turn root when present.
///
/// `operation` names the lifecycle stage (cache-aware dispatch, cold provision, or
/// reprovision) so provisioning spans stay distinguishable in traces without
/// putting any tenant-controlled data in the span name. `moa.sandbox.id` and
/// `moa.sandbox.cold_start_ms` are declared empty and recorded once the caller
/// knows the provisioned handle and, when a cold provision happened, its timing.
fn sandbox_provision_span(
    operation: &'static str,
    provider: &str,
    tier: &'static str,
) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "sandbox_provision",
            otel.name = %format!("sandbox_provision {operation}"),
            moa.sandbox.id = tracing::field::Empty,
            moa.sandbox.provider = %provider,
            moa.sandbox.tier = %tier,
            moa.sandbox.cold_start_ms = tracing::field::Empty,
        ),
        None => tracing::info_span!(
            "sandbox_provision",
            otel.name = %format!("sandbox_provision {operation}"),
            moa.sandbox.id = tracing::field::Empty,
            moa.sandbox.provider = %provider,
            moa.sandbox.tier = %tier,
            moa.sandbox.cold_start_ms = tracing::field::Empty,
        ),
    }
}

const HAND_LEASE_TTL_SECS: i64 = 60 * 60;
/// Remaining-TTL threshold below which a reused active lease is renewed.
///
/// Reusing a cached durable hand only rewrites the lease once less than half the
/// TTL remains, so the hot path avoids a lease UPDATE on every tool call.
const HAND_LEASE_RENEW_THRESHOLD_SECS: i64 = HAND_LEASE_TTL_SECS / 2;
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

    /// Stores trusted files that should be installed lazily before hand tool execution.
    ///
    /// `worker_id` scopes the manifest: `None` is the session-level scope used
    /// today; clearing one scope leaves other workers' manifests untouched.
    pub async fn set_trusted_sandbox_files(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        files: Vec<SandboxFile>,
    ) {
        let scope = scope_key(session, worker_id);
        if files.is_empty() {
            self.trusted_sandbox_files.write().await.remove(&scope);
            let scope_prefix = format!("{scope}:");
            self.installed_files
                .write()
                .await
                .retain(|key, _| !key.starts_with(&scope_prefix));
            return;
        }

        self.trusted_sandbox_files
            .write()
            .await
            .insert(scope, files);
    }

    pub(super) async fn install_trusted_files_for_hand(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        provider: &str,
        handle: &HandHandle,
    ) -> Result<()> {
        let files = self
            .trusted_sandbox_files
            .read()
            .await
            .get(&scope_key(session, worker_id))
            .cloned()
            .unwrap_or_default();
        if files.is_empty() {
            return Ok(());
        }
        self.install_files_on_handle(session, worker_id, provider, handle, &files)
            .await
    }

    async fn install_files_on_handle(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        provider: &str,
        handle: &HandHandle,
        files: &[SandboxFile],
    ) -> Result<()> {
        let key = session_provider_key(session, worker_id, provider);
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

    /// Destroys and removes cached and durably leased hands for a session,
    /// either across the whole session or for one worker scope.
    ///
    /// `scope` selects what is reclaimed:
    /// - `None` tears down the entire session — every worker scope under it
    ///   (cache keys `"{session_id}:*:{provider}"` and leases
    ///   `(session_id, *, provider)`) — so the session-level (coordinator) and
    ///   all worker hands are released.
    /// - `Some(worker_id)` reclaims only that `(session_id, worker_id)` scope, so
    ///   a finishing worker releases its own sandbox without over-releasing the
    ///   parent's or siblings' shared hands. It clears the cache keys with prefix
    ///   `"{session_id}:{worker_id}:"`, that scope's `installed_files`/
    ///   `trusted_sandbox_files` entries, and the durable leases
    ///   `(session_id, worker_id, provider)` across providers; the parent's
    ///   session-level (`""`) scope and any sibling worker scopes are untouched.
    ///
    /// Returns `true` when every matched hand was destroyed and its durable lease
    /// marked `Destroyed`. A `false` result means at least one release step
    /// failed and the affected leases stay reclaimable, so a worker caller can
    /// reschedule cleanup instead of clearing its state. Session teardown ignores
    /// the result and lets the lease TTL reclaim any straggler.
    pub async fn reclaim_hands(
        &self,
        session_id: &moa_core::types::identifiers::SessionId,
        scope: Option<&str>,
    ) -> bool {
        let mut complete = true;
        // Every cache/lease key for a session shares the `"{session_id}:"`
        // prefix. Whole-session teardown matches that prefix; a worker scope
        // narrows it to `"{session_id}:{worker_id}:"`. The trailing `:` keeps the
        // worker prefix collision-safe between sibling ids where one is a prefix
        // of another (e.g. `sub` vs `sub-x`).
        let session_prefix = format!("{session_id}:");
        let match_prefix = match scope {
            Some(worker_id) => format!("{session_prefix}{worker_id}:"),
            None => session_prefix.clone(),
        };

        let mut hands = {
            let mut active_hands = self.active_hands.write().await;
            let keys = active_hands
                .keys()
                .filter(|key| key.starts_with(&match_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| {
                    active_hands
                        .remove(&key)
                        .map(|handle| (key, (handle, None::<i64>)))
                })
                .collect::<HashMap<_, _>>()
        };
        self.installed_files
            .write()
            .await
            .retain(|key, _| !key.starts_with(&match_prefix));
        match scope {
            Some(worker_id) => {
                let scope_key = format!("{session_prefix}{worker_id}");
                self.preferred_hand_routes.write().await.remove(&scope_key);
            }
            None => {
                self.preferred_hand_routes
                    .write()
                    .await
                    .retain(|key, _| !key.starts_with(&session_prefix));
            }
        }
        match scope {
            // `trusted_sandbox_files` is keyed by the bare scope key
            // `"{session_id}:{worker_id}"`, so a worker clears its own entry
            // exactly rather than by prefix; whole-session teardown clears every
            // scope under the session prefix.
            Some(worker_id) => {
                let scope_key = format!("{session_prefix}{worker_id}");
                self.trusted_sandbox_files.write().await.remove(&scope_key);
            }
            None => {
                self.trusted_sandbox_files
                    .write()
                    .await
                    .retain(|key, _| !key.starts_with(&session_prefix));
            }
        }

        if let Some(lease_store) = &self.hand_leases {
            match lease_store.list_session(*session_id).await {
                Ok(leases) => {
                    for lease in leases {
                        if let Some(worker_id) = scope
                            && lease.worker_id != worker_id
                        {
                            continue;
                        }
                        if lease.status == HandLeaseStatus::Destroyed {
                            continue;
                        }
                        let key = format!(
                            "{}:{}:{}",
                            lease.session_id, lease.worker_id, lease.provider
                        );
                        match hands.entry(key) {
                            Entry::Occupied(mut entry) => {
                                // The cached hand was already drained above; carry
                                // its durable generation so the destroy loop marks
                                // the right generation without a second store read.
                                entry.get_mut().1 = Some(lease.generation);
                            }
                            Entry::Vacant(entry) => {
                                let Some(lease_handle) = lease.handle.as_ref() else {
                                    continue;
                                };
                                match self
                                    .hydrate_lease_handle(&lease.provider, lease_handle)
                                    .await
                                {
                                    Ok(handle) => {
                                        entry.insert((handle, Some(lease.generation)));
                                    }
                                    Err(error) => {
                                        complete = false;
                                        tracing::warn!(
                                            session_id = %session_id,
                                            worker_id = %lease.worker_id,
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
                }
                Err(error) => {
                    complete = false;
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to list durable hand leases during cleanup"
                    );
                }
            }
        }

        for (key, (handle, generation)) in hands {
            // Cache keys are `"{session_id}:{worker_id}:{provider}"`; strip the
            // session prefix and split off the final segment (no provider name
            // contains `:`) to recover the worker scope and provider name.
            let remainder = key.strip_prefix(&session_prefix).unwrap_or_default();
            let (worker_id, provider_name) = remainder.rsplit_once(':').unwrap_or(("", remainder));
            let handle_id = hand_id(&handle);
            let Some(provider) = self.providers.get(provider_name) else {
                complete = false;
                tracing::warn!(
                    session_id = %session_id,
                    worker_id = %worker_id,
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
                        worker_id = %worker_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        "destroyed cached hand"
                    );
                    // `list_session` already loaded this lease's generation, so
                    // mark it destroyed with that fence (mark_status no-ops on a
                    // generation mismatch) instead of re-fetching. A cached hand
                    // with no matching live lease carries no generation.
                    if let Some(generation) = generation
                        && let Some(lease_store) = &self.hand_leases
                        && let Err(error) = lease_store
                            .mark_status(
                                *session_id,
                                worker_id,
                                provider_name,
                                generation,
                                HandLeaseStatus::Destroyed,
                            )
                            .await
                    {
                        complete = false;
                        tracing::warn!(
                            session_id = %session_id,
                            worker_id = %worker_id,
                            provider = %provider_name,
                            generation = generation,
                            error = %error,
                            "failed to mark durable hand lease destroyed"
                        );
                    }
                }
                Err(error) => {
                    complete = false;
                    tracing::warn!(
                        session_id = %session_id,
                        worker_id = %worker_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        error = %error,
                        "failed to destroy cached hand"
                    );
                }
            }
        }
        complete
    }

    pub(super) async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
        worker_id: Option<&str>,
    ) -> Result<HandHandle> {
        let tier_label = sandbox_tier_label(&tier);
        let span = sandbox_provision_span("get_or_provision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            let key = session_provider_key(session, worker_id, provider);
            let handle = if self.hand_leases.is_some() {
                let cached_handle = self.active_hands.read().await.get(&key).cloned();
                if let Some(handle) = cached_handle
                    && let Some(validated) = self
                        .validate_cached_durable_hand(provider, session, worker_id, &key, &handle)
                        .await?
                {
                    validated
                } else {
                    self.get_or_provision_durable_hand(provider, tier, session, worker_id, key)
                        .await?
                }
            } else if let Some(handle) = self.active_hands.read().await.get(&key) {
                handle.clone()
            } else {
                self.provision_uncached_hand(provider, tier, session, key)
                    .await?
            };
            record_span.record("moa.sandbox.id", hand_id(&handle));
            Ok(handle)
        }
        .instrument(span)
        .await
    }

    async fn validate_cached_durable_hand(
        &self,
        provider: &str,
        session: &SessionMeta,
        worker_id: Option<&str>,
        key: &str,
        cached_handle: &HandHandle,
    ) -> Result<Option<HandHandle>> {
        let scope = worker_id.unwrap_or_default();
        let Some(lease_store) = &self.hand_leases else {
            return Ok(Some(cached_handle.clone()));
        };
        let Some(lease) = lease_store.get(session.id, scope, provider).await? else {
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
                    worker_id = %scope,
                    provider,
                    generation = lease.generation,
                    error = %error,
                    "failed to hydrate cached durable hand lease; marking stale"
                );
                self.remove_cached_hand_if_matches(key, cached_handle).await;
                lease_store
                    .mark_status(
                        session.id,
                        scope,
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
        // Renewing on every call issues a lease UPDATE despite the long TTL. The
        // `get` above already confirmed the lease is Active and unexpired and the
        // hydrated handle matches the cached one, so the cached hand is safe to
        // reuse without a write. Only extend (and re-fence the generation via the
        // renew) once the remaining TTL has dropped below half.
        if lease_renewal_due(lease.expires_at)
            && !lease_store
                .renew_active(
                    session.id,
                    scope,
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
        worker_id: Option<&str>,
        key: String,
    ) -> Result<HandHandle> {
        let scope = worker_id.unwrap_or_default();
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
                    scope,
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
                                            scope,
                                            provider,
                                            claim.generation,
                                            HandLeaseStatus::Failed,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            session_id = %session.id,
                                            worker_id = %scope,
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
                                scope,
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
                                scope,
                                provider,
                                claim.generation,
                                HandLeaseStatus::Failed,
                            )
                            .await
                        {
                            tracing::warn!(
                                session_id = %session.id,
                                worker_id = %scope,
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

            if let Some(lease) = lease_store.get(session.id, scope, provider).await? {
                match lease.status {
                    HandLeaseStatus::Active if lease.expires_at > Utc::now() => {
                        match self.resume_durable_lease(provider, &lease, &key).await {
                            Ok(handle) => {
                                if lease_store
                                    .renew_active(
                                        session.id,
                                        scope,
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
                                    worker_id = %scope,
                                    provider,
                                    generation = lease.generation,
                                    error = %error,
                                    "durable hand lease could not be resumed; marking stale"
                                );
                                lease_store
                                    .mark_status(
                                        session.id,
                                        scope,
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
                                scope,
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
        let tier_label = sandbox_tier_label(&tier);
        let span = sandbox_provision_span("provision_uncached_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            let provider_impl = self.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
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
            let cold_start = started_at.elapsed();
            record_sandbox_provision_duration(provider, tier_label, cold_start);
            record_span.record("moa.sandbox.id", hand_id(&handle));
            record_span.record("moa.sandbox.cold_start_ms", cold_start.as_millis() as i64);

            self.active_hands.write().await.insert(key, handle.clone());
            Ok(handle)
        }
        .instrument(span)
        .await
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
        worker_id: Option<&str>,
        provider: &str,
        tier: &SandboxTier,
    ) -> Result<HandHandle> {
        let tier_label = sandbox_tier_label(tier);
        let span = sandbox_provision_span("reprovision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            let scope = worker_id.unwrap_or_default();
            let key = session_provider_key(session, worker_id, provider);
            let old_handle = self.active_hands.write().await.remove(&key);
            let provider_impl = self.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;

            if let Some(handle) = old_handle.as_ref()
                && let Err(error) = provider_impl.destroy(handle).await
            {
                tracing::warn!(
                    session_id = %session.id,
                    worker_id = %scope,
                    provider,
                    hand_id = %hand_id(handle),
                    error = %error,
                    "failed to destroy unhealthy hand before re-provisioning"
                );
            }

            if let Some(lease_store) = &self.hand_leases
                && let Ok(Some(lease)) = lease_store.get(session.id, scope, provider).await
                && let Err(error) = lease_store
                    .mark_status(
                        session.id,
                        scope,
                        provider,
                        lease.generation,
                        HandLeaseStatus::Stale,
                    )
                    .await
            {
                tracing::warn!(
                    session_id = %session.id,
                    worker_id = %scope,
                    provider,
                    generation = lease.generation,
                    error = %error,
                    "failed to mark durable hand lease stale before re-provisioning"
                );
            }

            let started_at = Instant::now();
            let handle = self
                .get_or_provision_hand(provider, tier.clone(), session, worker_id)
                .await?;
            let cold_start = started_at.elapsed();
            record_span.record("moa.sandbox.id", hand_id(&handle));
            record_span.record("moa.sandbox.cold_start_ms", cold_start.as_millis() as i64);
            if let Some(files) = self.installed_files.read().await.get(&key).cloned() {
                provider_impl.install_files(&handle, &files).await?;
            }
            Ok(handle)
        }
        .instrument(span)
        .await
    }
}

/// Returns the scope key that namespaces a session's hands by worker.
///
/// The session-level (coordinator) scope is `None`, which yields
/// `"{session_id}:"` (an empty worker segment). A worker scope yields
/// `"{session_id}:{worker_id}"`. All scope keys share the `"{session_id}:"`
/// prefix so session teardown can match every worker scope at once.
pub(super) fn scope_key(session: &SessionMeta, worker_id: Option<&str>) -> String {
    format!("{}:{}", session.id, worker_id.unwrap_or_default())
}

/// Returns the cache/lease key for one hand within a worker scope.
///
/// The format is `"{session_id}:{worker_id}:{provider}"`. For the session-level
/// scope (`worker_id` is `None`) the middle segment is empty, giving
/// `"{session_id}::{provider}"`, which keeps behavior identical to the prior
/// single-scope world while reserving room for per-worker sandboxes.
pub(super) fn session_provider_key(
    session: &SessionMeta,
    worker_id: Option<&str>,
    provider: &str,
) -> String {
    format!("{}:{provider}", scope_key(session, worker_id))
}

fn tenant_key(session: &SessionMeta) -> TenantId {
    session.tenant_id
}

fn hand_lease_expires_at() -> chrono::DateTime<Utc> {
    Utc::now() + ChronoDuration::seconds(HAND_LEASE_TTL_SECS)
}

/// Returns whether a reused active lease should be renewed based on remaining TTL.
fn lease_renewal_due(expires_at: chrono::DateTime<Utc>) -> bool {
    expires_at.signed_duration_since(Utc::now())
        < ChronoDuration::seconds(HAND_LEASE_RENEW_THRESHOLD_SECS)
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
        traits::{HandProvider, Identity, IdentityType},
        types::action_policy::ActionClass,
        types::action_policy::ActionPolicyEffect,
        types::action_policy::RiskLevel,
        types::completion::ToolInvocation,
        types::identifiers::ToolCallId,
        types::tools::IdempotencyClass,
        types::tools::ToolDiffStrategy,
        types::tools::ToolInputShape,
        types::tools::ToolOutput,
        types::tools::ToolPolicySpec,
    };
    use serde_json::json;

    use crate::core::leases::{HandLeaseStore, MemoryHandLeaseStore};
    use crate::core::{HandRoute, ToolRegistry, ToolRouter};

    use super::*;

    struct CountingProvider {
        name: String,
        provision_delay: Duration,
        stale_generation_on_provision: Option<(
            Arc<MemoryHandLeaseStore>,
            moa_core::types::identifiers::SessionId,
        )>,
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
            session_id: moa_core::types::identifiers::SessionId,
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
                    .mark_status(*session_id, "", &self.name, 1, HandLeaseStatus::Stale)
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
        registry.retarget_hand_tools(vec![HandRoute {
            provider: provider.provider_name().to_string(),
            tier: SandboxTier::Container,
        }]);
        registry.retain_only(["bash"]);
        let provider_trait: Arc<dyn HandProvider> = provider;
        let mut providers = HashMap::new();
        providers.insert(provider_trait.provider_name().to_string(), provider_trait);
        ToolRouter::new(registry, providers).with_hand_lease_store(lease_store)
    }

    fn session() -> SessionMeta {
        let identity = identity();
        SessionMeta {
            id: moa_core::types::identifiers::SessionId::new(),
            tenant_id: identity.tenant_id,
            ..SessionMeta::default()
        }
    }

    fn identity() -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c311),
            tenant_id: TenantId::from(uuid::Uuid::from_u128(
                0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c312,
            )),
            api_key_id: None,
            acting_on_behalf_of: None,
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
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("first router provisions and executes");
        second_router
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
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
        let left_identity = identity();
        let right_identity = identity();
        let left_invocation = bash_invocation();
        let right_invocation = bash_invocation();

        let secured = tokio::join!(
            left_router.execute_authorized_with_recovery(
                &left_session,
                &left_identity,
                None,
                &left_invocation,
                ToolCallId::new(),
                None,
            ),
            right_router.execute_authorized_with_recovery(
                &right_session,
                &right_identity,
                None,
                &right_invocation,
                ToolCallId::new(),
                None,
            )
        );

        let (left, right) = secured;

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
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("first router provisions and executes");
        cleanup_router.reclaim_hands(&session.id, None).await;

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
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("first execution provisions");
        let first = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load first lease")
            .expect("first lease should exist");
        let short_expiry = Utc::now() + ChronoDuration::seconds(5);
        assert!(
            lease_store
                .renew_active(
                    session.id,
                    "",
                    provider.provider_name(),
                    first.generation,
                    short_expiry,
                )
                .await
                .expect("shrink active lease expiry")
        );

        router
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("second execution reuses renewed lease");
        let renewed = lease_store
            .get(session.id, "", provider.provider_name())
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
                "",
                provider.provider_name(),
                renewed.generation,
                HandLeaseStatus::Stale,
            )
            .await
            .expect("mark lease stale");
        let replacement_result = tokio::time::timeout(
            Duration::from_secs(1),
            router.execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            ),
        )
        .await;
        match replacement_result {
            Ok(result) => {
                result.expect("stale durable lease should be replaced");
            }
            Err(error) => {
                let lease = lease_store
                    .get(session.id, "", provider.provider_name())
                    .await
                    .expect("load lease after replacement timeout");
                panic!(
                    "stale durable lease replacement should not wait on provisioning; timeout={error:?}; lease={lease:?}"
                );
            }
        }

        let replacement = lease_store
            .get(session.id, "", provider.provider_name())
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
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect_err("activation fence loss should fail execution");

        assert!(
            error.to_string().contains("generation fence"),
            "error should report activation fence loss: {error}"
        );
        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, "", provider.provider_name())
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
            .execute_authorized_with_recovery(
                &session,
                &identity(),
                None,
                &bash_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect("provision before cleanup");
        cleanup_router.reclaim_hands(&session.id, None).await;

        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load lease after failed cleanup")
            .expect("lease should remain");
        assert_eq!(lease.status, HandLeaseStatus::Active);
    }

    #[tokio::test]
    async fn lifecycle_worker_scope_isolates_hands_and_leases() {
        // Pins: a worker scope provisions its own hand/lease, distinct from the session scope.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("scope-isolation"));
        let session = session();
        let router = router(provider.clone(), lease_store.clone());

        let root = router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                None,
            )
            .await
            .expect("session-scope hand provisions");
        let child = router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                Some("sub-x"),
            )
            .await
            .expect("worker-scope hand provisions");

        assert_ne!(root, child, "each scope must own a distinct hand");
        assert_eq!(
            provider.provision_calls(),
            2,
            "each scope provisions its own sandbox"
        );

        let root_lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load session lease")
            .expect("session lease exists");
        let child_lease = lease_store
            .get(session.id, "sub-x", provider.provider_name())
            .await
            .expect("load worker lease")
            .expect("worker lease exists");
        assert_eq!(root_lease.worker_id, "");
        assert_eq!(child_lease.worker_id, "sub-x");
        assert_ne!(
            root_lease.handle, child_lease.handle,
            "scoped leases hold distinct durable handles"
        );
    }

    #[tokio::test]
    async fn lifecycle_destroy_session_releases_all_worker_scopes() {
        // Pins: session teardown reclaims both the session-scope and worker-scope hands.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("scope-teardown"));
        let session = session();
        let router = router(provider.clone(), lease_store.clone());

        router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                None,
            )
            .await
            .expect("session-scope hand provisions");
        router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                Some("sub-x"),
            )
            .await
            .expect("worker-scope hand provisions");
        assert_eq!(provider.provision_calls(), 2);

        router.reclaim_hands(&session.id, None).await;

        assert_eq!(
            provider.destroy_calls(),
            2,
            "teardown releases every scope under the session"
        );
        let root_lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load session lease")
            .expect("session lease row remains");
        let child_lease = lease_store
            .get(session.id, "sub-x", provider.provider_name())
            .await
            .expect("load worker lease")
            .expect("worker lease row remains");
        assert_eq!(root_lease.status, HandLeaseStatus::Destroyed);
        assert_eq!(child_lease.status, HandLeaseStatus::Destroyed);
    }

    #[tokio::test]
    async fn lifecycle_destroy_worker_releases_only_target_scope() {
        // Pins: a finishing worker releases ONLY its own scope's hand/lease and leaves
        // the session-scope and a sibling worker's hand/lease intact (no over-release).
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("scope-release"));
        let session = session();
        let router = router(provider.clone(), lease_store.clone());

        for scope in [None, Some("sub-x"), Some("sub-y")] {
            router
                .get_or_provision_hand(
                    provider.provider_name(),
                    SandboxTier::Container,
                    &session,
                    scope,
                )
                .await
                .expect("scope hand provisions");
        }
        assert_eq!(provider.provision_calls(), 3);

        assert!(
            router.reclaim_hands(&session.id, Some("sub-x")).await,
            "target worker cleanup should fully complete"
        );

        assert_eq!(
            provider.destroy_calls(),
            1,
            "only the target worker's hand is destroyed"
        );
        let session_lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load session lease")
            .expect("session lease row remains");
        let target_lease = lease_store
            .get(session.id, "sub-x", provider.provider_name())
            .await
            .expect("load target lease")
            .expect("target lease row remains");
        let sibling_lease = lease_store
            .get(session.id, "sub-y", provider.provider_name())
            .await
            .expect("load sibling lease")
            .expect("sibling lease row remains");
        assert_eq!(
            target_lease.status,
            HandLeaseStatus::Destroyed,
            "target scope lease is destroyed"
        );
        assert_eq!(
            session_lease.status,
            HandLeaseStatus::Active,
            "session-level scope lease is left intact"
        );
        assert_eq!(
            sibling_lease.status,
            HandLeaseStatus::Active,
            "sibling worker lease is left intact"
        );

        // The intact scopes are still cached/active, so reusing them does not re-provision;
        // the destroyed target scope re-provisions on next demand.
        router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                None,
            )
            .await
            .expect("session-scope hand reused");
        router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                Some("sub-y"),
            )
            .await
            .expect("sibling-scope hand reused");
        assert_eq!(
            provider.provision_calls(),
            3,
            "intact scopes are reused, not re-provisioned"
        );
        router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                Some("sub-x"),
            )
            .await
            .expect("destroyed scope re-provisions");
        assert_eq!(
            provider.provision_calls(),
            4,
            "the released target scope re-provisions on next demand"
        );
    }

    #[tokio::test]
    async fn lifecycle_destroy_worker_failed_destroy_remains_retryable() {
        // Pins: worker cleanup reports incomplete and leaves the lease active when provider
        // destroy fails, so the orchestrator can retry instead of clearing the worker.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider =
            Arc::new(CountingProvider::new("worker-destroy-retry").with_destroy_failure());
        let session = session();
        let first_router = router(provider.clone(), lease_store.clone());
        let cleanup_router = router(provider.clone(), lease_store.clone());

        first_router
            .get_or_provision_hand(
                provider.provider_name(),
                SandboxTier::Container,
                &session,
                Some("sub-x"),
            )
            .await
            .expect("worker scope provisions");
        assert!(
            !cleanup_router
                .reclaim_hands(&session.id, Some("sub-x"))
                .await,
            "failed provider destroy should report incomplete cleanup"
        );

        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, "sub-x", provider.provider_name())
            .await
            .expect("load worker lease after failed cleanup")
            .expect("worker lease should remain");
        assert_eq!(lease.status, HandLeaseStatus::Active);
    }

    #[test]
    fn lifecycle_lease_renewal_is_deferred_until_half_ttl_remains() {
        // Pins: a freshly-renewed durable lease is not rewritten on reuse; the
        // renew (and its generation fence) only fires once less than half the TTL
        // remains, keeping the hot path free of a lease UPDATE per tool call.
        assert!(
            !lease_renewal_due(Utc::now() + ChronoDuration::seconds(HAND_LEASE_TTL_SECS - 60)),
            "a nearly-full lease should not be renewed on reuse"
        );
        assert!(
            lease_renewal_due(Utc::now() + ChronoDuration::seconds(60)),
            "a lease with well under half the TTL remaining should be renewed"
        );
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

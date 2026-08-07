//! Workspace-root tracking and lazy hand lifecycle management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::MoaError, error::Result, types::hands::EffectiveSandboxProfile,
    types::hands::HandHandle, types::hands::HandSpec, types::hands::HandStatus,
    types::hands::SandboxFile, types::hands::SandboxTier,
    types::identifiers::HandProvisioningOperationId, types::identifiers::TenantId,
    types::resource::ResourceBudget, types::session::SessionMeta,
};
use moa_observability::{current_turn_root_span, record_sandbox_provision_duration};
use tracing::Instrument;

use super::leases::{
    HandLease, HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStatus, LeaseHandle,
    provisioning_deadline,
};
use super::reaper::{ProvisioningAbsenceProof, destroy_provisioning_operations};
use super::{
    ActiveHand, DEFAULT_PROVIDER_NAME, DEFAULT_TOOL_TIMEOUT, HandRoute, HandScopeKey,
    InstalledManifestMarker, ToolCallScope, ToolRouter, TrustedSandboxManifest,
};

/// Builds a sandbox-provisioning span parented to the active turn root when present.
///
/// `operation` names the lifecycle stage (cache-aware dispatch, cold provision, or
/// reprovision) so provisioning spans stay distinguishable in traces without
/// putting any tenant-controlled data in the span name. `moa.sandbox.id`,
/// `moa.sandbox.cold_start_ms`, `moa.sandbox.profile_hash`, and
/// `moa.sandbox.egress_mode` are declared empty and recorded once the caller
/// knows the provisioned handle, the resolved policy, and — when a cold
/// provision happened — its timing.
///
/// Every field has to be declared here even though it is filled in later:
/// `Span::record` on a field the macro never declared is a silent no-op, so an
/// undeclared field looks like working telemetry and emits nothing.
///
/// Only the policy *identity* is emitted, never its contents: the hash and the
/// egress mode, never the allowlist destinations, mount paths, or environment.
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
            moa.sandbox.profile_hash = tracing::field::Empty,
            moa.sandbox.egress_mode = tracing::field::Empty,
        ),
        None => tracing::info_span!(
            "sandbox_provision",
            otel.name = %format!("sandbox_provision {operation}"),
            moa.sandbox.id = tracing::field::Empty,
            moa.sandbox.provider = %provider,
            moa.sandbox.tier = %tier,
            moa.sandbox.cold_start_ms = tracing::field::Empty,
            moa.sandbox.profile_hash = tracing::field::Empty,
            moa.sandbox.egress_mode = tracing::field::Empty,
        ),
    }
}

/// Idle window used when a profile declares an explicitly unbounded idle
/// timeout, which has no deadline of its own to renew.
///
/// Reusing a cached durable hand only rewrites the lease once less than half the
/// declared idle window remains, so the hot path avoids a lease UPDATE on every
/// tool call.
const HAND_LEASE_TTL_SECS: i64 = 60 * 60;
const HAND_LEASE_PROVISION_WAIT_MS: u64 = 25;
const HAND_DESTROY_CLAIM_TTL: StdDuration = StdDuration::from_secs(5 * 60);
const HAND_DESTROY_RETRY_DELAY: StdDuration = StdDuration::from_secs(15);

struct DurableHandProvisionContext<'a> {
    route: &'a HandRoute,
    session: &'a SessionMeta,
    worker_id: Option<&'a str>,
    key: String,
    effective: &'a EffectiveSandboxProfile,
    policy: &'a HandLeasePolicy,
    call_scope: ToolCallScope<'a>,
}

impl ToolRouter {
    /// Remembers the filesystem workspace root for one tenant.
    pub async fn remember_workspace_root(&self, tenant_id: TenantId, workspace_root: PathBuf) {
        self.hands
            .workspace_roots
            .write()
            .await
            .insert(tenant_id, workspace_root);
    }

    /// Returns the remembered filesystem workspace root for one tenant.
    pub async fn workspace_root(&self, tenant_id: &TenantId) -> Option<PathBuf> {
        self.hands
            .workspace_roots
            .read()
            .await
            .get(tenant_id)
            .cloned()
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
        let scope = manifest_scope_key(session, worker_id);
        let changed = {
            let mut manifests = self.hands.trusted_sandbox_files.write().await;
            if files.is_empty() {
                manifests.remove(&scope).is_some()
            } else if manifests
                .get(&scope)
                .is_some_and(|manifest| manifest.files.as_ref() == files.as_slice())
            {
                false
            } else {
                manifests.insert(
                    scope.clone(),
                    std::sync::Arc::new(TrustedSandboxManifest {
                        identity: uuid::Uuid::new_v4(),
                        files: files.into(),
                    }),
                );
                true
            }
        };
        if !changed {
            return;
        }

        // Installed markers are nested under the exact structured scope, so a
        // manifest change never scans or invalidates another session.
        self.hands.installed_files.write().await.remove(&scope);
    }

    pub(super) async fn install_trusted_files_for_hand(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        provider: &str,
        handle: &HandHandle,
        scope: ToolCallScope<'_>,
    ) -> Result<()> {
        let provider_impl =
            self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
        let manifest_key = manifest_scope_key(session, worker_id);
        let active_key = session_provider_key(session, worker_id, provider);

        loop {
            scope.admit()?;
            let manifest = self
                .hands
                .trusted_sandbox_files
                .read()
                .await
                .get(&manifest_key)
                .cloned();
            let Some(manifest) = manifest else {
                return Ok(());
            };
            let binding = self
                .hands
                .active_hands
                .read()
                .await
                .get(&active_key)
                .filter(|active| active.handle == *handle)
                .cloned()
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "hand {} was replaced before trusted files could be installed",
                        hand_id(handle)
                    ))
                })?;
            let already_installed = self
                .hands
                .installed_files
                .read()
                .await
                .get(&manifest_key)
                .and_then(|providers| providers.get(provider))
                .is_some_and(|installed| {
                    installed.manifest_identity == manifest.identity
                        && installed.handle == binding.handle
                        && installed.generation == binding.generation
                });
            if already_installed {
                return Ok(());
            }

            // Provider I/O runs with no manifest or installed-marker guard held.
            // If the trusted manifest changes concurrently, install its latest
            // snapshot again before returning so stale completion order cannot
            // leave the hand or cache behind the authoritative manifest.
            self.run_within_scope(
                scope,
                provider_impl.install_files(handle, manifest.files.as_ref()),
            )
            .await?;

            // No provider I/O occurs while lifecycle locks are held. Holding
            // these read guards only across marker publication makes the proof
            // atomic with respect to hand replacement and manifest mutation.
            let active_hands = self.hands.active_hands.read().await;
            let manifests = self.hands.trusted_sandbox_files.read().await;
            let binding_is_current = active_hands.get(&active_key) == Some(&binding);
            let manifest_is_current = manifests
                .get(&manifest_key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &manifest));
            if binding_is_current && manifest_is_current {
                self.hands
                    .installed_files
                    .write()
                    .await
                    .entry(manifest_key.clone())
                    .or_default()
                    .insert(
                        provider.to_string(),
                        InstalledManifestMarker {
                            manifest_identity: manifest.identity,
                            handle: binding.handle,
                            generation: binding.generation,
                        },
                    );
                return Ok(());
            }
            if !binding_is_current {
                return Err(MoaError::ProviderError(format!(
                    "hand {} was replaced while trusted files were being installed",
                    hand_id(handle)
                )));
            }
        }
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
    ///   `"{session_id}:{worker_id}:"`, that exact structured scope's
    ///   `installed_files`/`trusted_sandbox_files` entries, and the durable leases
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
        let session_prefix = format!("{session_id}:");
        let match_prefix = match scope {
            Some(worker_id) => format!("{session_prefix}{worker_id}:"),
            None => session_prefix.clone(),
        };
        match scope {
            Some(worker_id) => {
                let scope_key = format!("{session_prefix}{worker_id}");
                self.hands
                    .preferred_hand_routes
                    .write()
                    .await
                    .remove(&scope_key);
            }
            None => {
                self.hands
                    .preferred_hand_routes
                    .write()
                    .await
                    .retain(|key, _| !key.starts_with(&session_prefix));
            }
        }

        if let Some(lease_store) = &self.hands.hand_leases {
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
                        let claim_token = match lease_store
                            .claim_for_destroy(&lease, HAND_DESTROY_CLAIM_TTL)
                            .await
                        {
                            Ok(Some(claim_token)) => claim_token,
                            Ok(None) => {
                                // A newer generation or the durable reaper owns
                                // the row. Never steal or overwrite that fence.
                                complete = false;
                                continue;
                            }
                            Err(error) => {
                                complete = false;
                                tracing::warn!(
                                    session_id = %session_id,
                                    worker_id = %lease.worker_id,
                                    provider = %lease.provider,
                                    generation = lease.generation,
                                    error = %error,
                                    "failed to claim durable hand lease for cleanup"
                                );
                                continue;
                            }
                        };
                        let key = session_provider_key_from_parts(
                            lease.session_id,
                            &lease.worker_id,
                            &lease.provider,
                        );
                        if let Some(lease_handle) = lease.handle.as_ref() {
                            self.remove_cached_binding_if_matches(
                                &key,
                                &lease_handle.handle,
                                Some(lease.generation),
                            )
                            .await;
                        }
                        self.remove_installed_marker(
                            manifest_scope_key_from_parts(lease.session_id, &lease.worker_id),
                            &lease.provider,
                        )
                        .await;

                        let Some(provider) = self.hands.providers.get(&lease.provider) else {
                            complete = false;
                            let _ = lease_store
                                .release_destroy_claim(
                                    &lease,
                                    claim_token,
                                    HAND_DESTROY_RETRY_DELAY,
                                )
                                .await;
                            continue;
                        };
                        match destroy_provisioning_operations(
                            provider.as_ref(),
                            lease.provisioning_operation_id,
                            lease.handle.as_ref(),
                            if lease.handle.is_some()
                                && matches!(
                                    lease.status,
                                    HandLeaseStatus::Active | HandLeaseStatus::Stale
                                )
                            {
                                ProvisioningAbsenceProof::Immediate
                            } else {
                                ProvisioningAbsenceProof::Delayed
                            },
                        )
                        .await
                        {
                            Ok(()) => match lease_store.finalize_destroy(&lease, claim_token).await
                            {
                                Ok(true) => {}
                                Ok(false) => complete = false,
                                Err(error) => {
                                    complete = false;
                                    tracing::warn!(
                                        session_id = %session_id,
                                        worker_id = %lease.worker_id,
                                        provider = %lease.provider,
                                        generation = lease.generation,
                                        error = %error,
                                        "failed to finalize destroyed durable hand lease"
                                    );
                                }
                            },
                            Err(error) => {
                                complete = false;
                                if let Err(release_error) = lease_store
                                    .release_destroy_claim(
                                        &lease,
                                        claim_token,
                                        HAND_DESTROY_RETRY_DELAY,
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        worker_id = %lease.worker_id,
                                        provider = %lease.provider,
                                        generation = lease.generation,
                                        error = %release_error,
                                        "failed to release durable hand destroy claim"
                                    );
                                }
                                tracing::warn!(
                                    session_id = %session_id,
                                    worker_id = %lease.worker_id,
                                    provider = %lease.provider,
                                    generation = lease.generation,
                                    error = %error,
                                    "failed to reconcile claimed durable hand operation"
                                );
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
        } else {
            let hands = {
                let mut active_hands = self.hands.active_hands.write().await;
                let keys = active_hands
                    .keys()
                    .filter(|key| key.starts_with(&match_prefix))
                    .cloned()
                    .collect::<Vec<_>>();
                keys.into_iter()
                    .filter_map(|key| active_hands.remove(&key).map(|hand| (key, hand)))
                    .collect::<Vec<_>>()
            };
            for (key, hand) in hands {
                let remainder = key.strip_prefix(&session_prefix).unwrap_or_default();
                let (_, provider_name) = remainder.rsplit_once(':').unwrap_or(("", remainder));
                let Some(provider) = self.hands.providers.get(provider_name) else {
                    complete = false;
                    continue;
                };
                if let Err(error) = provider.destroy(&hand.handle).await {
                    complete = false;
                    tracing::warn!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %hand_id(&hand.handle),
                        error = %error,
                        "failed to destroy cached hand"
                    );
                }
            }
        }

        self.clear_manifest_scopes(*session_id, scope).await;
        complete
    }

    /// Provisions or reuses a hand on behalf of a run with `budget` left.
    ///
    /// The budget is stamped onto the [`HandSpec`], so a provider that can push
    /// a deadline into the sandbox itself sees the run's clock, not only the
    /// sandbox's much longer lifetime.
    pub(super) async fn get_or_provision_hand_within(
        &self,
        route: &HandRoute,
        session: &SessionMeta,
        worker_id: Option<&str>,
        scope: ToolCallScope<'_>,
    ) -> Result<HandHandle> {
        let provider = route.provider.as_str();
        let tier_label = route.tier.as_str();
        let span = sandbox_provision_span("get_or_provision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            scope.admit()?;
            // Policy resolution is read-only and may be abandoned at the
            // caller's deadline. Lifecycle mutations below re-admit immediately
            // before dispatch and then run their bookkeeping to completion.
            let effective = self
                .run_within_scope(scope, self.resolve_sandbox_profile(route, session))
                .await?;
            record_span.record("moa.sandbox.profile_hash", effective.profile_hash());
            record_span.record(
                "moa.sandbox.egress_mode",
                effective.profile().egress.mode().as_str(),
            );
            let policy = HandLeasePolicy::from_effective(&effective);
            let key = session_provider_key(session, worker_id, provider);
            let handle = if self.hands.hand_leases.is_some() {
                let cached = self.hands.active_hands.read().await.get(&key).cloned();
                if let Some(cached) = cached
                    && let Some(validated) = self
                        .validate_cached_durable_hand(
                            provider, session, worker_id, &cached, &policy, scope,
                        )
                        .await?
                {
                    validated
                } else {
                    self.get_or_provision_durable_hand(DurableHandProvisionContext {
                        route,
                        session,
                        worker_id,
                        key,
                        effective: &effective,
                        policy: &policy,
                        call_scope: scope,
                    })
                    .await?
                }
            } else {
                let cached = self.hands.active_hands.read().await.get(&key).cloned();
                if let Some(cached) = cached {
                    cached.handle
                } else {
                    scope.admit()?;
                    let provisioning_deadline_at =
                        provisioning_deadline(Utc::now(), scope.budget.deadline)?;
                    let handle = self
                        .create_hand(
                            route,
                            session,
                            &effective,
                            scope.budget,
                            HandProvisioningOperationId::new(),
                            provisioning_deadline_at,
                        )
                        .await?;
                    self.hands.active_hands.write().await.insert(
                        key,
                        ActiveHand {
                            handle: handle.clone(),
                            generation: None,
                        },
                    );
                    handle
                }
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
        cached: &ActiveHand,
        policy: &HandLeasePolicy,
        call_scope: ToolCallScope<'_>,
    ) -> Result<Option<HandHandle>> {
        let scope = worker_id.unwrap_or_default();
        let key = session_provider_key(session, worker_id, provider);
        let Some(lease_store) = &self.hands.hand_leases else {
            return Ok(Some(cached.handle.clone()));
        };
        let Some(lease) = lease_store.get(session.id, scope, provider).await? else {
            self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                .await;
            return Ok(None);
        };
        if lease.status != HandLeaseStatus::Active
            || lease_expired(&lease)
            || cached.generation != Some(lease.generation)
        {
            self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                .await;
            return Ok(None);
        }
        // The sandbox is reusable only if it was provisioned under exactly the
        // policy that resolves today. Any change to the profile, to one of the
        // five source revisions, or to the provider's capability revision moves
        // the hash, and the hand is replaced rather than reinterpreted.
        if !lease_matches_policy(&lease, policy) {
            self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                .await;
            tracing::info!(
                provider,
                generation = lease.generation,
                "durable hand lease no longer matches the resolved sandbox policy; replacing"
            );
            call_scope.admit()?;
            let _ = lease_store
                .transition_status(&lease, HandLeaseStatus::Stale)
                .await?;
            return Ok(None);
        }
        let Some(lease_handle) = lease.handle.as_ref() else {
            self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                .await;
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
                self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                    .await;
                call_scope.admit()?;
                let _ = lease_store
                    .transition_status(&lease, HandLeaseStatus::Stale)
                    .await?;
                return Ok(None);
            }
        };
        if durable_handle != cached.handle {
            self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                .await;
            return Ok(None);
        }
        // Renewing on every call issues a lease UPDATE despite the long idle
        // window. The `get` above already confirmed the lease is Active and
        // unexpired, the policy still matches, and the hydrated handle matches
        // the cached one, so the cached hand is safe to reuse without a write.
        // Only extend (and re-fence the generation via the renew) once the
        // remaining idle window has dropped below half.
        if let Some(idle_expires_at) = lease.idle_expires_at
            && lease_renewal_due(idle_expires_at, policy)
        {
            call_scope.admit()?;
            if !lease_store
                .renew_active(
                    session.id,
                    scope,
                    provider,
                    lease.generation,
                    lease.provisioning_operation_id,
                    next_idle_deadline(policy),
                )
                .await?
            {
                self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                    .await;
                return Ok(None);
            }
        }
        Ok(Some(cached.handle.clone()))
    }

    async fn remove_cached_binding_if_matches(
        &self,
        key: &str,
        expected_handle: &HandHandle,
        expected_generation: Option<i64>,
    ) {
        let mut active_hands = self.hands.active_hands.write().await;
        if active_hands.get(key).is_some_and(|active| {
            active.handle == *expected_handle && active.generation == expected_generation
        }) {
            active_hands.remove(key);
        }
    }

    async fn get_or_provision_durable_hand(
        &self,
        context: DurableHandProvisionContext<'_>,
    ) -> Result<HandHandle> {
        let DurableHandProvisionContext {
            route,
            session,
            worker_id,
            key,
            effective,
            policy,
            call_scope,
        } = context;
        let provider = route.provider.as_str();
        let scope = worker_id.unwrap_or_default();
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let wait_started = Instant::now();
        let wait_budget = provisioning_wait_budget(DEFAULT_TOOL_TIMEOUT);

        loop {
            call_scope.admit()?;
            if let Some(claim) = lease_store
                .claim_for_provisioning(HandLeaseProvisionRequest {
                    session_id: session.id,
                    worker_id: scope,
                    tenant_id: session.tenant_id,
                    provider,
                    tier: route.tier,
                    policy,
                    caller_deadline: call_scope.budget.deadline,
                })
                .await?
            {
                let mut claim = claim;
                if let Err(error) = call_scope.admit() {
                    let _ = lease_store
                        .transition_status(&claim, HandLeaseStatus::Failed)
                        .await?;
                    return Err(error);
                }
                if let Some(previous_handle) = claim.handle.as_ref() {
                    let Some(provider_impl) = self.hands.providers.get(provider) else {
                        let _ = lease_store
                            .transition_status(&claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(MoaError::ProviderError(format!(
                            "unknown hand provider: {provider}"
                        )));
                    };
                    if let Err(error) = call_scope.admit() {
                        let _ = lease_store
                            .transition_status(&claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                    // Once destroy is dispatched it is deliberately outside a
                    // DeadlineGuard. Its exact provisioning fence is finalized
                    // even if cancellation arrives while the provider runs.
                    if let Err(error) = destroy_provisioning_operations(
                        provider_impl.as_ref(),
                        claim.provisioning_operation_id,
                        Some(previous_handle),
                        ProvisioningAbsenceProof::Immediate,
                    )
                    .await
                    {
                        let _ = lease_store
                            .transition_status(&claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                    if !lease_store.clear_handle_for_provisioning(&claim).await? {
                        return Err(MoaError::StorageError(format!(
                            "hand lease replacement lost generation fence for session {} provider {provider}",
                            session.id
                        )));
                    }
                    claim.handle = None;
                    if let Err(error) = call_scope.admit() {
                        let _ = lease_store
                            .transition_status(&claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                }
                match self
                    .create_hand(
                        route,
                        session,
                        effective,
                        call_scope.budget,
                        claim.provisioning_operation_id,
                        claim.provisioning_deadline_at,
                    )
                    .await
                {
                    Ok(handle) => {
                        let lease_handle = match self
                            .lease_handle_for_provider(
                                provider,
                                claim.provisioning_operation_id,
                                &handle,
                            )
                            .await
                        {
                            Ok(lease_handle) => lease_handle,
                            Err(error) => {
                                self.destroy_unbound_operation(
                                    provider,
                                    claim.provisioning_operation_id,
                                    &handle,
                                )
                                .await;
                                if let Err(mark_error) = lease_store
                                    .transition_status(&claim, HandLeaseStatus::Failed)
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
                        match lease_store
                            .activate(
                                session.id,
                                scope,
                                provider,
                                claim.generation,
                                lease_handle.clone(),
                            )
                            .await
                        {
                            Ok(true) => {
                                // Durable activation is the publication fence:
                                // no process may observe this handle as active
                                // before the authoritative row does.
                                self.hands.active_hands.write().await.insert(
                                    key,
                                    ActiveHand {
                                        handle: handle.clone(),
                                        generation: Some(claim.generation),
                                    },
                                );
                                return Ok(handle);
                            }
                            Ok(false) => {
                                self.cleanup_after_activation_fence_loss(
                                    lease_store.as_ref(),
                                    &claim,
                                    provider,
                                    &lease_handle,
                                    &handle,
                                )
                                .await;
                                return Err(MoaError::StorageError(format!(
                                    "hand lease activation lost generation fence for session {} provider {provider}",
                                    session.id
                                )));
                            }
                            Err(error) => {
                                // A failed activation response is commit-ambiguous.
                                // Re-fence the original Provisioning row: if that
                                // transition wins, activation did not publish and
                                // the new hand is provably unbound. If it loses,
                                // the row may already be Active or owned by a
                                // newer generation, so destroying here would risk
                                // invalidating the durable binding.
                                match lease_store
                                    .transition_status(&claim, HandLeaseStatus::Failed)
                                    .await
                                {
                                    Ok(true) => {
                                        self.destroy_unbound_operation(
                                            provider,
                                            claim.provisioning_operation_id,
                                            &handle,
                                        )
                                        .await;
                                    }
                                    Ok(false) => {
                                        self.cleanup_after_activation_fence_loss(
                                            lease_store.as_ref(),
                                            &claim,
                                            provider,
                                            &lease_handle,
                                            &handle,
                                        )
                                        .await;
                                    }
                                    Err(mark_error) => tracing::warn!(
                                        session_id = %session.id,
                                        worker_id = %scope,
                                        provider,
                                        generation = claim.generation,
                                        error = %mark_error,
                                        "could not resolve ambiguous hand lease activation"
                                    ),
                                }
                                return Err(error);
                            }
                        }
                    }
                    Err(error) => {
                        if let Err(mark_error) = lease_store
                            .transition_status(&claim, HandLeaseStatus::Failed)
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
                    // A lease another replica activated is only reusable when it
                    // was provisioned under exactly the policy resolved above;
                    // otherwise it falls through to the stale branch and is
                    // replaced rather than adopted under a policy it never
                    // passed admission for.
                    HandLeaseStatus::Active
                        if !lease_expired(&lease) && lease_matches_policy(&lease, policy) =>
                    {
                        match self
                            .resume_durable_lease(provider, &lease, &key, call_scope)
                            .await
                        {
                            Ok(handle) => {
                                call_scope.admit()?;
                                if lease_store
                                    .renew_active(
                                        session.id,
                                        scope,
                                        provider,
                                        lease.generation,
                                        lease.provisioning_operation_id,
                                        next_idle_deadline(policy),
                                    )
                                    .await?
                                {
                                    return Ok(handle);
                                }
                                self.remove_cached_binding_if_matches(
                                    &key,
                                    &handle,
                                    Some(lease.generation),
                                )
                                .await;
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
                                call_scope.admit()?;
                                let _ = lease_store
                                    .transition_status(&lease, HandLeaseStatus::Stale)
                                    .await?;
                                continue;
                            }
                        }
                    }
                    HandLeaseStatus::Provisioning | HandLeaseStatus::Failed => {
                        if wait_started.elapsed() >= wait_budget {
                            break;
                        }
                        self.wait_for_provisioning(call_scope, wait_started, wait_budget)
                            .await?;
                        continue;
                    }
                    HandLeaseStatus::Active => {
                        call_scope.admit()?;
                        let _ = lease_store
                            .transition_status(&lease, HandLeaseStatus::Stale)
                            .await?;
                        continue;
                    }
                    // A generation the reaper owns is being destroyed. Waiting
                    // it out is correct: taking it back would race the destroy
                    // and hand out a sandbox that is about to disappear.
                    HandLeaseStatus::Reaping => {
                        if wait_started.elapsed() >= wait_budget {
                            break;
                        }
                        self.wait_for_provisioning(call_scope, wait_started, wait_budget)
                            .await?;
                        continue;
                    }
                    HandLeaseStatus::Stale | HandLeaseStatus::Destroyed => {
                        continue;
                    }
                }
            }

            if wait_started.elapsed() >= wait_budget {
                break;
            }
            self.wait_for_provisioning(call_scope, wait_started, wait_budget)
                .await?;
        }

        Err(MoaError::ProviderError(format!(
            "timed out waiting for durable hand lease for session {} provider {provider}",
            session.id
        )))
    }

    async fn create_hand(
        &self,
        route: &HandRoute,
        session: &SessionMeta,
        effective: &EffectiveSandboxProfile,
        budget: ResourceBudget,
        provisioning_operation_id: HandProvisioningOperationId,
        provisioning_deadline_at: chrono::DateTime<Utc>,
    ) -> Result<HandHandle> {
        let provider = route.provider.as_str();
        let tier = route.tier;
        let tier_label = tier.as_str();
        let span = sandbox_provision_span("create_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            let provider_impl = self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
            let workspace_mount =
                if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                    self.hands
                        .workspace_roots
                        .read()
                        .await
                        .get(&tenant_key(session))
                        .cloned()
                } else {
                    None
                };
            let remaining = (provisioning_deadline_at - Utc::now())
                .to_std()
                .unwrap_or(StdDuration::ZERO);
            if remaining.is_zero() {
                return Err(MoaError::ProviderError(format!(
                    "hand provisioning deadline elapsed before dispatch for provider {provider}"
                )));
            }
            let mut provisioning_budget = budget;
            provisioning_budget.deadline = Some(provisioning_deadline_at);
            let spec = HandSpec {
                provisioning_operation_id,
                sandbox_tier: tier,
                image: None,
                env: HashMap::new(),
                workspace_mount,
                effective_profile: effective.clone(),
                budget: provisioning_budget,
            };
            let started_at = Instant::now();
            let handle = tokio::time::timeout(remaining, provider_impl.provision(spec))
                .await
                .map_err(|_| {
                    MoaError::ProviderError(format!(
                        "hand provisioning exceeded its durable deadline for provider {provider}"
                    ))
                })??;
            let cold_start = started_at.elapsed();
            record_sandbox_provision_duration(provider, tier_label, cold_start);
            record_span.record("moa.sandbox.id", hand_id(&handle));
            record_span.record("moa.sandbox.cold_start_ms", cold_start.as_millis() as i64);

            Ok(handle)
        }
        .instrument(span)
        .await
    }

    async fn destroy_unbound_operation(
        &self,
        provider: &str,
        provisioning_operation_id: HandProvisioningOperationId,
        handle: &HandHandle,
    ) {
        let Some(provider_impl) = self.hands.providers.get(provider) else {
            tracing::warn!(
                provider,
                hand_id = %hand_id(handle),
                "provisioned hand provider missing during activation cleanup"
            );
            return;
        };
        let lease_handle = LeaseHandle::new(provisioning_operation_id, handle.clone());
        if let Err(error) = destroy_provisioning_operations(
            provider_impl.as_ref(),
            provisioning_operation_id,
            Some(&lease_handle),
            ProvisioningAbsenceProof::Delayed,
        )
        .await
        {
            tracing::warn!(
                provider,
                hand_id = %hand_id(handle),
                error = %error,
                "failed to reconcile provisioned hand after activation fence loss"
            );
        }
    }

    async fn cleanup_after_activation_fence_loss(
        &self,
        lease_store: &dyn super::leases::HandLeaseStore,
        claim: &HandLease,
        provider: &str,
        lease_handle: &LeaseHandle,
        handle: &HandHandle,
    ) {
        match lease_store
            .get(claim.session_id, &claim.worker_id, provider)
            .await
        {
            Ok(Some(lease))
                if lease.generation == claim.generation
                    && lease.provisioning_operation_id == claim.provisioning_operation_id
                    && lease.status == HandLeaseStatus::Active
                    && lease.handle.as_ref() == Some(lease_handle) => {}
            Ok(_) => {
                self.destroy_unbound_operation(provider, claim.provisioning_operation_id, handle)
                    .await;
            }
            Err(error) => tracing::warn!(
                session_id = %claim.session_id,
                worker_id = %claim.worker_id,
                provider,
                generation = claim.generation,
                error = %error,
                "could not reload an ambiguously activated hand lease; leaving cleanup to the durable reaper"
            ),
        }
    }

    async fn resume_durable_lease(
        &self,
        provider: &str,
        lease: &HandLease,
        key: &str,
        call_scope: ToolCallScope<'_>,
    ) -> Result<HandHandle> {
        let lease_handle = lease.handle.as_ref().ok_or_else(|| {
            MoaError::StorageError(format!(
                "active hand lease for session {} provider {provider} is missing a handle",
                lease.session_id
            ))
        })?;
        let handle = self.hydrate_lease_handle(provider, lease_handle).await?;
        let provider_impl =
            self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
        let status = self
            .run_within_scope(call_scope, provider_impl.status(&handle))
            .await?;
        match status {
            HandStatus::Running | HandStatus::Provisioning => {}
            HandStatus::Paused | HandStatus::Stopped => {
                call_scope.admit()?;
                provider_impl.resume(&handle).await?;
            }
            HandStatus::Destroyed | HandStatus::Failed => {
                return Err(MoaError::ProviderError(format!(
                    "durable hand lease {} for provider {provider} is not resumable",
                    hand_id(&handle)
                )));
            }
        }
        self.hands.active_hands.write().await.insert(
            key.to_string(),
            ActiveHand {
                handle: handle.clone(),
                generation: Some(lease.generation),
            },
        );
        Ok(handle)
    }

    async fn wait_for_provisioning(
        &self,
        scope: ToolCallScope<'_>,
        started_at: Instant,
        budget: StdDuration,
    ) -> Result<()> {
        self.run_within_scope(scope, async move {
            tokio::time::sleep(provisioning_poll_delay(started_at, budget)).await;
            Ok(())
        })
        .await
    }

    async fn lease_handle_for_provider(
        &self,
        provider: &str,
        provisioning_operation_id: HandProvisioningOperationId,
        handle: &HandHandle,
    ) -> Result<LeaseHandle> {
        if provider == DEFAULT_PROVIDER_NAME
            && let Some(local_provider) = &self.hands.local_provider
        {
            return local_provider
                .lease_handle(provisioning_operation_id, handle)
                .await;
        }
        Ok(LeaseHandle::new(provisioning_operation_id, handle.clone()))
    }

    async fn hydrate_lease_handle(
        &self,
        provider: &str,
        lease_handle: &LeaseHandle,
    ) -> Result<HandHandle> {
        if provider == DEFAULT_PROVIDER_NAME
            && let Some(local_provider) = &self.hands.local_provider
        {
            return local_provider.adopt_lease_handle(lease_handle).await;
        }
        Ok(lease_handle.handle.clone())
    }

    async fn remove_installed_marker(&self, scope: HandScopeKey, provider: &str) {
        let mut installed = self.hands.installed_files.write().await;
        let remove_scope = installed.get_mut(&scope).is_some_and(|providers| {
            providers.remove(provider);
            providers.is_empty()
        });
        if remove_scope {
            installed.remove(&scope);
        }
    }

    async fn clear_manifest_scopes(
        &self,
        session_id: moa_core::types::identifiers::SessionId,
        worker_id: Option<&str>,
    ) {
        match worker_id {
            Some(worker_id) => {
                let scope = manifest_scope_key_from_parts(session_id, worker_id);
                self.hands
                    .trusted_sandbox_files
                    .write()
                    .await
                    .remove(&scope);
                self.hands.installed_files.write().await.remove(&scope);
            }
            None => {
                self.hands
                    .trusted_sandbox_files
                    .write()
                    .await
                    .retain(|scope, _| scope.session_id != session_id);
                self.hands
                    .installed_files
                    .write()
                    .await
                    .retain(|scope, _| scope.session_id != session_id);
            }
        }
    }

    pub(super) async fn reprovision_hand(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        route: &HandRoute,
        call_scope: ToolCallScope<'_>,
    ) -> Result<HandHandle> {
        let provider = route.provider.as_str();
        let tier_label = route.tier.as_str();
        let span = sandbox_provision_span("reprovision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            call_scope.admit()?;
            let scope = worker_id.unwrap_or_default();
            let key = session_provider_key(session, worker_id, provider);
            let cached = self.hands.active_hands.read().await.get(&key).cloned();

            if let Some(lease_store) = &self.hands.hand_leases {
                if let Some(lease) = lease_store.get(session.id, scope, provider).await? {
                    let fenced = if lease.status == HandLeaseStatus::Reaping {
                        false
                    } else {
                        call_scope.admit()?;
                        lease_store
                            .transition_status(&lease, HandLeaseStatus::Stale)
                            .await?
                    };
                    if let Some(cached) = cached.as_ref()
                        && cached.generation == Some(lease.generation)
                    {
                        self.remove_cached_binding_if_matches(
                            &key,
                            &cached.handle,
                            cached.generation,
                        )
                        .await;
                    }
                    if !fenced && lease.status != HandLeaseStatus::Reaping {
                        tracing::debug!(
                            session_id = %session.id,
                            worker_id = %scope,
                            provider,
                            generation = lease.generation,
                            "hand recovery lost its stale transition fence"
                        );
                    }
                }
            } else if let Some(cached) = cached {
                call_scope.admit()?;
                self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                    .await;
                let provider_impl = self.hands.providers.get(provider).ok_or_else(|| {
                    MoaError::ProviderError(format!("unknown hand provider: {provider}"))
                })?;
                provider_impl.destroy(&cached.handle).await?;
            }
            self.remove_installed_marker(manifest_scope_key(session, worker_id), provider)
                .await;

            let started_at = Instant::now();
            let handle = self
                .get_or_provision_hand_within(route, session, worker_id, call_scope)
                .await?;
            let cold_start = started_at.elapsed();
            record_span.record("moa.sandbox.id", hand_id(&handle));
            record_span.record("moa.sandbox.cold_start_ms", cold_start.as_millis() as i64);
            self.install_trusted_files_for_hand(session, worker_id, provider, &handle, call_scope)
                .await?;
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

fn manifest_scope_key(session: &SessionMeta, worker_id: Option<&str>) -> HandScopeKey {
    manifest_scope_key_from_parts(session.id, worker_id.unwrap_or_default())
}

fn manifest_scope_key_from_parts(
    session_id: moa_core::types::identifiers::SessionId,
    worker_id: &str,
) -> HandScopeKey {
    HandScopeKey {
        session_id,
        worker_id: worker_id.to_string(),
    }
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

fn session_provider_key_from_parts(
    session_id: moa_core::types::identifiers::SessionId,
    worker_id: &str,
    provider: &str,
) -> String {
    format!("{session_id}:{worker_id}:{provider}")
}

fn tenant_key(session: &SessionMeta) -> TenantId {
    session.tenant_id
}

/// Returns the idle deadline a renewal should ask for under `policy`.
///
/// The policy's own idle timeout is the ceiling: renewal restarts the idle
/// window the operator declared, and never invents a longer one. A policy with
/// an explicitly unbounded idle timeout still gets a finite renewal request,
/// because an unbounded idle lease has no `idle_expires_at` to renew and never
/// reaches this path.
fn next_idle_deadline(policy: &HandLeasePolicy) -> chrono::DateTime<Utc> {
    policy
        .idle_deadline(Utc::now())
        .unwrap_or_else(|| Utc::now() + ChronoDuration::seconds(HAND_LEASE_TTL_SECS))
}

/// Returns whether either of a lease's deadlines has already passed.
fn lease_expired(lease: &HandLease) -> bool {
    let now = Utc::now();
    lease.idle_expires_at.is_some_and(|idle| idle <= now)
        || lease.hard_expires_at.is_some_and(|hard| hard <= now)
}

/// Returns whether a persisted lease was provisioned under exactly `policy`.
///
/// The comparison is on the policy identity hash alone, which already covers
/// the six-dimension profile, all five source revisions, and the provider's
/// capability revision. A lease with no complete persisted policy never
/// matches.
fn lease_matches_policy(lease: &HandLease, policy: &HandLeasePolicy) -> bool {
    lease
        .policy
        .as_ref()
        .is_some_and(|persisted| persisted.profile_hash == policy.profile_hash)
}

/// Returns whether a reused active lease should be renewed based on how much of
/// its declared idle window is left.
fn lease_renewal_due(idle_expires_at: chrono::DateTime<Utc>, policy: &HandLeasePolicy) -> bool {
    let window_secs = policy
        .profile
        .idle_timeout
        .bounded_seconds()
        .and_then(|seconds| i64::try_from(seconds.get()).ok())
        .unwrap_or(HAND_LEASE_TTL_SECS);
    idle_expires_at.signed_duration_since(Utc::now()) < ChronoDuration::seconds(window_secs / 2)
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
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use crate::core::leases::{HandLeaseStore, MemoryHandLeaseStore};
    use crate::core::profile::TenantSandboxPolicyStore;
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
        install_calls: AtomicUsize,
        completed_installs: std::sync::Mutex<Vec<(HandHandle, Vec<SandboxFile>)>>,
        first_install_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        first_install_release: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
        /// The effective profile of the most recent `provision` call, so tests
        /// can prove the router hands the provider the resolved policy rather
        /// than a substituted default.
        last_provisioned_profile: std::sync::Mutex<Option<EffectiveSandboxProfile>>,
        provisioned: std::sync::Mutex<HashMap<HandProvisioningOperationId, (HandSpec, HandHandle)>>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum LeaseBarrierPoint {
        AfterClaim,
        BeforeClear,
        BeforeActivate,
        BeforeTransition,
    }

    struct LeaseBarrier {
        point: LeaseBarrierPoint,
        started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        release: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl LeaseBarrier {
        async fn wait(&self, point: LeaseBarrierPoint) {
            if self.point != point {
                return;
            }
            if let Some(started) = self
                .started
                .lock()
                .expect("lock lease barrier start signal")
                .take()
            {
                let _ = started.send(());
            }
            let release = self
                .release
                .lock()
                .expect("lock lease barrier release signal")
                .take();
            if let Some(release) = release {
                let _ = release.await;
            }
        }
    }

    struct BarrierHandLeaseStore {
        inner: Arc<MemoryHandLeaseStore>,
        barrier: LeaseBarrier,
    }

    impl BarrierHandLeaseStore {
        fn new(
            inner: Arc<MemoryHandLeaseStore>,
            point: LeaseBarrierPoint,
        ) -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            (
                Arc::new(Self {
                    inner,
                    barrier: LeaseBarrier {
                        point,
                        started: std::sync::Mutex::new(Some(started_tx)),
                        release: std::sync::Mutex::new(Some(release_rx)),
                    },
                }),
                started_rx,
                release_tx,
            )
        }
    }

    #[async_trait]
    impl HandLeaseStore for BarrierHandLeaseStore {
        async fn claim_for_provisioning(
            &self,
            request: HandLeaseProvisionRequest<'_>,
        ) -> Result<Option<HandLease>> {
            let claim = self.inner.claim_for_provisioning(request).await?;
            if claim.is_some() {
                self.barrier.wait(LeaseBarrierPoint::AfterClaim).await;
            }
            Ok(claim)
        }

        async fn get(
            &self,
            session_id: moa_core::types::identifiers::SessionId,
            worker_id: &str,
            provider: &str,
        ) -> Result<Option<HandLease>> {
            self.inner.get(session_id, worker_id, provider).await
        }

        async fn list_session(
            &self,
            session_id: moa_core::types::identifiers::SessionId,
        ) -> Result<Vec<HandLease>> {
            self.inner.list_session(session_id).await
        }

        async fn activate(
            &self,
            session_id: moa_core::types::identifiers::SessionId,
            worker_id: &str,
            provider: &str,
            generation: i64,
            handle: LeaseHandle,
        ) -> Result<bool> {
            self.barrier.wait(LeaseBarrierPoint::BeforeActivate).await;
            self.inner
                .activate(session_id, worker_id, provider, generation, handle)
                .await
        }

        async fn clear_handle_for_provisioning(&self, claim: &HandLease) -> Result<bool> {
            self.barrier.wait(LeaseBarrierPoint::BeforeClear).await;
            self.inner.clear_handle_for_provisioning(claim).await
        }

        async fn renew_active(
            &self,
            session_id: moa_core::types::identifiers::SessionId,
            worker_id: &str,
            provider: &str,
            generation: i64,
            provisioning_operation_id: HandProvisioningOperationId,
            idle_expires_at: chrono::DateTime<Utc>,
        ) -> Result<bool> {
            self.inner
                .renew_active(
                    session_id,
                    worker_id,
                    provider,
                    generation,
                    provisioning_operation_id,
                    idle_expires_at,
                )
                .await
        }

        async fn transition_status(
            &self,
            expected: &HandLease,
            status: HandLeaseStatus,
        ) -> Result<bool> {
            self.barrier.wait(LeaseBarrierPoint::BeforeTransition).await;
            self.inner.transition_status(expected, status).await
        }

        async fn claim_for_destroy(
            &self,
            expected: &HandLease,
            claim_ttl: Duration,
        ) -> Result<Option<uuid::Uuid>> {
            self.inner.claim_for_destroy(expected, claim_ttl).await
        }

        async fn finalize_destroy(
            &self,
            expected: &HandLease,
            claim_token: uuid::Uuid,
        ) -> Result<bool> {
            self.inner.finalize_destroy(expected, claim_token).await
        }

        async fn release_destroy_claim(
            &self,
            expected: &HandLease,
            claim_token: uuid::Uuid,
            retry_after: Duration,
        ) -> Result<bool> {
            self.inner
                .release_destroy_claim(expected, claim_token, retry_after)
                .await
        }
    }

    #[derive(Default)]
    struct CountingTenantPolicyStore {
        reads: AtomicUsize,
    }

    #[async_trait]
    impl TenantSandboxPolicyStore for CountingTenantPolicyStore {
        async fn current(
            &self,
            _tenant_id: TenantId,
        ) -> Result<Option<moa_core::types::hands::SandboxPolicySnapshot>> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
            let revision = format!("tenant-policy-{read}");
            Ok(Some(
                moa_core::types::hands::SandboxPolicySnapshot::new(
                    &revision,
                    crate::core::profile::local_development_sandbox_policy().profile,
                )
                .expect("test tenant policy is valid"),
            ))
        }
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
                install_calls: AtomicUsize::new(0),
                completed_installs: std::sync::Mutex::new(Vec::new()),
                first_install_started: std::sync::Mutex::new(None),
                first_install_release: std::sync::Mutex::new(None),
                last_provisioned_profile: std::sync::Mutex::new(None),
                provisioned: std::sync::Mutex::new(HashMap::new()),
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

        fn with_first_install_barrier(
            mut self,
            started: oneshot::Sender<()>,
            release: oneshot::Receiver<()>,
        ) -> Self {
            self.first_install_started = std::sync::Mutex::new(Some(started));
            self.first_install_release = std::sync::Mutex::new(Some(release));
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

        fn install_calls(&self) -> usize {
            self.install_calls.load(Ordering::SeqCst)
        }

        fn completed_installs(&self) -> Vec<(HandHandle, Vec<SandboxFile>)> {
            self.completed_installs
                .lock()
                .expect("lock completed installs")
                .clone()
        }
    }

    #[async_trait]
    impl HandProvider for CountingProvider {
        fn capabilities(&self) -> moa_core::types::hands::HandProviderCapabilities {
            crate::adapters::local::LOCAL_HAND_CAPABILITIES.clone()
        }
        fn provider_name(&self) -> &str {
            &self.name
        }

        async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
            if let Ok(mut last) = self.last_provisioned_profile.lock() {
                *last = Some(spec.effective_profile.clone());
            }
            if !self.provision_delay.is_zero() {
                tokio::time::sleep(self.provision_delay).await;
            }
            let count = self.provision_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let handle = {
                let mut provisioned = self.provisioned.lock().map_err(|_| {
                    MoaError::ProviderError("lock counting provider resources".to_string())
                })?;
                match provisioned.get(&spec.provisioning_operation_id) {
                    Some((existing_spec, handle)) if existing_spec == &spec => handle.clone(),
                    Some(_) => {
                        return Err(MoaError::ProviderError(format!(
                            "provisioning operation {} was reused with a different spec",
                            spec.provisioning_operation_id
                        )));
                    }
                    None => {
                        let handle = HandHandle::docker(format!("{}-{count}", self.name));
                        provisioned.insert(
                            spec.provisioning_operation_id,
                            (spec.clone(), handle.clone()),
                        );
                        handle
                    }
                }
            };
            if let Some((lease_store, session_id)) = &self.stale_generation_on_provision {
                let lease = lease_store
                    .get(*session_id, "", &self.name)
                    .await?
                    .ok_or_else(|| MoaError::StorageError("missing test lease".to_string()))?;
                let _ = lease_store
                    .transition_status(&lease, HandLeaseStatus::Stale)
                    .await?;
            }
            Ok(handle)
        }

        async fn provisioned_hands(
            &self,
            operation_id: HandProvisioningOperationId,
        ) -> Result<Vec<HandHandle>> {
            let provisioned = self.provisioned.lock().map_err(|_| {
                MoaError::ProviderError("lock counting provider resources".to_string())
            })?;
            Ok(provisioned
                .get(&operation_id)
                .map(|(_, handle)| vec![handle.clone()])
                .unwrap_or_default())
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

        async fn install_files(&self, handle: &HandHandle, files: &[SandboxFile]) -> Result<()> {
            let call_index = self.install_calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                if let Some(started) = self
                    .first_install_started
                    .lock()
                    .expect("lock first install start signal")
                    .take()
                {
                    let _ = started.send(());
                }
                let release = self
                    .first_install_release
                    .lock()
                    .expect("lock first install release signal")
                    .take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            }
            self.completed_installs
                .lock()
                .expect("lock completed installs")
                .push((handle.clone(), files.to_vec()));
            Ok(())
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

        async fn destroy(&self, handle: &HandHandle) -> Result<()> {
            self.destroy_calls.fetch_add(1, Ordering::SeqCst);
            if self.destroy_fails {
                return Err(MoaError::ProviderError("destroy failed".to_string()));
            }
            self.provisioned
                .lock()
                .map_err(|_| {
                    MoaError::ProviderError("lock counting provider resources".to_string())
                })?
                .retain(|_, (_, provisioned_handle)| provisioned_handle != handle);
            Ok(())
        }
    }

    fn router(provider: Arc<CountingProvider>, lease_store: Arc<dyn HandLeaseStore>) -> ToolRouter {
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
        registry.retarget_hand_tools(vec![test_hand_route(provider.provider_name())]);
        registry.retain_only(["bash"]);
        let provider_trait: Arc<dyn HandProvider> = provider;
        let mut providers = HashMap::new();
        providers.insert(provider_trait.provider_name().to_string(), provider_trait);
        ToolRouter::new(
            registry,
            providers,
            crate::core::profile::local_development_sandbox_policy(),
        )
        .with_hand_lease_store(lease_store)
    }

    /// The container route used by every lifecycle test, with the named
    /// route-unset policy layer.
    fn test_hand_route(provider: &str) -> HandRoute {
        HandRoute {
            provider: provider.to_string(),
            tier: SandboxTier::Container,
            policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
            ),
        }
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

    fn sandbox_file(contents: &[u8]) -> SandboxFile {
        SandboxFile {
            path: ".moa/skills/r3/SKILL.md".to_string(),
            content: contents.to_vec(),
            executable: false,
        }
    }

    #[tokio::test]
    async fn cancellation_after_claim_terminalizes_without_provider_mutation() {
        // Pins: cancellation after the durable provisioning claim returns but
        // before provider dispatch moves that exact claim to Failed and creates
        // or destroys no sandbox.
        let inner = MemoryHandLeaseStore::shared();
        let (store, claimed_rx, release_tx) =
            BarrierHandLeaseStore::new(inner.clone(), LeaseBarrierPoint::AfterClaim);
        let provider = Arc::new(CountingProvider::new("cancel-after-claim"));
        let router = router(provider.clone(), store);
        let session = session();
        let route = test_hand_route(provider.provider_name());
        let cancel = CancellationToken::new();
        let call = router.get_or_provision_hand_within(
            &route,
            &session,
            None,
            ToolCallScope::from_tokens(Some(&cancel), Some(&cancel)),
        );
        let cancel_after_claim = async {
            claimed_rx
                .await
                .expect("provisioning claim should reach its barrier");
            cancel.cancel();
            release_tx
                .send(())
                .expect("release the claimed provisioning call");
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(call, cancel_after_claim)
        })
        .await
        .expect("claimed cancellation should settle promptly");

        assert!(matches!(result, Err(MoaError::Cancelled)));
        assert_eq!(provider.provision_calls(), 0);
        assert_eq!(provider.destroy_calls(), 0);
        let lease = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load cancelled claim")
            .expect("cancelled claim remains terminally recorded");
        assert_eq!(lease.status, HandLeaseStatus::Failed);
        assert_eq!(lease.handle, None);
    }

    #[tokio::test]
    async fn cancellation_after_destroy_still_clears_and_terminalizes_claim() {
        // Pins: cancellation after old-hand destroy dispatch cannot drop the
        // exact durable clear/finalization or start replacement creation.
        let inner = MemoryHandLeaseStore::shared();
        let (store, clear_rx, release_tx) =
            BarrierHandLeaseStore::new(inner.clone(), LeaseBarrierPoint::BeforeClear);
        let provider = Arc::new(CountingProvider::new("cancel-after-destroy"));
        let router = router(provider.clone(), store);
        let session = session();
        let route = test_hand_route(provider.provider_name());
        let hand_a = router
            .get_or_provision_hand_within(&route, &session, None, ToolCallScope::unbounded())
            .await
            .expect("provision hand A");
        let active = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load active hand A lease")
            .expect("hand A lease exists");
        assert!(
            inner
                .transition_status(&active, HandLeaseStatus::Stale)
                .await
                .expect("mark hand A stale")
        );

        let cancel = CancellationToken::new();
        let call = router.get_or_provision_hand_within(
            &route,
            &session,
            None,
            ToolCallScope::from_tokens(Some(&cancel), Some(&cancel)),
        );
        let cancel_after_destroy = async {
            clear_rx
                .await
                .expect("destroy should reach durable clear barrier");
            assert_eq!(provider.destroy_calls(), 1);
            let replacing = inner
                .get(session.id, "", provider.provider_name())
                .await
                .expect("load replacing lease")
                .expect("replacing lease exists");
            assert_eq!(replacing.status, HandLeaseStatus::Provisioning);
            assert_eq!(
                replacing.handle.as_ref().map(|handle| &handle.handle),
                Some(&hand_a)
            );
            cancel.cancel();
            release_tx
                .send(())
                .expect("release durable clear after cancellation");
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(call, cancel_after_destroy)
        })
        .await
        .expect("post-destroy cancellation should settle promptly");

        assert!(matches!(result, Err(MoaError::Cancelled)));
        assert_eq!(provider.provision_calls(), 1);
        assert_eq!(provider.destroy_calls(), 1);
        let terminal = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load terminal replacement claim")
            .expect("terminal replacement claim exists");
        assert_eq!(terminal.status, HandLeaseStatus::Failed);
        assert_eq!(terminal.handle, None);
    }

    #[tokio::test]
    async fn cancellation_after_create_completes_activation_before_cache_publication() {
        // Pins: cancellation after provider create cannot drop durable
        // activation; the handle is published only after that activation wins.
        let inner = MemoryHandLeaseStore::shared();
        let (store, activate_rx, release_tx) =
            BarrierHandLeaseStore::new(inner.clone(), LeaseBarrierPoint::BeforeActivate);
        let provider = Arc::new(CountingProvider::new("cancel-after-create"));
        let router = router(provider.clone(), store);
        let session = session();
        let route = test_hand_route(provider.provider_name());
        let cancel = CancellationToken::new();
        let key = session_provider_key(&session, None, provider.provider_name());
        let call = router.get_or_provision_hand_within(
            &route,
            &session,
            None,
            ToolCallScope::from_tokens(Some(&cancel), Some(&cancel)),
        );
        let cancel_before_activation = async {
            activate_rx
                .await
                .expect("created hand should reach activation barrier");
            assert_eq!(provider.provision_calls(), 1);
            assert!(
                !router.hands.active_hands.read().await.contains_key(&key),
                "unactivated hand must not be process-visible"
            );
            cancel.cancel();
            release_tx
                .send(())
                .expect("release activation after cancellation");
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(call, cancel_before_activation)
        })
        .await
        .expect("post-create cancellation should settle promptly");
        let handle = result.expect("created hand should finish durable activation");

        let lease = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load activated lease")
            .expect("activated lease exists");
        assert_eq!(lease.status, HandLeaseStatus::Active);
        assert_eq!(
            lease.handle.as_ref().map(|lease| &lease.handle),
            Some(&handle)
        );
        assert_eq!(provider.destroy_calls(), 0);
        assert_eq!(
            router.hands.active_hands.read().await.get(&key),
            Some(&ActiveHand {
                handle,
                generation: Some(lease.generation),
            })
        );
    }

    #[tokio::test]
    async fn concurrent_recovery_does_not_steal_reaper_destroy_ownership() {
        // Pins: if the durable reaper claims the active generation before
        // recovery's stale transition, recovery never destroys or overwrites
        // that Reaping generation and provisions only after reaper finalization.
        let inner = MemoryHandLeaseStore::shared();
        let (store, transition_rx, release_tx) =
            BarrierHandLeaseStore::new(inner.clone(), LeaseBarrierPoint::BeforeTransition);
        let provider = Arc::new(CountingProvider::new("recovery-reaper-race"));
        let router = router(provider.clone(), store);
        let session = session();
        let route = test_hand_route(provider.provider_name());
        let hand_a = router
            .get_or_provision_hand_within(&route, &session, None, ToolCallScope::unbounded())
            .await
            .expect("provision hand A");
        let active = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load active hand A lease")
            .expect("hand A lease exists");

        let call = router.reprovision_hand(&session, None, &route, ToolCallScope::unbounded());
        let reaper_wins = async {
            transition_rx
                .await
                .expect("recovery should reach stale transition barrier");
            let claim_token = inner
                .claim_for_destroy(&active, HAND_DESTROY_CLAIM_TTL)
                .await
                .expect("reaper claim succeeds")
                .expect("reaper owns the exact active generation");
            provider
                .destroy(&hand_a)
                .await
                .expect("simulated reaper destroys hand A");
            assert!(
                inner
                    .finalize_destroy(&active, claim_token)
                    .await
                    .expect("reaper finalization succeeds")
            );
            release_tx
                .send(())
                .expect("release recovery after reaper finalization");
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(call, reaper_wins)
        })
        .await
        .expect("recovery/reaper race should settle promptly");
        let hand_b = result.expect("recovery provisions after reaper finalizes");

        assert_ne!(hand_a, hand_b);
        assert_eq!(provider.provision_calls(), 2);
        assert_eq!(
            provider.destroy_calls(),
            1,
            "only the reaper destroys hand A"
        );
        let replacement = inner
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load replacement lease")
            .expect("replacement lease exists");
        assert_eq!(replacement.status, HandLeaseStatus::Active);
        assert_eq!(replacement.generation, active.generation + 1);
        assert_eq!(
            replacement.handle.as_ref().map(|lease| &lease.handle),
            Some(&hand_b)
        );
    }

    #[tokio::test]
    async fn stale_manifest_install_completion_cannot_replace_new_hand_marker() {
        // Pins: install completion for old hand A cannot overwrite the exact
        // manifest marker already published for replacement hand B.
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let provider = Arc::new(
            CountingProvider::new("manifest-race")
                .with_first_install_barrier(started_tx, release_rx),
        );
        let lease_store = MemoryHandLeaseStore::shared();
        let router = Arc::new(router(provider.clone(), lease_store.clone()));
        let session = Arc::new(session());
        let route = test_hand_route(provider.provider_name());
        let files = vec![sandbox_file(b"trusted")];
        let hand_a = router
            .get_or_provision_hand_within(&route, &session, None, ToolCallScope::unbounded())
            .await
            .expect("provision hand A");
        router
            .set_trusted_sandbox_files(&session, None, files.clone())
            .await;

        let first_router = Arc::clone(&router);
        let first_session = Arc::clone(&session);
        let first_handle = hand_a.clone();
        let first = tokio::spawn(async move {
            first_router
                .install_trusted_files_for_hand(
                    &first_session,
                    None,
                    "manifest-race",
                    &first_handle,
                    ToolCallScope::unbounded(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("the first install should reach its provider barrier")
            .expect("the first install should signal its provider barrier");

        let hand_b = tokio::time::timeout(
            Duration::from_secs(1),
            router.reprovision_hand(&session, None, &route, ToolCallScope::unbounded()),
        )
        .await
        .expect("replacement must not wait on hand A install I/O")
        .expect("replacement hand B should provision and install");
        assert_ne!(hand_a, hand_b);

        release_tx
            .send(())
            .expect("release the first provider install");
        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("the stale install should finish promptly")
            .expect("the first install task should join")
            .expect_err("the stale hand A install must lose its active-binding fence");

        let scope = manifest_scope_key(&session, None);
        let marker = router
            .hands
            .installed_files
            .read()
            .await
            .get(&scope)
            .and_then(|providers| providers.get("manifest-race"))
            .cloned()
            .expect("replacement hand B has an installed marker");
        let active = lease_store
            .get(session.id, "", "manifest-race")
            .await
            .expect("load replacement lease")
            .expect("replacement lease exists");
        assert_eq!(marker.handle, hand_b);
        assert_eq!(marker.generation, Some(active.generation));
        assert_eq!(provider.install_calls(), 2);
        assert_eq!(
            provider.completed_installs(),
            vec![(hand_b, files.clone()), (hand_a, files)],
            "hand B completes first and hand A's stale completion stays unmarked"
        );
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
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
            .await
            .expect("first router provisions and executes");
        second_router
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
            left_router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &left_session,
                caller_identity: &left_identity,
                worker_id: None,
                invocation: &left_invocation,
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            }),
            right_router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &right_session,
                caller_identity: &right_identity,
                worker_id: None,
                invocation: &right_invocation,
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
                    first.provisioning_operation_id,
                    short_expiry,
                )
                .await
                .expect("shrink active lease expiry")
        );

        router
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
            renewed.idle_expires_at > Some(short_expiry),
            "reuse should renew the active durable lease"
        );

        assert!(
            lease_store
                .transition_status(&renewed, HandLeaseStatus::Stale)
                .await
                .expect("mark lease stale")
        );
        let replacement_result = tokio::time::timeout(
            Duration::from_secs(1),
            router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            }),
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
        assert_eq!(
            provider.destroy_calls(),
            1,
            "the stale durable handle must be destroyed before its replacement is provisioned"
        );
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
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
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
            .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
                session: &session,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &bash_invocation(),
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: crate::core::ToolCallScope::unbounded(),
            })
            .await
            .expect("provision before cleanup");
        cleanup_router.reclaim_hands(&session.id, None).await;

        assert_eq!(provider.destroy_calls(), 1);
        let lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load lease after failed cleanup")
            .expect("lease should remain");
        assert_eq!(
            lease.status,
            HandLeaseStatus::Failed,
            "a failed session destroy remains retryable but cannot be reused"
        );
    }

    #[tokio::test]
    async fn lifecycle_worker_scope_isolates_hands_and_leases() {
        // Pins: a worker scope provisions its own hand/lease, distinct from the session scope.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("scope-isolation"));
        let session = session();
        let router = router(provider.clone(), lease_store.clone());

        let root = router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                None,
                ToolCallScope::unbounded(),
            )
            .await
            .expect("session-scope hand provisions");
        let child = router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                Some("sub-x"),
                ToolCallScope::unbounded(),
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
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                None,
                ToolCallScope::unbounded(),
            )
            .await
            .expect("session-scope hand provisions");
        router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                Some("sub-x"),
                ToolCallScope::unbounded(),
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
                .get_or_provision_hand_within(
                    &test_hand_route(provider.provider_name()),
                    &session,
                    scope,
                    ToolCallScope::unbounded(),
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
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                None,
                ToolCallScope::unbounded(),
            )
            .await
            .expect("session-scope hand reused");
        router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                Some("sub-y"),
                ToolCallScope::unbounded(),
            )
            .await
            .expect("sibling-scope hand reused");
        assert_eq!(
            provider.provision_calls(),
            3,
            "intact scopes are reused, not re-provisioned"
        );
        router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                Some("sub-x"),
                ToolCallScope::unbounded(),
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
        // Pins: worker cleanup reports incomplete and leaves the lease failed
        // when provider destroy fails, so it stays retryable without reuse.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider =
            Arc::new(CountingProvider::new("worker-destroy-retry").with_destroy_failure());
        let session = session();
        let first_router = router(provider.clone(), lease_store.clone());
        let cleanup_router = router(provider.clone(), lease_store.clone());

        first_router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                Some("sub-x"),
                ToolCallScope::unbounded(),
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
        assert_eq!(
            lease.status,
            HandLeaseStatus::Failed,
            "a failed worker destroy remains retryable but cannot be reused"
        );
    }

    #[tokio::test]
    async fn provisioning_hands_the_provider_the_resolved_policy_not_a_default() {
        // Pins: the profile the router resolved is the profile the provider is
        // asked to honor. Before this contract, provisioning substituted
        // `HandResources::default()` and one fixed timeout for both deadlines,
        // so every policy layer stopped at the router. A substitution here
        // would silently discard whatever the deployment, tenant, agent, and
        // route layers agreed on.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("profile-passthrough"));
        let session = session();
        let router = router(provider.clone(), lease_store);
        let route = test_hand_route(provider.provider_name());

        router
            .get_or_provision_hand_within(&route, &session, None, ToolCallScope::unbounded())
            .await
            .expect("hand provisions");

        let resolved = router
            .resolve_sandbox_profile(&route, &session)
            .await
            .expect("resolve the same policy the router used");
        let provisioned = provider
            .last_provisioned_profile
            .lock()
            .expect("provisioned profile lock")
            .clone()
            .expect("the provider was asked to provision");

        assert_eq!(
            provisioned.profile_hash(),
            resolved.profile_hash(),
            "the provider must receive the resolved policy identity, not a substituted default"
        );
        assert_eq!(provisioned.profile(), resolved.profile());
        assert_eq!(
            provisioned.sources().deployment,
            "local-development-unbounded",
            "the deployment layer must reach the provider by name"
        );
        assert_eq!(
            provisioned.capability_revision(),
            provider.capabilities().revision,
            "the serving provider's capability revision must reach the spec"
        );
    }

    #[tokio::test]
    async fn durable_claim_and_provider_share_one_policy_resolution() {
        // Pins: the tenant policy is resolved exactly once for a provisioning
        // decision, and that same immutable effective profile is persisted on
        // the lease and handed to the provider. Re-resolving between those steps
        // can claim under revision N and provision under revision N+1.
        let lease_store = MemoryHandLeaseStore::shared();
        let provider = Arc::new(CountingProvider::new("single-policy-resolution"));
        let tenant_policy = Arc::new(CountingTenantPolicyStore::default());
        let session = session();
        let router = router(provider.clone(), lease_store.clone())
            .with_tenant_sandbox_policy_store(tenant_policy.clone());
        let route = test_hand_route(provider.provider_name());

        router
            .get_or_provision_hand_within(&route, &session, None, ToolCallScope::unbounded())
            .await
            .expect("hand provisions");

        assert_eq!(
            tenant_policy.reads.load(Ordering::SeqCst),
            1,
            "one provisioning decision must resolve tenant policy only once"
        );
        let provisioned = provider
            .last_provisioned_profile
            .lock()
            .expect("provisioned profile lock")
            .clone()
            .expect("provider received one profile");
        let lease = lease_store
            .get(session.id, "", provider.provider_name())
            .await
            .expect("load lease")
            .expect("lease exists");
        assert_eq!(
            lease.policy.expect("active lease has policy").profile_hash,
            provisioned.profile_hash(),
            "lease claim and provider spec must carry the same resolved profile"
        );
    }

    #[test]
    fn lifecycle_lease_renewal_is_deferred_until_half_the_declared_idle_window_remains() {
        // Pins: a freshly-renewed durable lease is not rewritten on reuse; the
        // renew (and its generation fence) only fires once less than half of the
        // *policy's own* idle window remains, keeping the hot path free of a
        // lease UPDATE per tool call. The threshold tracks the declared idle
        // timeout rather than a fixed constant, so a 10-minute policy is not
        // renewed on the schedule of a 1-hour one.
        let policy = crate::core::leases::test_support::lease_policy(
            Some(600),
            Some(3600),
            "renewal-capabilities-v1",
        );
        assert!(
            !lease_renewal_due(Utc::now() + ChronoDuration::seconds(540), &policy),
            "a nearly-full idle window should not be renewed on reuse"
        );
        assert!(
            lease_renewal_due(Utc::now() + ChronoDuration::seconds(60), &policy),
            "an idle window with well under half remaining should be renewed"
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

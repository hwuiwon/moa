//! Workspace-root tracking and lazy hand lifecycle management.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::MoaError,
    error::Result,
    types::hands::EffectiveSandboxProfile,
    types::hands::HandHandle,
    types::hands::HandSpec,
    types::hands::HandStatus,
    types::hands::SandboxFile,
    types::identifiers::HandProvisioningOperationId,
    types::identifiers::ProviderAccountId,
    types::identifiers::SandboxWorkspaceId,
    types::identifiers::TenantId,
    types::resource::ResourceBudget,
    types::sandbox_workspace::{
        DurabilityClass, SandboxFilesystemLayout, SandboxWorkspaceScope, WorkspaceBinding,
    },
    types::session::SessionMeta,
};
use moa_observability::{current_turn_root_span, record_sandbox_provision_duration};
use tracing::Instrument;
use uuid::Uuid;

use super::leases::{
    HandLease, HandLeaseActivateRequest, HandLeasePolicy, HandLeaseProvisionRequest,
    HandLeaseRenewRequest, HandLeaseStatus, LeaseHandle, provisioning_deadline,
};
use super::reaper::{ProvisioningAbsenceProof, destroy_provisioning_operations};
use super::sandbox_workspace::lifecycle::lease_attachment;
#[cfg(test)]
use super::sandbox_workspace::lifecycle::validate_managed_restore_target;
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
    workspace_binding: &'a WorkspaceBinding,
    key: String,
    effective: &'a EffectiveSandboxProfile,
    policy: &'a HandLeasePolicy,
    call_scope: ToolCallScope<'a>,
}

struct CreateHandContext<'a> {
    route: &'a HandRoute,
    workspace_binding: &'a WorkspaceBinding,
    effective: &'a EffectiveSandboxProfile,
    budget: ResourceBudget,
    provisioning_operation_id: HandProvisioningOperationId,
    provisioning_deadline_at: chrono::DateTime<Utc>,
}

struct ProvisionedHandRecoveryContext<'a> {
    session: &'a SessionMeta,
    worker_id: &'a str,
    provider: &'a str,
    workspace_binding: &'a WorkspaceBinding,
    cache_key: &'a str,
    lease: &'a HandLease,
    policy: &'a HandLeasePolicy,
    call_scope: ToolCallScope<'a>,
    lease_store: &'a dyn super::leases::HandLeaseStore,
}

struct ProvisionedHandActivationContext<'a> {
    session: &'a SessionMeta,
    worker_id: &'a str,
    provider: &'a str,
    workspace_binding: &'a WorkspaceBinding,
    cache_key: &'a str,
    lease: &'a HandLease,
    handle: HandHandle,
    call_scope: ToolCallScope<'a>,
    lease_store: &'a dyn super::leases::HandLeaseStore,
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
    /// A populated `worker_id` is the opaque lease key derived from a typed
    /// worker or execution-task workspace owner. `None` stores only the
    /// sandbox-free turn manifest used by trusted host-side reads; it does not
    /// create an ownable hand or workspace scope. Clearing one scope leaves
    /// other owners' manifests untouched.
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
    /// either across the whole session or for one typed owner scope.
    ///
    /// `scope` selects what is reclaimed:
    /// - `None` tears down the entire session — every typed worker or
    ///   execution-task scope under it (cache keys
    ///   `"{session_id}:*:{provider}"` and leases `(session_id, *, provider)`).
    ///   It is an aggregate cleanup selector, not an ownable hand scope.
    /// - `Some(owner_key)` reclaims only that `(session_id, owner_key)` scope, so
    ///   a finishing worker or execution task releases its own sandbox without
    ///   over-releasing sibling owners. It clears the cache keys with prefix
    ///   `"{session_id}:{owner_key}:"`, that exact structured scope's
    ///   `installed_files`/`trusted_sandbox_files` entries, and the durable leases
    ///   `(session_id, owner_key, provider)` across providers; all sibling typed
    ///   owner scopes are untouched.
    ///
    /// Returns `true` when every matched hand was destroyed and its durable lease
    /// marked `Destroyed`. A `false` result means at least one release step
    /// failed and the affected leases stay reclaimable, so an owner-specific
    /// caller can reschedule cleanup instead of clearing its state. Session
    /// teardown ignores the result and lets the lease TTL reclaim any straggler.
    pub async fn reclaim_hands(
        &self,
        tenant_id: TenantId,
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
            match lease_store.list_session(tenant_id, *session_id).await {
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
                        if self.hands.workspace_repository.is_some() && lease.attachment.is_some() {
                            let terminal_status = match lease.status {
                                HandLeaseStatus::Provisioning | HandLeaseStatus::Failed => {
                                    HandLeaseStatus::Failed
                                }
                                HandLeaseStatus::Reaping => {
                                    complete = false;
                                    continue;
                                }
                                HandLeaseStatus::Active | HandLeaseStatus::Stale => {
                                    HandLeaseStatus::Stale
                                }
                                HandLeaseStatus::Destroyed => continue,
                            };
                            if lease.status != terminal_status {
                                match lease_store
                                    .transition_status(tenant_id, &lease, terminal_status)
                                    .await
                                {
                                    Ok(true) => {}
                                    Ok(false) => {
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
                                            "failed to hand terminal workspace cleanup to the durable reaper"
                                        );
                                        continue;
                                    }
                                }
                            }
                            if let Some(lease_handle) = lease.handle.as_ref() {
                                let key = session_provider_key_from_parts(
                                    lease.session_id,
                                    &lease.worker_id,
                                    &lease.provider,
                                );
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
                            continue;
                        }
                        let claim_token = match lease_store
                            .claim_for_destroy(tenant_id, &lease, HAND_DESTROY_CLAIM_TTL)
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
                                    tenant_id,
                                    &lease,
                                    claim_token,
                                    HAND_DESTROY_RETRY_DELAY,
                                )
                                .await;
                            continue;
                        };
                        match destroy_provisioning_operations(
                            provider.as_ref(),
                            lease
                                .handle
                                .as_ref()
                                .and_then(|handle| handle.handle.provider_account())
                                .map_or(ProviderAccountId(Uuid::nil()), |context| context.0),
                            lease
                                .handle
                                .as_ref()
                                .and_then(|handle| handle.handle.provider_account())
                                .map_or(0, |context| context.1),
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
                            Ok(()) => match lease_store
                                .finalize_destroy(tenant_id, &lease, claim_token)
                                .await
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
                                        tenant_id,
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
        workspace_scope: &SandboxWorkspaceScope,
        scope: ToolCallScope<'_>,
    ) -> Result<HandHandle> {
        let worker_scope = workspace_lease_scope(workspace_scope);
        let worker_id = Some(worker_scope.as_str());
        let provider = route.provider.as_str();
        let tier_label = route.tier.as_str();
        let span = sandbox_provision_span("get_or_provision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            scope.admit()?;
            let workspace_binding = self
                .prepare_workspace_for_provision(route, session, workspace_scope, scope)
                .await?;
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
                        workspace_binding: &workspace_binding,
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
                        .create_hand(CreateHandContext {
                            route,
                            workspace_binding: &workspace_binding,
                            effective: &effective,
                            budget: scope.budget,
                            provisioning_operation_id: HandProvisioningOperationId::new(),
                            provisioning_deadline_at,
                        })
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
        let Some(lease) = lease_store
            .get(session.tenant_id, session.id, scope, provider)
            .await?
        else {
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
                .transition_status(session.tenant_id, &lease, HandLeaseStatus::Stale)
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
                    .transition_status(session.tenant_id, &lease, HandLeaseStatus::Stale)
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
                .renew_active(HandLeaseRenewRequest {
                    tenant_id: session.tenant_id,
                    session_id: session.id,
                    worker_id: scope,
                    provider,
                    generation: lease.generation,
                    provisioning_operation_id: lease.provisioning_operation_id,
                    attachment: lease.attachment.clone().ok_or_else(|| {
                        MoaError::StorageError(
                            "active hand lease is missing its workspace attachment".to_string(),
                        )
                    })?,
                    idle_expires_at: next_idle_deadline(policy),
                })
                .await?
            {
                self.remove_cached_binding_if_matches(&key, &cached.handle, cached.generation)
                    .await;
                return Ok(None);
            }
        }
        Ok(Some(cached.handle.clone()))
    }

    pub(in crate::core) async fn remove_cached_binding_if_matches(
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
            workspace_binding,
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
                    attachment: lease_attachment(workspace_binding)?,
                    policy,
                    caller_deadline: call_scope.budget.deadline,
                })
                .await?
            {
                let mut claim = claim;
                if let Err(error) = call_scope.admit() {
                    let _ = lease_store
                        .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
                        .await?;
                    return Err(error);
                }
                if let Some(previous_handle) = claim.handle.as_ref() {
                    let Some(provider_impl) = self.hands.providers.get(provider) else {
                        let _ = lease_store
                            .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(MoaError::ProviderError(format!(
                            "unknown hand provider: {provider}"
                        )));
                    };
                    if let Err(error) = call_scope.admit() {
                        let _ = lease_store
                            .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                    // Once destroy is dispatched it is deliberately outside a
                    // DeadlineGuard. Its exact provisioning fence is finalized
                    // even if cancellation arrives while the provider runs.
                    if let Err(error) = destroy_provisioning_operations(
                        provider_impl.as_ref(),
                        previous_handle
                            .handle
                            .provider_account()
                            .map_or(ProviderAccountId(Uuid::nil()), |context| context.0),
                        previous_handle
                            .handle
                            .provider_account()
                            .map_or(0, |context| context.1),
                        claim.provisioning_operation_id,
                        Some(previous_handle),
                        ProvisioningAbsenceProof::Immediate,
                    )
                    .await
                    {
                        let _ = lease_store
                            .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                    if !lease_store
                        .clear_handle_for_provisioning(session.tenant_id, &claim)
                        .await?
                    {
                        return Err(MoaError::StorageError(format!(
                            "hand lease replacement lost generation fence for session {} provider {provider}",
                            session.id
                        )));
                    }
                    claim.handle = None;
                    if let Err(error) = call_scope.admit() {
                        let _ = lease_store
                            .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
                            .await?;
                        return Err(error);
                    }
                }
                match self
                    .create_hand(CreateHandContext {
                        route,
                        workspace_binding,
                        effective,
                        budget: call_scope.budget,
                        provisioning_operation_id: claim.provisioning_operation_id,
                        provisioning_deadline_at: claim.provisioning_deadline_at,
                    })
                    .await
                {
                    Ok(handle) => {
                        super::sandbox_workspace::failpoints::hit(
                            "post_provider_create_pre_activation",
                        )
                        .await?;
                        return self
                            .finish_provisioned_hand(ProvisionedHandActivationContext {
                                session,
                                worker_id: scope,
                                provider,
                                workspace_binding,
                                cache_key: &key,
                                lease: &claim,
                                handle,
                                call_scope,
                                lease_store: lease_store.as_ref(),
                            })
                            .await;
                    }
                    Err(error) => {
                        if let Err(mark_error) = lease_store
                            .transition_status(session.tenant_id, &claim, HandLeaseStatus::Failed)
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

            if let Some(lease) = lease_store
                .get(session.tenant_id, session.id, scope, provider)
                .await?
            {
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
                                    .renew_active(HandLeaseRenewRequest {
                                        tenant_id: session.tenant_id,
                                        session_id: session.id,
                                        worker_id: scope,
                                        provider,
                                        generation: lease.generation,
                                        provisioning_operation_id: lease.provisioning_operation_id,
                                        attachment: lease.attachment.clone().ok_or_else(|| {
                                            MoaError::StorageError(
                                                "active hand lease is missing its workspace attachment"
                                                    .to_string(),
                                            )
                                        })?,
                                        idle_expires_at: next_idle_deadline(policy),
                                    })
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
                                    .transition_status(
                                        session.tenant_id,
                                        &lease,
                                        HandLeaseStatus::Stale,
                                    )
                                    .await?;
                                continue;
                            }
                        }
                    }
                    HandLeaseStatus::Provisioning => {
                        if let Some(handle) = self
                            .recover_provisioning_lease(ProvisionedHandRecoveryContext {
                                session,
                                worker_id: scope,
                                provider,
                                workspace_binding,
                                cache_key: &key,
                                lease: &lease,
                                policy,
                                call_scope,
                                lease_store: lease_store.as_ref(),
                            })
                            .await?
                        {
                            return Ok(handle);
                        }
                        if wait_started.elapsed() >= wait_budget {
                            break;
                        }
                        self.wait_for_provisioning(call_scope, wait_started, wait_budget)
                            .await?;
                        continue;
                    }
                    HandLeaseStatus::Failed => {
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
                            .transition_status(session.tenant_id, &lease, HandLeaseStatus::Stale)
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

    async fn recover_provisioning_lease(
        &self,
        context: ProvisionedHandRecoveryContext<'_>,
    ) -> Result<Option<HandHandle>> {
        let ProvisionedHandRecoveryContext {
            session,
            worker_id,
            provider,
            workspace_binding,
            cache_key,
            lease,
            policy,
            call_scope,
            lease_store,
        } = context;
        let expected_attachment = lease_attachment(workspace_binding)?;
        if !lease_matches_policy(lease, policy)
            || lease.attachment.as_ref() != Some(&expected_attachment)
        {
            let _ = lease_store
                .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                .await?;
            return Err(MoaError::StorageError(format!(
                "provisioning lease recovery lost its policy or workspace fence for session {} provider {provider}",
                session.id
            )));
        }
        let provider_impl =
            self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
        call_scope.admit()?;
        let mut handles = self
            .run_within_scope(
                call_scope,
                provider_impl.provisioned_hands(
                    workspace_binding.provider_account_id,
                    workspace_binding.provider_account_generation,
                    lease.provisioning_operation_id,
                ),
            )
            .await?;
        handles.sort_by_cached_key(hand_id);
        handles.dedup();
        if handles.len() > 1 {
            let _ = lease_store
                .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                .await?;
            return Err(MoaError::ProviderError(format!(
                "provider {provider} returned {} hands for provisioning operation {}; duplicate resources remain reaper-owned",
                handles.len(),
                lease.provisioning_operation_id
            )));
        }
        let Some(handle) = handles.pop() else {
            return Ok(None);
        };
        if handle.provider_account().is_some_and(|account| {
            account
                != (
                    workspace_binding.provider_account_id,
                    workspace_binding.provider_account_generation,
                )
        }) {
            let _ = lease_store
                .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                .await?;
            return Err(MoaError::ProviderError(format!(
                "provider {provider} returned a hand outside the provisioning account fence"
            )));
        }
        match self
            .run_within_scope(call_scope, provider_impl.status(&handle))
            .await?
        {
            HandStatus::Running => {}
            HandStatus::Provisioning => return Ok(None),
            HandStatus::Paused | HandStatus::Stopped => {
                call_scope.admit()?;
                provider_impl.resume(&handle).await?;
            }
            HandStatus::Destroyed | HandStatus::Failed => {
                let _ = lease_store
                    .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                    .await?;
                return Err(MoaError::ProviderError(format!(
                    "provider {provider} returned a non-routable hand for provisioning operation {}",
                    lease.provisioning_operation_id
                )));
            }
        }
        self.finish_provisioned_hand(ProvisionedHandActivationContext {
            session,
            worker_id,
            provider,
            workspace_binding,
            cache_key,
            lease,
            handle,
            call_scope,
            lease_store,
        })
        .await
        .map(Some)
    }

    async fn finish_provisioned_hand(
        &self,
        context: ProvisionedHandActivationContext<'_>,
    ) -> Result<HandHandle> {
        let ProvisionedHandActivationContext {
            session,
            worker_id,
            provider,
            workspace_binding,
            cache_key,
            lease,
            handle,
            call_scope,
            lease_store,
        } = context;
        let lease_handle = match self
            .lease_handle_for_provider(provider, lease.provisioning_operation_id, &handle)
            .await
        {
            Ok(lease_handle) => lease_handle,
            Err(error) => {
                self.destroy_unbound_operation(provider, lease.provisioning_operation_id, &handle)
                    .await;
                if let Err(mark_error) = lease_store
                    .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                    .await
                {
                    tracing::warn!(
                        session_id = %session.id,
                        worker_id,
                        provider,
                        generation = lease.generation,
                        error = %mark_error,
                        "failed to mark hand lease provisioning failure"
                    );
                }
                return Err(error);
            }
        };
        if let Err(error) = self
            .hydrate_provisioned_workspace(workspace_binding, lease, &handle, call_scope)
            .await
        {
            if let Err(mark_error) = lease_store
                .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                .await
            {
                tracing::warn!(
                    session_id = %session.id,
                    worker_id,
                    provider,
                    generation = lease.generation,
                    error = %mark_error,
                    "failed to terminalize a non-routable hydration lease"
                );
            }
            return Err(error);
        }
        let installed_manifest = match self
            .reinstall_trusted_files_before_activation(
                session, worker_id, provider, &handle, call_scope,
            )
            .await
        {
            Ok(manifest) => manifest,
            Err(error) => {
                if let Err(mark_error) = lease_store
                    .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                    .await
                {
                    tracing::warn!(
                        session_id = %session.id,
                        worker_id,
                        provider,
                        generation = lease.generation,
                        error = %mark_error,
                        "failed to terminalize a lease after trusted-file reinstall"
                    );
                }
                return Err(error);
            }
        };
        let activated = if let Some(repository) = self.hands.workspace_repository.as_ref() {
            repository
                .activate_hydrated(
                    super::sandbox_workspace::model::ActivateHydratedWorkspaceRequest {
                        binding: workspace_binding,
                        lease,
                        handle: lease_handle.clone(),
                    },
                )
                .await
        } else {
            lease_store
                .activate(HandLeaseActivateRequest {
                    tenant_id: session.tenant_id,
                    session_id: session.id,
                    worker_id,
                    provider,
                    generation: lease.generation,
                    handle: lease_handle.clone(),
                    attachment: lease_attachment(workspace_binding)?,
                })
                .await
        };
        match activated {
            Ok(true) => {
                let active = ActiveHand {
                    handle: handle.clone(),
                    generation: Some(lease.generation),
                };
                self.hands
                    .active_hands
                    .write()
                    .await
                    .insert(cache_key.to_string(), active.clone());
                self.remember_preactivation_manifest_install(
                    session,
                    worker_id,
                    provider,
                    cache_key,
                    &active,
                    installed_manifest.as_ref(),
                )
                .await;
                Ok(handle)
            }
            Ok(false) => {
                let already_active = lease_store
                    .get(session.tenant_id, session.id, worker_id, provider)
                    .await?
                    .is_some_and(|current| {
                        current.generation == lease.generation
                            && current.provisioning_operation_id == lease.provisioning_operation_id
                            && current.status == HandLeaseStatus::Active
                            && current.handle.as_ref() == Some(&lease_handle)
                            && current.attachment == lease.attachment
                    });
                if already_active {
                    let active = ActiveHand {
                        handle: handle.clone(),
                        generation: Some(lease.generation),
                    };
                    self.hands
                        .active_hands
                        .write()
                        .await
                        .insert(cache_key.to_string(), active.clone());
                    self.remember_preactivation_manifest_install(
                        session,
                        worker_id,
                        provider,
                        cache_key,
                        &active,
                        installed_manifest.as_ref(),
                    )
                    .await;
                    return Ok(handle);
                }
                self.cleanup_after_activation_fence_loss(
                    lease_store,
                    lease,
                    provider,
                    &lease_handle,
                    &handle,
                )
                .await;
                Err(MoaError::StorageError(format!(
                    "hand lease activation lost generation fence for session {} provider {provider}",
                    session.id
                )))
            }
            Err(error) => {
                match lease_store
                    .transition_status(session.tenant_id, lease, HandLeaseStatus::Failed)
                    .await
                {
                    Ok(true) => {
                        self.destroy_unbound_operation(
                            provider,
                            lease.provisioning_operation_id,
                            &handle,
                        )
                        .await;
                    }
                    Ok(false) => {
                        self.cleanup_after_activation_fence_loss(
                            lease_store,
                            lease,
                            provider,
                            &lease_handle,
                            &handle,
                        )
                        .await;
                    }
                    Err(mark_error) => tracing::warn!(
                        session_id = %session.id,
                        worker_id,
                        provider,
                        generation = lease.generation,
                        error = %mark_error,
                        "could not resolve ambiguous hand lease activation"
                    ),
                }
                Err(error)
            }
        }
    }

    async fn create_hand(&self, context: CreateHandContext<'_>) -> Result<HandHandle> {
        let CreateHandContext {
            route,
            workspace_binding,
            effective,
            budget,
            provisioning_operation_id,
            provisioning_deadline_at,
        } = context;
        let provider = route.provider.as_str();
        let tier = route.tier;
        let tier_label = tier.as_str();
        let span = sandbox_provision_span("create_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            let provider_impl = self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
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
                workspace: workspace_binding.clone(),
                sandbox_tier: tier,
                image: None,
                env: HashMap::new(),
                filesystem: SandboxFilesystemLayout::standard(),
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
        let account = handle.provider_account();
        if let Err(error) = destroy_provisioning_operations(
            provider_impl.as_ref(),
            account.map_or(ProviderAccountId(Uuid::nil()), |context| context.0),
            account.map_or(0, |context| context.1),
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
            .get(
                claim.tenant_id,
                claim.session_id,
                &claim.worker_id,
                provider,
            )
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

    pub(in crate::core) async fn remove_installed_marker(
        &self,
        scope: HandScopeKey,
        provider: &str,
    ) {
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
        workspace_scope: &SandboxWorkspaceScope,
        route: &HandRoute,
        call_scope: ToolCallScope<'_>,
    ) -> Result<HandHandle> {
        let worker_scope = workspace_lease_scope(workspace_scope);
        let scope = worker_scope.as_str();
        let worker_id = Some(scope);
        let provider = route.provider.as_str();
        let tier_label = route.tier.as_str();
        let span = sandbox_provision_span("reprovision_hand", provider, tier_label);
        let record_span = span.clone();
        async move {
            call_scope.admit()?;
            let key = session_provider_key(session, worker_id, provider);
            let cached = self.hands.active_hands.read().await.get(&key).cloned();

            if let Some(lease_store) = &self.hands.hand_leases {
                if let Some(lease) = lease_store
                    .get(session.tenant_id, session.id, scope, provider)
                    .await?
                {
                    let fenced = if lease.status == HandLeaseStatus::Reaping {
                        false
                    } else {
                        call_scope.admit()?;
                        lease_store
                            .transition_status(session.tenant_id, &lease, HandLeaseStatus::Stale)
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
                .get_or_provision_hand_within(route, session, workspace_scope, call_scope)
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

/// Constructs a deterministic binding only for direct adapters without durable repositories.
pub(in crate::core) fn workspace_binding_for_hand(
    session: &SessionMeta,
    workspace_scope: &SandboxWorkspaceScope,
    provider: &str,
) -> WorkspaceBinding {
    let scope_key = format!("tenant={};scope={workspace_scope:?}", session.tenant_id,);
    let workspace_uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, scope_key.as_bytes());
    let provider_account_uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, provider.as_bytes());
    WorkspaceBinding {
        tenant_id: session.tenant_id,
        scope: workspace_scope.clone(),
        workspace_id: SandboxWorkspaceId(workspace_uuid),
        provider_account_id: ProviderAccountId(provider_account_uuid),
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        writer_epoch: 1,
        instance_generation: 1,
        current_revision: None,
    }
}

/// Returns the stable lease/cache key segment for one typed workspace owner.
pub(super) fn workspace_lease_scope(scope: &SandboxWorkspaceScope) -> String {
    match scope {
        SandboxWorkspaceScope::Worker { worker_id, .. } => worker_id.clone(),
        SandboxWorkspaceScope::ExecutionTask { run_id, task_id } => {
            format!("execution:{run_id}:{task_id}")
        }
    }
}

/// Returns the scope key that namespaces a session's hands by typed owner.
///
/// A populated lease key yields `"{session_id}:{owner_key}"`. `None` yields the
/// non-owning `"{session_id}:"` aggregate key used only by session-wide
/// bookkeeping. Sandbox admission never accepts that aggregate key as a
/// workspace owner. All keys share the `"{session_id}:"` prefix so session
/// teardown can match every typed owner scope at once.
pub(super) fn scope_key(session: &SessionMeta, worker_id: Option<&str>) -> String {
    format!("{}:{}", session.id, worker_id.unwrap_or_default())
}

pub(in crate::core) fn manifest_scope_key(
    session: &SessionMeta,
    worker_id: Option<&str>,
) -> HandScopeKey {
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

/// Returns the cache/lease key for one hand within a typed owner scope.
///
/// The format is `"{session_id}:{owner_key}:{provider}"`. A missing owner key
/// produces the reserved non-owning aggregate form
/// `"{session_id}::{provider}"`; sandbox dispatch never provisions against it.
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
        HandHandle::Daytona { workspace_id, .. } => workspace_id.clone(),
        HandHandle::E2B { sandbox_id, .. } => sandbox_id.clone(),
    }
}

#[cfg(test)]
mod tests;

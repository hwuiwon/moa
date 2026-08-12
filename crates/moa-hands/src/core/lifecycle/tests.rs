//! Unit coverage for hand and managed-workspace lifecycle behavior.

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
    types::hands::SandboxTier,
    types::identifiers::{ToolCallId, WorkspaceCheckpointId},
    types::sandbox_workspace::WorkspaceCheckpointState,
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

#[test]
fn managed_restore_requires_the_exact_available_current_checkpoint_offline() {
    // Pins: a caller-authorized workspace ID cannot be paired with a stale,
    // historical, failed, or different checkpoint identity.
    let current = WorkspaceCheckpointId::new();
    validate_managed_restore_target(
        Some(current),
        4,
        current,
        current,
        4,
        WorkspaceCheckpointState::Available,
    )
    .expect("the exact available current head must be restorable");

    for (requested, checkpoint, generation, state) in [
        (
            WorkspaceCheckpointId::new(),
            current,
            4,
            WorkspaceCheckpointState::Available,
        ),
        (
            current,
            WorkspaceCheckpointId::new(),
            4,
            WorkspaceCheckpointState::Available,
        ),
        (current, current, 3, WorkspaceCheckpointState::Available),
        (current, current, 4, WorkspaceCheckpointState::Failed),
    ] {
        validate_managed_restore_target(Some(current), 4, requested, checkpoint, generation, state)
            .expect_err("every non-exact restore target must fail closed");
    }
}

struct CountingProvider {
    name: String,
    provision_delay: Duration,
    stale_generation_on_provision: Option<(
        Arc<MemoryHandLeaseStore>,
        TenantId,
        moa_core::types::identifiers::SessionId,
    )>,
    destroy_fails: bool,
    duplicate_discovery: bool,
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
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
        worker_id: &str,
        provider: &str,
    ) -> Result<Option<HandLease>> {
        self.inner
            .get(tenant_id, session_id, worker_id, provider)
            .await
    }

    async fn get_exact_generation(
        &self,
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
        worker_id: &str,
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId,
        generation: i64,
    ) -> Result<Option<HandLease>> {
        self.inner
            .get_exact_generation(
                tenant_id,
                session_id,
                worker_id,
                provisioning_operation_id,
                generation,
            )
            .await
    }

    async fn list_live_owner_candidates(
        &self,
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
        worker_id: &str,
    ) -> Result<Vec<HandLease>> {
        self.inner
            .list_live_owner_candidates(tenant_id, session_id, worker_id)
            .await
    }

    async fn has_live_owner(
        &self,
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
        worker_id: &str,
    ) -> Result<bool> {
        self.inner
            .has_live_owner(tenant_id, session_id, worker_id)
            .await
    }

    async fn list_live_session_page(
        &self,
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
        cursor: Option<&crate::core::leases::HandLeaseSessionCursor>,
    ) -> Result<crate::core::leases::HandLeaseSessionPage> {
        self.inner
            .list_live_session_page(tenant_id, session_id, cursor)
            .await
    }

    async fn activate(&self, request: HandLeaseActivateRequest<'_>) -> Result<bool> {
        self.barrier.wait(LeaseBarrierPoint::BeforeActivate).await;
        self.inner.activate(request).await
    }

    async fn clear_handle_for_provisioning(
        &self,
        tenant_id: TenantId,
        claim: &HandLease,
    ) -> Result<bool> {
        self.barrier.wait(LeaseBarrierPoint::BeforeClear).await;
        self.inner
            .clear_handle_for_provisioning(tenant_id, claim)
            .await
    }

    async fn renew_active(&self, request: HandLeaseRenewRequest<'_>) -> Result<bool> {
        self.inner.renew_active(request).await
    }

    async fn transition_status(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        status: HandLeaseStatus,
    ) -> Result<bool> {
        self.barrier.wait(LeaseBarrierPoint::BeforeTransition).await;
        self.inner
            .transition_status(tenant_id, expected, status)
            .await
    }

    async fn claim_for_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_ttl: Duration,
    ) -> Result<Option<uuid::Uuid>> {
        self.inner
            .claim_for_destroy(tenant_id, expected, claim_ttl)
            .await
    }

    async fn finalize_destroy(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: uuid::Uuid,
    ) -> Result<bool> {
        self.inner
            .finalize_destroy(tenant_id, expected, claim_token)
            .await
    }

    async fn release_destroy_claim(
        &self,
        tenant_id: TenantId,
        expected: &HandLease,
        claim_token: uuid::Uuid,
        retry_after: Duration,
    ) -> Result<bool> {
        self.inner
            .release_destroy_claim(tenant_id, expected, claim_token, retry_after)
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
            duplicate_discovery: false,
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
        tenant_id: TenantId,
        session_id: moa_core::types::identifiers::SessionId,
    ) -> Self {
        self.stale_generation_on_provision = Some((lease_store, tenant_id, session_id));
        self
    }

    fn with_destroy_failure(mut self) -> Self {
        self.destroy_fails = true;
        self
    }

    fn with_duplicate_discovery(mut self) -> Self {
        self.duplicate_discovery = true;
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
        if let Some((lease_store, tenant_id, session_id)) = &self.stale_generation_on_provision {
            let lease = lease_store
                .get(*tenant_id, *session_id, TEST_WORKER_ID, &self.name)
                .await?
                .ok_or_else(|| MoaError::StorageError("missing test lease".to_string()))?;
            let _ = lease_store
                .transition_status(*tenant_id, &lease, HandLeaseStatus::Stale)
                .await?;
        }
        Ok(handle)
    }

    async fn provisioned_hands(
        &self,
        _provider_account_id: ProviderAccountId,
        _provider_account_generation: u64,
        operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        let provisioned = self
            .provisioned
            .lock()
            .map_err(|_| MoaError::ProviderError("lock counting provider resources".to_string()))?;
        let mut handles = provisioned
            .get(&operation_id)
            .map(|(_, handle)| vec![handle.clone()])
            .unwrap_or_default();
        if self.duplicate_discovery && !handles.is_empty() {
            handles.push(HandHandle::docker(format!("{}-duplicate", self.name)));
        }
        Ok(handles)
    }

    async fn execute(&self, _handle: &HandHandle, _tool: &str, _input: &str) -> Result<ToolOutput> {
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
            .map_err(|_| MoaError::ProviderError("lock counting provider resources".to_string()))?
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

const TEST_WORKER_ID: &str = "lifecycle-test-worker";

fn workspace_scope(session: &SessionMeta, worker_id: &str) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::Worker {
        session_id: session.id,
        worker_id: worker_id.to_string(),
    }
}

fn sandbox_file(contents: &[u8]) -> SandboxFile {
    SandboxFile {
        path: ".moa/skills/r3/SKILL.md".to_string(),
        content: contents.to_vec(),
        executable: false,
    }
}

async fn seed_ambiguous_provisioning(
    router: &ToolRouter,
    provider: &CountingProvider,
    lease_store: &MemoryHandLeaseStore,
    session: &SessionMeta,
    route: &HandRoute,
) -> (HandLease, HandHandle) {
    let workspace_scope = workspace_scope(session, TEST_WORKER_ID);
    let binding = workspace_binding_for_hand(session, &workspace_scope, provider.provider_name());
    let effective = router
        .resolve_sandbox_profile(route, session)
        .await
        .expect("resolve the provisioning policy");
    let policy = HandLeasePolicy::from_effective(&effective);
    let lease = lease_store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id: session.id,
            worker_id: TEST_WORKER_ID,
            tenant_id: session.tenant_id,
            provider: provider.provider_name(),
            tier: route.tier,
            attachment: lease_attachment(&binding).expect("workspace attachment validates"),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim ambiguous provisioning lease")
        .expect("fresh provisioning claim wins");
    let handle = provider
        .provision(HandSpec {
            provisioning_operation_id: lease.provisioning_operation_id,
            workspace: binding,
            sandbox_tier: route.tier,
            image: None,
            env: HashMap::new(),
            filesystem: SandboxFilesystemLayout::standard(),
            effective_profile: effective,
            budget: ResourceBudget::until(lease.provisioning_deadline_at),
        })
        .await
        .expect("provider create succeeds before simulated process loss");
    (lease, handle)
}

#[tokio::test]
async fn provisioning_replay_discovers_and_activates_the_exact_created_hand() {
    // Pins: a process loss after provider create but before lease activation
    // recovers by the persisted operation/account fence, never by creating
    // a second hand or waiting for the provisioning row to time out.
    let lease_store = MemoryHandLeaseStore::shared();
    let provider = Arc::new(CountingProvider::new("provisioning-replay"));
    let router = router(provider.clone(), lease_store.clone());
    let session = session();
    let route = test_hand_route(provider.provider_name());
    let (provisioning, created) = seed_ambiguous_provisioning(
        &router,
        provider.as_ref(),
        lease_store.as_ref(),
        &session,
        &route,
    )
    .await;

    let recovered = router
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded().with_budget(ResourceBudget::until(
                Utc::now() + ChronoDuration::milliseconds(500),
            )),
        )
        .await
        .expect("replay discovers and activates the created hand");

    assert_eq!(recovered, created);
    assert_eq!(
        provider.provision_calls(),
        1,
        "replay must not create again"
    );
    assert_eq!(provider.destroy_calls(), 0);
    let active = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load recovered lease")
        .expect("recovered lease exists");
    assert_eq!(active.status, HandLeaseStatus::Active);
    assert_eq!(active.generation, provisioning.generation);
    assert_eq!(
        active.provisioning_operation_id,
        provisioning.provisioning_operation_id
    );
    assert_eq!(
        active.handle.as_ref().map(|handle| &handle.handle),
        Some(&created)
    );
}

#[tokio::test]
async fn provisioning_replay_fails_closed_when_provider_discovery_has_duplicates() {
    // Pins: ambiguous providers that expose two live resources for one
    // provisioning operation cannot choose an arbitrary writable hand;
    // the exact lease becomes reaper-owned failed state instead.
    let lease_store = MemoryHandLeaseStore::shared();
    let provider =
        Arc::new(CountingProvider::new("provisioning-duplicates").with_duplicate_discovery());
    let router = router(provider.clone(), lease_store.clone());
    let session = session();
    let route = test_hand_route(provider.provider_name());
    let (provisioning, _) = seed_ambiguous_provisioning(
        &router,
        provider.as_ref(),
        lease_store.as_ref(),
        &session,
        &route,
    )
    .await;

    let error = router
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect_err("duplicate provider resources fail closed");
    assert!(
        matches!(error, MoaError::ProviderError(message) if message.contains("returned 2 hands"))
    );
    assert_eq!(
        provider.provision_calls(),
        1,
        "replay must not create again"
    );
    let failed = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load duplicate-fenced lease")
        .expect("duplicate-fenced lease exists");
    assert_eq!(failed.status, HandLeaseStatus::Failed);
    assert_eq!(failed.generation, provisioning.generation);
    assert_eq!(failed.handle, None);
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
    let workspace_scope = workspace_scope(&session, TEST_WORKER_ID);
    let call = router.get_or_provision_hand_within(
        &route,
        &session,
        &workspace_scope,
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("provision hand A");
    let active = inner
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load active hand A lease")
        .expect("hand A lease exists");
    assert!(
        inner
            .transition_status(session.tenant_id, &active, HandLeaseStatus::Stale)
            .await
            .expect("mark hand A stale")
    );

    let cancel = CancellationToken::new();
    let workspace_scope = workspace_scope(&session, TEST_WORKER_ID);
    let call = router.get_or_provision_hand_within(
        &route,
        &session,
        &workspace_scope,
        ToolCallScope::from_tokens(Some(&cancel), Some(&cancel)),
    );
    let cancel_after_destroy = async {
        clear_rx
            .await
            .expect("destroy should reach durable clear barrier");
        assert_eq!(provider.destroy_calls(), 1);
        let replacing = inner
            .get(
                session.tenant_id,
                session.id,
                TEST_WORKER_ID,
                provider.provider_name(),
            )
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
    let workspace_scope = workspace_scope(&session, TEST_WORKER_ID);
    let key = session_provider_key(&session, Some(TEST_WORKER_ID), provider.provider_name());
    let call = router.get_or_provision_hand_within(
        &route,
        &session,
        &workspace_scope,
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("provision hand A");
    let active = inner
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load active hand A lease")
        .expect("hand A lease exists");

    let workspace_scope = workspace_scope(&session, TEST_WORKER_ID);
    let call = router.reprovision_hand(
        &session,
        &workspace_scope,
        &route,
        ToolCallScope::unbounded(),
    );
    let reaper_wins = async {
        transition_rx
            .await
            .expect("recovery should reach stale transition barrier");
        let claim_token = inner
            .claim_for_destroy(session.tenant_id, &active, HAND_DESTROY_CLAIM_TTL)
            .await
            .expect("reaper claim succeeds")
            .expect("reaper owns the exact active generation");
        provider
            .destroy(&hand_a)
            .await
            .expect("simulated reaper destroys hand A");
        assert!(
            inner
                .finalize_destroy(session.tenant_id, &active, claim_token)
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
        CountingProvider::new("manifest-race").with_first_install_barrier(started_tx, release_rx),
    );
    let lease_store = MemoryHandLeaseStore::shared();
    let router = Arc::new(router(provider.clone(), lease_store.clone()));
    let session = Arc::new(session());
    let route = test_hand_route(provider.provider_name());
    let files = vec![sandbox_file(b"trusted")];
    let hand_a = router
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("provision hand A");
    router
        .set_trusted_sandbox_files(&session, Some(TEST_WORKER_ID), files.clone())
        .await;

    let first_router = Arc::clone(&router);
    let first_session = Arc::clone(&session);
    let first_handle = hand_a.clone();
    let first = tokio::spawn(async move {
        first_router
            .install_trusted_files_for_hand(
                &first_session,
                Some(TEST_WORKER_ID),
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
        router.reprovision_hand(
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            &route,
            ToolCallScope::unbounded(),
        ),
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

    let marker_scope = manifest_scope_key(&session, Some(TEST_WORKER_ID));
    let marker = router
        .hands
        .installed_files
        .read()
        .await
        .get(&marker_scope)
        .and_then(|providers| providers.get("manifest-race"))
        .cloned()
        .expect("replacement hand B has an installed marker");
    let active = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            "manifest-race",
        )
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
async fn preactivation_marker_requires_exact_active_generation_and_manifest() {
    // Pins: a preactivation install may publish its marker only after the
    // same handle/generation is active and the installed manifest remains
    // authoritative; any stale fence leaves the marker absent.
    let provider = Arc::new(CountingProvider::new("preactivation-marker"));
    let router = router(provider.clone(), MemoryHandLeaseStore::shared());
    let session = session();
    let worker_id = TEST_WORKER_ID;
    let manifest_key = manifest_scope_key(&session, Some(worker_id));
    let cache_key = session_provider_key(&session, Some(worker_id), provider.provider_name());
    let hand_a = HandHandle::docker("preactivation-hand-a");
    let hand_b = HandHandle::docker("preactivation-hand-b");
    let installed = ActiveHand {
        handle: hand_a.clone(),
        generation: Some(1),
    };

    router
        .set_trusted_sandbox_files(
            &session,
            Some(worker_id),
            vec![sandbox_file(b"manifest-one")],
        )
        .await;
    let first_manifest = router
        .hands
        .trusted_sandbox_files
        .read()
        .await
        .get(&manifest_key)
        .cloned()
        .expect("first manifest is stored");

    router.hands.active_hands.write().await.insert(
        cache_key.clone(),
        ActiveHand {
            handle: hand_b,
            generation: Some(1),
        },
    );
    router
        .remember_preactivation_manifest_install(
            &session,
            worker_id,
            provider.provider_name(),
            &cache_key,
            &installed,
            Some(&first_manifest),
        )
        .await;
    assert!(router.hands.installed_files.read().await.is_empty());

    router.hands.active_hands.write().await.insert(
        cache_key.clone(),
        ActiveHand {
            handle: hand_a.clone(),
            generation: Some(2),
        },
    );
    router
        .remember_preactivation_manifest_install(
            &session,
            worker_id,
            provider.provider_name(),
            &cache_key,
            &installed,
            Some(&first_manifest),
        )
        .await;
    assert!(router.hands.installed_files.read().await.is_empty());

    router
        .hands
        .active_hands
        .write()
        .await
        .insert(cache_key.clone(), installed.clone());
    router
        .set_trusted_sandbox_files(
            &session,
            Some(worker_id),
            vec![sandbox_file(b"manifest-two")],
        )
        .await;
    router
        .remember_preactivation_manifest_install(
            &session,
            worker_id,
            provider.provider_name(),
            &cache_key,
            &installed,
            Some(&first_manifest),
        )
        .await;
    assert!(router.hands.installed_files.read().await.is_empty());

    let current_manifest = router
        .hands
        .trusted_sandbox_files
        .read()
        .await
        .get(&manifest_key)
        .cloned()
        .expect("replacement manifest is stored");
    router
        .remember_preactivation_manifest_install(
            &session,
            worker_id,
            provider.provider_name(),
            &cache_key,
            &installed,
            Some(&current_manifest),
        )
        .await;
    let marker = router
        .hands
        .installed_files
        .read()
        .await
        .get(&manifest_key)
        .and_then(|providers| providers.get(provider.provider_name()))
        .cloned()
        .expect("exact current fences publish the marker");
    assert_eq!(marker.handle, hand_a);
    assert_eq!(marker.generation, Some(1));
    assert_eq!(marker.manifest_identity, current_manifest.identity);
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
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
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
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
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
    let left_workspace_scope = workspace_scope(&left_session, TEST_WORKER_ID);
    let right_workspace_scope = workspace_scope(&right_session, TEST_WORKER_ID);

    let secured = tokio::join!(
        left_router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &left_session,
            caller_identity: &left_identity,
            workspace_scope: Some(&left_workspace_scope),
            invocation: &left_invocation,
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        }),
        right_router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &right_session,
            caller_identity: &right_identity,
            workspace_scope: Some(&right_workspace_scope),
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
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("first router provisions and executes");
    cleanup_router
        .reclaim_hands(session.tenant_id, &session.id, None)
        .await;

    assert_eq!(provider.destroy_calls(), 1);
}

#[tokio::test]
async fn lifecycle_session_cleanup_reports_incomplete_until_every_cache_page_is_destroyed() {
    // Pins: terminal session cleanup processes at most one non-empty 64-entry cache page per
    // activation, reports incomplete while another page remains, and reaches exact completion
    // without leaking the final short page.
    let lease_store = MemoryHandLeaseStore::shared();
    let provider = Arc::new(CountingProvider::new("paged-session-cleanup"));
    let session = session();
    let router = router(provider.clone(), lease_store);
    let count = crate::core::HAND_LEASE_SESSION_PAGE_SIZE + 7;

    for index in 0..count {
        let scope = HandScopeKey::new(
            session.tenant_id,
            session.id,
            format!("session-owner-{index:03}"),
        );
        router.hands.active_hands.write().await.insert(
            crate::core::HandProviderCacheKey::new(scope, provider.provider_name()),
            ActiveHand {
                handle: HandHandle::local(std::path::PathBuf::from(format!(
                    "/tmp/session-cleanup-{index}"
                ))),
                generation: None,
            },
        );
    }

    assert!(
        !router
            .reclaim_hands(session.tenant_id, &session.id, None)
            .await,
        "the first bounded page must request a durable continuation"
    );
    assert_eq!(
        provider.destroy_calls(),
        crate::core::HAND_LEASE_SESSION_PAGE_SIZE
    );
    assert!(
        router
            .reclaim_hands(session.tenant_id, &session.id, None)
            .await,
        "the final short page must prove complete cleanup"
    );
    assert_eq!(provider.destroy_calls(), count);
    assert!(router.hands.active_hands.read().await.is_empty());
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
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("first execution provisions");
    let first = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load first lease")
        .expect("first lease should exist");
    let short_expiry = Utc::now() + ChronoDuration::seconds(5);
    assert!(
        lease_store
            .renew_active(HandLeaseRenewRequest {
                tenant_id: session.tenant_id,
                session_id: session.id,
                worker_id: TEST_WORKER_ID,
                provider: provider.provider_name(),
                generation: first.generation,
                provisioning_operation_id: first.provisioning_operation_id,
                attachment: first.attachment.clone().expect("active lease attachment"),
                idle_expires_at: short_expiry,
            })
            .await
            .expect("shrink active lease expiry")
    );

    router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session,
            caller_identity: &identity(),
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("second execution reuses renewed lease");
    let renewed = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
            .transition_status(session.tenant_id, &renewed, HandLeaseStatus::Stale)
            .await
            .expect("mark lease stale")
    );
    let replacement_result = tokio::time::timeout(
        Duration::from_secs(1),
        router.execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session,
            caller_identity: &identity(),
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
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
                .get(
                    session.tenant_id,
                    session.id,
                    TEST_WORKER_ID,
                    provider.provider_name(),
                )
                .await
                .expect("load lease after replacement timeout");
            panic!(
                "stale durable lease replacement should not wait on provisioning; timeout={error:?}; lease={lease:?}"
            );
        }
    }

    let replacement = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
        CountingProvider::new("activation-fence").with_stale_generation_on_provision(
            lease_store.clone(),
            session.tenant_id,
            session.id,
        ),
    );
    let router = router(provider.clone(), lease_store.clone());

    let error = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session,
            caller_identity: &identity(),
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
            workspace_scope: Some(&workspace_scope(&session, TEST_WORKER_ID)),
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("provision before cleanup");
    cleanup_router
        .reclaim_hands(session.tenant_id, &session.id, None)
        .await;

    assert_eq!(provider.destroy_calls(), 1);
    let lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("session-scope hand provisions");
    let child = router
        .get_or_provision_hand_within(
            &test_hand_route(provider.provider_name()),
            &session,
            &workspace_scope(&session, "sub-x"),
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load session lease")
        .expect("session lease exists");
    let child_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            "sub-x",
            provider.provider_name(),
        )
        .await
        .expect("load worker lease")
        .expect("worker lease exists");
    assert_eq!(root_lease.worker_id, TEST_WORKER_ID);
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
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("session-scope hand provisions");
    router
        .get_or_provision_hand_within(
            &test_hand_route(provider.provider_name()),
            &session,
            &workspace_scope(&session, "sub-x"),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("worker-scope hand provisions");
    assert_eq!(provider.provision_calls(), 2);

    router
        .reclaim_hands(session.tenant_id, &session.id, None)
        .await;

    assert_eq!(
        provider.destroy_calls(),
        2,
        "teardown releases every scope under the session"
    );
    let root_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load session lease")
        .expect("session lease row remains");
    let child_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            "sub-x",
            provider.provider_name(),
        )
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

    for worker_id in [TEST_WORKER_ID, "sub-x", "sub-y"] {
        let scope = workspace_scope(&session, worker_id);
        router
            .get_or_provision_hand_within(
                &test_hand_route(provider.provider_name()),
                &session,
                &scope,
                ToolCallScope::unbounded(),
            )
            .await
            .expect("scope hand provisions");
    }
    assert_eq!(provider.provision_calls(), 3);

    assert!(
        router
            .reclaim_hands(session.tenant_id, &session.id, Some("sub-x"))
            .await,
        "target worker cleanup should fully complete"
    );

    assert_eq!(
        provider.destroy_calls(),
        1,
        "only the target worker's hand is destroyed"
    );
    let session_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
        .await
        .expect("load session lease")
        .expect("session lease row remains");
    let target_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            "sub-x",
            provider.provider_name(),
        )
        .await
        .expect("load target lease")
        .expect("target lease row remains");
    let sibling_lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            "sub-y",
            provider.provider_name(),
        )
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
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("session-scope hand reused");
    router
        .get_or_provision_hand_within(
            &test_hand_route(provider.provider_name()),
            &session,
            &workspace_scope(&session, "sub-y"),
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
            &workspace_scope(&session, "sub-x"),
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
    let provider = Arc::new(CountingProvider::new("worker-destroy-retry").with_destroy_failure());
    let session = session();
    let first_router = router(provider.clone(), lease_store.clone());
    let cleanup_router = router(provider.clone(), lease_store.clone());

    first_router
        .get_or_provision_hand_within(
            &test_hand_route(provider.provider_name()),
            &session,
            &workspace_scope(&session, "sub-x"),
            ToolCallScope::unbounded(),
        )
        .await
        .expect("worker scope provisions");
    assert!(
        !cleanup_router
            .reclaim_hands(session.tenant_id, &session.id, Some("sub-x"))
            .await,
        "failed provider destroy should report incomplete cleanup"
    );

    assert_eq!(provider.destroy_calls(), 1);
    let lease = lease_store
        .get(
            session.tenant_id,
            session.id,
            "sub-x",
            provider.provider_name(),
        )
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
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
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
        .get_or_provision_hand_within(
            &route,
            &session,
            &workspace_scope(&session, TEST_WORKER_ID),
            ToolCallScope::unbounded(),
        )
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
        .get(
            session.tenant_id,
            session.id,
            TEST_WORKER_ID,
            provider.provider_name(),
        )
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

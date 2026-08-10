//! Offline contract coverage for typed sandbox workspace recovery.
//!
//! These tests drive the public [`moa_hands::ToolRouter`] dispatch seam with
//! observable providers. They pin the points where an absent workspace owner
//! or a tempting fallback route must fail closed before new provider state is
//! created.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    error::{MoaError, Result},
    traits::{HandProvider, Identity, IdentityType},
    types::{
        completion::ToolInvocation,
        hands::{
            BuiltinPolicyRevision, DeadlineEnforcement, EgressMode, HandHandle,
            HandProviderCapabilities, HandSpec, HandStatus, ResourceSupport, SandboxPolicySnapshot,
            SandboxTier, SandboxTierCapabilities,
        },
        identifiers::{HandProvisioningOperationId, ModelId, SessionId, TenantId, ToolCallId},
        sandbox_workspace::SandboxWorkspaceScope,
        session::SessionMeta,
        tools::ToolOutput,
    },
};
use moa_hands::{
    AuthorizedToolCall, HandRoute, ToolCallScope, ToolRegistry, ToolRouter,
    core::sandbox_workspace::checkpoint::archive::{
        ArchiveLimits, build_checkpoint_archive, restore_checkpoint_archive,
    },
    local_development_sandbox_policy,
};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProviderIoSnapshot {
    provision: u32,
    discover: u32,
    execute: u32,
    health: u32,
    status: u32,
    pause: u32,
    resume: u32,
    destroy: u32,
}

#[derive(Default)]
struct ProviderState {
    io: ProviderIoSnapshot,
    provision_results: VecDeque<Result<()>>,
    execute_results: VecDeque<Result<ToolOutput>>,
}

#[derive(Clone)]
struct ObservableProvider {
    name: &'static str,
    state: Arc<Mutex<ProviderState>>,
}

impl ObservableProvider {
    fn new(name: &'static str, state: ProviderState) -> Self {
        Self {
            name,
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn io(&self) -> ProviderIoSnapshot {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
    }
}

fn provider_capabilities() -> HandProviderCapabilities {
    let tier = |tier| SandboxTierCapabilities {
        tier,
        cpu: ResourceSupport::unbounded_only(),
        memory: ResourceSupport::unbounded_only(),
        ephemeral_disk: ResourceSupport::unbounded_only(),
        egress_modes: vec![
            EgressMode::DenyAll,
            EgressMode::AllowList,
            EgressMode::Unrestricted,
        ],
        idle_enforcement: DeadlineEnforcement::DurableReaper,
        max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
    };
    HandProviderCapabilities {
        revision: "sandbox-workspace-recovery-offline-v1".to_string(),
        tiers: vec![
            tier(SandboxTier::Local),
            tier(SandboxTier::Container),
            tier(SandboxTier::MicroVM),
        ],
    }
}

#[async_trait]
impl HandProvider for ObservableProvider {
    fn provider_name(&self) -> &str {
        self.name
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        provider_capabilities()
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        let mut state = self
            .state
            .lock()
            .expect("observable provider state should not be poisoned");
        state.io.provision += 1;
        if let Some(result) = state.provision_results.pop_front() {
            result?;
        }
        Ok(HandHandle::docker(format!(
            "{}-{}",
            self.name, spec.provisioning_operation_id
        )))
    }

    async fn provisioned_hands(
        &self,
        _provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        _provider_account_generation: u64,
        _operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .discover += 1;
        Ok(Vec::new())
    }

    async fn execute(&self, _handle: &HandHandle, _tool: &str, _input: &str) -> Result<ToolOutput> {
        let mut state = self
            .state
            .lock()
            .expect("observable provider state should not be poisoned");
        state.io.execute += 1;
        state
            .execute_results
            .pop_front()
            .unwrap_or_else(|| Ok(ToolOutput::text("ok", Duration::from_millis(1))))
    }

    async fn health_check(&self, _handle: &HandHandle) -> Result<bool> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .health += 1;
        Ok(true)
    }

    async fn status(&self, _handle: &HandHandle) -> Result<HandStatus> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .status += 1;
        Ok(HandStatus::Running)
    }

    async fn pause(&self, _handle: &HandHandle) -> Result<()> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .pause += 1;
        Ok(())
    }

    async fn resume(&self, _handle: &HandHandle) -> Result<()> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .resume += 1;
        Ok(())
    }

    async fn destroy(&self, _handle: &HandHandle) -> Result<()> {
        self.state
            .lock()
            .expect("observable provider state should not be poisoned")
            .io
            .destroy += 1;
        Ok(())
    }
}

fn route(provider: &ObservableProvider, tier: SandboxTier) -> HandRoute {
    HandRoute {
        provider: provider.provider_name().to_string(),
        tier,
        policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
    }
}

fn router(providers: &[Arc<ObservableProvider>], routes: Vec<HandRoute>) -> ToolRouter {
    let mut registry = ToolRegistry::default_local();
    registry.retarget_hand_tools(routes);
    registry.retain_only(["bash"]);
    let providers = providers
        .iter()
        .map(|provider| {
            (
                provider.provider_name().to_string(),
                Arc::clone(provider) as Arc<dyn HandProvider>,
            )
        })
        .collect::<HashMap<_, _>>();
    ToolRouter::new(registry, providers, local_development_sandbox_policy())
}

struct CallFixture {
    session: SessionMeta,
    identity: Identity,
    workspace_scope: SandboxWorkspaceScope,
    invocation: ToolInvocation,
}

impl CallFixture {
    fn new() -> Self {
        let tenant_id = TenantId::from(Uuid::new_v4());
        let session_id = SessionId(Uuid::new_v4());
        Self {
            session: SessionMeta {
                id: session_id,
                tenant_id,
                model: ModelId::new("sandbox-workspace-recovery-offline"),
                ..SessionMeta::default()
            },
            identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::new_v4(),
                tenant_id,
                api_key_id: None,
                acting_on_behalf_of: None,
            },
            workspace_scope: SandboxWorkspaceScope::Worker {
                session_id,
                worker_id: format!("worker-{}", Uuid::new_v4()),
            },
            invocation: ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "printf ok" }),
            },
        }
    }

    fn request<'a>(
        &'a self,
        workspace_scope: Option<&'a SandboxWorkspaceScope>,
    ) -> AuthorizedToolCall<'a> {
        AuthorizedToolCall {
            session: &self.session,
            caller_identity: &self.identity,
            workspace_scope,
            invocation: &self.invocation,
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: ToolCallScope::unbounded(),
        }
    }
}

#[tokio::test]
async fn sandbox_call_without_typed_scope_fails_before_provider_io_offline() {
    // Pins: a coordinator or bare session cannot acquire a sandbox implicitly;
    // rejection happens before provisioning, recovery discovery, or execution.
    let provider = Arc::new(ObservableProvider::new(
        "scope-guard",
        ProviderState::default(),
    ));
    let router = router(
        &[Arc::clone(&provider)],
        vec![route(&provider, SandboxTier::Local)],
    );
    let call = CallFixture::new();

    let error = router
        .execute_authorized_with_recovery(call.request(None))
        .await
        .expect_err("a sandbox call without a typed owner must fail closed");

    assert!(
        matches!(error, MoaError::PermissionDenied(ref message) if message == "sandbox tools require a typed worker or execution-task workspace scope"),
        "the rejection must identify the missing typed workspace scope: {error}"
    );
    assert_eq!(
        provider.io(),
        ProviderIoSnapshot::default(),
        "missing workspace identity must cause exactly zero provider I/O"
    );
}

#[tokio::test]
async fn typed_durable_scope_disables_provider_fallback_before_execution_offline() {
    // Pins: once a typed workspace owner exists, even a primary provisioning
    // failure cannot create an empty workspace on a different provider.
    let primary = Arc::new(ObservableProvider::new(
        "pinned-primary",
        ProviderState {
            provision_results: VecDeque::from([Err(MoaError::ProviderError(
                "primary unavailable".to_string(),
            ))]),
            ..ProviderState::default()
        },
    ));
    let fallback = Arc::new(ObservableProvider::new(
        "blank-fallback",
        ProviderState::default(),
    ));
    let router = router(
        &[Arc::clone(&primary), Arc::clone(&fallback)],
        vec![
            route(&primary, SandboxTier::Container),
            route(&fallback, SandboxTier::MicroVM),
        ],
    );
    let call = CallFixture::new();

    let error = router
        .execute_authorized_with_recovery(call.request(Some(&call.workspace_scope)))
        .await
        .expect_err("a pinned workspace must surface the primary provisioning failure");

    assert!(
        matches!(error, MoaError::ProviderError(ref message) if message == "primary unavailable"),
        "the original pinned-provider error must be preserved: {error}"
    );
    assert_eq!(primary.io().provision, 1);
    assert_eq!(
        fallback.io(),
        ProviderIoSnapshot::default(),
        "the fallback provider must not be touched for a typed durable workspace"
    );
}

#[tokio::test]
async fn uncertain_mutating_failure_never_starts_blank_fallback_offline() {
    // Pins: an ambiguous command outcome is returned as reconciliation work;
    // neither retry nor a blank provider fallback may execute the command again.
    let primary = Arc::new(ObservableProvider::new(
        "uncertain-primary",
        ProviderState {
            execute_results: VecDeque::from([Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: "uncertain-command-1".to_string(),
            })]),
            ..ProviderState::default()
        },
    ));
    let fallback = Arc::new(ObservableProvider::new(
        "uncertain-fallback",
        ProviderState::default(),
    ));
    let router = router(
        &[Arc::clone(&primary), Arc::clone(&fallback)],
        vec![
            route(&primary, SandboxTier::Container),
            route(&fallback, SandboxTier::MicroVM),
        ],
    );
    let call = CallFixture::new();

    let error = router
        .execute_authorized_with_recovery(call.request(Some(&call.workspace_scope)))
        .await
        .expect_err("an uncertain command outcome must remain non-success");

    assert!(
        matches!(error, MoaError::ExternalEffectUnknownOutcome { ref operation_id } if operation_id == "uncertain-command-1"),
        "the exact ambiguous operation must remain available to reconciliation: {error}"
    );
    assert_eq!(primary.io().provision, 1);
    assert_eq!(primary.io().execute, 1);
    assert_eq!(
        fallback.io(),
        ProviderIoSnapshot::default(),
        "an unknown mutating outcome must never start a blank fallback"
    );
}

#[tokio::test]
async fn portable_restore_cannot_resurrect_control_plane_state_offline() {
    // Pins: a portable checkpoint reads and restores only the mutable tenant
    // root; credentials, policy, authorization, trusted-file status, runtime
    // controls, and network state remain the fresh compute instance's values.
    let temporary = TempDir::new().expect("create recovery isolation root");
    let source = temporary.path().join("source");
    let source_mutable = source.join("workspace");
    let source_trusted = source.join("trusted");
    let source_runtime = source.join("runtime");
    fs::create_dir_all(&source_mutable).expect("create source mutable root");
    fs::create_dir_all(&source_trusted).expect("create source trusted root");
    fs::create_dir_all(&source_runtime).expect("create source runtime root");
    fs::write(source_mutable.join("durable.txt"), b"tenant durable bytes")
        .expect("write durable tenant marker");
    for (path, bytes) in [
        (
            source_trusted.join("credential"),
            b"old-credential".as_slice(),
        ),
        (source_trusted.join("policy"), b"old-policy".as_slice()),
        (
            source_trusted.join("authorization"),
            b"old-authorization".as_slice(),
        ),
        (
            source_trusted.join("trusted-status"),
            b"old-trusted-status".as_slice(),
        ),
        (
            source_runtime.join("runtime-control"),
            b"old-runtime-control".as_slice(),
        ),
        (
            source_runtime.join("network-state"),
            b"old-network-state".as_slice(),
        ),
    ] {
        fs::write(path, bytes).expect("write excluded source control state");
    }

    let archive = build_checkpoint_archive(&source_mutable, ArchiveLimits::default())
        .await
        .expect("build portable archive from only the mutable root");
    assert_eq!(
        archive
            .manifest
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec!["durable.txt"],
        "the archive manifest must contain no sibling control-plane paths"
    );

    let replacement = temporary.path().join("replacement");
    let replacement_mutable = replacement.join("workspace");
    let replacement_trusted = replacement.join("trusted");
    let replacement_runtime = replacement.join("runtime");
    fs::create_dir_all(&replacement_trusted).expect("create fresh trusted root");
    fs::create_dir_all(&replacement_runtime).expect("create fresh runtime root");
    let current_controls = [
        (
            replacement_trusted.join("credential"),
            b"current-credential".as_slice(),
        ),
        (
            replacement_trusted.join("policy"),
            b"current-policy".as_slice(),
        ),
        (
            replacement_trusted.join("authorization"),
            b"current-authorization".as_slice(),
        ),
        (
            replacement_trusted.join("trusted-status"),
            b"current-trusted-status".as_slice(),
        ),
        (
            replacement_runtime.join("runtime-control"),
            b"current-runtime-control".as_slice(),
        ),
        (
            replacement_runtime.join("network-state"),
            b"current-network-state".as_slice(),
        ),
    ];
    for (path, bytes) in &current_controls {
        fs::write(path, bytes).expect("install current control state on fresh compute");
    }

    restore_checkpoint_archive(archive, &replacement_mutable, ArchiveLimits::default())
        .await
        .expect("restore mutable bytes beside current fresh-compute controls");
    assert_eq!(
        fs::read(replacement_mutable.join("durable.txt")).expect("read restored tenant marker"),
        b"tenant durable bytes"
    );
    for (path, expected) in current_controls {
        assert_eq!(
            fs::read(&path).expect("read fresh control after mutable restore"),
            expected,
            "mutable restore must not replace current control {}",
            path.display()
        );
    }
}

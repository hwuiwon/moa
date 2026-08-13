//! Dedicated database-role boundary and recovery for process-owned workspace maintenance.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use metrics_exporter_prometheus::PrometheusBuilder;
use moa_core::{
    error::Result,
    types::{
        action_policy::CallOrigin,
        hands::{
            BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, HandHandle, LifetimeLimit,
            MemoryLimit, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
        },
        identifiers::{
            ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId, WorkspaceCheckpointId,
            WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, ProviderStorageKind, ProviderStorageRef, SandboxWorkspaceScope,
            SandboxWorkspaceState, WorkspaceCapacityDimension, WorkspaceCheckpointPublication,
            WorkspaceCheckpointState, WorkspaceConfirmedDisposition, WorkspaceOperationKind,
            WorkspaceOperationOutcome, WorkspacePostCommitState, WorkspaceRevisionRef,
        },
    },
};
use moa_db::ScopedConn;
use moa_hands::core::{
    leases::{
        HandLease, HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStatus, HandLeaseStore,
        HandLeaseWorkspaceAttachment, LeaseHandle, PostgresHandLeaseStore,
    },
    sandbox_workspace::{
        capacity::{
            CapacityQuantity, CapacityReservationRequest, PostgresWorkspaceCapacityRepository,
        },
        checkpoint::model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
        maintenance::WorkspaceMaintenanceCoordinator,
        model::{
            ActivateHydratedWorkspaceRequest, CreateWorkspaceRequest, SandboxWorkspace,
            WorkspaceTransition, WorkspaceWriterClaim,
        },
        operations::{
            ClaimedWorkspaceOperation, PostgresWorkspaceOperationRepository,
            WorkspaceOperationIntent,
        },
        reaper::{WorkspaceInventoryObservation, WorkspaceReaper, WorkspaceReconciliationProbe},
        repository::PostgresWorkspaceRepository,
    },
};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};

use super::sandbox_workspace_retention_db::{
    create_workspace, maintenance_fixture, pools, seed_account as seed_workspace_account,
};
use super::seed_session;

fn required_url(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be configured for this DB test"))
}

async fn seed_account(pool: &PgPool, account_id: ProviderAccountId) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'local', $2, $3, '{}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("maintenance-{account_id}"))
    .bind(format!("maintenance-org-{account_id}"))
    .execute(pool)
    .await
    .expect("seed provider account");
}

fn lease_policy() -> HandLeasePolicy {
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        LifetimeLimit::Unbounded,
        LifetimeLimit::Unbounded,
    )
    .expect("test profile validates");
    let effective = moa_core::types::hands::resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("workspace-maintenance-deployment", profile)
            .expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "workspace-maintenance-capabilities-v1",
    )
    .expect("test resolution succeeds");
    HandLeasePolicy::from_effective(&effective)
}

async fn active_workspace_and_lease(
    pool: &PgPool,
    tenant_id: TenantId,
    session_id: SessionId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
    worker_id: &str,
) -> (SandboxWorkspace, HandLease) {
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: SandboxWorkspaceScope::Worker {
                session_id,
                worker_id: worker_id.to_string(),
            },
            provider: "local".to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("create workspace intent");
    assert!(
        workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Creating,
                to: SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("transition workspace ready")
    );
    let restoring = workspaces
        .claim_writer(WorkspaceWriterClaim {
            tenant_id,
            workspace_id,
            expected_state: SandboxWorkspaceState::Ready,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
        })
        .await
        .expect("claim workspace writer")
        .expect("writer claim succeeds");
    let binding = restoring.binding().expect("restoring binding validates");
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let policy = lease_policy();
    let provisioning = leases
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: HandLeaseWorkspaceAttachment::new(
                workspace_id,
                restoring.writer_epoch,
                restoring.instance_generation,
                None,
            )
            .expect("workspace attachment validates"),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim hand provisioning")
        .expect("provisioning lease exists");
    assert!(
        workspaces
            .activate_hydrated(ActivateHydratedWorkspaceRequest {
                binding: &binding,
                lease: &provisioning,
                handle: LeaseHandle::new(
                    provisioning.provisioning_operation_id,
                    HandHandle::local(PathBuf::from(format!(
                        "/tmp/moa-workspace-maintenance-{workspace_id}"
                    ))),
                ),
            })
            .await
            .expect("activate hydrated workspace")
    );
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load active workspace")
        .expect("active workspace exists");
    let lease = leases
        .get(tenant_id, session_id, worker_id, "local")
        .await
        .expect("load active lease")
        .expect("active lease exists");
    (active, lease)
}

#[derive(Clone)]
struct CheckpointPublicationProbe {
    calls: Arc<AtomicUsize>,
    binding: moa_core::types::sandbox_workspace::WorkspaceBinding,
    publication: WorkspaceCheckpointPublication,
    lease: HandLease,
}

#[async_trait]
impl WorkspaceReconciliationProbe for CheckpointPublicationProbe {
    async fn observe(
        &self,
        _claimed: &ClaimedWorkspaceOperation,
    ) -> Result<WorkspaceInventoryObservation> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(WorkspaceInventoryObservation::CheckpointPublication {
            inventory_digest: format!("sha256:{}", self.publication.revision.checkpoint_id),
            binding: Box::new(self.binding.clone()),
            publication: Box::new(self.publication.clone()),
            post_commit_state: WorkspacePostCommitState::AttachmentRetained,
            lease: Box::new(self.lease.clone()),
        })
    }
}

#[tokio::test]
#[ignore = "requires distinct runtime and workspace-maintenance Postgres logins"]
async fn workspace_maintenance_pool_requires_noninheriting_member_login_db() {
    // Pins: ordinary request connections cannot activate maintenance-only
    // SECURITY DEFINER functions, while the dedicated generated login can
    // explicitly assume the exact NOLOGIN role.
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect(&required_url("MOA_DATABASE_URL"))
        .await
        .expect("connect ordinary runtime database login");
    let maintenance = PgPoolOptions::new()
        .max_connections(1)
        .connect(&required_url("MOA_DATABASE_MAINTENANCE_URL"))
        .await
        .expect("connect dedicated workspace-maintenance login");

    assert!(
        WorkspaceMaintenanceCoordinator::verify_maintenance_pool(&runtime)
            .await
            .is_err(),
        "ordinary runtime pool must not pass the maintenance role boundary"
    );
    WorkspaceMaintenanceCoordinator::verify_maintenance_pool(&maintenance)
        .await
        .expect("dedicated NOINHERIT member login should pass exact role activation");

    runtime.close().await;
    maintenance.close().await;
}

fn quota_ratio(rendered: &str, dimension: &str) -> f64 {
    let prefix =
        format!("moa_sandbox_workspace_quota_utilization_ratio{{dimension=\"{dimension}\"}} ");
    rendered
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("quota scrape is missing {dimension}:\n{rendered}"))
        .parse::<f64>()
        .unwrap_or_else(|error| panic!("quota ratio for {dimension} is not numeric: {error}"))
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a fresh V60 database and distinct runtime/workspace-maintenance logins"]
async fn workspace_quota_metrics_use_json_limits_and_highest_enforced_scope_db() {
    // Pins: fleet quota telemetry reads the JSONB limit schema used by admission,
    // reports the highest tenant/provider-account pressure, distinguishes an
    // explicit zero ceiling from an absent unbounded ceiling, and zero-fills it.
    let (runtime, maintenance) = pools().await;
    let tenant_id = TenantId::new();
    let account_id = ProviderAccountId::new();
    seed_workspace_account(&runtime, account_id).await;
    sqlx::query(
        "UPDATE moa.sandbox_provider_accounts \
         SET configured_limits = '{\"workspaces\": 10}'::jsonb \
         WHERE provider_account_id = $1 AND generation = 1",
    )
    .bind(account_id)
    .execute(&runtime)
    .await
    .expect("seed provider-account workspace ceiling");
    sqlx::query(
        "INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits) \
         VALUES ($1, '{\"workspaces\": 4}'::jsonb)",
    )
    .bind(tenant_id)
    .execute(&runtime)
    .await
    .expect("seed tenant workspace ceiling");
    for _ in 0..3 {
        create_workspace(&runtime, tenant_id, account_id).await;
    }

    let fixture = maintenance_fixture(
        &runtime,
        &maintenance,
        account_id,
        moa_config::CheckpointRetentionConfig::default(),
    )
    .await;
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);

    fixture
        .coordinator
        .emit_fleet_metrics()
        .await
        .expect("emit tenant-dominated quota snapshot");
    assert_eq!(quota_ratio(&handle.render(), "workspaces"), 0.75);

    sqlx::query(
        "UPDATE moa.sandbox_tenant_capacity_limits \
         SET configured_limits = '{\"workspaces\": 30}'::jsonb \
         WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .execute(&runtime)
    .await
    .expect("lower tenant pressure below provider-account pressure");
    fixture
        .coordinator
        .emit_fleet_metrics()
        .await
        .expect("emit provider-account-dominated quota snapshot");
    assert_eq!(quota_ratio(&handle.render(), "workspaces"), 0.3);

    sqlx::query(
        "UPDATE moa.sandbox_provider_accounts \
         SET configured_limits = '{\"workspaces\": 0}'::jsonb \
         WHERE provider_account_id = $1 AND generation = 1",
    )
    .bind(account_id)
    .execute(&runtime)
    .await
    .expect("set explicit zero provider-account ceiling");
    fixture
        .coordinator
        .emit_fleet_metrics()
        .await
        .expect("emit over-zero-ceiling quota snapshot");
    assert_eq!(quota_ratio(&handle.render(), "workspaces"), 1.0);

    sqlx::query(
        "UPDATE moa.sandbox_provider_accounts \
         SET configured_limits = '{}'::jsonb \
         WHERE provider_account_id = $1 AND generation = 1",
    )
    .bind(account_id)
    .execute(&runtime)
    .await
    .expect("remove provider-account ceiling");
    sqlx::query("DELETE FROM moa.sandbox_tenant_capacity_limits WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&runtime)
        .await
        .expect("remove tenant ceiling");
    fixture
        .coordinator
        .emit_fleet_metrics()
        .await
        .expect("emit unbounded quota snapshot");
    assert_eq!(quota_ratio(&handle.render(), "workspaces"), 0.0);

    sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&runtime)
        .await
        .expect("clean isolated capacity reservations");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&runtime)
        .await
        .expect("clean isolated workspaces");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&runtime)
        .await
        .expect("clean isolated provider account");
    runtime.close().await;
    maintenance.close().await;
}

#[tokio::test]
#[ignore = "requires distinct runtime and workspace-maintenance Postgres logins"]
async fn delayed_checkpoint_reconciliation_atomically_publishes_without_resend_db() {
    // Pins: after the foreground commit becomes ambiguous and its first reaper
    // dies, a later reaper may consume complete provider evidence exactly once,
    // but cannot resend the mutation or publish under an expired claim.
    let runtime = PgPoolOptions::new()
        .max_connections(5)
        .connect(&required_url("MOA_DATABASE_URL"))
        .await
        .expect("connect ordinary runtime database login");
    let maintenance = PgPoolOptions::new()
        .max_connections(3)
        .connect(&required_url("MOA_DATABASE_MAINTENANCE_URL"))
        .await
        .expect("connect dedicated workspace-maintenance login");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("maintenance-{workspace_id}");
    seed_session(&runtime, session_id, tenant_id).await;
    seed_account(&runtime, account_id).await;
    let (active, lease) = active_workspace_and_lease(
        &runtime,
        tenant_id,
        session_id,
        workspace_id,
        account_id,
        &worker_id,
    )
    .await;
    let binding = active
        .binding()
        .expect("active workspace binding validates");
    let workspaces = PostgresWorkspaceRepository::new(runtime.clone());
    for (from, to) in [
        (
            SandboxWorkspaceState::Active,
            SandboxWorkspaceState::Quiescing,
        ),
        (
            SandboxWorkspaceState::Quiescing,
            SandboxWorkspaceState::Committing,
        ),
    ] {
        assert!(
            workspaces
                .transition(WorkspaceTransition {
                    tenant_id,
                    workspace_id,
                    from,
                    to,
                    writer_epoch: active.writer_epoch,
                    instance_generation: active.instance_generation,
                })
                .await
                .expect("enter commit barrier")
        );
    }

    let operation_id = WorkspaceOperationId::new();
    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let now = Utc::now();
    let operations = PostgresWorkspaceOperationRepository::new(runtime.clone());
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Commit,
            request_hash: format!("sha256:{operation_id}"),
            expected_writer_epoch: active.writer_epoch,
            expected_instance_generation: active.instance_generation,
            expected_checkpoint_generation: active.checkpoint_generation,
            deadline_at: now - ChronoDuration::days(36_501),
            // This test shares the fleet-wide maintenance claim queue with
            // parallel DB tests. Put its exact operation well ahead of normal
            // fixture deadlines so `LIMIT 1` cannot select a sibling row.
            reconcile_not_before: now - ChronoDuration::days(36_500),
        })
        .await
        .expect("persist delayed commit intent");
    workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: active.writer_epoch,
            expected_instance_generation: active.instance_generation,
            expected_checkpoint_generation: active.checkpoint_generation,
        })
        .await
        .expect("create deterministic checkpoint intent")
        .expect("checkpoint intent passes exact fences");
    PostgresWorkspaceCapacityRepository::new(runtime.clone())
        .reserve(&CapacityReservationRequest {
            tenant_id,
            workspace_id,
            operation_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            expected_writer_epoch: active.writer_epoch,
            expected_instance_generation: active.instance_generation,
            quantities: vec![CapacityQuantity {
                dimension: WorkspaceCapacityDimension::Checkpoints,
                quantity: 1,
            }],
        })
        .await
        .expect("reserve checkpoint capacity");
    assert!(
        operations
            .begin_provider_attempt(tenant_id, operation_id)
            .await
            .expect("fence provider attempt unknown")
    );
    assert!(
        operations
            .mark_unknown(tenant_id, operation_id)
            .await
            .expect("move operation and reservation to reconciliation")
    );

    let publication = WorkspaceCheckpointPublication {
        revision: WorkspaceRevisionRef {
            checkpoint_id,
            generation: 1,
            format_version: 1,
        },
        storage: ProviderStorageRef {
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: format!("checkpoint/{checkpoint_id}"),
            workspace_locator: None,
        },
        manifest_digest: format!("sha256:manifest-{checkpoint_id}"),
        logical_bytes: 17,
    };
    let maintenance_operations = Arc::new(PostgresWorkspaceOperationRepository::new_maintenance(
        maintenance.clone(),
    ));
    let stale_claim = maintenance_operations
        .claim_reconciliation(1, Duration::from_secs(60))
        .await
        .expect("first reaper claims ambiguous commit")
        .pop()
        .expect("ambiguous commit is claimable");
    assert_eq!(
        stale_claim.operation.operation_id, operation_id,
        "the fleet-wide claim must select this test's isolated operation"
    );
    let mut maintenance_tx = ScopedConn::begin_control_plane(&maintenance)
        .await
        .expect("begin maintenance expiry");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(maintenance_tx.as_mut())
        .await
        .expect("activate maintenance role");
    let expired = sqlx::query(
        "UPDATE moa.sandbox_workspace_operations SET claim_expires_at = now() - interval '1 second' \
         WHERE tenant_id = $1 AND operation_id = $2 AND claim_token = $3",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .bind(stale_claim.claim_token)
    .execute(maintenance_tx.as_mut())
    .await
    .expect("expire crashed reaper claim");
    assert_eq!(
        expired.rows_affected(),
        1,
        "expire the exact claimed operation"
    );
    maintenance_tx.commit().await.expect("commit claim expiry");

    let maintenance_workspaces = Arc::new(PostgresWorkspaceRepository::new_maintenance(
        maintenance.clone(),
    ));
    assert!(
        !maintenance_workspaces
            .publish_checkpoint_commit_claimed(
                PublishCheckpointCommitRequest {
                    binding: &binding,
                    operation_id,
                    publication: &publication,
                    post_commit_state: WorkspacePostCommitState::AttachmentRetained,
                    lease: &lease,
                },
                &stale_claim,
            )
            .await
            .expect("expired claim is a fenced miss")
    );
    let before = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after stale publish")
        .expect("workspace still exists");
    assert_eq!(before.state, SandboxWorkspaceState::Reconciling);
    assert_eq!(
        (before.checkpoint_generation, before.checkpoint_id),
        (0, None)
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let reaper = WorkspaceReaper::new(
        Arc::clone(&maintenance_operations),
        Arc::clone(&maintenance_workspaces),
        Arc::new(CheckpointPublicationProbe {
            calls: Arc::clone(&calls),
            binding: binding.clone(),
            publication: publication.clone(),
            lease: lease.clone(),
        }),
        Duration::from_secs(60),
        1,
    )
    .expect("construct bounded workspace reaper");
    let pass = reaper
        .run_once(1)
        .await
        .expect("reclaim and publish checkpoint");
    assert_eq!((pass.claimed, pass.confirmed_present), (1, 1));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "recovery reconciles once and never resends commit"
    );

    let recovered = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load recovered workspace")
        .expect("recovered workspace exists");
    assert_eq!(recovered.state, SandboxWorkspaceState::Active);
    assert_eq!(
        (recovered.checkpoint_generation, recovered.checkpoint_id),
        (1, Some(checkpoint_id))
    );
    let checkpoint = workspaces
        .get_checkpoint_for_operation(tenant_id, workspace_id, operation_id)
        .await
        .expect("load recovered checkpoint")
        .expect("checkpoint exists");
    assert_eq!(checkpoint.state, WorkspaceCheckpointState::Available);
    assert_eq!(
        checkpoint.object_reference,
        Some(publication.storage.resource_id.clone())
    );
    let recovered_operation = operations
        .get(tenant_id, operation_id)
        .await
        .expect("load recovered operation")
        .expect("operation exists");
    assert_eq!(
        recovered_operation.outcome,
        WorkspaceOperationOutcome::Confirmed
    );
    assert_eq!(
        recovered_operation.confirmed_disposition,
        Some(WorkspaceConfirmedDisposition::ResourcePresent)
    );
    assert_eq!(
        (
            recovered_operation.claim_token,
            recovered_operation.claim_expires_at
        ),
        (None, None)
    );
    let recovered_lease = PostgresHandLeaseStore::new(runtime.clone())
        .get(tenant_id, session_id, &worker_id, "local")
        .await
        .expect("load recovered lease")
        .expect("recovered lease exists");
    assert_eq!(recovered_lease.status, HandLeaseStatus::Active);
    assert_eq!(
        recovered_lease
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.restored_checkpoint_id),
        Some(checkpoint_id)
    );
    let mut reservation_tx = ScopedConn::begin_control_plane(&maintenance)
        .await
        .expect("begin reservation check");
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(reservation_tx.as_mut())
        .await
        .expect("activate maintenance role");
    let reservation_state: String = sqlx::query(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations \
         WHERE tenant_id = $1 AND operation_id = $2 AND resource_dimension = 'checkpoints'",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(reservation_tx.as_mut())
    .await
    .expect("load recovered reservation")
    .try_get("reservation_state")
    .expect("decode reservation state");
    reservation_tx
        .commit()
        .await
        .expect("commit reservation check");
    assert_eq!(reservation_state, "committed");

    runtime.close().await;
    maintenance.close().await;
}

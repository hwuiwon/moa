//! Durable workspace lifecycle, fencing, and reconciliation against Postgres.

use std::{path::PathBuf, time::Duration};

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::error::MoaError;
use moa_core::types::{
    action_policy::CallOrigin,
    hands::{
        BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, HandHandle, LifetimeLimit,
        MemoryLimit, SandboxPolicySnapshot, SandboxProfile, SandboxTier,
    },
    identifiers::{
        ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
        SessionId, TenantId, WorkspaceCheckpointId, WorkspaceOperationId,
    },
    sandbox_workspace::{
        DurabilityClass, ProviderStorageKind, ProviderStorageRef, SandboxWorkspaceScope,
        SandboxWorkspaceState, WorkspaceBinding, WorkspaceCheckpointPublication,
        WorkspaceOperationKind, WorkspacePostCommitState, WorkspaceRevisionRef,
    },
};
use moa_hands::core::{
    leases::{
        HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStatus, HandLeaseStore,
        HandLeaseWorkspaceAttachment, LeaseHandle, PostgresHandLeaseStore,
    },
    sandbox_workspace::{
        checkpoint::model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
        model::{
            ActivateHydratedWorkspaceRequest, CreateWorkspaceRequest, SandboxWorkspace,
            WorkspaceTransition, WorkspaceWriterClaim,
        },
        operations::{
            AbsenceObservation, PostgresWorkspaceOperationRepository, WorkspaceOperationIntent,
        },
        repository::PostgresWorkspaceRepository,
    },
};
use sqlx::postgres::PgPoolOptions;

use super::{database_url, seed_session};

async fn seed_account(pool: &sqlx::PgPool, account_id: ProviderAccountId) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, 'local', $2, $3, '{}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(format!("lifecycle-{account_id}"))
    .bind(format!("org-{account_id}"))
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
        &SandboxPolicySnapshot::new("workspace-lifecycle-deployment", profile)
            .expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "workspace-lifecycle-capabilities-v1",
    )
    .expect("test resolution succeeds");
    HandLeasePolicy::from_effective(&effective)
}

fn binding(workspace: &SandboxWorkspace) -> WorkspaceBinding {
    assert_eq!(
        (workspace.checkpoint_generation, workspace.checkpoint_id),
        (0, None),
        "the initial hydration must restore generation zero"
    );
    WorkspaceBinding {
        tenant_id: workspace.tenant_id,
        scope: workspace.scope.clone(),
        workspace_id: workspace.workspace_id,
        provider_account_id: workspace.provider_account_id,
        provider_account_generation: u64::try_from(workspace.provider_account_generation)
            .expect("positive provider account generation"),
        durability_class: workspace.durability_class,
        writer_epoch: u64::try_from(workspace.writer_epoch).expect("nonnegative writer epoch"),
        instance_generation: u64::try_from(workspace.instance_generation)
            .expect("nonnegative instance generation"),
        current_revision: None,
    }
}

async fn activate_hydrated_workspace(
    pool: &sqlx::PgPool,
    session_id: SessionId,
    worker_id: &str,
    workspace: &SandboxWorkspace,
) {
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let attachment = HandLeaseWorkspaceAttachment::new(
        workspace.workspace_id,
        workspace.writer_epoch,
        workspace.instance_generation,
        None,
    )
    .expect("claimed workspace attachment validates");
    let policy = lease_policy();
    let provisioning = leases
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id,
            tenant_id: workspace.tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment,
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim provisioning lease")
        .expect("provisioning lease exists");
    let lease_handle = LeaseHandle::new(
        provisioning.provisioning_operation_id,
        HandHandle::local(PathBuf::from(format!(
            "/tmp/moa-workspace-lifecycle-{}",
            workspace.workspace_id
        ))),
    );
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    assert!(
        workspaces
            .activate_hydrated(ActivateHydratedWorkspaceRequest {
                binding: &binding(workspace),
                lease: &provisioning,
                handle: lease_handle,
            })
            .await
            .expect("atomically activate exact hydrated lease and workspace")
    );
    let active_lease = leases
        .get(workspace.tenant_id, session_id, worker_id, "local")
        .await
        .expect("load activated hand lease")
        .expect("activated hand lease exists");
    assert_eq!(active_lease.status, HandLeaseStatus::Active);
    assert_eq!(active_lease.attachment, provisioning.attachment);
}

fn create_request(
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
) -> CreateWorkspaceRequest {
    CreateWorkspaceRequest {
        workspace_id,
        tenant_id,
        scope: SandboxWorkspaceScope::ExecutionTask {
            run_id: ExecutionRunScopeId::new(),
            task_id: ExecutionTaskScopeId::new(),
        },
        provider: "local".to_string(),
        provider_account_id: account_id,
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        retention_deadline_at: None,
    }
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn workspace_writer_and_reconciliation_callbacks_are_generation_fenced_db() {
    // Pins: competing writers have one CAS winner, and changed inventory cannot
    // satisfy the two-separated-empty absence proof.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("writer-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    workspaces
        .create(&create_request(tenant_id, workspace_id, account_id))
        .await
        .expect("persist workspace before provider I/O");
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
            .expect("ready transition")
    );

    let claim = WorkspaceWriterClaim {
        tenant_id,
        workspace_id,
        expected_state: SandboxWorkspaceState::Ready,
        expected_writer_epoch: 0,
        expected_instance_generation: 0,
    };
    let (left, right) = tokio::join!(
        workspaces.claim_writer(claim),
        workspaces.claim_writer(claim)
    );
    let claims = [
        left.expect("left writer claim"),
        right.expect("right writer claim"),
    ];
    assert_eq!(claims.iter().filter(|claim| claim.is_some()).count(), 1);
    let restoring = claims
        .into_iter()
        .flatten()
        .next()
        .expect("exactly one writer owns the workspace");
    assert_eq!(restoring.state, SandboxWorkspaceState::Restoring);
    assert_eq!(
        (restoring.writer_epoch, restoring.instance_generation),
        (1, 1)
    );
    activate_hydrated_workspace(&pool, session_id, &worker_id, &restoring).await;
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after hydrated activation")
        .expect("activated workspace exists");
    assert_eq!(active.state, SandboxWorkspaceState::Active);
    assert_eq!((active.writer_epoch, active.instance_generation), (1, 1));

    assert!(
        !workspaces
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: SandboxWorkspaceState::Active,
                to: SandboxWorkspaceState::Quiescing,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await
            .expect("stale transition is a fenced miss")
    );

    let now = Utc::now();
    let operation_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Create,
            request_hash: "sha256:lifecycle-create".to_string(),
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
            deadline_at: now - ChronoDuration::seconds(3),
            reconcile_not_before: now - ChronoDuration::seconds(2),
        })
        .await
        .expect("intent is durable before provider observation");
    operations
        .mark_unknown(tenant_id, operation_id)
        .await
        .expect("ambiguous send enters reconciliation");
    let first_claim = operations
        .claim_reconciliation(1, Duration::from_secs(30))
        .await
        .expect("claim first observation")
        .pop()
        .expect("operation is claimable");
    assert_eq!(
        operations
            .record_inventory_observation(&first_claim, true, "digest-a", now)
            .await
            .expect("record first empty"),
        AbsenceObservation::First
    );
    operations
        .release_after_first_empty(&first_claim)
        .await
        .expect("release for separated observation");
    sqlx::query(
        "UPDATE moa.sandbox_workspace_operations SET retry_not_before = now() - interval '1 second' WHERE operation_id = $1",
    )
    .bind(operation_id)
    .execute(&pool)
    .await
    .expect("avoid wall-clock sleep in DB test");
    let second_claim = operations
        .claim_reconciliation(1, Duration::from_secs(30))
        .await
        .expect("claim changed observation")
        .pop()
        .expect("operation is claimable again");
    assert_eq!(
        operations
            .record_inventory_observation(
                &second_claim,
                true,
                "digest-b",
                now + ChronoDuration::seconds(2),
            )
            .await
            .expect("changed inventory resets proof"),
        AbsenceObservation::First
    );

    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean hand lease");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean session");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn checkpoint_metadata_is_created_before_bytes_and_remains_immutable_db() {
    // Pins: checkpoint bytes cannot exist as a published revision without an
    // earlier exact operation row, and the atomic production publication path
    // cannot rewrite verified payload metadata.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable");
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("checkpoint-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    workspaces
        .create(&create_request(tenant_id, workspace_id, account_id))
        .await
        .expect("create workspace intent");
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
        .expect("workspace ready");
    let restoring = workspaces
        .claim_writer(WorkspaceWriterClaim {
            tenant_id,
            workspace_id,
            expected_state: SandboxWorkspaceState::Ready,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
        })
        .await
        .expect("claim writer")
        .expect("writer claim succeeds");
    assert_eq!(restoring.state, SandboxWorkspaceState::Restoring);
    activate_hydrated_workspace(&pool, session_id, &worker_id, &restoring).await;
    let active = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace after hydrated activation")
        .expect("activated workspace exists");
    assert_eq!(active.state, SandboxWorkspaceState::Active);
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
                .expect("commit-barrier transition")
        );
    }

    let now = Utc::now();
    let operation_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&WorkspaceOperationIntent {
            operation_id,
            tenant_id,
            workspace_id,
            provider_account_id: account_id,
            provider_account_generation: 1,
            kind: WorkspaceOperationKind::Checkpoint,
            request_hash: "sha256:checkpoint-one".to_string(),
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
            deadline_at: now + ChronoDuration::seconds(10),
            reconcile_not_before: now + ChronoDuration::seconds(20),
        })
        .await
        .expect("persist checkpoint operation before byte I/O");
    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let creating = workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            expected_checkpoint_generation: 0,
        })
        .await
        .expect("create checkpoint metadata")
        .expect("exact operation fence creates row");
    assert_eq!(creating.generation, 1);
    assert_eq!(creating.state.as_str(), "creating");
    assert_eq!(
        (
            creating.object_reference,
            creating.manifest_digest,
            creating.logical_bytes,
            creating.verified_at,
        ),
        (None, None, None, None),
        "no byte-derived metadata exists before verification"
    );
    let conflicting_replay = workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id: WorkspaceCheckpointId::new(),
            tenant_id,
            workspace_id,
            parent_checkpoint_id: None,
            operation_id,
            expected_writer_epoch: 0,
            expected_instance_generation: 0,
            expected_checkpoint_generation: 0,
        })
        .await
        .expect_err("one operation identity cannot replay with different checkpoint fences");
    assert!(matches!(conflicting_replay, MoaError::ValidationError(_)));

    assert!(
        workspaces
            .create_checkpoint(CreateCheckpointRequest {
                checkpoint_id: WorkspaceCheckpointId::new(),
                tenant_id,
                workspace_id,
                parent_checkpoint_id: None,
                operation_id: WorkspaceOperationId::new(),
                expected_writer_epoch: 0,
                expected_instance_generation: 0,
                expected_checkpoint_generation: 0,
            })
            .await
            .expect("an unpersisted stale operation is a fenced miss")
            .is_none()
    );

    let active_lease = PostgresHandLeaseStore::new(pool.clone())
        .get(tenant_id, session_id, &worker_id, "local")
        .await
        .expect("load active hand lease")
        .expect("active hand lease exists");
    let active_binding = binding(&active);
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
            resource_id: "object://checkpoint-one".to_string(),
            workspace_locator: None,
        },
        manifest_digest: "sha256:manifest-one".to_string(),
        logical_bytes: 17,
    };
    assert!(
        workspaces
            .publish_workspace_checkpoint(PublishCheckpointCommitRequest {
                binding: &active_binding,
                operation_id,
                publication: &publication,
                post_commit_state: WorkspacePostCommitState::AttachmentRetained,
                lease: &active_lease,
            })
            .await
            .expect("atomically publish checkpoint, workspace head, operation, and lease")
    );
    let payload_rewrite = sqlx::query(
        "UPDATE moa.sandbox_workspace_checkpoints SET manifest_digest = 'sha256:tampered' WHERE checkpoint_id = $1",
    )
    .bind(checkpoint_id)
    .execute(&pool)
    .await;
    assert!(
        payload_rewrite.is_err(),
        "verified payload metadata is immutable"
    );

    sqlx::query(
        "UPDATE moa.sandbox_workspaces SET current_checkpoint_id = NULL, current_checkpoint_generation = 0 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(&pool)
    .await
    .expect("detach checkpoint head for fixture cleanup");

    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean hand lease");
    sqlx::query("DELETE FROM moa.sandbox_workspace_checkpoints WHERE checkpoint_id = $1")
        .bind(checkpoint_id)
        .execute(&pool)
        .await
        .expect("clean checkpoint");
    sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE operation_id = $1")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("clean operation");
    sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("clean workspace");
    sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(&pool)
        .await
        .expect("clean provider account");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("clean session");
    pool.close().await;
}

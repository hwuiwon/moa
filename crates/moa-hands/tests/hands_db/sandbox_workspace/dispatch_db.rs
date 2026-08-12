//! Atomic sandbox-workspace hydration, commit publication, and replay against Postgres.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::{MoaError, Result},
    traits::{HandProvider, Identity, IdentityType, SandboxStorageProvider},
    types::{
        action_policy::{ActionClass, ActionPolicyEffect, CallOrigin, RiskLevel},
        completion::ToolInvocation,
        hands::{
            BuiltinPolicyRevision, CpuLimit, DeadlineEnforcement, DiskLimit, EgressMode,
            EgressPolicy, HandHandle, HandProviderCapabilities, HandSpec, HandStatus,
            LifetimeLimit, MemoryLimit, ResourceSupport, SandboxPolicySnapshot, SandboxProfile,
            SandboxTier, SandboxTierCapabilities,
        },
        identifiers::{
            HandProvisioningOperationId, ModelId, ProviderAccountId, SandboxWorkspaceId, SessionId,
            TenantId, ToolCallId, WorkspaceCheckpointId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            DurabilityClass, ProviderAccountStorageInventory, ProviderStorageKind,
            ProviderStorageRef, SandboxWorkspaceScope, SandboxWorkspaceState,
            TenantStoragePurgeRequest, WorkspaceAttachRequest, WorkspaceBinding,
            WorkspaceCheckpointPublication, WorkspaceCheckpointPublishRequest,
            WorkspaceConfirmedDisposition, WorkspaceOperationKind, WorkspaceOperationOutcome,
            WorkspacePostCommitState, WorkspaceReconcileRequest, WorkspaceRestoreRequest,
            WorkspaceRevisionRef, WorkspaceStorageDeleteRequest, WorkspaceStorageOperation,
            WorkspaceStorageOperationResult, WorkspaceStoragePrepareRequest,
        },
        session::SessionMeta,
        tools::{IdempotencyClass, ToolDiffStrategy, ToolInputShape, ToolOutput, ToolPolicySpec},
    },
};
use moa_hands::{
    AuthorizedToolCall, HandRoute, JournaledWorkspaceCommit, ToolCallScope, ToolRegistry,
    ToolRouter,
    core::{
        leases::{
            HandLeasePolicy, HandLeaseProvisionRequest, HandLeaseStatus, HandLeaseStore,
            HandLeaseWorkspaceAttachment, LeaseHandle, PostgresHandLeaseStore,
        },
        sandbox_workspace::{
            capacity::PostgresWorkspaceCapacityRepository,
            checkpoint::model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
            model::{
                ActivateHydratedWorkspaceRequest, CreateWorkspaceRequest, SandboxWorkspace,
                WorkspaceTransition, WorkspaceWriterClaim,
            },
            operations::{PostgresWorkspaceOperationRepository, WorkspaceOperationIntent},
            repository::PostgresWorkspaceRepository,
        },
    },
    local_development_sandbox_policy,
};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tokio::sync::oneshot;

use super::{database_url, seed_session};

async fn pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable")
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
    .bind(format!("workspace-dispatch-{account_id}"))
    .bind(format!("workspace-dispatch-org-{account_id}"))
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
        &SandboxPolicySnapshot::new("workspace-dispatch-deployment", profile)
            .expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "workspace-dispatch-capabilities-v1",
    )
    .expect("test resolution succeeds");
    HandLeasePolicy::from_effective(&effective)
}

fn create_request(
    tenant_id: TenantId,
    session_id: SessionId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
    worker_id: &str,
) -> CreateWorkspaceRequest {
    CreateWorkspaceRequest {
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
    }
}

fn binding(workspace: &SandboxWorkspace) -> WorkspaceBinding {
    let current_revision = match (workspace.checkpoint_generation, workspace.checkpoint_id) {
        (0, None) => None,
        (generation, Some(checkpoint_id)) => Some(WorkspaceRevisionRef {
            checkpoint_id,
            generation: u64::try_from(generation).expect("positive checkpoint generation"),
            format_version: 1,
        }),
        other => panic!("invalid test workspace head: {other:?}"),
    };
    WorkspaceBinding {
        tenant_id: workspace.tenant_id,
        scope: workspace.scope.clone(),
        workspace_id: workspace.workspace_id,
        provider_account_id: workspace.provider_account_id,
        provider_account_generation: u64::try_from(workspace.provider_account_generation)
            .expect("positive account generation"),
        durability_class: workspace.durability_class,
        writer_epoch: u64::try_from(workspace.writer_epoch).expect("nonnegative writer epoch"),
        instance_generation: u64::try_from(workspace.instance_generation)
            .expect("nonnegative instance generation"),
        current_revision,
    }
}

fn operation_intent(
    tenant_id: TenantId,
    workspace: &SandboxWorkspace,
    operation_id: WorkspaceOperationId,
    kind: WorkspaceOperationKind,
    request_hash: &str,
) -> WorkspaceOperationIntent {
    let now = Utc::now();
    let deadline_at = now + ChronoDuration::minutes(1);
    WorkspaceOperationIntent {
        operation_id,
        tenant_id,
        workspace_id: workspace.workspace_id,
        provider_account_id: workspace.provider_account_id,
        provider_account_generation: workspace.provider_account_generation,
        kind,
        request_hash: request_hash.to_string(),
        expected_writer_epoch: workspace.writer_epoch,
        expected_instance_generation: workspace.instance_generation,
        expected_checkpoint_generation: workspace.checkpoint_generation,
        deadline_at,
        reconcile_not_before: deadline_at + ChronoDuration::seconds(30),
    }
}

fn publication(
    binding: &WorkspaceBinding,
    operation_id: WorkspaceOperationId,
    logical_bytes: u64,
) -> WorkspaceCheckpointPublication {
    let generation = binding
        .current_revision
        .as_ref()
        .map_or(1, |revision| revision.generation + 1);
    WorkspaceCheckpointPublication {
        revision: WorkspaceRevisionRef {
            checkpoint_id: WorkspaceCheckpointId(operation_id.0),
            generation,
            format_version: 1,
        },
        storage: ProviderStorageRef {
            provider_account_id: binding.provider_account_id,
            provider_account_generation: binding.provider_account_generation,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: format!("checkpoint/{operation_id}"),
            workspace_locator: None,
        },
        manifest_digest: format!("manifest-{operation_id}"),
        logical_bytes,
    }
}

async fn cleanup(
    pool: &PgPool,
    session_id: SessionId,
    workspace_id: SandboxWorkspaceId,
    account_id: ProviderAccountId,
) {
    let _ = sqlx::query(
        "UPDATE moa.sandbox_workspaces SET current_checkpoint_id = NULL, \
         current_checkpoint_generation = 0 WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_capacity_reservations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspace_checkpoints WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspace_grants WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(account_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn hydration_and_checkpoint_commits_are_atomic_replay_safe_and_generation_fenced_db() {
    // Pins: a hand cannot become routable before exact hydration, and a
    // mutating result cannot become durable unless checkpoint, head, operation,
    // and exact lease disposition commit in one generation-fenced transaction.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("worker-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    let leases = PostgresHandLeaseStore::new(pool.clone());
    let scope = SandboxWorkspaceScope::Worker {
        session_id,
        worker_id: worker_id.clone(),
    };
    workspaces
        .create(&create_request(
            tenant_id,
            session_id,
            workspace_id,
            account_id,
            &worker_id,
        ))
        .await
        .expect("create workspace before provider I/O");
    assert_eq!(
        workspaces
            .get_by_scope(tenant_id, &scope)
            .await
            .expect("resolve typed scope")
            .expect("workspace exists")
            .workspace_id,
        workspace_id,
        "typed-scope replay resolves one durable workspace"
    );
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
            .expect("workspace becomes ready")
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
        .expect("claim writer")
        .expect("writer claim wins");
    assert_eq!(restoring.state, SandboxWorkspaceState::Restoring);
    let initial_binding = binding(&restoring);
    let attachment = HandLeaseWorkspaceAttachment::new(workspace_id, 1, 1, None)
        .expect("initial attachment validates");
    let policy = lease_policy();
    let provisioning = leases
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: &worker_id,
            tenant_id,
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
        HandHandle::local(PathBuf::from(format!("/tmp/{workspace_id}"))),
    );
    assert!(
        workspaces
            .activate_hydrated(ActivateHydratedWorkspaceRequest {
                binding: &initial_binding,
                lease: &provisioning,
                handle: lease_handle,
            })
            .await
            .expect("activate exact hydrated lease")
    );
    let mut active_workspace = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace")
        .expect("workspace exists");
    assert_eq!(active_workspace.state, SandboxWorkspaceState::Active);
    let mut active_lease = leases
        .get(tenant_id, session_id, &worker_id, "local")
        .await
        .expect("load hand lease")
        .expect("hand lease exists");
    assert_eq!(active_lease.status, HandLeaseStatus::Active);

    for commit_index in 0..2_u64 {
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
                        writer_epoch: active_workspace.writer_epoch,
                        instance_generation: active_workspace.instance_generation,
                    })
                    .await
                    .expect("enter exact commit barrier")
            );
        }
        active_workspace = workspaces
            .get(tenant_id, workspace_id)
            .await
            .expect("load committing workspace")
            .expect("committing workspace exists");
        let commit_binding = binding(&active_workspace);
        let operation_id = WorkspaceOperationId::new();
        let intent = operation_intent(
            tenant_id,
            &active_workspace,
            operation_id,
            WorkspaceOperationKind::Commit,
            &format!("sha256:commit-{commit_index}"),
        );
        let persisted = operations
            .persist_intent(&intent)
            .await
            .expect("persist commit intent");
        assert_eq!(
            operations
                .persist_intent(&intent)
                .await
                .expect("exact intent replay succeeds"),
            persisted
        );
        let mut conflicting = intent.clone();
        conflicting.request_hash.push_str("-conflict");
        assert!(
            operations.persist_intent(&conflicting).await.is_err(),
            "same operation identity cannot acquire different meaning"
        );

        let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
        let parent_checkpoint_id = commit_binding
            .current_revision
            .as_ref()
            .map(|revision| revision.checkpoint_id);
        let create_checkpoint = CreateCheckpointRequest {
            checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id,
            operation_id,
            expected_writer_epoch: active_workspace.writer_epoch,
            expected_instance_generation: active_workspace.instance_generation,
            expected_checkpoint_generation: active_workspace.checkpoint_generation,
        };
        let creating = workspaces
            .create_checkpoint(create_checkpoint)
            .await
            .expect("create checkpoint row")
            .expect("checkpoint fence matches");
        assert_eq!(creating.parent_checkpoint_id, parent_checkpoint_id);
        assert_eq!(
            workspaces
                .create_checkpoint(create_checkpoint)
                .await
                .expect("checkpoint replay succeeds")
                .expect("checkpoint replay resolves row"),
            creating
        );
        assert_eq!(
            workspaces
                .get_checkpoint_for_operation(tenant_id, workspace_id, operation_id)
                .await
                .expect("lookup checkpoint by operation"),
            Some(creating.clone())
        );
        let publication = publication(&commit_binding, operation_id, 17 + commit_index);
        PostgresWorkspaceCapacityRepository::new(pool.clone())
            .reserve_checkpoint_publication(
                &WorkspaceStorageOperation {
                    operation_id,
                    kind: WorkspaceOperationKind::Commit,
                    binding: commit_binding.clone(),
                    deadline: intent.deadline_at,
                    request_hash: intent.request_hash.clone(),
                },
                publication.logical_bytes,
            )
            .await
            .expect("reserve checkpoint capacity before publication");

        if commit_index == 1 {
            let mut stale_lease = active_lease.clone();
            stale_lease.generation += 1;
            assert!(
                !workspaces
                    .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                        binding: &commit_binding,
                        operation_id,
                        publication: &publication,
                        post_commit_state: WorkspacePostCommitState::ComputeDestroyed,
                        lease: &stale_lease,
                    })
                    .await
                    .expect("stale lease is a fenced miss")
            );
            assert_eq!(
                workspaces
                    .get_checkpoint(tenant_id, workspace_id, checkpoint_id)
                    .await
                    .expect("load rolled-back checkpoint")
                    .expect("checkpoint remains")
                    .state
                    .as_str(),
                "creating",
                "checkpoint availability rolls back when the lease CAS loses"
            );
            assert_eq!(
                workspaces
                    .get(tenant_id, workspace_id)
                    .await
                    .expect("load head after stale callback")
                    .expect("workspace remains")
                    .checkpoint_generation,
                1,
                "stale callback cannot advance the prior head"
            );
        }

        let post_commit_state = if commit_index == 0 {
            WorkspacePostCommitState::AttachmentRetained
        } else {
            WorkspacePostCommitState::ComputeDestroyed
        };
        assert!(
            workspaces
                .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                    binding: &commit_binding,
                    operation_id,
                    publication: &publication,
                    post_commit_state,
                    lease: &active_lease,
                })
                .await
                .expect("publish checkpoint, head, operation, and lease atomically")
        );
        assert!(
            workspaces
                .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                    binding: &commit_binding,
                    operation_id,
                    publication: &publication,
                    post_commit_state,
                    lease: &active_lease,
                })
                .await
                .expect("exact publication replay succeeds")
        );
        active_workspace = workspaces
            .get(tenant_id, workspace_id)
            .await
            .expect("load published workspace")
            .expect("published workspace exists");
        assert_eq!(
            active_workspace.checkpoint_generation,
            i64::try_from(commit_index + 1).expect("small generation")
        );
        assert_eq!(active_workspace.checkpoint_id, Some(checkpoint_id));
        active_lease = leases
            .get(tenant_id, session_id, &worker_id, "local")
            .await
            .expect("load post-commit lease")
            .expect("post-commit lease exists");
        assert!(
            workspaces
                .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                    binding: &commit_binding,
                    operation_id,
                    publication: &publication,
                    post_commit_state,
                    lease: &active_lease,
                })
                .await
                .expect("restart replay proves the durable post-commit lease state")
        );
        if commit_index == 0 {
            assert_eq!(active_workspace.state, SandboxWorkspaceState::Active);
            assert_eq!(active_lease.status, HandLeaseStatus::Active);
            assert_eq!(
                active_lease
                    .attachment
                    .as_ref()
                    .and_then(|attachment| attachment.restored_checkpoint_id),
                Some(checkpoint_id)
            );

            // Pins: if another writer wins after immutable bytes are uploaded,
            // the losing checkpoint is failed and its exact count/bytes charge is
            // released so the upload can be garbage-collected without a quota leak.
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
                            writer_epoch: active_workspace.writer_epoch,
                            instance_generation: active_workspace.instance_generation,
                        })
                        .await
                        .expect("enter abandoned checkpoint barrier")
                );
            }
            let abandoned_workspace = workspaces
                .get(tenant_id, workspace_id)
                .await
                .expect("load abandoned workspace")
                .expect("abandoned workspace exists");
            let abandoned_binding = binding(&abandoned_workspace);
            let abandoned_operation_id = WorkspaceOperationId::new();
            let abandoned_intent = operation_intent(
                tenant_id,
                &abandoned_workspace,
                abandoned_operation_id,
                WorkspaceOperationKind::Commit,
                "sha256:abandoned-cas-loss",
            );
            operations
                .persist_intent(&abandoned_intent)
                .await
                .expect("persist abandoned operation");
            let abandoned_checkpoint_id = WorkspaceCheckpointId(abandoned_operation_id.0);
            workspaces
                .create_checkpoint(CreateCheckpointRequest {
                    checkpoint_id: abandoned_checkpoint_id,
                    tenant_id,
                    workspace_id,
                    parent_checkpoint_id: Some(checkpoint_id),
                    operation_id: abandoned_operation_id,
                    expected_writer_epoch: abandoned_workspace.writer_epoch,
                    expected_instance_generation: abandoned_workspace.instance_generation,
                    expected_checkpoint_generation: abandoned_workspace.checkpoint_generation,
                })
                .await
                .expect("create abandoned checkpoint")
                .expect("abandoned checkpoint fence matches");
            let abandoned_publication =
                self::publication(&abandoned_binding, abandoned_operation_id, 23);
            PostgresWorkspaceCapacityRepository::new(pool.clone())
                .reserve_checkpoint_publication(
                    &WorkspaceStorageOperation {
                        operation_id: abandoned_operation_id,
                        kind: WorkspaceOperationKind::Commit,
                        binding: abandoned_binding.clone(),
                        deadline: abandoned_intent.deadline_at,
                        request_hash: abandoned_intent.request_hash.clone(),
                    },
                    abandoned_publication.logical_bytes,
                )
                .await
                .expect("reserve abandoned checkpoint before upload");
            sqlx::query(
                "UPDATE moa.sandbox_workspaces SET lifecycle_state = 'active' WHERE workspace_id = $1",
            )
            .bind(workspace_id)
            .execute(&pool)
            .await
            .expect("simulate another writer winning the lifecycle CAS");
            assert!(
                !workspaces
                    .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                        binding: &abandoned_binding,
                        operation_id: abandoned_operation_id,
                        publication: &abandoned_publication,
                        post_commit_state: WorkspacePostCommitState::AttachmentRetained,
                        lease: &active_lease,
                    })
                    .await
                    .expect("CAS loss must be safely abandoned")
            );
            let abandoned_state = sqlx::query_as::<_, (String, String)>(
                r#"
                SELECT checkpoint.lifecycle_state, reservation.reservation_state
                FROM moa.sandbox_workspace_checkpoints AS checkpoint
                JOIN moa.sandbox_capacity_reservations AS reservation
                  ON reservation.operation_id = checkpoint.operation_id
                 AND reservation.resource_dimension = 'checkpoints'
                WHERE checkpoint.checkpoint_id = $1
                "#,
            )
            .bind(abandoned_checkpoint_id)
            .fetch_one(&pool)
            .await
            .expect("load abandoned checkpoint and capacity");
            assert_eq!(
                abandoned_state,
                ("failed".to_string(), "released".to_string())
            );
        } else {
            assert_eq!(active_workspace.state, SandboxWorkspaceState::Ready);
            assert_eq!(active_lease.status, HandLeaseStatus::Destroyed);
            assert_eq!(active_lease.attachment, None);
            assert_eq!(active_lease.handle, None);
        }
    }

    cleanup(&pool, session_id, workspace_id, account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn synchronous_absence_and_reconciled_absence_use_distinct_proof_rules_db() {
    // Pins: a provider's synchronous non-delete no-resource result can release
    // its reservation, while an ambiguous result and every delete require two
    // separated empty observations before absence can become durable.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("absence-{workspace_id}");
    seed_session(&pool, session_id, tenant_id).await;
    seed_account(&pool, account_id).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    let workspace = workspaces
        .create(&create_request(
            tenant_id,
            session_id,
            workspace_id,
            account_id,
            &worker_id,
        ))
        .await
        .expect("create workspace");

    let synchronous_id = WorkspaceOperationId::new();
    let synchronous = operation_intent(
        tenant_id,
        &workspace,
        synchronous_id,
        WorkspaceOperationKind::Create,
        "sha256:synchronous-absent",
    );
    operations
        .persist_intent(&synchronous)
        .await
        .expect("persist synchronous operation");
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_capacity_reservations (
            reservation_id, tenant_id, provider_account_id,
            provider_account_generation, workspace_id, operation_id,
            expected_writer_epoch, expected_instance_generation,
            resource_dimension, quantity
        ) VALUES (gen_random_uuid(), $1, $2, 1, $3, $4, 0, 0, 'workspaces', 1)
        "#,
    )
    .bind(tenant_id)
    .bind(account_id)
    .bind(workspace_id)
    .bind(synchronous_id)
    .execute(&pool)
    .await
    .expect("seed synchronous reservation");
    assert!(
        operations
            .confirm_disposition(
                tenant_id,
                synchronous_id,
                WorkspaceConfirmedDisposition::ResourceAbsent,
            )
            .await
            .expect("confirm synchronous absence")
    );
    let reservation_state: String = sqlx::query_scalar(
        "SELECT reservation_state FROM moa.sandbox_capacity_reservations WHERE operation_id = $1",
    )
    .bind(synchronous_id)
    .fetch_one(&pool)
    .await
    .expect("load released reservation");
    assert_eq!(reservation_state, "released");

    let ambiguous_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&operation_intent(
            tenant_id,
            &workspace,
            ambiguous_id,
            WorkspaceOperationKind::Create,
            "sha256:ambiguous-absent",
        ))
        .await
        .expect("persist ambiguous operation");
    operations
        .mark_unknown(tenant_id, ambiguous_id)
        .await
        .expect("mark operation ambiguous");
    assert!(
        sqlx::query(
            "UPDATE moa.sandbox_workspace_operations SET outcome_class = 'confirmed', \
             confirmed_disposition = 'resource_absent' WHERE operation_id = $1",
        )
        .bind(ambiguous_id)
        .execute(&pool)
        .await
        .is_err(),
        "ambiguous absence without inventory proof is rejected"
    );

    let delete_id = WorkspaceOperationId::new();
    operations
        .persist_intent(&operation_intent(
            tenant_id,
            &workspace,
            delete_id,
            WorkspaceOperationKind::Delete,
            "sha256:delete-absent",
        ))
        .await
        .expect("persist delete operation");
    assert!(
        sqlx::query(
            "UPDATE moa.sandbox_workspace_operations SET outcome_class = 'confirmed', \
             confirmed_disposition = 'resource_absent' WHERE operation_id = $1",
        )
        .bind(delete_id)
        .execute(&pool)
        .await
        .is_err(),
        "delete absence requires proof even from not_sent"
    );
    operations
        .mark_unknown(tenant_id, delete_id)
        .await
        .expect("mark delete ambiguous");
    let first = Utc::now();
    sqlx::query(
        r#"
        UPDATE moa.sandbox_workspace_operations
        SET absence_observation_count = 2, absence_first_observed_at = $2,
            absence_last_observed_at = $2 + interval '2 seconds',
            absence_inventory_digest = 'empty-inventory'
        WHERE operation_id = $1
        "#,
    )
    .bind(delete_id)
    .bind(first)
    .execute(&pool)
    .await
    .expect("seed a separated two-empty proof");
    sqlx::query(
        "UPDATE moa.sandbox_workspace_operations SET outcome_class = 'confirmed', \
         confirmed_disposition = 'resource_absent' WHERE operation_id = $1",
    )
    .bind(delete_id)
    .execute(&pool)
    .await
    .expect("two separated observations permit confirmed delete absence");
    let row = sqlx::query(
        "SELECT outcome_class, confirmed_disposition, absence_observation_count \
         FROM moa.sandbox_workspace_operations WHERE operation_id = $1",
    )
    .bind(delete_id)
    .fetch_one(&pool)
    .await
    .expect("load confirmed delete absence");
    assert_eq!(row.get::<String, _>("outcome_class"), "confirmed");
    assert_eq!(
        row.get::<String, _>("confirmed_disposition"),
        "resource_absent"
    );
    assert_eq!(row.get::<i32, _>("absence_observation_count"), 2);

    cleanup(&pool, session_id, workspace_id, account_id).await;
    pool.close().await;
}

const GATED_PROVIDER: &str = "workspace-commit-gate";

struct GatedWorkspaceProvider {
    commit_started: Mutex<Option<oneshot::Sender<WorkspaceCheckpointPublishRequest>>>,
    commit_release: Mutex<Option<oneshot::Receiver<()>>>,
    commit_calls: AtomicUsize,
    attach_calls: AtomicUsize,
    checkpoint_calls: AtomicUsize,
    restore_calls: AtomicUsize,
    reconcile_calls: AtomicUsize,
    checkpoint_post_commit_state: WorkspacePostCommitState,
}

impl GatedWorkspaceProvider {
    fn new() -> (
        Self,
        oneshot::Receiver<WorkspaceCheckpointPublishRequest>,
        oneshot::Sender<()>,
    ) {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        (
            Self {
                commit_started: Mutex::new(Some(started_tx)),
                commit_release: Mutex::new(Some(release_rx)),
                commit_calls: AtomicUsize::new(0),
                attach_calls: AtomicUsize::new(0),
                checkpoint_calls: AtomicUsize::new(0),
                restore_calls: AtomicUsize::new(0),
                reconcile_calls: AtomicUsize::new(0),
                checkpoint_post_commit_state: WorkspacePostCommitState::AttachmentRetained,
            },
            started_rx,
            release_tx,
        )
    }

    fn management() -> Self {
        let (started_tx, _started_rx) = oneshot::channel();
        let (_release_tx, release_rx) = oneshot::channel();
        Self {
            commit_started: Mutex::new(Some(started_tx)),
            commit_release: Mutex::new(Some(release_rx)),
            commit_calls: AtomicUsize::new(0),
            attach_calls: AtomicUsize::new(0),
            checkpoint_calls: AtomicUsize::new(0),
            restore_calls: AtomicUsize::new(0),
            reconcile_calls: AtomicUsize::new(0),
            checkpoint_post_commit_state: WorkspacePostCommitState::ComputeDestroyed,
        }
    }

    fn confirmed(storage: Option<ProviderStorageRef>) -> WorkspaceStorageOperationResult {
        WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage,
            checkpoint_publication: None,
            post_commit_state: None,
        }
    }

    fn mutable_storage(binding: &WorkspaceBinding) -> ProviderStorageRef {
        ProviderStorageRef {
            provider_account_id: binding.provider_account_id,
            provider_account_generation: binding.provider_account_generation,
            kind: ProviderStorageKind::MutableFilesystem,
            resource_id: format!("mutable/{}", binding.workspace_id),
            workspace_locator: None,
        }
    }

    fn committed_result(
        &self,
        operation: &WorkspaceStorageOperation,
    ) -> WorkspaceStorageOperationResult {
        let generation = operation
            .binding
            .current_revision
            .as_ref()
            .map_or(1, |parent| parent.generation + 1);
        let checkpoint_id = WorkspaceCheckpointId(operation.operation_id.0);
        let storage = ProviderStorageRef {
            provider_account_id: operation.binding.provider_account_id,
            provider_account_generation: operation.binding.provider_account_generation,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: format!("checkpoint/{checkpoint_id}"),
            workspace_locator: None,
        };
        WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(storage.clone()),
            checkpoint_publication: Some(WorkspaceCheckpointPublication {
                revision: WorkspaceRevisionRef {
                    checkpoint_id,
                    generation,
                    format_version: 1,
                },
                storage,
                manifest_digest: format!("sha256:manifest-{checkpoint_id}"),
                logical_bytes: 19,
            }),
            post_commit_state: Some(self.checkpoint_post_commit_state),
        }
    }
}

fn gated_hand_capabilities() -> HandProviderCapabilities {
    HandProviderCapabilities {
        revision: "workspace-commit-gate-hands-v1".to_string(),
        tiers: vec![SandboxTierCapabilities {
            tier: SandboxTier::Container,
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
        }],
    }
}

#[async_trait]
impl HandProvider for GatedWorkspaceProvider {
    fn provider_name(&self) -> &str {
        GATED_PROVIDER
    }

    fn capabilities(&self) -> HandProviderCapabilities {
        gated_hand_capabilities()
    }

    async fn provision(&self, spec: HandSpec) -> Result<HandHandle> {
        Ok(HandHandle::docker(format!(
            "workspace-commit-gate-{}",
            spec.provisioning_operation_id
        )))
    }

    async fn provisioned_hands(
        &self,
        _provider_account_id: ProviderAccountId,
        _provider_account_generation: u64,
        _operation_id: HandProvisioningOperationId,
    ) -> Result<Vec<HandHandle>> {
        Ok(Vec::new())
    }

    async fn execute(&self, _handle: &HandHandle, tool: &str, input: &str) -> Result<ToolOutput> {
        assert_eq!(tool, "mutate_workspace");
        assert_eq!(input, r#"{"value":"committed"}"#);
        Ok(ToolOutput::text(
            "mutation-complete",
            Duration::from_millis(7),
        ))
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
        Ok(())
    }
}

#[async_trait]
impl SandboxStorageProvider for GatedWorkspaceProvider {
    fn storage_provider_name(&self) -> &str {
        GATED_PROVIDER
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderAccountStorageInventory> {
        Ok(ProviderAccountStorageInventory {
            provider_account_id,
            provider_account_generation,
            observed_at: Utc::now(),
            resources: Vec::new(),
        })
    }

    async fn prepare_workspace_storage(
        &self,
        request: WorkspaceStoragePrepareRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Ok(Self::confirmed(Some(Self::mutable_storage(
            &request.operation.binding,
        ))))
    }

    async fn attach_workspace(
        &self,
        request: WorkspaceAttachRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        self.attach_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Self::confirmed(Some(Self::mutable_storage(
            &request.operation.binding,
        ))))
    }

    async fn publish_workspace_checkpoint(
        &self,
        request: WorkspaceCheckpointPublishRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        match request.operation.kind {
            WorkspaceOperationKind::Commit => {
                self.commit_calls.fetch_add(1, Ordering::SeqCst);
                self.commit_started
                    .lock()
                    .expect("commit gate mutex should not be poisoned")
                    .take()
                    .expect("the router must publish exactly one commit request")
                    .send(request.clone())
                    .map_err(|_| {
                        MoaError::StorageError("commit observer disappeared".to_string())
                    })?;
                let release = self
                    .commit_release
                    .lock()
                    .expect("commit release mutex should not be poisoned")
                    .take()
                    .expect("the router must await exactly one commit release");
                release.await.map_err(|_| {
                    MoaError::StorageError("commit release disappeared".to_string())
                })?;
            }
            WorkspaceOperationKind::Checkpoint => {
                self.checkpoint_calls.fetch_add(1, Ordering::SeqCst);
            }
            _ => {
                return Err(MoaError::ValidationError(
                    "checkpoint publication requires a commit or checkpoint operation".to_string(),
                ));
            }
        }
        Ok(self.committed_result(&request.operation))
    }

    async fn restore_workspace(
        &self,
        _request: WorkspaceRestoreRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        self.restore_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Self::confirmed(None))
    }

    async fn delete_workspace_storage(
        &self,
        _request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Err(MoaError::Unsupported(
            "delete is outside the commit-barrier scenario".to_string(),
        ))
    }

    async fn delete_tenant_storage_resource(
        &self,
        _request: TenantStoragePurgeRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        Err(MoaError::Unsupported(
            "tenant purge is outside the commit-barrier scenario".to_string(),
        ))
    }

    async fn reconcile_workspace_operation(
        &self,
        request: WorkspaceReconcileRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.committed_result(request.operation()))
    }

    async fn verify_workspace_storage(&self, _storage: &ProviderStorageRef) -> Result<bool> {
        Ok(true)
    }
}

async fn seed_named_account(pool: &PgPool, account_id: ProviderAccountId, provider: &str) {
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, configured_limits
        ) VALUES ($1, 1, $2, $3, $4, '{}'::jsonb)
        "#,
    )
    .bind(account_id)
    .bind(provider)
    .bind(format!("workspace-commit-gate-{account_id}"))
    .bind(format!("workspace-commit-gate-org-{account_id}"))
    .execute(pool)
    .await
    .expect("seed named provider account");
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn public_management_attach_checkpoint_and_exact_restore_are_durable_db() {
    // Pins: the public management seam performs real provider I/O, checkpoints
    // through the atomic head/lease barrier, replays without a second export,
    // and restores only the exact current committed checkpoint.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("management-{workspace_id}");
    let scope = SandboxWorkspaceScope::Worker {
        session_id,
        worker_id: worker_id.clone(),
    };
    seed_session(&pool, session_id, tenant_id).await;
    seed_named_account(&pool, account_id, GATED_PROVIDER).await;
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: scope.clone(),
            provider: GATED_PROVIDER.to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("create public-management workspace");

    let provider = Arc::new(GatedWorkspaceProvider::management());
    let mut registry = ToolRegistry::new();
    registry.register_hand(
        "management_route_anchor",
        "exposes the configured management provider route",
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        ToolPolicySpec {
            risk_level: RiskLevel::Low,
            default_effect: ActionPolicyEffect::Allow,
            action_class: ActionClass::Read,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        },
        IdempotencyClass::Idempotent,
    );
    registry.retarget_hand_tools(vec![HandRoute {
        provider: GATED_PROVIDER.to_string(),
        tier: SandboxTier::Container,
        policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
    }]);
    let mut hand_providers = HashMap::new();
    hand_providers.insert(
        GATED_PROVIDER.to_string(),
        Arc::clone(&provider) as Arc<dyn HandProvider>,
    );
    let router = ToolRouter::new(registry, hand_providers, local_development_sandbox_policy())
        .with_sandbox_storage_provider(Arc::clone(&provider) as Arc<dyn SandboxStorageProvider>)
        .expect("register management storage provider")
        .with_workspace_repositories(pool.clone())
        .with_hand_lease_store(Arc::new(PostgresHandLeaseStore::new(pool.clone())))
        .with_hand_lease_reaper();
    let session = SessionMeta {
        id: session_id,
        tenant_id,
        model: ModelId::new("workspace-management-model"),
        ..SessionMeta::default()
    };

    router
        .attach_managed_workspace(&session, &scope, workspace_id)
        .await
        .expect("attach must materialize provider compute and storage");
    assert_eq!(provider.attach_calls.load(Ordering::SeqCst), 1);
    let attached = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load attached workspace")
        .expect("attached workspace exists");
    assert_eq!(attached.state, SandboxWorkspaceState::Active);

    let operation_id = WorkspaceOperationId::new();
    router
        .checkpoint_managed_workspace(&session, &scope, workspace_id, operation_id)
        .await
        .expect("explicit checkpoint must cross the atomic publication barrier");
    assert_eq!(provider.checkpoint_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.commit_calls.load(Ordering::SeqCst), 0);
    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let checkpointed = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load checkpointed workspace")
        .expect("checkpointed workspace exists");
    assert_eq!(checkpointed.state, SandboxWorkspaceState::Ready);
    assert_eq!(checkpointed.checkpoint_generation, 1);
    assert_eq!(checkpointed.checkpoint_id, Some(checkpoint_id));
    let operation = PostgresWorkspaceOperationRepository::new(pool.clone())
        .get(tenant_id, operation_id)
        .await
        .expect("load explicit checkpoint operation")
        .expect("checkpoint operation is durable");
    assert_eq!(operation.kind, WorkspaceOperationKind::Checkpoint);
    assert_eq!(operation.outcome, WorkspaceOperationOutcome::Confirmed);

    router
        .checkpoint_managed_workspace(&session, &scope, workspace_id, operation_id)
        .await
        .expect("exact checkpoint replay must be idempotent");
    assert_eq!(
        provider.checkpoint_calls.load(Ordering::SeqCst),
        1,
        "confirmed replay must not export a second checkpoint"
    );

    router
        .restore_managed_workspace(&session, &scope, workspace_id, WorkspaceCheckpointId::new())
        .await
        .expect_err("restore must reject a checkpoint outside the exact current head");
    assert_eq!(provider.restore_calls.load(Ordering::SeqCst), 0);
    router
        .restore_managed_workspace(&session, &scope, workspace_id, checkpoint_id)
        .await
        .expect("exact current checkpoint must restore into fresh compute");
    assert_eq!(provider.restore_calls.load(Ordering::SeqCst), 1);
    let restored = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load restored workspace")
        .expect("restored workspace exists");
    assert_eq!(restored.state, SandboxWorkspaceState::Active);
    assert_eq!(restored.checkpoint_id, Some(checkpoint_id));
    assert_eq!(restored.checkpoint_generation, 1);

    cleanup(&pool, session_id, workspace_id, account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires a fresh V58 compose Postgres via MOA_DATABASE_URL"]
async fn may_write_result_waits_for_atomic_checkpoint_publication_db() {
    // Pins: the production router cannot return a successful MayWrite result
    // while provider checkpoint publication is blocked; only the atomic
    // checkpoint/head/operation/lease CAS releases the exact tool output.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    let worker_id = format!("commit-gate-{workspace_id}");
    let scope = SandboxWorkspaceScope::Worker {
        session_id,
        worker_id: worker_id.clone(),
    };
    seed_session(&pool, session_id, tenant_id).await;
    seed_named_account(&pool, account_id, GATED_PROVIDER).await;

    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(&CreateWorkspaceRequest {
            workspace_id,
            tenant_id,
            scope: scope.clone(),
            provider: GATED_PROVIDER.to_string(),
            provider_account_id: account_id,
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        })
        .await
        .expect("create typed workspace before router dispatch");

    let (provider, commit_started, commit_release) = GatedWorkspaceProvider::new();
    let provider = Arc::new(provider);
    let mut registry = ToolRegistry::new();
    registry.register_hand(
        "mutate_workspace",
        "mutate the durable workspace for commit-barrier coverage",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        ToolPolicySpec {
            risk_level: RiskLevel::Medium,
            default_effect: ActionPolicyEffect::Allow,
            action_class: ActionClass::LocalWrite,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        },
        IdempotencyClass::NonIdempotent,
    );
    registry.retarget_hand_tools(vec![HandRoute {
        provider: GATED_PROVIDER.to_string(),
        tier: SandboxTier::Container,
        policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
    }]);
    let mut hand_providers = HashMap::new();
    hand_providers.insert(
        GATED_PROVIDER.to_string(),
        Arc::clone(&provider) as Arc<dyn HandProvider>,
    );
    let router = ToolRouter::new(registry, hand_providers, local_development_sandbox_policy());
    let router = Arc::new(
        router
            .with_sandbox_storage_provider(Arc::clone(&provider) as Arc<dyn SandboxStorageProvider>)
            .expect("register matching storage provider")
            .with_workspace_repositories(pool.clone())
            .with_hand_lease_store(Arc::new(PostgresHandLeaseStore::new(pool.clone())))
            .with_hand_lease_reaper(),
    );

    let session = SessionMeta {
        id: session_id,
        tenant_id,
        model: ModelId::new("workspace-commit-gate-model"),
        ..SessionMeta::default()
    };
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: uuid::Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let invocation = ToolInvocation {
        id: None,
        name: "mutate_workspace".to_string(),
        input: serde_json::json!({ "value": "committed" }),
    };
    let tool_call_id = ToolCallId::new();
    let dispatch_router = Arc::clone(&router);
    let dispatch_session = session.clone();
    let dispatch_identity = identity.clone();
    let dispatch_scope = scope.clone();
    let dispatch_invocation = invocation.clone();
    let dispatch = tokio::spawn(async move {
        dispatch_router
            .execute_authorized(AuthorizedToolCall {
                session: &dispatch_session,
                caller_identity: &dispatch_identity,
                workspace_scope: Some(&dispatch_scope),
                invocation: &dispatch_invocation,
                tool_call_id,
                active_canary: None,
                catalog: None,
                scope: ToolCallScope::unbounded(),
            })
            .await
    });

    let commit_request = tokio::time::timeout(Duration::from_secs(5), commit_started)
        .await
        .expect("router should reach provider checkpoint publication")
        .expect("commit observer should receive the exact request");
    assert!(
        !dispatch.is_finished(),
        "successful ToolOutput must remain pending behind checkpoint publication"
    );
    let blocked_workspace = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load workspace while provider is blocked")
        .expect("workspace remains present");
    assert_eq!(blocked_workspace.state, SandboxWorkspaceState::Committing);
    assert_eq!(blocked_workspace.checkpoint_generation, 0);
    assert_eq!(blocked_workspace.checkpoint_id, None);
    let operation_id = commit_request.operation.operation_id;
    let blocked_operation = sqlx::query(
        "SELECT outcome_class, confirmed_disposition FROM moa.sandbox_workspace_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(&pool)
    .await
    .expect("load blocked commit intent");
    assert_eq!(
        blocked_operation.get::<String, _>("outcome_class"),
        "unknown",
        "the provider attempt is durably fenced before I/O without moving the workspace out of committing"
    );
    assert_eq!(
        blocked_operation.get::<Option<String>, _>("confirmed_disposition"),
        None
    );
    let blocked_checkpoint = workspaces
        .get_checkpoint(
            tenant_id,
            workspace_id,
            WorkspaceCheckpointId(operation_id.0),
        )
        .await
        .expect("load blocked checkpoint")
        .expect("checkpoint intent exists before provider publication");
    assert_eq!(blocked_checkpoint.state.as_str(), "creating");
    let blocked_lease = PostgresHandLeaseStore::new(pool.clone())
        .get(tenant_id, session_id, &worker_id, GATED_PROVIDER)
        .await
        .expect("load active lease while commit is blocked")
        .expect("active lease exists");
    assert_eq!(blocked_lease.status, HandLeaseStatus::Active);
    assert_eq!(
        blocked_lease
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.restored_checkpoint_id),
        None
    );

    commit_release
        .send(())
        .expect("release the provider publication gate");
    let secured = tokio::time::timeout(Duration::from_secs(5), dispatch)
        .await
        .expect("router should finish after publication release")
        .expect("dispatch task should not panic")
        .expect("atomic publication should return the tool result");
    assert_eq!(
        secured.safe_output,
        ToolOutput::text("mutation-complete", Duration::from_millis(7))
    );

    let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
    let committed_workspace = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load committed workspace")
        .expect("workspace remains present");
    assert_eq!(committed_workspace.state, SandboxWorkspaceState::Active);
    assert_eq!(committed_workspace.checkpoint_generation, 1);
    assert_eq!(committed_workspace.checkpoint_id, Some(checkpoint_id));
    let committed_checkpoint = workspaces
        .get_checkpoint(tenant_id, workspace_id, checkpoint_id)
        .await
        .expect("load committed checkpoint")
        .expect("published checkpoint exists");
    assert_eq!(committed_checkpoint.state.as_str(), "available");
    assert_eq!(committed_checkpoint.generation, 1);
    let committed_operation = sqlx::query(
        "SELECT outcome_class, confirmed_disposition FROM moa.sandbox_workspace_operations \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id)
    .bind(operation_id)
    .fetch_one(&pool)
    .await
    .expect("load committed operation");
    assert_eq!(
        committed_operation.get::<String, _>("outcome_class"),
        "confirmed"
    );
    assert_eq!(
        committed_operation.get::<String, _>("confirmed_disposition"),
        "resource_present"
    );
    let committed_lease = PostgresHandLeaseStore::new(pool.clone())
        .get(tenant_id, session_id, &worker_id, GATED_PROVIDER)
        .await
        .expect("load committed lease")
        .expect("lease remains active");
    assert_eq!(committed_lease.status, HandLeaseStatus::Active);
    assert_eq!(
        committed_lease
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.restored_checkpoint_id),
        Some(checkpoint_id)
    );

    // Pins: a crash after the provider artifact is ready but before the atomic
    // head CAS leaves an Unknown operation and Creating checkpoint. Replay must
    // reconcile that deterministic artifact without sending a second commit.
    assert_eq!(provider.commit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.reconcile_calls.load(Ordering::SeqCst), 0);
    let replay_binding = binding(&committed_workspace);
    let replay_tool_call_id = ToolCallId::new();
    let replay_operation_id = WorkspaceOperationId(uuid::Uuid::new_v5(
        &workspace_id.0,
        format!("tool-commit-v1:{replay_tool_call_id}").as_bytes(),
    ));
    let request_bytes = serde_json::to_vec(&(&replay_binding, replay_tool_call_id))
        .expect("serialize deterministic replay request");
    let replay_request_hash = format!("sha256:{}", hex::encode(Sha256::digest(request_bytes)));
    let replay_intent = operation_intent(
        tenant_id,
        &committed_workspace,
        replay_operation_id,
        WorkspaceOperationKind::Commit,
        &replay_request_hash,
    );
    let operations = PostgresWorkspaceOperationRepository::new(pool.clone());
    operations
        .persist_intent(&replay_intent)
        .await
        .expect("persist the replay-stable second commit intent");
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
                    writer_epoch: committed_workspace.writer_epoch,
                    instance_generation: committed_workspace.instance_generation,
                })
                .await
                .expect("enter the second exact commit barrier")
        );
    }
    let replay_checkpoint_id = WorkspaceCheckpointId(replay_operation_id.0);
    workspaces
        .create_checkpoint(CreateCheckpointRequest {
            checkpoint_id: replay_checkpoint_id,
            tenant_id,
            workspace_id,
            parent_checkpoint_id: Some(checkpoint_id),
            operation_id: replay_operation_id,
            expected_writer_epoch: committed_workspace.writer_epoch,
            expected_instance_generation: committed_workspace.instance_generation,
            expected_checkpoint_generation: committed_workspace.checkpoint_generation,
        })
        .await
        .expect("create the rolled-back checkpoint intent")
        .expect("second checkpoint fences match");
    assert!(
        operations
            .begin_provider_attempt(tenant_id, replay_operation_id)
            .await
            .expect("fence the externally started provider attempt")
    );

    router
        .commit_authorized_workspace_after_tool(JournaledWorkspaceCommit {
            session: &session,
            workspace_scope: &scope,
            tool_call_id: replay_tool_call_id,
            scope: ToolCallScope::unbounded(),
        })
        .await
        .expect("Unknown replay reconciles and atomically publishes the exact artifact");

    assert_eq!(
        provider.commit_calls.load(Ordering::SeqCst),
        1,
        "Unknown replay must never resend commit_workspace"
    );
    assert_eq!(
        provider.reconcile_calls.load(Ordering::SeqCst),
        1,
        "Unknown replay must inspect the exact provider artifact once"
    );
    let replayed_workspace = workspaces
        .get(tenant_id, workspace_id)
        .await
        .expect("load replayed workspace")
        .expect("replayed workspace remains present");
    assert_eq!(replayed_workspace.state, SandboxWorkspaceState::Active);
    assert_eq!(replayed_workspace.checkpoint_generation, 2);
    assert_eq!(replayed_workspace.checkpoint_id, Some(replay_checkpoint_id));
    let replayed_checkpoint = workspaces
        .get_checkpoint(tenant_id, workspace_id, replay_checkpoint_id)
        .await
        .expect("load reconciled checkpoint")
        .expect("reconciled checkpoint exists");
    assert_eq!(replayed_checkpoint.state.as_str(), "available");
    assert_eq!(replayed_checkpoint.generation, 2);
    let replayed_operation = operations
        .get(tenant_id, replay_operation_id)
        .await
        .expect("load reconciled operation")
        .expect("reconciled operation exists");
    assert_eq!(
        replayed_operation.outcome,
        WorkspaceOperationOutcome::Confirmed
    );
    assert_eq!(
        replayed_operation.confirmed_disposition,
        Some(WorkspaceConfirmedDisposition::ResourcePresent)
    );
    let replayed_lease = PostgresHandLeaseStore::new(pool.clone())
        .get(tenant_id, session_id, &worker_id, GATED_PROVIDER)
        .await
        .expect("load reconciled lease")
        .expect("reconciled lease remains active");
    assert_eq!(replayed_lease.status, HandLeaseStatus::Active);
    assert_eq!(
        replayed_lease
            .attachment
            .as_ref()
            .and_then(|attachment| attachment.restored_checkpoint_id),
        Some(replay_checkpoint_id)
    );

    cleanup(&pool, session_id, workspace_id, account_id).await;
    pool.close().await;
}

//! Managed durable sandbox-workspace lifecycle and commit barriers.

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        hands::HandHandle,
        identifiers::{
            ExecutionCompensationScopeId, SandboxWorkspaceId, ToolCallId, WorkspaceCheckpointId,
            WorkspaceOperationId,
        },
        sandbox_workspace::{
            ExecutionHandContinuationDisposition, ExecutionHandReleaseOwner,
            ExecutionHandReleaseReceipt, ProviderStorageKind, ProviderStorageRef,
            SandboxWorkspaceScope, SandboxWorkspaceState, WorkspaceAttachRequest, WorkspaceBinding,
            WorkspaceCheckpointPublishRequest, WorkspaceCheckpointState,
            WorkspaceConfirmedDisposition, WorkspaceOperationKind, WorkspaceOperationOutcome,
            WorkspacePostCommitState, WorkspaceReconcileRequest, WorkspaceRestoreRequest,
            WorkspaceStorageOperation, WorkspaceStoragePrepareRequest,
        },
        session::SessionMeta,
    },
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    checkpoint::{
        archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION,
        model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
    },
    failpoints,
    model::{
        AbsentTaskHandReleaseIntent, CompensationHandReleaseIntent, SandboxWorkspace,
        TaskHandReleaseIntent, WorkspaceTransition, WorkspaceWriterClaim,
    },
    operations::WorkspaceOperationIntent,
};
use moa_observability::{
    SandboxWorkspaceCheckpointOperation, SandboxWorkspaceLifecycleOperation,
    SandboxWorkspaceMetricResult,
};

use crate::core::{
    ActiveHand, ExecutionHandReleaseRequest, ExecutionHandRetentionRequest, HandProviderCacheKey,
    HandRoute, InstalledManifestMarker, JournaledWorkspaceCommit, ToolCallScope, ToolExecution,
    ToolRouter, TrustedSandboxManifest,
    leases::{HandLease, HandLeaseStatus, HandLeaseWorkspaceAttachment},
    lifecycle::{
        active_hand_capacity_request, manifest_scope_key, session_provider_key,
        workspace_binding_for_hand, workspace_lease_scope,
    },
    telemetry::{
        record_workspace_checkpoint, record_workspace_lifecycle, record_workspace_release,
        record_workspace_restore,
    },
};

#[derive(Clone, Copy)]
/// Internal identity and policy for one deterministic workspace commit.
pub(in crate::core) struct WorkspaceCommitExecution<'a> {
    /// Session owning the workspace.
    pub(in crate::core) session: &'a SessionMeta,
    /// Typed durable workspace owner.
    pub(in crate::core) workspace_scope: &'a SandboxWorkspaceScope,
    /// Deterministic tool or yield identity.
    pub(in crate::core) tool_call_id: ToolCallId,
    /// Pinned hand and storage provider.
    pub(in crate::core) provider_name: &'a str,
    /// Exact active compute handle.
    pub(in crate::core) hand: &'a HandHandle,
    /// Bounded execution scope.
    pub(in crate::core) call_scope: ToolCallScope<'a>,
    /// Whether verified publication must destroy compute.
    pub(in crate::core) release_compute: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutionReleaseStep {
    DurableReconciliation,
    ProviderIo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistedLeaseReleaseState {
    Missing,
    Destroyed,
    LiveOrAmbiguous,
}

const fn compensation_release_identity_is_verified(
    persisted_identity_present: bool,
    lease_state: PersistedLeaseReleaseState,
) -> bool {
    matches!(
        (persisted_identity_present, lease_state),
        (true, PersistedLeaseReleaseState::Destroyed)
            | (false, PersistedLeaseReleaseState::Missing)
    )
}

fn admit_execution_release_step(
    scope: ToolCallScope<'_>,
    step: ExecutionReleaseStep,
) -> Result<()> {
    match step {
        ExecutionReleaseStep::DurableReconciliation => Ok(()),
        ExecutionReleaseStep::ProviderIo => scope.admit(),
    }
}

impl ToolRouter {
    /// Materializes the exact authorized worker workspace on its pinned provider.
    pub async fn attach_managed_workspace(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<()> {
        let workspace = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        let route = self.management_route(&workspace.provider)?;
        self.get_or_provision_hand_within(
            &route,
            session,
            workspace_scope,
            ToolCallScope::unbounded(),
        )
        .await?;
        let active = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        if active.state != SandboxWorkspaceState::Active {
            return Err(MoaError::StorageError(
                "workspace attach completed without an active fenced writer".to_string(),
            ));
        }
        Ok(())
    }

    /// Publishes one replay-stable explicit checkpoint through the durable commit barrier.
    pub async fn checkpoint_managed_workspace(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        workspace_id: SandboxWorkspaceId,
        operation_id: WorkspaceOperationId,
    ) -> Result<()> {
        let mut workspace = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        if self
            .confirmed_management_checkpoint_replay(&workspace, operation_id)
            .await?
        {
            return Ok(());
        }
        let hand = if matches!(
            workspace.state,
            SandboxWorkspaceState::Quiescing | SandboxWorkspaceState::Committing
        ) {
            let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace operation repository missing".to_string())
            })?;
            let operation = operations
                .get(session.tenant_id, operation_id)
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                })?;
            if operation.kind != WorkspaceOperationKind::Checkpoint
                || operation.workspace_id != workspace_id
                || operation.expected_writer_epoch != workspace.writer_epoch
                || operation.expected_instance_generation != workspace.instance_generation
                || operation.expected_checkpoint_generation != workspace.checkpoint_generation
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
                MoaError::StorageError("durable hand lease store missing".to_string())
            })?;
            lease_store
                .get(
                    session.tenant_id,
                    session.id,
                    &workspace_lease_scope(workspace_scope),
                    &workspace.provider,
                )
                .await?
                .and_then(|lease| lease.handle.map(|handle| handle.handle))
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                })?
        } else {
            let route = self.management_route(&workspace.provider)?;
            self.get_or_provision_hand_within(
                &route,
                session,
                workspace_scope,
                ToolCallScope::unbounded(),
            )
            .await?
        };
        workspace = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        self.checkpoint_active_managed_workspace(
            session,
            workspace_scope,
            &workspace,
            operation_id,
            &hand,
        )
        .await
    }

    /// Restores the exact current committed checkpoint into fresh provider compute.
    ///
    /// Historical revisions remain immutable retention records. Public restore
    /// cannot silently move the monotonic workspace head backwards, so the
    /// requested checkpoint must be the exact current recovery authority.
    pub async fn restore_managed_workspace(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        workspace_id: SandboxWorkspaceId,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<()> {
        let workspace = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let checkpoint = repository
            .get_checkpoint(session.tenant_id, workspace_id, checkpoint_id)
            .await?
            .ok_or_else(|| {
                MoaError::ValidationError(
                    "restore checkpoint does not belong to the authorized workspace".to_string(),
                )
            })?;
        validate_managed_restore_target(
            workspace.checkpoint_id,
            workspace.checkpoint_generation,
            checkpoint_id,
            checkpoint.checkpoint_id,
            checkpoint.generation,
            checkpoint.state,
        )?;
        let route = self.management_route(&workspace.provider)?;
        if workspace.state == SandboxWorkspaceState::Active {
            self.reprovision_hand(session, workspace_scope, &route, ToolCallScope::unbounded())
                .await?;
        } else {
            self.get_or_provision_hand_within(
                &route,
                session,
                workspace_scope,
                ToolCallScope::unbounded(),
            )
            .await?;
        }
        let restored = self
            .managed_workspace(session, workspace_scope, workspace_id)
            .await?;
        if restored.state != SandboxWorkspaceState::Active
            || restored.checkpoint_id != Some(checkpoint_id)
            || restored.checkpoint_generation != checkpoint.generation
        {
            return Err(MoaError::StorageError(
                "workspace restore completed without the exact committed checkpoint".to_string(),
            ));
        }
        Ok(())
    }

    async fn managed_workspace(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspace> {
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let workspace = repository
            .get_by_scope(session.tenant_id, workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "sandbox workspace is not owned by the verified scope".to_string(),
                )
            })?;
        if workspace.workspace_id != workspace_id
            || workspace.tenant_id != session.tenant_id
            || workspace.scope != *workspace_scope
            || workspace.access_fenced_at.is_some()
        {
            return Err(MoaError::PermissionDenied(
                "sandbox workspace is not owned by the verified scope".to_string(),
            ));
        }
        Ok(workspace)
    }

    fn management_route(&self, provider: &str) -> Result<HandRoute> {
        self.catalog
            .activated()
            .capability_registrations()
            .into_iter()
            .find_map(|(_, execution)| match execution {
                ToolExecution::Hand { routes } => {
                    routes.into_iter().find(|route| route.provider == provider)
                }
                _ => None,
            })
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "workspace provider {provider} has no configured hand route"
                ))
            })
    }

    async fn confirmed_management_checkpoint_replay(
        &self,
        workspace: &SandboxWorkspace,
        operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let Some(operation) = operations.get(workspace.tenant_id, operation_id).await? else {
            return Ok(false);
        };
        if operation.outcome != WorkspaceOperationOutcome::Confirmed {
            return Ok(false);
        }
        let checkpoint = repository
            .get_checkpoint_for_operation(workspace.tenant_id, workspace.workspace_id, operation_id)
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            })?;
        let committed_generation = operation
            .expected_checkpoint_generation
            .checked_add(1)
            .ok_or_else(|| {
                MoaError::StorageError("workspace checkpoint generation overflowed".to_string())
            })?;
        let parent_revision = revision_from_checkpoint_parent(
            operation.expected_checkpoint_generation,
            checkpoint.parent_checkpoint_id,
        )?;
        let mut original_binding = workspace.binding()?;
        original_binding.current_revision = parent_revision;
        let request_hash = management_checkpoint_request_hash(&original_binding, operation_id)?;
        if operation.kind != WorkspaceOperationKind::Checkpoint
            || operation.workspace_id != workspace.workspace_id
            || operation.provider_account_id != workspace.provider_account_id
            || operation.provider_account_generation != workspace.provider_account_generation
            || operation.expected_writer_epoch != workspace.writer_epoch
            || operation.expected_instance_generation != workspace.instance_generation
            || operation.request_hash != request_hash
            || operation.confirmed_disposition
                != Some(WorkspaceConfirmedDisposition::ResourcePresent)
            || checkpoint.state != WorkspaceCheckpointState::Available
            || checkpoint.checkpoint_id != WorkspaceCheckpointId(operation_id.0)
            || checkpoint.generation != committed_generation
            || checkpoint.source_writer_epoch != workspace.writer_epoch
            || checkpoint.source_instance_generation != workspace.instance_generation
            || workspace.checkpoint_id != Some(checkpoint.checkpoint_id)
            || workspace.checkpoint_generation != committed_generation
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        Ok(true)
    }

    async fn checkpoint_active_managed_workspace(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        workspace: &SandboxWorkspace,
        operation_id: WorkspaceOperationId,
        hand: &HandHandle,
    ) -> Result<()> {
        if !matches!(
            workspace.state,
            SandboxWorkspaceState::Active
                | SandboxWorkspaceState::Quiescing
                | SandboxWorkspaceState::Committing
        ) {
            return Err(MoaError::StorageError(
                "workspace must be active before checkpoint publication".to_string(),
            ));
        }
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let storage_provider = self
            .hands
            .storage_providers
            .get(&workspace.provider)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "workspace storage provider {} is not registered",
                    workspace.provider
                ))
            })?;
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let binding = workspace.binding()?;
        let lease_scope = workspace_lease_scope(workspace_scope);
        let lease = lease_store
            .get(
                session.tenant_id,
                session.id,
                &lease_scope,
                &workspace.provider,
            )
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "active workspace lease is missing before checkpoint".to_string(),
                )
            })?;
        if lease.status != HandLeaseStatus::Active
            || lease.handle.as_ref().map(|lease| &lease.handle) != Some(hand)
            || lease.attachment != Some(lease_attachment(&binding)?)
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }

        let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
        let existing_operation = operations.get(binding.tenant_id, operation_id).await?;
        let deadline_at = existing_operation.as_ref().map_or_else(
            || Utc::now() + ChronoDuration::minutes(5),
            |operation| operation.deadline_at,
        );
        let request_hash = management_checkpoint_request_hash(&binding, operation_id)?;
        let expected_writer_epoch = i64::try_from(binding.writer_epoch)
            .map_err(|_| MoaError::StorageError("workspace writer epoch is invalid".to_string()))?;
        let expected_instance_generation =
            i64::try_from(binding.instance_generation).map_err(|_| {
                MoaError::StorageError("workspace instance generation is invalid".to_string())
            })?;
        let expected_checkpoint_generation =
            binding
                .current_revision
                .as_ref()
                .map_or(Ok(0_i64), |revision| {
                    i64::try_from(revision.generation).map_err(|_| {
                        MoaError::StorageError(
                            "workspace checkpoint generation is invalid".to_string(),
                        )
                    })
                })?;
        let operation = operations
            .persist_intent(&WorkspaceOperationIntent {
                operation_id,
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                provider_account_id: binding.provider_account_id,
                provider_account_generation: i64::try_from(binding.provider_account_generation)
                    .map_err(|_| {
                        MoaError::StorageError(
                            "workspace provider-account generation is invalid".to_string(),
                        )
                    })?,
                kind: WorkspaceOperationKind::Checkpoint,
                request_hash: request_hash.clone(),
                expected_writer_epoch,
                expected_instance_generation,
                expected_checkpoint_generation,
                deadline_at,
                reconcile_not_before: deadline_at + ChronoDuration::seconds(30),
            })
            .await?;
        if operation.outcome == WorkspaceOperationOutcome::Confirmed {
            let current = self
                .managed_workspace(session, workspace_scope, workspace.workspace_id)
                .await?;
            return self
                .confirmed_management_checkpoint_replay(&current, operation_id)
                .await?
                .then_some(())
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
        }

        let transitioned = match workspace.state {
            SandboxWorkspaceState::Active => {
                repository
                    .transition(WorkspaceTransition {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        from: SandboxWorkspaceState::Active,
                        to: SandboxWorkspaceState::Quiescing,
                        writer_epoch: expected_writer_epoch,
                        instance_generation: expected_instance_generation,
                    })
                    .await?
                    && repository
                        .transition(WorkspaceTransition {
                            tenant_id: binding.tenant_id,
                            workspace_id: binding.workspace_id,
                            from: SandboxWorkspaceState::Quiescing,
                            to: SandboxWorkspaceState::Committing,
                            writer_epoch: expected_writer_epoch,
                            instance_generation: expected_instance_generation,
                        })
                        .await?
            }
            SandboxWorkspaceState::Quiescing => {
                repository
                    .transition(WorkspaceTransition {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        from: SandboxWorkspaceState::Quiescing,
                        to: SandboxWorkspaceState::Committing,
                        writer_epoch: expected_writer_epoch,
                        instance_generation: expected_instance_generation,
                    })
                    .await?
            }
            SandboxWorkspaceState::Committing => true,
            _ => false,
        };
        if !transitioned {
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        if repository
            .create_checkpoint(CreateCheckpointRequest {
                checkpoint_id,
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                parent_checkpoint_id: binding
                    .current_revision
                    .as_ref()
                    .map(|revision| revision.checkpoint_id),
                operation_id,
                expected_writer_epoch,
                expected_instance_generation,
                expected_checkpoint_generation,
            })
            .await?
            .is_none()
        {
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        let storage_operation = WorkspaceStorageOperation {
            operation_id,
            kind: WorkspaceOperationKind::Checkpoint,
            binding: binding.clone(),
            deadline: deadline_at,
            request_hash,
        };
        let provider_result = if operation.outcome == WorkspaceOperationOutcome::Unknown {
            let storage = self.hands.checkpoint_store.as_ref().map(|store| {
                store.storage_reference(
                    crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        checkpoint_id,
                        provider_account_id: binding.provider_account_id,
                        provider_account_generation: binding.provider_account_generation,
                    },
                )
            });
            storage_provider
                .reconcile_workspace_operation(WorkspaceReconcileRequest::new(
                    storage_operation,
                    Some(hand.clone()),
                    storage,
                )?)
                .await
        } else {
            if !operations
                .begin_provider_attempt(binding.tenant_id, operation_id)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            storage_provider
                .publish_workspace_checkpoint(WorkspaceCheckpointPublishRequest {
                    operation: storage_operation,
                    hand: hand.clone(),
                    parent_revision: binding.current_revision.clone(),
                    release_compute: false,
                })
                .await
        };
        let result = match provider_result {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %error,
                    "workspace checkpoint provider outcome is ambiguous"
                );
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        let (publication, post_commit_state) = match (
            result.outcome,
            result.confirmed_disposition,
            result.checkpoint_publication.as_ref(),
            result.post_commit_state,
        ) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
                Some(publication),
                Some(post_commit_state),
            ) => (publication, post_commit_state),
            _ => {
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        if !repository
            .publish_workspace_checkpoint(PublishCheckpointCommitRequest {
                binding: &binding,
                operation_id,
                publication,
                post_commit_state,
                lease: &lease,
            })
            .await?
        {
            self.delete_abandoned_checkpoint_prefix(&binding, publication.revision.checkpoint_id)
                .await?;
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        if post_commit_state != WorkspacePostCommitState::AttachmentRetained {
            let key = session_provider_key(session, Some(&lease_scope), &workspace.provider);
            self.remove_cached_binding_if_matches(&key, hand, Some(lease.generation))
                .await;
            self.remove_installed_marker(
                manifest_scope_key(session, Some(&lease_scope)),
                &workspace.provider,
            )
            .await;
        }
        Ok(())
    }

    /// Resolves or creates the exact durable binding used for provisioning.
    pub(in crate::core) async fn prepare_workspace_for_provision(
        &self,
        route: &HandRoute,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        call_scope: ToolCallScope<'_>,
    ) -> Result<WorkspaceBinding> {
        let Some(repository) = self.hands.workspace_repository.as_ref() else {
            return Ok(workspace_binding_for_hand(
                session,
                workspace_scope,
                &route.provider,
            ));
        };
        let mut workspace = repository
            .get_by_scope(session.tenant_id, workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "authorized sandbox workspace has not been resolved for this execution scope"
                        .to_string(),
                )
            })?;
        if workspace.provider != route.provider {
            return Err(MoaError::ProviderError(format!(
                "workspace is pinned to provider {}; cross-provider recovery is disabled",
                workspace.provider
            )));
        }
        if workspace.access_fenced_at.is_some()
            || matches!(
                workspace.state,
                moa_core::types::sandbox_workspace::SandboxWorkspaceState::Deleting
                    | moa_core::types::sandbox_workspace::SandboxWorkspaceState::Deleted
                    | moa_core::types::sandbox_workspace::SandboxWorkspaceState::Reconciling
                    | moa_core::types::sandbox_workspace::SandboxWorkspaceState::Failed
            )
        {
            return Err(MoaError::PermissionDenied(
                "sandbox workspace is fenced or requires reconciliation".to_string(),
            ));
        }
        if workspace.state == SandboxWorkspaceState::Creating {
            call_scope.admit()?;
            self.prepare_initial_workspace_storage(&workspace, call_scope)
                .await?;
            if !repository
                .transition(WorkspaceTransition {
                    tenant_id: workspace.tenant_id,
                    workspace_id: workspace.workspace_id,
                    from: SandboxWorkspaceState::Creating,
                    to: SandboxWorkspaceState::Ready,
                    writer_epoch: workspace.writer_epoch,
                    instance_generation: workspace.instance_generation,
                })
                .await?
            {
                workspace = repository
                    .get_by_scope(session.tenant_id, workspace_scope)
                    .await?
                    .ok_or_else(|| {
                        MoaError::StorageError(
                            "workspace disappeared while storage preparation completed".to_string(),
                        )
                    })?;
            } else {
                workspace.state = SandboxWorkspaceState::Ready;
            }
        }
        if workspace.state == SandboxWorkspaceState::Ready {
            workspace = repository
                .claim_writer(WorkspaceWriterClaim {
                    tenant_id: workspace.tenant_id,
                    workspace_id: workspace.workspace_id,
                    expected_state: workspace.state,
                    expected_writer_epoch: workspace.writer_epoch,
                    expected_instance_generation: workspace.instance_generation,
                })
                .await?
                .ok_or_else(|| {
                    MoaError::StorageError(
                        "workspace writer claim lost its lifecycle fence".to_string(),
                    )
                })?;
        }
        if !matches!(
            workspace.state,
            SandboxWorkspaceState::Active | SandboxWorkspaceState::Restoring
        ) {
            return Err(MoaError::StorageError(format!(
                "workspace is not dispatchable while in state {}",
                workspace.state.as_str()
            )));
        }
        workspace.binding()
    }

    async fn prepare_initial_workspace_storage(
        &self,
        workspace: &SandboxWorkspace,
        call_scope: ToolCallScope<'_>,
    ) -> Result<()> {
        let prepare_started_at = std::time::Instant::now();
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let storage_provider = self
            .hands
            .storage_providers
            .get(&workspace.provider)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "workspace storage provider {} is not registered",
                    workspace.provider
                ))
            })?;
        let binding = workspace.binding()?;
        let operation_id = moa_core::types::identifiers::WorkspaceOperationId(Uuid::new_v5(
            &workspace.workspace_id.0,
            b"prepare-initial-storage-v1",
        ));
        let deadline_at = call_scope
            .budget
            .deadline
            .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5));
        let hash_bytes = serde_json::to_vec(&binding)?;
        let request_hash = format!("sha256:{}", hex::encode(Sha256::digest(hash_bytes)));
        let intent = WorkspaceOperationIntent {
            operation_id,
            tenant_id: workspace.tenant_id,
            workspace_id: workspace.workspace_id,
            provider_account_id: workspace.provider_account_id,
            provider_account_generation: workspace.provider_account_generation,
            kind: WorkspaceOperationKind::Create,
            request_hash: request_hash.clone(),
            expected_writer_epoch: workspace.writer_epoch,
            expected_instance_generation: workspace.instance_generation,
            expected_checkpoint_generation: workspace.checkpoint_generation,
            deadline_at,
            reconcile_not_before: deadline_at + ChronoDuration::seconds(30),
        };
        match operations.get(workspace.tenant_id, operation_id).await? {
            Some(existing)
                if existing.request_hash == request_hash
                    && existing.kind == WorkspaceOperationKind::Create => {}
            Some(_) => {
                return Err(MoaError::StorageError(
                    "workspace storage preparation replay changed its durable request".to_string(),
                ));
            }
            None => {
                operations.persist_intent(&intent).await?;
            }
        }
        failpoints::hit("post_reservation_pre_provider_create").await?;
        let result = storage_provider
            .prepare_workspace_storage(WorkspaceStoragePrepareRequest {
                operation: WorkspaceStorageOperation {
                    operation_id,
                    kind: WorkspaceOperationKind::Create,
                    binding,
                    deadline: deadline_at,
                    request_hash,
                },
            })
            .await?;
        // Every arm records an outcome, so the lifecycle counter carries the real
        // success/ambiguous ratio rather than only the happy path.
        match (result.outcome, result.confirmed_disposition) {
            (WorkspaceOperationOutcome::Confirmed, Some(disposition)) => {
                operations
                    .confirm_disposition(workspace.tenant_id, operation_id, disposition)
                    .await?;
                record_workspace_lifecycle(
                    &workspace.provider,
                    SandboxWorkspaceLifecycleOperation::Create,
                    SandboxWorkspaceMetricResult::Succeeded,
                    prepare_started_at.elapsed(),
                );
                Ok(())
            }
            (WorkspaceOperationOutcome::Unknown, None) => {
                operations
                    .mark_unknown(workspace.tenant_id, operation_id)
                    .await?;
                record_workspace_lifecycle(
                    &workspace.provider,
                    SandboxWorkspaceLifecycleOperation::Create,
                    SandboxWorkspaceMetricResult::Ambiguous,
                    prepare_started_at.elapsed(),
                );
                Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                })
            }
            _ => {
                record_workspace_lifecycle(
                    &workspace.provider,
                    SandboxWorkspaceLifecycleOperation::Create,
                    SandboxWorkspaceMetricResult::Failed,
                    prepare_started_at.elapsed(),
                );
                Err(MoaError::ProviderError(
                    "workspace storage provider returned an inconsistent preparation result"
                        .to_string(),
                ))
            }
        }
    }

    /// Restores and verifies durable workspace bytes before lease activation.
    pub(in crate::core) async fn hydrate_provisioned_workspace(
        &self,
        binding: &WorkspaceBinding,
        claim: &HandLease,
        hand: &HandHandle,
        call_scope: ToolCallScope<'_>,
    ) -> Result<()> {
        let Some(repository) = self.hands.workspace_repository.as_ref() else {
            return Ok(());
        };
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let provider = self
            .hands
            .storage_providers
            .get(&claim.provider)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "workspace storage provider {} is not registered",
                    claim.provider
                ))
            })?;
        let hydration_started_at = std::time::Instant::now();
        let kind = if binding.current_revision.is_some() {
            WorkspaceOperationKind::Restore
        } else {
            WorkspaceOperationKind::Attach
        };
        let operation_id = WorkspaceOperationId(Uuid::new_v5(
            &binding.workspace_id.0,
            format!(
                "hydrate-v1:{}:{}",
                claim.provisioning_operation_id,
                kind.as_str()
            )
            .as_bytes(),
        ));
        let request_bytes = serde_json::to_vec(&(binding, hand, kind))?;
        let request_hash = format!("sha256:{}", hex::encode(Sha256::digest(request_bytes)));
        let intent = WorkspaceOperationIntent {
            operation_id,
            tenant_id: binding.tenant_id,
            workspace_id: binding.workspace_id,
            provider_account_id: binding.provider_account_id,
            provider_account_generation: i64::try_from(binding.provider_account_generation)
                .map_err(|_| {
                    MoaError::StorageError(
                        "workspace provider-account generation is invalid".to_string(),
                    )
                })?,
            kind,
            request_hash: request_hash.clone(),
            expected_writer_epoch: i64::try_from(binding.writer_epoch).map_err(|_| {
                MoaError::StorageError("workspace writer epoch is invalid".to_string())
            })?,
            expected_instance_generation: i64::try_from(binding.instance_generation).map_err(
                |_| MoaError::StorageError("workspace instance generation is invalid".to_string()),
            )?,
            expected_checkpoint_generation: binding.current_revision.as_ref().map_or(
                Ok(0_i64),
                |revision| {
                    i64::try_from(revision.generation).map_err(|_| {
                        MoaError::StorageError(
                            "workspace checkpoint generation is invalid".to_string(),
                        )
                    })
                },
            )?,
            deadline_at: claim.provisioning_deadline_at,
            reconcile_not_before: claim.provisioning_deadline_at + ChronoDuration::seconds(30),
        };
        operations.persist_intent(&intent).await?;
        call_scope.admit()?;
        let operation = WorkspaceStorageOperation {
            operation_id,
            kind,
            binding: binding.clone(),
            deadline: claim.provisioning_deadline_at,
            request_hash,
        };
        let provider_result = match binding.current_revision.as_ref() {
            None => {
                self.run_within_scope(
                    call_scope,
                    provider.attach_workspace(WorkspaceAttachRequest {
                        operation,
                        hand: hand.clone(),
                        storage: None,
                    }),
                )
                .await
            }
            Some(revision) => {
                let checkpoint = repository
                    .get_checkpoint(
                        binding.tenant_id,
                        binding.workspace_id,
                        revision.checkpoint_id,
                    )
                    .await?
                    .ok_or_else(|| {
                        MoaError::StorageError(
                            "workspace head checkpoint is missing during restore".to_string(),
                        )
                    })?;
                if checkpoint.state
                    != moa_core::types::sandbox_workspace::WorkspaceCheckpointState::Available
                    || checkpoint.generation
                        != i64::try_from(revision.generation).map_err(|_| {
                            MoaError::StorageError(
                                "workspace checkpoint generation is invalid".to_string(),
                            )
                        })?
                {
                    return Err(MoaError::StorageError(
                        "workspace head checkpoint is not an exact available revision".to_string(),
                    ));
                }
                let resource_id = checkpoint.object_reference.ok_or_else(|| {
                    MoaError::StorageError(
                        "workspace head checkpoint has no portable object reference".to_string(),
                    )
                })?;
                self.run_within_scope(
                    call_scope,
                    provider.restore_workspace(WorkspaceRestoreRequest {
                        operation,
                        hand: hand.clone(),
                        revision: revision.clone(),
                        checkpoint: ProviderStorageRef {
                            provider_account_id: binding.provider_account_id,
                            provider_account_generation: binding.provider_account_generation,
                            kind: ProviderStorageKind::PortableCheckpoint,
                            resource_id,
                            workspace_locator: None,
                        },
                    }),
                )
                .await
            }
        };
        let result = match provider_result {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %error,
                    "workspace commit provider outcome is ambiguous"
                );
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        match (result.outcome, result.confirmed_disposition) {
            (WorkspaceOperationOutcome::Confirmed, Some(disposition)) => {
                if !operations
                    .confirm_disposition(binding.tenant_id, operation_id, disposition)
                    .await?
                {
                    return Err(MoaError::StorageError(
                        "workspace hydration lost its durable operation fence".to_string(),
                    ));
                }
                // Only a confirmed restore counts: an ambiguous or failed provider result
                // leaves no verified checkpoint in fresh compute, so counting it here
                // would overstate successful restores.
                if kind == WorkspaceOperationKind::Restore {
                    record_workspace_restore(&claim.provider);
                    record_workspace_checkpoint(
                        &claim.provider,
                        SandboxWorkspaceCheckpointOperation::Restore,
                        SandboxWorkspaceMetricResult::Succeeded,
                        0,
                        hydration_started_at.elapsed(),
                    );
                }
                Ok(())
            }
            (WorkspaceOperationOutcome::Unknown, None) => {
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                })
            }
            _ => Err(MoaError::ProviderError(
                "workspace storage provider returned an inconsistent hydration result".to_string(),
            )),
        }
    }

    /// Reinstalls the current trusted manifest before publishing an active lease.
    pub(in crate::core) async fn reinstall_trusted_files_before_activation(
        &self,
        session: &SessionMeta,
        worker_id: &str,
        provider: &str,
        hand: &HandHandle,
        call_scope: ToolCallScope<'_>,
    ) -> Result<Option<std::sync::Arc<TrustedSandboxManifest>>> {
        let provider_impl =
            self.hands.providers.get(provider).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
        let manifest_key = manifest_scope_key(session, Some(worker_id));
        loop {
            call_scope.admit()?;
            let manifest = self
                .hands
                .trusted_sandbox_files
                .read()
                .await
                .get(&manifest_key)
                .cloned();
            let Some(manifest) = manifest else {
                return Ok(None);
            };
            self.run_within_scope(
                call_scope,
                provider_impl.install_files(hand, manifest.files.as_ref()),
            )
            .await?;
            call_scope.admit()?;
            if self
                .hands
                .trusted_sandbox_files
                .read()
                .await
                .get(&manifest_key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &manifest))
            {
                return Ok(Some(manifest));
            }
        }
    }

    /// Records a trusted manifest installed on the exact preactivation hand.
    pub(in crate::core) async fn remember_preactivation_manifest_install(
        &self,
        session: &SessionMeta,
        worker_id: &str,
        provider: &str,
        cache_key: &HandProviderCacheKey,
        active: &ActiveHand,
        manifest: Option<&std::sync::Arc<TrustedSandboxManifest>>,
    ) {
        let Some(manifest) = manifest else {
            return;
        };
        let manifest_key = manifest_scope_key(session, Some(worker_id));
        let binding_is_current = self
            .hands
            .active_hands
            .read()
            .await
            .get(cache_key)
            .is_some_and(|current| current == active);
        if binding_is_current
            && self
                .hands
                .trusted_sandbox_files
                .read()
                .await
                .get(&manifest_key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, manifest))
        {
            self.hands
                .installed_files
                .write()
                .await
                .entry(manifest_key)
                .or_default()
                .insert(
                    provider.to_string(),
                    InstalledManifestMarker {
                        manifest_identity: manifest.identity,
                        handle: active.handle.clone(),
                        generation: active.generation,
                    },
                );
        }
    }

    async fn confirmed_workspace_commit_replay(
        &self,
        workspace: &SandboxWorkspace,
        tool_call_id: ToolCallId,
    ) -> Result<bool> {
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let operation_id = WorkspaceOperationId(Uuid::new_v5(
            &workspace.workspace_id.0,
            format!("tool-commit-v1:{tool_call_id}").as_bytes(),
        ));
        let Some(operation) = operations.get(workspace.tenant_id, operation_id).await? else {
            return Ok(false);
        };
        if operation.outcome == WorkspaceOperationOutcome::NotSent {
            return Ok(false);
        }
        if operation.outcome == WorkspaceOperationOutcome::Unknown {
            return Ok(false);
        }
        let checkpoint = repository
            .get_checkpoint_for_operation(workspace.tenant_id, workspace.workspace_id, operation_id)
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            })?;
        let committed_generation = operation
            .expected_checkpoint_generation
            .checked_add(1)
            .ok_or_else(|| {
                MoaError::StorageError("workspace checkpoint generation overflowed".to_string())
            })?;
        let parent_revision = match (
            operation.expected_checkpoint_generation,
            checkpoint.parent_checkpoint_id,
        ) {
            (0, None) => None,
            (generation, Some(checkpoint_id)) if generation > 0 => {
                Some(moa_core::types::sandbox_workspace::WorkspaceRevisionRef {
                    checkpoint_id,
                    generation: u64::try_from(generation).map_err(|_| {
                        MoaError::StorageError(
                            "workspace checkpoint generation is invalid".to_string(),
                        )
                    })?,
                    format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
                })
            }
            _ => {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        let mut original_binding = workspace.binding()?;
        original_binding.current_revision = parent_revision;
        let request_bytes = serde_json::to_vec(&(&original_binding, tool_call_id))?;
        let request_hash = format!("sha256:{}", hex::encode(Sha256::digest(request_bytes)));
        if operation.kind != WorkspaceOperationKind::Commit
            || operation.workspace_id != workspace.workspace_id
            || operation.provider_account_id != workspace.provider_account_id
            || operation.provider_account_generation != workspace.provider_account_generation
            || operation.expected_writer_epoch != workspace.writer_epoch
            || operation.expected_instance_generation != workspace.instance_generation
            || operation.request_hash != request_hash
            || operation.confirmed_disposition
                != Some(WorkspaceConfirmedDisposition::ResourcePresent)
            || checkpoint.state != WorkspaceCheckpointState::Available
            || checkpoint.checkpoint_id != WorkspaceCheckpointId(operation_id.0)
            || checkpoint.generation != committed_generation
            || checkpoint.source_writer_epoch != workspace.writer_epoch
            || checkpoint.source_instance_generation != workspace.instance_generation
            || workspace.checkpoint_id != Some(checkpoint.checkpoint_id)
            || workspace.checkpoint_generation != committed_generation
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        Ok(true)
    }

    /// Publishes the mutable workspace for one already-journaled sandbox command.
    ///
    /// This never dispatches the command. It reloads the exact workspace,
    /// active lease, and hand, then starts or resumes the deterministic commit.
    pub async fn commit_authorized_workspace_after_tool(
        &self,
        request: JournaledWorkspaceCommit<'_>,
    ) -> Result<()> {
        request.scope.admit()?;
        let workspace_scope = request.workspace_scope;
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let workspace = repository
            .get_by_scope(request.session.tenant_id, workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "authorized sandbox workspace disappeared before commit".to_string(),
                )
            })?;
        if self
            .confirmed_workspace_commit_replay(&workspace, request.tool_call_id)
            .await?
        {
            return Ok(());
        }
        let lease_scope = workspace_lease_scope(workspace_scope);
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!("workspace-tool-call:{}", request.tool_call_id),
            })?;
        let hand = lease
            .handle
            .as_ref()
            .map(|handle| handle.handle.clone())
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!("workspace-tool-call:{}", request.tool_call_id),
            })?;
        self.commit_workspace_after_tool(WorkspaceCommitExecution {
            session: request.session,
            workspace_scope,
            tool_call_id: request.tool_call_id,
            provider_name: &workspace.provider,
            hand: &hand,
            call_scope: request.scope,
            release_compute: false,
        })
        .await
    }

    /// Publishes one execution-task continuation checkpoint and keeps what it can.
    ///
    /// A plain model/tool boundary is not a wait: the next slice is enqueued for
    /// immediate re-admission, so destroying the sandbox here and restoring it
    /// milliseconds later is pure loss — object-store read, decrypt, extract, and a
    /// per-file upload round trip on both cloud providers. This publishes the exact
    /// same durable checkpoint the release path publishes, so the portable recovery
    /// authority advances on every boundary, and then chooses how to keep the
    /// sandbox based on what the provider can actually do:
    ///
    /// * A provider with real compute suspension stops the sandbox **in this
    ///   call** and hands its `ActiveHands` slot back to the fleet. Release timing
    ///   is deterministic rather than reaper-lagged, and an idle sandbox stops
    ///   costing compute and stops competing with runnable work for admission.
    /// * A provider without it keeps the hand hot on a deliberately short
    ///   reaper-owned deadline. That bet only pays off when the next slice arrives
    ///   fast, so a longer window would only extend the loss.
    ///
    /// Both paths are safe for the same reason: the checkpoint commits *before*
    /// any deadline is armed or any compute is stopped, so losing the warm
    /// sandbox is a pure cache miss — the next slice restores from the same head.
    pub async fn checkpoint_execution_hand_retaining_compute(
        &self,
        request: ExecutionHandRetentionRequest<'_>,
    ) -> Result<ExecutionHandContinuationDisposition> {
        if request.attempt_generation == 0 || request.logical_generation == 0 {
            return Err(MoaError::ValidationError(
                "execution task attempt and logical generations must be positive".to_string(),
            ));
        }
        let Some(repository) = self.hands.workspace_repository.as_ref() else {
            return Ok(ExecutionHandContinuationDisposition::NoComputeOwned);
        };
        let workspace_scope = SandboxWorkspaceScope::ExecutionTask {
            run_id: request.run_id,
            task_id: request.task_id,
        };
        // An attempt that never provisioned a durable workspace or whose lease is no
        // longer live has nothing to publish and nothing to keep. Its committed head
        // is already the recovery authority, so this is a no-op rather than an error.
        let Some(workspace) = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
        else {
            return Ok(ExecutionHandContinuationDisposition::NoComputeOwned);
        };
        let lease_scope = workspace_lease_scope(&workspace_scope);
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let Some(lease) = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &workspace.provider,
            )
            .await?
        else {
            return Ok(ExecutionHandContinuationDisposition::NoComputeOwned);
        };
        if lease.status != HandLeaseStatus::Active {
            return Ok(ExecutionHandContinuationDisposition::NoComputeOwned);
        }
        let Some(hand) = lease.handle.as_ref().map(|handle| handle.handle.clone()) else {
            return Ok(ExecutionHandContinuationDisposition::NoComputeOwned);
        };

        let continuation_key = format!(
            "execution-task-continuation-v1:{}:{}:{}",
            request.run_id, request.task_id, request.attempt_generation
        );
        let tool_call_id = ToolCallId(Uuid::new_v5(
            &workspace.workspace_id.0,
            continuation_key.as_bytes(),
        ));
        self.commit_workspace_after_tool(WorkspaceCommitExecution {
            session: request.session,
            workspace_scope: &workspace_scope,
            tool_call_id,
            provider_name: &workspace.provider,
            hand: &hand,
            call_scope: request.scope,
            release_compute: false,
        })
        .await?;

        let provider_impl = self
            .hands
            .providers
            .get(&workspace.provider)
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "hand provider {} is not registered",
                    workspace.provider
                ))
            })?
            .clone();
        if provider_impl.supports_suspend() {
            return self
                .suspend_continuation_hand(
                    &request,
                    provider_impl.as_ref(),
                    &workspace,
                    &lease,
                    &lease_scope,
                    &hand,
                )
                .await;
        }

        // Armed only after the checkpoint commits. Arming it first would let the
        // reaper claim and destroy the sandbox in the middle of its own publication.
        let started_at = std::time::Instant::now();
        self.bound_retained_hand_lifetime(request, &lease_scope, &workspace.provider)
            .await?;
        record_workspace_lifecycle(
            &workspace.provider,
            SandboxWorkspaceLifecycleOperation::Retain,
            SandboxWorkspaceMetricResult::Succeeded,
            started_at.elapsed(),
        );
        Ok(ExecutionHandContinuationDisposition::RetainedHot)
    }

    /// Stops a continuation sandbox's compute and returns its admission slot.
    ///
    /// The provider stop runs before the capacity release on purpose. Releasing
    /// first and then failing to stop would under-count a sandbox that is still
    /// burning compute; this order can only over-count a sandbox that is already
    /// stopped, which the reattach path resolves without double-charging.
    async fn suspend_continuation_hand(
        &self,
        request: &ExecutionHandRetentionRequest<'_>,
        provider_impl: &dyn moa_core::traits::HandProvider,
        workspace: &SandboxWorkspace,
        lease: &HandLease,
        lease_scope: &str,
        hand: &HandHandle,
    ) -> Result<ExecutionHandContinuationDisposition> {
        let started_at = std::time::Instant::now();
        if let Err(error) = self
            .run_within_scope(request.scope, provider_impl.suspend(hand))
            .await
        {
            // Non-fatal by contract: the checkpoint is already published, so the
            // caller finishes the ordinary checkpoint-and-destroy path instead of
            // leaving a hand hot on a bet that has already lost.
            tracing::warn!(
                provider = %workspace.provider,
                generation = lease.generation,
                error = %error,
                "continuation sandbox suspension failed; falling back to release"
            );
            record_workspace_lifecycle(
                &workspace.provider,
                SandboxWorkspaceLifecycleOperation::Suspend,
                SandboxWorkspaceMetricResult::Failed,
                started_at.elapsed(),
            );
            return Ok(ExecutionHandContinuationDisposition::SuspendFailed);
        }

        // The in-process binding cache hands out an active lease's handle without
        // consulting the provider, so a stopped sandbox must be evicted here or the
        // next same-process slice would dispatch into compute that is not running.
        let cache_key =
            session_provider_key(request.session, Some(lease_scope), &workspace.provider);
        self.remove_cached_binding_if_matches(&cache_key, hand, Some(lease.generation))
            .await;

        if let Some(capacity) = self.hands.workspace_capacity.as_ref() {
            let binding = workspace.binding()?;
            let released = capacity
                .release_suspended_active_hand(&active_hand_capacity_request(&binding, lease)?)
                .await?;
            if !released {
                // The charge stays held, which is the conservative direction: the
                // sandbox really is stopped, so the fleet is only under-admitting.
                tracing::warn!(
                    provider = %workspace.provider,
                    generation = lease.generation,
                    "suspended continuation sandbox kept its active-hands charge"
                );
            }
        }
        record_workspace_lifecycle(
            &workspace.provider,
            SandboxWorkspaceLifecycleOperation::Suspend,
            SandboxWorkspaceMetricResult::Succeeded,
            started_at.elapsed(),
        );
        Ok(ExecutionHandContinuationDisposition::Suspended)
    }

    /// Shortens a retained continuation hand's idle deadline to its retention bound.
    ///
    /// Reuses the ordinary active-lease renewal, which sets the idle deadline under
    /// the immutable hard lifetime. The requested bound is additionally clamped to the
    /// lease's current idle deadline so retention can only shorten a sandbox's life,
    /// never extend it past the policy it was admitted under.
    async fn bound_retained_hand_lifetime(
        &self,
        request: ExecutionHandRetentionRequest<'_>,
        lease_scope: &str,
        provider: &str,
    ) -> Result<()> {
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let Some(lease) = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                lease_scope,
                provider,
            )
            .await?
        else {
            return Ok(());
        };
        if lease.status != HandLeaseStatus::Active {
            return Ok(());
        }
        let Some(attachment) = lease.attachment.clone() else {
            return Ok(());
        };
        let retention_deadline_at = lease
            .idle_expires_at
            .map_or(request.retention_deadline_at, |idle| {
                idle.min(request.retention_deadline_at)
            });
        if !lease_store
            .renew_active(crate::core::leases::HandLeaseRenewRequest {
                tenant_id: request.session.tenant_id,
                session_id: request.session.id,
                worker_id: lease_scope,
                provider,
                generation: lease.generation,
                provisioning_operation_id: lease.provisioning_operation_id,
                attachment,
                idle_expires_at: retention_deadline_at,
            })
            .await?
        {
            // The lease moved under us, so some other durable owner already governs
            // this sandbox's lifetime. The checkpoint is published either way, so the
            // worst outcome is that the hand expires on its ordinary idle policy.
            tracing::warn!(
                provider,
                generation = lease.generation,
                "retained execution continuation hand kept its ordinary idle deadline"
            );
        }
        Ok(())
    }

    /// Checkpoints one execution-task workspace and releases its exact compute lease.
    ///
    /// The returned receipt is the durable proof required before a task may yield to
    /// a timer, external callback, pause, or long backoff. Retries return the same
    /// receipt. Provider teardown errors remain ambiguous and never produce release
    /// proof; a later retry reconciles the checkpoint and repeats exact destruction.
    pub async fn checkpoint_and_release_execution_hand(
        &self,
        request: ExecutionHandReleaseRequest<'_>,
    ) -> Result<ExecutionHandReleaseReceipt> {
        if request.attempt_generation == 0 {
            return Err(MoaError::ValidationError(
                "execution task attempt generation must be positive".to_string(),
            ));
        }
        let (task_id, logical_generation) = match request.owner {
            ExecutionHandReleaseOwner::Task {
                task_id,
                logical_generation,
            } if logical_generation > 0 => (task_id, logical_generation),
            ExecutionHandReleaseOwner::Task { .. } => {
                return Err(MoaError::ValidationError(
                    "execution task logical generation must be positive".to_string(),
                ));
            }
            ExecutionHandReleaseOwner::Compensation {
                compensation_id,
                logical_generation,
            } => {
                return self
                    .release_execution_compensation_hand(
                        request,
                        compensation_id,
                        logical_generation,
                    )
                    .await;
            }
        };
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        if let Some(receipt) = repository
            .get_task_execution_hand_release_receipt(
                request.session.tenant_id,
                request.run_id,
                task_id,
                logical_generation,
                request.attempt_generation,
            )
            .await?
        {
            return Ok(receipt);
        }

        let absence_receipt_id = Uuid::new_v5(
            &request.run_id.0,
            format!(
                "execution-task-hand-absence-v1:{task_id}:{logical_generation}:{}",
                request.attempt_generation
            )
            .as_bytes(),
        );
        match repository
            .record_absent_task_execution_hand_release_receipt(AbsentTaskHandReleaseIntent {
                receipt_id: absence_receipt_id,
                tenant_id: request.session.tenant_id,
                run_id: request.run_id,
                task_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                verified_at: Utc::now(),
            })
            .await
        {
            Ok(receipt) => return Ok(receipt),
            Err(MoaError::ExternalEffectUnknownOutcome { .. }) => {}
            Err(error) => return Err(error),
        }

        let workspace_scope = SandboxWorkspaceScope::ExecutionTask {
            run_id: request.run_id,
            task_id,
        };
        let initial_workspace = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "execution-task workspace disappeared before hand release".to_string(),
                )
            })?;
        let release_key = format!(
            "execution-task-yield-v1:{}:{}:{}",
            request.run_id, task_id, request.attempt_generation
        );
        let tool_call_id = ToolCallId(Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            release_key.as_bytes(),
        ));
        let candidate_receipt_id = Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            format!("release-receipt-v1:{release_key}").as_bytes(),
        );
        let lease_scope = workspace_lease_scope(&workspace_scope);
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let initial_lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &initial_workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        let (receipt_id, release_claim_token, requested_at) = repository
            .begin_task_execution_hand_release(TaskHandReleaseIntent {
                receipt_id: candidate_receipt_id,
                run_id: request.run_id,
                task_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                deadline_at: request
                    .scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5)),
                recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
                workspace: &initial_workspace,
                lease: &initial_lease,
            })
            .await?;

        admit_execution_release_step(
            request.scope,
            if initial_lease.status == HandLeaseStatus::Active {
                ExecutionReleaseStep::ProviderIo
            } else {
                ExecutionReleaseStep::DurableReconciliation
            },
        )?;

        match initial_lease.status {
            HandLeaseStatus::Active => {
                let hand = initial_lease
                    .handle
                    .as_ref()
                    .map(|handle| handle.handle.clone())
                    .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    })?;
                self.commit_workspace_after_tool(WorkspaceCommitExecution {
                    session: request.session,
                    workspace_scope: &workspace_scope,
                    tool_call_id,
                    provider_name: &initial_workspace.provider,
                    hand: &hand,
                    call_scope: request.scope,
                    release_compute: true,
                })
                .await?;
            }
            HandLeaseStatus::Destroyed => {
                if !self
                    .confirmed_workspace_commit_replay(&initial_workspace, tool_call_id)
                    .await?
                {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    });
                }
            }
            HandLeaseStatus::Provisioning
            | HandLeaseStatus::Stale
            | HandLeaseStatus::Failed
            | HandLeaseStatus::Reaping => {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
        }

        let mut final_workspace = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        let mut final_lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &initial_workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;

        // An unknown provider outcome may reconcile the already-verified bytes
        // while conservatively retaining the attachment. Finish the destroy as a
        // separate exact step, then atomically release lease and capacity ownership.
        if final_lease.status == HandLeaseStatus::Active {
            if !self
                .confirmed_workspace_commit_replay(&final_workspace, tool_call_id)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
            let hand = final_lease
                .handle
                .as_ref()
                .map(|handle| handle.handle.clone())
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
            let provider = self
                .hands
                .providers
                .get(&initial_workspace.provider)
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "hand provider {} is not registered",
                        initial_workspace.provider
                    ))
                })?;
            self.run_within_scope(request.scope, provider.destroy(&hand))
                .await
                .map_err(|error| {
                    tracing::warn!(
                        operation_id = %release_key,
                        error = %error,
                        "execution-task hand destroy outcome is ambiguous"
                    );
                    MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    }
                })?;
            if !repository
                .finalize_task_yield_destroy(&final_workspace.binding()?, &final_lease)
                .await?
            {
                // The compute is gone but the durable release did not commit, so the
                // charge is still held and a reconciler owns it. Recorded as ambiguous
                // rather than succeeded so the two are distinguishable on the dashboard.
                record_workspace_release(
                    &initial_workspace.provider,
                    SandboxWorkspaceMetricResult::Ambiguous,
                );
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
            // Counted only after provider destruction is verified AND the release
            // receipt commits, which together are what actually free the capacity.
            record_workspace_release(
                &initial_workspace.provider,
                SandboxWorkspaceMetricResult::Succeeded,
            );
            let key = session_provider_key(
                request.session,
                Some(&lease_scope),
                &initial_workspace.provider,
            );
            self.remove_cached_binding_if_matches(&key, &hand, Some(initial_lease.generation))
                .await;
            self.remove_installed_marker(
                manifest_scope_key(request.session, Some(&lease_scope)),
                &initial_workspace.provider,
            )
            .await;
            final_workspace = repository
                .get_by_scope(request.session.tenant_id, &workspace_scope)
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
            final_lease = lease_store
                .get(
                    request.session.tenant_id,
                    request.session.id,
                    &lease_scope,
                    &initial_workspace.provider,
                )
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
        }

        let operation_id = WorkspaceOperationId(Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            format!("tool-commit-v1:{tool_call_id}").as_bytes(),
        ));
        let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
        let checkpoint = repository
            .get_checkpoint(
                request.session.tenant_id,
                final_workspace.workspace_id,
                checkpoint_id,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        if final_workspace.state != SandboxWorkspaceState::Ready
            || final_workspace.writer_epoch != initial_workspace.writer_epoch
            || final_workspace.instance_generation != initial_workspace.instance_generation
            || final_workspace.checkpoint_id != Some(checkpoint_id)
            || final_workspace.checkpoint_generation != checkpoint.generation
            || final_lease.status != HandLeaseStatus::Destroyed
            || final_lease.handle.is_some()
            || final_lease.generation != initial_lease.generation
            || final_lease.provisioning_operation_id != initial_lease.provisioning_operation_id
            || checkpoint.state != WorkspaceCheckpointState::Available
            || checkpoint.manifest_digest.is_none()
            || checkpoint.logical_bytes.is_none()
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key,
            });
        }
        let receipt = ExecutionHandReleaseReceipt {
            receipt_id,
            tenant_id: request.session.tenant_id,
            run_id: request.run_id,
            owner: request.owner,
            attempt_generation: request.attempt_generation,
            workspace_id: Some(final_workspace.workspace_id),
            writer_epoch: Some(u64::try_from(final_workspace.writer_epoch).map_err(|_| {
                MoaError::StorageError("workspace writer epoch is invalid".to_string())
            })?),
            instance_generation: Some(u64::try_from(final_workspace.instance_generation).map_err(
                |_| MoaError::StorageError("workspace instance generation is invalid".to_string()),
            )?),
            hand_provisioning_operation_id: Some(initial_lease.provisioning_operation_id),
            hand_lease_generation: Some(u64::try_from(initial_lease.generation).map_err(|_| {
                MoaError::StorageError("hand lease generation is invalid".to_string())
            })?),
            checkpoint_id: Some(checkpoint_id),
            checkpoint_generation: Some(u64::try_from(checkpoint.generation).map_err(|_| {
                MoaError::StorageError("checkpoint generation is invalid".to_string())
            })?),
            checkpoint_manifest_digest: Some(checkpoint.manifest_digest.ok_or_else(|| {
                MoaError::StorageError("verified checkpoint digest is missing".to_string())
            })?),
            checkpoint_logical_bytes: Some(
                u64::try_from(checkpoint.logical_bytes.ok_or_else(|| {
                    MoaError::StorageError("verified checkpoint bytes are missing".to_string())
                })?)
                .map_err(|_| MoaError::StorageError("checkpoint bytes are negative".to_string()))?,
            ),
            requested_at,
            released_at: Utc::now(),
        };
        repository
            .record_task_execution_hand_release_receipt(&receipt, release_claim_token)
            .await
    }

    async fn release_execution_compensation_hand(
        &self,
        request: ExecutionHandReleaseRequest<'_>,
        compensation_id: ExecutionCompensationScopeId,
        logical_generation: u64,
    ) -> Result<ExecutionHandReleaseReceipt> {
        if logical_generation == 0 {
            return Err(MoaError::ValidationError(
                "execution compensation logical generation must be positive".to_string(),
            ));
        }
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        if let Some(receipt) = repository
            .get_compensation_execution_hand_release_receipt(
                request.session.tenant_id,
                request.run_id,
                compensation_id,
                logical_generation,
                request.attempt_generation,
            )
            .await?
        {
            return Ok(receipt);
        }

        let hand_scope = format!(
            "execution_compensation:{}:{}",
            request.run_id, compensation_id
        );
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        if let Some(claim) = repository
            .claim_pending_compensation_execution_hand_release(
                request.session.tenant_id,
                request.run_id,
                compensation_id,
                logical_generation,
                request.attempt_generation,
                Utc::now() + ChronoDuration::minutes(5),
            )
            .await?
        {
            let persisted_identity = match (
                claim.hand_provisioning_operation_id,
                claim.hand_lease_generation,
            ) {
                (Some(operation_id), Some(generation)) => Some((operation_id, generation)),
                (None, None) => None,
                _ => {
                    return Err(MoaError::StorageError(
                        "pending compensation release has a partial hand identity".to_string(),
                    ));
                }
            };
            let exact_lease = match persisted_identity {
                Some((operation_id, generation)) => {
                    lease_store
                        .get_exact_generation(
                            request.session.tenant_id,
                            request.session.id,
                            &hand_scope,
                            operation_id,
                            generation,
                        )
                        .await?
                }
                None => None,
            };
            let provider_io_required = exact_lease
                .as_ref()
                .is_some_and(|lease| lease.status != HandLeaseStatus::Destroyed);
            admit_execution_release_step(
                request.scope,
                if provider_io_required {
                    ExecutionReleaseStep::ProviderIo
                } else {
                    ExecutionReleaseStep::DurableReconciliation
                },
            )?;
            if provider_io_required
                && !self
                    .reclaim_hands(
                        request.session.tenant_id,
                        &request.session.id,
                        Some(&hand_scope),
                    )
                    .await
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: format!(
                        "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                        request.run_id, request.attempt_generation
                    ),
                });
            }
            let exact_lease = match persisted_identity {
                Some((operation_id, generation)) => {
                    lease_store
                        .get_exact_generation(
                            request.session.tenant_id,
                            request.session.id,
                            &hand_scope,
                            operation_id,
                            generation,
                        )
                        .await?
                }
                None => None,
            };
            let lease_state = match exact_lease.as_ref() {
                None => PersistedLeaseReleaseState::Missing,
                Some(lease)
                    if lease.status == HandLeaseStatus::Destroyed && lease.handle.is_none() =>
                {
                    PersistedLeaseReleaseState::Destroyed
                }
                Some(_) => PersistedLeaseReleaseState::LiveOrAmbiguous,
            };
            let exact_released = compensation_release_identity_is_verified(
                persisted_identity.is_some(),
                lease_state,
            );
            let replacement = lease_store
                .has_live_owner(request.session.tenant_id, request.session.id, &hand_scope)
                .await?;
            if !exact_released || replacement {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: format!(
                        "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                        request.run_id, request.attempt_generation
                    ),
                });
            }
            let hand_lease_generation = claim
                .hand_lease_generation
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    MoaError::StorageError("hand lease generation is invalid".to_string())
                })?;
            return repository
                .record_compensation_execution_hand_release_receipt(
                    &ExecutionHandReleaseReceipt {
                        receipt_id: claim.receipt_id,
                        tenant_id: request.session.tenant_id,
                        run_id: request.run_id,
                        owner: request.owner,
                        attempt_generation: request.attempt_generation,
                        workspace_id: None,
                        writer_epoch: None,
                        instance_generation: None,
                        hand_provisioning_operation_id: claim.hand_provisioning_operation_id,
                        hand_lease_generation,
                        checkpoint_id: None,
                        checkpoint_generation: None,
                        checkpoint_manifest_digest: None,
                        checkpoint_logical_bytes: None,
                        requested_at: claim.requested_at,
                        released_at: Utc::now(),
                    },
                    request.session.id,
                    &hand_scope,
                    claim.claim_token,
                )
                .await;
        }
        let leases = lease_store
            .list_live_owner_candidates(request.session.tenant_id, request.session.id, &hand_scope)
            .await?;
        if leases.len() > 1 {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!(
                    "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                    request.run_id, request.attempt_generation
                ),
            });
        }
        let initial_lease = leases.into_iter().next();

        let release_key = format!(
            "execution-compensation-release-v1:{}:{compensation_id}:{logical_generation}:{}",
            request.run_id, request.attempt_generation
        );
        let receipt_id = Uuid::new_v5(&request.run_id.0, release_key.as_bytes());
        let (receipt_id, claim_token, requested_at) = repository
            .begin_compensation_execution_hand_release(CompensationHandReleaseIntent {
                receipt_id,
                tenant_id: request.session.tenant_id,
                session_id: request.session.id,
                run_id: request.run_id,
                compensation_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                hand_scope: &hand_scope,
                lease: initial_lease.as_ref(),
                deadline_at: request
                    .scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5)),
                recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
            })
            .await?;
        admit_execution_release_step(
            request.scope,
            if initial_lease.is_some() {
                ExecutionReleaseStep::ProviderIo
            } else {
                ExecutionReleaseStep::DurableReconciliation
            },
        )?;
        if initial_lease.is_some()
            && !self
                .reclaim_hands(
                    request.session.tenant_id,
                    &request.session.id,
                    Some(&hand_scope),
                )
                .await
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key,
            });
        }
        if let Some(initial_lease) = initial_lease.as_ref() {
            let exact_lease = lease_store
                .get(
                    request.session.tenant_id,
                    request.session.id,
                    &hand_scope,
                    &initial_lease.provider,
                )
                .await?;
            let exact_destroyed = exact_lease.as_ref().is_some_and(|lease| {
                lease.worker_id == hand_scope
                    && lease.provisioning_operation_id == initial_lease.provisioning_operation_id
                    && lease.generation == initial_lease.generation
                    && lease.status == HandLeaseStatus::Destroyed
                    && lease.handle.is_none()
            });
            let replacement = lease_store
                .has_live_owner(request.session.tenant_id, request.session.id, &hand_scope)
                .await?;
            if !exact_destroyed || replacement {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key,
                });
            }
        }
        let hand_lease_generation = initial_lease
            .as_ref()
            .map(|lease| {
                u64::try_from(lease.generation).map_err(|_| {
                    MoaError::StorageError("hand lease generation is invalid".to_string())
                })
            })
            .transpose()?;
        repository
            .record_compensation_execution_hand_release_receipt(
                &ExecutionHandReleaseReceipt {
                    receipt_id,
                    tenant_id: request.session.tenant_id,
                    run_id: request.run_id,
                    owner: request.owner,
                    attempt_generation: request.attempt_generation,
                    workspace_id: None,
                    writer_epoch: None,
                    instance_generation: None,
                    hand_provisioning_operation_id: initial_lease
                        .as_ref()
                        .map(|lease| lease.provisioning_operation_id),
                    hand_lease_generation,
                    checkpoint_id: None,
                    checkpoint_generation: None,
                    checkpoint_manifest_digest: None,
                    checkpoint_logical_bytes: None,
                    requested_at,
                    released_at: Utc::now(),
                },
                request.session.id,
                &hand_scope,
                claim_token,
            )
            .await
    }

    pub(in crate::core) async fn commit_workspace_after_tool(
        &self,
        request: WorkspaceCommitExecution<'_>,
    ) -> Result<()> {
        let WorkspaceCommitExecution {
            session,
            workspace_scope,
            tool_call_id,
            provider_name,
            hand,
            call_scope,
            release_compute,
        } = request;
        let Some(repository) = self.hands.workspace_repository.as_ref() else {
            return Ok(());
        };
        let operations = self.hands.workspace_operations.as_ref().ok_or_else(|| {
            MoaError::StorageError("workspace operation repository missing".to_string())
        })?;
        let storage_provider =
            self.hands
                .storage_providers
                .get(provider_name)
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "workspace storage provider {provider_name} is not registered"
                    ))
                })?;
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let workspace = repository
            .get_by_scope(session.tenant_id, workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "authorized sandbox workspace disappeared before commit".to_string(),
                )
            })?;
        if self
            .confirmed_workspace_commit_replay(&workspace, tool_call_id)
            .await?
        {
            return Ok(());
        }
        if !matches!(
            workspace.state,
            SandboxWorkspaceState::Active
                | SandboxWorkspaceState::Quiescing
                | SandboxWorkspaceState::Committing
        ) || workspace.provider != provider_name
            || workspace.access_fenced_at.is_some()
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!("workspace-tool-call:{tool_call_id}"),
            });
        }
        let binding = workspace.binding()?;
        let lease_scope = workspace_lease_scope(workspace_scope);
        let lease = lease_store
            .get(session.tenant_id, session.id, &lease_scope, provider_name)
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "active workspace lease is missing before commit".to_string(),
                )
            })?;
        if lease.status != HandLeaseStatus::Active
            || lease.handle.as_ref().map(|lease| &lease.handle) != Some(hand)
            || lease.attachment != Some(lease_attachment(&binding)?)
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!("workspace-tool-call:{tool_call_id}"),
            });
        }

        let operation_id = WorkspaceOperationId(Uuid::new_v5(
            &binding.workspace_id.0,
            format!("tool-commit-v1:{tool_call_id}").as_bytes(),
        ));
        let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
        let existing_operation = operations.get(binding.tenant_id, operation_id).await?;
        let mut deadline_at = existing_operation.as_ref().map_or_else(
            || {
                call_scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5))
            },
            |operation| operation.deadline_at,
        );
        let request_bytes = serde_json::to_vec(&(&binding, tool_call_id))?;
        let request_hash = format!("sha256:{}", hex::encode(Sha256::digest(request_bytes)));
        let expected_writer_epoch = i64::try_from(binding.writer_epoch)
            .map_err(|_| MoaError::StorageError("workspace writer epoch is invalid".to_string()))?;
        let expected_instance_generation =
            i64::try_from(binding.instance_generation).map_err(|_| {
                MoaError::StorageError("workspace instance generation is invalid".to_string())
            })?;
        let expected_checkpoint_generation =
            binding
                .current_revision
                .as_ref()
                .map_or(Ok(0_i64), |revision| {
                    i64::try_from(revision.generation).map_err(|_| {
                        MoaError::StorageError(
                            "workspace checkpoint generation is invalid".to_string(),
                        )
                    })
                })?;
        let operation = operations
            .persist_intent(&WorkspaceOperationIntent {
                operation_id,
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                provider_account_id: binding.provider_account_id,
                provider_account_generation: i64::try_from(binding.provider_account_generation)
                    .map_err(|_| {
                        MoaError::StorageError(
                            "workspace provider-account generation is invalid".to_string(),
                        )
                    })?,
                kind: WorkspaceOperationKind::Commit,
                request_hash: request_hash.clone(),
                expected_writer_epoch,
                expected_instance_generation,
                expected_checkpoint_generation,
                deadline_at,
                reconcile_not_before: deadline_at + ChronoDuration::seconds(30),
            })
            .await?;
        debug_assert_ne!(operation.outcome, WorkspaceOperationOutcome::Confirmed);
        let transitioned = match workspace.state {
            SandboxWorkspaceState::Active => {
                repository
                    .transition(WorkspaceTransition {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        from: SandboxWorkspaceState::Active,
                        to: SandboxWorkspaceState::Quiescing,
                        writer_epoch: expected_writer_epoch,
                        instance_generation: expected_instance_generation,
                    })
                    .await?
                    && repository
                        .transition(WorkspaceTransition {
                            tenant_id: binding.tenant_id,
                            workspace_id: binding.workspace_id,
                            from: SandboxWorkspaceState::Quiescing,
                            to: SandboxWorkspaceState::Committing,
                            writer_epoch: expected_writer_epoch,
                            instance_generation: expected_instance_generation,
                        })
                        .await?
            }
            SandboxWorkspaceState::Quiescing => {
                repository
                    .transition(WorkspaceTransition {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        from: SandboxWorkspaceState::Quiescing,
                        to: SandboxWorkspaceState::Committing,
                        writer_epoch: expected_writer_epoch,
                        instance_generation: expected_instance_generation,
                    })
                    .await?
            }
            SandboxWorkspaceState::Committing => true,
            _ => false,
        };
        if !transitioned {
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        if let Err(error) = call_scope.admit() {
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(error);
        }
        let checkpoint = repository
            .create_checkpoint(CreateCheckpointRequest {
                checkpoint_id,
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                parent_checkpoint_id: binding
                    .current_revision
                    .as_ref()
                    .map(|revision| revision.checkpoint_id),
                operation_id,
                expected_writer_epoch,
                expected_instance_generation,
                expected_checkpoint_generation,
            })
            .await?;
        if checkpoint.is_none() {
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        failpoints::hit("post_command_pre_checkpoint_publication").await?;
        if operation.outcome == WorkspaceOperationOutcome::NotSent && deadline_at <= Utc::now() {
            let renewed_deadline = call_scope
                .budget
                .deadline
                .filter(|deadline| *deadline > Utc::now())
                .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5));
            if !operations
                .renew_not_sent_commit_deadline(
                    binding.tenant_id,
                    operation_id,
                    deadline_at,
                    renewed_deadline,
                )
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            deadline_at = renewed_deadline;
        }
        let storage_operation = WorkspaceStorageOperation {
            operation_id,
            kind: WorkspaceOperationKind::Commit,
            binding: binding.clone(),
            deadline: deadline_at,
            request_hash,
        };
        let provider_result = if operation.outcome == WorkspaceOperationOutcome::Unknown {
            let storage = self.hands.checkpoint_store.as_ref().map(|store| {
                store.storage_reference(
                    crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                        tenant_id: binding.tenant_id,
                        workspace_id: binding.workspace_id,
                        checkpoint_id,
                        provider_account_id: binding.provider_account_id,
                        provider_account_generation: binding.provider_account_generation,
                    },
                )
            });
            let reconcile =
                WorkspaceReconcileRequest::new(storage_operation, Some(hand.clone()), storage)?;
            self.run_within_scope(
                call_scope,
                storage_provider.reconcile_workspace_operation(reconcile),
            )
            .await
        } else {
            if !operations
                .begin_provider_attempt(binding.tenant_id, operation_id)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            self.run_within_scope(
                call_scope,
                storage_provider.publish_workspace_checkpoint(WorkspaceCheckpointPublishRequest {
                    operation: storage_operation,
                    hand: hand.clone(),
                    parent_revision: binding.current_revision.clone(),
                    release_compute,
                }),
            )
            .await
        };
        let result = match provider_result {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %error,
                    "workspace tool commit provider outcome is ambiguous"
                );
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        let (publication, post_commit_state) = match (
            result.outcome,
            result.confirmed_disposition,
            result.checkpoint_publication.as_ref(),
            result.post_commit_state,
        ) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
                Some(publication),
                Some(post_commit_state),
            ) => (publication, post_commit_state),
            _ => {
                operations
                    .mark_unknown(binding.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        if !repository
            .publish_checkpoint_commit(PublishCheckpointCommitRequest {
                binding: &binding,
                operation_id,
                publication,
                post_commit_state,
                lease: &lease,
            })
            .await?
        {
            self.delete_abandoned_checkpoint_prefix(&binding, publication.revision.checkpoint_id)
                .await?;
            operations
                .mark_unknown(binding.tenant_id, operation_id)
                .await?;
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        if post_commit_state != WorkspacePostCommitState::AttachmentRetained {
            let key = session_provider_key(session, Some(&lease_scope), provider_name);
            self.remove_cached_binding_if_matches(&key, hand, Some(lease.generation))
                .await;
            self.remove_installed_marker(
                manifest_scope_key(session, Some(&lease_scope)),
                provider_name,
            )
            .await;
        }
        Ok(())
    }

    async fn delete_abandoned_checkpoint_prefix(
        &self,
        binding: &WorkspaceBinding,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<()> {
        let store = self.hands.checkpoint_store.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "checkpoint CAS cleanup requires the durable checkpoint store".to_string(),
            )
        })?;
        store
            .delete(
                crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                    tenant_id: binding.tenant_id,
                    workspace_id: binding.workspace_id,
                    checkpoint_id,
                    provider_account_id: binding.provider_account_id,
                    provider_account_generation: binding.provider_account_generation,
                },
            )
            .await
    }
}

fn revision_from_checkpoint_parent(
    generation: i64,
    checkpoint_id: Option<WorkspaceCheckpointId>,
) -> Result<Option<moa_core::types::sandbox_workspace::WorkspaceRevisionRef>> {
    match (generation, checkpoint_id) {
        (0, None) => Ok(None),
        (generation, Some(checkpoint_id)) if generation > 0 => Ok(Some(
            moa_core::types::sandbox_workspace::WorkspaceRevisionRef {
                checkpoint_id,
                generation: u64::try_from(generation).map_err(|_| {
                    MoaError::StorageError("workspace checkpoint generation is invalid".to_string())
                })?,
                format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
            },
        )),
        _ => Err(MoaError::StorageError(
            "workspace checkpoint parent is inconsistent with its generation".to_string(),
        )),
    }
}

fn management_checkpoint_request_hash(
    binding: &WorkspaceBinding,
    operation_id: WorkspaceOperationId,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(binding, operation_id, WorkspaceOperationKind::Checkpoint))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

pub(in crate::core) fn validate_managed_restore_target(
    current_checkpoint_id: Option<WorkspaceCheckpointId>,
    current_generation: i64,
    requested_checkpoint_id: WorkspaceCheckpointId,
    checkpoint_id: WorkspaceCheckpointId,
    checkpoint_generation: i64,
    checkpoint_state: WorkspaceCheckpointState,
) -> Result<()> {
    if checkpoint_state != WorkspaceCheckpointState::Available
        || checkpoint_id != requested_checkpoint_id
        || current_checkpoint_id != Some(requested_checkpoint_id)
        || current_generation != checkpoint_generation
    {
        return Err(MoaError::ValidationError(
            "restore requires the exact available current workspace checkpoint".to_string(),
        ));
    }
    Ok(())
}

pub(in crate::core) fn lease_attachment(
    binding: &WorkspaceBinding,
) -> Result<HandLeaseWorkspaceAttachment> {
    HandLeaseWorkspaceAttachment::new(
        binding.workspace_id,
        i64::try_from(binding.writer_epoch).map_err(|_| {
            MoaError::ValidationError("workspace writer epoch overflows bigint".to_string())
        })?,
        i64::try_from(binding.instance_generation).map_err(|_| {
            MoaError::ValidationError("workspace instance generation overflows bigint".to_string())
        })?,
        binding
            .current_revision
            .as_ref()
            .map(|revision| revision.checkpoint_id),
    )
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use moa_core::{error::MoaError, types::resource::ResourceBudget};

    use super::{
        ExecutionReleaseStep, PersistedLeaseReleaseState, ToolCallScope,
        admit_execution_release_step, compensation_release_identity_is_verified,
    };

    #[test]
    fn expired_release_budget_allows_reconciliation_but_rejects_new_provider_io() {
        // Pins: retrying an exact release after its five-minute I/O window may return a durable
        // receipt or finalize verified absence, but it must not start fresh provider operations.
        let release_started_at = Utc::now() - Duration::minutes(6);
        let scope = ToolCallScope::unbounded().with_budget(ResourceBudget::until(
            release_started_at + Duration::minutes(5),
        ));

        assert!(
            admit_execution_release_step(scope, ExecutionReleaseStep::DurableReconciliation)
                .is_ok()
        );
        assert!(matches!(
            admit_execution_release_step(scope, ExecutionReleaseStep::ProviderIo),
            Err(MoaError::BudgetExhausted(_))
        ));
    }

    #[test]
    fn persisted_compensation_lease_identity_requires_the_exact_destroyed_row() {
        // Pins: after provider teardown, a persisted op/generation may finalize only against its
        // exact Destroyed row; a missing row is not interchangeable with an attempt that proved
        // it never acquired a hand.
        assert!(compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::Destroyed,
        ));
        assert!(!compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::Missing,
        ));
        assert!(!compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::LiveOrAmbiguous,
        ));
        assert!(compensation_release_identity_is_verified(
            false,
            PersistedLeaseReleaseState::Missing,
        ));
    }
}

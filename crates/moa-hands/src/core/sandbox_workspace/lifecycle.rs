//! Managed durable sandbox-workspace lifecycle and commit barriers.

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        hands::HandHandle,
        identifiers::{
            SandboxWorkspaceId, ToolCallId, WorkspaceCheckpointId, WorkspaceOperationId,
        },
        sandbox_workspace::{
            ProviderStorageKind, ProviderStorageRef, SandboxWorkspaceScope, SandboxWorkspaceState,
            WorkspaceAttachRequest, WorkspaceBinding, WorkspaceCheckpointPublishRequest,
            WorkspaceCheckpointState, WorkspaceConfirmedDisposition, WorkspaceOperationKind,
            WorkspaceOperationOutcome, WorkspacePostCommitState, WorkspaceReconcileRequest,
            WorkspaceRestoreRequest, WorkspaceStorageOperation, WorkspaceStoragePrepareRequest,
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
    model::{SandboxWorkspace, WorkspaceTransition, WorkspaceWriterClaim},
    operations::WorkspaceOperationIntent,
};
use crate::core::{
    ActiveHand, HandRoute, InstalledManifestMarker, JournaledWorkspaceCommit, ToolCallScope,
    ToolExecution, ToolRouter, TrustedSandboxManifest,
    leases::{HandLease, HandLeaseStatus, HandLeaseWorkspaceAttachment},
    lifecycle::{
        manifest_scope_key, session_provider_key, workspace_binding_for_hand, workspace_lease_scope,
    },
};

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
        match (result.outcome, result.confirmed_disposition) {
            (WorkspaceOperationOutcome::Confirmed, Some(disposition)) => {
                operations
                    .confirm_disposition(workspace.tenant_id, operation_id, disposition)
                    .await?;
                Ok(())
            }
            (WorkspaceOperationOutcome::Unknown, None) => {
                operations
                    .mark_unknown(workspace.tenant_id, operation_id)
                    .await?;
                Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                })
            }
            _ => Err(MoaError::ProviderError(
                "workspace storage provider returned an inconsistent preparation result"
                    .to_string(),
            )),
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
        cache_key: &str,
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
        self.commit_workspace_after_tool(
            request.session,
            workspace_scope,
            request.tool_call_id,
            &workspace.provider,
            &hand,
            request.scope,
        )
        .await
    }

    pub(in crate::core) async fn commit_workspace_after_tool(
        &self,
        session: &SessionMeta,
        workspace_scope: &SandboxWorkspaceScope,
        tool_call_id: ToolCallId,
        provider_name: &str,
        hand: &HandHandle,
        call_scope: ToolCallScope<'_>,
    ) -> Result<()> {
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

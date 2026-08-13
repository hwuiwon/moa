//! Durable sandbox-workspace commit publication.

use super::*;

impl ToolRouter {
    pub(super) async fn confirmed_workspace_commit_replay(
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
}

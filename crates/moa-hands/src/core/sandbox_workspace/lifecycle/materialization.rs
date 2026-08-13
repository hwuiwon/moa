//! Initial sandbox-workspace storage materialization and hand hydration.

use super::*;

impl ToolRouter {
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
        let existing = operations.get(workspace.tenant_id, operation_id).await?;
        let deadline_at = existing.as_ref().map_or_else(
            || {
                call_scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5))
            },
            |operation| operation.deadline_at,
        );
        let reconcile_not_before = existing.as_ref().map_or_else(
            || deadline_at + ChronoDuration::seconds(30),
            |operation| operation.reconcile_not_before,
        );
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
            reconcile_not_before,
        };
        let operation = operations.persist_intent(&intent).await?;
        match (operation.outcome, operation.confirmed_disposition) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
            ) => return Ok(()),
            (WorkspaceOperationOutcome::Confirmed, _) => {
                return Err(MoaError::ProviderError(
                    "workspace storage preparation was durably confirmed absent".to_string(),
                ));
            }
            (WorkspaceOperationOutcome::Unknown, _) => {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            (WorkspaceOperationOutcome::NotSent, None) => {}
            _ => {
                return Err(MoaError::StorageError(
                    "workspace storage preparation has an inconsistent durable outcome".to_string(),
                ));
            }
        }
        failpoints::hit("post_reservation_pre_provider_create").await?;
        call_scope.admit()?;
        if !operations
            .begin_provider_attempt(workspace.tenant_id, operation_id)
            .await?
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
        let result = match storage_provider
            .prepare_workspace_storage(WorkspaceStoragePrepareRequest {
                operation: WorkspaceStorageOperation {
                    operation_id,
                    kind: WorkspaceOperationKind::Create,
                    binding,
                    deadline: deadline_at,
                    request_hash,
                },
            })
            .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    operation_id = %operation_id,
                    error = %error,
                    "workspace storage preparation outcome is ambiguous"
                );
                operations
                    .mark_unknown(workspace.tenant_id, operation_id)
                    .await?;
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
        };
        // Every arm records an outcome, so the lifecycle counter carries the real
        // success/ambiguous ratio rather than only the happy path.
        match (result.outcome, result.confirmed_disposition) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
            ) => {
                if !operations
                    .confirm_disposition(
                        workspace.tenant_id,
                        operation_id,
                        WorkspaceConfirmedDisposition::ResourcePresent,
                    )
                    .await?
                {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: operation_id.to_string(),
                    });
                }
                record_workspace_lifecycle(
                    &workspace.provider,
                    SandboxWorkspaceLifecycleOperation::Create,
                    SandboxWorkspaceMetricResult::Succeeded,
                    prepare_started_at.elapsed(),
                );
                Ok(())
            }
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourceAbsent),
            ) => {
                if !operations
                    .confirm_disposition(
                        workspace.tenant_id,
                        operation_id,
                        WorkspaceConfirmedDisposition::ResourceAbsent,
                    )
                    .await?
                {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: operation_id.to_string(),
                    });
                }
                record_workspace_lifecycle(
                    &workspace.provider,
                    SandboxWorkspaceLifecycleOperation::Create,
                    SandboxWorkspaceMetricResult::Failed,
                    prepare_started_at.elapsed(),
                );
                Err(MoaError::ProviderError(
                    "workspace storage preparation was confirmed absent".to_string(),
                ))
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
        let persisted = operations.persist_intent(&intent).await?;
        match (persisted.outcome, persisted.confirmed_disposition) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
            ) => return Ok(()),
            (WorkspaceOperationOutcome::Confirmed, _) => {
                return Err(MoaError::ProviderError(format!(
                    "workspace {} was durably confirmed absent",
                    kind.as_str()
                )));
            }
            (WorkspaceOperationOutcome::Unknown, _) => {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_id.to_string(),
                });
            }
            (WorkspaceOperationOutcome::NotSent, None) => {}
            _ => {
                return Err(MoaError::StorageError(format!(
                    "workspace {} has an inconsistent durable outcome",
                    kind.as_str()
                )));
            }
        }
        call_scope.admit()?;
        if !operations
            .begin_provider_attempt(binding.tenant_id, operation_id)
            .await?
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_id.to_string(),
            });
        }
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
                    "workspace hydration provider outcome is ambiguous"
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
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
            ) => {
                if !operations
                    .confirm_disposition(
                        binding.tenant_id,
                        operation_id,
                        WorkspaceConfirmedDisposition::ResourcePresent,
                    )
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
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourceAbsent),
            ) => {
                if !operations
                    .confirm_disposition(
                        binding.tenant_id,
                        operation_id,
                        WorkspaceConfirmedDisposition::ResourceAbsent,
                    )
                    .await?
                {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: operation_id.to_string(),
                    });
                }
                Err(MoaError::ProviderError(format!(
                    "workspace {} was confirmed absent",
                    kind.as_str()
                )))
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
}

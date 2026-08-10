//! Persistent sandbox-workspace storage behavior for the local provider.

use super::*;

impl LocalHandProvider {
    fn checkpoint_store(&self) -> Result<&CheckpointObjectStore> {
        self.checkpoint_store.as_deref().ok_or_else(|| {
            MoaError::ConfigError(
                "local persistent workspaces require configured checkpoint object storage"
                    .to_string(),
            )
        })
    }

    async fn workspace_data_root(&self, handle: &HandHandle) -> Result<PathBuf> {
        match handle {
            HandHandle::Local { sandbox_dir } => {
                let sandbox = self.resolve_local_sandbox(sandbox_dir).await;
                Ok(sandbox.execution_root)
            }
            HandHandle::Docker { container_id } => self
                .docker_sandboxes
                .read()
                .await
                .get(container_id)
                .map(|sandbox| sandbox.sandbox_dir.clone())
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "unknown Docker sandbox handle: {container_id}"
                    ))
                }),
            _ => Err(MoaError::Unsupported(
                "non-local hand handle passed to local workspace storage".to_string(),
            )),
        }
    }

    fn checkpoint_context(
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> CheckpointStoreContext {
        CheckpointStoreContext {
            tenant_id: operation.binding.tenant_id,
            workspace_id: operation.binding.workspace_id,
            checkpoint_id,
            provider_account_id: operation.binding.provider_account_id,
            provider_account_generation: operation.binding.provider_account_generation,
        }
    }

    async fn create_portable_checkpoint(
        &self,
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
        hand: &HandHandle,
        parent_revision: Option<&WorkspaceRevisionRef>,
    ) -> Result<WorkspaceStorageOperationResult> {
        if Utc::now() >= operation.deadline {
            return Err(MoaError::ProviderTimeout(
                "workspace checkpoint deadline elapsed before local archive creation".to_string(),
            ));
        }
        let checkpoint_id = WorkspaceCheckpointId(operation.operation_id.0);
        let revision = next_workspace_revision(operation, parent_revision, checkpoint_id, None)?;
        let root = self.workspace_data_root(hand).await?;
        let store = self.checkpoint_store()?;
        let archive = build_checkpoint_archive(
            &root,
            crate::core::sandbox_workspace::checkpoint::archive::ArchiveLimits::default(),
        )
        .await?;
        let published = store
            .publish(Self::checkpoint_context(operation, checkpoint_id), archive)
            .await?;
        let checkpoint_publication = WorkspaceCheckpointPublication {
            revision,
            storage: published.storage.clone(),
            manifest_digest: published.manifest_sha256,
            logical_bytes: published.logical_bytes,
        };
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(published.storage),
            checkpoint_publication: Some(checkpoint_publication),
            post_commit_state: (operation.kind == WorkspaceOperationKind::Commit)
                .then_some(WorkspacePostCommitState::AttachmentRetained),
        })
    }
}

#[async_trait]
impl SandboxStorageProvider for LocalHandProvider {
    fn storage_provider_name(&self) -> &str {
        "local"
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderAccountStorageInventory> {
        let local_resources = self
            .local_sandboxes
            .read()
            .await
            .iter()
            .filter_map(|(path, sandbox)| {
                sandbox
                    .inventory_identity
                    .as_ref()
                    .filter(|identity| {
                        identity.provider_account_id == provider_account_id
                            && identity.provider_account_generation == provider_account_generation
                    })
                    .map(|identity| (path.to_string_lossy().into_owned(), identity.owner.clone()))
            })
            .collect::<Vec<_>>();
        let docker_resources = self
            .docker_sandboxes
            .read()
            .await
            .iter()
            .filter_map(|(container_id, sandbox)| {
                sandbox
                    .inventory_identity
                    .as_ref()
                    .filter(|identity| {
                        identity.provider_account_id == provider_account_id
                            && identity.provider_account_generation == provider_account_generation
                    })
                    .map(|identity| (container_id.clone(), identity.owner.clone()))
            })
            .collect::<Vec<_>>();
        let mut resources = local_resources
            .into_iter()
            .chain(docker_resources)
            .map(|(reference, owner)| ProviderInventoryResource {
                kind: ProviderInventoryResourceKind::Compute,
                resource_fingerprint: format!(
                    "sha256:{:x}",
                    Sha256::digest(format!("local-sandbox-v1\0{reference}").as_bytes())
                ),
                evidence_digest: format!(
                    "sha256:{:x}",
                    Sha256::digest(format!("local-inventory-v1\0{reference}").as_bytes())
                ),
                provider_reference: reference,
                // This identity is captured from the exact fenced HandSpec and
                // persisted in the durable lease payload before adoption.
                verified_owner: Some(owner),
            })
            .collect::<Vec<_>>();
        resources.sort_by(|left, right| left.resource_fingerprint.cmp(&right.resource_fingerprint));
        Ok(ProviderAccountStorageInventory {
            provider_account_id,
            provider_account_generation,
            observed_at: Utc::now(),
            resources,
        })
    }

    async fn prepare_workspace_storage(
        &self,
        request: WorkspaceStoragePrepareRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        let storage = ProviderStorageRef {
            provider_account_id: request.operation.binding.provider_account_id,
            provider_account_generation: request.operation.binding.provider_account_generation,
            kind: ProviderStorageKind::MutableFilesystem,
            resource_id: local_mutable_storage_id(&request.operation.binding),
            workspace_locator: None,
        };
        Ok(confirmed_storage_result(Some(storage)))
    }

    async fn attach_workspace(
        &self,
        request: WorkspaceAttachRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        let _ = self.workspace_data_root(&request.hand).await?;
        let storage = request.storage.unwrap_or_else(|| ProviderStorageRef {
            provider_account_id: request.operation.binding.provider_account_id,
            provider_account_generation: request.operation.binding.provider_account_generation,
            kind: ProviderStorageKind::MutableFilesystem,
            resource_id: local_mutable_storage_id(&request.operation.binding),
            workspace_locator: None,
        });
        if storage.provider_account_id != request.operation.binding.provider_account_id
            || storage.provider_account_generation
                != request.operation.binding.provider_account_generation
            || storage.kind != ProviderStorageKind::MutableFilesystem
            || storage.resource_id != local_mutable_storage_id(&request.operation.binding)
        {
            return Err(MoaError::ValidationError(
                "local mutable storage reference does not match workspace binding".to_string(),
            ));
        }
        Ok(confirmed_storage_result(Some(storage)))
    }

    async fn publish_workspace_checkpoint(
        &self,
        request: WorkspaceCheckpointPublishRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        self.create_portable_checkpoint(
            &request.operation,
            &request.hand,
            request.parent_revision.as_ref(),
        )
        .await
    }

    async fn restore_workspace(
        &self,
        request: WorkspaceRestoreRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        let store = self.checkpoint_store()?;
        let context = Self::checkpoint_context(&request.operation, request.revision.checkpoint_id);
        if request.revision.format_version
            != crate::core::sandbox_workspace::checkpoint::archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION
            || !store.reference_matches(context, &request.checkpoint)
        {
            return Err(MoaError::ValidationError(
                "portable checkpoint reference does not match restore fences".to_string(),
            ));
        }
        let root = self.workspace_data_root(&request.hand).await?;
        let staging = root.with_extension(format!("verified-restore-{}", Uuid::new_v4()));
        store.restore(context, &staging).await?;
        promote_into_empty_compute_root(&staging, &root).await?;
        Ok(confirmed_storage_result(Some(request.checkpoint)))
    }

    async fn delete_workspace_storage(
        &self,
        request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        match request.storage.kind {
            ProviderStorageKind::PortableCheckpoint => {
                let context = Self::checkpoint_context(
                    &request.operation,
                    required_current_revision(&request.operation, "local")?.checkpoint_id,
                );
                let store = self.checkpoint_store()?;
                if !store.reference_matches(context, &request.storage) {
                    return Err(MoaError::ValidationError(
                        "portable checkpoint delete reference does not match workspace fences"
                            .to_string(),
                    ));
                }
                store.delete(context).await?;
            }
            ProviderStorageKind::MutableFilesystem => {
                if request.storage.provider_account_id
                    != request.operation.binding.provider_account_id
                    || request.storage.provider_account_generation
                        != request.operation.binding.provider_account_generation
                    || request.storage.resource_id
                        != local_mutable_storage_id(&request.operation.binding)
                {
                    return Err(MoaError::ValidationError(
                        "local mutable delete reference does not match workspace fences"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourceAbsent),
            storage: None,
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn delete_tenant_storage_resource(
        &self,
        request: TenantStoragePurgeRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        if request.purge_operation_id.trim().is_empty()
            || request.storage.provider_account_id != request.provider_account_id
            || request.storage.provider_account_generation != request.provider_account_generation
            || request.storage.kind != ProviderStorageKind::MutableFilesystem
        {
            return Err(MoaError::ValidationError(
                "local tenant purge storage reference lost its exact account fence".to_string(),
            ));
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourceAbsent),
            storage: None,
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn reconcile_workspace_operation(
        &self,
        request: WorkspaceReconcileRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        request.validate()?;
        let storage = request.storage().cloned();
        if let Some(storage) = storage.as_ref()
            && storage.kind == ProviderStorageKind::PortableCheckpoint
        {
            let checkpoint_id = match request.operation().kind {
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint => {
                    WorkspaceCheckpointId(request.operation().operation_id.0)
                }
                WorkspaceOperationKind::Restore | WorkspaceOperationKind::Delete => {
                    required_current_revision(request.operation(), "local")?.checkpoint_id
                }
                WorkspaceOperationKind::Create | WorkspaceOperationKind::Attach => {
                    return Err(MoaError::ValidationError(
                        "local create/attach reconciliation cannot name portable checkpoint storage"
                            .to_string(),
                    ));
                }
            };
            let context = Self::checkpoint_context(request.operation(), checkpoint_id);
            let published = self
                .checkpoint_store()?
                .inspect_publication(context, storage)
                .await?;
            return match (request.operation().kind, published) {
                (
                    WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint,
                    Some(published),
                ) => Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Confirmed,
                    confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
                    storage: Some(storage.clone()),
                    checkpoint_publication: Some(WorkspaceCheckpointPublication {
                        revision: next_workspace_revision(
                            request.operation(),
                            request.operation().binding.current_revision.as_ref(),
                            checkpoint_id,
                            None,
                        )?,
                        storage: published.storage,
                        manifest_digest: published.manifest_sha256,
                        logical_bytes: published.logical_bytes,
                    }),
                    post_commit_state: Some(WorkspacePostCommitState::AttachmentRetained),
                }),
                (WorkspaceOperationKind::Delete, None) => Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Confirmed,
                    confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourceAbsent),
                    storage: None,
                    checkpoint_publication: None,
                    post_commit_state: None,
                }),
                (_, published) => Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: published.map(|_| storage.clone()),
                    checkpoint_publication: None,
                    post_commit_state: None,
                }),
            };
        }
        if let Some(storage) = storage.as_ref()
            && !self.verify_workspace_storage(storage).await?
        {
            return Err(MoaError::ValidationError(
                "local reconciliation storage does not match a supported exact resource"
                    .to_string(),
            ));
        }
        let disposition = if request.operation().kind == WorkspaceOperationKind::Delete {
            WorkspaceConfirmedDisposition::ResourceAbsent
        } else {
            WorkspaceConfirmedDisposition::ResourcePresent
        };
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(disposition),
            storage,
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn verify_workspace_storage(&self, storage: &ProviderStorageRef) -> Result<bool> {
        match storage.kind {
            ProviderStorageKind::PortableCheckpoint => {
                self.checkpoint_store()?.verify_reference(storage).await
            }
            ProviderStorageKind::MutableFilesystem => Ok(storage.workspace_locator.is_none()
                && storage.resource_id.len() == 64
                && storage
                    .resource_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())),
        }
    }
}

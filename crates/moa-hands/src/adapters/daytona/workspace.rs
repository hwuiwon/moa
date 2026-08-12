//! Persistent sandbox-workspace storage behavior for the Daytona provider.

use super::*;

impl DaytonaHandProvider {
    fn storage_dependencies(&self) -> Result<&storage::DaytonaStorageDependencies> {
        self.storage.as_deref().ok_or_else(|| {
            MoaError::ConfigError(
                "Daytona persistent workspace operation has no durable storage owners".to_string(),
            )
        })
    }

    async fn exact_mutable_storage_resource(
        &self,
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
        storage: &ProviderStorageRef,
    ) -> Result<crate::core::sandbox_workspace::storage_resources::StorageResource> {
        use moa_core::types::sandbox_workspace::ProviderStorageKind;

        verify_request_resources(operation, None, Some(storage))?;
        if storage.kind != ProviderStorageKind::MutableFilesystem {
            return Err(MoaError::ValidationError(
                "Daytona mutable-storage operation requires a tenant-volume reference".to_string(),
            ));
        }
        let generation = i64::try_from(storage.provider_account_generation).map_err(|_| {
            MoaError::ValidationError("Daytona account generation overflows bigint".to_string())
        })?;
        self.storage_dependencies()?
            .storage_resources
            .by_provider_reference(
                operation.binding.tenant_id,
                storage.provider_account_id,
                generation,
                &storage.resource_id,
            )
            .await?
            .ok_or_else(|| {
                MoaError::ValidationError(
                    "Daytona storage reference has no exact tenant-owned resource row".to_string(),
                )
            })
    }

    async fn cleanup_workspace_subpath(
        &self,
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
        storage_ref: &ProviderStorageRef,
    ) -> Result<bool> {
        let locator = storage_ref.workspace_locator.as_deref().ok_or_else(|| {
            MoaError::ValidationError(
                "Daytona workspace cleanup requires an exact opaque subpath".to_string(),
            )
        })?;
        storage::validate_workspace_subpath(locator)?;
        let attempt = self
            .attempt(
                storage_ref.provider_account_id,
                storage_ref.provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        let identity = DaytonaProvisioningIdentity {
            sandbox_name: format!("moa-cleanup-{}", operation.operation_id),
            operation_id: operation.operation_id.to_string(),
            spec_fingerprint: Some(operation.request_hash.clone()),
        };
        let workspace_id = if let Some(existing) =
            self.resolve_workspace(&attempt, &identity).await?
        {
            existing
        } else {
            let image = attempt
                .default_runtime()
                .unwrap_or(DEFAULT_DAYTONA_IMAGE)
                .to_string();
            let body = json!({
                "name": identity.sandbox_name,
                "image": image,
                "env": {},
                "autoStopInterval": 5,
                "labels": {
                    (PROVISIONING_OPERATION_LABEL): identity.operation_id.as_str(),
                    (PROVISIONING_SPEC_LABEL): operation.request_hash.as_str(),
                    (WORKSPACE_ID_LABEL): operation.binding.workspace_id.to_string(),
                    (TENANT_OWNER_LABEL): opaque_tenant_owner(&operation.binding),
                    (WRITER_EPOCH_LABEL): operation.binding.writer_epoch.to_string(),
                    (INSTANCE_GENERATION_LABEL): operation.binding.instance_generation.to_string(),
                },
                "volumes": [volume::DaytonaSandboxVolumeMount {
                    volume_id: &storage_ref.resource_id,
                    mount_path: "/workspace",
                    subpath: locator,
                }],
            });
            let create = attempt
                .client()
                .post(format!("{}/api/sandbox", attempt.origin()))
                .bearer_auth(attempt.credential())
                .json(&body)
                .send()
                .await;
            let (expected_id, create_error) = match create {
                Ok(response) if response.status().is_success() => {
                    let value = expect_success_json(response, "Daytona cleanup sandbox").await?;
                    verify_created_workspace_identity(&value, &identity)?;
                    (Some(extract_workspace_id(&value)?), None)
                }
                Ok(response) => (None, Some(http_error(response).await)),
                Err(error) => (
                    None,
                    Some(MoaError::ProviderTransport(format!(
                        "create Daytona cleanup sandbox: {error}"
                    ))),
                ),
            };
            match self
                .resolve_workspace_with_retries(&attempt, &identity, expected_id.as_deref())
                .await?
            {
                Some(workspace_id) => workspace_id,
                None => {
                    return Err(create_error.unwrap_or_else(|| {
                        MoaError::ProviderError(
                            "Daytona cleanup sandbox create outcome could not be reconciled"
                                .to_string(),
                        )
                    }));
                }
            }
        };
        let handle = HandHandle::daytona(
            workspace_id.clone(),
            storage_ref.provider_account_id,
            storage_ref.provider_account_generation,
        );
        let cleanup_result = async {
            self.resume(&handle).await?;
            let toolbox_attempt = self
                .attempt(
                    storage_ref.provider_account_id,
                    storage_ref.provider_account_generation,
                    ProviderEndpoint::Toolbox,
                )
                .await?;
            let cleanup = self
                .execute_command(
                    &toolbox_attempt,
                    &workspace_id,
                    "find /workspace -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +",
                    Some("/workspace"),
                    Some(DEFAULT_COMMAND_TIMEOUT),
                )
                .await?;
            if cleanup.is_error {
                return Err(MoaError::ProviderError(
                    "Daytona cleanup sandbox could not purge its fenced subpath".to_string(),
                ));
            }
            let consistency = Duration::from_secs(
                self.storage_dependencies()?
                    .config
                    .consistency_window_seconds
                    .max(1),
            );
            for observation in 0..2 {
                if !storage::workspace_root_is_empty(&toolbox_attempt, &workspace_id, "/workspace")
                    .await?
                {
                    return Ok(false);
                }
                if observation == 0 {
                    sleep(consistency).await;
                }
            }
            Ok(true)
        }
        .await;
        let destroy_result = self.destroy(&handle).await;
        match (cleanup_result, destroy_result) {
            (Ok(empty), Ok(())) => Ok(empty),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    /// Deletes a whole tenant-dedicated volume during the tenant-purge lane.
    ///
    /// Ordinary workspace deletion must use [`SandboxStorageProvider`] cleanup
    /// so it removes only the workspace's opaque subpath. The purge owner may
    /// call this method only after proving every tenant sandbox detached.
    pub async fn delete_tenant_volume_for_purge(
        &self,
        request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use moa_core::types::sandbox_workspace::{
            ProviderStorageKind, WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };

        verify_request_resources(&request.operation, None, Some(&request.storage))?;
        if request.storage.kind != ProviderStorageKind::MutableFilesystem
            || request.storage.workspace_locator.is_some()
        {
            return Err(MoaError::ValidationError(
                "Daytona tenant purge requires a whole-volume reference without a workspace subpath"
                    .to_string(),
            ));
        }
        let dependencies = self.storage_dependencies()?;
        let resource = self
            .exact_mutable_storage_resource(&request.operation, &request.storage)
            .await?;
        if !dependencies
            .storage_resources
            .begin_delete(
                resource.tenant_id,
                resource.storage_resource_id,
                resource.generation,
                request.operation.operation_id,
            )
            .await?
            && resource.deletion_operation_id != Some(request.operation.operation_id)
        {
            return Err(MoaError::StorageError(
                "Daytona tenant-volume purge lost its exact operation/resource fence".to_string(),
            ));
        }
        let attempt = self
            .attempt(
                request.storage.provider_account_id,
                request.storage.provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        match volume::delete_volume(&attempt, &request.storage.resource_id).await? {
            volume::DaytonaVolumeDeleteOutcome::MountedConflict => {
                return Err(MoaError::HttpStatus {
                    status: reqwest::StatusCode::CONFLICT.as_u16(),
                    retry_after: None,
                    message: "Daytona tenant volume remains mounted by a sandbox".to_string(),
                });
            }
            volume::DaytonaVolumeDeleteOutcome::Accepted
            | volume::DaytonaVolumeDeleteOutcome::Absent => {}
        }
        let consistency =
            Duration::from_secs(dependencies.config.consistency_window_seconds.max(1));
        for observation in 0..2 {
            if volume::get_volume(&attempt, &request.storage.resource_id)
                .await?
                .is_some()
            {
                return Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: Some(request.storage),
                    checkpoint_publication: None,
                    post_commit_state: None,
                });
            }
            if observation == 0 {
                sleep(consistency).await;
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

    async fn create_portable_checkpoint(
        &self,
        operation: &moa_core::types::sandbox_workspace::WorkspaceStorageOperation,
        hand: &HandHandle,
        parent_revision: Option<&WorkspaceRevisionRef>,
        release_compute: bool,
    ) -> Result<WorkspaceStorageOperationResult> {
        use crate::core::sandbox_workspace::checkpoint::{
            archive::build_checkpoint_archive, store::CheckpointStoreContext,
        };
        use moa_core::types::sandbox_workspace::{
            WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };

        verify_request_resources(operation, Some(hand), None)?;
        let checkpoint_id = WorkspaceCheckpointId(operation.operation_id.0);
        let revision =
            next_workspace_revision(operation, parent_revision, checkpoint_id, Some("Daytona"))?;
        let dependencies = self.storage_dependencies()?;
        let sandbox_id = hand.daytona_id()?;
        let (account_id, account_generation) = cloud_account(hand, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Toolbox)
            .await?;
        let remaining = (operation.deadline - chrono::Utc::now())
            .to_std()
            .map_err(|_| {
                MoaError::ProviderTimeout(
                    "Daytona checkpoint deadline elapsed before quiesce".to_string(),
                )
            })?;
        let sync = self
            .execute_command(
                &attempt,
                sandbox_id,
                "sync",
                Some("/workspace"),
                Some(remaining),
            )
            .await?;
        if sync.is_error {
            return Err(MoaError::ProviderError(
                "Daytona workspace sync failed before checkpoint publication".to_string(),
            ));
        }
        let claims = storage::DaytonaWorkspaceMarkerClaims {
            operation_id: operation.operation_id,
            request_hash: operation.request_hash.clone(),
            spec_hash: operation.request_hash.clone(),
            writer_epoch: operation.binding.writer_epoch,
            instance_generation: operation.binding.instance_generation,
        };
        let marker =
            storage::seal_workspace_marker(dependencies.kms.as_ref(), operation, &claims).await?;
        storage::upload_workspace_marker(&attempt, sandbox_id, "/workspace", &marker).await?;
        let downloaded =
            storage::download_workspace_marker(&attempt, sandbox_id, "/workspace").await?;
        storage::open_workspace_marker(dependencies.kms.as_ref(), operation, &claims, &downloaded)
            .await?;

        let limits = dependencies.checkpoint_store.archive_limits();
        let staging = storage::materialize_workspace_for_checkpoint(
            &attempt,
            sandbox_id,
            "/workspace",
            limits,
        )
        .await?;
        let archive = build_checkpoint_archive(staging.path(), limits).await?;
        let logical_bytes = archive.manifest.logical_bytes;
        dependencies
            .capacity
            .reserve_checkpoint_publication(operation, logical_bytes)
            .await?;
        let published = dependencies
            .checkpoint_store
            .publish(
                CheckpointStoreContext {
                    tenant_id: operation.binding.tenant_id,
                    workspace_id: operation.binding.workspace_id,
                    checkpoint_id,
                    provider_account_id: operation.binding.provider_account_id,
                    provider_account_generation: operation.binding.provider_account_generation,
                },
                archive,
            )
            .await?;
        let checkpoint_publication = WorkspaceCheckpointPublication {
            revision,
            storage: published.storage.clone(),
            manifest_digest: published.manifest_sha256,
            logical_bytes: published.logical_bytes,
        };
        if release_compute {
            <Self as HandProvider>::destroy(self, hand).await?;
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(published.storage),
            checkpoint_publication: Some(checkpoint_publication),
            post_commit_state: (operation.kind
                == moa_core::types::sandbox_workspace::WorkspaceOperationKind::Commit)
                .then_some(if release_compute {
                    WorkspacePostCommitState::ComputeDestroyed
                } else {
                    WorkspacePostCommitState::AttachmentRetained
                }),
        })
    }
}

#[async_trait]
impl SandboxStorageProvider for DaytonaHandProvider {
    fn storage_provider_name(&self) -> &str {
        "daytona"
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderAccountStorageInventory> {
        let attempt = self
            .attempt(
                provider_account_id,
                provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        let mut resources = volume::list_volumes(&attempt)
            .await?
            .into_iter()
            .filter(|volume| volume.state != volume::DaytonaVolumeState::Deleted)
            .map(|volume| {
                let evidence = format!(
                    "daytona-volume-v1\0{}\0{}\0{}\0{:?}",
                    volume.id, volume.name, volume.organization_id, volume.state
                );
                ProviderInventoryResource {
                    kind: ProviderInventoryResourceKind::MutableFilesystem,
                    provider_reference: volume.id,
                    resource_fingerprint: format!(
                        "sha256:{:x}",
                        Sha256::digest(format!("daytona-volume-v1\0{}", volume.name).as_bytes())
                    ),
                    evidence_digest: format!("sha256:{:x}", Sha256::digest(evidence.as_bytes())),
                    // Daytona volume inventory authenticates organization
                    // ownership but carries no workspace-level metadata. The
                    // coordinator matches the exact provider reference and
                    // deterministic name to the durable storage row.
                    verified_owner: None,
                }
            })
            .collect::<Vec<_>>();
        resources.sort_by(|left, right| left.resource_fingerprint.cmp(&right.resource_fingerprint));
        Ok(ProviderAccountStorageInventory {
            provider_account_id,
            provider_account_generation,
            observed_at: chrono::Utc::now(),
            resources,
        })
    }

    async fn prepare_workspace_storage(
        &self,
        request: WorkspaceStoragePrepareRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use crate::core::sandbox_workspace::storage_resources::{
            StorageResourceCreateIntent, StorageResourceState,
        };
        use moa_core::types::sandbox_workspace::{
            WorkspaceConfirmedDisposition, WorkspaceOperationKind, WorkspaceOperationOutcome,
        };

        if request.operation.kind != WorkspaceOperationKind::Create
            || chrono::Utc::now() >= request.operation.deadline
        {
            return Err(MoaError::ValidationError(
                "Daytona storage preparation requires a live exact create operation".to_string(),
            ));
        }
        let dependencies = self.storage_dependencies()?;
        let binding = &request.operation.binding;
        let account = dependencies
            .config
            .account(binding.provider_account_id)
            .ok_or_else(|| {
                MoaError::ConfigError(
                    "Daytona workspace account has no configured volume security class".to_string(),
                )
            })?;
        let generation = i64::try_from(binding.provider_account_generation).map_err(|_| {
            MoaError::ValidationError("Daytona account generation overflows bigint".to_string())
        })?;
        let deterministic_name = tenant_volume_name(
            binding.tenant_id,
            binding.provider_account_id,
            &account.security_class,
        );
        let storage_resource_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("moa:daytona:tenant-volume:{deterministic_name}").as_bytes(),
        );
        if let Some(existing) = dependencies
            .storage_resources
            .live_tenant_volume(
                binding.tenant_id,
                binding.provider_account_id,
                generation,
                &account.security_class,
            )
            .await?
        {
            if !matches!(
                existing.state,
                StorageResourceState::Ready | StorageResourceState::Attached
            ) {
                return Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: None,
                    checkpoint_publication: None,
                    post_commit_state: None,
                });
            }
            let provider_reference = existing.provider_reference.ok_or_else(|| {
                MoaError::StorageError(
                    "ready Daytona tenant volume is missing its provider id".to_string(),
                )
            })?;
            return Ok(WorkspaceStorageOperationResult {
                outcome: WorkspaceOperationOutcome::Confirmed,
                confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
                storage: Some(storage::mutable_storage_reference(
                    &request.operation,
                    provider_reference,
                )?),
                checkpoint_publication: None,
                post_commit_state: None,
            });
        }
        let owner_fingerprint = opaque_tenant_owner(binding);
        let resource = dependencies
            .storage_resources
            .persist_create_intent(&StorageResourceCreateIntent {
                storage_resource_id,
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                create_operation_id: request.operation.operation_id,
                provider_account_id: binding.provider_account_id,
                provider_account_generation: generation,
                security_class: account.security_class.clone(),
                deterministic_name: deterministic_name.clone(),
                verified_owner_fingerprint: owner_fingerprint,
            })
            .await?;

        let attempt = self
            .attempt(
                binding.provider_account_id,
                binding.provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        let observed = volume::list_volumes(&attempt).await?;
        let capacity_request =
            crate::core::sandbox_workspace::capacity::CapacityReservationRequest {
                tenant_id: binding.tenant_id,
                workspace_id: binding.workspace_id,
                operation_id: request.operation.operation_id,
                provider_account_id: binding.provider_account_id,
                provider_account_generation: generation,
                expected_writer_epoch: i64::try_from(binding.writer_epoch).map_err(|_| {
                    MoaError::ValidationError("Daytona writer epoch overflows bigint".to_string())
                })?,
                expected_instance_generation: i64::try_from(binding.instance_generation).map_err(
                    |_| {
                        MoaError::ValidationError(
                            "Daytona instance generation overflows bigint".to_string(),
                        )
                    },
                )?,
                quantities: vec![crate::core::sandbox_workspace::capacity::CapacityQuantity {
                    dimension:
                        moa_core::types::sandbox_workspace::WorkspaceCapacityDimension::Volumes,
                    quantity: 1,
                }],
            };
        dependencies
            .capacity
            .reserve_lifetime_volume(
                &capacity_request,
                storage_resource_id,
                account.volume_ceiling,
                account.admission_headroom,
                &observed
                    .iter()
                    .filter(|volume| volume.state != volume::DaytonaVolumeState::Deleted)
                    .map(|volume| volume.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        let resolved = if let Some(provider_reference) = resource.provider_reference.as_deref() {
            volume::get_volume(&attempt, provider_reference).await?
        } else {
            volume::get_volume_by_name(&attempt, &deterministic_name).await?
        };
        let mut volume = match resolved {
            Some(volume) => volume,
            None if matches!(resource.state, StorageResourceState::Creating) => {
                enforce_volume_headroom(
                    observed.len(),
                    usize::from(account.volume_ceiling),
                    usize::from(account.admission_headroom),
                )?;
                match volume::create_volume(&attempt, &deterministic_name).await {
                    Ok(volume) => volume,
                    Err(error) => {
                        dependencies
                            .storage_resources
                            .mark_unknown(
                                binding.tenant_id,
                                storage_resource_id,
                                resource.generation,
                            )
                            .await?;
                        dependencies
                            .capacity
                            .mark_lifetime_volume_reconciling(
                                &capacity_request,
                                storage_resource_id,
                            )
                            .await?;
                        return Err(error);
                    }
                }
            }
            None => {
                dependencies
                    .storage_resources
                    .mark_unknown(binding.tenant_id, storage_resource_id, resource.generation)
                    .await?;
                return Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: None,
                    checkpoint_publication: None,
                    post_commit_state: None,
                });
            }
        };
        let poll_interval =
            Duration::from_secs(dependencies.config.consistency_window_seconds.max(1));
        loop {
            if volume.name != deterministic_name {
                dependencies
                    .storage_resources
                    .mark_unknown(binding.tenant_id, storage_resource_id, resource.generation)
                    .await?;
                return Err(MoaError::ProviderError(
                    "Daytona tenant volume resolved under a different deterministic name"
                        .to_string(),
                ));
            }
            match volume.state {
                volume::DaytonaVolumeState::Ready => break,
                volume::DaytonaVolumeState::Creating
                | volume::DaytonaVolumeState::PendingCreate => {}
                volume::DaytonaVolumeState::Error
                | volume::DaytonaVolumeState::PendingDelete
                | volume::DaytonaVolumeState::Deleting
                | volume::DaytonaVolumeState::Deleted => {
                    dependencies
                        .storage_resources
                        .mark_unknown(binding.tenant_id, storage_resource_id, resource.generation)
                        .await?;
                    return Err(MoaError::ProviderError(format!(
                        "Daytona tenant volume entered terminal state {:?}",
                        volume.state
                    )));
                }
            }

            let remaining = (request.operation.deadline - chrono::Utc::now())
                .to_std()
                .map_err(|_| {
                    MoaError::ProviderTimeout(
                        "Daytona tenant volume did not become ready before its operation deadline"
                            .to_string(),
                    )
                })?;
            sleep(poll_interval.min(remaining)).await;
            if let Some(observed) = volume::get_volume(&attempt, &volume.id).await? {
                volume = observed;
            }
        }
        if !dependencies
            .storage_resources
            .confirm_created(
                binding.tenant_id,
                storage_resource_id,
                resource.generation,
                request.operation.operation_id,
                &volume.id,
            )
            .await?
        {
            return Err(MoaError::StorageError(
                "Daytona volume create result lost its storage-resource fence".to_string(),
            ));
        }
        if !dependencies
            .capacity
            .commit_lifetime_volume(&capacity_request, storage_resource_id)
            .await?
        {
            return Err(MoaError::StorageError(
                "Daytona tenant volume lost its lifetime capacity reservation fence".to_string(),
            ));
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(storage::mutable_storage_reference(
                &request.operation,
                volume.id,
            )?),
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn attach_workspace(
        &self,
        request: WorkspaceAttachRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use moa_core::types::sandbox_workspace::{
            WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };
        let storage = match request.storage {
            Some(storage) => storage,
            None => {
                self.mutable_storage_for_operation(self.storage_dependencies()?, &request.operation)
                    .await?
            }
        };
        verify_request_resources(&request.operation, Some(&request.hand), Some(&storage))?;
        let resource = self
            .exact_mutable_storage_resource(&request.operation, &storage)
            .await?;
        if !matches!(
            resource.state,
            crate::core::sandbox_workspace::storage_resources::StorageResourceState::Ready
                | crate::core::sandbox_workspace::storage_resources::StorageResourceState::Attached
        ) {
            return Err(MoaError::StorageError(
                "Daytona attach requires a ready tenant-owned volume".to_string(),
            ));
        }
        if !self.verify_workspace_storage(&storage).await? {
            return Err(MoaError::ValidationError(
                "Daytona attach storage reference is invalid".to_string(),
            ));
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(storage),
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn publish_workspace_checkpoint(
        &self,
        request: WorkspaceCheckpointPublishRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        self.create_portable_checkpoint(
            &request.operation,
            &request.hand,
            request.parent_revision.as_ref(),
            request.release_compute,
        )
        .await
    }

    async fn restore_workspace(
        &self,
        request: WorkspaceRestoreRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext;
        use moa_core::types::sandbox_workspace::{
            WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };
        verify_request_resources(&request.operation, Some(&request.hand), None)?;
        let dependencies = self.storage_dependencies()?;
        let context = CheckpointStoreContext {
            tenant_id: request.operation.binding.tenant_id,
            workspace_id: request.operation.binding.workspace_id,
            checkpoint_id: request.revision.checkpoint_id,
            provider_account_id: request.operation.binding.provider_account_id,
            provider_account_generation: request.operation.binding.provider_account_generation,
        };
        if !dependencies
            .checkpoint_store
            .reference_matches(context, &request.checkpoint)
        {
            return Err(MoaError::ValidationError(
                "Daytona restore checkpoint reference does not match workspace fences".to_string(),
            ));
        }
        let staging = tempfile::tempdir().map_err(|error| {
            MoaError::StorageError(format!("create Daytona restore staging root: {error}"))
        })?;
        let restored = staging.path().join("verified");
        dependencies
            .checkpoint_store
            .restore(context, &restored)
            .await?;
        let sandbox_id = request.hand.daytona_id()?;
        let (account_id, account_generation) = cloud_account(&request.hand, "Daytona")?;
        let attempt = self
            .attempt(account_id, account_generation, ProviderEndpoint::Toolbox)
            .await?;
        let cleanup = self
            .execute_command(
                &attempt,
                sandbox_id,
                "find /workspace -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +",
                Some("/workspace"),
                Some(DEFAULT_COMMAND_TIMEOUT),
            )
            .await?;
        if cleanup.is_error {
            return Err(MoaError::ProviderError(
                "Daytona restore could not clear the fenced target subpath".to_string(),
            ));
        }
        storage::upload_restored_workspace(
            &attempt,
            sandbox_id,
            "/workspace",
            &restored,
            dependencies.checkpoint_store.archive_limits(),
        )
        .await?;
        let claims = storage::DaytonaWorkspaceMarkerClaims {
            operation_id: request.operation.operation_id,
            request_hash: request.operation.request_hash.clone(),
            spec_hash: request.operation.request_hash.clone(),
            writer_epoch: request.operation.binding.writer_epoch,
            instance_generation: request.operation.binding.instance_generation,
        };
        let marker =
            storage::seal_workspace_marker(dependencies.kms.as_ref(), &request.operation, &claims)
                .await?;
        storage::upload_workspace_marker(&attempt, sandbox_id, "/workspace", &marker).await?;
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(request.checkpoint),
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn delete_workspace_storage(
        &self,
        request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use moa_core::types::sandbox_workspace::{
            ProviderStorageKind, WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };
        verify_request_resources(&request.operation, None, Some(&request.storage))?;
        let dependencies = self.storage_dependencies()?;
        if request.storage.kind == ProviderStorageKind::PortableCheckpoint {
            let context =
                crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                    tenant_id: request.operation.binding.tenant_id,
                    workspace_id: request.operation.binding.workspace_id,
                    checkpoint_id: required_current_revision(&request.operation, "Daytona")?
                        .checkpoint_id,
                    provider_account_id: request.operation.binding.provider_account_id,
                    provider_account_generation: request
                        .operation
                        .binding
                        .provider_account_generation,
                };
            if !dependencies
                .checkpoint_store
                .reference_matches(context, &request.storage)
            {
                return Err(MoaError::ValidationError(
                    "Daytona checkpoint deletion reference lost its workspace fence".to_string(),
                ));
            }
            dependencies.checkpoint_store.delete(context).await?;
            return Ok(WorkspaceStorageOperationResult {
                outcome: WorkspaceOperationOutcome::Confirmed,
                confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourceAbsent),
                storage: None,
                checkpoint_publication: None,
                post_commit_state: None,
            });
        }
        if request.storage.kind != ProviderStorageKind::MutableFilesystem {
            return Err(MoaError::Unsupported(
                "Daytona can delete only mutable tenant volumes or portable checkpoints"
                    .to_string(),
            ));
        }
        self.exact_mutable_storage_resource(&request.operation, &request.storage)
            .await?;
        if request.storage.workspace_locator.is_none() {
            return Err(MoaError::ValidationError(
                "ordinary Daytona workspace deletion cannot delete a whole tenant volume; the tenant-purge owner must call delete_tenant_volume_for_purge"
                    .to_string(),
            ));
        }
        let empty = self
            .cleanup_workspace_subpath(&request.operation, &request.storage)
            .await?;
        Ok(WorkspaceStorageOperationResult {
            outcome: if empty {
                WorkspaceOperationOutcome::Confirmed
            } else {
                WorkspaceOperationOutcome::Unknown
            },
            confirmed_disposition: empty.then_some(WorkspaceConfirmedDisposition::ResourceAbsent),
            storage: (!empty).then_some(request.storage),
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn delete_tenant_storage_resource(
        &self,
        request: TenantStoragePurgeRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        use moa_core::types::sandbox_workspace::{
            ProviderStorageKind, WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
        };
        if request.purge_operation_id.trim().is_empty()
            || request.storage.kind != ProviderStorageKind::MutableFilesystem
            || request.storage.workspace_locator.is_some()
            || request.storage.provider_account_id != request.provider_account_id
            || request.storage.provider_account_generation != request.provider_account_generation
        {
            return Err(MoaError::ValidationError(
                "Daytona tenant purge requires one exact whole-volume account reference"
                    .to_string(),
            ));
        }
        let generation = i64::try_from(request.provider_account_generation).map_err(|_| {
            MoaError::ValidationError("Daytona account generation overflows bigint".to_string())
        })?;
        self.storage_dependencies()?
            .storage_resources
            .by_provider_reference(
                request.tenant_id,
                request.provider_account_id,
                generation,
                &request.storage.resource_id,
            )
            .await?
            .ok_or_else(|| {
                MoaError::ValidationError(
                    "Daytona tenant purge storage reference has no exact durable owner".to_string(),
                )
            })?;
        let attempt = self
            .attempt(
                request.provider_account_id,
                request.provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        match volume::delete_volume(&attempt, &request.storage.resource_id).await? {
            volume::DaytonaVolumeDeleteOutcome::MountedConflict => {
                return Err(MoaError::HttpStatus {
                    status: reqwest::StatusCode::CONFLICT.as_u16(),
                    retry_after: None,
                    message: "Daytona tenant volume remains mounted by a sandbox".to_string(),
                });
            }
            volume::DaytonaVolumeDeleteOutcome::Accepted
            | volume::DaytonaVolumeDeleteOutcome::Absent => {}
        }
        let consistency = Duration::from_secs(
            self.storage_dependencies()?
                .config
                .consistency_window_seconds
                .max(1),
        );
        for observation in 0..2 {
            if volume::get_volume(&attempt, &request.storage.resource_id)
                .await?
                .is_some()
            {
                return Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: Some(request.storage),
                    checkpoint_publication: None,
                    post_commit_state: None,
                });
            }
            if observation == 0 {
                sleep(consistency).await;
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

    async fn reconcile_workspace_operation(
        &self,
        request: WorkspaceReconcileRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        request.validate()?;
        use moa_core::types::sandbox_workspace::{
            ProviderStorageKind, WorkspaceConfirmedDisposition, WorkspaceOperationKind,
            WorkspaceOperationOutcome,
        };
        let storage = request.storage().cloned().ok_or_else(|| {
            MoaError::ValidationError(
                "Daytona reconciliation requires an exact durable storage reference".to_string(),
            )
        })?;
        verify_request_resources(request.operation(), request.hand(), Some(&storage))?;
        if storage.kind == ProviderStorageKind::PortableCheckpoint {
            let checkpoint_id = match request.operation().kind {
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint => {
                    WorkspaceCheckpointId(request.operation().operation_id.0)
                }
                WorkspaceOperationKind::Restore | WorkspaceOperationKind::Delete => {
                    required_current_revision(request.operation(), "Daytona")?.checkpoint_id
                }
                WorkspaceOperationKind::Create | WorkspaceOperationKind::Attach => {
                    return Err(MoaError::ValidationError(
                        "Daytona create/attach reconciliation cannot name a portable checkpoint"
                            .to_string(),
                    ));
                }
            };
            let context =
                crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                    tenant_id: request.operation().binding.tenant_id,
                    workspace_id: request.operation().binding.workspace_id,
                    checkpoint_id,
                    provider_account_id: request.operation().binding.provider_account_id,
                    provider_account_generation: request
                        .operation()
                        .binding
                        .provider_account_generation,
                };
            let store = &self.storage_dependencies()?.checkpoint_store;
            if !store.reference_matches(context, &storage) {
                return Err(MoaError::ValidationError(
                    "Daytona reconciliation checkpoint lost its exact workspace fence".to_string(),
                ));
            }
            let published = store.inspect_publication(context, &storage).await?;
            let present = published.is_some();
            let confirmed = matches!(
                (request.operation().kind, present),
                (
                    WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint,
                    true
                ) | (WorkspaceOperationKind::Delete, false)
            );
            if !confirmed {
                return Ok(WorkspaceStorageOperationResult {
                    outcome: WorkspaceOperationOutcome::Unknown,
                    confirmed_disposition: None,
                    storage: present.then_some(storage),
                    checkpoint_publication: None,
                    post_commit_state: None,
                });
            }
            let disposition = if present {
                WorkspaceConfirmedDisposition::ResourcePresent
            } else {
                WorkspaceConfirmedDisposition::ResourceAbsent
            };
            let checkpoint_publication = if matches!(
                request.operation().kind,
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint
            ) {
                let published = published.ok_or_else(|| {
                    MoaError::StorageError(
                        "confirmed Daytona checkpoint reconciliation lost its manifest".to_string(),
                    )
                })?;
                Some(WorkspaceCheckpointPublication {
                    revision: next_workspace_revision(
                        request.operation(),
                        request.operation().binding.current_revision.as_ref(),
                        checkpoint_id,
                        Some("Daytona"),
                    )?,
                    storage: published.storage,
                    manifest_digest: published.manifest_sha256,
                    logical_bytes: published.logical_bytes,
                })
            } else {
                None
            };
            return Ok(WorkspaceStorageOperationResult {
                outcome: WorkspaceOperationOutcome::Confirmed,
                confirmed_disposition: Some(disposition),
                storage: present.then_some(storage),
                checkpoint_publication,
                post_commit_state: Some(WorkspacePostCommitState::AttachmentRetained),
            });
        }
        self.exact_mutable_storage_resource(request.operation(), &storage)
            .await?;
        let attempt = self
            .attempt(
                storage.provider_account_id,
                storage.provider_account_generation,
                ProviderEndpoint::Api,
            )
            .await?;
        let present = volume::get_volume(&attempt, &storage.resource_id)
            .await?
            .is_some();
        let disposition = if present {
            WorkspaceConfirmedDisposition::ResourcePresent
        } else {
            WorkspaceConfirmedDisposition::ResourceAbsent
        };
        if request.operation().kind != WorkspaceOperationKind::Delete && !present {
            return Ok(WorkspaceStorageOperationResult {
                outcome: WorkspaceOperationOutcome::Unknown,
                confirmed_disposition: None,
                storage: None,
                checkpoint_publication: None,
                post_commit_state: None,
            });
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(disposition),
            storage: present.then_some(storage),
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn verify_workspace_storage(&self, storage: &ProviderStorageRef) -> Result<bool> {
        use moa_core::types::sandbox_workspace::ProviderStorageKind;

        match storage.kind {
            ProviderStorageKind::PortableCheckpoint => {
                self.storage_dependencies()?
                    .checkpoint_store
                    .verify_reference(storage)
                    .await
            }
            ProviderStorageKind::MutableFilesystem => {
                let locator_is_valid = storage
                    .workspace_locator
                    .as_deref()
                    .is_some_and(|locator| storage::validate_workspace_subpath(locator).is_ok());
                if storage.resource_id.trim().is_empty() || !locator_is_valid {
                    return Ok(false);
                }
                let attempt = self
                    .attempt(
                        storage.provider_account_id,
                        storage.provider_account_generation,
                        ProviderEndpoint::Api,
                    )
                    .await?;
                Ok(volume::get_volume(&attempt, &storage.resource_id)
                    .await?
                    .is_some_and(|volume| {
                        matches!(volume.state, volume::DaytonaVolumeState::Ready)
                    }))
            }
        }
    }
}

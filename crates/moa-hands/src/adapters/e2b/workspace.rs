//! Persistent sandbox-workspace storage behavior for the E2B provider.

use super::*;

impl E2BHandProvider {
    fn checkpoint_store(&self) -> Result<&CheckpointObjectStore> {
        self.checkpoint_store.as_deref().ok_or_else(|| {
            MoaError::ConfigError(
                "E2B persistent workspaces require configured checkpoint object storage"
                    .to_string(),
            )
        })
    }

    /// Resolves a running sandbox after verifying its exact workspace binding.
    pub(super) async fn workspace_sandbox(
        &self,
        hand: &HandHandle,
        binding: &moa_core::types::sandbox_workspace::WorkspaceBinding,
    ) -> Result<(String, ProviderSandboxAttempt, ConnectedSandbox)> {
        let sandbox_id = sandbox_id(hand)?.to_string();
        let account = cloud_account(hand)?;
        if account
            != (
                binding.provider_account_id,
                binding.provider_account_generation,
            )
        {
            return Err(MoaError::ValidationError(
                "E2B hand does not match the workspace provider-account fence".to_string(),
            ));
        }
        let api_attempt = self.api_attempt(account.0, account.1).await?;
        let sandbox = self
            .running_sandbox(&api_attempt, &sandbox_id, account, Some(binding))
            .await?;
        let envd_origin = self.envd_url(&sandbox_id, &sandbox);
        let sandbox_attempt = self
            .credentials
            .admit_sandbox_attempt(
                account.0,
                account.1,
                CloudHandProviderKind::E2b,
                &envd_origin,
                DEFAULT_COMMAND_TIMEOUT,
            )
            .await?;
        Ok((sandbox_id, sandbox_attempt, sandbox))
    }

    async fn kill_exact_workspace_hand(
        &self,
        hand: &HandHandle,
        binding: &moa_core::types::sandbox_workspace::WorkspaceBinding,
    ) -> Result<()> {
        let sandbox_id = sandbox_id(hand)?;
        let account = cloud_account(hand)?;
        let attempt = self.api_attempt(account.0, account.1).await?;
        if self
            .inspect_sandbox(&attempt, sandbox_id, account, Some(binding))
            .await?
            .is_none()
        {
            return Ok(());
        }
        let response = attempt
            .client()
            .delete(format!("{}/sandboxes/{sandbox_id}", attempt.origin()))
            .header("X-API-KEY", attempt.credential())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("failed to kill exact E2B sandbox: {error}"))
            })?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(http_error(response).await)
    }

    fn checkpoint_context(
        operation: &WorkspaceStorageOperation,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
        crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
            tenant_id: operation.binding.tenant_id,
            workspace_id: operation.binding.workspace_id,
            checkpoint_id,
            provider_account_id: operation.binding.provider_account_id,
            provider_account_generation: operation.binding.provider_account_generation,
        }
    }

    async fn create_portable_checkpoint(
        &self,
        operation: &WorkspaceStorageOperation,
        hand: &HandHandle,
        parent_revision: Option<&WorkspaceRevisionRef>,
    ) -> Result<WorkspaceStorageOperationResult> {
        if chrono::Utc::now() >= operation.deadline {
            return Err(MoaError::ProviderTimeout(
                "workspace checkpoint deadline elapsed before E2B export".to_string(),
            ));
        }
        let checkpoint_id = WorkspaceCheckpointId(operation.operation_id.0);
        let revision =
            next_workspace_revision(operation, parent_revision, checkpoint_id, Some("E2B"))?;
        let store = self.checkpoint_store()?;
        let (sandbox_id, sandbox_attempt, sandbox) =
            self.workspace_sandbox(hand, &operation.binding).await?;
        self.run_checked_command(
            &sandbox_attempt,
            &sandbox_id,
            &sandbox,
            "checkpoint_sync",
            "sync",
        )
        .await?;
        let archive = storage::export_data_root(
            self,
            &sandbox_attempt,
            &sandbox_id,
            &sandbox,
            store.archive_limits(),
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
        self.kill_exact_workspace_hand(hand, &operation.binding)
            .await?;
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Confirmed,
            confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
            storage: Some(published.storage),
            checkpoint_publication: Some(checkpoint_publication),
            post_commit_state: (operation.kind == WorkspaceOperationKind::Commit)
                .then_some(WorkspacePostCommitState::ComputeDestroyed),
        })
    }
}

#[async_trait]
impl SandboxStorageProvider for E2BHandProvider {
    fn storage_provider_name(&self) -> &str {
        "e2b"
    }

    async fn enumerate_account_storage(
        &self,
        provider_account_id: moa_core::types::identifiers::ProviderAccountId,
        provider_account_generation: u64,
    ) -> Result<ProviderAccountStorageInventory> {
        let attempt = self
            .api_attempt(provider_account_id, provider_account_generation)
            .await?;
        let metadata_query = format!("{E2B_PROVIDER_ACCOUNT_METADATA_KEY}={provider_account_id}");
        let page_limit = E2B_SANDBOX_LIST_LIMIT.to_string();
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        let mut resources = Vec::new();
        loop {
            let mut url = build_url(
                &format!("{}/v2/sandboxes", attempt.origin()),
                &[
                    ("metadata", metadata_query.as_str()),
                    ("limit", page_limit.as_str()),
                    ("state", "running,paused"),
                ],
                "E2B",
            )?;
            if let Some(token) = next_token.as_deref() {
                url.query_pairs_mut().append_pair("nextToken", token);
            }
            let response = attempt
                .client()
                .get(url)
                .header("X-API-KEY", attempt.credential())
                .send()
                .await
                .map_err(|error| {
                    MoaError::ProviderTransport(format!(
                        "failed to enumerate E2B account inventory: {error}"
                    ))
                })?;
            if !response.status().is_success() {
                return Err(http_error(response).await);
            }
            let response_next_token = response
                .headers()
                .get("x-next-token")
                .map(|value| {
                    value
                        .to_str()
                        .map(str::trim)
                        .map(str::to_string)
                        .map_err(|error| {
                            MoaError::ProviderError(format!(
                                "invalid E2B inventory continuation header: {error}"
                            ))
                        })
                })
                .transpose()?
                .filter(|token| !token.is_empty());
            let value = expect_success_json(response, "E2B inventory").await?;
            let page = value.as_array().ok_or_else(|| {
                MoaError::ProviderError(
                    "E2B account inventory response must be a JSON array".to_string(),
                )
            })?;
            for item in page {
                let sandbox_id = required_string_field(item, "sandboxID")?.to_string();
                let metadata =
                    item.get("metadata")
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            MoaError::ProviderError(
                                "E2B account inventory resource omitted metadata".to_string(),
                            )
                        })?;
                let observed_account =
                    required_metadata(metadata, E2B_PROVIDER_ACCOUNT_METADATA_KEY)?;
                let observed_generation =
                    required_metadata(metadata, E2B_PROVIDER_ACCOUNT_GENERATION_METADATA_KEY)?;
                if observed_account != provider_account_id.to_string()
                    || observed_generation != provider_account_generation.to_string()
                {
                    return Err(MoaError::ProviderError(
                        "E2B inventory filter returned a resource from another account generation"
                            .to_string(),
                    ));
                }
                let tenant_id = required_metadata(metadata, E2B_TENANT_METADATA_KEY)?
                    .parse::<uuid::Uuid>()
                    .map(moa_core::types::identifiers::TenantId)
                    .map_err(|_| {
                        MoaError::ProviderError(
                            "E2B inventory carries malformed tenant ownership metadata".to_string(),
                        )
                    })?;
                let workspace_id = required_metadata(metadata, E2B_WORKSPACE_METADATA_KEY)?
                    .parse::<uuid::Uuid>()
                    .map(moa_core::types::identifiers::SandboxWorkspaceId)
                    .map_err(|_| {
                        MoaError::ProviderError(
                            "E2B inventory carries malformed workspace ownership metadata"
                                .to_string(),
                        )
                    })?;
                let writer_epoch = required_metadata(metadata, E2B_WRITER_EPOCH_METADATA_KEY)?
                    .parse::<u64>()
                    .map_err(|_| {
                        MoaError::ProviderError(
                            "E2B inventory carries malformed writer ownership metadata".to_string(),
                        )
                    })?;
                let instance_generation =
                    required_metadata(metadata, E2B_INSTANCE_GENERATION_METADATA_KEY)?
                        .parse::<u64>()
                        .map_err(|_| {
                            MoaError::ProviderError(
                                "E2B inventory carries malformed instance ownership metadata"
                                    .to_string(),
                            )
                        })?;
                let provisioning_operation_id =
                    required_metadata(metadata, E2B_PROVISIONING_OPERATION_METADATA_KEY)?
                        .parse::<uuid::Uuid>()
                        .map(moa_core::types::identifiers::HandProvisioningOperationId)
                        .map_err(|_| {
                            MoaError::ProviderError(
                                "E2B inventory carries malformed provisioning operation metadata"
                                    .to_string(),
                            )
                        })?;
                let evidence = serde_json::to_vec(metadata).map_err(|error| {
                    MoaError::ProviderError(format!(
                        "failed to canonicalize E2B inventory evidence: {error}"
                    ))
                })?;
                resources.push(ProviderInventoryResource {
                    kind: ProviderInventoryResourceKind::Compute,
                    provider_reference: sandbox_id.clone(),
                    resource_fingerprint: format!(
                        "sha256:{:x}",
                        Sha256::digest(format!("e2b-sandbox-v1\0{sandbox_id}").as_bytes())
                    ),
                    evidence_digest: format!("sha256:{:x}", Sha256::digest(&evidence)),
                    verified_owner: Some(ProviderInventoryOwner {
                        tenant_id,
                        workspace_id,
                        provisioning_operation_id: Some(provisioning_operation_id),
                        writer_epoch: Some(writer_epoch),
                        instance_generation: Some(instance_generation),
                    }),
                });
            }
            match response_next_token {
                Some(token) => {
                    if !seen_tokens.insert(token.clone()) {
                        return Err(MoaError::ProviderError(
                            "E2B inventory pagination repeated a continuation token".to_string(),
                        ));
                    }
                    next_token = Some(token);
                }
                None => break,
            }
        }
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
        if request.operation.kind != WorkspaceOperationKind::Create
            || request.operation.binding.durability_class != DurabilityClass::PortableFilesystem
            || chrono::Utc::now() >= request.operation.deadline
        {
            return Err(MoaError::ValidationError(
                "E2B storage preparation requires a live exact portable create operation"
                    .to_string(),
            ));
        }
        Ok(confirmed_storage_result(
            None,
            WorkspaceConfirmedDisposition::ResourceAbsent,
        ))
    }

    async fn attach_workspace(
        &self,
        request: WorkspaceAttachRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        if request.storage.is_some() {
            return Err(MoaError::ValidationError(
                "E2B has no mutable volume or snapshot storage to attach".to_string(),
            ));
        }
        let (sandbox_id, attempt, sandbox) = self
            .workspace_sandbox(&request.hand, &request.operation.binding)
            .await?;
        self.run_checked_command(
            &attempt,
            &sandbox_id,
            &sandbox,
            "attach_data_root",
            &storage::prepare_data_root_command(false),
        )
        .await?;
        Ok(confirmed_storage_result(
            None,
            WorkspaceConfirmedDisposition::ResourcePresent,
        ))
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
                "portable checkpoint reference does not match E2B restore fences".to_string(),
            ));
        }
        let (sandbox_id, attempt, sandbox) = self
            .workspace_sandbox(&request.hand, &request.operation.binding)
            .await?;
        storage::restore_checkpoint_data_root(
            self,
            store,
            context,
            &attempt,
            &sandbox_id,
            &sandbox,
            store.archive_limits(),
        )
        .await?;
        Ok(confirmed_storage_result(
            Some(request.checkpoint),
            WorkspaceConfirmedDisposition::ResourcePresent,
        ))
    }

    async fn delete_workspace_storage(
        &self,
        request: WorkspaceStorageDeleteRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        if request.storage.kind != ProviderStorageKind::PortableCheckpoint {
            return Err(MoaError::Unsupported(
                "E2B has no production volume, paused filesystem, or native snapshot storage"
                    .to_string(),
            ));
        }
        let context = Self::checkpoint_context(
            &request.operation,
            required_current_revision(&request.operation, "E2B")?.checkpoint_id,
        );
        let store = self.checkpoint_store()?;
        if !store.reference_matches(context, &request.storage) {
            return Err(MoaError::ValidationError(
                "portable checkpoint delete reference does not match E2B workspace fences"
                    .to_string(),
            ));
        }
        store.delete(context).await?;
        Ok(confirmed_storage_result(
            None,
            WorkspaceConfirmedDisposition::ResourceAbsent,
        ))
    }

    async fn delete_tenant_storage_resource(
        &self,
        request: TenantStoragePurgeRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        if request.purge_operation_id.trim().is_empty()
            || request.storage.provider_account_id != request.provider_account_id
            || request.storage.provider_account_generation != request.provider_account_generation
        {
            return Err(MoaError::ValidationError(
                "E2B tenant purge storage reference lost its exact account fence".to_string(),
            ));
        }
        Err(MoaError::Unsupported(
            "E2B has no tenant-wide mutable storage resource to purge".to_string(),
        ))
    }

    async fn reconcile_workspace_operation(
        &self,
        request: WorkspaceReconcileRequest,
    ) -> Result<WorkspaceStorageOperationResult> {
        request.validate()?;
        let operation = request.operation();
        if let Some(storage) = request.storage()
            && storage.kind == ProviderStorageKind::PortableCheckpoint
        {
            let checkpoint_id = match operation.kind {
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint => {
                    WorkspaceCheckpointId(operation.operation_id.0)
                }
                WorkspaceOperationKind::Restore | WorkspaceOperationKind::Delete => {
                    required_current_revision(operation, "E2B")?.checkpoint_id
                }
                WorkspaceOperationKind::Create | WorkspaceOperationKind::Attach => {
                    return Err(MoaError::ValidationError(
                        "E2B create/attach reconciliation cannot name portable checkpoint storage"
                            .to_string(),
                    ));
                }
            };
            let store = self.checkpoint_store()?;
            let context = Self::checkpoint_context(operation, checkpoint_id);
            if !store.reference_matches(context, storage) {
                return Err(MoaError::ValidationError(
                    "E2B reconciliation checkpoint does not match the exact workspace operation"
                        .to_string(),
                ));
            }
            let published = store.inspect_publication(context, storage).await?;
            let present = published.is_some();
            match operation.kind {
                WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint if present => {
                    if let Some(hand) = request.hand() {
                        self.kill_exact_workspace_hand(hand, &operation.binding)
                            .await?;
                    }
                    let published = published.ok_or_else(|| {
                        MoaError::StorageError(
                            "confirmed E2B checkpoint reconciliation lost its manifest".to_string(),
                        )
                    })?;
                    let checkpoint_publication = WorkspaceCheckpointPublication {
                        revision: next_workspace_revision(
                            operation,
                            operation.binding.current_revision.as_ref(),
                            checkpoint_id,
                            Some("E2B"),
                        )?,
                        storage: published.storage,
                        manifest_digest: published.manifest_sha256,
                        logical_bytes: published.logical_bytes,
                    };
                    return Ok(WorkspaceStorageOperationResult {
                        outcome: WorkspaceOperationOutcome::Confirmed,
                        confirmed_disposition: Some(WorkspaceConfirmedDisposition::ResourcePresent),
                        storage: Some(storage.clone()),
                        checkpoint_publication: Some(checkpoint_publication),
                        post_commit_state: Some(WorkspacePostCommitState::ComputeDestroyed),
                    });
                }
                WorkspaceOperationKind::Delete if !present => {
                    return Ok(confirmed_storage_result(
                        None,
                        WorkspaceConfirmedDisposition::ResourceAbsent,
                    ));
                }
                // A present source checkpoint cannot prove that an ambiguous
                // restore uploaded every entry into the fresh hand. Likewise,
                // absence cannot prove where a commit/delete stopped. Keep the
                // operation unknown so lifecycle recovery replaces compute or
                // retries the exact fenced operation instead of guessing.
                WorkspaceOperationKind::Restore
                | WorkspaceOperationKind::Commit
                | WorkspaceOperationKind::Checkpoint
                | WorkspaceOperationKind::Delete => {
                    return Ok(WorkspaceStorageOperationResult {
                        outcome: WorkspaceOperationOutcome::Unknown,
                        confirmed_disposition: None,
                        storage: present.then(|| storage.clone()),
                        checkpoint_publication: None,
                        post_commit_state: None,
                    });
                }
                WorkspaceOperationKind::Create | WorkspaceOperationKind::Attach => {
                    return Err(MoaError::ValidationError(
                        "E2B create/attach reconciliation cannot name portable checkpoint storage"
                            .to_string(),
                    ));
                }
            }
        }
        if let Some(hand) = request.hand() {
            let sandbox_id = sandbox_id(hand)?;
            let account = cloud_account(hand)?;
            let attempt = self.api_attempt(account.0, account.1).await?;
            let present = self
                .inspect_sandbox(&attempt, sandbox_id, account, Some(&operation.binding))
                .await?
                .is_some();
            return Ok(confirmed_storage_result(
                None,
                if present {
                    WorkspaceConfirmedDisposition::ResourcePresent
                } else {
                    WorkspaceConfirmedDisposition::ResourceAbsent
                },
            ));
        }
        Ok(WorkspaceStorageOperationResult {
            outcome: WorkspaceOperationOutcome::Unknown,
            confirmed_disposition: None,
            storage: None,
            checkpoint_publication: None,
            post_commit_state: None,
        })
    }

    async fn verify_workspace_storage(&self, storage: &ProviderStorageRef) -> Result<bool> {
        match storage.kind {
            ProviderStorageKind::PortableCheckpoint => {
                self.checkpoint_store()?.verify_reference(storage).await
            }
            ProviderStorageKind::MutableFilesystem => Ok(false),
        }
    }
}

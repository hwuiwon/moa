//! External-first tenant purge and separated absence proofs.

use super::inventory::{InventoryFindingKind, finding, validate_inventory_identity};
use super::*;

impl WorkspaceMaintenanceCoordinator {
    /// Fences access, removes external sandbox state, and returns exact absence evidence.
    ///
    /// Relational metadata remains after the fence so provider outages retain
    /// ownership and reconciliation evidence for the next replay.
    pub async fn purge_tenant_external(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<WorkspaceTenantPurgeProof> {
        if operation_id.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "workspace tenant purge requires an operation id".to_string(),
            ));
        }
        failpoints::record_purge_external_phase(operation_id, "entered").await;
        self.start_and_fence_tenant_purge(tenant_id, operation_id)
            .await?;
        failpoints::record_purge_external_phase(operation_id, "fenced").await;
        self.ensure_tenant_mutations_drained(tenant_id).await?;
        failpoints::record_purge_external_phase(operation_id, "mutations_drained").await;
        let hands = self.purge_tenant_hands(tenant_id).await?;
        failpoints::record_purge_external_phase(operation_id, "hands_absent").await;
        let checkpoints = self.purge_tenant_checkpoints(tenant_id).await?;
        failpoints::record_purge_external_phase(operation_id, "checkpoints_absent").await;
        let storage_resources = self.purge_tenant_storage(tenant_id, operation_id).await?;
        failpoints::record_purge_external_phase(operation_id, "storage_absent").await;
        let (provider_accounts, provider_inventory_digest) =
            self.prove_tenant_provider_absence(tenant_id).await?;
        failpoints::record_purge_external_phase(operation_id, "proof_ready").await;
        Ok(WorkspaceTenantPurgeProof {
            tenant_id,
            operation_id: operation_id.to_string(),
            hands,
            storage_resources,
            checkpoints,
            provider_accounts,
            provider_inventory_digest,
        })
    }

    /// Durably confirms one exact journaled external-absence proof without provider I/O.
    pub async fn confirm_tenant_external_absence(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        proof: &WorkspaceTenantPurgeProof,
    ) -> Result<()> {
        proof.validate_for(tenant_id, operation_id)?;
        failpoints::hit("post_provider_delete_pre_durable_confirmation").await?;
        let digest = proof.evidence_digest();
        let mut conn = maintenance_conn(&self.pool).await?;
        sqlx::query("SELECT moa.confirm_sandbox_external_absence_for_tenant_purge($1, $2, $3)")
            .bind(tenant_id)
            .bind(operation_id)
            .bind(digest)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?;
        conn.commit().await?;
        Ok(())
    }

    /// Emits a complete zero-inclusive fleet snapshot for workspace states.
    async fn start_and_fence_tenant_purge(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
            .bind(tenant_id)
            .bind(operation_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?;
        sqlx::query("SELECT moa.fence_sandbox_workspaces_for_tenant_purge($1, $2)")
            .bind(tenant_id)
            .bind(operation_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?;
        conn.commit().await?;
        Ok(())
    }

    async fn ensure_tenant_mutations_drained(&self, tenant_id: TenantId) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let ambiguous: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM moa.hand_leases
                WHERE tenant_id = $1 AND status = 'provisioning'
                UNION ALL
                SELECT 1 FROM moa.sandbox_workspace_operations
                WHERE tenant_id = $1 AND outcome_class IN ('not_sent', 'unknown')
            )
            "#,
        )
        .bind(tenant_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        if ambiguous {
            return Err(MoaError::StorageError(
                "tenant purge is waiting for all in-flight provider mutations to reconcile"
                    .to_string(),
            ));
        }
        Ok(())
    }

    async fn prove_tenant_provider_absence(&self, tenant_id: TenantId) -> Result<(u64, String)> {
        let accounts = self.tenant_provider_accounts(tenant_id).await?;
        for observation in 0..2 {
            for account in &accounts {
                let inventory = self
                    .storage_provider(&account.provider)?
                    .enumerate_account_storage(account.id, account.generation)
                    .await?;
                validate_inventory_identity(account, &inventory)?;
                let durable = self.durable_inventory(account).await?;
                for resource in &inventory.resources {
                    match &resource.verified_owner {
                        Some(owner) if owner.tenant_id == tenant_id => {
                            return Err(MoaError::StorageError(
                                "tenant-owned provider compute remains after purge".to_string(),
                            ));
                        }
                        Some(owner) => match durable.workspaces.get(&owner.workspace_id) {
                            Some(durable_owner)
                                if durable_owner.tenant_id == owner.tenant_id
                                    && durable_owner.provider_account_id == account.id
                                    && u64::try_from(durable_owner.provider_account_generation)
                                        .ok()
                                        == Some(account.generation)
                                    && owner.writer_epoch.is_some_and(|epoch| {
                                        i64::try_from(epoch).ok()
                                            == Some(durable_owner.writer_epoch)
                                    })
                                    && owner.instance_generation.is_some_and(|generation| {
                                        i64::try_from(generation).ok()
                                            == Some(durable_owner.instance_generation)
                                    })
                                    && owner.provisioning_operation_id.is_some_and(
                                        |operation| {
                                            durable.hands.get(&operation).is_some_and(|hand| {
                                                hand.tenant_id == owner.tenant_id
                                                    && hand.workspace_id == owner.workspace_id
                                            })
                                        },
                                    ) => {}
                            _ => {
                                self.upsert_inventory_finding(&finding(
                                    account,
                                    resource,
                                    InventoryFindingKind::WrongOwner,
                                ))
                                .await?;
                                return Err(MoaError::StorageError(
                                    "provider compute inventory has unverifiable ownership"
                                        .to_string(),
                                ));
                            }
                        },
                        None => match durable.storage.get(&resource.provider_reference) {
                            Some(owner) if *owner != tenant_id => {}
                            Some(_) => {
                                return Err(MoaError::StorageError(
                                    "tenant-owned provider storage remains after purge".to_string(),
                                ));
                            }
                            None => {
                                self.upsert_inventory_finding(&finding(
                                    account,
                                    resource,
                                    InventoryFindingKind::Unknown,
                                ))
                                .await?;
                                return Err(MoaError::StorageError(
                                    "provider inventory contains quarantined unknown storage"
                                        .to_string(),
                                ));
                            }
                        },
                    }
                }
            }
            if observation == 0 && !accounts.is_empty() {
                tokio::time::sleep(self.checkpoint_store.deletion_consistency_window()).await;
            }
        }
        let mut digest = Sha256::new();
        digest.update(b"moa/tenant-provider-empty-inventory/v1\0");
        digest.update(tenant_id.0.as_bytes());
        for account in &accounts {
            digest.update(account.id.0.as_bytes());
            digest.update(account.generation.to_be_bytes());
        }
        Ok((
            accounts.len() as u64,
            format!("sha256:{:x}", digest.finalize()),
        ))
    }

    async fn purge_tenant_hands(&self, tenant_id: TenantId) -> Result<u64> {
        let hands = self.tenant_hands(tenant_id).await?;
        let mut discovered_counts = Vec::with_capacity(hands.len());
        for hand in &hands {
            let provider = self.hand_provider(&hand.provider)?;
            let provisioned = provider
                .provisioned_hands(
                    hand.provider_account_id,
                    hand.provider_account_generation,
                    hand.provisioning_operation_id,
                )
                .await?;
            if provisioned.iter().any(|handle| {
                handle.provider_account().is_some_and(|account| {
                    account != (hand.provider_account_id, hand.provider_account_generation)
                })
            }) {
                return Err(MoaError::StorageError(
                    "tenant purge provider inventory crossed its exact account fence".to_string(),
                ));
            }
            let mut targets = hand
                .handle
                .as_ref()
                .map(|handle| vec![handle.handle.clone()])
                .unwrap_or_default();
            for duplicate in provisioned {
                if !targets.contains(&duplicate) {
                    targets.push(duplicate);
                }
            }
            if targets
                .iter()
                .any(|target| match target.provider_account() {
                    Some(account) => {
                        account != (hand.provider_account_id, hand.provider_account_generation)
                    }
                    None => hand.provider != "local",
                })
            {
                return Err(MoaError::StorageError(
                    "tenant purge durable hand crossed its exact provider account fence"
                        .to_string(),
                ));
            }
            for target in &targets {
                provider.destroy(target).await?;
            }
            discovered_counts.push(targets.len() as u64);
        }
        for observation in 0..2 {
            for hand in &hands {
                let provider = self.hand_provider(&hand.provider)?;
                if !provider
                    .provisioned_hands(
                        hand.provider_account_id,
                        hand.provider_account_generation,
                        hand.provisioning_operation_id,
                    )
                    .await?
                    .is_empty()
                {
                    return Err(MoaError::StorageError(
                        "tenant purge cannot prove hand absence".to_string(),
                    ));
                }
            }
            if observation == 0 && !hands.is_empty() {
                tokio::time::sleep(self.checkpoint_store.deletion_consistency_window()).await;
            }
        }
        Ok(discovered_counts.into_iter().sum())
    }

    async fn purge_tenant_checkpoints(&self, tenant_id: TenantId) -> Result<u64> {
        let checkpoints = self.tenant_checkpoints(tenant_id).await?;
        for checkpoint in &checkpoints {
            self.checkpoint_store.delete(checkpoint.context).await?;
        }
        let first_at = Utc::now();
        let mut first = Vec::with_capacity(checkpoints.len());
        for checkpoint in &checkpoints {
            match self
                .checkpoint_store
                .observe_absence(checkpoint.context, None, first_at)
                .await?
            {
                CheckpointPrefixObservation::EmptyPending(observation) => first.push(observation),
                CheckpointPrefixObservation::Absent(_) => {
                    return Err(MoaError::StorageError(
                        "checkpoint absence proof unexpectedly skipped its first observation"
                            .to_string(),
                    ));
                }
                CheckpointPrefixObservation::Present(_) => {
                    return Err(MoaError::StorageError(
                        "checkpoint objects remain after tenant purge deletion".to_string(),
                    ));
                }
            }
        }
        tokio::time::sleep(self.checkpoint_store.deletion_consistency_window()).await;
        for (checkpoint, first) in checkpoints.iter().zip(&first) {
            if !matches!(
                self.checkpoint_store
                    .observe_absence(checkpoint.context, Some(first), Utc::now())
                    .await?,
                CheckpointPrefixObservation::Absent(_)
            ) {
                return Err(MoaError::StorageError(
                    "checkpoint prefix lacks two separated empty observations".to_string(),
                ));
            }
        }
        Ok(checkpoints.len() as u64)
    }

    async fn purge_tenant_storage(&self, tenant_id: TenantId, operation_id: &str) -> Result<u64> {
        let resources = self
            .tenant_storage_resources(tenant_id, operation_id)
            .await?;
        for resource in &resources {
            let result = self
                .storage_provider(&resource.provider)?
                .delete_tenant_storage_resource(resource.request.clone())
                .await?;
            if result.confirmed_disposition != Some(WorkspaceConfirmedDisposition::ResourceAbsent) {
                return Err(MoaError::StorageError(
                    "tenant storage deletion remains ambiguous".to_string(),
                ));
            }
        }
        Ok(resources.len() as u64)
    }

    async fn tenant_hands(&self, tenant_id: TenantId) -> Result<Vec<TenantHand>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT lease.provider, lease.handle, lease.provisioning_operation_id,
                   workspace.provider_account_id,
                   workspace.provider_account_generation
            FROM moa.hand_leases AS lease
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = lease.tenant_id
             AND workspace.workspace_id = lease.workspace_id
            WHERE lease.tenant_id = $1 AND lease.status <> 'destroyed'
            ORDER BY lease.session_id, lease.worker_id
            "#,
        )
        .bind(tenant_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let hands = rows
            .iter()
            .map(|row| {
                Ok(TenantHand {
                    provider: row.try_get("provider").map_err(map_sqlx)?,
                    handle: row
                        .try_get::<Option<Json<LeaseHandle>>, _>("handle")
                        .map_err(map_sqlx)?
                        .map(|handle| handle.0),
                    provisioning_operation_id: row
                        .try_get("provisioning_operation_id")
                        .map_err(map_sqlx)?,
                    provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx)?,
                    provider_account_generation: u64::try_from(
                        row.try_get::<i64, _>("provider_account_generation")
                            .map_err(map_sqlx)?,
                    )
                    .map_err(|_| {
                        MoaError::StorageError(
                            "hand provider account generation is invalid".to_string(),
                        )
                    })?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(hands)
    }

    async fn tenant_checkpoints(&self, tenant_id: TenantId) -> Result<Vec<TenantCheckpoint>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT checkpoint.checkpoint_id, checkpoint.workspace_id,
                   workspace.provider_account_id, workspace.provider_account_generation
            FROM moa.sandbox_workspace_checkpoints AS checkpoint
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = checkpoint.tenant_id
             AND workspace.workspace_id = checkpoint.workspace_id
            WHERE checkpoint.tenant_id = $1 AND checkpoint.lifecycle_state <> 'deleted'
            ORDER BY checkpoint.generation DESC
            "#,
        )
        .bind(tenant_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let checkpoints = rows
            .iter()
            .map(|row| {
                let generation: i64 = row
                    .try_get("provider_account_generation")
                    .map_err(map_sqlx)?;
                Ok(TenantCheckpoint {
                    context: CheckpointStoreContext {
                        tenant_id,
                        workspace_id: row.try_get("workspace_id").map_err(map_sqlx)?,
                        checkpoint_id: row.try_get("checkpoint_id").map_err(map_sqlx)?,
                        provider_account_id: row
                            .try_get("provider_account_id")
                            .map_err(map_sqlx)?,
                        provider_account_generation: u64::try_from(generation).map_err(|_| {
                            MoaError::StorageError(
                                "checkpoint account generation is invalid".to_string(),
                            )
                        })?,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(checkpoints)
    }

    async fn tenant_storage_resources(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<Vec<TenantStorageResource>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT resource.provider_account_id, resource.provider_account_generation,
                   resource.provider_reference, resource.resource_kind, account.provider
            FROM moa.sandbox_storage_resources AS resource
            JOIN moa.sandbox_provider_accounts AS account
              ON account.provider_account_id = resource.provider_account_id
             AND account.generation = resource.provider_account_generation
            WHERE resource.tenant_id = $1 AND resource.lifecycle_state <> 'deleted'
              AND resource.provider_reference IS NOT NULL
            ORDER BY resource.storage_resource_id
            "#,
        )
        .bind(tenant_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let resources = rows
            .iter()
            .map(|row| {
                let account_id: ProviderAccountId =
                    row.try_get("provider_account_id").map_err(map_sqlx)?;
                let generation = u64::try_from(
                    row.try_get::<i64, _>("provider_account_generation")
                        .map_err(map_sqlx)?,
                )
                .map_err(|_| {
                    MoaError::StorageError("storage account generation is invalid".to_string())
                })?;
                let provider_reference: String =
                    row.try_get("provider_reference").map_err(map_sqlx)?;
                let kind = match row
                    .try_get::<String, _>("resource_kind")
                    .map_err(map_sqlx)?
                    .as_str()
                {
                    "volume" => ProviderStorageKind::MutableFilesystem,
                    _ => {
                        return Err(MoaError::StorageError(
                            "tenant storage resource has an unknown kind".to_string(),
                        ));
                    }
                };
                Ok(TenantStorageResource {
                    provider: row.try_get("provider").map_err(map_sqlx)?,
                    request: TenantStoragePurgeRequest {
                        tenant_id,
                        purge_operation_id: operation_id.to_string(),
                        provider_account_id: account_id,
                        provider_account_generation: generation,
                        storage: ProviderStorageRef {
                            provider_account_id: account_id,
                            provider_account_generation: generation,
                            kind,
                            resource_id: provider_reference,
                            workspace_locator: None,
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(resources)
    }
}
struct TenantHand {
    provider: String,
    handle: Option<LeaseHandle>,
    provisioning_operation_id: HandProvisioningOperationId,
    provider_account_id: ProviderAccountId,
    provider_account_generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct TenantCheckpoint {
    context: CheckpointStoreContext,
}

#[derive(Debug, Clone)]
struct TenantStorageResource {
    provider: String,
    request: TenantStoragePurgeRequest,
}

/// Hashes one verified empty inventory observation for durable comparison.
pub(super) fn empty_inventory_digest(request_hash: &str) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(format!("moa/empty-workspace-inventory/v1\0{request_hash}").as_bytes())
    )
}

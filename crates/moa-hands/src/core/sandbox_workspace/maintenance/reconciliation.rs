//! Ambiguous-operation reconciliation against verified provider inventory.

use super::*;
use super::{
    inventory::{ProviderAccount, validate_inventory_identity},
    tenant_purge::empty_inventory_digest,
};

impl WorkspaceMaintenanceCoordinator {
    /// Constructs the durable ambiguous-operation reaper owned by this runtime.
    pub fn workspace_reaper(&self, max_concurrency: usize) -> Result<WorkspaceReaper> {
        let probe = Arc::new(MaintenanceReconciliationProbe {
            pool: self.pool.clone(),
            storage_providers: Arc::clone(&self.storage_providers),
            checkpoint_store: Arc::clone(&self.checkpoint_store),
        });
        WorkspaceReaper::new(
            Arc::new(PostgresWorkspaceOperationRepository::new_maintenance(
                self.pool.clone(),
            )),
            Arc::new(
                super::super::repository::PostgresWorkspaceRepository::new_maintenance(
                    self.pool.clone(),
                ),
            ),
            probe,
            self.reconciliation_claim_ttl,
            max_concurrency,
        )
    }
}
struct MaintenanceReconciliationProbe {
    pool: PgPool,
    storage_providers: Arc<HashMap<String, Arc<dyn SandboxStorageProvider>>>,
    checkpoint_store: Arc<CheckpointObjectStore>,
}

impl MaintenanceReconciliationProbe {
    async fn observe_checkpoint_publication(
        &self,
        claimed: &ClaimedWorkspaceOperation,
    ) -> Result<WorkspaceInventoryObservation> {
        let repository = super::super::repository::PostgresWorkspaceRepository::new_maintenance(
            self.pool.clone(),
        );
        let workspace = repository
            .get(claimed.operation.tenant_id, claimed.operation.workspace_id)
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "checkpoint reconciliation lost its durable workspace".to_string(),
                )
            })?;
        let binding = workspace.binding()?;
        if !matches!(
            workspace.state,
            SandboxWorkspaceState::Committing | SandboxWorkspaceState::Reconciling
        ) || binding.provider_account_id != claimed.operation.provider_account_id
            || i64::try_from(binding.provider_account_generation).ok()
                != Some(claimed.operation.provider_account_generation)
            || i64::try_from(binding.writer_epoch).ok()
                != Some(claimed.operation.expected_writer_epoch)
            || i64::try_from(binding.instance_generation).ok()
                != Some(claimed.operation.expected_instance_generation)
            || binding
                .current_revision
                .as_ref()
                .map_or(0, |revision| revision.generation)
                != u64::try_from(claimed.operation.expected_checkpoint_generation).map_err(
                    |_| {
                        MoaError::StorageError(
                            "checkpoint reconciliation has an invalid expected generation"
                                .to_string(),
                        )
                    },
                )?
        {
            return Err(MoaError::StorageError(
                "checkpoint reconciliation crossed its durable workspace fences".to_string(),
            ));
        }
        let checkpoint = repository
            .get_checkpoint_for_operation(
                claimed.operation.tenant_id,
                claimed.operation.workspace_id,
                claimed.operation.operation_id,
            )
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "checkpoint reconciliation lost its creating checkpoint row".to_string(),
                )
            })?;
        if checkpoint.checkpoint_id.0 != claimed.operation.operation_id.0
            || checkpoint.state
                != moa_core::types::sandbox_workspace::WorkspaceCheckpointState::Creating
            || checkpoint.source_writer_epoch != claimed.operation.expected_writer_epoch
            || checkpoint.source_instance_generation
                != claimed.operation.expected_instance_generation
        {
            return Err(MoaError::StorageError(
                "checkpoint reconciliation checkpoint row crossed its operation fences".to_string(),
            ));
        }
        let lease = PostgresHandLeaseStore::new_maintenance(self.pool.clone())
            .get_for_workspace_reconciliation(
                claimed.operation.tenant_id,
                claimed.operation.workspace_id,
                claimed.operation.expected_writer_epoch,
                claimed.operation.expected_instance_generation,
            )
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "checkpoint reconciliation requires the exact active writer lease".to_string(),
                )
            })?;
        let hand = lease
            .handle
            .as_ref()
            .map(|handle| handle.handle.clone())
            .ok_or_else(|| {
                MoaError::StorageError(
                    "checkpoint reconciliation active lease has no provider handle".to_string(),
                )
            })?;
        let context = CheckpointStoreContext {
            tenant_id: binding.tenant_id,
            workspace_id: binding.workspace_id,
            checkpoint_id: checkpoint.checkpoint_id,
            provider_account_id: binding.provider_account_id,
            provider_account_generation: binding.provider_account_generation,
        };
        let storage = self.checkpoint_store.storage_reference(context);
        let operation = WorkspaceStorageOperation {
            operation_id: claimed.operation.operation_id,
            kind: claimed.operation.kind,
            binding: binding.clone(),
            deadline: claimed.operation.deadline_at,
            request_hash: claimed.operation.request_hash.clone(),
        };
        let provider = self
            .storage_providers
            .get(&workspace.provider)
            .ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "workspace reaper has no storage provider for `{}`",
                    workspace.provider
                ))
            })?;
        let result = provider
            .reconcile_workspace_operation(WorkspaceReconcileRequest::new(
                operation,
                Some(hand),
                Some(storage),
            )?)
            .await?;
        let (publication, post_commit_state) = match (
            result.outcome,
            result.confirmed_disposition,
            result.checkpoint_publication,
            result.post_commit_state,
        ) {
            (
                WorkspaceOperationOutcome::Confirmed,
                Some(WorkspaceConfirmedDisposition::ResourcePresent),
                Some(publication),
                Some(post_commit_state),
            ) => (publication, post_commit_state),
            _ => {
                return Err(MoaError::ProviderError(
                    "checkpoint reconciliation did not return complete publication evidence"
                        .to_string(),
                ));
            }
        };
        let evidence = serde_json::to_vec(&(&publication, post_commit_state))?;
        Ok(WorkspaceInventoryObservation::CheckpointPublication {
            inventory_digest: format!("sha256:{:x}", Sha256::digest(evidence)),
            binding: Box::new(binding),
            publication: Box::new(publication),
            post_commit_state,
            lease: Box::new(lease),
        })
    }
}

#[async_trait]
impl WorkspaceReconciliationProbe for MaintenanceReconciliationProbe {
    async fn observe(
        &self,
        claimed: &super::super::operations::ClaimedWorkspaceOperation,
    ) -> Result<WorkspaceInventoryObservation> {
        if matches!(
            claimed.operation.kind,
            moa_core::types::sandbox_workspace::WorkspaceOperationKind::Commit
                | moa_core::types::sandbox_workspace::WorkspaceOperationKind::Checkpoint
        ) {
            return self.observe_checkpoint_publication(claimed).await;
        }
        let mut conn = maintenance_conn(&self.pool).await?;
        let row = sqlx::query(
            r#"
            SELECT workspace.provider, workspace.provider_account_id,
                   workspace.provider_account_generation, workspace.writer_epoch,
                   workspace.instance_generation, resource.provider_reference,
                   ARRAY(
                       SELECT lease.provisioning_operation_id
                       FROM moa.hand_leases AS lease
                       WHERE lease.tenant_id = workspace.tenant_id
                         AND lease.workspace_id = workspace.workspace_id
                   ) AS provisioning_operation_ids
            FROM moa.sandbox_workspaces AS workspace
            LEFT JOIN moa.sandbox_storage_resources AS resource
              ON resource.tenant_id = workspace.tenant_id
             AND resource.create_operation_id = $3
             AND resource.provider_account_id = workspace.provider_account_id
             AND resource.provider_account_generation = workspace.provider_account_generation
             AND resource.lifecycle_state <> 'deleted'
            WHERE workspace.tenant_id = $1 AND workspace.workspace_id = $2
            ORDER BY resource.storage_resource_id
            LIMIT 1
            "#,
        )
        .bind(claimed.operation.tenant_id)
        .bind(claimed.operation.workspace_id)
        .bind(claimed.operation.operation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let Some(row) = row else {
            return Ok(WorkspaceInventoryObservation::Empty {
                inventory_digest: empty_inventory_digest(&claimed.operation.request_hash),
            });
        };
        let durable_account_id: ProviderAccountId =
            row.try_get("provider_account_id").map_err(map_sqlx)?;
        let durable_account_generation: i64 = row
            .try_get("provider_account_generation")
            .map_err(map_sqlx)?;
        let durable_writer_epoch: i64 = row.try_get("writer_epoch").map_err(map_sqlx)?;
        let durable_instance_generation: i64 =
            row.try_get("instance_generation").map_err(map_sqlx)?;
        if durable_account_id != claimed.operation.provider_account_id
            || durable_account_generation != claimed.operation.provider_account_generation
            || durable_writer_epoch != claimed.operation.expected_writer_epoch
            || durable_instance_generation != claimed.operation.expected_instance_generation
        {
            return Err(MoaError::StorageError(
                "workspace reconciliation durable binding crossed the claimed generation fence"
                    .to_string(),
            ));
        }
        let provider_name: String = row.try_get("provider").map_err(map_sqlx)?;
        let provider = self.storage_providers.get(&provider_name).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "workspace reaper has no storage provider for `{provider_name}`"
            ))
        })?;
        let inventory = provider
            .enumerate_account_storage(
                claimed.operation.provider_account_id,
                u64::try_from(claimed.operation.provider_account_generation).map_err(|_| {
                    MoaError::StorageError(
                        "reconciliation account generation is invalid".to_string(),
                    )
                })?,
            )
            .await?;
        validate_inventory_identity(
            &ProviderAccount {
                id: claimed.operation.provider_account_id,
                generation: u64::try_from(claimed.operation.provider_account_generation).map_err(
                    |_| {
                        MoaError::StorageError(
                            "reconciliation account generation is invalid".to_string(),
                        )
                    },
                )?,
                provider: provider_name.clone(),
            },
            &inventory,
        )?;
        let provider_reference: Option<String> =
            row.try_get("provider_reference").map_err(map_sqlx)?;
        let provisioning_operation_ids: Vec<Uuid> = row
            .try_get("provisioning_operation_ids")
            .map_err(map_sqlx)?;
        if inventory.resources.iter().any(|resource| {
            resource.verified_owner.is_none()
                && !provider_reference
                    .as_ref()
                    .is_some_and(|reference| resource.provider_reference == *reference)
        }) {
            return Err(MoaError::ProviderError(
                "workspace reconciliation inventory contains ownerless resources that cannot prove exact absence"
                    .to_string(),
            ));
        }
        let matching = inventory.resources.iter().filter(|resource| {
            reconciliation_resource_matches_claim(
                resource,
                claimed,
                provider_reference.as_deref(),
                &provisioning_operation_ids,
            )
        });
        let digests = matching
            .map(|resource| resource.evidence_digest.as_str())
            .collect::<BTreeSet<_>>();
        if digests.is_empty() {
            Ok(WorkspaceInventoryObservation::Empty {
                inventory_digest: empty_inventory_digest(&claimed.operation.request_hash),
            })
        } else {
            let mut digest = Sha256::new();
            digest.update(b"moa/workspace-reconciliation-inventory/v1\0");
            for evidence in digests {
                digest.update(evidence.as_bytes());
                digest.update([0]);
            }
            Ok(WorkspaceInventoryObservation::Present {
                inventory_digest: format!("sha256:{:x}", digest.finalize()),
            })
        }
    }
}

/// Returns whether a provider resource proves the exact claimed operation.
pub(super) fn reconciliation_resource_matches_claim(
    resource: &moa_core::types::sandbox_workspace::ProviderInventoryResource,
    claimed: &super::super::operations::ClaimedWorkspaceOperation,
    provider_reference: Option<&str>,
    provisioning_operation_ids: &[Uuid],
) -> bool {
    let reference_matches =
        provider_reference.is_none_or(|reference| resource.provider_reference == reference);
    if !reference_matches {
        return false;
    }
    let Some(owner) = &resource.verified_owner else {
        return provider_reference.is_some()
            && resource.kind != ProviderInventoryResourceKind::Compute;
    };
    owner.tenant_id == claimed.operation.tenant_id
        && owner.workspace_id == claimed.operation.workspace_id
        && owner.writer_epoch.is_some_and(|epoch| {
            i64::try_from(epoch).ok() == Some(claimed.operation.expected_writer_epoch)
        })
        && owner.instance_generation.is_some_and(|generation| {
            i64::try_from(generation).ok() == Some(claimed.operation.expected_instance_generation)
        })
        && (resource.kind != ProviderInventoryResourceKind::Compute
            || owner
                .provisioning_operation_id
                .is_some_and(|operation_id| provisioning_operation_ids.contains(&operation_id.0)))
}

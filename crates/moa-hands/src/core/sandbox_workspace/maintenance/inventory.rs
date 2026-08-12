//! Provider-inventory reconciliation and durable drift accounting.

use super::*;

impl WorkspaceMaintenanceCoordinator {
    /// Claims and reconciles one bounded shard of provider-account generations.
    pub async fn reconcile_claimed_provider_inventory_once(
        &self,
        batch_size: usize,
    ) -> Result<WorkspaceInventoryPass> {
        let accounts = self.claim_provider_accounts(batch_size).await?;
        let mut pass = WorkspaceInventoryPass {
            accounts: accounts.len() as u64,
            ..WorkspaceInventoryPass::default()
        };
        let mut first_error = None;
        for account in accounts {
            let result = self.reconcile_claimed_provider_account(&account).await;
            match result {
                Ok((resources, account_counts)) => {
                    pass.resources += resources;
                    pass.unresolved_findings += account_counts.values().sum::<u64>();
                    if !self.complete_provider_inventory_claim(&account).await? {
                        first_error.get_or_insert_with(|| MoaError::ExternalEffectUnknownOutcome {
                            operation_id: format!(
                                "provider-inventory-claim:{}:{}:{}",
                                account.id, account.generation, account.claim_generation
                            ),
                        });
                    }
                }
                Err(error) => {
                    let released = self
                        .fail_provider_inventory_claim(&account, &error.to_string())
                        .await?;
                    if !released {
                        first_error.get_or_insert_with(|| MoaError::ExternalEffectUnknownOutcome {
                            operation_id: format!(
                                "provider-inventory-claim:{}:{}:{}",
                                account.id, account.generation, account.claim_generation
                            ),
                        });
                    } else if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        let fleet_counts = self.unresolved_inventory_counts().await?;
        emit_complete_inventory_metrics(&fleet_counts);
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(pass)
    }

    async fn unresolved_inventory_counts(
        &self,
    ) -> Result<BTreeMap<(String, InventoryFindingKind), u64>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT account.provider, finding.finding_kind, count(*)::BIGINT AS count
            FROM moa.sandbox_provider_inventory_findings AS finding
            JOIN moa.sandbox_provider_accounts AS account
              ON account.provider_account_id = finding.provider_account_id
             AND account.generation = finding.provider_account_generation
            WHERE finding.quarantine_state <> 'resolved'
            GROUP BY account.provider, finding.finding_kind
            "#,
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let mut counts = BTreeMap::new();
        for row in rows {
            let provider: String = row.try_get("provider").map_err(map_sqlx)?;
            let kind = InventoryFindingKind::from_label(
                &row.try_get::<String, _>("finding_kind").map_err(map_sqlx)?,
            )?;
            let count: i64 = row.try_get("count").map_err(map_sqlx)?;
            counts.insert(
                (provider_metric_label(&provider).to_string(), kind),
                u64::try_from(count).map_err(|_| {
                    MoaError::StorageError("inventory finding count is negative".to_string())
                })?,
            );
        }
        conn.commit().await?;
        Ok(counts)
    }

    async fn reconcile_claimed_provider_account(
        &self,
        claimed: &ClaimedProviderAccount,
    ) -> Result<(u64, BTreeMap<(String, InventoryFindingKind), u64>)> {
        let account = claimed.account();
        let provider = self.storage_provider(&account.provider)?;
        let inventory = provider
            .enumerate_account_storage(account.id, account.generation)
            .await?;
        validate_inventory_identity(&account, &inventory)?;
        let durable = self.durable_inventory(&account).await?;
        let findings = compare_inventory(&account, &inventory, &durable);
        let mut observed_keys = HashSet::new();
        let mut counts = BTreeMap::new();
        for finding in findings {
            observed_keys.insert(finding.key());
            *counts
                .entry((
                    provider_metric_label(&account.provider).to_string(),
                    finding.kind,
                ))
                .or_default() += 1;
            self.upsert_inventory_finding(&finding).await?;
        }
        self.resolve_unseen_findings(
            &observed_keys,
            &HashSet::from([(account.id, account.generation)]),
        )
        .await?;
        Ok((inventory.resources.len() as u64, counts))
    }

    async fn claim_provider_accounts(
        &self,
        batch_size: usize,
    ) -> Result<Vec<ClaimedProviderAccount>> {
        if batch_size == 0 {
            return Err(MoaError::ValidationError(
                "provider inventory claim batch must be positive".to_string(),
            ));
        }
        let batch_size = i64::try_from(batch_size).map_err(|_| {
            MoaError::ValidationError("provider inventory batch overflows bigint".to_string())
        })?;
        let ttl_seconds = i64::try_from(self.reconciliation_claim_ttl.as_secs()).map_err(|_| {
            MoaError::ValidationError("provider inventory claim TTL overflows bigint".to_string())
        })?;
        let claim_token = Uuid::now_v7();
        let mut conn = maintenance_conn(&self.pool).await?;
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_provider_inventory_claims AS claim (
                provider_account_id, provider_account_generation, provider
            )
            SELECT provider_account_id, generation, provider
            FROM moa.sandbox_provider_accounts
            WHERE health <> 'disabled'
            ON CONFLICT (provider_account_id, provider_account_generation)
            DO UPDATE SET provider = EXCLUDED.provider
            WHERE claim.provider IS DISTINCT FROM EXCLUDED.provider
            "#,
        )
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let rows = sqlx::query(
            r#"
            WITH candidates AS (
                SELECT claim.provider_account_id, claim.provider_account_generation
                FROM moa.sandbox_provider_inventory_claims AS claim
                JOIN moa.sandbox_provider_accounts AS account
                  ON account.provider_account_id = claim.provider_account_id
                 AND account.generation = claim.provider_account_generation
                WHERE account.health <> 'disabled'
                  AND (claim.claim_token IS NULL OR claim.claim_expires_at <= now())
                ORDER BY claim.last_succeeded_at NULLS FIRST,
                         claim.provider, claim.provider_account_id,
                         claim.provider_account_generation
                LIMIT $1
                FOR UPDATE OF claim SKIP LOCKED
            )
            UPDATE moa.sandbox_provider_inventory_claims AS claim
            SET claim_generation = claim.claim_generation + 1,
                claim_owner = $2, claim_token = $3,
                claimed_at = now(),
                claim_expires_at = now() + make_interval(secs => $4),
                updated_at = now()
            FROM candidates
            WHERE claim.provider_account_id = candidates.provider_account_id
              AND claim.provider_account_generation = candidates.provider_account_generation
            RETURNING claim.provider_account_id, claim.provider_account_generation,
                      claim.provider, claim.claim_generation, claim.claim_token
            "#,
        )
        .bind(batch_size)
        .bind(self.inventory_claim_owner)
        .bind(claim_token)
        .bind(ttl_seconds)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let accounts = rows
            .iter()
            .map(|row| {
                let generation: i64 = row
                    .try_get("provider_account_generation")
                    .map_err(map_sqlx)?;
                let claim_generation: i64 = row.try_get("claim_generation").map_err(map_sqlx)?;
                Ok(ClaimedProviderAccount {
                    id: row.try_get("provider_account_id").map_err(map_sqlx)?,
                    generation: u64::try_from(generation).map_err(|_| {
                        MoaError::StorageError(
                            "provider-account generation is not positive".to_string(),
                        )
                    })?,
                    provider: row.try_get("provider").map_err(map_sqlx)?,
                    claim_generation: u64::try_from(claim_generation).map_err(|_| {
                        MoaError::StorageError(
                            "provider inventory claim generation is invalid".to_string(),
                        )
                    })?,
                    claim_token: row.try_get("claim_token").map_err(map_sqlx)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(accounts)
    }

    async fn complete_provider_inventory_claim(
        &self,
        claim: &ClaimedProviderAccount,
    ) -> Result<bool> {
        self.finish_provider_inventory_claim(claim, None).await
    }

    async fn fail_provider_inventory_claim(
        &self,
        claim: &ClaimedProviderAccount,
        error: &str,
    ) -> Result<bool> {
        self.finish_provider_inventory_claim(claim, Some(error))
            .await
    }

    async fn finish_provider_inventory_claim(
        &self,
        claim: &ClaimedProviderAccount,
        error: Option<&str>,
    ) -> Result<bool> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let affected = if let Some(error) = error {
            let error = truncate_inventory_error(error);
            sqlx::query(
                r#"
                UPDATE moa.sandbox_provider_inventory_claims
                SET claim_owner = NULL, claim_token = NULL,
                    claimed_at = NULL, claim_expires_at = NULL,
                    last_error = $6, last_error_at = now(), updated_at = now()
                WHERE provider_account_id = $1 AND provider_account_generation = $2
                  AND claim_generation = $3 AND claim_owner = $4 AND claim_token = $5
                  AND claim_expires_at > now()
                "#,
            )
            .bind(claim.id)
            .bind(i64::try_from(claim.generation).map_err(|_| {
                MoaError::StorageError("provider account generation overflows bigint".to_string())
            })?)
            .bind(i64::try_from(claim.claim_generation).map_err(|_| {
                MoaError::StorageError("inventory claim generation overflows bigint".to_string())
            })?)
            .bind(self.inventory_claim_owner)
            .bind(claim.claim_token)
            .bind(error)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?
            .rows_affected()
        } else {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_provider_inventory_claims
                SET claim_owner = NULL, claim_token = NULL,
                    claimed_at = NULL, claim_expires_at = NULL,
                    last_succeeded_at = now(), last_error = NULL,
                    last_error_at = NULL, updated_at = now()
                WHERE provider_account_id = $1 AND provider_account_generation = $2
                  AND claim_generation = $3 AND claim_owner = $4 AND claim_token = $5
                  AND claim_expires_at > now()
                "#,
            )
            .bind(claim.id)
            .bind(i64::try_from(claim.generation).map_err(|_| {
                MoaError::StorageError("provider account generation overflows bigint".to_string())
            })?)
            .bind(i64::try_from(claim.claim_generation).map_err(|_| {
                MoaError::StorageError("inventory claim generation overflows bigint".to_string())
            })?)
            .bind(self.inventory_claim_owner)
            .bind(claim.claim_token)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?
            .rows_affected()
        };
        conn.commit().await?;
        Ok(affected == 1)
    }

    /// Lists exact provider-account generations referenced by one tenant.
    pub(super) async fn tenant_provider_accounts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<ProviderAccount>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT account.provider_account_id, account.generation, account.provider
            FROM moa.sandbox_provider_accounts AS account
            JOIN (
                SELECT provider_account_id, provider_account_generation
                FROM moa.sandbox_workspaces WHERE tenant_id = $1
                UNION
                SELECT provider_account_id, provider_account_generation
                FROM moa.sandbox_storage_resources WHERE tenant_id = $1
            ) AS owned
              ON owned.provider_account_id = account.provider_account_id
             AND owned.provider_account_generation = account.generation
            ORDER BY account.provider, account.provider_account_id
            "#,
        )
        .bind(tenant_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let accounts = rows
            .iter()
            .map(|row| {
                let generation: i64 = row.try_get("generation").map_err(map_sqlx)?;
                Ok(ProviderAccount {
                    id: row.try_get("provider_account_id").map_err(map_sqlx)?,
                    generation: u64::try_from(generation).map_err(|_| {
                        MoaError::StorageError(
                            "tenant provider-account generation is invalid".to_string(),
                        )
                    })?,
                    provider: row.try_get("provider").map_err(map_sqlx)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(accounts)
    }

    /// Loads durable ownership rows for one provider-account generation.
    pub(super) async fn durable_inventory(
        &self,
        account: &ProviderAccount,
    ) -> Result<DurableInventory> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let storage_rows = sqlx::query(
            r#"
            SELECT provider_reference, tenant_id
            FROM moa.sandbox_storage_resources
            WHERE provider_account_id = $1 AND provider_account_generation = $2
              AND lifecycle_state <> 'deleted' AND provider_reference IS NOT NULL
            "#,
        )
        .bind(account.id)
        .bind(i64::try_from(account.generation).map_err(|_| {
            MoaError::StorageError("provider account generation overflows Postgres".to_string())
        })?)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let workspace_rows = sqlx::query(
            r#"
            SELECT workspace_id, tenant_id, writer_epoch, instance_generation,
                   provider_account_id, provider_account_generation
            FROM moa.sandbox_workspaces
            WHERE provider_account_id = $1 AND provider_account_generation = $2
              AND lifecycle_state <> 'deleted'
            "#,
        )
        .bind(account.id)
        .bind(i64::try_from(account.generation).map_err(|_| {
            MoaError::StorageError("provider account generation overflows Postgres".to_string())
        })?)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let hand_rows = sqlx::query(
            r#"
            SELECT lease.provisioning_operation_id, lease.tenant_id, lease.workspace_id
            FROM moa.hand_leases AS lease
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = lease.tenant_id
             AND workspace.workspace_id = lease.workspace_id
            WHERE workspace.provider_account_id = $1
              AND workspace.provider_account_generation = $2
              AND lease.status <> 'destroyed'
            "#,
        )
        .bind(account.id)
        .bind(i64::try_from(account.generation).map_err(|_| {
            MoaError::StorageError("provider account generation overflows Postgres".to_string())
        })?)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let storage = storage_rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("provider_reference")
                        .map_err(map_sqlx)?,
                    row.try_get::<TenantId, _>("tenant_id").map_err(map_sqlx)?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let workspaces = workspace_rows
            .iter()
            .map(|row| {
                let workspace_id: SandboxWorkspaceId =
                    row.try_get("workspace_id").map_err(map_sqlx)?;
                Ok((
                    workspace_id,
                    DurableWorkspaceOwner {
                        tenant_id: row.try_get("tenant_id").map_err(map_sqlx)?,
                        provider_account_id: row
                            .try_get("provider_account_id")
                            .map_err(map_sqlx)?,
                        provider_account_generation: row
                            .try_get("provider_account_generation")
                            .map_err(map_sqlx)?,
                        writer_epoch: row.try_get("writer_epoch").map_err(map_sqlx)?,
                        instance_generation: row
                            .try_get("instance_generation")
                            .map_err(map_sqlx)?,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let hands = hand_rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get("provisioning_operation_id").map_err(map_sqlx)?,
                    DurableHandOwner {
                        tenant_id: row.try_get("tenant_id").map_err(map_sqlx)?,
                        workspace_id: row.try_get("workspace_id").map_err(map_sqlx)?,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(DurableInventory {
            storage,
            workspaces,
            hands,
        })
    }

    /// Persists or refreshes one exact provider-inventory finding.
    pub(super) async fn upsert_inventory_finding(&self, finding: &InventoryFinding) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        sqlx::query(
            r#"
            INSERT INTO moa.sandbox_provider_inventory_findings (
                provider_account_id, provider_account_generation,
                resource_fingerprint, finding_kind, evidence_digest,
                quarantine_state, first_seen_at, last_seen_at
            ) VALUES ($1, $2, $3, $4, $5, 'quarantined', now(), now())
            ON CONFLICT (
                provider_account_id, provider_account_generation,
                resource_fingerprint, finding_kind
            ) DO UPDATE SET evidence_digest = EXCLUDED.evidence_digest,
                quarantine_state = 'quarantined', last_seen_at = now(),
                resolved_at = NULL, resolved_by = NULL,
                resolution_evidence_digest = NULL, updated_at = now()
            "#,
        )
        .bind(finding.account_id)
        .bind(i64::try_from(finding.account_generation).map_err(|_| {
            MoaError::StorageError("provider account generation overflows Postgres".to_string())
        })?)
        .bind(&finding.resource_fingerprint)
        .bind(finding.kind.as_str())
        .bind(&finding.evidence_digest)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        Ok(())
    }

    async fn resolve_unseen_findings(
        &self,
        observed: &HashSet<InventoryFindingKey>,
        observed_accounts: &HashSet<(ProviderAccountId, u64)>,
    ) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT provider_account_id, provider_account_generation,
                   resource_fingerprint, finding_kind
            FROM moa.sandbox_provider_inventory_findings
            WHERE quarantine_state <> 'resolved'
            "#,
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        for row in rows {
            let key = InventoryFindingKey {
                account_id: row.try_get("provider_account_id").map_err(map_sqlx)?,
                account_generation: u64::try_from(
                    row.try_get::<i64, _>("provider_account_generation")
                        .map_err(map_sqlx)?,
                )
                .map_err(|_| {
                    MoaError::StorageError(
                        "inventory finding account generation is invalid".to_string(),
                    )
                })?,
                resource_fingerprint: row.try_get("resource_fingerprint").map_err(map_sqlx)?,
                kind: InventoryFindingKind::from_label(
                    &row.try_get::<String, _>("finding_kind").map_err(map_sqlx)?,
                )?,
            };
            if !observed_accounts.contains(&(key.account_id, key.account_generation))
                || observed.contains(&key)
            {
                continue;
            }
            let digest = format!(
                "sha256:{:x}",
                Sha256::digest(
                    format!(
                        "moa/inventory-finding-resolution/v1\0{}\0{}",
                        key.resource_fingerprint,
                        key.kind.as_str()
                    )
                    .as_bytes()
                )
            );
            sqlx::query(
                r#"
                UPDATE moa.sandbox_provider_inventory_findings
                SET quarantine_state = 'resolved', resolved_at = now(),
                    resolved_by = 'workspace-maintenance',
                    resolution_evidence_digest = $5, updated_at = now()
                WHERE provider_account_id = $1
                  AND provider_account_generation = $2
                  AND resource_fingerprint = $3 AND finding_kind = $4
                  AND quarantine_state <> 'resolved'
                "#,
            )
            .bind(key.account_id)
            .bind(i64::try_from(key.account_generation).map_err(|_| {
                MoaError::StorageError("provider account generation overflows Postgres".to_string())
            })?)
            .bind(&key.resource_fingerprint)
            .bind(key.kind.as_str())
            .bind(digest)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx)?;
        }
        conn.commit().await?;
        Ok(())
    }
}
/// One configured provider-account generation included in maintenance.
pub(super) struct ProviderAccount {
    /// Stable provider-account identity.
    pub(super) id: ProviderAccountId,
    /// Exact admitted account generation.
    pub(super) generation: u64,
    /// Provider adapter name.
    pub(super) provider: String,
}

#[derive(Debug, Clone)]
struct ClaimedProviderAccount {
    id: ProviderAccountId,
    generation: u64,
    provider: String,
    claim_generation: u64,
    claim_token: Uuid,
}

impl ClaimedProviderAccount {
    fn account(&self) -> ProviderAccount {
        ProviderAccount {
            id: self.id,
            generation: self.generation,
            provider: self.provider.clone(),
        }
    }
}

fn truncate_inventory_error(error: &str) -> String {
    const LIMIT: usize = 2_048;
    error.chars().take(LIMIT).collect()
}

#[derive(Debug, Clone)]
/// Durable ownership fences for one workspace provider resource.
pub(super) struct DurableWorkspaceOwner {
    /// Tenant owner.
    pub(super) tenant_id: TenantId,
    /// Provider-account owner.
    pub(super) provider_account_id: ProviderAccountId,
    /// Exact provider-account generation.
    pub(super) provider_account_generation: i64,
    /// Workspace writer fence.
    pub(super) writer_epoch: i64,
    /// Workspace compute-instance fence.
    pub(super) instance_generation: i64,
}

#[derive(Debug)]
/// Durable hand ownership used to validate provider compute inventory.
pub(super) struct DurableHandOwner {
    /// Tenant owning the hand lease.
    pub(super) tenant_id: TenantId,
    /// Durable workspace attached to the hand lease.
    pub(super) workspace_id: SandboxWorkspaceId,
}

#[derive(Debug, Default)]
/// Durable inventory compared with one provider-account observation.
pub(super) struct DurableInventory {
    /// Mutable-storage references keyed by provider reference.
    pub(super) storage: HashMap<String, TenantId>,
    /// Workspace ownership keyed by logical workspace identity.
    pub(super) workspaces: HashMap<SandboxWorkspaceId, DurableWorkspaceOwner>,
    /// Hand ownership keyed by provisioning operation.
    pub(super) hands: HashMap<HandProvisioningOperationId, DurableHandOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Persistent provider inventory drift classification.
pub(super) enum InventoryFindingKind {
    /// Provider resource has no durable owner.
    Unknown,
    /// Multiple provider resources claim one durable owner.
    Duplicate,
    /// Resource belongs to a different provider-account generation.
    WrongAccount,
    /// Resource metadata disagrees with its durable tenant or workspace owner.
    WrongOwner,
    /// Durable resource is absent from the provider observation.
    Missing,
}

impl InventoryFindingKind {
    const ALL: [Self; 5] = [
        Self::Unknown,
        Self::Duplicate,
        Self::WrongAccount,
        Self::WrongOwner,
        Self::Missing,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Duplicate => "duplicate",
            Self::WrongAccount => "wrong_account",
            Self::WrongOwner => "wrong_owner",
            Self::Missing => "missing",
        }
    }

    fn from_label(label: &str) -> Result<Self> {
        match label {
            "unknown" => Ok(Self::Unknown),
            "duplicate" => Ok(Self::Duplicate),
            "wrong_account" => Ok(Self::WrongAccount),
            "wrong_owner" => Ok(Self::WrongOwner),
            "missing" => Ok(Self::Missing),
            _ => Err(MoaError::StorageError(
                "inventory finding carries an unknown classification".to_string(),
            )),
        }
    }

    const fn metric(self) -> SandboxWorkspaceInventoryDrift {
        match self {
            Self::Unknown => SandboxWorkspaceInventoryDrift::Unknown,
            Self::Duplicate => SandboxWorkspaceInventoryDrift::Duplicate,
            Self::WrongAccount => SandboxWorkspaceInventoryDrift::WrongAccount,
            Self::WrongOwner => SandboxWorkspaceInventoryDrift::WrongOwner,
            Self::Missing => SandboxWorkspaceInventoryDrift::Missing,
        }
    }
}

#[derive(Debug, Clone)]
/// One durable provider-inventory discrepancy and its evidence digest.
pub(super) struct InventoryFinding {
    account_id: ProviderAccountId,
    account_generation: u64,
    resource_fingerprint: String,
    pub(super) kind: InventoryFindingKind,
    evidence_digest: String,
}

impl InventoryFinding {
    fn key(&self) -> InventoryFindingKey {
        InventoryFindingKey {
            account_id: self.account_id,
            account_generation: self.account_generation,
            resource_fingerprint: self.resource_fingerprint.clone(),
            kind: self.kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InventoryFindingKey {
    account_id: ProviderAccountId,
    account_generation: u64,
    resource_fingerprint: String,
    kind: InventoryFindingKind,
}

/// Compares one exact provider observation with durable ownership records.
pub(super) fn compare_inventory(
    account: &ProviderAccount,
    inventory: &ProviderAccountStorageInventory,
    durable: &DurableInventory,
) -> Vec<InventoryFinding> {
    let mut findings = Vec::new();
    let mut observed_storage = HashSet::new();
    let mut owner_counts: HashMap<SandboxWorkspaceId, usize> = HashMap::new();
    for resource in &inventory.resources {
        if resource.kind == ProviderInventoryResourceKind::MutableFilesystem
            && durable.storage.contains_key(&resource.provider_reference)
        {
            observed_storage.insert(resource.provider_reference.clone());
        } else if resource.verified_owner.is_none() {
            findings.push(finding(account, resource, InventoryFindingKind::Unknown));
        }
        if let Some(owner) = &resource.verified_owner {
            *owner_counts.entry(owner.workspace_id).or_default() += 1;
            if resource.kind == ProviderInventoryResourceKind::Compute {
                let Some(provisioning_operation_id) = owner.provisioning_operation_id else {
                    findings.push(finding(account, resource, InventoryFindingKind::Unknown));
                    continue;
                };
                match durable.hands.get(&provisioning_operation_id) {
                    Some(hand_owner)
                        if hand_owner.tenant_id == owner.tenant_id
                            && hand_owner.workspace_id == owner.workspace_id => {}
                    Some(_) => {
                        findings.push(finding(account, resource, InventoryFindingKind::WrongOwner));
                        continue;
                    }
                    None => {
                        findings.push(finding(account, resource, InventoryFindingKind::Unknown));
                        continue;
                    }
                }
            }
            match durable.workspaces.get(&owner.workspace_id) {
                None => findings.push(finding(account, resource, InventoryFindingKind::Unknown)),
                Some(durable_owner)
                    if durable_owner.provider_account_id != account.id
                        || u64::try_from(durable_owner.provider_account_generation).ok()
                            != Some(account.generation) =>
                {
                    findings.push(finding(
                        account,
                        resource,
                        InventoryFindingKind::WrongAccount,
                    ));
                }
                Some(durable_owner) if durable_owner.tenant_id != owner.tenant_id => {
                    findings.push(finding(account, resource, InventoryFindingKind::WrongOwner));
                }
                Some(durable_owner)
                    if owner.writer_epoch.is_some_and(|epoch| {
                        i64::try_from(epoch).ok() != Some(durable_owner.writer_epoch)
                    }) || owner.instance_generation.is_some_and(|generation| {
                        i64::try_from(generation).ok() != Some(durable_owner.instance_generation)
                    }) =>
                {
                    findings.push(finding(account, resource, InventoryFindingKind::WrongOwner));
                }
                Some(_) => {}
            }
        }
    }
    for resource in &inventory.resources {
        if resource
            .verified_owner
            .as_ref()
            .is_some_and(|owner| owner_counts.get(&owner.workspace_id).copied().unwrap_or(0) > 1)
        {
            findings.push(finding(account, resource, InventoryFindingKind::Duplicate));
        }
    }
    for reference in durable.storage.keys() {
        if !observed_storage.contains(reference) {
            findings.push(InventoryFinding {
                account_id: account.id,
                account_generation: account.generation,
                resource_fingerprint: format!(
                    "sha256:{:x}",
                    Sha256::digest(format!("missing-provider-resource-v1\0{reference}").as_bytes())
                ),
                kind: InventoryFindingKind::Missing,
                evidence_digest: format!(
                    "sha256:{:x}",
                    Sha256::digest(format!("missing-provider-evidence-v1\0{reference}").as_bytes())
                ),
            });
        }
    }
    findings.sort_by(|left, right| left.resource_fingerprint.cmp(&right.resource_fingerprint));
    findings.dedup_by(|left, right| {
        left.resource_fingerprint == right.resource_fingerprint && left.kind == right.kind
    });
    findings
}

/// Builds one normalized provider-inventory discrepancy.
pub(super) fn finding(
    account: &ProviderAccount,
    resource: &moa_core::types::sandbox_workspace::ProviderInventoryResource,
    kind: InventoryFindingKind,
) -> InventoryFinding {
    InventoryFinding {
        account_id: account.id,
        account_generation: account.generation,
        resource_fingerprint: resource.resource_fingerprint.clone(),
        kind,
        evidence_digest: resource.evidence_digest.clone(),
    }
}

/// Validates that an inventory response belongs to the requested account generation.
pub(super) fn validate_inventory_identity(
    account: &ProviderAccount,
    inventory: &ProviderAccountStorageInventory,
) -> Result<()> {
    if inventory.provider_account_id != account.id
        || inventory.provider_account_generation != account.generation
    {
        return Err(MoaError::ProviderError(
            "provider inventory returned another account generation".to_string(),
        ));
    }
    let mut fingerprints = HashSet::new();
    if inventory.resources.iter().any(|resource| {
        resource.provider_reference.trim().is_empty()
            || resource.resource_fingerprint.trim().is_empty()
            || resource.evidence_digest.trim().is_empty()
            || !fingerprints.insert(resource.resource_fingerprint.as_str())
    }) {
        return Err(MoaError::ProviderError(
            "provider inventory contains malformed or duplicate resource evidence".to_string(),
        ));
    }
    Ok(())
}

fn emit_complete_inventory_metrics(counts: &BTreeMap<(String, InventoryFindingKind), u64>) {
    for provider in ["local", "daytona", "e2b", "other"] {
        for kind in InventoryFindingKind::ALL {
            record_workspace_inventory_drift(
                provider,
                kind.metric(),
                counts
                    .get(&(provider.to_string(), kind))
                    .copied()
                    .unwrap_or(0),
            );
        }
    }
}

/// Returns the bounded provider label used by workspace metrics.
pub(super) fn provider_metric_label(provider: &str) -> &'static str {
    match provider {
        "local" => "local",
        "daytona" => "daytona",
        "e2b" => "e2b",
        _ => "other",
    }
}

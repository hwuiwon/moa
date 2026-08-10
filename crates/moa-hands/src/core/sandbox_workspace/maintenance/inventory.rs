//! Provider-inventory reconciliation and durable drift accounting.

use super::*;

impl WorkspaceMaintenanceCoordinator {
    /// Reconciles every persisted provider-account generation and persists drift.
    pub async fn reconcile_provider_inventory_once(&self) -> Result<WorkspaceInventoryPass> {
        let accounts = self.provider_accounts().await?;
        let mut observed_keys = HashSet::new();
        let mut observed_accounts = HashSet::new();
        let mut pass = WorkspaceInventoryPass::default();
        let mut counts: BTreeMap<(String, InventoryFindingKind), u64> = BTreeMap::new();
        for account in accounts {
            let provider = self.storage_provider(&account.provider)?;
            let inventory = provider
                .enumerate_account_storage(account.id, account.generation)
                .await?;
            validate_inventory_identity(&account, &inventory)?;
            observed_accounts.insert((account.id, account.generation));
            let durable = self.durable_inventory(&account).await?;
            pass.accounts += 1;
            pass.resources += inventory.resources.len() as u64;
            let findings = compare_inventory(&account, &inventory, &durable);
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
        }
        self.resolve_unseen_findings(&observed_keys, &observed_accounts)
            .await?;
        pass.unresolved_findings = counts.values().sum();
        emit_complete_inventory_metrics(&counts);
        Ok(pass)
    }

    /// Fences access, removes all external sandbox state, and returns exact absence evidence.
    ///
    /// Relational metadata is deliberately untouched after the access fence. A
    /// provider outage therefore leaves every ownership and reconciliation row
    /// available for the next Restate replay.
    async fn provider_accounts(&self) -> Result<Vec<ProviderAccount>> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT provider_account_id, generation, provider FROM moa.sandbox_provider_accounts WHERE health <> 'disabled' ORDER BY provider, provider_account_id",
        )
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
                            "provider-account generation is not positive".to_string(),
                        )
                    })?,
                    provider: row.try_get("provider").map_err(map_sqlx)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(accounts)
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
            WHERE lifecycle_state <> 'deleted'
            "#,
        )
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

// Knowledge credential-owner fake and recorded operations.

/// One stored credential version in the in-memory credential store double.
#[derive(Debug, Clone)]
struct FakeCredentialVersion {
    tenant_id: TenantId,
    connection_uid: Uuid,
    material: String,
    active: bool,
    revoked: bool,
}

/// In-memory stand-in for the durable credential owner.
///
/// Mirrors the properties the service depends on: references are opaque
/// identifiers with no parseable address, material is keyed by the owning
/// connection rather than by provider account, and every operation records the
/// acting principal so tests can assert who a resolution was attributed to.
#[derive(Debug, Clone, Default)]
struct FakeKnowledgeCredentialStore {
    state: Arc<Mutex<FakeCredentialState>>,
}

#[derive(Debug, Default)]
struct FakeCredentialState {
    versions: HashMap<Uuid, FakeCredentialVersion>,
    staged_by_operation: HashMap<(String, Uuid), (Uuid, Option<Uuid>)>,
    operations: Vec<(String, CredentialPrincipal)>,
    status_batch_calls: usize,
    fail_rollback_with_revoked_prior: bool,
}

impl FakeKnowledgeCredentialStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeCredentialState> {
        self.state
            .lock()
            .expect("fake credential store should not be poisoned")
    }

    fn stored_account_count(&self) -> usize {
        self.lock().versions.len()
    }

    fn status_batch_calls(&self) -> usize {
        self.lock().status_batch_calls
    }

    fn fail_rollback_with_revoked_prior(&self) {
        self.lock().fail_rollback_with_revoked_prior = true;
    }

    fn active_reference_for_connection(&self, connection_uid: Uuid) -> Option<String> {
        self.lock()
            .versions
            .iter()
            .find(|(_, version)| {
                version.connection_uid == connection_uid && version.active && !version.revoked
            })
            .map(|(reference, _)| reference.to_string())
    }

    /// Returns the opaque reference issued for one connection, if any.
    fn reference_for_connection(&self, connection_uid: Uuid) -> Option<String> {
        self.lock()
            .versions
            .iter()
            .find(|(_, version)| version.connection_uid == connection_uid)
            .map(|(reference, _)| reference.to_string())
    }

    /// Returns the connection a stored reference belongs to.
    fn connection_for_reference(&self, reference: &str) -> Option<Uuid> {
        let reference = Uuid::parse_str(reference).ok()?;
        self.lock()
            .versions
            .get(&reference)
            .map(|version| version.connection_uid)
    }

    /// Returns every reference the store has issued, in creation order.
    fn references(&self) -> Vec<String> {
        let state = self.lock();
        let mut references: Vec<(Uuid, String)> = state
            .versions
            .keys()
            .map(|reference| (*reference, reference.to_string()))
            .collect();
        references.sort_by_key(|(uid, _)| *uid);
        references
            .into_iter()
            .map(|(_, reference)| reference)
            .collect()
    }

    /// Returns the references that have been revoked, in creation order.
    fn revoked_references(&self) -> Vec<String> {
        let state = self.lock();
        let mut revoked: Vec<Uuid> = state
            .versions
            .iter()
            .filter(|(_, version)| version.revoked)
            .map(|(reference, _)| *reference)
            .collect();
        revoked.sort_unstable();
        revoked
            .into_iter()
            .map(|reference| reference.to_string())
            .collect()
    }

    /// Returns the principals recorded for operations whose id ends with `step`.
    fn principals_for_step(&self, step: &str) -> Vec<CredentialPrincipal> {
        let suffix = format!(":{step}");
        self.lock()
            .operations
            .iter()
            .filter(|(operation_id, _)| operation_id.ends_with(&suffix))
            .map(|(_, principal)| *principal)
            .collect()
    }

    fn record(&self, operation_id: String, principal: CredentialPrincipal) {
        self.lock().operations.push((operation_id, principal));
    }

    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<String, KnowledgeServiceError> {
        let staged = self
            .stage_linked_account(tenant_id, connection_uid, caller, account)
            .await?;
        let reference = staged.vault_candidate_reference().ok_or_else(|| {
            KnowledgeServiceError::Credential(
                "fake managed credential did not produce a vault receipt".to_string(),
            )
        })?;
        self.activate_staged_linked_account(&staged, caller).await?;
        Ok(reference)
    }
}

#[async_trait]
impl KnowledgeCredentialStore for FakeKnowledgeCredentialStore {
    async fn stage_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<StagedKnowledgeCredential, KnowledgeServiceError> {
        let operation_id = caller.step("credential-stage");
        self.record(operation_id.clone(), caller.principal());
        match ManagedParentDefinition::for_knowledge_provider(account.provider.as_str())? {
            ManagedParentDefinition::KnowledgeNango => {
                if account.credential_material.is_some() {
                    return Err(KnowledgeServiceError::InvalidRequest(
                        "fake Nango account returned credential material".to_string(),
                    ));
                }
                return Ok(StagedKnowledgeCredential::ProviderNative);
            }
            ManagedParentDefinition::KnowledgeMerge => {}
        }
        let material = account.credential_material.clone().ok_or_else(|| {
            KnowledgeServiceError::InvalidRequest(
                "fake Merge account omitted credential material".to_string(),
            )
        })?;
        let mut state = self.lock();
        let stage_key = (operation_id, connection_uid);
        let (reference, prior) = if let Some(receipt) = state.staged_by_operation.get(&stage_key) {
            *receipt
        } else {
            let prior = state
                .versions
                .iter()
                .find(|(_, version)| {
                    version.tenant_id == tenant_id
                        && version.connection_uid == connection_uid
                        && version.active
                        && !version.revoked
                })
                .map(|(reference, _)| *reference);
            let reference = Uuid::now_v7();
            state
                .staged_by_operation
                .insert(stage_key, (reference, prior));
            state.versions.insert(
                reference,
                FakeCredentialVersion {
                    tenant_id,
                    connection_uid,
                    material,
                    active: false,
                    revoked: false,
                },
            );
            (reference, prior)
        };
        Ok(StagedKnowledgeCredential::Managed {
            staging: CredentialStagingToken::new(
                CredentialRef::from_uuid(reference),
                CredentialIdentity {
                    tenant_id,
                    connection_uid,
                    kind: CredentialKind::ProviderApiKey,
                    slot_name: CredentialSlotName::PRIMARY,
                },
                1,
                prior.map(CredentialRef::from_uuid),
            ),
        })
    }

    async fn activate_staged_linked_account(
        &self,
        staged: &StagedKnowledgeCredential,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        self.record(caller.step("credential-activate"), caller.principal());
        let StagedKnowledgeCredential::Managed { staging } = staged else {
            return Ok(());
        };
        let mut state = self.lock();
        for version in state.versions.values_mut().filter(|version| {
            version.tenant_id == staging.identity().tenant_id
                && version.connection_uid == staging.identity().connection_uid
        }) {
            version.active = false;
        }
        let candidate = state
            .versions
            .get_mut(&staging.staged_reference().as_uuid())
            .ok_or_else(|| {
                KnowledgeServiceError::Credential("fake staged credential missing".to_string())
            })?;
        candidate.active = true;
        Ok(())
    }

    async fn rollback_linked_account_activation(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        candidate_credential_ref: &str,
        previous_credential_ref: Option<&str>,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        self.record(
            caller.step("credential-rollback-activation"),
            caller.principal(),
        );
        let Some(candidate) = Uuid::parse_str(candidate_credential_ref).ok() else {
            return Ok(());
        };
        let previous = previous_credential_ref.and_then(|value| Uuid::parse_str(value).ok());
        let mut state = self.lock();
        if state.fail_rollback_with_revoked_prior
            && let Some(previous) = previous
        {
            if let Some(version) = state.versions.get_mut(&previous) {
                version.active = false;
                version.revoked = true;
            }
            return Err(KnowledgeServiceError::Credential(
                "fake prior credential is revoked".to_string(),
            ));
        }
        if let Some(version) = state.versions.get_mut(&candidate)
            && version.tenant_id == tenant_id
            && version.connection_uid == connection_uid
        {
            version.active = false;
            version.revoked = true;
        }
        if let Some(previous) = previous
            && let Some(version) = state.versions.get_mut(&previous)
        {
            version.active = true;
        }
        Ok(())
    }

    async fn resolve_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<RedactedSecret>, KnowledgeServiceError> {
        self.record(caller.step("credential-resolve"), caller.principal());
        if ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())?
            == ManagedParentDefinition::KnowledgeNango
        {
            return Ok(None);
        }
        let state = self.lock();
        if let Some(version) = state.versions.values().find(|version| {
            version.tenant_id == tenant_id
                && version.connection_uid == connection.connection_uid
                && version.active
                && !version.revoked
        }) {
            return Ok(Some(RedactedSecret::new(version.material.clone())));
        }
        Err(KnowledgeServiceError::Credential(
            "fake credential is not resolvable".to_string(),
        ))
    }

    async fn revoke_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<bool, KnowledgeServiceError> {
        self.record(
            caller.step("credential-revoke-connection"),
            caller.principal(),
        );
        if ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())?
            == ManagedParentDefinition::KnowledgeNango
        {
            return Ok(false);
        }
        let mut state = self.lock();
        let mut changed = false;
        for version in state.versions.values_mut().filter(|version| {
            version.tenant_id == tenant_id && version.connection_uid == connection.connection_uid
        }) {
            if !version.revoked {
                version.revoked = true;
                version.active = false;
                changed = true;
            }
        }
        Ok(changed)
    }

    async fn credential_statuses(
        &self,
        tenant_id: TenantId,
        connections: &[&KnowledgeConnection],
        _caller: &KnowledgeCaller,
    ) -> Result<Vec<Option<String>>, KnowledgeServiceError> {
        let mut state = self.lock();
        state.status_batch_calls += 1;
        Ok(connections
            .iter()
            .map(|connection| {
                match ManagedParentDefinition::for_knowledge_provider(
                    connection.provider.as_str(),
                ) {
                    Ok(ManagedParentDefinition::KnowledgeNango) => None,
                    Ok(ManagedParentDefinition::KnowledgeMerge) => Some(
                        if state.versions.values().any(|version| {
                            version.tenant_id == tenant_id
                                && version.connection_uid == connection.connection_uid
                                && version.active
                                && !version.revoked
                        }) {
                            "present"
                        } else {
                            "missing"
                        }
                        .to_string(),
                    ),
                    Err(_) => Some("missing".to_string()),
                }
            })
            .collect())
    }
}

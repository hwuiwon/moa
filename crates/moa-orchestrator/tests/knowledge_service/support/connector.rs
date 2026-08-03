// Generic connector-parent lifecycle fake.

/// In-memory generic-connector lifecycle used by knowledge service tests.
type FakeManagedParentClaims = HashMap<(TenantId, String), (String, ConnectorConnectionId, bool)>;

#[derive(Clone, Debug, Default)]
struct FakeKnowledgeConnectorConnections {
    connections: Arc<Mutex<HashMap<ConnectorConnectionId, ConnectorConnection>>>,
    claims: Arc<Mutex<FakeManagedParentClaims>>,
}

impl FakeKnowledgeConnectorConnections {
    fn connection(&self, connection_id: ConnectorConnectionId) -> Option<ConnectorConnection> {
        self.connections
            .lock()
            .expect("fake connector connection state should not be poisoned")
            .get(&connection_id)
            .cloned()
    }

    fn connection_for_claim(request: &ManagedParentClaimRequest) -> ConnectorConnection {
        let now = moa_test_support::fixtures::pg_now();
        ConnectorConnection {
            connection_id: request.connection_id,
            tenant_id: request.tenant_id,
            display_name: request.display_name.clone(),
            definition: request.definition.definition_ref(),
            origin: None,
            non_secret_config: json!({}),
            generation: ConnectionGeneration::new(1)
                .expect("fixture generation should be positive"),
            status: ParentConnectionStatus::PendingAuth,
            health: ConnectionHealth::Pending,
            health_reason: None,
            created_by_identity_id: request.owner_identity_id,
            owner_identity_id: request.owner_identity_id,
            created_at: now,
            updated_at: now,
        }
    }

    fn invalid(message: &str) -> moa_connectors::Error {
        moa_connectors::Error::InvalidContract {
            message: message.to_string(),
        }
    }

    fn legacy_active(
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> ConnectorConnection {
        let now = moa_test_support::fixtures::pg_now();
        ConnectorConnection {
            connection_id,
            tenant_id,
            display_name: "fixture knowledge connection".to_string(),
            definition: ManagedParentDefinition::for_knowledge_provider(PROVIDER)
                .expect("fixture provider should have a managed definition")
                .definition_ref(),
            non_secret_config: json!({}),
            origin: None,
            generation: ConnectionGeneration::new(1)
                .expect("fixture generation should be positive"),
            status: ParentConnectionStatus::Active,
            health: ConnectionHealth::Ready,
            health_reason: None,
            created_by_identity_id: None,
            owner_identity_id: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[async_trait]
impl KnowledgeConnectorConnections for FakeKnowledgeConnectorConnections {
    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> moa_connectors::Result<ManagedParentClaim> {
        let claim_key = (request.tenant_id, request.operation_id.clone());
        if let Some((request_hash, connection_id, parent_created_by_claim)) = self
            .claims
            .lock()
            .expect("fake connector claim state should not be poisoned")
            .get(&claim_key)
            .cloned()
        {
            if request_hash != request.request_hash || connection_id != request.connection_id {
                return Err(moa_connectors::Error::ManagedParentClaimConflict {
                    connection_id: request.connection_id,
                });
            }
            let connection = self
                .connections
                .lock()
                .expect("fake connector connection state should not be poisoned")
                .get(&connection_id)
                .cloned()
                .ok_or(moa_connectors::Error::ConnectionNotFound { connection_id })?;
            return Ok(ManagedParentClaim {
                connection,
                parent_created_by_claim,
            });
        }
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        if let Some(connection) = connections.get(&request.connection_id) {
            if connection.tenant_id != request.tenant_id
                || connection.definition != request.definition.definition_ref()
            {
                return Err(moa_connectors::Error::ManagedParentMismatch {
                    connection_id: request.connection_id,
                    field: "identity",
                });
            }
            let claim = ManagedParentClaim {
                connection: connection.clone(),
                parent_created_by_claim: false,
            };
            self.claims
                .lock()
                .expect("fake connector claim state should not be poisoned")
                .insert(
                    claim_key,
                    (request.request_hash, request.connection_id, false),
                );
            return Ok(claim);
        }
        if request.owner_identity_id.is_none() {
            return Err(moa_connectors::Error::ManagedParentOwnerRequired {
                connection_id: request.connection_id,
            });
        }
        let connection = Self::connection_for_claim(&request);
        connections.insert(request.connection_id, connection.clone());
        self.claims
            .lock()
            .expect("fake connector claim state should not be poisoned")
            .insert(
                claim_key,
                (request.request_hash, request.connection_id, true),
            );
        Ok(ManagedParentClaim {
            connection,
            parent_created_by_claim: true,
        })
    }

    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> moa_connectors::Result<ConnectorConnection> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections.get_mut(&request.connection_id).ok_or(
            moa_connectors::Error::ConnectionNotFound {
                connection_id: request.connection_id,
            },
        )?;
        if connection.tenant_id != request.tenant_id
            || connection.definition != request.definition.definition_ref()
        {
            return Err(Self::invalid("fake managed-parent identity mismatch"));
        }
        if connection.generation != request.expected_generation {
            return Err(moa_connectors::Error::GenerationConflict {
                expected: request.expected_generation,
                actual: connection.generation,
            });
        }
        if matches!(
            connection.status,
            ParentConnectionStatus::Disconnecting | ParentConnectionStatus::Deleted
        ) {
            return Err(Self::invalid("fake managed parent is in teardown"));
        }
        connection.status = ParentConnectionStatus::Active;
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(connection.clone())
    }

    async fn advance_credential_generation(
        &self,
        request: CredentialGenerationFenceRequest,
    ) -> moa_connectors::Result<ConnectorConnection> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections.get_mut(&request.connection_id).ok_or(
            moa_connectors::Error::ConnectionNotFound {
                connection_id: request.connection_id,
            },
        )?;
        if connection.tenant_id != request.tenant_id {
            return Err(Self::invalid("fake managed-parent tenant mismatch"));
        }
        if connection.generation != request.expected_generation {
            return Err(moa_connectors::Error::GenerationConflict {
                expected: request.expected_generation,
                actual: connection.generation,
            });
        }
        connection.generation = connection.generation.next()?;
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(connection.clone())
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> moa_connectors::Result<Option<ConnectorConnection>> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        if let Some(connection) = connections.get(&connection_id) {
            return Ok((connection.tenant_id == tenant_id).then(|| connection.clone()));
        }
        let connection = Self::legacy_active(tenant_id, connection_id);
        connections.insert(connection_id, connection.clone());
        Ok(Some(connection))
    }

    async fn disconnect(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections
            .get_mut(&connection_id)
            .ok_or(moa_connectors::Error::ConnectionNotFound { connection_id })?;
        if connection.tenant_id != tenant_id {
            return Err(moa_connectors::Error::ConnectionNotFound { connection_id });
        }
        if connection.generation != expected_generation {
            return Err(moa_connectors::Error::GenerationConflict {
                expected: expected_generation,
                actual: connection.generation,
            });
        }
        if connection.status != ParentConnectionStatus::Disconnecting {
            connection.status = connection
                .status
                .transition(ParentConnectionStatus::Disconnecting)?;
        }
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(connection.clone())
    }

    async fn delete(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections
            .get_mut(&connection_id)
            .ok_or(moa_connectors::Error::ConnectionNotFound { connection_id })?;
        if connection.tenant_id != tenant_id || connection.generation != expected_generation {
            return Err(Self::invalid("fake managed-parent delete fence mismatch"));
        }
        if connection.status != ParentConnectionStatus::Deleted {
            connection.status = connection
                .status
                .transition(ParentConnectionStatus::Deleted)?;
        }
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(connection.clone())
    }

    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> moa_connectors::Result<ManagedParentDeleteOutcome> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections.get_mut(&request.connection_id).ok_or(
            moa_connectors::Error::ConnectionNotFound {
                connection_id: request.connection_id,
            },
        )?;
        if connection.tenant_id != request.tenant_id {
            return Err(Self::invalid("fake managed-parent tenant mismatch"));
        }
        if connection.status == ParentConnectionStatus::Deleted {
            return Ok(ManagedParentDeleteOutcome::AlreadyDeleted(
                connection.clone(),
            ));
        }
        connection.status = ParentConnectionStatus::Deleted;
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(ManagedParentDeleteOutcome::Deleted(connection.clone()))
    }

    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> moa_connectors::Result<ConnectorConnection> {
        let mut connections = self
            .connections
            .lock()
            .expect("fake connector connection state should not be poisoned");
        let connection = connections
            .get_mut(&connection_id)
            .ok_or(moa_connectors::Error::ConnectionNotFound { connection_id })?;
        if connection.tenant_id != tenant_id || connection.generation != expected_generation {
            return Err(Self::invalid("fake managed-parent health fence mismatch"));
        }
        connection.health = health;
        connection.health_reason = reason;
        connection.updated_at = moa_test_support::fixtures::pg_now();
        Ok(connection.clone())
    }
}

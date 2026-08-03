//! Authenticated per-session installed-connector catalog composition.

use std::sync::Arc;

use moa_connectors::catalog::{InstalledConnectorCatalog, InstalledConnectorCatalogQuery};
use moa_connectors::executor::ConnectorActionRuntime;
use moa_core::error::MoaError;
use moa_core::traits::Identity;
use moa_core::types::agent::AgentContext;
use moa_core::types::identifiers::TenantId;
use moa_core::types::session::SessionMeta;
use moa_hands::{ToolCatalogPin, ToolCatalogSnapshot, ToolRouter};
use serde_json::Value;

/// Failure while deriving one authenticated connector-action catalog.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ScopedConnectorCatalogError {
    /// The authenticated principal and authoritative owner disagree on tenant scope.
    #[error("connector catalog caller tenant does not match the authoritative owner")]
    TenantMismatch,
    /// The session's pinned agent policy could not be interpreted.
    #[error("connector catalog agent policy is invalid: {0}")]
    AgentPolicy(#[source] MoaError),
    /// The governed installed catalog rejected authorization or durable state.
    #[error("connector catalog projection failed: {0}")]
    Installed(#[source] moa_connectors::Error),
    /// The immutable deployment catalog and installed projection could not be joined.
    #[error("connector catalog overlay failed: {0}")]
    Overlay(#[source] MoaError),
}

impl ScopedConnectorCatalogError {
    /// Converts catalog construction failure into the orchestrator's common error contract.
    pub(crate) fn into_moa_error(self) -> MoaError {
        match self {
            Self::TenantMismatch => MoaError::PermissionDenied(self.to_string()),
            Self::Installed(moa_connectors::Error::AuthorizationDenied) => {
                MoaError::PermissionDenied("connector use authorization denied".to_string())
            }
            Self::Installed(moa_connectors::Error::AuthorizationUnavailable) => {
                MoaError::ProviderError("connector use authorization unavailable".to_string())
            }
            other => MoaError::ValidationError(other.to_string()),
        }
    }
}

/// One immutable deployment catalog plus the exact pin derived from it.
#[derive(Clone)]
pub(crate) struct ScopedToolCatalog {
    snapshot: Arc<ToolCatalogSnapshot>,
    pin: ToolCatalogPin,
}

impl ScopedToolCatalog {
    fn from_snapshot(
        snapshot: Arc<ToolCatalogSnapshot>,
    ) -> Result<Self, ScopedConnectorCatalogError> {
        let pin = snapshot
            .pin()
            .map_err(ScopedConnectorCatalogError::Overlay)?;
        Ok(Self { snapshot, pin })
    }

    /// Returns the exact immutable snapshot used for schemas and dispatch.
    pub(crate) const fn snapshot(&self) -> &Arc<ToolCatalogSnapshot> {
        &self.snapshot
    }

    /// Returns the catalog pin derived from the same immutable snapshot.
    pub(crate) const fn pin(&self) -> &ToolCatalogPin {
        &self.pin
    }

    /// Returns model-visible schemas from the same immutable snapshot.
    pub(crate) fn schemas(&self) -> Arc<Vec<Value>> {
        self.snapshot.tool_schema_snapshot()
    }
}

/// Builds ephemeral connector-action overlays from authenticated agent bindings.
#[derive(Clone)]
pub(crate) struct ScopedConnectorCatalogProvider {
    router: Arc<ToolRouter>,
    installed_catalog: Arc<dyn InstalledConnectorCatalog>,
    connector_runtime: Arc<dyn ConnectorActionRuntime>,
}

impl ScopedConnectorCatalogProvider {
    /// Creates the centralized scoped-catalog composition boundary.
    #[must_use]
    pub(crate) fn new(
        router: Arc<ToolRouter>,
        installed_catalog: Arc<dyn InstalledConnectorCatalog>,
        connector_runtime: Arc<dyn ConnectorActionRuntime>,
    ) -> Self {
        Self {
            router,
            installed_catalog,
            connector_runtime,
        }
    }

    /// Returns the immutable deployment catalog without claiming tenant scope.
    pub(crate) fn deployment_catalog(
        &self,
    ) -> Result<ScopedToolCatalog, ScopedConnectorCatalogError> {
        ScopedToolCatalog::from_snapshot(self.router.activated_catalog())
    }

    /// Builds one catalog from an authenticated caller and authoritative session state.
    pub(crate) async fn for_session(
        &self,
        caller: &Identity,
        session: &SessionMeta,
    ) -> Result<ScopedToolCatalog, ScopedConnectorCatalogError> {
        self.for_agent_context(caller, session.tenant_id, session.agent_context.as_ref())
            .await
    }

    /// Builds one catalog from an authenticated caller and exact pinned agent context.
    pub(crate) async fn for_agent_context(
        &self,
        caller: &Identity,
        tenant_id: TenantId,
        agent_context: Option<&AgentContext>,
    ) -> Result<ScopedToolCatalog, ScopedConnectorCatalogError> {
        if caller.tenant_id != tenant_id {
            return Err(ScopedConnectorCatalogError::TenantMismatch);
        }

        let Some(agent_context) = agent_context else {
            return self.deployment_catalog();
        };
        let policy = agent_context
            .parsed_policy_snapshot()
            .map_err(ScopedConnectorCatalogError::AgentPolicy)?;
        let bindings = policy.action_policy.connector_bindings;
        if bindings.is_empty() {
            return self.deployment_catalog();
        }

        let installed = self
            .installed_catalog
            .snapshot(InstalledConnectorCatalogQuery::new(
                caller.clone(),
                bindings.iter().map(|binding| binding.connection_id),
            ))
            .await
            .map_err(ScopedConnectorCatalogError::Installed)?;
        let base = self.router.activated_catalog();
        let snapshot = self
            .router
            .installed_connector_overlay(
                base.as_ref(),
                &installed,
                &bindings,
                Arc::clone(&self.connector_runtime),
            )
            .map_err(ScopedConnectorCatalogError::Overlay)?;
        ScopedToolCatalog::from_snapshot(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_artifacts::connector::RuntimeConnectorDefinitionV1;
    use moa_connectors::catalog::{
        ConnectorUseAuthorizer, GovernedInstalledConnectorCatalog, InstalledConnectorCatalogSource,
    };
    use moa_connectors::domain::{
        CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth,
        ConnectionStatus, ConnectorConnection, InstalledActionBinding, InstalledActionBindingId,
    };
    use moa_connectors::executor::{
        ConnectorActionInvocation, ConnectorActionRuntime, RawConnectorActionResult,
    };
    use moa_core::traits::{Identity, IdentityType};
    use moa_core::types::action_policy::ActionPolicyEffect;
    use moa_core::types::agent::{
        AgentActionPolicy, AgentConnectorBinding, AgentContext, AgentPolicySnapshot,
    };
    use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
    use moa_hands::{ToolExecution, ToolRegistry, ToolRouter};
    use serde_json::json;
    use uuid::Uuid;

    use super::{ScopedConnectorCatalogError, ScopedConnectorCatalogProvider};

    #[derive(Clone)]
    struct RecordingCatalogSource {
        candidates: Vec<(ConnectorConnection, InstalledActionBinding)>,
        requests: Arc<Mutex<Vec<Vec<ConnectorConnectionId>>>>,
    }

    #[async_trait]
    impl InstalledConnectorCatalogSource for RecordingCatalogSource {
        async fn candidates(
            &self,
            tenant_id: TenantId,
            connection_ids: &[ConnectorConnectionId],
        ) -> moa_connectors::Result<Vec<(ConnectorConnection, InstalledActionBinding)>> {
            self.requests
                .lock()
                .expect("recording source lock should remain available")
                .push(connection_ids.to_vec());
            let selected = connection_ids.iter().copied().collect::<HashSet<_>>();
            Ok(self
                .candidates
                .iter()
                .filter(|(connection, _)| {
                    connection.tenant_id == tenant_id
                        && selected.contains(&connection.connection_id)
                })
                .cloned()
                .collect())
        }
    }

    #[derive(Clone)]
    struct OverfetchingCatalogSource {
        candidates: Vec<(ConnectorConnection, InstalledActionBinding)>,
        requests: Arc<Mutex<Vec<Vec<ConnectorConnectionId>>>>,
    }

    #[async_trait]
    impl InstalledConnectorCatalogSource for OverfetchingCatalogSource {
        async fn candidates(
            &self,
            tenant_id: TenantId,
            connection_ids: &[ConnectorConnectionId],
        ) -> moa_connectors::Result<Vec<(ConnectorConnection, InstalledActionBinding)>> {
            self.requests
                .lock()
                .expect("overfetching source lock should remain available")
                .push(connection_ids.to_vec());
            Ok(self
                .candidates
                .iter()
                .filter(|(connection, _)| connection.tenant_id == tenant_id)
                .cloned()
                .collect())
        }
    }

    struct AllowSelectedConnections {
        allowed: HashSet<ConnectorConnectionId>,
    }

    #[async_trait]
    impl ConnectorUseAuthorizer for AllowSelectedConnections {
        async fn require_use(
            &self,
            _caller: &Identity,
            connection_id: ConnectorConnectionId,
        ) -> moa_connectors::Result<()> {
            if self.allowed.contains(&connection_id) {
                Ok(())
            } else {
                Err(moa_connectors::Error::AuthorizationDenied)
            }
        }
    }

    struct RejectingConnectorRuntime;

    #[async_trait]
    impl ConnectorActionRuntime for RejectingConnectorRuntime {
        async fn invoke(
            &self,
            _invocation: ConnectorActionInvocation,
        ) -> moa_connectors::Result<RawConnectorActionResult> {
            Err(moa_connectors::Error::Http {
                code: "catalog_fixture_runtime_must_not_execute",
            })
        }
    }

    #[tokio::test]
    async fn scoped_catalogs_are_agent_isolated_and_never_globally_published() {
        // Pins: exact agent connection selectors produce ephemeral, disjoint
        // overlays while the immutable deployment catalog remains unchanged.
        let tenant_id = TenantId::new();
        let first = connector_fixture(tenant_id, "first_action");
        let second = connector_fixture(tenant_id, "second_action");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(RecordingCatalogSource {
            candidates: vec![first.candidate.clone(), second.candidate.clone()],
            requests: Arc::clone(&requests),
        });
        let authorizer = Arc::new(AllowSelectedConnections {
            allowed: [first.binding.connection_id, second.binding.connection_id]
                .into_iter()
                .collect(),
        });
        let installed_catalog =
            Arc::new(GovernedInstalledConnectorCatalog::new(source, authorizer));
        let router = Arc::new(ToolRouter::new(
            ToolRegistry::new(),
            std::collections::HashMap::new(),
            moa_hands::local_development_sandbox_policy(),
        ));
        let deployment_pin = router
            .activated_catalog()
            .pin()
            .expect("empty deployment catalog should pin");
        let provider = ScopedConnectorCatalogProvider::new(
            Arc::clone(&router),
            installed_catalog,
            Arc::new(RejectingConnectorRuntime),
        );
        let caller = identity(tenant_id);

        let first_catalog = provider
            .for_agent_context(
                &caller,
                tenant_id,
                Some(&agent_context(first.binding.clone())),
            )
            .await
            .expect("first exact connector binding should project");
        let second_catalog = provider
            .for_agent_context(
                &caller,
                tenant_id,
                Some(&agent_context(second.binding.clone())),
            )
            .await
            .expect("second exact connector binding should project");

        assert_eq!(
            installed_connection_ids(first_catalog.snapshot()),
            vec![first.binding.connection_id]
        );
        assert_eq!(
            installed_connection_ids(second_catalog.snapshot()),
            vec![second.binding.connection_id]
        );
        assert_ne!(first_catalog.pin(), second_catalog.pin());
        assert_eq!(
            *requests
                .lock()
                .expect("recording source lock should remain available"),
            vec![
                vec![first.binding.connection_id],
                vec![second.binding.connection_id]
            ]
        );

        let deployment_after = router.activated_catalog();
        assert_eq!(
            deployment_after
                .pin()
                .expect("deployment catalog should still pin"),
            deployment_pin
        );
        assert!(installed_connection_ids(&deployment_after).is_empty());
    }

    #[tokio::test]
    async fn scoped_catalog_selects_one_connection_among_same_definition_installations_offline() {
        // Pins: when one tenant installs the same exact connector definition twice,
        // one agent binding exposes only its selected connection and action provenance.
        let tenant_id = TenantId::new();
        let artifact_uid = Uuid::new_v4();
        let revision_uid = Uuid::new_v4();
        let selected = connector_fixture_with_definition(
            tenant_id,
            "shared_action",
            artifact_uid,
            revision_uid,
        );
        let sibling = connector_fixture_with_definition(
            tenant_id,
            "shared_action",
            artifact_uid,
            revision_uid,
        );
        assert_ne!(
            selected.binding.connection_id,
            sibling.binding.connection_id
        );
        assert_eq!(
            selected.candidate.0.definition,
            sibling.candidate.0.definition
        );

        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(OverfetchingCatalogSource {
            candidates: vec![selected.candidate.clone(), sibling.candidate.clone()],
            requests: Arc::clone(&requests),
        });
        let authorizer = Arc::new(AllowSelectedConnections {
            allowed: [selected.binding.connection_id].into_iter().collect(),
        });
        let router = Arc::new(ToolRouter::new(
            ToolRegistry::new(),
            std::collections::HashMap::new(),
            moa_hands::local_development_sandbox_policy(),
        ));
        let provider = ScopedConnectorCatalogProvider::new(
            Arc::clone(&router),
            Arc::new(GovernedInstalledConnectorCatalog::new(source, authorizer)),
            Arc::new(RejectingConnectorRuntime),
        );

        let catalog = provider
            .for_agent_context(
                &identity(tenant_id),
                tenant_id,
                Some(&agent_context(selected.binding.clone())),
            )
            .await
            .expect("the exact selected connection should produce a scoped catalog");

        assert_eq!(catalog.schemas().len(), 1);
        assert_eq!(
            installed_connection_ids(catalog.snapshot()),
            vec![selected.binding.connection_id]
        );
        assert_eq!(
            *requests
                .lock()
                .expect("overfetching source lock should remain available"),
            vec![vec![selected.binding.connection_id]]
        );

        let registrations = catalog.snapshot().capability_registrations();
        assert_eq!(registrations.len(), 1);
        let (tool, execution) = registrations
            .first()
            .expect("the selected connection should expose one action");
        let selected_tool_name = moa_artifacts::connector::connection_action_tool_reference(
            selected.binding.connection_id,
            "shared_action",
        )
        .expect("selected fixture action should produce a tool reference");
        let sibling_tool_name = moa_artifacts::connector::connection_action_tool_reference(
            sibling.binding.connection_id,
            "shared_action",
        )
        .expect("sibling fixture action should produce a tool reference");
        assert_eq!(tool.name, selected_tool_name);
        assert!(
            catalog
                .snapshot()
                .tool_definition(&sibling_tool_name)
                .is_none()
        );

        let ToolExecution::InstalledConnectorAction {
            connector_ref,
            connection_id,
            binding_id,
            connection_generation,
            definition_artifact_uid,
            definition_revision_uid,
            action_id,
            contract_hash,
            governed_contract_revision,
            minimum_effect,
            runtime: _,
        } = execution
        else {
            panic!("the scoped registration must retain installed-connector provenance");
        };
        assert_eq!(connector_ref, &selected.binding.connector_ref);
        assert_eq!(*connection_id, selected.binding.connection_id);
        assert_eq!(*binding_id, selected.candidate.1.binding_id);
        assert_eq!(
            *connection_generation,
            selected.candidate.1.connection_generation
        );
        assert_eq!(*definition_artifact_uid, artifact_uid);
        assert_eq!(*definition_revision_uid, revision_uid);
        assert_eq!(action_id, "shared_action");
        assert_eq!(*contract_hash, selected.candidate.1.contract_hash);
        assert_eq!(
            governed_contract_revision,
            &selected.candidate.1.governed_contract_revision
        );
        assert_eq!(*minimum_effect, ActionPolicyEffect::AdminReview);
    }

    #[tokio::test]
    async fn scoped_catalog_rejects_caller_tenant_mismatch_before_catalog_read() {
        // Pins: an agent policy cannot redirect an authenticated caller into a
        // different tenant's connector catalog.
        let owner_tenant = TenantId::new();
        let fixture = connector_fixture(owner_tenant, "tenant_action");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(RecordingCatalogSource {
            candidates: vec![fixture.candidate],
            requests: Arc::clone(&requests),
        });
        let authorizer = Arc::new(AllowSelectedConnections {
            allowed: [fixture.binding.connection_id].into_iter().collect(),
        });
        let provider = ScopedConnectorCatalogProvider::new(
            Arc::new(ToolRouter::new(
                ToolRegistry::new(),
                std::collections::HashMap::new(),
                moa_hands::local_development_sandbox_policy(),
            )),
            Arc::new(GovernedInstalledConnectorCatalog::new(source, authorizer)),
            Arc::new(RejectingConnectorRuntime),
        );

        let error = match provider
            .for_agent_context(
                &identity(TenantId::new()),
                owner_tenant,
                Some(&agent_context(fixture.binding)),
            )
            .await
        {
            Ok(_) => panic!("cross-tenant caller must fail closed"),
            Err(error) => error,
        };

        assert!(matches!(error, ScopedConnectorCatalogError::TenantMismatch));
        assert!(
            requests
                .lock()
                .expect("recording source lock should remain available")
                .is_empty()
        );
    }

    fn installed_connection_ids(
        snapshot: &moa_hands::ToolCatalogSnapshot,
    ) -> Vec<ConnectorConnectionId> {
        snapshot
            .capability_registrations()
            .into_iter()
            .filter_map(|(_, execution)| match execution {
                ToolExecution::InstalledConnectorAction { connection_id, .. } => {
                    Some(connection_id)
                }
                ToolExecution::BuiltIn(_)
                | ToolExecution::Hand { .. }
                | ToolExecution::Mcp { .. } => None,
            })
            .collect()
    }

    fn identity(tenant_id: TenantId) -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::new_v4(),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    fn agent_context(binding: AgentConnectorBinding) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            action_policy: AgentActionPolicy {
                connector_bindings: vec![binding],
                ..AgentActionPolicy::default()
            },
            ..AgentPolicySnapshot::default()
        };
        let mut context = AgentContext::system_default();
        context.policy_snapshot =
            serde_json::to_value(snapshot).expect("serialize connector policy snapshot");
        context
    }

    #[derive(Clone)]
    struct ConnectorFixture {
        candidate: (ConnectorConnection, InstalledActionBinding),
        binding: AgentConnectorBinding,
    }

    fn connector_fixture(tenant_id: TenantId, action_id: &str) -> ConnectorFixture {
        let artifact_uid = Uuid::new_v4();
        let revision_uid = Uuid::new_v4();
        connector_fixture_with_definition(tenant_id, action_id, artifact_uid, revision_uid)
    }

    fn connector_fixture_with_definition(
        tenant_id: TenantId,
        action_id: &str,
        artifact_uid: Uuid,
        revision_uid: Uuid,
    ) -> ConnectorFixture {
        let connection_id = ConnectorConnectionId::new();
        let generation =
            ConnectionGeneration::new(1).expect("fixture generation should be positive");
        let definition: RuntimeConnectorDefinitionV1 = serde_json::from_value(json!({
            "definition_version": "v1",
            "display_name": "Scoped catalog fixture",
            "runtime": {"type": "built_in_managed", "provider": "fixture/v1"},
            "auth": [{"type": "managed_oauth", "slot": "primary"}],
            "actions": [{
                "id": action_id,
                "description": "Scoped connector action.",
                "binding": {
                    "type": "built_in_managed",
                    "operation": "fixture.read",
                    "contract": {
                        "input_schema": {"type": "object"},
                        "output_schema": {"type": "object"},
                        "data_classes": [],
                        "action_class": "external_write",
                        "risk_level": "high",
                        "minimum_effect": "admin_review",
                        "idempotency": "idempotent"
                    }
                }
            }]
        }))
        .expect("fixture definition should deserialize");
        let action = definition
            .actions
            .first()
            .expect("fixture definition should declare an action");
        let compiled_contract = CompiledOperationContract::compile(&definition, action)
            .expect("fixture contract should compile");
        let contract_hash = compiled_contract
            .hash()
            .expect("fixture contract should hash");
        let now = Utc::now();
        let connection = ConnectorConnection {
            connection_id,
            tenant_id,
            display_name: format!("Account {action_id}"),
            definition: ConnectionDefinitionRef::Artifact {
                artifact_uid,
                revision_uid,
            },
            non_secret_config: json!({}),
            generation,
            status: ConnectionStatus::Active,
            health: ConnectionHealth::Ready,
            health_reason: None,
            created_by_identity_id: None,
            owner_identity_id: None,
            created_at: now,
            updated_at: now,
        };
        let installed_binding = InstalledActionBinding {
            binding_id: InstalledActionBindingId(Uuid::new_v4()),
            tenant_id,
            connection_id,
            connection_generation: generation,
            action_id: action_id.to_string(),
            compiled_contract,
            contract_hash,
            governed_contract_revision: format!("connector-action/v1/{action_id}"),
            minimum_effect: ActionPolicyEffect::AdminReview,
            enabled: true,
        };
        ConnectorFixture {
            candidate: (connection, installed_binding),
            binding: AgentConnectorBinding {
                connector_ref: format!("connector://{action_id}"),
                connection_id,
                artifact_uid,
                revision_uid,
            },
        }
    }
}

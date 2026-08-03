//! Exact connector-definition resolution.

use async_trait::async_trait;
use moa_artifacts::connector::{ConnectorDefinition, RuntimeConnectorAuthRequirement};
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, StoredArtifactRevision};
use moa_connectors::domain::{ConnectionDefinitionRef, ManagedParentDefinition};
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use moa_wire::connectors::ConnectorArtifactReference;

/// Exact connector-definition resolution failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConnectorDefinitionResolutionError {
    /// The exact definition does not exist in the caller's visible scope.
    #[error("connector definition not found")]
    NotFound,
    /// The exact definition exists but is not eligible for a new installation.
    #[error("connector definition is not published")]
    NotPublished,
    /// The referenced artifact is not a connection-installable connector.
    #[error("connector definition is not installable")]
    NotInstallable,
    /// The code-owned built-in definition has no configured resolver.
    #[error("built-in connector definition unavailable")]
    BuiltInUnavailable,
    /// Definition persistence could not produce a trustworthy result.
    #[error("connector definition resolution unavailable")]
    Unavailable,
}
/// One exact immutable connector definition resolved from an approved source.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedConnectorDefinition {
    /// Domain definition reference expected on the connection row.
    pub definition_ref: ConnectionDefinitionRef,
    /// Validated artifact definition when this is an action-capable connector.
    pub definition: Option<ConnectorDefinition>,
    /// Closed secret-free credential requirements for readiness projection.
    pub credential_requirements: Vec<RuntimeConnectorAuthRequirement>,
}

impl ResolvedConnectorDefinition {
    pub(super) fn artifact_definition(
        &self,
    ) -> Result<&ConnectorDefinition, ConnectorDefinitionResolutionError> {
        self.definition
            .as_ref()
            .ok_or(ConnectorDefinitionResolutionError::NotInstallable)
    }
}

/// Definition lookup port separating artifact and code-owned catalogs.
#[async_trait]
pub trait ConnectorDefinitionResolver: Send + Sync {
    /// Resolves an exact definition eligible for a new installation.
    async fn resolve_for_install(
        &self,
        tenant_id: TenantId,
        reference: &ConnectorArtifactReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError>;

    /// Resolves the exact immutable definition already pinned by a connection.
    async fn resolve_installed(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError>;

    /// Resolves exact installed definitions in request order using one artifact read.
    async fn resolve_installed_batch(
        &self,
        tenant_id: TenantId,
        references: &[ConnectionDefinitionRef],
    ) -> Result<Vec<ResolvedConnectorDefinition>, ConnectorDefinitionResolutionError>;
}

/// Resolver for tenant-visible connector revisions and closed managed-knowledge definitions.
///
/// Artifact bodies come only from the registry. The two supported built-ins
/// are code-owned enum variants and are never inferred from arbitrary keys.
#[derive(Clone)]
pub struct ArtifactConnectorDefinitionResolver {
    registry: ArtifactRegistry,
}

impl ArtifactConnectorDefinitionResolver {
    /// Creates an artifact-backed resolver.
    #[must_use]
    pub fn new(registry: ArtifactRegistry) -> Self {
        Self { registry }
    }

    async fn load_artifact(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
        require_published: bool,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        let ConnectionDefinitionRef::Artifact { revision_uid, .. } = reference else {
            return Err(ConnectorDefinitionResolutionError::NotInstallable);
        };
        let scope = ActionRuleScope::Tenant { tenant_id };
        let stored = self
            .registry
            .load_revision(&scope, *revision_uid)
            .await
            .map_err(|_| ConnectorDefinitionResolutionError::Unavailable)?
            .ok_or(ConnectorDefinitionResolutionError::NotFound)?;
        resolved_artifact_definition(&stored, reference, require_published)
    }
}

#[async_trait]
impl ConnectorDefinitionResolver for ArtifactConnectorDefinitionResolver {
    async fn resolve_for_install(
        &self,
        tenant_id: TenantId,
        reference: &ConnectorArtifactReference,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        let reference = ConnectionDefinitionRef::Artifact {
            artifact_uid: reference.artifact_uid,
            revision_uid: reference.revision_uid,
        };
        self.load_artifact(tenant_id, &reference, true).await
    }

    async fn resolve_installed(
        &self,
        tenant_id: TenantId,
        reference: &ConnectionDefinitionRef,
    ) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
        match reference {
            ConnectionDefinitionRef::Artifact { .. } => {
                self.load_artifact(tenant_id, reference, false).await
            }
            ConnectionDefinitionRef::BuiltIn { .. } => resolved_managed_definition(reference),
        }
    }

    async fn resolve_installed_batch(
        &self,
        tenant_id: TenantId,
        references: &[ConnectionDefinitionRef],
    ) -> Result<Vec<ResolvedConnectorDefinition>, ConnectorDefinitionResolutionError> {
        let revision_uids = references
            .iter()
            .filter_map(|reference| match reference {
                ConnectionDefinitionRef::Artifact { revision_uid, .. } => Some(*revision_uid),
                ConnectionDefinitionRef::BuiltIn { .. } => None,
            })
            .collect::<Vec<_>>();
        let scope = ActionRuleScope::Tenant { tenant_id };
        let stored = self
            .registry
            .load_revisions(&scope, &revision_uids)
            .await
            .map_err(|_| ConnectorDefinitionResolutionError::Unavailable)?;
        let by_revision = stored
            .into_iter()
            .map(|revision| (revision.revision_uid, revision))
            .collect::<std::collections::HashMap<_, _>>();

        references
            .iter()
            .map(|reference| match reference {
                ConnectionDefinitionRef::Artifact { revision_uid, .. } => {
                    let stored = by_revision
                        .get(revision_uid)
                        .ok_or(ConnectorDefinitionResolutionError::NotFound)?;
                    resolved_artifact_definition(stored, reference, false)
                }
                ConnectionDefinitionRef::BuiltIn { .. } => resolved_managed_definition(reference),
            })
            .collect()
    }
}

fn resolved_artifact_definition(
    stored: &StoredArtifactRevision,
    reference: &ConnectionDefinitionRef,
    require_published: bool,
) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
    let ConnectionDefinitionRef::Artifact {
        artifact_uid,
        revision_uid,
    } = reference
    else {
        return Err(ConnectorDefinitionResolutionError::NotInstallable);
    };
    if stored.artifact_uid != *artifact_uid
        || stored.revision_uid != *revision_uid
        || stored.kind != ArtifactKind::Connector
    {
        return Err(ConnectorDefinitionResolutionError::NotFound);
    }
    if require_published && stored.status != ArtifactStatus::Published {
        return Err(ConnectorDefinitionResolutionError::NotPublished);
    }
    let ArtifactDefinition::Connector(connector) = &stored.document.definition else {
        return Err(ConnectorDefinitionResolutionError::NotInstallable);
    };
    Ok(ResolvedConnectorDefinition {
        definition_ref: reference.clone(),
        credential_requirements: connector.auth.clone(),
        definition: Some(connector.clone()),
    })
}

fn resolved_managed_definition(
    reference: &ConnectionDefinitionRef,
) -> Result<ResolvedConnectorDefinition, ConnectorDefinitionResolutionError> {
    let managed = managed_knowledge_definition(reference)
        .ok_or(ConnectorDefinitionResolutionError::BuiltInUnavailable)?;
    Ok(ResolvedConnectorDefinition {
        definition_ref: managed.definition_ref(),
        definition: None,
        credential_requirements: managed.credential_requirements(),
    })
}

pub(super) fn managed_knowledge_definition(
    reference: &ConnectionDefinitionRef,
) -> Option<ManagedParentDefinition> {
    [
        ManagedParentDefinition::KnowledgeNango,
        ManagedParentDefinition::KnowledgeMerge,
    ]
    .into_iter()
    .find(|managed| &managed.definition_ref() == reference)
}

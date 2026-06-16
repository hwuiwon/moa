//! Reference resolution for artifact publish validation.

use moa_core::{MemoryScope, Result};

use crate::connector::ConnectorDefinition;
use crate::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind};
use crate::reference::{ArtifactRef, ArtifactRefKind, ReferenceResolution};
use crate::registry::ArtifactRegistry;

/// Resolves artifact references against visible published revisions.
pub struct ArtifactResolver {
    registry: ArtifactRegistry,
}

impl ArtifactResolver {
    /// Creates a resolver backed by an artifact registry.
    #[must_use]
    pub fn new(registry: ArtifactRegistry) -> Self {
        Self { registry }
    }

    /// Resolves all references declared by a document for the provided scope.
    pub async fn resolve_document(
        &self,
        scope: &MemoryScope,
        document: &ArtifactDocument,
    ) -> Result<Vec<ReferenceResolution>> {
        let references = document.reference_paths();
        let mut resolutions = Vec::with_capacity(references.len());
        for (path, artifact_ref) in references {
            if self.resolve_one(scope, &artifact_ref).await? {
                resolutions.push(ReferenceResolution::resolved(path, artifact_ref));
            } else {
                resolutions.push(ReferenceResolution::unresolved(path, artifact_ref));
            }
        }
        Ok(resolutions)
    }

    async fn resolve_one(&self, scope: &MemoryScope, artifact_ref: &ArtifactRef) -> Result<bool> {
        match artifact_ref.kind {
            ArtifactRefKind::Tool => Ok(true),
            ArtifactRefKind::Skill => self
                .registry
                .load_visible_published(scope, ArtifactKind::Skill, &artifact_ref.target)
                .await
                .map(|artifact| artifact.is_some()),
            ArtifactRefKind::Workflow => self
                .registry
                .load_visible_published(scope, ArtifactKind::Workflow, &artifact_ref.target)
                .await
                .map(|artifact| artifact.is_some()),
            ArtifactRefKind::Connector => self
                .registry
                .load_visible_published(scope, ArtifactKind::Connector, &artifact_ref.target)
                .await
                .map(|artifact| artifact.is_some()),
            ArtifactRefKind::Action => self.resolve_action(scope, artifact_ref).await,
        }
    }

    async fn resolve_action(
        &self,
        scope: &MemoryScope,
        artifact_ref: &ArtifactRef,
    ) -> Result<bool> {
        let Some(action_name) = artifact_ref.action.as_deref() else {
            return Ok(false);
        };
        let Some(connector) = self
            .registry
            .load_visible_published(scope, ArtifactKind::Connector, &artifact_ref.target)
            .await?
        else {
            return Ok(false);
        };
        let ArtifactDefinition::Connector(ConnectorDefinition { actions, .. }) =
            connector.document.definition
        else {
            return Ok(false);
        };
        Ok(actions.iter().any(|action| action.id == action_name))
    }
}

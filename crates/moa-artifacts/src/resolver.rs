//! Reference resolution for artifact publish validation.

use moa_core::{error::Result, types::action_policy::ActionRuleScope};

use crate::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind};
use crate::reference::{ArtifactRef, ReferenceResolution};
use crate::registry::ArtifactRegistry;

/// Resolves artifact references against what the tenant actually serves.
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
        scope: &ActionRuleScope,
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

    async fn resolve_one(
        &self,
        scope: &ActionRuleScope,
        artifact_ref: &ArtifactRef,
    ) -> Result<bool> {
        match artifact_ref {
            ArtifactRef::Tool { .. } => Ok(true),
            // A dependency resolves only when the tenant serves it. Skills answer
            // from their type-owned serving pointer; other kinds retain their
            // established published-revision lifecycle.
            ArtifactRef::Artifact { kind, name } => self
                .load_dependency(scope, kind.clone(), name)
                .await
                .map(|artifact| artifact.is_some()),
            ArtifactRef::Action { .. } => self.resolve_action(scope, artifact_ref).await,
        }
    }

    async fn load_dependency(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<crate::registry::StoredArtifactRevision>> {
        match kind {
            ArtifactKind::Skill | ArtifactKind::Action => {
                self.registry.load_serving(scope, kind, name).await
            }
            _ => {
                self.registry
                    .load_visible_published(scope, kind, name)
                    .await
            }
        }
    }

    async fn resolve_action(
        &self,
        scope: &ActionRuleScope,
        artifact_ref: &ArtifactRef,
    ) -> Result<bool> {
        let ArtifactRef::Action { connector, action } = artifact_ref else {
            return Ok(false);
        };
        let Some(connector) = self
            .load_dependency(scope, ArtifactKind::Connector, connector)
            .await?
        else {
            return Ok(false);
        };
        let ArtifactDefinition::Connector(definition) = connector.document.definition else {
            return Ok(false);
        };
        Ok(definition
            .actions()
            .any(|candidate| candidate.id() == action))
    }
}

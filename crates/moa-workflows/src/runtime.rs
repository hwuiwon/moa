//! Durable runtime lifecycle for artifact-backed workflow runs.

use moa_artifacts::document::ArtifactKind;
use moa_artifacts::reference::{ArtifactRef, ArtifactRefKind};
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRun, ArtifactRunStatus, NewArtifactRun};
use moa_core::{MemoryScope, SessionId};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};

/// Input for creating one workflow run.
#[derive(Clone, Debug, PartialEq)]
pub struct StartWorkflowRun {
    /// Workflow artifact reference, for example `workflow://damaged-food-order`.
    pub workflow_ref: String,
    /// Initial workflow input payload.
    pub input: Value,
    /// Session associated with this workflow run, when the run starts from a session.
    pub session_id: Option<SessionId>,
    /// Optional idempotency key for run creation.
    pub idempotency_key: Option<String>,
}

/// Runtime entrypoint for artifact-backed workflow runs.
pub struct WorkflowRuntime {
    registry: ArtifactRegistry,
}

impl WorkflowRuntime {
    /// Creates a workflow runtime backed by the artifact registry.
    #[must_use]
    pub fn new(registry: ArtifactRegistry) -> Self {
        Self { registry }
    }

    /// Creates a durable workflow run for a published workflow artifact.
    pub async fn start(
        &self,
        scope: &MemoryScope,
        request: StartWorkflowRun,
    ) -> Result<ArtifactRun> {
        let artifact_ref = parse_workflow_ref(&request.workflow_ref)?;
        let workflow = self
            .registry
            .load_visible_published(scope, ArtifactKind::Workflow, &artifact_ref.target)
            .await?
            .ok_or_else(|| WorkflowError::WorkflowNotFound {
                workflow_ref: request.workflow_ref.clone(),
            })?;

        Ok(self
            .registry
            .append_run(
                scope,
                NewArtifactRun {
                    artifact_uid: Some(workflow.artifact_uid),
                    revision_uid: Some(workflow.revision_uid),
                    session_id: request.session_id,
                    workflow_ref: request.workflow_ref,
                    status: ArtifactRunStatus::Queued,
                    current_node_id: None,
                    input: request.input,
                    state: Value::Object(serde_json::Map::new()),
                    output: None,
                    error: None,
                    idempotency_key: request.idempotency_key,
                },
            )
            .await?)
    }

    /// Loads the current projection for a visible workflow run.
    pub async fn status(&self, scope: &MemoryScope, run_uid: Uuid) -> Result<Option<ArtifactRun>> {
        Ok(self.registry.load_run(scope, run_uid).await?)
    }

    /// Marks a visible workflow run as cancelled when it is still cancellable.
    pub async fn cancel(
        &self,
        scope: &MemoryScope,
        run_uid: Uuid,
        reason: Option<String>,
    ) -> Result<Option<ArtifactRun>> {
        Ok(self.registry.cancel_run(scope, run_uid, reason).await?)
    }
}

fn parse_workflow_ref(value: &str) -> Result<ArtifactRef> {
    let artifact_ref =
        value
            .parse::<ArtifactRef>()
            .map_err(|error| WorkflowError::InvalidReference {
                reference: value.to_string(),
                message: error.to_string(),
            })?;
    if artifact_ref.kind != ArtifactRefKind::Workflow {
        return Err(WorkflowError::WrongReferenceKind);
    }
    Ok(artifact_ref)
}

#[cfg(test)]
mod tests {
    use super::parse_workflow_ref;
    use crate::error::WorkflowError;

    #[test]
    fn parse_workflow_ref_accepts_workflow_scheme_only() {
        // Pins: the runtime never creates workflow runs from skill or action references.
        let workflow_ref =
            parse_workflow_ref("workflow://damaged-food-order").expect("workflow ref parses");
        assert_eq!(workflow_ref.target, "damaged-food-order");

        let error = parse_workflow_ref("skill://damaged-food-order")
            .expect_err("skill references must not start workflow runs");
        assert!(matches!(error, WorkflowError::WrongReferenceKind));
    }
}

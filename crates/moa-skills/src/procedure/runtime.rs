//! Durable runtime lifecycle for skill-backed procedure runs.

use moa_artifacts::document::{ArtifactDefinition, ArtifactKind};
use moa_artifacts::procedure::ProcedureDefinition;
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRun, ArtifactRunStatus, NewArtifactRun};
use moa_core::{ActionRuleScope, SessionId};
use serde_json::Value;
use uuid::Uuid;

use crate::procedure::error::{ProcedureError, Result};
use crate::procedure::input_validation::validate_input_against_schema;

/// Input for creating one procedure run.
#[derive(Clone, Debug, PartialEq)]
pub struct StartProcedureRun {
    /// Skill artifact reference carrying the procedure, for example `skill://damaged-food-order`.
    pub procedure_ref: String,
    /// Initial procedure input payload.
    pub input: Value,
    /// Session associated with this procedure run, when the run starts from a session.
    pub session_id: Option<SessionId>,
    /// Optional idempotency key for run creation.
    pub idempotency_key: Option<String>,
}

/// Runtime entrypoint for skill-backed procedure runs.
pub struct ProcedureRuntime {
    registry: ArtifactRegistry,
}

impl ProcedureRuntime {
    /// Creates a procedure runtime backed by the artifact registry.
    #[must_use]
    pub fn new(registry: ArtifactRegistry) -> Self {
        Self { registry }
    }

    /// Creates a durable procedure run for a published skill artifact that carries a procedure.
    pub async fn start(
        &self,
        scope: &ActionRuleScope,
        request: StartProcedureRun,
    ) -> Result<ArtifactRun> {
        let artifact_ref = parse_procedure_ref(&request.procedure_ref)?;
        let skill = self
            .registry
            .load_visible_published(scope, ArtifactKind::Skill, artifact_ref.target_name())
            .await?
            .ok_or_else(|| ProcedureError::ProcedureNotFound {
                procedure_ref: request.procedure_ref.clone(),
            })?;

        let procedure = procedure_definition(&skill.document.definition).ok_or_else(|| {
            ProcedureError::SkillHasNoProcedure {
                procedure_ref: request.procedure_ref.clone(),
            }
        })?;

        // Checklist enforcement: reject a run whose input does not satisfy the
        // procedure's `input_schema` before any durable row is written, so the
        // caller is forced to collect required information first.
        let violations = validate_input_against_schema(&procedure.input_schema, &request.input);
        if !violations.is_empty() {
            return Err(ProcedureError::MissingRequiredInputs {
                missing: violations.missing,
                invalid: violations.invalid,
            });
        }

        Ok(self
            .registry
            .append_run(
                scope,
                NewArtifactRun {
                    artifact_uid: Some(skill.artifact_uid),
                    revision_uid: Some(skill.revision_uid),
                    session_id: request.session_id,
                    procedure_ref: request.procedure_ref,
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

    /// Loads the current projection for a visible procedure run.
    pub async fn status(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> Result<Option<ArtifactRun>> {
        Ok(self.registry.load_run(scope, run_uid).await?)
    }

    /// Marks a visible procedure run as cancelled when it is still cancellable.
    pub async fn cancel(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reason: Option<String>,
    ) -> Result<Option<ArtifactRun>> {
        Ok(self.registry.cancel_run(scope, run_uid, reason).await?)
    }
}

/// Returns the procedure graph carried by a skill definition, when present.
fn procedure_definition(definition: &ArtifactDefinition) -> Option<&ProcedureDefinition> {
    match definition {
        ArtifactDefinition::Skill(skill) => skill.procedure.as_ref(),
        _ => None,
    }
}

fn parse_procedure_ref(value: &str) -> Result<ArtifactRef> {
    let artifact_ref =
        value
            .parse::<ArtifactRef>()
            .map_err(|error| ProcedureError::InvalidReference {
                reference: value.to_string(),
                message: error.to_string(),
            })?;
    if artifact_ref.artifact_kind() != Some(&ArtifactKind::Skill) {
        return Err(ProcedureError::WrongReferenceKind);
    }
    Ok(artifact_ref)
}

#[cfg(test)]
mod tests {
    use moa_artifacts::document::ArtifactDefinition;
    use moa_artifacts::skill::SkillDefinition;
    use serde_json::json;

    use super::{parse_procedure_ref, procedure_definition};
    use crate::procedure::error::ProcedureError;

    fn skill_definition(value: serde_json::Value) -> ArtifactDefinition {
        ArtifactDefinition::Skill(
            serde_json::from_value::<SkillDefinition>(value).expect("skill definition"),
        )
    }

    #[test]
    fn procedure_definition_present_only_when_skill_carries_procedure() {
        // Pins: the runtime treats a skill without a procedure graph as having no
        // procedure, which is what drives the SkillHasNoProcedure run error.
        let with_procedure = skill_definition(json!({
            "procedure": {"input_schema": {"type": "object"}}
        }));
        assert!(procedure_definition(&with_procedure).is_some());

        let without_procedure = skill_definition(json!({}));
        assert!(procedure_definition(&without_procedure).is_none());
    }

    #[test]
    fn parse_procedure_ref_accepts_skill_scheme_only() {
        // Pins: the runtime only starts procedure runs from skill references that carry procedures.
        let procedure_ref =
            parse_procedure_ref("skill://damaged-food-order").expect("skill ref parses");
        assert_eq!(procedure_ref.target_name(), "damaged-food-order");

        let error = parse_procedure_ref("action://damaged-food-order")
            .expect_err("non-skill references must not start procedure runs");
        assert!(matches!(error, ProcedureError::WrongReferenceKind));
    }
}

//! Semantic validation for artifact documents.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::connector::ConnectorDefinition;
use crate::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
use crate::reference::{ReferenceResolution, ReferenceState};
use crate::skill::SkillDefinition;
use crate::workflow::{WorkflowDefinition, WorkflowNodeKind};

/// A single semantic validation error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationError {
    /// JSON-ish path to the invalid field.
    pub path: String,
    /// Human-readable validation failure.
    pub message: String,
}

/// Semantic validation report for an artifact document.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationReport {
    /// Validation errors that block publish.
    #[serde(default)]
    pub errors: Vec<ValidationError>,
    /// Reference resolution results included with validation.
    #[serde(default)]
    pub references: Vec<ReferenceResolution>,
}

impl ValidationReport {
    /// Returns true when the report contains no errors.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Adds a validation error to this report.
    pub fn push_error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(ValidationError {
            path: path.into(),
            message: message.into(),
        });
    }
}

/// Validates a document as if it were being saved with the requested status.
#[must_use]
pub fn validate_for_status(
    document: &ArtifactDocument,
    requested_status: ArtifactStatus,
) -> ValidationReport {
    let mut report = ValidationReport {
        references: document.reference_resolutions.clone(),
        ..ValidationReport::default()
    };

    validate_envelope(document, &mut report);
    match &document.definition {
        ArtifactDefinition::Skill(definition) => validate_skill(definition, &mut report),
        ArtifactDefinition::Connector(definition) => validate_connector(definition, &mut report),
        ArtifactDefinition::Workflow(definition) => validate_workflow(definition, &mut report),
    }

    if requested_status == ArtifactStatus::Published {
        for resolution in &document.reference_resolutions {
            if resolution.state == ReferenceState::Unresolved {
                report.push_error(
                    resolution.path.clone(),
                    format!("unresolved reference {}", resolution.artifact_ref),
                );
            }
        }
    }

    report
}

fn validate_envelope(document: &ArtifactDocument, report: &mut ValidationReport) {
    if document.metadata.name.trim().is_empty() {
        report.push_error("metadata.name", "artifact name must not be empty");
    }

    let actual_kind = document.definition.kind();
    if document.kind != actual_kind {
        report.push_error(
            "kind",
            format!(
                "document kind {} does not match definition kind {}",
                document.kind, actual_kind
            ),
        );
    }

    if document.kind == ArtifactKind::Action {
        report.push_error(
            "kind",
            "standalone action documents are reserved for a later schema version",
        );
    }
}

fn validate_skill(definition: &SkillDefinition, report: &mut ValidationReport) {
    let mut action_ids = HashSet::new();
    for (index, action) in definition.actions.iter().enumerate() {
        let path = format!("definition.spec.actions[{index}].id");
        if action.id.trim().is_empty() {
            report.push_error(path, "skill action id must not be empty");
        } else if !action_ids.insert(action.id.as_str()) {
            report.push_error(path, "duplicate skill action id");
        }
    }
}

fn validate_connector(definition: &ConnectorDefinition, report: &mut ValidationReport) {
    let mut action_ids = HashSet::new();
    for (index, action) in definition.actions.iter().enumerate() {
        let path = format!("definition.spec.actions[{index}].id");
        if action.id.trim().is_empty() {
            report.push_error(path, "connector action id must not be empty");
        } else if !action_ids.insert(action.id.as_str()) {
            report.push_error(path, "duplicate connector action id");
        }
    }
}

fn validate_workflow(definition: &WorkflowDefinition, report: &mut ValidationReport) {
    let mut node_ids = HashSet::new();
    let mut saw_start = false;
    let mut saw_end = false;

    for (index, node) in definition.nodes.iter().enumerate() {
        let id_path = format!("definition.spec.nodes[{index}].id");
        if node.id.trim().is_empty() {
            report.push_error(id_path.clone(), "workflow node id must not be empty");
        } else if !node_ids.insert(node.id.as_str()) {
            report.push_error(id_path, "duplicate workflow node id");
        }

        saw_start |= node.kind == WorkflowNodeKind::Start;
        saw_end |= node.kind == WorkflowNodeKind::End;
    }

    if !saw_start {
        report.push_error(
            "definition.spec.nodes",
            "workflow must include a start node",
        );
    }
    if !saw_end {
        report.push_error("definition.spec.nodes", "workflow must include an end node");
    }

    for (index, edge) in definition.edges.iter().enumerate() {
        if !node_ids.contains(edge.from.as_str()) {
            report.push_error(
                format!("definition.spec.edges[{index}].from"),
                "edge source node does not exist",
            );
        }
        if !node_ids.contains(edge.to.as_str()) {
            report.push_error(
                format!("definition.spec.edges[{index}].to"),
                "edge destination node does not exist",
            );
        }
    }
}

use std::str::FromStr;

use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::reference::{ArtifactRef, ArtifactRefKind, ReferenceState};
use moa_artifacts::validation::validate_for_status;

#[test]
fn json_and_yaml_skill_imports_have_same_canonical_hash() {
    // Pins: JSON and YAML are import/export formats over the same canonical model.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: transaction-dispute
  description: Help file a card transaction dispute.
  tags: [banking, disputes]
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    inputs:
      type: object
      properties:
        transaction_id:
          type: string
    outputs:
      type: object
    connectors:
      - connector://credit_card
    allowed_tools:
      - get_transaction_info
    actions:
      - id: freeze_card
        description: Freeze the card for a suspected dispute.
        kind: connector_action
        ref: action://credit_card.freeze
"#;

    let json = r#"
{
  "api_version": "moa.artifact/v1",
  "kind": "skill",
  "metadata": {
    "name": "transaction-dispute",
    "description": "Help file a card transaction dispute.",
    "tags": ["banking", "disputes"]
  },
  "status": "draft",
  "definition": {
    "type": "skill",
    "spec": {
      "instructions": { "path": "SKILL.md" },
      "inputs": {
        "type": "object",
        "properties": {
          "transaction_id": { "type": "string" }
        }
      },
      "outputs": { "type": "object" },
      "connectors": ["connector://credit_card"],
      "allowed_tools": ["get_transaction_info"],
      "actions": [
        {
          "id": "freeze_card",
          "description": "Freeze the card for a suspected dispute.",
          "kind": "connector_action",
          "ref": "action://credit_card.freeze"
        }
      ]
    }
  }
}
"#;

    let yaml_doc = ArtifactDocument::from_yaml(yaml).expect("parse yaml artifact");
    let json_doc = ArtifactDocument::from_json(json).expect("parse json artifact");

    assert_eq!(
        canonical_hash(&yaml_doc).expect("hash yaml doc"),
        canonical_hash(&json_doc).expect("hash json doc")
    );
    assert_eq!(yaml_doc.references(), json_doc.references());
    let paths = yaml_doc
        .reference_paths()
        .into_iter()
        .map(|(path, artifact_ref)| (path, artifact_ref.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            (
                "definition.spec.connectors[0]".to_string(),
                "connector://credit_card".to_string()
            ),
            (
                "definition.spec.allowed_tools[0]".to_string(),
                "tool://get_transaction_info".to_string()
            ),
            (
                "definition.spec.actions[0].ref".to_string(),
                "action://credit_card.freeze".to_string()
            )
        ]
    );
}

#[test]
fn artifact_refs_parse_and_format_supported_schemes() {
    // Pins: code and UI can exchange stable string references.
    let cases = [
        (
            "skill://refund-policy",
            ArtifactRefKind::Skill,
            "refund-policy",
            None,
        ),
        (
            "action://credit_card.freeze",
            ArtifactRefKind::Action,
            "credit_card",
            Some("freeze"),
        ),
        (
            "workflow://damaged-food-order",
            ArtifactRefKind::Workflow,
            "damaged-food-order",
            None,
        ),
        (
            "connector://orders",
            ArtifactRefKind::Connector,
            "orders",
            None,
        ),
        (
            "tool://web_search",
            ArtifactRefKind::Tool,
            "web_search",
            None,
        ),
    ];

    for (input, kind, target, action) in cases {
        let artifact_ref = ArtifactRef::from_str(input).expect("parse reference");
        assert_eq!(artifact_ref.kind, kind);
        assert_eq!(artifact_ref.target, target);
        assert_eq!(artifact_ref.action.as_deref(), action);
        assert_eq!(artifact_ref.to_string(), input);
    }
}

#[test]
fn draft_allows_unresolved_refs_but_published_rejects_them() {
    // Pins: visual-builder drafts may link capabilities that are created later.
    let yaml = r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: damaged-food-order
status: draft
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
      - id: submit_issue
        kind: action
        ref: action://orders.submit_issue
      - id: done
        kind: end
    edges:
      - from: start
        to: submit_issue
      - from: submit_issue
        to: done
reference_resolutions:
  - path: definition.spec.nodes[1].ref
    ref: action://orders.submit_issue
    state: unresolved
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse workflow artifact");

    let draft_report = validate_for_status(&document, ArtifactStatus::Draft);
    assert!(
        draft_report.is_ok(),
        "draft report should not reject unresolved refs: {draft_report:?}"
    );
    assert_eq!(draft_report.references[0].state, ReferenceState::Unresolved);

    let published_report = validate_for_status(&document, ArtifactStatus::Published);
    assert_eq!(published_report.errors.len(), 1);
    assert_eq!(
        published_report.errors[0].path,
        "definition.spec.nodes[1].ref"
    );
}

#[test]
fn workflow_validation_rejects_duplicate_node_ids() {
    // Pins: workflow graphs must have unambiguous node identities.
    let yaml = r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: invalid-workflow
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
      - id: start
        kind: end
    edges:
      - from: start
        to: missing
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid workflow");
    let report = validate_for_status(&document, ArtifactStatus::Draft);

    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message == "duplicate workflow node id"),
        "expected duplicate-node error: {report:?}"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message == "edge destination node does not exist"),
        "expected missing-edge-target error: {report:?}"
    );
}

#[test]
fn prompt_examples_parse_as_draft_artifacts() {
    // Pins: docs examples stay executable by the canonical parser.
    let skill = include_str!("../../../docs/examples/artifacts/transaction-dispute.skill.yaml");
    let workflow =
        include_str!("../../../docs/examples/artifacts/damaged-food-order.workflow.yaml");

    for source in [skill, workflow] {
        let document = ArtifactDocument::from_yaml(source).expect("parse example artifact");
        let report = validate_for_status(&document, ArtifactStatus::Draft);
        assert!(report.is_ok(), "example should be draft-valid: {report:?}");
    }
}

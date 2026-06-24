use std::str::FromStr;

use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::{ArtifactRef, ReferenceState};
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
            "agent://support-triage",
            Some(ArtifactKind::Agent),
            "support-triage",
            None,
        ),
        (
            "skill://refund-policy",
            Some(ArtifactKind::Skill),
            "refund-policy",
            None,
        ),
        (
            "workflow://damaged-food-order",
            Some(ArtifactKind::Workflow),
            "damaged-food-order",
            None,
        ),
        (
            "connector://orders",
            Some(ArtifactKind::Connector),
            "orders",
            None,
        ),
        (
            "action://refund-order",
            Some(ArtifactKind::Action),
            "refund-order",
            None,
        ),
        ("tool://web_search", None, "web_search", None),
    ];

    for (input, kind, target, action) in cases {
        let artifact_ref = ArtifactRef::from_str(input).expect("parse reference");
        assert_eq!(artifact_ref.artifact_kind(), kind.as_ref());
        assert_eq!(artifact_ref.target_name(), target);
        assert_eq!(artifact_ref.action_name(), action);
        assert_eq!(artifact_ref.to_string(), input);
    }

    let action = ArtifactRef::from_str("action://credit_card.freeze")
        .expect("parse connector action reference");
    assert_eq!(action.artifact_kind(), None);
    assert_eq!(action.target_name(), "credit_card");
    assert_eq!(action.action_name(), Some("freeze"));
    assert_eq!(action.to_string(), "action://credit_card.freeze");
}

#[test]
fn agent_and_action_artifacts_roundtrip_and_extract_refs() {
    // Pins: tenant-configurable agents and standalone actions are first-class artifacts.
    let agent_yaml = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: support-triage
  description: Triage support requests.
status: draft
definition:
  type: agent
  spec:
    display_name: Support Triage
    purpose:
      summary: Triage customer support requests.
      expected_outputs:
        - prioritized response
    model_policy:
      default_model: claude-sonnet-4-6
      allowed_models:
        - claude-sonnet-4-6
    instruction_policy:
      system_prompt: Stay within the support scope.
    skill_policy:
      mode: pinned
      refs:
        - skill://refund-policy
    workflow_policy:
      allowed:
        - workflow://escalation
    action_policy:
      allowed:
        - action://refund-order
        - action://orders.cancel
    tool_policy:
      mode: allowlist
      tools:
        - file_read
"#;
    let action_yaml = r#"
api_version: moa.artifact/v1
kind: action
metadata:
  name: refund-order
definition:
  type: action
  spec:
    id: refund-order
    connector_ref: connector://orders
    tool_name: orders.refund
"#;

    let agent_doc = ArtifactDocument::from_yaml(agent_yaml).expect("parse agent artifact");
    let action_doc = ArtifactDocument::from_yaml(action_yaml).expect("parse action artifact");

    assert_eq!(agent_doc.kind, ArtifactKind::Agent);
    assert_eq!(
        ArtifactDocument::from_json(&agent_doc.to_json().expect("serialize agent json"))
            .expect("parse agent json"),
        agent_doc
    );
    assert_eq!(action_doc.kind, ArtifactKind::Action);
    assert_eq!(
        ArtifactDocument::from_yaml(&action_doc.to_yaml().expect("serialize action yaml"))
            .expect("parse action yaml"),
        action_doc
    );

    let agent_refs = agent_doc
        .references()
        .into_iter()
        .map(|artifact_ref| artifact_ref.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        agent_refs,
        vec![
            "skill://refund-policy",
            "workflow://escalation",
            "action://refund-order",
            "action://orders.cancel",
            "tool://file_read",
        ]
    );
    let action_refs = action_doc
        .references()
        .into_iter()
        .map(|artifact_ref| artifact_ref.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        action_refs,
        vec!["connector://orders", "tool://orders.refund"]
    );

    assert!(
        validate_for_status(&agent_doc, ArtifactStatus::Draft).is_ok(),
        "draft agent policy should be valid"
    );
    assert!(
        validate_for_status(&action_doc, ArtifactStatus::Draft).is_ok(),
        "draft action should be valid"
    );
}

#[test]
fn agent_validation_rejects_empty_required_policy_fields() {
    // Pins: invalid tenant-configurable agent policies fail before publish.
    let yaml = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: invalid-agent
definition:
  type: agent
  spec:
    display_name: ""
    purpose:
      summary: ""
    skill_policy:
      mode: allowlist
      refs: []
    tool_policy:
      mode: allowlist
      tools: []
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid agent artifact");
    let report = validate_for_status(&document, ArtifactStatus::Draft);

    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == "definition.spec.display_name"),
        "expected display_name error: {report:?}"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == "definition.spec.purpose.summary"),
        "expected purpose summary error: {report:?}"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == "definition.spec.skill_policy.refs"),
        "expected skill allowlist error: {report:?}"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == "definition.spec.tool_policy.tools"),
        "expected tool allowlist error: {report:?}"
    );
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
fn workflow_validation_rejects_executable_nodes_without_invocation_targets() {
    // Pins: published workflow action/tool nodes fail validation before runtime if no target can be executed.
    let yaml = r#"
api_version: moa.artifact/v1
kind: workflow
metadata:
  name: invalid-executable-workflow
definition:
  type: workflow
  spec:
    nodes:
      - id: start
        kind: start
      - id: notify_customer
        kind: action
        input:
          template: Tell the customer what happens next.
      - id: call_tool
        kind: tool
      - id: done
        kind: end
    edges:
      - from: start
        to: notify_customer
      - from: notify_customer
        to: call_tool
      - from: call_tool
        to: done
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid workflow");
    let report = validate_for_status(&document, ArtifactStatus::Published);

    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.nodes[1]"
                && error.message
                    == "workflow action node must specify ref, input.tool_name, or input.tool"
        }),
        "expected missing action invocation target error: {report:?}"
    );
    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.nodes[2]"
                && error.message
                    == "workflow tool node must specify exactly one tool_ref, input.tool_name, or input.tool"
        }),
        "expected missing tool invocation target error: {report:?}"
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

#[test]
fn capability_pattern_workflow_examples_parse_as_draft_artifacts() {
    // Pins: pattern examples stay round-trippable for future dashboard editing.
    let examples = [
        include_str!("../../../docs/examples/artifacts/patterns/sequential.workflow.yaml"),
        include_str!("../../../docs/examples/artifacts/patterns/parallel-review.workflow.yaml"),
        include_str!("../../../docs/examples/artifacts/patterns/react-agent.workflow.yaml"),
        include_str!("../../../docs/examples/artifacts/patterns/human-approval.workflow.yaml"),
        include_str!("../../../docs/examples/artifacts/patterns/custom-logic.workflow.yaml"),
    ];

    for source in examples {
        let document = ArtifactDocument::from_yaml(source).expect("parse pattern workflow");
        let report = validate_for_status(&document, ArtifactStatus::Draft);
        assert!(
            report.is_ok(),
            "pattern workflow should be draft-valid: {report:?}"
        );
        let yaml = document.to_yaml().expect("serialize pattern workflow");
        let reparsed = ArtifactDocument::from_yaml(&yaml).expect("parse serialized workflow");
        assert_eq!(reparsed, document);
    }
}

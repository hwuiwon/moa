use std::str::FromStr;

use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
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

    assert_error(
        &report,
        "definition.spec.display_name",
        "agent display_name must not be empty",
    );
    assert_error(
        &report,
        "definition.spec.purpose.summary",
        "agent purpose summary must not be empty",
    );
    assert_error(
        &report,
        "definition.spec.skill_policy.refs",
        "non-auto skill policy must include at least one reference",
    );
    assert_error(
        &report,
        "definition.spec.tool_policy.tools",
        "non-auto tool policy must include at least one tool",
    );
}

fn assert_error(report: &moa_artifacts::validation::ValidationReport, path: &str, message: &str) {
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == path && error.message == message),
        "expected validation error at {path} with message {message:?}, got {:?}",
        report.errors
    );
}

#[test]
fn draft_allows_unresolved_refs_but_published_rejects_them() {
    // Pins: visual-builder drafts may link procedure capabilities that are created later.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: damaged-food-order
status: draft
definition:
  type: skill
  spec:
    procedure:
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
  - path: definition.spec.procedure.nodes[1].ref
    ref: action://orders.submit_issue
    state: unresolved
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse skill artifact");

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
        "definition.spec.procedure.nodes[1].ref"
    );
}

#[test]
fn procedure_validation_rejects_duplicate_node_ids() {
    // Pins: skill procedure graphs must have unambiguous node identities.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: invalid-procedure
definition:
  type: skill
  spec:
    procedure:
      nodes:
        - id: start
          kind: start
        - id: start
          kind: end
      edges:
        - from: start
          to: missing
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid skill procedure");
    let report = validate_for_status(&document, ArtifactStatus::Draft);

    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message == "duplicate procedure node id"),
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
fn procedure_validation_rejects_executable_nodes_without_invocation_targets() {
    // Pins: published procedure action/tool nodes fail validation before runtime if no target can be executed.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: invalid-executable-procedure
definition:
  type: skill
  spec:
    procedure:
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
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid skill procedure");
    let report = validate_for_status(&document, ArtifactStatus::Published);

    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.procedure.nodes[1]"
                && error.message
                    == "procedure action node must specify ref, input.tool_name, or input.tool"
        }),
        "expected missing action invocation target error: {report:?}"
    );
    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.procedure.nodes[2]"
                && error.message
                    == "procedure tool node must specify exactly one tool_ref, input.tool_name, or input.tool"
        }),
        "expected missing tool invocation target error: {report:?}"
    );
}

#[test]
fn prompt_examples_parse_as_skill_procedures() {
    // Pins: docs skill examples stay executable by the canonical parser; the converted
    // procedure examples each keep a deterministic procedure graph, and the purely
    // agent-mediated example stays a procedure-less skill.
    let procedure_examples: [(&str, &str); 6] = [
        (
            "damaged-food-order",
            include_str!("../../../../docs/examples/artifacts/damaged-food-order.skill.yaml"),
        ),
        (
            "patterns/custom-logic",
            include_str!("../../../../docs/examples/artifacts/patterns/custom-logic.skill.yaml"),
        ),
        (
            "patterns/human-approval",
            include_str!("../../../../docs/examples/artifacts/patterns/human-approval.skill.yaml"),
        ),
        (
            "patterns/parallel-review",
            include_str!("../../../../docs/examples/artifacts/patterns/parallel-review.skill.yaml"),
        ),
        (
            "patterns/react-agent",
            include_str!("../../../../docs/examples/artifacts/patterns/react-agent.skill.yaml"),
        ),
        (
            "patterns/sequential",
            include_str!("../../../../docs/examples/artifacts/patterns/sequential.skill.yaml"),
        ),
    ];

    for (name, yaml) in procedure_examples {
        let procedure = parse_skill_example(name, yaml);
        assert!(
            procedure.procedure.is_some(),
            "example {name} should declare a procedure"
        );
    }

    // The transaction-dispute example is a purely agent-mediated skill with no procedure.
    let agent_mediated = parse_skill_example(
        "transaction-dispute",
        include_str!("../../../../docs/examples/artifacts/transaction-dispute.skill.yaml"),
    );
    assert!(
        agent_mediated.procedure.is_none(),
        "transaction-dispute example should stay procedure-less"
    );
}

fn parse_skill_example(name: &str, yaml: &str) -> moa_artifacts::skill::SkillDefinition {
    let document = ArtifactDocument::from_yaml(yaml)
        .unwrap_or_else(|error| panic!("example {name} should parse: {error}"));
    assert_eq!(
        document.kind,
        ArtifactKind::Skill,
        "example {name} should be a skill artifact"
    );
    let report = validate_for_status(&document, ArtifactStatus::Draft);
    assert!(
        report.is_ok(),
        "example {name} should be draft-valid: {report:?}"
    );
    let ArtifactDefinition::Skill(skill) = document.definition else {
        panic!("example {name} should yield a skill definition");
    };
    skill
}

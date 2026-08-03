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
fn artifact_ref_schema_matches_parser_and_canonical_round_trip() {
    // Pins: the planner schema and Rust parser accept the same canonical reference language.
    let schema = serde_json::to_value(schemars::schema_for!(ArtifactRef)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("compile artifact-ref schema");
    let boundary = format!("skill://{}", "a".repeat(512));
    let accepted = [
        "agent://support",
        "skill://research-v1",
        "connector://orders/api#v1",
        "action://refund-order",
        "action://orders.refund.v2",
        "experiment_plan://cohort~v1",
        "tool://mcp:search/web@v1#result",
        boundary.as_str(),
    ];
    for value in accepted {
        assert!(
            validator.is_valid(&serde_json::json!(value)),
            "schema: {value}"
        );
        let parsed = ArtifactRef::from_str(value).expect("parser accepts schema value");
        assert_eq!(parsed.canonical_string().expect("canonical"), value);
        assert_eq!(
            serde_json::to_string(&parsed).expect("serialize"),
            format!("\"{value}\"")
        );
    }

    let too_long = format!("skill://{}", "a".repeat(513));
    let rejected = [
        "Skill://support",
        "unknown://support",
        "skill://",
        "skill://-support",
        "skill://support-",
        "skill://white space",
        "skill://café",
        "skill://percent%20encoded",
        "skill://nested://target",
        "action://.refund",
        "action://orders.",
        too_long.as_str(),
    ];
    for value in rejected {
        assert!(
            !validator.is_valid(&serde_json::json!(value)),
            "schema: {value}"
        );
        assert!(ArtifactRef::from_str(value).is_err(), "parser: {value}");
    }
}

#[test]
fn artifact_ref_invalid_public_variants_fail_closed() {
    // Pins: unchecked constructors cannot emit a noncanonical reference through Display or serde.
    let invalid = [
        ArtifactRef::artifact(ArtifactKind::Skill, " leading"),
        ArtifactRef::artifact(ArtifactKind::Action, "orders.refund"),
        ArtifactRef::action("orders.v2", "refund"),
        ArtifactRef::action("orders", ""),
        ArtifactRef::tool("web search"),
    ];
    for reference in invalid {
        assert!(reference.canonical_string().is_err());
        assert!(serde_json::to_string(&reference).is_err());
    }
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
fn agent_connector_bindings_are_reference_visible_and_structurally_unique() {
    // Pins: authoring policies resolve one logical connector to one explicit
    // connection without adding a second top-level agent capability surface.
    let valid_yaml = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: billing-agent
definition:
  type: agent
  spec:
    display_name: Billing Agent
    purpose:
      summary: Manage billing operations.
    action_policy:
      allowed:
        - action://billing.CreateInvoice
      connector_bindings:
        - connector_ref: connector://billing
          connection_id: 018f8f1f-36a6-7c90-a7f8-2f2f57f5c111
"#;
    let document = ArtifactDocument::from_yaml(valid_yaml).expect("bound agent should parse");
    let references = document
        .reference_paths()
        .into_iter()
        .map(|(path, reference)| (path, reference.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        references,
        vec![
            (
                "definition.spec.action_policy.allowed[0]".to_string(),
                "action://billing.CreateInvoice".to_string(),
            ),
            (
                "definition.spec.action_policy.connector_bindings[0].connector_ref".to_string(),
                "connector://billing".to_string(),
            ),
        ]
    );
    assert!(
        validate_for_status(&document, ArtifactStatus::Draft).is_ok(),
        "one canonical connector binding should validate"
    );

    let duplicate_yaml = valid_yaml.replace(
        "          connection_id: 018f8f1f-36a6-7c90-a7f8-2f2f57f5c111",
        r#"          connection_id: 018f8f1f-36a6-7c90-a7f8-2f2f57f5c111
        - connector_ref: connector://billing
          connection_id: 018f8f1f-36a6-7c90-a7f8-2f2f57f5c111"#,
    );
    let duplicate = ArtifactDocument::from_yaml(&duplicate_yaml)
        .expect("duplicate binding fixture should parse before semantic validation");
    let report = validate_for_status(&duplicate, ArtifactStatus::Draft);
    assert_error(
        &report,
        "definition.spec.action_policy.connector_bindings[1].connector_ref",
        "duplicate connector binding reference",
    );
    assert_error(
        &report,
        "definition.spec.action_policy.connector_bindings[1].connection_id",
        "connection may be bound to only one logical connector reference",
    );
}

#[test]
fn empty_agent_connector_bindings_preserve_the_pre_t1_policy_shape() {
    // Pins: agents without connection bindings keep byte-identical authoring policy JSON.
    let yaml = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: legacy-agent
definition:
  type: agent
  spec:
    display_name: Legacy Agent
    purpose:
      summary: Continue serving legacy actions.
    action_policy:
      allowed:
        - action://legacy-action
      require_admin_review:
        - action://legacy-action
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("legacy agent should parse");
    let encoded = serde_json::to_value(&document).expect("legacy agent should serialize");
    assert_eq!(
        encoded["definition"]["spec"]["action_policy"],
        serde_json::json!({
            "allowed": ["action://legacy-action"],
            "require_admin_review": ["action://legacy-action"]
        })
    );
}

#[test]
fn agent_connector_binding_rejects_embedded_credential_material() {
    // Pins: agent artifacts select only a connection ID and cannot carry its secret.
    let yaml = r#"
api_version: moa.artifact/v1
kind: agent
metadata:
  name: unsafe-agent
definition:
  type: agent
  spec:
    display_name: Unsafe Agent
    purpose:
      summary: This fixture must fail before it can persist.
    action_policy:
      connector_bindings:
        - connector_ref: connector://billing
          connection_id: 018f8f1f-36a6-7c90-a7f8-2f2f57f5c111
          credential: plaintext-must-not-decode
"#;
    assert!(
        ArtifactDocument::from_yaml(yaml).is_err(),
        "connector binding DTO must reject credential material"
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
    // Pins: execution-plan agent skill_refs use the existing draft/publish artifact-reference path.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: damaged-food-order
status: draft
definition:
  type: skill
  spec:
    execution_plan:
      goal:
        requirements:
          - id: req_customer_advice
            description: Advise the customer.
        deliverables: []
        coverage: []
        constraints: []
        completion_checks: []
      plan:
        schema_version: 1
        input_schema: { type: object }
        output_schema: { type: object }
        nodes:
          - id: advise_customer
            requirement_ids: [req_customer_advice]
            depends_on: []
            input: {}
            output_schema: { type: object }
            operation:
              kind: agent
              instructions: Advise the customer about the damaged order.
              skill_refs: [skill://customer-reassurance]
              capability_refs: []
              max_turns: 1
            retry:
              max_attempts: 1
              initial_backoff_ms: 0
              max_backoff_ms: 0
          - id: output
            requirement_ids: [req_customer_advice]
            depends_on: [advise_customer]
            input: {}
            output_schema: { type: object }
            operation:
              kind: output
              value:
                $ref: $.nodes.advise_customer.output
            retry:
              max_attempts: 1
              initial_backoff_ms: 0
              max_backoff_ms: 0
reference_resolutions:
  - path: definition.spec.execution_plan.plan.nodes[0].operation.skill_refs[0]
    ref: skill://customer-reassurance
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
        "definition.spec.execution_plan.plan.nodes[0].operation.skill_refs[0]"
    );
    assert_eq!(
        document.reference_paths(),
        vec![(
            "definition.spec.execution_plan.plan.nodes[0].operation.skill_refs[0]".to_string(),
            ArtifactRef::from_str("skill://customer-reassurance")
                .expect("parse execution-plan skill reference"),
        )]
    );
}

#[test]
fn execution_plan_validation_rejects_duplicate_node_ids() {
    // Pins: skill execution plans must have unambiguous stable node identities.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: invalid-execution-plan
definition:
  type: skill
  spec:
    execution_plan:
      goal:
        requirements:
          - id: req_one
            description: Exercise duplicate node validation.
        deliverables: []
        coverage: []
        constraints: []
        completion_checks: []
      plan:
        schema_version: 1
        input_schema: { type: object }
        output_schema: { type: object }
        nodes:
          - id: duplicate
            requirement_ids: [req_one]
            depends_on: []
            input: {}
            output_schema: { type: object }
            operation:
              kind: capability
              reference: { name: first.lookup, version: v1 }
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
          - id: duplicate
            requirement_ids: [req_one]
            depends_on: []
            input: {}
            output_schema: { type: object }
            operation:
              kind: capability
              reference: { name: second.lookup, version: v1 }
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
          - id: output
            requirement_ids: [req_one]
            depends_on: [duplicate]
            input: {}
            output_schema: { type: object }
            operation:
              kind: output
              value: { $ref: $.nodes.duplicate.output }
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid skill execution plan");
    let report = validate_for_status(&document, ArtifactStatus::Draft);

    assert!(
        report
            .errors
            .iter()
            .any(|error| error.message == "duplicate execution node id"),
        "expected duplicate-node error: {report:?}"
    );
}

#[test]
fn execution_plan_validation_rejects_malformed_capability_and_unbounded_agent() {
    // Pins: execution plans reject malformed capability syntax and zero-turn agents before publish.
    let yaml = r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: invalid-execution-targets
definition:
  type: skill
  spec:
    execution_plan:
      goal:
        requirements:
          - id: req_target
            description: Exercise target validation.
        deliverables: []
        coverage: []
        constraints: []
        completion_checks: []
      plan:
        schema_version: 1
        input_schema: { type: object }
        output_schema: { type: object }
        nodes:
          - id: malformed_capability
            requirement_ids: [req_target]
            depends_on: []
            input: {}
            output_schema: { type: object }
            operation:
              kind: capability
              reference: { name: "bad capability", version: v1 }
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
          - id: unbounded_agent
            requirement_ids: [req_target]
            depends_on: []
            input: {}
            output_schema: { type: object }
            operation:
              kind: agent
              instructions: Summarize the result.
              skill_refs: []
              capability_refs: []
              max_turns: 0
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
          - id: output
            requirement_ids: [req_target]
            depends_on: [malformed_capability, unbounded_agent]
            input: {}
            output_schema: { type: object }
            operation:
              kind: output
              value: {}
            retry: { max_attempts: 1, initial_backoff_ms: 0, max_backoff_ms: 0 }
"#;
    let document = ArtifactDocument::from_yaml(yaml).expect("parse invalid skill execution plan");
    let report = validate_for_status(&document, ArtifactStatus::Published);

    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.execution_plan.plan.nodes[0].operation.reference.name"
                && error.message
                    == "capability name must be a non-empty ASCII name of at most 256 characters"
        }),
        "expected malformed capability error: {report:?}"
    );
    assert!(
        report.errors.iter().any(|error| {
            error.path == "definition.spec.execution_plan.plan.nodes[1].operation.max_turns"
                && error.message == "agent max_turns must be at least one"
        }),
        "expected zero-turn agent error: {report:?}"
    );
}

#[test]
fn prompt_examples_parse_as_skill_execution_plans() {
    // Pins: docs skill examples stay executable by the canonical parser; the converted
    // examples each keep an execution plan, and the agent-mediated example stays plan-less.
    let execution_plan_examples: [(&str, &str); 6] = [
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

    for (name, yaml) in execution_plan_examples {
        let skill = parse_skill_example(name, yaml);
        assert!(
            skill.execution_plan.is_some(),
            "example {name} should declare an execution plan"
        );
    }

    // The transaction-dispute example is purely agent-mediated with no execution plan.
    let agent_mediated = parse_skill_example(
        "transaction-dispute",
        include_str!("../../../../docs/examples/artifacts/transaction-dispute.skill.yaml"),
    );
    assert!(
        agent_mediated.execution_plan.is_none(),
        "transaction-dispute example should stay execution-plan-less"
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

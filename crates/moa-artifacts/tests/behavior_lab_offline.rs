use std::str::FromStr;

use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::{ArtifactRef, ReferenceResolution};
use moa_artifacts::validation::{ValidationError, ValidationReport, validate_for_status};
use serde_json::{Value, json};

#[test]
fn behavior_lab_plan_round_trips_json_yaml_and_exposes_external_refs() {
    // Pins: behavior-lab simulation blocks are embedded in one experiment_plan artifact.
    let yaml = r#"
api_version: moa.artifact/v1
kind: experiment_plan
metadata:
  name: checkout-behavior
  description: Trial matrix for checkout support behavior.
status: draft
definition:
  type: experiment_plan
  spec:
    simulation:
      scenarios:
        - id: checkout-delay
          initial_situation: The shopper asks why checkout is delayed.
          goals:
            - Get a clear next step.
          success_criteria:
            - The agent explains the delay.
          max_turns: 3
          data_bundle_ids:
            - orders-fixtures
      personas:
        - id: careful-shopper
          voice: Patient and precise.
          goals:
            - Resolve the checkout delay.
          stop_behavior: Stop after the agent gives a concrete next step.
      profiles:
        - id: vip-customer
          facts:
            account_tier: vip
      data_bundles:
        - id: orders-fixtures
          sources:
            - id: orders
              kind: connector_fixture
              connector_ref: connector://orders
              fixture:
                order_id: ORDER-42
    target_variants:
      - key: agent-loop
        kind: agent_loop
    simulator_model: gpt-4.1-mini
    target_model: gpt-4.1-mini
    parallelism: 2
    trials_per_combination: 3
    budget:
      max_total_cents: 5000
      max_trial_cents: 1000
      max_total_tokens: 100000
      max_trial_tokens: 10000
    scorecard:
      metrics:
        - id: resolution
    learning_proposals:
      enabled: true
ui:
  label: Checkout behavior
  layout:
    x: 12
"#;

    let yaml_doc = ArtifactDocument::from_yaml(yaml).expect("parse behavior-lab yaml");
    let json_doc = ArtifactDocument::from_json(
        &yaml_doc
            .to_json()
            .expect("serialize behavior-lab yaml doc as json"),
    )
    .expect("parse behavior-lab json");
    let yaml_export_doc = ArtifactDocument::from_yaml(
        &yaml_doc
            .to_yaml()
            .expect("serialize behavior-lab doc as yaml"),
    )
    .expect("parse exported behavior-lab yaml");

    assert_eq!(yaml_doc, json_doc);
    assert_eq!(yaml_doc, yaml_export_doc);
    assert_eq!(yaml_doc.kind, ArtifactKind::ExperimentPlan);

    let report = validate_for_status(&yaml_doc, ArtifactStatus::Draft);
    assert!(
        report.is_ok(),
        "plan should validate as a draft: {report:?}"
    );

    let paths = yaml_doc
        .reference_paths()
        .into_iter()
        .map(|(path, artifact_ref)| (path, artifact_ref.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![(
            "definition.spec.simulation.data_bundles[0].sources[0].connector_ref".to_string(),
            "connector://orders".to_string()
        )]
    );
}

#[test]
fn behavior_lab_schema_has_only_experiment_plan_as_simulation_artifact_kind() {
    // Pins: simulation pieces are embedded plan blocks, not standalone artifact families.
    let schema: Value = serde_json::from_str(include_str!(
        "../../../docs/schemas/moa-artifact-v1.schema.json"
    ))
    .expect("parse artifact schema");
    let kind_labels = enum_labels(&schema, "/properties/kind/enum");
    let type_labels = enum_labels(&schema, "/properties/definition/properties/type/enum");

    for label in [
        "simulation_persona",
        "simulation_profile",
        "simulation_data_bundle",
        "simulation_scenario",
    ] {
        assert!(
            !kind_labels.contains(&label),
            "schema kind enum should not contain {label}"
        );
        assert!(
            !type_labels.contains(&label),
            "schema type enum should not contain {label}"
        );
        assert!(ArtifactKind::from_str(label).is_err());
    }
    assert!(kind_labels.contains(&"experiment_plan"));
    assert!(type_labels.contains(&"experiment_plan"));
    assert_eq!(
        ArtifactKind::from_str("experiment_plan")
            .expect("parse experiment_plan kind")
            .to_string(),
        "experiment_plan"
    );
}

#[test]
fn artifact_refs_parse_and_format_generic_artifact_schemes() {
    // Pins: references grow with ArtifactKind instead of a separate mirrored enum.
    let cases = [
        ("skill://refunds", Some(ArtifactKind::Skill), "refunds"),
        (
            "connector://orders",
            Some(ArtifactKind::Connector),
            "orders",
        ),
        (
            "experiment_plan://checkout-behavior",
            Some(ArtifactKind::ExperimentPlan),
            "checkout-behavior",
        ),
        ("tool://file_read", None, "file_read"),
    ];

    for (input, kind, target) in cases {
        let artifact_ref = ArtifactRef::from_str(input).expect("parse reference");
        assert_eq!(artifact_ref.artifact_kind(), kind.as_ref());
        assert_eq!(artifact_ref.target_name(), target);
        assert_eq!(artifact_ref.action_name(), None);
        assert_eq!(artifact_ref.to_string(), input);
    }

    let action = ArtifactRef::from_str("action://orders.lookup").expect("parse action reference");
    assert_eq!(action.artifact_kind(), None);
    assert_eq!(action.target_name(), "orders");
    assert_eq!(action.action_name(), Some("lookup"));
    assert_eq!(action.to_string(), "action://orders.lookup");
}

#[test]
fn plan_validation_rejects_invalid_embedded_simulation_and_wrong_refs() {
    // Pins: plan expansion is bounded and validates embedded simulation IDs before execution.
    let document = ArtifactDocument::from_json(
        &json!({
            "api_version": "moa.artifact/v1",
            "kind": "experiment_plan",
            "metadata": { "name": "invalid-plan" },
            "definition": {
                "type": "experiment_plan",
                "spec": {
                    "simulation": {
                        "scenarios": [{
                            "id": "checkout-delay",
                            "initial_situation": "The user asks about a delayed order.",
                            "goals": [],
                            "success_criteria": ["The agent gives a concrete next step."],
                            "max_turns": 101,
                            "data_bundle_ids": ["missing-bundle"]
                        }],
                        "personas": [{
                            "id": "careful-shopper",
                            "voice": "",
                            "goals": ["Resolve a checkout delay."],
                            "stop_behavior": ""
                        }],
                        "profiles": [{
                            "id": "vip-customer",
                            "facts": {}
                        }],
                        "data_bundles": []
                    },
                    "target_variants": [
                        {
                            "key": "agent-loop",
                            "kind": "agent_loop"
                        }
                    ],
                    "simulator_model": "",
                    "parallelism": 0,
                    "trials_per_combination": 101,
                    "budget": {
                        "max_total_cents": 1000001,
                        "max_trial_cents": 100001,
                        "max_total_tokens": 10000001,
                        "max_trial_tokens": 1000001
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("parse invalid plan");

    let report = validate_for_status(&document, ArtifactStatus::Draft);

    assert_error(
        &report,
        "definition.spec.simulation.scenarios[0].goals",
        "simulation scenario must include at least one goal",
    );
    assert_error(
        &report,
        "definition.spec.simulation.scenarios[0].max_turns",
        "scenario max_turns must be between 1 and 100",
    );
    assert_error(
        &report,
        "definition.spec.simulation.scenarios[0].data_bundle_ids[0]",
        "scenario data bundle id must exist in simulation.data_bundles",
    );
    assert_error(
        &report,
        "definition.spec.simulation.personas[0].voice",
        "persona voice must not be empty",
    );
    assert_error(
        &report,
        "definition.spec.simulation.profiles[0].facts",
        "simulation profile facts must be a non-empty object",
    );
    assert_error(
        &report,
        "definition.spec.parallelism",
        "experiment plan parallelism must be between 1 and 64",
    );
}

#[test]
fn published_behavior_lab_docs_reject_unresolved_external_refs_from_resolver() {
    // Pins: publish validation rejects unresolved external refs, not embedded simulation IDs.
    let mut document =
        ArtifactDocument::from_json(&minimal_valid_plan()).expect("parse valid plan");
    document.reference_resolutions = vec![ReferenceResolution::unresolved(
        "definition.spec.simulation.data_bundles[0].sources[0].connector_ref",
        ArtifactRef::connector("missing-orders"),
    )];

    let report = validate_for_status(&document, ArtifactStatus::Published);

    assert_error(
        &report,
        "definition.spec.simulation.data_bundles[0].sources[0].connector_ref",
        "unresolved reference connector://missing-orders",
    );
}

fn enum_labels<'a>(schema: &'a Value, pointer: &str) -> Vec<&'a str> {
    schema
        .pointer(pointer)
        .and_then(Value::as_array)
        .expect("schema enum exists")
        .iter()
        .map(|value| value.as_str().expect("schema enum entry is a string"))
        .collect()
}

fn minimal_valid_plan() -> String {
    json!({
        "api_version": "moa.artifact/v1",
        "kind": "experiment_plan",
        "metadata": { "name": "checkout-behavior" },
        "definition": {
            "type": "experiment_plan",
            "spec": {
                "simulation": {
                    "scenarios": [{
                        "id": "checkout-delay",
                        "initial_situation": "The user asks about a delayed order.",
                        "goals": ["Resolve a checkout delay."],
                        "success_criteria": ["The agent gives a concrete next step."],
                        "max_turns": 3
                    }],
                    "personas": [{
                        "id": "careful-shopper",
                        "voice": "Patient and precise.",
                        "goals": ["Resolve a checkout delay."],
                        "stop_behavior": "Stop after a concrete next step."
                    }],
                    "profiles": [{
                        "id": "vip-customer",
                        "facts": { "account_tier": "vip" }
                    }]
                },
                "target_variants": [
                    { "key": "agent-loop", "kind": "agent_loop" }
                ],
                "simulator_model": "gpt-4.1-mini",
                "parallelism": 1,
                "trials_per_combination": 1,
                "budget": { "max_total_cents": 1000 }
            }
        }
    })
    .to_string()
}

fn assert_error(report: &ValidationReport, path: &str, message: &str) {
    let expected = ValidationError {
        path: path.to_string(),
        message: message.to_string(),
    };
    assert!(
        report.errors.contains(&expected),
        "expected validation error {expected:?}, got {report:?}"
    );
}

//! Offline coverage for connector and connector-backed action governance.

use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::validation::{ValidationError, validate_for_status};

#[test]
fn connector_backed_actions_require_a_named_backing_tool() {
    // Pins: a connector reference cannot become an executable action without an explicit tool.
    let missing_tool = action_document("connector_ref: connector://orders");
    let blank_tool = action_document(
        r#"connector_ref: connector://orders
    tool_name: "   ""#,
    );
    let tool_only = action_document("tool_name: orders.refund");

    assert_eq!(
        validate_for_status(&missing_tool, ArtifactStatus::Draft).errors,
        vec![ValidationError {
            path: "definition.spec.tool_name".to_string(),
            message: "connector-backed action must name a backing tool".to_string(),
        }],
        "connector-backed actions without a tool must fail closed"
    );
    assert_eq!(
        validate_for_status(&blank_tool, ArtifactStatus::Draft).errors,
        vec![ValidationError {
            path: "definition.spec.tool_name".to_string(),
            message: "action tool_name must not be empty".to_string(),
        }],
        "blank connector-backed tool names must not satisfy the binding"
    );
    assert!(
        validate_for_status(&tool_only, ArtifactStatus::Draft).is_ok(),
        "standalone actions may continue to bind directly to a named tool"
    );
}

fn action_document(binding: &str) -> ArtifactDocument {
    ArtifactDocument::from_yaml(&format!(
        r#"api_version: moa.artifact/v1
kind: action
metadata:
  name: refund-order
definition:
  type: action
  spec:
    id: refund-order
    {binding}
"#
    ))
    .expect("fixture: action artifact YAML should parse")
}

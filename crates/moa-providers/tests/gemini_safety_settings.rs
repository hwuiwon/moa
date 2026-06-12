//! Snapshot coverage for Gemini safety settings and provider-specific envelope fields.

use std::collections::HashMap;

use moa_core::{CompletionRequest, ContextMessage, JsonResponseFormat, ModelId};
use moa_providers::debug_build_gemini_request_body;
use serde_json::{Value, json};

const MODEL: &str = "gemini-3-flash-preview";
const SAFETY_THRESHOLD_METADATA_KEY: &str = "_moa.gemini.safety_threshold";

#[test]
fn gemini_request_includes_default_safety_settings_for_4_categories() {
    let body = gemini_body(&base_request());

    assert_eq!(
        body["safetySettings"]
            .as_array()
            .expect("safetySettings array")
            .len(),
        4
    );
    snapshot_json(
        "gemini_safety_settings__gemini_request_includes_default_safety_settings_for_4_categories",
        &body,
    );
}

#[test]
fn gemini_request_with_custom_safety_threshold_overrides_default() {
    let mut request = base_request();
    request.metadata.insert(
        SAFETY_THRESHOLD_METADATA_KEY.to_string(),
        json!("BLOCK_ONLY_HIGH"),
    );
    let body = gemini_body(&request);

    assert!(
        body["safetySettings"]
            .as_array()
            .expect("safetySettings array")
            .iter()
            .all(|setting| setting["threshold"] == "BLOCK_ONLY_HIGH")
    );
    snapshot_json(
        "gemini_safety_settings__gemini_request_with_custom_safety_threshold_overrides_default",
        &body,
    );
}

#[test]
fn gemini_request_with_json_response_mime_type_includes_field() {
    let mut request = base_request();
    request.response_format = Some(JsonResponseFormat::strict_json_schema(
        "status_report",
        "Status report.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "status": { "type": "string" }
            },
            "required": ["status"]
        }),
    ));
    let body = gemini_body(&request);

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    snapshot_json(
        "gemini_safety_settings__gemini_request_with_json_response_mime_type_includes_field",
        &body,
    );
}

#[test]
fn gemini_request_with_function_declarations_serializes_correctly() {
    let mut request = base_request();
    request.tools = vec![file_read_tool(), shell_command_tool()];
    let body = gemini_body(&request);

    assert_eq!(
        body["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("function declarations")
            .len(),
        2
    );
    snapshot_json(
        "gemini_safety_settings__gemini_request_with_function_declarations_serializes_correctly",
        &body,
    );
}

fn gemini_body(request: &CompletionRequest) -> Value {
    debug_build_gemini_request_body(request, false).expect("Gemini request body should build")
}

fn snapshot_json(name: &str, value: &Value) {
    insta::with_settings!({ prepend_module_to_snapshot => false, sort_maps => true }, {
        insta::assert_json_snapshot!(name, value);
    });
}

fn base_request() -> CompletionRequest {
    CompletionRequest {
        model: Some(ModelId::new(MODEL)),
        messages: vec![
            ContextMessage::system("Follow the workspace policy."),
            ContextMessage::user("Inspect the provider request shape."),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(256),
        temperature: Some(0.0),
        response_format: None,
        metadata: HashMap::new(),
    }
}

fn file_read_tool() -> Value {
    json!({
        "name": "file_read",
        "description": "Read a UTF-8 file from the workspace.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path"
                }
            },
            "required": ["path"]
        }
    })
}

fn shell_command_tool() -> Value {
    json!({
        "name": "shell_command",
        "description": "Run a read-only shell command.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Command and arguments"
                }
            },
            "required": ["cmd"]
        }
    })
}

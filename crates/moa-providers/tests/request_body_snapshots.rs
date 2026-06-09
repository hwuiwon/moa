//! Snapshot tests for provider request-body wire shapes.

#![allow(non_snake_case)]

use moa_core::{CompletionRequest, ContextMessage, ModelId};
use moa_providers::{
    debug_build_anthropic_request_body, debug_build_gemini_request_body,
    debug_build_openai_request_body,
};
use serde_json::{Value, json};

const SYSTEM_PROMPT: &str = "You are MOA. Use tools only when they are necessary.";
const USER_MESSAGE: &str = "Read the project status and list the next shell command.";
const MAX_OUTPUT_TOKENS: usize = 256;
const TEMPERATURE: f32 = 0.2;

const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const OPENAI_MODEL: &str = "gpt-5.4-mini";
const GEMINI_MODEL: &str = "gemini-3-flash-preview";

const FILE_READ_TOOL_NAME: &str = "file_read";
const FILE_READ_TOOL_DESCRIPTION: &str = "Read a UTF-8 text file from the workspace.";
const FILE_READ_TOOL_PROPERTY: &str = "path";
const FILE_READ_TOOL_PROPERTY_DESCRIPTION: &str = "Workspace-relative file path.";

const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
const SHELL_COMMAND_TOOL_DESCRIPTION: &str = "Run a read-only shell command in the workspace.";
const SHELL_COMMAND_TOOL_PROPERTY: &str = "cmd";
const SHELL_COMMAND_TOOL_PROPERTY_DESCRIPTION: &str = "Command and arguments to run.";

#[test]
fn anthropic_request_body__minimal_request_serializes_with_stable_byte_layout() {
    let request = minimal_request(ANTHROPIC_MODEL);
    let body = debug_build_anthropic_request_body(&request, false)
        .expect("anthropic request body should build");

    insta::with_settings!({ prepend_module_to_snapshot => false, sort_maps => true }, {
        insta::assert_json_snapshot!("anthropic_request_body__minimal_request", body, {
            ".metadata.request_id" => "[redacted]",
            ".timestamp" => "[redacted]"
        });
    });
}

#[test]
fn openai_request_body__minimal_request_serializes_with_stable_byte_layout() {
    let request = minimal_request(OPENAI_MODEL);
    let body =
        debug_build_openai_request_body(&request, false).expect("openai request body should build");

    insta::with_settings!({ prepend_module_to_snapshot => false, sort_maps => true }, {
        insta::assert_json_snapshot!("openai_request_body__minimal_request", body, {
            ".metadata.request_id" => "[redacted]",
            ".timestamp" => "[redacted]"
        });
    });
}

#[test]
fn gemini_request_body__minimal_request_serializes_with_stable_byte_layout() {
    let request = minimal_request(GEMINI_MODEL);
    let body =
        debug_build_gemini_request_body(&request, false).expect("gemini request body should build");

    insta::with_settings!({ prepend_module_to_snapshot => false, sort_maps => true }, {
        insta::assert_json_snapshot!("gemini_request_body__minimal_request", body, {
            ".metadata.request_id" => "[redacted]",
            ".timestamp" => "[redacted]"
        });
    });
}

fn minimal_request(model: &str) -> CompletionRequest {
    CompletionRequest {
        model: Some(ModelId::new(model)),
        messages: vec![
            ContextMessage::system(SYSTEM_PROMPT),
            ContextMessage::user(USER_MESSAGE),
        ],
        tools: vec![file_read_tool(), shell_command_tool()],
        max_output_tokens: Some(MAX_OUTPUT_TOKENS),
        temperature: Some(TEMPERATURE),
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    }
}

fn file_read_tool() -> Value {
    tool_schema(
        FILE_READ_TOOL_NAME,
        FILE_READ_TOOL_DESCRIPTION,
        FILE_READ_TOOL_PROPERTY,
        FILE_READ_TOOL_PROPERTY_DESCRIPTION,
    )
}

fn shell_command_tool() -> Value {
    tool_schema(
        SHELL_COMMAND_TOOL_NAME,
        SHELL_COMMAND_TOOL_DESCRIPTION,
        SHELL_COMMAND_TOOL_PROPERTY,
        SHELL_COMMAND_TOOL_PROPERTY_DESCRIPTION,
    )
}

fn tool_schema(name: &str, description: &str, property: &str, property_description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                property: {
                    "type": "string",
                    "description": property_description
                }
            },
            "required": [property]
        }
    })
}

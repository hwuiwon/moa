//! Offline contract tests for durable tool requests and output codecs.

use std::time::Duration;

use uuid::Uuid;

use moa_core::{
    traits::{Identity, IdentityType},
    types::{
        events_stream::ClaimCheck,
        identifiers::{SessionId, TenantId, ToolCallId},
        tools::{
            ProcessOutput, ToolArtifactByteRange, ToolArtifactStream, ToolCallRequest, ToolContent,
            ToolOutput, ToolOutputArtifact,
        },
    },
};

#[test]
fn tool_call_request_requires_persisted_session_and_contract_identity() {
    // Pins: every durable tool execution names its exact session and admitted
    // catalog contract on the wire.
    let request = ToolCallRequest {
        tool_call_id: ToolCallId::new(),
        caller_identity: Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::now_v7(),
            tenant_id: TenantId::new(),
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        provider_tool_use_id: None,
        tool_name: "memory_search".to_string(),
        expected_tool_contract_revision: "contract-v1".to_string(),
        input: serde_json::json!({}),
        active_canary: None,
        session_id: SessionId::new(),
        trusted_sandbox_manifest: None,
        worker_id: None,
        resource_budget: Default::default(),
    };
    let mut missing_session = serde_json::to_value(request.clone()).expect("serialize request");
    missing_session
        .as_object_mut()
        .expect("tool request should serialize as an object")
        .remove("session_id");

    let error = serde_json::from_value::<ToolCallRequest>(missing_session)
        .expect_err("missing persisted session id must fail decoding");

    assert!(error.to_string().contains("missing field `session_id`"));

    let mut missing_contract = serde_json::to_value(request).expect("serialize request");
    missing_contract
        .as_object_mut()
        .expect("tool request should serialize as an object")
        .remove("expected_tool_contract_revision");

    let error = serde_json::from_value::<ToolCallRequest>(missing_contract)
        .expect_err("missing admitted contract must fail decoding");

    assert!(
        error
            .to_string()
            .contains("missing field `expected_tool_contract_revision`")
    );
}

#[test]
fn tool_output_text_creates_single_text_block() {
    let output = ToolOutput::text("hello", Duration::from_millis(5));

    assert!(!output.is_error);
    assert_eq!(
        output.content,
        vec![ToolContent::Text {
            text: "hello".to_string()
        }]
    );
    assert!(!output.truncated);
    assert_eq!(output.to_text(), "hello");
}

#[test]
fn tool_output_from_process_success_preserves_stdout() {
    let output = ToolOutput::from_process(
        "hello\n".to_string(),
        String::new(),
        0,
        Duration::from_millis(1),
    );

    assert!(!output.is_error);
    assert!(!output.truncated);
    assert_eq!(output.process_exit_code(), Some(0));
    assert_eq!(output.process_stdout(), Some("hello\n"));
    assert_eq!(output.to_text(), "hello");
}

#[test]
fn process_output_has_one_canonical_stream_carrier() {
    // Pins: process streams live once in memory even though the explicit
    // durable codec materializes the parent-compatible DTO on serialization.
    let output = ToolOutput::from_process(
        "hello\n".to_string(),
        "warning\n".to_string(),
        0,
        Duration::from_millis(1),
    );

    assert_eq!(output.content.len(), 1);
    let ToolContent::Process { output: process } = &output.content[0] else {
        panic!("process output should have one process content carrier");
    };
    assert_eq!(process.stdout, "hello\n");
    assert_eq!(process.stderr, "warning\n");
    assert!(output.structured.is_none());
    assert!(output.structured_payload().is_none());

    let encoded = serde_json::to_value(&output).expect("serialize process output");
    assert_eq!(encoded["content"][0]["type"], "text");
    assert_eq!(encoded["structured"]["stdout"], "hello\n");
    assert_eq!(encoded["structured"]["stderr"], "warning\n");
    assert!(
        encoded["content"]
            .as_array()
            .expect("wire content array")
            .iter()
            .all(|block| block["type"] != "process"),
        "retained parent readers must never receive the process discriminator"
    );

    let replayed: ToolOutput = serde_json::from_value(encoded).expect("replay process output");
    assert_eq!(replayed.content.len(), 1);
    assert!(matches!(replayed.content[0], ToolContent::Process { .. }));
    assert!(replayed.structured.is_none());
    assert_eq!(replayed.process_stdout(), Some("hello\n"));
    assert_eq!(replayed.process_stderr(), Some("warning\n"));
    assert_eq!(replayed.process_exit_code(), Some(0));
    assert_eq!(replayed.to_text(), "hello\n\nstderr:\nwarning");
}

#[test]
fn process_output_decodes_transient_process_wire_shape() {
    // Pins: the immediately preceding runtime revision briefly emitted the
    // process discriminator, so current readers must retain that rollout row.
    let encoded = serde_json::json!({
        "content": [{
            "type": "process",
            "output": {
                "stdout": "row from the prior revision\n",
                "stderr": "",
                "exit_code": 0,
                "stdout_truncated": false,
                "stderr_truncated": false
            }
        }],
        "is_error": false,
        "structured": null,
        "duration": { "secs": 0, "nanos": 1 },
        "truncated": false
    });

    let replayed: ToolOutput =
        serde_json::from_value(encoded).expect("decode transient process wire row");

    assert_eq!(replayed.content.len(), 1);
    assert_eq!(
        replayed.process_stdout(),
        Some("row from the prior revision\n")
    );
    assert!(replayed.structured.is_none());
}

#[test]
fn tool_output_from_process_failure_includes_exit_code_and_stderr() {
    let output = ToolOutput::from_process(
        "partial".to_string(),
        "boom".to_string(),
        7,
        Duration::from_millis(2),
    );

    assert!(output.is_error);
    assert!(!output.truncated);
    assert_eq!(output.process_exit_code(), Some(7));
    assert_eq!(output.process_stderr(), Some("boom"));
    assert!(output.to_text().contains("stderr:\nboom"));
    assert!(output.to_text().contains("exit_code: 7"));
}

#[test]
fn tool_output_json_creates_text_and_json_blocks() {
    let data = serde_json::json!([{ "path": "a.txt" }]);
    let output = ToolOutput::json("2 matches", data.clone(), Duration::from_millis(3));

    assert!(!output.is_error);
    assert!(matches!(output.content[0], ToolContent::Text { .. }));
    assert!(matches!(output.content[1], ToolContent::Json { .. }));
    assert!(output.structured.is_none());
    assert_eq!(output.structured_payload(), Some(&data));
    assert!(!output.truncated);
    assert!(output.to_text().contains("2 matches"));
    assert!(output.to_text().contains("\"path\": \"a.txt\""));

    let encoded = serde_json::to_value(&output).expect("serialize JSON output");
    assert_eq!(encoded["structured"], data);
    let replayed: ToolOutput = serde_json::from_value(encoded).expect("replay JSON output");
    assert_eq!(replayed.structured, None);
    assert_eq!(replayed.structured_payload(), Some(&data));
}

fn legacy_wire(
    content: &[&str],
    structured: serde_json::Value,
    is_error: bool,
    truncated: bool,
) -> serde_json::Value {
    let content = content
        .iter()
        .map(|text| serde_json::json!({ "type": "text", "text": text }))
        .collect::<Vec<_>>();
    serde_json::json!({
        "content": content,
        "is_error": is_error,
        "structured": structured,
        "duration": { "secs": 0, "nanos": 1 },
        "truncated": truncated
    })
}

fn assert_rejected_legacy_candidate_is_preserved(label: &str, encoded: serde_json::Value) {
    let expected_content = encoded["content"]
        .as_array()
        .expect("legacy content should be an array")
        .iter()
        .map(|block| ToolContent::Text {
            text: block["text"]
                .as_str()
                .expect("legacy test content should be text")
                .to_string(),
        })
        .collect::<Vec<_>>();
    let expected_structured = encoded["structured"].clone();

    let replayed: ToolOutput = serde_json::from_value(encoded)
        .unwrap_or_else(|error| panic!("{label}: rejected candidate should decode: {error}"));

    assert_eq!(replayed.content, expected_content, "{label}: content");
    assert_eq!(
        replayed.structured,
        Some(expected_structured),
        "{label}: structured payload"
    );
}

#[test]
fn legacy_process_three_and_five_key_rows_promote_exactly() {
    // Pins: exact historical parent rows, including the unavoidable exact
    // collision with manually authored structured data, become one process carrier.
    let three_key = legacy_wire(
        &["hello\n"],
        serde_json::json!({
            "stdout": "hello\n",
            "stderr": "",
            "exit_code": 0
        }),
        false,
        false,
    );
    let replayed: ToolOutput =
        serde_json::from_value(three_key).expect("decode exact three-key legacy process");
    assert_eq!(
        replayed.content,
        vec![ToolContent::Process {
            output: ProcessOutput {
                stdout: "hello\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                stdout_truncated: false,
                stderr_truncated: false,
            }
        }]
    );
    assert!(replayed.structured.is_none());

    let five_key = legacy_wire(
        &["out \n", "stderr:\nwarn\t\n", "exit_code: 7"],
        serde_json::json!({
            "stdout": "out \n",
            "stderr": "warn\t\n",
            "exit_code": 7,
            "stdout_truncated": false,
            "stderr_truncated": true
        }),
        true,
        true,
    );
    let replayed: ToolOutput =
        serde_json::from_value(five_key).expect("decode exact five-key legacy process");
    assert_eq!(replayed.process_stdout(), Some("out \n"));
    assert_eq!(replayed.process_stderr(), Some("warn\t\n"));
    assert_eq!(replayed.process_exit_code(), Some(7));
    assert!(!replayed.process_stdout_truncated());
    assert!(replayed.process_stderr_truncated());
    assert!(replayed.structured.is_none());
}

#[test]
fn matching_process_like_json_block_keeps_canonical_json_precedence() {
    // Pins: a tool's canonical JSON remains JSON even when its exact object
    // and summary happen to collide with the historical process encoding.
    let data = serde_json::json!({
        "stdout": "manual\n",
        "stderr": "",
        "exit_code": 0
    });
    let output = ToolOutput::json("manual\n", data.clone(), Duration::from_nanos(1));
    let expected_content = output.content.clone();

    let encoded = serde_json::to_value(&output).expect("serialize process-like JSON output");
    let replayed: ToolOutput =
        serde_json::from_value(encoded).expect("replay process-like JSON output");

    assert_eq!(replayed.content, expected_content);
    assert!(
        replayed
            .content
            .iter()
            .all(|block| !matches!(block, ToolContent::Process { .. }))
    );
    assert!(replayed.structured.is_none());
    assert_eq!(replayed.structured_payload(), Some(&data));
}

#[test]
fn malformed_legacy_process_candidates_preserve_every_original_carrier() {
    // Pins: process promotion is an exact historical-row proof, never a
    // best-effort parse that discards user content or structured metadata.
    let rendered = ["out \n", "stderr:\nwarn\t\n", "exit_code: 7"];
    let exact = serde_json::json!({
        "stdout": "out \n",
        "stderr": "warn\t\n",
        "exit_code": 7,
        "stdout_truncated": false,
        "stderr_truncated": true
    });

    let mut extra = exact.clone();
    extra
        .as_object_mut()
        .expect("exact candidate object")
        .insert("extra".to_string(), serde_json::json!(true));
    let mut missing = exact.clone();
    missing
        .as_object_mut()
        .expect("exact candidate object")
        .remove("stderr");
    let mut mistyped = exact.clone();
    mistyped["stdout"] = serde_json::json!(42);
    let mut out_of_range = exact.clone();
    out_of_range["exit_code"] = serde_json::json!(i64::from(i32::MAX) + 1);
    let mut partial_truncation_shape = exact.clone();
    partial_truncation_shape
        .as_object_mut()
        .expect("exact candidate object")
        .remove("stderr_truncated");

    let cases = [
        ("extra key", legacy_wire(&rendered, extra, true, true)),
        ("missing key", legacy_wire(&rendered, missing, true, true)),
        (
            "mistyped field",
            legacy_wire(&rendered, mistyped, true, true),
        ),
        (
            "out-of-range exit code",
            legacy_wire(&rendered, out_of_range, true, true),
        ),
        (
            "partial truncation shape",
            legacy_wire(&rendered, partial_truncation_shape, true, true),
        ),
        (
            "reordered blocks",
            legacy_wire(
                &[rendered[1], rendered[0], rendered[2]],
                exact.clone(),
                true,
                true,
            ),
        ),
        (
            "trailing-whitespace mismatch",
            legacy_wire(
                &["out\n", rendered[1], rendered[2]],
                exact.clone(),
                true,
                true,
            ),
        ),
        (
            "error mismatch",
            legacy_wire(&rendered, exact.clone(), false, true),
        ),
        (
            "truncation mismatch",
            legacy_wire(&rendered, exact, true, false),
        ),
    ];

    for (label, encoded) in cases {
        assert_rejected_legacy_candidate_is_preserved(label, encoded);
    }
}

#[test]
fn noncanonical_process_shapes_fail_serialization_without_losing_data() {
    // Pins: the parent-compatible process projection is lossless only for
    // exactly one process block, so every mixed or duplicate carrier fails.
    let canonical = ToolOutput::from_process(
        "out\n".to_string(),
        "warn\n".to_string(),
        0,
        Duration::from_nanos(1),
    );
    let process = canonical.content[0].clone();

    let mut mixed_text = canonical.clone();
    mixed_text.content.push(ToolContent::Text {
        text: "unrelated".to_string(),
    });
    let mut mixed_json = canonical.clone();
    mixed_json.content.push(ToolContent::Json {
        data: serde_json::json!({ "unrelated": true }),
    });
    let mut multiple_processes = canonical.clone();
    multiple_processes.content.push(process);
    let mut separate_structured = canonical;
    separate_structured.structured = Some(serde_json::json!({ "unrelated": true }));

    for (label, output) in [
        ("process plus text", mixed_text),
        ("process plus JSON", mixed_json),
        ("multiple processes", multiple_processes),
        ("process plus structured", separate_structured),
    ] {
        let expected = output.clone();
        let error = serde_json::to_value(&output)
            .expect_err("noncanonical process output must fail serialization");
        let error_message = error.to_string();
        assert!(
            error_message.contains(
                "process ToolOutput must contain exactly one process block and no structured or JSON carrier"
            ),
            "{label}: unexpected serialization error: {error}"
        );
        assert_eq!(output, expected, "{label}: serialization mutated output");
    }
}

#[test]
fn tool_output_error_sets_error_flag() {
    let output = ToolOutput::error("failed", Duration::from_secs(1));

    assert!(output.is_error);
    assert!(!output.truncated);
    assert_eq!(output.to_text(), "failed");
}

#[test]
fn tool_output_artifact_streams_report_available_entries() {
    let artifact = ToolOutputArtifact {
        combined: ClaimCheck {
            blob_id: "combined".to_string(),
            size: 12,
            preview: "hello".to_string(),
        },
        estimated_tokens: 10,
        line_count: 3,
        stdout_range: None,
        stderr_range: None,
        stdout: Some(ClaimCheck {
            blob_id: "stdout".to_string(),
            size: 5,
            preview: "out".to_string(),
        }),
        stderr: None,
    };

    assert_eq!(
        artifact.available_streams(),
        vec![
            ToolArtifactStream::Combined.as_str(),
            ToolArtifactStream::Stdout.as_str()
        ]
    );
    assert_eq!(
        artifact
            .claim_check(ToolArtifactStream::Stdout)
            .expect("stdout claim check")
            .blob_id,
        "stdout"
    );
    assert!(artifact.claim_check(ToolArtifactStream::Stderr).is_none());
}

#[test]
fn single_blob_stream_ranges_slice_unicode_safely() {
    let combined = "α\nβ\nstderr:\n警告\n";
    let stdout_end = "α\nβ".len();
    let stderr_start = combined.find("警告").expect("stderr text");
    let artifact = ToolOutputArtifact {
        combined: ClaimCheck {
            blob_id: "combined".to_string(),
            size: combined.len(),
            preview: combined.to_string(),
        },
        estimated_tokens: 4,
        line_count: 4,
        stdout_range: Some(ToolArtifactByteRange {
            start: 0,
            end: stdout_end,
        }),
        stderr_range: Some(ToolArtifactByteRange {
            start: stderr_start,
            end: stderr_start + "警告".len(),
        }),
        stdout: None,
        stderr: None,
    };

    assert_eq!(
        artifact
            .slice_stream(ToolArtifactStream::Stdout, combined)
            .expect("stdout range"),
        Some("α\nβ")
    );
    assert_eq!(
        artifact
            .slice_stream(ToolArtifactStream::Stderr, combined)
            .expect("stderr range"),
        Some("警告")
    );
    assert_eq!(
        artifact
            .slice_stream(ToolArtifactStream::Combined, combined)
            .expect("combined range"),
        Some(combined)
    );

    let invalid = ToolArtifactByteRange { start: 1, end: 2 };
    assert!(invalid.slice(combined).is_err());
}

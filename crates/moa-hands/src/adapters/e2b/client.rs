//! E2B HTTP, envd, connect-stream, and error helper functions.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_core::{HandStatus, MoaError, Result, ToolFailureClass, ToolOutput, classify_tool_error};
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::Value;

use super::{ConnectedSandbox, DEFAULT_ENVD_PORT};

const CONNECT_COMPRESSED_FLAG: u8 = 0b0000_0001;
pub(super) const CONNECT_END_STREAM_FLAG: u8 = 0b0000_0010;

pub(super) fn default_headers(api_key: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "X-API-KEY",
        HeaderValue::from_str(api_key).map_err(|error| {
            MoaError::ValidationError(format!("invalid E2B API key header: {error}"))
        })?,
    );
    Ok(headers)
}

pub(super) fn envd_headers(sandbox_id: &str, sandbox: &ConnectedSandbox) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "E2b-Sandbox-Id",
        HeaderValue::from_str(sandbox_id).map_err(|error| {
            MoaError::ValidationError(format!("invalid sandbox header: {error}"))
        })?,
    );
    headers.insert(
        "E2b-Sandbox-Port",
        HeaderValue::from_str(&DEFAULT_ENVD_PORT.to_string()).map_err(|error| {
            MoaError::ValidationError(format!("invalid sandbox port header: {error}"))
        })?,
    );
    headers.insert(
        "X-Access-Token",
        HeaderValue::from_str(&sandbox.envd_access_token).map_err(|error| {
            MoaError::ValidationError(format!("invalid E2B access token header: {error}"))
        })?,
    );
    Ok(headers)
}

pub(super) fn build_url(base: &str, params: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base)
        .map_err(|error| MoaError::ValidationError(format!("invalid E2B URL {base}: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

pub(super) async fn expect_success_json(response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| MoaError::ProviderError(format!("invalid E2B JSON response: {error}")))
}

pub(super) async fn expect_success(response: reqwest::Response) -> Result<()> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    Ok(())
}

pub(super) async fn http_error(response: reqwest::Response) -> MoaError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    let message = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read response body".to_string());
    MoaError::HttpStatus {
        status,
        retry_after,
        message,
    }
}

pub(super) fn encode_connect_request(value: &Value) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(value)?;
    let length = u32::try_from(json.len())
        .map_err(|_| MoaError::ValidationError("E2B connect request too large".to_string()))?;
    let mut envelope = Vec::with_capacity(json.len() + 5);
    envelope.push(0);
    envelope.extend_from_slice(&length.to_be_bytes());
    envelope.extend_from_slice(&json);
    Ok(envelope)
}

pub(super) fn parse_e2b_connect_stream(body: &[u8], duration: Duration) -> Result<ToolOutput> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0;
    let mut cursor = 0;
    while cursor + 5 <= body.len() {
        let flags = body[cursor];
        let length = u32::from_be_bytes([
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
            body[cursor + 4],
        ]) as usize;
        cursor += 5;
        if cursor + length > body.len() {
            return Err(MoaError::StreamError(
                "incomplete E2B connect envelope".to_string(),
            ));
        }
        let payload = &body[cursor..cursor + length];
        cursor += length;

        if (flags & CONNECT_COMPRESSED_FLAG) != 0 {
            return Err(MoaError::StreamError(
                "compressed E2B command envelopes are unsupported".to_string(),
            ));
        }
        if (flags & CONNECT_END_STREAM_FLAG) != 0 {
            if payload != b"{}" && !payload.is_empty() {
                let value: Value = serde_json::from_slice(payload).map_err(|error| {
                    MoaError::StreamError(format!("invalid E2B end-stream event: {error}"))
                })?;
                if let Some(message) = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                {
                    return Err(MoaError::ProviderError(format!(
                        "E2B command stream error: {message}"
                    )));
                }
            }
            continue;
        }

        let value: Value = serde_json::from_slice(payload).map_err(|error| {
            MoaError::StreamError(format!("invalid E2B command event: {error}"))
        })?;
        let Some(event) = value.get("event").and_then(Value::as_object) else {
            continue;
        };
        if let Some(data) = event.get("data").and_then(Value::as_object) {
            if let Some(text) = data.get("stdout").and_then(Value::as_str) {
                stdout.push_str(&decode_stream_chunk(text));
            }
            if let Some(text) = data.get("stderr").and_then(Value::as_str) {
                stderr.push_str(&decode_stream_chunk(text));
            }
            continue;
        }
        if let Some(end) = event.get("end").and_then(Value::as_object) {
            exit_code = extract_exit_code(end);
            if let Some(error) = end.get("error").and_then(Value::as_str) {
                stderr.push_str(error);
            }
        }
    }

    if cursor != body.len() {
        return Err(MoaError::StreamError(
            "trailing bytes in E2B connect stream".to_string(),
        ));
    }

    Ok(ToolOutput::from_process(
        stdout, stderr, exit_code, duration,
    ))
}

/// Classifies one E2B execution error for retry and re-provision decisions.
pub(super) fn classify_error(
    error: &MoaError,
    status: Option<HandStatus>,
    consecutive_timeouts: u32,
) -> ToolFailureClass {
    if matches!(
        status,
        Some(HandStatus::Stopped | HandStatus::Destroyed | HandStatus::Failed)
    ) {
        return ToolFailureClass::ReProvision {
            reason: "E2B sandbox is no longer healthy".to_string(),
        };
    }

    match error {
        MoaError::HttpStatus { status: 404, .. } => ToolFailureClass::ReProvision {
            reason: "E2B sandbox no longer exists".to_string(),
        },
        MoaError::ProviderError(message)
        | MoaError::StreamError(message)
        | MoaError::ToolError(message) => {
            let message_lower = message.to_ascii_lowercase();
            if message_lower.contains("timeoutexception")
                && (message_lower.contains("unavailable") || message_lower.contains("unknown"))
            {
                return ToolFailureClass::ReProvision {
                    reason: "E2B sandbox became unavailable".to_string(),
                };
            }
            if message_lower.contains("deadline_exceeded") {
                return ToolFailureClass::Retryable {
                    reason: message.clone(),
                    backoff_hint: Duration::ZERO,
                };
            }
            classify_tool_error(error, consecutive_timeouts)
        }
        _ => classify_tool_error(error, consecutive_timeouts),
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn decode_stream_chunk(value: &str) -> String {
    BASE64
        .decode(value)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| value.to_string())
}

fn extract_exit_code(end: &serde_json::Map<String, Value>) -> i32 {
    if let Some(exit_code) = end
        .get("exitCode")
        .or_else(|| end.get("exit_code"))
        .and_then(Value::as_i64)
    {
        return exit_code as i32;
    }
    if let Some(status) = end.get("status").and_then(Value::as_str) {
        if status == "exit status 0" {
            return 0;
        }
        if let Some(code) = status
            .strip_prefix("exit status ")
            .and_then(|raw| raw.parse::<i32>().ok())
        {
            return code;
        }
    }
    if end.get("error").and_then(Value::as_str).is_some() {
        return 1;
    }
    0
}

pub(super) fn required_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| MoaError::ValidationError(format!("missing string field `{field}`")))
}

pub(super) fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

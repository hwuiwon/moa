//! Document parser trait and parser adapters.

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::{
    domain::{ParseInput, ParsedDocument, WebhookEvent},
    error::{Error, Result},
    normalize::redact_provider_metadata,
    providers::http::nested_value,
};

pub mod llamaparse;
pub mod native;
pub mod reducto;
pub mod unstructured;

/// Structure-aware parser seam for tenant knowledge ingestion.
#[async_trait]
pub trait DocumentParser: Send + Sync {
    /// Parses one source object into normalized document elements.
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument>;
}

/// Verifies a parser-origin webhook and maps safe job/object metadata.
pub fn verify_parser_webhook(
    parser: &str,
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
) -> Result<WebhookEvent> {
    if signing_key.trim().is_empty() {
        return Err(Error::Config(format!(
            "{parser} webhook verifier requires a signing key"
        )));
    }
    verify_parser_signature(parser, headers, body, signing_key)?;
    map_parser_webhook(parser, headers, body)
}

/// Maps a verified parser-origin webhook into safe normalized event metadata.
pub fn map_parser_webhook(parser: &str, headers: &HeaderMap, body: &[u8]) -> Result<WebhookEvent> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| Error::parser(parser, format!("webhook JSON decode failed: {error}")))?;
    Ok(WebhookEvent {
        provider: parser.to_string(),
        event_id: webhook_string(
            &value,
            &["event_id", "webhook_id", "id", "job_id", "data.id"],
        )
        .or_else(|| webhook_header(headers, &["svix-id", "x-svix-id"]))
        .unwrap_or_else(|| "unknown".to_string()),
        event_type: webhook_string(
            &value,
            &[
                "event_type",
                "type",
                "event",
                "status",
                "data.event_type",
                "data.status",
            ],
        )
        .or_else(|| webhook_header(headers, &["svix-event-type", "x-event-type"]))
        .unwrap_or_else(|| "unknown".to_string()),
        metadata: parser_webhook_metadata(&value),
    })
}

fn verify_parser_signature(
    parser: &str,
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
) -> Result<()> {
    if webhook_header(headers, &["svix-signature", "x-svix-signature"]).is_some() {
        return verify_svix_signature(parser, headers, body, signing_key);
    }
    let signature = parser_signature_header(parser, headers)?;
    let signature = decode_signature(signature, parser)?;
    verify_hmac(parser, signing_key.as_bytes(), body, &signature)
}

fn verify_svix_signature(
    parser: &str,
    headers: &HeaderMap,
    body: &[u8],
    signing_key: &str,
) -> Result<()> {
    let message_id = webhook_header(headers, &["svix-id", "x-svix-id"])
        .ok_or_else(|| Error::parser(parser, "webhook missing svix-id header"))?;
    let timestamp = webhook_header(headers, &["svix-timestamp", "x-svix-timestamp"])
        .ok_or_else(|| Error::parser(parser, "webhook missing svix-timestamp header"))?;
    verify_svix_timestamp(parser, &timestamp)?;
    let signature = webhook_header(headers, &["svix-signature", "x-svix-signature"])
        .ok_or_else(|| Error::parser(parser, "webhook missing svix-signature header"))?;
    let key = svix_signing_key(signing_key, parser)?;
    let signed_payload = format!(
        "{message_id}.{timestamp}.{}",
        std::str::from_utf8(body).map_err(|error| {
            Error::parser(parser, format!("webhook body was not UTF-8: {error}"))
        })?
    );
    for candidate in signature.split_whitespace() {
        if let Some(encoded) = candidate.strip_prefix("v1,") {
            let signature = decode_base64_signature(encoded, parser)?;
            if verify_hmac(parser, &key, signed_payload.as_bytes(), &signature).is_ok() {
                return Ok(());
            }
        }
    }
    Err(Error::parser(
        parser,
        "webhook signature verification failed",
    ))
}

fn verify_svix_timestamp(parser: &str, timestamp: &str) -> Result<()> {
    let ts = timestamp
        .parse::<i64>()
        .map_err(|_| Error::parser(parser, "webhook svix-timestamp was not numeric"))?;
    let now = Utc::now().timestamp();
    if (now - ts).abs() <= 300 {
        return Ok(());
    }
    Err(Error::parser(
        parser,
        "webhook svix-timestamp was outside the replay window",
    ))
}

fn parser_signature_header<'a>(parser: &str, headers: &'a HeaderMap) -> Result<&'a str> {
    let parser_header = format!("x-{parser}-webhook-signature");
    for name in [parser_header.as_str(), "x-moa-knowledge-webhook-signature"] {
        if let Some(value) = headers.get(name) {
            return value.to_str().map_err(|error| {
                Error::parser(parser, format!("webhook header `{name}` failed: {error}"))
            });
        }
    }
    Err(Error::parser(parser, "webhook missing signature header"))
}

fn decode_signature(value: &str, parser: &str) -> Result<Vec<u8>> {
    let value = value.trim().trim_start_matches("sha256=");
    if let Ok(decoded) = hex::decode(value)
        && decoded.len() == 32
    {
        return Ok(decoded);
    }
    decode_base64_signature(value, parser)
}

fn decode_base64_signature(value: &str, parser: &str) -> Result<Vec<u8>> {
    general_purpose::STANDARD
        .decode(value.trim())
        .or_else(|_| general_purpose::URL_SAFE.decode(value.trim()))
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(value.trim()))
        .map_err(|error| {
            Error::parser(
                parser,
                format!("webhook signature was not hex or base64: {error}"),
            )
        })
}

fn svix_signing_key(signing_key: &str, parser: &str) -> Result<Vec<u8>> {
    let Some(encoded) = signing_key.trim().strip_prefix("whsec_") else {
        return Ok(signing_key.as_bytes().to_vec());
    };
    general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| general_purpose::URL_SAFE.decode(encoded))
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(encoded))
        .map_err(|error| Error::parser(parser, format!("Svix signing key failed: {error}")))
}

fn verify_hmac(parser: &str, key: &[u8], payload: &[u8], signature: &[u8]) -> Result<()> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|error| Error::parser(parser, format!("webhook signing key failed: {error}")))?;
    mac.update(payload);
    mac.verify_slice(signature)
        .map_err(|_| Error::parser(parser, "webhook signature verification failed"))
}

fn parser_webhook_metadata(value: &Value) -> Value {
    let redacted = redact_provider_metadata(value.clone());
    let mut metadata = Map::new();
    copy_first_string(
        &redacted,
        &mut metadata,
        "tenant_id",
        &[
            "tenant_id",
            "tenantId",
            "connection.tenant_id",
            "metadata.tenant_id",
            "metadata.tenantId",
            "data.tenant_id",
            "data.tenantId",
            "data.metadata.tenant_id",
            "data.metadata.tenantId",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "connection_uid",
        &[
            "connection_uid",
            "connection_id",
            "metadata.connection_uid",
            "metadata.connection_id",
            "data.connection_uid",
            "data.connection_id",
            "data.metadata.connection_uid",
            "data.metadata.connection_id",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "event_id",
        &["event_id", "id", "job_id", "data.id"],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "event_type",
        &[
            "event_type",
            "type",
            "event",
            "status",
            "data.event_type",
            "data.status",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "parser_job_id",
        &[
            "job_id",
            "id",
            "data.job_id",
            "data.id",
            "data.parse.job_id",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "object_uid",
        &[
            "object_uid",
            "object_id",
            "metadata.object_uid",
            "metadata.object_id",
            "data.object_uid",
            "data.object_id",
            "data.metadata.object_uid",
            "data.metadata.object_id",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "source_id",
        &[
            "source_id",
            "metadata.source_id",
            "data.source_id",
            "data.metadata.source_id",
        ],
    );
    copy_first_string(
        &redacted,
        &mut metadata,
        "status",
        &["status", "data.status", "data.metadata.status"],
    );
    copy_safe_value(&redacted, &mut metadata, "metadata", &["metadata"]);
    copy_safe_value(
        &redacted,
        &mut metadata,
        "data_metadata",
        &["data.metadata"],
    );
    Value::Object(metadata)
}

fn copy_first_string(value: &Value, output: &mut Map<String, Value>, name: &str, paths: &[&str]) {
    if let Some(found) = paths
        .iter()
        .find_map(|path| nested_value(value, path)?.as_str())
    {
        output.insert(name.to_string(), Value::String(found.to_string()));
    }
}

fn copy_safe_value(value: &Value, output: &mut Map<String, Value>, name: &str, paths: &[&str]) {
    if let Some(found) = paths
        .iter()
        .find_map(|path| safe_metadata_value(nested_value(value, path)?))
    {
        output.insert(name.to_string(), found);
    }
}

fn safe_metadata_value(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) => {
            let filtered = map
                .iter()
                .filter(|(key, _)| !is_raw_document_key(key))
                .filter_map(|(key, value)| {
                    safe_metadata_value(value).map(|value| (key.clone(), value))
                })
                .collect::<Map<_, _>>();
            (!filtered.is_empty()).then_some(Value::Object(filtered))
        }
        Value::Array(_) => None,
        Value::String(value) if value.len() <= 512 => Some(Value::String(value.clone())),
        Value::Number(_) | Value::Bool(_) | Value::Null => Some(value.clone()),
        Value::String(_) => None,
    }
}

fn is_raw_document_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("body")
        || key.contains("chunk")
        || key.contains("content")
        || key.contains("document")
        || key.contains("file")
        || key.contains("html")
        || key.contains("markdown")
        || key.contains("page")
        || key.contains("payload")
        || key.contains("raw")
        || key.contains("text")
}

fn webhook_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string())
    })
}

fn webhook_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| nested_value(value, key)?.as_str().map(ToOwned::to_owned))
}

//! Provider-record and text normalization helpers.

use chrono::Utc;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::domain::{KnowledgeObject, ObjectStatus, ProviderRecord};

/// Normalizes source text for deterministic block identities.
#[must_use]
pub fn normalize_text(input: &str) -> String {
    let normalized = normalize_line_endings_and_unicode(input);
    let mut output = String::with_capacity(normalized.len());
    let mut in_fenced_code = false;

    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            push_normalized_line(&mut output, trimmed);
            in_fenced_code = !in_fenced_code;
            continue;
        }

        let normalized_line = if in_fenced_code || is_table_sensitive_line(line) {
            line.trim_end().to_string()
        } else {
            collapse_inline_whitespace(line).trim().to_string()
        };

        if normalized_line.is_empty() {
            if !output.is_empty() && !output.ends_with("\n\n") {
                output.push('\n');
            }
        } else {
            push_normalized_line(&mut output, &normalized_line);
        }
    }

    output.trim().to_string()
}

/// Normalizes line endings and Unicode without changing whitespace shape.
#[must_use]
pub fn normalize_line_endings_and_unicode(input: &str) -> String {
    input
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .nfc()
        .collect::<String>()
}

fn push_normalized_line(output: &mut String, line: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(line);
}

fn collapse_inline_whitespace(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut last_was_space = false;
    for ch in input.chars() {
        if ch == '\n' {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            last_was_space = false;
        } else if ch.is_whitespace() {
            if !last_was_space {
                output.push(' ');
                last_was_space = true;
            }
        } else {
            output.push(ch);
            last_was_space = false;
        }
    }
    output
}

fn is_table_sensitive_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|') || trimmed.contains('\t')
}

/// Removes fields that may carry credentials, tokens, or raw secrets.
#[must_use]
pub fn redact_provider_metadata(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(key, value)| {
                    if is_secret_key(&key) {
                        None
                    } else {
                        Some((key, redact_provider_metadata(value)))
                    }
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => {
            Value::Array(values.into_iter().map(redact_provider_metadata).collect())
        }
        Value::String(value) if is_secret_value(&value) => Value::String("[redacted]".to_string()),
        other => other,
    }
}

/// Converts a provider record into a tenant knowledge object.
#[must_use]
pub fn normalize_provider_record(
    tenant_object_seed: &str,
    tenant_id: moa_core::TenantId,
    connection_uid: Uuid,
    record: ProviderRecord,
) -> KnowledgeObject {
    let object_uid = stable_object_uid(tenant_object_seed, &record.source_id);
    KnowledgeObject {
        object_uid,
        tenant_id,
        connection_uid,
        object_type: record.object_type,
        source_id: record.source_id,
        parent_source_id: None,
        source_uri: record.source_uri,
        title: record.title,
        change_token: record.change_token,
        metadata: redact_provider_metadata(record.metadata),
        status: if record.deleted {
            ObjectStatus::Deleted
        } else {
            ObjectStatus::Pending
        },
        source_updated_at: record.source_updated_at,
        deleted_at: record.deleted.then(Utc::now),
    }
}

fn stable_object_uid(seed: &str, source_id: &str) -> Uuid {
    let hash = blake3::hash(format!("{seed}:{source_id}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[0..16]);
    Uuid::from_bytes(bytes)
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("credential")
        || key.contains("authorization")
        || key == "auth"
        || key.contains("bearer")
        || key.contains("api_key")
        || key.contains("apikey")
}

fn is_secret_value(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("bearer ") || value.contains("authorization: bearer ")
}

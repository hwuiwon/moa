//! Provider-record and text normalization helpers.

use chrono::Utc;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::domain::{KnowledgeObject, ObjectStatus, ProviderRecord};
use crate::graph_delta::stable_uid;

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

/// Redacts and normalizes provider-native selected source state.
#[must_use]
pub fn normalize_source_selection(value: Value) -> Value {
    let value = redact_provider_metadata(value);
    if value.is_null() {
        Value::Object(Map::new())
    } else {
        value
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
    let object_uid = stable_uid(&format!("{tenant_object_seed}:{}", record.source_id));
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

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', ' ', '.'], "_");
    matches!(
        normalized.as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "secret"
            | "client_secret"
            | "password"
            | "credential"
            | "credential_ref"
            | "credential_reference"
            | "credentials"
            | "authorization"
            | "auth"
            | "bearer"
            | "api_key"
            | "apikey"
    ) || normalized.ends_with("_secret")
        || normalized.ends_with("_password")
        || normalized.ends_with("_credential")
        || normalized.ends_with("_credentials")
        || normalized.ends_with("_authorization")
        || normalized.ends_with("_api_key")
        || normalized.ends_with("_apikey")
        || (normalized.ends_with("_token") && normalized != "token_count")
}

fn is_secret_value(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("bearer ") || value.contains("authorization: bearer ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{normalize_source_selection, normalize_text, redact_provider_metadata};

    #[test]
    fn normalize_text_preserves_fenced_code_block_whitespace_verbatim() {
        // Pins: content inside ``` fenced blocks keeps internal indentation/spacing
        // verbatim while surrounding prose is whitespace-collapsed.
        let input = "Prose   with     spaces.\n```\n    indented = 1\n    nested   spacing\n```\nMore    prose.";
        assert_eq!(
            normalize_text(input),
            "Prose with spaces.\n```\n    indented = 1\n    nested   spacing\n```\nMore prose."
        );
    }

    #[test]
    fn normalize_text_preserves_markdown_table_column_whitespace() {
        // Pins: pipe table rows keep their column padding via is_table_sensitive_line
        // while non-table prose is still collapsed.
        let input =
            "Intro    text.\n| Col A    | Col B   |\n| ---      | ---     |\nOutro    text.";
        assert_eq!(
            normalize_text(input),
            "Intro text.\n| Col A    | Col B   |\n| ---      | ---     |\nOutro text."
        );
    }

    #[test]
    fn redact_provider_metadata_keeps_token_count_but_drops_token_suffixed_keys() {
        // Pins: is_secret_key's `token_count` carve-out keeps the safe key while
        // `_token`-suffixed keys are redacted (dropped entirely).
        assert_eq!(
            redact_provider_metadata(json!({
                "token_count": 42,
                "session_token": "must-redact"
            })),
            json!({ "token_count": 42 })
        );
    }

    #[test]
    fn normalize_source_selection_defaults_to_empty_and_redacts_nested_secrets() {
        // Pins: omitted selected-source state means provider default/all, not persisted JSON null.
        assert_eq!(normalize_source_selection(json!(null)), json!({}));
        assert_eq!(
            normalize_source_selection(json!({
                "metadata": {
                    "selected_folder_ids": ["folder-1"],
                    "access_token": "must-redact"
                }
            })),
            json!({
                "metadata": {
                    "selected_folder_ids": ["folder-1"]
                }
            })
        );
    }
}
